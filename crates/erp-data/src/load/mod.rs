//! Loading data files into the database (design;, §8,
//! §9, §12).
//!
//! The readers of this crate (`XmlDataReader`, `parse_csv`) turn a data file into typed
//! operations; a [`LoadSession`] writes them.
//!
//! ```ignore
//! let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
//! let file = FileContext::new("m", "data/cat.xml", ConvertMode::Init);
//! let mut ops = XmlDataReader::new(xml, "data/cat.xml", "m", false);
//! session.load_file(&mut tx, &file, &mut ops).await?;
//! ```
//!
//! The loader is model-agnostic: it works on whatever models the transaction's registry has, and
//! the module system hands it the registry of the module being loaded. A session does not
//! outlive its transaction; it holds no `Records` (a registry switch between files cannot leave
//! it stale handles).
//!
//! Work is divided by file: the record pipeline and its window (`record`, `batch`, `sequence`,
//! `process_end`), `function`/`delete`/`csv`, the value forms with `menu`/`template`
//! (`values_ext`), and the uninstall `purge`. A feature a later slice adds fails with
//! [`LoadErrorKind::Unsupported`] or [`LoadErrorKind::ModelArrivesLater`] naming that slice.

mod batch;
mod csv;
mod delete;
mod dispatch;
mod env;
pub mod error;
mod expr;
pub mod files;
mod function;
pub mod load_state;
mod menu;
mod process_end;
mod purge;
mod record;
mod sequence;
mod template;
mod values;
mod values_ext;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use erp_expr::ExprValue;
use erp_fields::Value;
use erp_orm::Transaction;

use crate::ast::{DataOperation, Expr, XmlId};
use crate::error::ParseError;
use crate::model_data::{self, XidCache};
use crate::reader::ConvertMode;

pub use batch::BatchLimits;
pub use env::INSTALL_DEMO;
pub use error::{DataNotice, LoadError, LoadErrorKind, NoticeKind, OpLabel};
pub use files::{AddonFiles, FileReadError, NoFiles};
pub use purge::{CascadedNamedView, PurgeReport, SCHEMA_MODELS, purge_module_data};

use expr::Known;

/// What the module system gives a session when it opens it.
#[derive(Clone)]
#[non_exhaustive]
pub struct SessionOptions {
    series: String,
    files: Arc<dyn AddonFiles>,
}

impl SessionOptions {
    /// `20.0`);
    /// `files` reads the files of the addons being loaded.
    pub fn new(series: impl Into<String>, files: Arc<dyn AddonFiles>) -> Self {
        Self {
            series: series.into(),
            files,
        }
    }

    /// The addon files reader.
    pub fn files(&self) -> &Arc<dyn AddonFiles> {
        &self.files
    }
}

impl std::fmt::Debug for SessionOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOptions")
            .field("series", &self.series)
            .finish_non_exhaustive()
    }
}

/// The file being loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileContext {
    module: String,
    path: String,
    /// `<module>/<path>`, built once (review A3-R8).
    install_filename: Arc<str>,
    mode: ConvertMode,
    demo: bool,
}

impl FileContext {
    /// A data file `path` of `module` loaded in `mode`.
    pub fn new(module: &str, path: &str, mode: ConvertMode) -> Self {
        Self {
            module: module.to_owned(),
            path: path.to_owned(),
            install_filename: Arc::from(format!("{module}/{path}")),
            mode,
            demo: false,
        }
    }

    /// Whether the file is a demo file (the context then carries `install_demo`).
    #[must_use]
    pub fn demo(mut self, demo: bool) -> Self {
        self.demo = demo;
        self
    }

    /// The module.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// The path as listed in the manifest.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The one place the value is built, for record windows and CSV imports alike
    /// (review A2-R2), shared rather than formatted per write (review A3-R8).
    pub fn install_filename(&self) -> &Arc<str> {
        &self.install_filename
    }

    /// `init` or `update`.
    pub fn mode(&self) -> ConvertMode {
        self.mode
    }

    /// Whether this is a demo file.
    pub fn is_demo(&self) -> bool {
        self.demo
    }
}

/// A point of a session to go back to when a savepoint is rolled back (the demo files).
///
/// A mark is good until the session goes back to it or to an earlier mark: marks taken after the
/// point it went back to are stale, and so is a mark whose notices were already handed out by
/// [`LoadSession::take_notices`]; [`LoadSession::rollback_to`] refuses both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMark {
    session: u64,
    /// Identifies the mark among the session's marks that are still good.
    serial: u64,
    loaded: usize,
    /// Notices emitted by the session when the mark was taken (handed out ones included).
    notices: usize,
    /// Translation entries collected when the mark was taken.
    translations: usize,
}

static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// One load session: see the [module documentation](self).
pub struct LoadSession {
    id: u64,
    options: SessionOptions,
    cache: XidCache,
    /// Module to the names loaded in it (looked up by borrowed keys).
    loaded: HashMap<String, HashSet<String>>,
    loaded_count: usize,
    loaded_order: Vec<(String, String)>,
    notices: Vec<DataNotice>,
    window: Option<batch::Window>,
    counters: sequence::Counters,
    limits: BatchLimits,
    /// Bytes of addon files read since the last [`LoadSession::take_read_bytes`].
    read_bytes: usize,
    /// Serials of the marks still good, oldest first.
    marks: Vec<u64>,
    next_mark: u64,
    /// Notices handed out by [`LoadSession::take_notices`] so far.
    notices_taken: usize,
    /// The module operation and the load-state context values ([`load_state`]).
    operation: load_state::OperationState,
    /// Translations of the loaded files, saved at the end of each file.
    importer: crate::i18n::TranslationImporter,
    translation_mode: crate::i18n::OverwriteMode,
    /// External ids (`module.name`) of records of the current file that were skipped for an
    /// unresolved `ref=`: they were not written, so their collected translations are dropped.
    unwritten_targets: HashSet<String>,
}

impl LoadSession {
    /// A session with an empty cache and an empty loaded set.
    pub fn new(options: SessionOptions) -> Self {
        Self {
            id: NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            options,
            cache: XidCache::new(),
            loaded: HashMap::new(),
            loaded_count: 0,
            loaded_order: Vec::new(),
            notices: Vec::new(),
            window: None,
            counters: sequence::Counters::default(),
            limits: BatchLimits::default(),
            read_bytes: 0,
            marks: Vec::new(),
            next_mark: 0,
            notices_taken: 0,
            operation: load_state::OperationState::default(),
            importer: crate::i18n::TranslationImporter::new(),
            translation_mode: crate::i18n::OverwriteMode::default(),
            unwritten_targets: HashSet::new(),
        }
    }

    /// How the translations of the next files treat values the database already holds (default
    /// [`crate::i18n::OverwriteMode::NoOverwrite`]).
    pub fn set_translation_mode(&mut self, mode: crate::i18n::OverwriteMode) {
        self.translation_mode = mode;
    }

    /// The modules whose data this operation installs or upgrades, in load order. The views a
    /// write checks skip those of modules not loaded yet (they still hold the archs of before
    /// the operation). Once per session.
    ///
    /// # Errors
    ///
    /// [`load_state::OperationOrderError::AlreadyBegun`] (nothing changes) when the session has
    /// begun an operation already.
    pub fn begin_operation<'m>(
        &mut self,
        modules: impl IntoIterator<Item = &'m str>,
    ) -> Result<(), load_state::OperationOrderError> {
        self.operation
            .begin(modules.into_iter().map(str::to_owned).collect())
    }

    /// The data of `module` is loaded: its views count as loaded for the next modules' checks.
    /// Returns the names (without the module) the operation loaded for it, sorted: the module
    /// system validates the `noupdate` views among them after an upgrade
    /// (`ir.ui.view._validate_module_views`).
    ///
    /// # Errors
    ///
    /// [`load_state::OperationOrderError`] (nothing changes) before [`Self::begin_operation`],
    /// for a module that is not one of the operation, or for a module finished already.
    pub fn finish_module(
        &mut self,
        module: &str,
    ) -> Result<Vec<String>, load_state::OperationOrderError> {
        self.operation.finish(module)?;
        let mut names: Vec<String> = self
            .loaded_names(module)
            .into_iter()
            .map(str::to_owned)
            .collect();
        names.sort_unstable();
        Ok(names)
    }

    /// The names (without the module) of the external ids of `module` the operation loaded, in
    /// no particular order.
    pub fn loaded_names(&self, module: &str) -> Vec<&str> {
        // a module without loaded names has none to list
        self.loaded
            .get(module)
            .map(|names| names.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// The [`load_state::PENDING_MODULES`] and [`load_state::LOADED_NAMES`] context values for a
    /// write of `module`, shared with the previous ones when nothing changed.
    pub(crate) fn load_state_values(&mut self, module: &str) -> (Value, Value) {
        let loaded = &self.loaded;
        self.operation.values(module, || {
            loaded
                .get(module)
                .map(|names| names.iter().map(String::as_str).collect())
                .unwrap_or_default()
        })
    }

    /// `records` with the load-state keys for a write of `module` ([`load_state`]).
    pub(crate) fn with_load_state(
        &mut self,
        records: &erp_orm::Records,
        module: &str,
    ) -> erp_orm::Records {
        let (pending, loaded) = self.load_state_values(module);
        records.with_context([
            (load_state::PENDING_MODULES, pending),
            (load_state::LOADED_NAMES, loaded),
        ])
    }

    /// The number of load-state chunks of the current module (allocation and bound tests).
    #[doc(hidden)]
    pub fn load_state_chunks_for_tests(&self) -> usize {
        self.operation.chunk_count()
    }

    /// Mark `name` of `module` loaded and return the load-state values a write of `module` would
    /// carry: what an `ir.ui.view` window after a window of another model costs (the allocation
    /// pin `tests/alloc_load_state.rs`).
    #[doc(hidden)]
    ///
    /// # Errors
    ///
    /// The external id `module.name` is not valid.
    pub fn load_state_after_for_tests(
        &mut self,
        module: &str,
        name: &str,
    ) -> Result<(Value, Value), crate::ast::InvalidXmlId> {
        let xid = XmlId::qualified(module, name)?;
        self.mark_loaded(&xid);
        Ok(self.load_state_values(module))
    }

    /// Lower how much one batch of `<record>` elements may hold before it is written (the
    /// defaults are the ceilings, [`BatchLimits`]).
    pub fn set_batch_limits(&mut self, limits: BatchLimits) {
        self.limits = limits;
    }

    /// Load the operations of one file, in order. A parse error the iterator yields ends the file
    /// ([`LoadErrorKind::Parse`]).
    ///
    /// Consecutive `<record>` elements of one model are written together (design §3.4): the
    /// records of the file are all written when this returns.
    ///
    /// The external-id cache is emptied first: hooks and methods that ran between files may have
    /// changed `ir.model.data` behind it.
    ///
    /// # Errors
    /// A [`LoadError`] naming the file, line and operation; the transaction must be rolled back
    /// (or a savepoint with it): earlier operations of the file stay written in it.
    pub async fn load_file(
        &mut self,
        tx: &mut Transaction,
        file: &FileContext,
        operations: &mut (dyn Iterator<Item = Result<DataOperation, ParseError>> + Send),
    ) -> Result<(), LoadError> {
        self.cache.clear();
        let collected = self.importer.len();
        let result = self.load_operations(tx, file, operations).await;
        if result.is_err() {
            // records of the failed file that were never written must not leak into the next one
            batch::discard(self);
            self.importer.truncate(collected);
        }
        result
    }

    async fn load_operations(
        &mut self,
        tx: &mut Transaction,
        file: &FileContext,
        operations: &mut (dyn Iterator<Item = Result<DataOperation, ParseError>> + Send),
    ) -> Result<(), LoadError> {
        for item in operations {
            let op = item?;
            // boxed: the operation futures nest deeply, and the module system embeds this one
            Box::pin(dispatch::run(self, tx, file, op)).await?;
        }
        // boxed like the operations: the module system embeds these futures too
        Box::pin(batch::flush(self, tx)).await?;
        Box::pin(self.save_translations(tx, file)).await
    }

    /// Write the translations the file collected (its records exist now), then forget them.
    async fn save_translations(
        &mut self,
        tx: &mut Transaction,
        file: &FileContext,
    ) -> Result<(), LoadError> {
        let unwritten = std::mem::take(&mut self.unwritten_targets);
        self.importer.remove_targets(&unwritten);
        if self.importer.is_empty() {
            return Ok(());
        }
        let result = self.importer.save(tx, self.translation_mode).await;
        self.importer.clear();
        result.map(drop).map_err(|e| {
            LoadError::at(
                file.install_filename(),
                0,
                error::OpLabel::new(format!("translations of module {}", file.module())),
                LoadErrorKind::Orm(e),
            )
        })
    }

    /// The current point, before a savepoint the caller may roll back.
    pub fn mark(&mut self) -> SessionMark {
        let serial = self.next_mark;
        self.next_mark += 1;
        self.marks.push(serial);
        SessionMark {
            session: self.id,
            serial,
            loaded: self.loaded_order.len(),
            notices: self.notices_taken + self.notices.len(),
            translations: self.importer.len(),
        }
    }

    /// Forget what happened after `mark`: the external ids loaded, the notices, and the cache
    /// (its entries may name rows the rolled back savepoint created). The marks taken after
    /// `mark` become stale.
    ///
    /// # Errors
    /// `Programming` (nothing is changed) when `mark` was taken from another session, is stale
    /// (the session already went back to it or to an earlier mark), or when its notices were
    /// handed out by [`LoadSession::take_notices`] since.
    pub fn rollback_to(&mut self, mark: SessionMark) -> Result<(), erp_fields::Error> {
        if mark.session != self.id {
            return Err(erp_fields::Error::programming(
                "a session mark from another load session was given to rollback_to",
            ));
        }
        let Some(position) = self.marks.iter().position(|&serial| serial == mark.serial) else {
            return Err(erp_fields::Error::programming(
                "a stale session mark was given to rollback_to: the session already went back to \
                 it or to an earlier mark",
            ));
        };
        if mark.notices < self.notices_taken {
            return Err(erp_fields::Error::programming(
                "a session mark was given to rollback_to after take_notices handed out notices it \
                 covers; take the notices after the savepoint is settled",
            ));
        }
        self.marks.truncate(position);
        while self.loaded_order.len() > mark.loaded {
            if let Some((module, name)) = self.loaded_order.pop()
                && let Some(names) = self.loaded.get_mut(&module)
                && names.remove(&name)
            {
                self.loaded_count -= 1;
            }
        }
        self.notices.truncate(mark.notices - self.notices_taken);
        self.importer.truncate(mark.translations);
        self.cache.clear();
        // the loaded names went back: the load-state values are rebuilt
        self.operation.reset_names();
        // records of the failed file that were never written
        batch::discard(self);
        Ok(())
    }

    /// The bytes of addon files read since the last call (the batch counts them against its byte
    /// limit).
    pub(crate) fn take_read_bytes(&mut self) -> usize {
        std::mem::take(&mut self.read_bytes)
    }

    /// Empty the external-id cache (call after code that may change `ir.model.data`).
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// The notices collected since the last call.
    pub fn take_notices(&mut self) -> Vec<DataNotice> {
        let taken = std::mem::take(&mut self.notices);
        self.notices_taken += taken.len();
        taken
    }

    /// Whether the operation loaded `xid` (a record of a file, found by the `noupdate` skip, or
    /// a CSV row). An unqualified ID is never considered loaded.
    pub fn is_loaded(&self, xid: &XmlId) -> bool {
        match xid.module() {
            Some(m) => self.is_loaded_key(m, xid.name()),
            None => false,
        }
    }

    /// Whether the operation loaded `xid`.
    pub fn is_loaded_qualified(&self, xid: &crate::ast::QualifiedXmlId) -> bool {
        self.is_loaded_key(xid.module(), xid.name())
    }

    /// [`LoadSession::is_loaded`] for a stored `(module, name)` pair.
    pub(crate) fn is_loaded_key(&self, module: &str, name: &str) -> bool {
        self.loaded
            .get(module)
            .is_some_and(|names| names.contains(name))
    }

    /// Number of external ids loaded.
    pub fn loaded_count(&self) -> usize {
        self.loaded_count
    }

    /// The external-id cache.
    pub fn cache(&self) -> &XidCache {
        &self.cache
    }

    /// Run the end-of-run cleanup for the modules that were installed, upgraded or reinitialised
    /// (`_process_end`).
    ///
    /// # Errors
    /// A [`LoadError`] without a place: an ORM or database error of the deletes (a record that a
    /// module that stays still needs cannot be deleted).
    pub async fn process_end(
        &mut self,
        tx: &mut Transaction,
        updated: &[&str],
    ) -> Result<(), LoadError> {
        Box::pin(batch::flush(self, tx)).await?;
        let result = Box::pin(process_end::run(self, tx, updated)).await;
        // the cleanup deletes behind the cache
        self.cache.clear();
        result
    }

    pub(crate) fn mark_loaded(&mut self, xid: &XmlId) {
        let (module, name) = loaded_key(xid);
        if self
            .loaded
            .entry(module.clone())
            .or_default()
            .insert(name.clone())
        {
            self.loaded_count += 1;
            self.operation.note_loaded(&module, &name);
            self.loaded_order.push((module, name));
        }
    }

    /// Look up the external ids `names` (qualified strings) through the cache and record what is
    /// known about each in `known`: one statement for the names the cache lacks, none otherwise.
    pub(crate) async fn resolve(
        &mut self,
        tx: &mut Transaction,
        names: &[String],
        known: &mut HashMap<String, Known>,
    ) -> Result<(), LoadErrorKind> {
        let mut wanted: Vec<(String, XmlId)> = Vec::new();
        let mut asked: HashSet<&str> = HashSet::new();
        for name in names {
            if known.contains_key(name) || !asked.insert(name.as_str()) {
                continue;
            }
            let xid = XmlId::parse(name).map_err(LoadErrorKind::InvalidExternalId)?;
            wanted.push((name.clone(), xid));
        }
        if wanted.is_empty() {
            return Ok(());
        }
        let xids: Vec<XmlId> = wanted.iter().map(|(_, x)| x.clone()).collect();
        let env = tx.env();
        let found = model_data::lookup_cached(tx, &env, &mut self.cache, &xids)
            .await
            .map_err(LoadErrorKind::Orm)?;
        for (name, xid) in wanted {
            let state = match found
                .get(&xid)
                .and_then(|row| row.res_id.map(|id| (row, id)))
            {
                Some((row, id)) => Known::Record {
                    model: row.model.clone(),
                    id,
                },
                None => Known::Absent,
            };
            known.insert(name, state);
        }
        Ok(())
    }

    /// Evaluate `expr` (an `eval` or `context` expression) of a file of `module`.
    /// `ref('<literal>')` names are looked up first, with the rest of `known`; a name computed at
    /// run time is looked up after a first failure and the evaluation repeated (at most
    /// [`MAX_EVAL_ROUNDS`] lookup rounds).
    pub(crate) async fn eval(
        &mut self,
        tx: &mut Transaction,
        module: &str,
        expr: &Expr,
        known: &mut HashMap<String, Known>,
    ) -> Result<ExprValue, LoadErrorKind> {
        self.eval_with_model(tx, module, expr, None, known).await
    }

    /// Evaluate `expr` with an optional target `model` (binding `obj`).
    ///
    /// Each failed evaluation that names data it needs (external ids, record fields, searches)
    /// starts one lookup round that fetches all of it; the evaluation then runs again. More than
    /// [`MAX_EVAL_ROUNDS`] rounds fail with `LimitExceeded` naming the limit.
    ///
    /// Model and field
    /// names are checked against the registry before any SQL.
    pub(crate) async fn eval_with_model(
        &mut self,
        tx: &mut Transaction,
        module: &str,
        expr: &Expr,
        model: Option<&str>,
        known: &mut HashMap<String, Known>,
    ) -> Result<ExprValue, LoadErrorKind> {
        let source = expr.as_str();
        let eval_error = |error| LoadErrorKind::Eval {
            expr: source.to_owned(),
            error,
        };
        let ast = expr.parsed().map_err(|e| eval_error(e.clone()))?;
        let mut names = Vec::new();
        expr::literal_refs(ast, module, &mut names);
        self.resolve(tx, &names, known).await?;
        let now = expr::clock_now()
            .map_err(|message| eval_error(erp_expr::ExprError::value_error(message)))?;

        let mut cached_fields: HashMap<(String, i64, String), ExprValue> = HashMap::new();
        let mut cached_searches: HashMap<expr::SearchKey, Vec<i64>> = HashMap::new();
        let mut cached_groups: HashMap<String, bool> = HashMap::new();
        let mut cached_user_groups: HashMap<(i64, String), bool> = HashMap::new();

        let env = tx.env();
        let user_id = env.uid();
        let company_id = env.company_id().map_err(LoadErrorKind::Orm)?;
        let company_ids = env
            .companies()
            .map_err(LoadErrorKind::Orm)?
            .iter()
            .map(|c| c.get())
            .collect::<Vec<i64>>();
        let context_expr = expr::context_to_expr(env.context()).map_err(LoadErrorKind::Internal)?;

        let mut rounds = 0;
        loop {
            let registry = Arc::clone(tx.env().registry());
            let failure = match expr::evaluate(
                ast,
                module,
                model,
                &expr::Lookups {
                    known,
                    fields: &cached_fields,
                    searches: &cached_searches,
                    groups: &cached_groups,
                    user_groups: &cached_user_groups,
                    registry,
                    user_id,
                    company_id,
                    company_ids: company_ids.clone(),
                    context: context_expr.clone(),
                },
                now,
                &self.options.series,
            ) {
                Ok(value) => return Ok(value),
                Err(failure) => failure,
            };
            let has_missing = !failure.unknown.is_empty()
                || !failure.missing_fields.is_empty()
                || !failure.missing_searches.is_empty()
                || !failure.missing_groups.is_empty()
                || !failure.missing_user_groups.is_empty();
            if !has_missing {
                return Err(eval_error(failure.error));
            }
            if rounds == MAX_EVAL_ROUNDS {
                return Err(eval_error(erp_expr::ExprError::limit(format!(
                    "data expression lookup rounds exceeded (limit {MAX_EVAL_ROUNDS}); the \
                     last evaluation still needed data: {}",
                    failure.error
                ))));
            }
            rounds += 1;

            if !failure.unknown.is_empty() {
                self.resolve(tx, &failure.unknown, known).await?;
                names.extend(failure.unknown);
            }

            if !failure.missing_searches.is_empty() || !failure.missing_fields.is_empty() {
                Box::pin(batch::flush(self, tx))
                    .await
                    .map_err(|e| e.into_kind())?;
            }

            for req in failure.missing_searches {
                let key = req.key();
                if cached_searches.contains_key(&key) {
                    continue;
                }
                let found = eval_search(tx, &req).await?;
                cached_searches.insert(key, found);
            }

            for group_xmlid in failure.missing_groups {
                if cached_groups.contains_key(&group_xmlid) {
                    continue;
                }
                let in_group = eval_group(tx, &group_xmlid).await?;
                cached_groups.insert(group_xmlid, in_group);
            }

            for (uid, group_xmlid) in failure.missing_user_groups {
                let key = (uid, group_xmlid);
                if cached_user_groups.contains_key(&key) {
                    continue;
                }
                let in_group = eval_user_group(tx, key.0, &key.1).await?;
                cached_user_groups.insert(key, in_group);
            }

            for (model_name, id, field_name) in failure.missing_fields {
                let key = (model_name.clone(), id, field_name.clone());
                if cached_fields.contains_key(&key) {
                    continue;
                }
                let value = eval_read(tx, &model_name, id, &field_name).await?;
                cached_fields.insert(key, value);
            }
        }
    }
}

/// The ids `search(domain, offset=offset, limit=limit, order=order)` of a data expression finds on
/// `model`. At most [`erp_expr::MAX_SEARCH_RESULTS`] + 1 rows are fetched: the proxy fails on the
/// extra one, so an unlimited search never reads an unbounded id list.
async fn eval_search(
    tx: &mut Transaction,
    req: &expr::SearchRequirement,
) -> Result<Vec<i64>, LoadErrorKind> {
    let mut env = tx.env().clone().sudo(req.sudo);

    let ctx = expr::expr_to_context(&req.context_val).map_err(LoadErrorKind::Internal)?;
    env = env.with_context_replaced(ctx);

    if env.registry().model(&req.target_model).is_none() {
        return Err(LoadErrorKind::ModelMissing {
            what: "eval search".to_owned(),
            model: req.target_model.clone(),
        });
    }
    let domain_value =
        expr::to_value(&req.domain_val).map_err(|got| LoadErrorKind::NotAFieldValue {
            model: req.target_model.clone(),
            field: "search".to_owned(),
            got,
        })?;
    let domain = erp_orm::web::domain_from_value(&domain_value).map_err(LoadErrorKind::Orm)?;
    let fetch_cap = u64::try_from(erp_expr::MAX_SEARCH_RESULTS)
        .map_err(|_| LoadErrorKind::Internal("MAX_SEARCH_RESULTS exceeds u64".to_owned()))?
        .saturating_add(1);
    let opts = erp_orm::SearchOptions {
        offset: req.offset,
        limit: Some(req.limit.map_or(fetch_cap, |l| l.min(fetch_cap))),
        order: req.order.clone(),
        ..Default::default()
    };
    let found = env
        .records(&req.target_model)
        .map_err(LoadErrorKind::Orm)?
        .search_with(tx, &domain, &opts)
        .await
        .map_err(LoadErrorKind::Orm)?;
    Ok(found.ids().to_vec())
}

async fn eval_group(tx: &mut Transaction, group_xmlid: &str) -> Result<bool, LoadErrorKind> {
    let env = tx.env().clone();
    tx.has_group(&env, group_xmlid)
        .await
        .map_err(LoadErrorKind::Orm)
}

/// Whether `res.users(uid)` is in the group `group_xmlid` (`user.has_group` of another user).
/// The user must exist: a missing row fails loudly and never answers `false`. The question is
/// the user's, so superuser mode does not change it, and it is asked under the evaluating
/// principal's own environment, switched to that subject with `Env::with_user`.
/// Data files are trusted content: `Env::with_user` does not check the impersonation permission
/// yet, so any principal can ask about any existing user, and the existence read
/// is `sudo`, so a missing user is reported even to a principal that cannot read `res.users`.
async fn eval_user_group(
    tx: &mut Transaction,
    uid: i64,
    group_xmlid: &str,
) -> Result<bool, LoadErrorKind> {
    if uid <= 0 {
        return Err(LoadErrorKind::Orm(erp_fields::Error::missing(format!(
            "Record does not exist or has been deleted: res.users({uid}) (has_group('{group_xmlid}') in \
             a data expression)"
        ))));
    }
    let env = tx.env().clone();
    let user = env
        .sudo(true)
        .records("res.users")
        .map_err(LoadErrorKind::Orm)?
        .browse([uid])
        .map_err(LoadErrorKind::Orm)?;
    if user
        .exists(tx)
        .await
        .map_err(LoadErrorKind::Orm)?
        .is_empty()
    {
        return Err(LoadErrorKind::Orm(erp_fields::Error::missing(format!(
            "Record does not exist or has been deleted: res.users({uid}) (has_group('{group_xmlid}') in a data expression)"
        ))));
    }
    let user_env = env.with_user(uid);
    tx.has_group(&user_env, group_xmlid)
        .await
        .map_err(LoadErrorKind::Orm)
}

/// Field `field` of record `model(id)` as an expression value: a many2one a record of its comodel
/// (an empty recordset when empty), an x2many a recordset, other fields their raw value. The
/// field exists (the evaluator's resolver checked it against the registry).
///
/// An x2many is read bounded (`Records::read_x2many_capped`, at most
/// [`erp_expr::MAX_SEARCH_RESULTS`] + 1 rows): a value over the recordset limit fails with
/// `LimitExceeded` naming the model, field, record and limit before its ids are collected. The
/// exceptions are the values `read_x2many_capped` reads whole and counts afterwards: a value
/// already in the cache, and computed, related, `_inherits` or non-stored x2many fields.
async fn eval_read(
    tx: &mut Transaction,
    model: &str,
    id: i64,
    field: &str,
) -> Result<ExprValue, LoadErrorKind> {
    let env = tx.env();
    let Some(model_def) = env.registry().model(model) else {
        return Err(LoadErrorKind::ModelMissing {
            what: "eval field read".to_owned(),
            model: model.to_owned(),
        });
    };
    let Some(field_def) = model_def.field(field) else {
        return Err(LoadErrorKind::Internal(format!(
            "a data expression asked for field '{field}' of {model}, which the registry does \
             not have"
        )));
    };
    let record = env
        .records(model)
        .map_err(LoadErrorKind::Orm)?
        .browse([id])
        .map_err(LoadErrorKind::Orm)?;
    let missing = || {
        LoadErrorKind::Orm(erp_fields::Error::missing(format!(
            "Record does not exist or has been deleted: {model}({id}) (reading field '{field}' \
             in a data expression)"
        )))
    };
    if let erp_fields::FieldType::Many2many { comodel, .. }
    | erp_fields::FieldType::One2many { comodel, .. } = &field_def.ty
    {
        // the bounded read of a missing record finds no lines: check the row exists first, so it
        // fails as the scalar read does (review D2), never with an empty recordset
        if record
            .exists(tx)
            .await
            .map_err(LoadErrorKind::Orm)?
            .is_empty()
        {
            return Err(missing());
        }
        let cap = erp_expr::MAX_SEARCH_RESULTS;
        let expr = || format!("{model}({id}).{field}");
        let ids = match record
            .read_x2many_capped(tx, field, cap)
            .await
            .map_err(LoadErrorKind::Orm)?
        {
            erp_orm::CappedIds::Within(ids) => ids,
            erp_orm::CappedIds::OverCap => {
                return Err(LoadErrorKind::Eval {
                    expr: expr(),
                    error: erp_expr::ExprError::limit(format!(
                        "field '{field}' of {model}({id}) holds more than {cap} records, the \
                         limit of a recordset in a data expression"
                    )),
                });
            }
        };
        return Ok(ExprValue::Object(Arc::new(
            erp_expr::RecordsetProxy::new(comodel.clone(), ids).map_err(|error| {
                LoadErrorKind::Eval {
                    expr: expr(),
                    error,
                }
            })?,
        )));
    }
    let rows = record
        .read_with(tx, &[field], erp_orm::Load::Raw)
        .await
        .map_err(LoadErrorKind::Orm)?;
    let Some(row) = rows.first() else {
        return Err(missing());
    };
    let Some(value) = row.get(field) else {
        return Err(LoadErrorKind::Internal(format!(
            "the read of {model}({id}) returned no value for field '{field}'"
        )));
    };
    let bad_record = |error: erp_expr::ExprError| {
        LoadErrorKind::Internal(format!(
            "field '{field}' of {model}({id}) holds no valid record id: {error}"
        ))
    };
    Ok(match &field_def.ty {
        erp_fields::FieldType::Many2one { comodel, .. } => match value {
            Value::Int(target) => ExprValue::Object(Arc::new(
                erp_expr::RecordProxy::new(comodel.clone(), *target).map_err(bad_record)?,
            )),
            Value::False => ExprValue::Object(Arc::new(
                erp_expr::RecordsetProxy::new(comodel.clone(), Vec::new()).map_err(bad_record)?,
            )),
            other => {
                return Err(LoadErrorKind::Internal(format!(
                    "many2one field '{field}' of {model}({id}) read as {}, not an id or False",
                    expr::value_kind(other)
                )));
            }
        },
        erp_fields::FieldType::Many2many { .. } | erp_fields::FieldType::One2many { .. } => {
            return Err(LoadErrorKind::Internal(format!(
                "x2many field '{field}' of {model}({id}) reached the scalar read"
            )));
        }
        _ => expr::value_to_expr(value).map_err(|got| LoadErrorKind::NotAFieldValue {
            model: model.to_owned(),
            field: field.to_owned(),
            got: format!("{got}, which data expressions cannot read"),
        })?,
    })
}

/// Lookup rounds for one expression: each round fetches every external id, record field and
/// search the previous evaluation named, so a chain `a.b.c` of record attributes needs one round
/// per hop.
pub const MAX_EVAL_ROUNDS: usize = 32;

fn loaded_key(xid: &XmlId) -> (String, String) {
    if let Some(q) = xid.as_qualified() {
        (q.module().to_owned(), q.name().to_owned())
    } else {
        (String::new(), xid.name().to_owned())
    }
}
