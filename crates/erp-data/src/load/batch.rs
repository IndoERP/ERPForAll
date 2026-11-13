//! The batching window of the `<record>` pipeline (design §3.4).
//!
//! The session instead appends consecutive
//! records of one model and one environment to a [`Window`] and writes the window as one batch,
//! so a flush costs a fixed number of statements whatever its size:
//!
//! | Step | Statements |
//! |---|---|
//! | external ids of the window (and the sub-records of `noupdate` records), only when a decision needs them | 1 |
//! | existence of the rows they name, same condition | 1 per model |
//! | references of the window that are not in the session's cache | 1 |
//! | `model_data::load_records_cached`: lookup, create, update, upsert of the ids | 3-5 |
//!
//! A window is flushed before anything that could observe or depend on records still waiting in
//! it: a record of another model or environment, a record that names an external id pending in
//! the window (`ref=`, a literal `ref()` in an `eval`, `uid`, a context expression), a record
//! whose own external id is pending (the second one updates the first), a record that reads the
//! database while it is evaluated (`search=`, a `ref()` with a computed name, the value forms of
//! ), a record with sub-records (the children need its id), any operation that is not a
//! `<record>`, the end of the file, and when it holds [`BatchLimits`] records or bytes.
//!
//! The window holds the operations themselves (clones) and ids, never `Records`: it is flushed
//! at the end of every file, so a switch of the stage registry cannot leave it stale handles.
//!
//! The values (`:409-469`) are
//! evaluated afterwards, in order, only for records that were not skipped. A database error of
//! the write itself belongs to the whole batch and is reported at its first record, naming the
//! batch.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use erp_fields::{Error, Value};
use erp_orm::Transaction;

use super::error::{DataNotice, NoticeKind, OpLabel};
use super::expr::Known;
use super::record::{self, Built};
use super::{FileContext, LoadError, LoadErrorKind, LoadSession, env};
use crate::ast::{FieldValue, OpEnv, RecordOp, XmlId};
use crate::model_data::{self, LoadRecord, XidRecord};
use crate::reader::ConvertMode;

/// How much one batch may hold before it is written. The defaults are the ceilings; a host can
/// only lower them ([`BatchLimits::with_max_records`], [`BatchLimits::with_max_bytes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BatchLimits {
    max_records: usize,
    max_bytes: usize,
}

impl BatchLimits {
    /// Records in one batch (ceiling).
    pub const MAX_RECORDS: usize = 1_000;
    /// Bytes of field values in one batch (ceiling): text, expressions and markup, plus a fixed
    /// allowance per field.
    pub const MAX_BYTES: usize = 8 << 20;

    /// Lower the number of records per batch.
    ///
    /// # Errors
    /// `Programming` when `max` is zero (zero never means unlimited) or above
    /// [`BatchLimits::MAX_RECORDS`].
    pub fn with_max_records(mut self, max: usize) -> Result<Self, Error> {
        if max == 0 || max > Self::MAX_RECORDS {
            return Err(Error::programming(format!(
                "a batch holds 1 to {} records, not {max}",
                Self::MAX_RECORDS
            )));
        }
        self.max_records = max;
        Ok(self)
    }

    /// Lower the number of bytes per batch.
    ///
    /// # Errors
    /// `Programming` when `max` is zero or above [`BatchLimits::MAX_BYTES`].
    pub fn with_max_bytes(mut self, max: usize) -> Result<Self, Error> {
        if max == 0 || max > Self::MAX_BYTES {
            return Err(Error::programming(format!(
                "a batch holds 1 to {} bytes of field values, not {max}",
                Self::MAX_BYTES
            )));
        }
        self.max_bytes = max;
        Ok(self)
    }

    /// Records per batch.
    pub fn max_records(&self) -> usize {
        self.max_records
    }

    /// Bytes of field values per batch.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            max_records: Self::MAX_RECORDS,
            max_bytes: Self::MAX_BYTES,
        }
    }
}

/// What became of a record of a flushed window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Created or updated (or left alone by the model's own `noupdate` rule): the record's id.
    Loaded(i64),
    /// Not loaded: skipped by `noupdate`, or by an unresolved `ref=` with `forcecreate="0"`.
    Skipped,
}

/// One record waiting in the window.
pub(crate) struct Item {
    op: Arc<RecordOp>,
    /// The inverse field and the parent's id of a sub-record.
    extra: Option<(String, i64)>,
    /// The top-level operation this record belongs to (its `auto_sequence` counter).
    tree: u64,
    /// The external id, qualified with the module of the file.
    xid: Option<XmlId>,
    /// The id names another module than the file's.
    foreign: bool,
    /// External ids the record reads (`ref=`, `ref('…')`, `uid`, contexts), qualified.
    refs: Vec<String>,
}

/// What makes two records fit in one window.
#[derive(PartialEq)]
struct Key {
    model: String,
    env: OpEnv,
    update: bool,
    file: FileContext,
    /// Records with an external id carry the `install_*` context keys, records without do not.
    with_xid: bool,
}

/// The records collected since the last flush.
pub(crate) struct Window {
    key: Key,
    items: Vec<Item>,
    pending: HashSet<(String, String)>,
    bytes: usize,
}

/// A record to append to the window.
pub(crate) struct Push<'a> {
    pub(crate) file: &'a FileContext,
    pub(crate) op: Arc<RecordOp>,
    pub(crate) xid: Option<XmlId>,
    pub(crate) foreign: bool,
    pub(crate) extra: Option<(String, i64)>,
    pub(crate) tree: u64,
    pub(crate) refs: Vec<String>,
    /// Write the window now, whatever its size (the record has sub-records).
    pub(crate) flush_now: bool,
}

fn pending_key(xid: &XmlId) -> (String, String) {
    if let Some(q) = xid.as_qualified() {
        (q.module().to_owned(), q.name().to_owned())
    } else {
        debug_assert!(false, "pending_key requires a qualified external id: {xid}");
        (String::new(), xid.name().to_owned())
    }
}

/// Whether the qualified external id `name` is waiting in the window.
pub(crate) fn is_pending(session: &LoadSession, name: &str) -> bool {
    let Some(window) = &session.window else {
        return false;
    };
    let Some((module, id)) = name.split_once('.') else {
        return false;
    };
    window.pending.contains(&(module.to_owned(), id.to_owned()))
}

/// Whether the window holds a record with this external id.
pub(crate) fn pending_xid(session: &LoadSession, xid: &XmlId) -> bool {
    session
        .window
        .as_ref()
        .is_some_and(|w| w.pending.contains(&pending_key(xid)))
}

/// Forget the window and the counters without writing (the transaction is being rolled back to a
/// savepoint).
pub(crate) fn discard(session: &mut LoadSession) {
    session.window = None;
    session.counters.clear();
    session.unwritten_targets.clear();
}

/// A rough size of a record's values, for the byte limit.
fn estimate(op: &RecordOp) -> usize {
    /// Allowance for one field's name, bookkeeping and a short value.
    const FIELD: usize = 64;
    op.fields()
        .iter()
        .map(|f| {
            FIELD
                + match f.value() {
                    FieldValue::Text(s) | FieldValue::Xml(s) | FieldValue::Html(s) => s.len(),
                    // the content of a file is counted when it is read (`take_read_bytes`)
                    FieldValue::Eval { expr, .. } => expr.as_str().len(),
                    _ => 0,
                }
        })
        .sum::<usize>()
        + FIELD
}

fn located(op: &RecordOp, kind: LoadErrorKind) -> LoadError {
    LoadError::at(op.source_file(), op.line(), OpLabel::of_record(op), kind)
}

/// Append a record to the window, flushing first when it does not fit, and again afterwards when
/// the window is full or `flush_now` is set. Returns the record's outcome when a flush covered it.
pub(crate) async fn push(
    session: &mut LoadSession,
    tx: &mut Transaction,
    p: Push<'_>,
) -> Result<Option<Outcome>, LoadError> {
    let update = p.file.mode() == ConvertMode::Update;
    let fits = session.window.as_ref().is_none_or(|w| {
        w.key.model == p.op.model()
            && w.key.update == update
            && w.key.with_xid == p.xid.is_some()
            && w.key.env == *p.op.env()
            && w.key.file == *p.file
    });
    if !fits {
        flush(session, tx).await?;
    }
    let limits = session.limits;
    let window = session.window.get_or_insert_with(|| Window {
        key: Key {
            model: p.op.model().to_owned(),
            env: p.op.env().clone(),
            update,
            file: p.file.clone(),
            with_xid: p.xid.is_some(),
        },
        items: Vec::new(),
        pending: HashSet::new(),
        bytes: 0,
    });
    window.bytes = window.bytes.saturating_add(estimate(&p.op));
    if let Some(xid) = &p.xid {
        window.pending.insert(pending_key(xid));
    }
    window.items.push(Item {
        op: p.op,
        extra: p.extra,
        tree: p.tree,
        xid: p.xid,
        foreign: p.foreign,
        refs: p.refs,
    });
    let full = window.items.len() >= limits.max_records || window.bytes >= limits.max_bytes;
    if p.flush_now || full {
        let mut outcomes = flush(session, tx).await?;
        return Ok(outcomes.pop());
    }
    Ok(None)
}

/// Write the window. Returns one outcome per record, in the order they were appended.
pub(crate) async fn flush(
    session: &mut LoadSession,
    tx: &mut Transaction,
) -> Result<Vec<Outcome>, LoadError> {
    let Some(window) = session.window.take() else {
        return Ok(Vec::new());
    };
    let result = run_window(session, tx, &window).await;
    session.counters.retain_open();
    result
}

/// What to do with a record, decided before its values are evaluated.
enum Decision {
    Run,
    /// Leave it alone; mark these external ids loaded (the record's and those of its existing
    /// sub-records).
    Skip(Vec<XmlId>),
    Fail(LoadErrorKind),
}

/// `xid` qualified with `module` when it has no module part.
pub(crate) fn qualified(xid: &XmlId, module: &str) -> Result<XmlId, crate::ast::InvalidXmlId> {
    if xid.module().is_some() {
        Ok(xid.clone())
    } else {
        XmlId::qualified(module, xid.name())
    }
}

/// The external ids of the `<record id=…>` elements nested in `op`'s fields, at any depth
/// (`rec.xpath('.//record[@id]')`).
fn descendant_ids(op: &RecordOp, module: &str, out: &mut Vec<XmlId>) -> Result<(), LoadErrorKind> {
    for field in op.fields() {
        if let FieldValue::SubRecords(children) = field.value() {
            for child in children {
                if let Some(id) = child.id() {
                    out.push(qualified(id, module).map_err(record::invalid)?);
                }
                descendant_ids(child, module, out)?;
            }
        }
    }
    Ok(())
}

/// Which of the rows name a record that exists (one `exists` per model).
async fn existing_records(
    tx: &mut Transaction,
    env: &erp_orm::Env,
    rows: &HashMap<XmlId, XidRecord>,
) -> Result<HashMap<XmlId, i64>, Error> {
    let mut by_model: HashMap<&str, Vec<i64>> = HashMap::new();
    for row in rows.values() {
        if let Some(id) = row.res_id {
            by_model.entry(row.model.as_str()).or_default().push(id);
        }
    }
    let mut alive: HashSet<(&str, i64)> = HashSet::new();
    for (model, ids) in by_model {
        let found = env
            .records(model)?
            .sudo(true)
            .browse(ids)?
            .exists(tx)
            .await?;
        for id in found.ids() {
            alive.insert((model, id));
        }
    }
    Ok(rows
        .iter()
        .filter_map(|(xid, row)| {
            let id = row.res_id?;
            alive
                .contains(&(row.model.as_str(), id))
                .then(|| (xid.clone(), id))
        })
        .collect())
}

#[allow(
    clippy::too_many_lines,
    reason = "one pass over the window, in the reference ERP's order"
)]
async fn run_window(
    session: &mut LoadSession,
    tx: &mut Transaction,
    window: &Window,
) -> Result<Vec<Outcome>, LoadError> {
    let Some(first) = window.items.first() else {
        return Ok(Vec::new());
    };
    let key = &window.key;
    let module = key.file.module();
    let update = key.update;
    let guard = update && key.env.noupdate();
    let here = |item: &Item, kind: LoadErrorKind| located(&item.op, kind);
    let orm = |item: &Item, e: Error| located(&item.op, LoadErrorKind::Orm(e));

    let mut known: HashMap<String, Known> = HashMap::new();
    let env = env::operation_env(session, tx, &key.file, &key.env, &mut known)
        .await
        .map_err(|k| here(first, k))?;
    let mut records = env.records(&key.model).map_err(|e| orm(first, e))?;
    if key.with_xid {
        records = env::with_install_keys(&records, &key.file);
        // every write that carries `install_module` carries the load state, so a view written by
        // another model's override sees it too (review A-S2); `write_batch` refreshes it before
        // each batch it writes (review A2-S5)
        records = session.with_load_state(&records, key.file.module());
        if key.model == env::VIEW_MODEL {
            // `load_records` tells each view write how far its batch got (review A-R1)
            records = records.with_context([(super::load_state::TRACK_BATCH, Value::Bool(true))]);
        }
    }
    let registry = std::sync::Arc::clone(tx.registry());

    // 1. what the database knows about the external ids of the window, when a decision needs
    // it: the `noupdate` skip and foreign ids. A plain window leaves the lookup (and the check
    // of the model of an existing id) to `load_records`.
    let needs_rows = guard || window.items.iter().any(|i| i.foreign);
    let mut rows: HashMap<XmlId, XidRecord> = HashMap::new();
    let mut existing: HashMap<XmlId, i64> = HashMap::new();
    if needs_rows {
        let mut wanted: Vec<XmlId> = Vec::new();
        for item in &window.items {
            if let Some(xid) = &item.xid {
                wanted.push(xid.clone());
                if guard {
                    descendant_ids(&item.op, module, &mut wanted).map_err(|k| here(item, k))?;
                }
            }
        }
        rows = model_data::lookup_cached(tx, &env, &mut session.cache, &wanted)
            .await
            .map_err(|e| orm(first, e))?;
        existing = existing_records(tx, &env, &rows)
            .await
            .map_err(|e| orm(first, e))?;
    }

    // 2. the decisions, per record, in document order
    let mut decisions: Vec<Decision> = Vec::with_capacity(window.items.len());
    for item in &window.items {
        decisions.push(decide(item, key, guard, &rows, &existing, &registry));
    }

    // 3. the references of the records that will be evaluated, in one lookup
    let mut names: Vec<String> = Vec::new();
    for (item, decision) in window.items.iter().zip(&decisions) {
        if matches!(decision, Decision::Run) {
            names.extend(item.refs.iter().cloned());
        }
    }
    session
        .resolve(tx, &names, &mut known)
        .await
        .map_err(|k| here(first, k))?;

    // 4. the values, in order
    let spec = records.spec();
    let mut outcomes = vec![Outcome::Skipped; window.items.len()];
    let mut batch: Vec<LoadRecord> = Vec::new();
    let mut ran: Vec<usize> = Vec::new();
    let mut built_bytes: usize = 0;
    session.take_read_bytes();
    for (k, (item, decision)) in window.items.iter().zip(decisions).enumerate() {
        match decision {
            Decision::Fail(kind) => return Err(here(item, kind)),
            Decision::Skip(marks) => {
                for xid in &marks {
                    session.mark_loaded(xid);
                }
                continue;
            }
            Decision::Run => {}
        }
        let built =
            record::build_values(session, tx, &key.file, &records, &item.op, spec, &mut known)
                .await
                .map_err(|kind| here(item, kind))?;
        let mut pairs = match built {
            Built::Values(pairs) => pairs,
            Built::UnresolvedRef { field, reference } => {
                // keyed by external id: the same id written earlier in this file loses its
                // translations too (it fails loudly as an absent translation, never as a wrong one)
                if let Some(xid) = &item.xid {
                    session.unwritten_targets.insert(xid.to_string());
                }
                session.notices.push(DataNotice::new(
                    module,
                    key.file.path(),
                    NoticeKind::SkippedUnresolvedRef {
                        xid: item.xid.as_ref().map(ToString::to_string),
                        field,
                        reference,
                    },
                ));
                continue;
            }
        };
        if let Some((inverse, parent)) = &item.extra {
            set(&mut pairs, inverse, Value::Int(*parent));
        }
        if spec.field("sequence").is_some()
            && !pairs.iter().any(|(n, _)| n == "sequence")
            && let Some(number) = session.counters.next(item.tree)
        {
            pairs.push(("sequence".to_owned(), Value::Int(number)));
        }
        if let Some(xid) = &item.xid
            && spec.field("xmlid").is_some()
            && !pairs.iter().any(|(n, _)| n == "xmlid")
        {
            pairs.push(("xmlid".to_owned(), Value::str(xid.to_string())));
        }
        let vals = records
            .vals_from_values(pairs.iter().map(|(n, v)| (n, v)))
            .map_err(|e| orm(item, e))?;
        batch.push(LoadRecord::new(item.xid.clone(), vals, key.env.noupdate()));
        ran.push(k);
        // the content of the files the record read counts toward the batch's bytes: write what is
        // built when it passes the limit, so a window never holds more file content than that
        built_bytes = built_bytes.saturating_add(session.take_read_bytes());
        if built_bytes >= session.limits.max_bytes {
            write_batch(
                session,
                tx,
                window,
                &records,
                &mut batch,
                &mut ran,
                &mut outcomes,
            )
            .await?;
            built_bytes = 0;
        }
    }

    // 5. the rest as one batch
    write_batch(
        session,
        tx,
        window,
        &records,
        &mut batch,
        &mut ran,
        &mut outcomes,
    )
    .await?;
    Ok(outcomes)
}

/// Write the built records of a window with one `load_records` and record their outcomes; the
/// lists are left empty.
async fn write_batch(
    session: &mut LoadSession,
    tx: &mut Transaction,
    window: &Window,
    records: &erp_orm::Records,
    batch: &mut Vec<LoadRecord>,
    ran: &mut Vec<usize>,
    outcomes: &mut [Outcome],
) -> Result<(), LoadError> {
    if batch.is_empty() {
        return Ok(());
    }
    let key = &window.key;
    // the load state as of this write, not of the window's start: the `noupdate` records the
    // window skipped and an earlier batch of the window are loaded by now (review A2-S5)
    let fresh;
    let records = if key.with_xid {
        fresh = session.with_load_state(records, key.file.module());
        &fresh
    } else {
        records
    };
    let loaded_batch = std::mem::take(batch);
    let loaded =
        model_data::load_records_cached(tx, records, loaded_batch, key.update, &mut session.cache)
            .await
            .map_err(|e| batch_error(window, ran, LoadErrorKind::Orm(e)))?;
    let ids = loaded.ids();
    if ids.len() != ran.len() {
        return Err(batch_error(
            window,
            ran,
            LoadErrorKind::Internal(format!(
                "loading {} records of '{}' returned {} records",
                ran.len(),
                key.model,
                ids.len()
            )),
        ));
    }
    for (&k, id) in ran.iter().zip(ids) {
        outcomes[k] = Outcome::Loaded(id);
        if let Some(xid) = &window.items[k].xid {
            session.mark_loaded(xid);
        }
    }
    batch.clear();
    ran.clear();
    Ok(())
}

/// Replace or append a value.
fn set(pairs: &mut Vec<(String, Value)>, name: &str, value: Value) {
    match pairs.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = value,
        None => pairs.push((name.to_owned(), value)),
    }
}

/// An error of the write, located at the batch's first record, or exactly when only one record
/// was written.
fn batch_error(window: &Window, ran: &[usize], kind: LoadErrorKind) -> LoadError {
    let (Some(&first), Some(&last)) = (ran.first(), ran.last()) else {
        return LoadError::without_place(kind);
    };
    let item = &window.items[first];
    if ran.len() == 1 {
        return located(&item.op, kind);
    }
    let label = format!(
        "{}\n(one of {} <record> elements of model '{}', lines {} to {}, written as one batch)",
        OpLabel::of_record(&item.op),
        ran.len(),
        window.key.model,
        item.op.line(),
        window.items[last].op.line(),
    );
    LoadError::at(
        item.op.source_file(),
        item.op.line(),
        OpLabel::new(label),
        kind,
    )
}

fn decide(
    item: &Item,
    key: &Key,
    guard: bool,
    rows: &HashMap<XmlId, XidRecord>,
    existing: &HashMap<XmlId, i64>,
    registry: &erp_registry::Registry,
) -> Decision {
    let module = key.file.module();
    let Some(xid) = &item.xid else {
        // no external id: created in init mode; `load_records` refuses it in update mode;
        // the `noupdate` skip of such a record happened before the push
        return if key.update {
            Decision::Fail(LoadErrorKind::Orm(Error::validation(
                "Cannot update a record without specifying its id or xml_id",
            )))
        } else {
            Decision::Run
        };
    };
    let exists = existing.contains_key(xid);
    let forcecreate = item.op.forcecreate();
    if guard {
        if exists {
            // the sub-records with ids that exist are marked too
            let mut marks = vec![xid.clone()];
            let mut nested = Vec::new();
            if let Err(kind) = descendant_ids(&item.op, module, &mut nested) {
                return Decision::Fail(kind);
            }
            for sub in nested {
                if sub.module().is_some_and(|m| m != module)
                    && let Some(kind) = uninstalled(&sub, registry)
                {
                    return Decision::Fail(kind);
                }
                if existing.contains_key(&sub) {
                    marks.push(sub);
                }
            }
            return Decision::Skip(marks);
        }
        if forcecreate == Some(false) {
            return Decision::Skip(Vec::new());
        }
    }
    if item.foreign && !exists {
        match forcecreate {
            Some(true) => {}
            Some(false) if key.env.noupdate() => return Decision::Skip(Vec::new()),
            _ => {
                return Decision::Fail(LoadErrorKind::MissingForeignRecord {
                    xid: xid.to_string(),
                });
            }
        }
    }
    if let Some(row) = rows.get(xid)
        && row.model != key.model
    {
        return Decision::Fail(LoadErrorKind::Orm(Error::validation(format!(
            "For external id {xid} when trying to create/update a record of model {} found \
             record of different model {} ({})",
            key.model, row.model, row.id
        ))));
    }
    Decision::Run
}

/// `_test_xml_id`: an id of another module needs that module in this
/// install step.
pub(crate) fn uninstalled(xid: &XmlId, registry: &erp_registry::Registry) -> Option<LoadErrorKind> {
    let module = xid.module()?;
    if registry.addons().iter().any(|a| a.name == module) {
        None
    } else {
        Some(LoadErrorKind::UninstalledModule {
            xid: xid.to_string(),
        })
    }
}
