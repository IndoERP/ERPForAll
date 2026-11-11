//! Flow, per call, inside one savepoint that is rolled back when any message is an error:
//!
//! 1. Column names become paths; every path is checked against the models (unknown fields and
//!    sub-paths of non-relational columns are errors). At most [`MAX_ROWS`] rows are accepted.
//! 2. The matrix is checked: a column given twice or a row whose length differs from the header
//!    is an error message, and nothing is imported.
//! 3. Rows become record trees ([`super::extract`]).
//! 4. Records are cut into batches.
//!    Batches are also capped at
//!    [`DEFAULT_BATCH_SIZE`] to bound extracted record trees in memory (PERF.LOAD-STREAM).
//! 5. Per batch, references are resolved: one name search per distinct (comodel, name), one
//!    external-id query per comodel, one existence query per comodel, one search for the
//!    records' own `.id`s; then each record is converted.
//! 6. The batch is written: external ids looked up in one statement, existing records written
//!    one `write` each, new records created with one `create`, their external ids written with one
//!    upsert. If the batch fails on data, it is retried record by record, each in its own
//!    savepoint (two statements more per record plus its own writes), to report every failing
//!    record, stopping after 10 errors when they are at least one in ten.
//!
//! Errors that are not about the data (programming errors, serialization failures, lost
//! connections) are returned as errors, never turned into messages.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use erp_domain::{Domain, Operator, Value as DValue};
use erp_fields::{Cell, Error, ErrorKind, FieldType, Result, Value};
use erp_orm::{Env, NameSearch, RawVals, Records, Transaction};
use erp_registry::{ModelId, ModelSpec, Registry};

use super::convert::{Answers, Collector, Converter, FieldIssue, LiftedLine, Settings};
use super::extract::{self, Extracted, Key, Path, Raw, RawRecord};
use super::message::{ImportMessage, LoadResult, MessageType, RowSpan};
use super::path::checked_path;
use super::row::RowSource;
use crate::ast::XmlId;
use crate::model_data::{self, LoadRecord};

/// Most rows one `load` call accepts. Callers taking data from outside (the JSON-2 layer, M3)
/// must also cap rows, cell bytes and total request bytes; this guard only keeps a direct caller
/// from building unbounded record trees (design note, "Limits").
pub const MAX_ROWS: usize = 1_000_000;

/// Default maximum number of extracted records per batch (PERF.LOAD-STREAM).
pub const DEFAULT_BATCH_SIZE: usize = 1_000;

const DB_PREFIX: &str = "The operation cannot be completed: ";

const TOO_MANY: &str = "Found more than 10 errors and more than one error per 10 records, \
                        interrupted to avoid showing too many errors.";

const UNSUPPORTED_KEYS: [&str; 3] = [
    "name_create_enabled_fields",
    "import_skip_records",
    "import_set_empty_fields",
];

/// The source of the error that rolls the load savepoint back on purpose; only this module can
/// build it, so no other error is mistaken for it.
#[derive(Debug)]
struct ImportFailed;

impl fmt::Display for ImportFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the import has errors; its changes are rolled back")
    }
}

impl std::error::Error for ImportFailed {}

fn is_import_failed(e: &Error) -> bool {
    std::error::Error::source(e).is_some_and(|s| s.is::<ImportFailed>())
}

/// `load(fields, data)` on the model of `records`.
///
/// Context keys: `module` (prefix of unqualified external ids, default `__import__`),
/// `noupdate` (flag of new external ids), `mode` (`init`, the default, or `update`, which keeps
/// records whose external id is `noupdate`), `import_file` (reference errors carry their column
/// path, type and value), `update_many2many` (many2many references are added, not replaced),
/// `_import_limit` (stop after that many rows; `nextrow` tells where to resume).
pub async fn load(
    records: &Records,
    tx: &mut Transaction,
    fields: &[&str],
    data: &[Vec<String>],
) -> Result<LoadResult> {
    load_rows(records, tx, fields, data).await
}

/// Generic streaming/batched row loader without matrix copies (PERF.LOAD-STREAM).
pub async fn load_rows<'a, R: RowSource<'a>>(
    records: &Records,
    tx: &mut Transaction,
    fields: &[&str],
    rows: R,
) -> Result<LoadResult> {
    let env = records.env().clone();
    let options = Options::from_env(&env)?;
    let registry = Arc::clone(env.registry());
    let spec = records.spec();
    let path_strings: Vec<Vec<String>> = fields
        .iter()
        .map(|f| checked_path(f))
        .collect::<Result<_>>()?;
    let paths: Vec<Path> = path_strings
        .into_iter()
        .map(|p| p.into_iter().map(|s| Key::Field(s.into())).collect())
        .collect();
    extract::validate(&registry, spec, &paths)?;
    if rows.len() > MAX_ROWS {
        return Err(Error::programming(format!(
            "load received {} rows; at most {MAX_ROWS} are accepted per call",
            rows.len()
        )));
    }
    let problems = structure_problems(fields, &paths, &rows);
    if !problems.is_empty() {
        return Ok(LoadResult {
            ids: None,
            messages: problems,
            nextrow: 0,
        });
    }
    if rows.is_empty() {
        return Ok(LoadResult {
            ids: Some(Vec::new()),
            messages: Vec::new(),
            nextrow: 0,
        });
    }
    let creatable = creatable_models(&registry, spec, &paths);

    let mut loader = Loader {
        records,
        registry: &registry,
        options: &options,
        creatable,
        messages: Vec::new(),
        ids: Vec::new(),
        group: Vec::new(),
        group_first_message: 0,
        pending_xids: HashSet::new(),
    };
    let mut last_processed_row = 0;
    let outcome = tx
        .savepoint(async |tx| {
            let mut current_row = 0;
            let mut record_offset = 0;
            while current_row < rows.len() && current_row < options.limit {
                let (extracted_batch, next_row) = extract::extract_batch(
                    &registry,
                    spec,
                    &paths,
                    &rows,
                    current_row,
                    DEFAULT_BATCH_SIZE,
                    options.limit,
                )?;
                if extracted_batch.is_empty() {
                    break;
                }
                loader
                    .run_batch(tx, &extracted_batch, record_offset)
                    .await?;
                record_offset += extracted_batch.len();
                current_row = next_row;
                last_processed_row = next_row;
            }
            loader.finish(tx).await?;
            if loader.messages.iter().any(|m| m.kind == MessageType::Error) {
                Err(Error::programming(ImportFailed.to_string()).with_source(ImportFailed))
            } else {
                Ok(())
            }
        })
        .await;
    let failed = match outcome {
        Ok(()) => false,
        Err(e) if is_import_failed(&e) => true,
        Err(e) => return Err(e),
    };
    Ok(LoadResult {
        ids: (!failed).then_some(loader.ids),
        messages: loader.messages,
        nextrow: if last_processed_row < options.limit {
            0
        } else {
            last_processed_row
        },
    })
}

/// Columns given twice and rows whose length differs from the header, as error messages.
fn structure_problems<'a, R: RowSource<'a> + ?Sized>(
    fields: &[&str],
    paths: &[Path],
    rows: &R,
) -> Vec<ImportMessage> {
    let mut problems = Vec::new();
    let mut seen: HashSet<&Path> = HashSet::new();
    let mut reported: HashSet<&Path> = HashSet::new();
    for (column, path) in fields.iter().zip(paths) {
        if !seen.insert(path) && reported.insert(path) {
            let mut message = ImportMessage::new(
                MessageType::Error,
                format!("Column '{column}' appears more than once"),
            );
            if let Some(Key::Field(name)) = path.first() {
                message = message.with_field(name.to_string());
            }
            problems.push(message);
        }
    }
    for index in 0..rows.len() {
        let len = rows.row_len(index);
        if len != fields.len() {
            problems.push(
                ImportMessage::new(
                    MessageType::Error,
                    format!(
                        "This row has {} cells but there are {} columns",
                        len,
                        fields.len()
                    ),
                )
                .with_rows(RowSpan::new(index, index)),
            );
        }
    }
    problems
}

/// `mode` of the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `init` (default): every matched record is written.
    Init,
    /// `update`: records whose external id is `noupdate` are left alone.
    Update,
}

/// The context of a `load` call.
struct Options {
    settings: Settings,
    mode: Mode,
    noupdate: bool,
    limit: usize,
}

fn context_flag(env: &Env, key: &str) -> Result<bool> {
    match env.context().get(key) {
        // Python truthiness: False is the absent flag
        None | Some(Value::False) => Ok(false),
        Some(Value::Bool(b)) => Ok(b),
        Some(other) => Err(Error::programming(format!(
            "context key '{key}' must be a boolean, got {other}"
        ))),
    }
}

fn context_str(env: &Env, key: &str) -> Result<Option<String>> {
    match env.context().get(key) {
        // Python truthiness: False is the absent value
        None | Some(Value::False) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.to_string())),
        Some(other) => Err(Error::programming(format!(
            "context key '{key}' must be a string, got {other}"
        ))),
    }
}

impl Options {
    fn from_env(env: &Env) -> Result<Self> {
        for key in UNSUPPORTED_KEYS {
            if env.context().get(key).is_some_and(|v| v.is_truthy()) {
                return Err(Error::programming(format!(
                    "context key '{key}' is not supported by load until M4 (the web client's \
                     import wizard)"
                )));
            }
        }
        let limit = match env.context().get("_import_limit") {
            None | Some(Value::False) => usize::MAX,
            // beyond usize (32-bit hosts) a limit cannot be reached: no limit
            Some(Value::Int(n)) if n >= 0 => usize::try_from(n).unwrap_or(usize::MAX),
            Some(other) => {
                return Err(Error::programming(format!(
                    "context key '_import_limit' must be a non-negative integer, got {other}"
                )));
            }
        };
        let mode = match context_str(env, "mode")?.as_deref() {
            None | Some("init") => Mode::Init,
            Some("update") => Mode::Update,
            Some(other) => {
                return Err(Error::programming(format!(
                    "context key 'mode' must be 'init' or 'update', got '{other}'"
                )));
            }
        };
        Ok(Self {
            settings: Settings {
                module: context_str(env, "module")?.unwrap_or_else(|| "__import__".to_owned()),
                import_file: context_flag(env, "import_file")?,
                update_many2many: context_flag(env, "update_many2many")?,
                tz: env.tz().to_owned(),
            },
            mode,
            noupdate: context_flag(env, "noupdate")?,
            limit,
        })
    }
}

fn creatable_models(registry: &Registry, spec: &ModelSpec, paths: &[Path]) -> HashSet<ModelId> {
    let mut models = HashSet::from([spec.id]);
    for path in paths {
        let mut current = spec;
        for key in path {
            let Some(name) = key.field().filter(|n| *n != "id" && *n != ".id") else {
                break;
            };
            if let Some(field) = current.field(name)
                && let FieldType::One2many { comodel, .. } = &field.ty
                && let Some(next) = registry.model(comodel)
            {
                models.insert(next.id);
                current = next;
            }
        }
    }
    models
}

/// One record ready to be written.
#[derive(Debug, Clone)]
struct Pending {
    /// Qualified external id (`module.name`), as typed after qualification.
    xid: Option<String>,
    dbid: Option<i64>,
    vals: RawVals,
    rows: RowSpan,
    record: usize,
    lifted: Vec<LiftedLine>,
}

impl Pending {
    fn message(&self, kind: MessageType, message: String) -> ImportMessage {
        ImportMessage::at(kind, self.rows, self.record, message)
    }
}

/// Collect all external ids declared on a record or its nested child records.
fn collect_declared_xids(record: &RawRecord<'_>, settings: &Settings, out: &mut HashSet<String>) {
    for (key, raw) in &record.entries {
        match raw {
            Raw::Text(t) => {
                if key.field() == Some("id") && !t.is_empty() {
                    out.insert(settings.qualify(t));
                }
            }
            Raw::Records(records) => {
                for rec in records {
                    collect_declared_xids(rec, settings, out);
                }
            }
        }
    }
}

struct Loader<'a, 'opt> {
    records: &'a Records,
    registry: &'a Registry,
    options: &'opt Options,
    creatable: HashSet<ModelId>,
    messages: Vec<ImportMessage>,
    ids: Vec<i64>,
    /// The open flush group: converted records not yet written.
    group: Vec<Pending>,
    /// Where the open group's messages start (index into `messages`).
    group_first_message: usize,
    /// Qualified external ids of the open group's records.
    pending_xids: HashSet<String>,
}

/// The record's own text entry for `key` (`id`, `.id`), when non-empty.
fn own<'a>(extracted: &Extracted<'a>, key: &str) -> Option<&'a str> {
    match extracted.record.get_field(key) {
        Some(Raw::Text(t)) if !t.is_empty() => Some(*t),
        _ => None,
    }
}

/// Errors about the data: reported as messages. Everything else aborts the import.
fn is_data_error(e: &Error) -> bool {
    match e.kind() {
        // a stale expected row version is about one record's data, like `Missing`
        ErrorKind::UserError
        | ErrorKind::Validation
        | ErrorKind::Access
        | ErrorKind::Missing
        | ErrorKind::Conflict => true,
        // data exceptions (22) and integrity violations (23) only
        ErrorKind::Database => e
            .sqlstate()
            .is_some_and(|s| s.starts_with("22") || s.starts_with("23")),
        ErrorKind::AccessDenied
        | ErrorKind::SessionExpired
        | ErrorKind::Lock
        | ErrorKind::Redirect
        | ErrorKind::Concurrency
        | ErrorKind::Programming
        | ErrorKind::Internal
        | ErrorKind::ResourceExhausted => false,
    }
}

impl Loader<'_, '_> {
    fn spec(&self) -> &ModelSpec {
        self.records.spec()
    }

    fn converter(&self) -> Converter<'_> {
        Converter {
            registry: self.registry,
            settings: &self.options.settings,
        }
    }

    /// Convert one chunk of extracted records into the open flush group. A real flush trigger (a
    /// reference to a creatable model by name, or to an external id pending in the group) writes
    /// the group first. The group is not flushed at the chunk end: only [`Loader::finish`] does.
    async fn run_batch<'ext>(
        &mut self,
        tx: &mut Transaction,
        extracted: &[Extracted<'ext>],
        record_offset: usize,
    ) -> Result<()> {
        let mut batch: Vec<(usize, Collector)> = Vec::new();
        for (index_in_batch, item) in extracted.iter().enumerate() {
            let mut questions = Collector::default();
            self.converter()
                .record(self.spec(), &item.record, &[], &mut questions)?;
            let needs_flush = questions
                .names
                .iter()
                .any(|(model, _)| self.creatable.contains(model))
                || questions
                    .xids
                    .iter()
                    .any(|(_, x)| self.pending_xids.contains(x));
            if needs_flush && (!batch.is_empty() || !self.group.is_empty()) {
                self.prepare(tx, extracted, std::mem::take(&mut batch), record_offset)
                    .await?;
                self.flush_group(tx).await?;
            }
            collect_declared_xids(&item.record, &self.options.settings, &mut self.pending_xids);
            batch.push((index_in_batch, questions));
        }
        self.prepare(tx, extracted, batch, record_offset).await
    }

    /// Write the open flush group at the end of the input.
    async fn finish(&mut self, tx: &mut Transaction) -> Result<()> {
        self.flush_group(tx).await
    }

    async fn flush_group(&mut self, tx: &mut Transaction) -> Result<()> {
        self.pending_xids.clear();
        let group = std::mem::take(&mut self.group);
        let first_message = self.group_first_message;
        self.flush(tx, group, first_message).await
    }

    /// Resolve and convert a sub-batch, appending it to the open flush group. `Pending` owns its
    /// values, so the extracted chunk can be dropped afterwards.
    async fn prepare<'ext>(
        &mut self,
        tx: &mut Transaction,
        extracted: &[Extracted<'ext>],
        batch: Vec<(usize, Collector)>,
        record_offset: usize,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        if self.group.is_empty() {
            self.group_first_message = self.messages.len();
        }
        let own_ids = self.existing_own_ids(tx, extracted, &batch).await?;
        let mut answers = self.answer(tx, &batch).await?;
        let mut pending = Vec::with_capacity(batch.len());
        for (index_in_batch, _) in &batch {
            let item = &extracted[*index_in_batch];
            let item_record_index = record_offset + *index_in_batch;
            let mut dbid = None;
            if let Some(text) = own(item, ".id") {
                match text.trim().parse::<i64>() {
                    Ok(id) if own_ids.contains(&id) => dbid = Some(id),
                    parsed => {
                        let shown = parsed.map_or_else(|_| text.to_owned(), |id| id.to_string());
                        self.messages.push(
                            ImportMessage::at(
                                MessageType::Error,
                                item.rows,
                                item_record_index,
                                format!("Unknown database identifier '{shown}'"),
                            )
                            .with_field(".id"),
                        );
                    }
                }
            }
            let (vals, issues, lifted) =
                self.converter()
                    .record(self.spec(), &item.record, &[], &mut answers)?;
            if let Some(question) = answers.missed.take() {
                return Err(Error::programming(format!(
                    "load asked about {question} without collecting the question first"
                )));
            }
            for issue in issues {
                self.messages
                    .push(field_message(item.rows, item_record_index, issue));
            }
            let xid = own(item, "id").map(|x| self.options.settings.qualify(x));
            pending.push(Pending {
                // an external id wins over `.id`
                dbid: if xid.is_some() { None } else { dbid },
                xid,
                vals,
                rows: item.rows,
                record: item_record_index,
                lifted,
            });
        }
        self.group.extend(pending);
        Ok(())
    }

    async fn existing_own_ids<'ext>(
        &self,
        tx: &mut Transaction,
        extracted: &[Extracted<'ext>],
        batch: &[(usize, Collector)],
    ) -> Result<HashSet<i64>> {
        let ids: Vec<i64> = batch
            .iter()
            .filter_map(|(i, _)| own(&extracted[*i], ".id"))
            // non-numeric ids cannot exist; they are reported as unknown in `process`
            .filter_map(|t| t.trim().parse::<i64>().ok())
            .filter(|id| *id > 0)
            .collect();
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let domain = Domain::condition("id", Operator::In, DValue::list(ids))?;
        let found = self.records.search(tx, &domain).await?;
        Ok(found.ids().into_iter().collect())
    }

    /// Answer every question of the batch: one name search per distinct (comodel, name), one
    /// statement per comodel for external ids and for existing ids.
    async fn answer(&self, tx: &mut Transaction, batch: &[(usize, Collector)]) -> Result<Answers> {
        let env = self.records.env();
        let mut answers = Answers::default();
        let mut names: Vec<(ModelId, String)> = Vec::new();
        let mut seen_names: HashSet<(ModelId, String)> = HashSet::new();
        let mut xids: BTreeMap<usize, (ModelId, Vec<XmlId>)> = BTreeMap::new();
        let mut dbids: BTreeMap<usize, (ModelId, Vec<i64>)> = BTreeMap::new();
        for (_, questions) in batch {
            for (model, name) in &questions.names {
                if seen_names.insert((*model, name.clone())) {
                    names.push((*model, name.clone()));
                }
            }
            for (model, xid) in &questions.xids {
                if answers.asked_xids.insert((*model, xid.clone()))
                    && let Ok(parsed) = XmlId::parse(xid)
                {
                    xids.entry(model.index())
                        .or_insert_with(|| (*model, Vec::new()))
                        .1
                        .push(parsed);
                }
            }
            for (model, id) in &questions.dbids {
                if answers.asked_dbids.insert((*model, *id)) {
                    dbids
                        .entry(model.index())
                        .or_insert_with(|| (*model, Vec::new()))
                        .1
                        .push(*id);
                }
            }
        }
        let options = NameSearch::new().operator(Operator::Eq);
        for (model, name) in names {
            let comodel = env.records(self.registry.model_by_id(model).name)?;
            let found = comodel.name_search_with(tx, &name, &options).await?;
            answers
                .names
                .insert((model, name), found.into_iter().map(|(id, _)| id).collect());
        }
        for (model, list) in xids.into_values() {
            let comodel = env.records(self.registry.model_by_id(model).name)?;
            for (xid, target) in model_data::xids_to_records(tx, &comodel, &list).await? {
                let key = xid.to_string();
                answers.xids.insert((model, key), target);
            }
        }
        for (model, list) in dbids.into_values() {
            let comodel = env.records(self.registry.model_by_id(model).name)?;
            // the converter only asks about positive ids
            let found = comodel.browse(list)?.exists(tx).await?;
            answers
                .dbids
                .extend(found.ids().into_iter().map(|id| (model, id)));
        }
        Ok(answers)
    }

    /// `first_message` is where this group's messages start.
    async fn flush(
        &mut self,
        tx: &mut Transaction,
        batch: Vec<Pending>,
        first_message: usize,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let attempt = {
            let all = batch.as_slice();
            let this = &*self;
            tx.savepoint(async |tx| this.load_records(tx, all).await)
                .await
        };
        let global = match attempt {
            Ok(ids) => {
                self.ids.extend(ids);
                return Ok(());
            }
            Err(e) if !is_data_error(&e) => return Err(e),
            Err(e) => (e.sqlstate().is_none())
                .then(|| batch[0].message(MessageType::Error, e.message().to_owned())),
        };
        let mut errors = 0usize;
        for (position, item) in batch.iter().enumerate() {
            let attempt = {
                let one = std::slice::from_ref(item);
                let this = &*self;
                tx.savepoint(async |tx| this.load_records(tx, one).await)
                    .await
            };
            match attempt {
                Ok(ids) => self.ids.extend(ids),
                Err(e) if !is_data_error(&e) => return Err(e),
                Err(e) => {
                    let message = self.record_error(item, &e);
                    self.messages.push(message);
                    errors += 1;
                }
            }
            if errors >= 10 && errors.saturating_mul(10) > position {
                self.messages
                    .push(ImportMessage::new(MessageType::Warning, TOO_MANY));
                break;
            }
        }
        if errors > 0
            && let Some(global) = global
            && !self.messages[first_message..].contains(&global)
        {
            self.messages.insert(0, global);
        }
        Ok(())
    }

    /// The message of a record that failed alone.
    fn record_error(&self, item: &Pending, e: &Error) -> ImportMessage {
        if e.sqlstate().is_none() {
            return item.message(MessageType::Error, e.message().to_owned());
        }
        // messages of other database errors carry no prefix
        let bare = e.message().strip_prefix(DB_PREFIX).unwrap_or(e.message());
        // psycopg2 renders a plain database error with a final newline
        let text = if e.kind() == ErrorKind::Database {
            format!("{bare}\n")
        } else {
            bare.to_owned()
        };
        let mut message = item.message(MessageType::Error, text);
        message.field = self.error_column(e);
        message
    }

    /// The field a database error names, when it is a single column of this model's table.
    fn error_column(&self, e: &Error) -> Option<String> {
        let mut source = std::error::Error::source(e);
        while let Some(err) = source {
            if let Some(pg) = err.downcast_ref::<tokio_postgres::Error>() {
                let db = pg.as_db_error()?;
                let spec = self.spec();
                return (db.table() == Some(spec.table.as_str()))
                    .then(|| db.column())
                    .flatten()
                    .filter(|c| spec.field(c).is_some())
                    .map(str::to_owned);
            }
            source = err.source();
        }
        None
    }

    /// Create or update the records of `batch` and give new ones their external ids
    /// ([`model_data::load_records`]); the ids in batch order.
    async fn load_records(&self, tx: &mut Transaction, batch: &[Pending]) -> Result<Vec<i64>> {
        let update = self.options.mode == Mode::Update;
        // a malformed external id (empty name, spaces) is a data error of its record
        let items: Vec<LoadRecord> = batch
            .iter()
            .map(|p| {
                let xid = p
                    .xid
                    .as_deref()
                    .map(|x| XmlId::parse(x).map_err(|e| Error::validation(e.to_string())))
                    .transpose()?;
                let item = LoadRecord::new(xid, p.vals.clone(), self.options.noupdate);
                Ok(match p.dbid {
                    Some(id) => item.with_db_id(id),
                    None => item,
                })
            })
            .collect::<Result<_>>()?;
        let records = model_data::load_records(tx, self.records, items, update).await?;
        let ids = records.ids();
        for (item, parent_id) in batch.iter().zip(&ids) {
            if !item.lifted.is_empty() {
                self.load_lifted(tx, *parent_id, &item.lifted, update)
                    .await?;
            }
        }
        Ok(ids)
    }

    /// Load lifted child lines whose parent was just created/updated, assigning them their own
    /// external ids and setting their inverse field to `parent_id`.
    fn load_lifted<'b>(
        &'b self,
        tx: &'b mut Transaction,
        parent_id: i64,
        lifted: &'b [LiftedLine],
        update: bool,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'b>> {
        Box::pin(async move {
            let env = self.records.env();
            for line in lifted {
                let comodel_spec = self.registry.model_by_id(line.comodel);
                let comodel_records = env.records(comodel_spec.name)?;
                let field_spec = comodel_spec.field(&line.inverse).ok_or_else(|| {
                    Error::validation(format!(
                        "inverse field '{}' does not exist on comodel '{}'",
                        line.inverse, comodel_spec.name
                    ))
                })?;
                if !matches!(field_spec.ty, FieldType::Many2one { .. }) {
                    return Err(Error::validation(format!(
                        "inverse field '{}' on comodel '{}' is not a many2one",
                        line.inverse, comodel_spec.name
                    )));
                }
                let mut vals = line.vals.clone();
                if let Some(existing) = vals.get(&line.inverse)
                    && *existing != Cell::Null
                    && *existing != Cell::Id(parent_id)
                {
                    return Err(Error::validation(format!(
                        "conflicting value for inverse field '{}' on lifted line with xid '{}'",
                        line.inverse, line.xid
                    )));
                }
                vals.insert(line.inverse.clone().into(), Cell::Id(parent_id));
                let xid = XmlId::parse(&line.xid).map_err(|e| Error::validation(e.to_string()))?;
                let item = LoadRecord::new(Some(xid), vals, self.options.noupdate);
                let child_records =
                    model_data::load_records(tx, &comodel_records, vec![item], update).await?;
                if !line.lifted.is_empty() {
                    let child_id = child_records.ids().first().copied().ok_or_else(|| {
                        Error::validation(format!(
                            "failed to get created id for lifted child line with xid '{}'",
                            line.xid
                        ))
                    })?;
                    self.load_lifted(tx, child_id, &line.lifted, update).await?;
                }
            }
            Ok(())
        })
    }
}

fn field_message(rows: RowSpan, record: usize, found: FieldIssue) -> ImportMessage {
    let issue = found.issue;
    let mut message = ImportMessage::at(
        issue.kind,
        rows,
        record,
        issue.template.render(&found.label),
    );
    if !issue.record_level {
        message.field = Some(found.field.to_owned());
    }
    if let Some(extra) = issue.extra {
        message.field_name = Some(found.label);
        message.moreinfo = extra.moreinfo;
        message.field_path = extra.field_path;
        message.field_type = extra.field_type;
        message.value = extra.value;
    }
    message
}
