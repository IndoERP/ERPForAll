//! The module system of one database: install, upgrade and uninstall in one transaction per
//! operation, plain loads, database initialization and creation.
//!
//! ```ignore
//! let system = ModuleSystem::builder(pool, index).code(&my_addon::ADDON).build()?;
//! system.initialize(InitOptions::new()).await?;           // a new database: base and auto-installs
//! let outcome = system
//!     .button_immediate_install(&superuser_env, &[ModuleName::new("sale")?])
//!     .await?;                                             // one transaction, then post-commit
//! let orm = outcome.orm;                                   // the registry of the installed set
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use deadpool_postgres::Pool;
use erp_data::{ConvertMode, DataOperation, ParseError, XmlDataReader};
use erp_orm::{
    BaseInstallGrant, ConstraintImpl, Env, GenerationAttachmentFactory, MethodImpl,
    OperationControl, OperationKey, Orm, OrmConfig, Replanned, Resume,
    SCHEMA_LOCK_TIMEOUT_DURATION, SCHEMA_STATEMENT_TIMEOUT_DURATION, SUPERUSER_ID,
    SchemaStepReport, SchemaSync, SchemaSyncOptions, SelfApplied, Signal, SignalKey, StaticPolicy,
    StepCursor, StepFingerprint, StepProgress, Transaction, model_hooks,
};
use erp_registry::{AddonDescriptor, BASE_ADDON, ModelDecl, Registry, RegistryBuilder, WEB_ADDON};
use erp_views::{PublishedViewTypes, ViewTypeContribution};
use indexmap::IndexMap;

use crate::base_models;
use crate::data_loader::ErpDataLoader;
use crate::discovery::{AddonEntry, AddonIndex, AddonSourceKind};
use crate::error::{
    Error, InconsistentDependency, LimitKind, ModuleStep, OwnedRecords, Result, SourceKind,
    StoredStateRow,
};
use crate::graph::{DropReport, LoadMode, ModuleGraph, ModuleState};
use crate::hooks::{HOOKS, HookImpl, HookKind};
use crate::lifecycle::{Catalog, InstallOptions, ModuleAction, UpdateListResult};
use crate::lifecycle_hook::{LifecycleChange, LifecycleHook};
use crate::loader::{
    Checked, DataFile, DataKind, DataLoader, LoadSession, LoadedNames, LoaderVerdict,
    SessionContext,
};
use crate::manifest::{Manifest, ModuleName};
use crate::migrations::{
    MIGRATIONS, MigrationBudget, MigrationImpl, MigrationStage, MigrationStep, NativeMigration,
};
use crate::package_runtime::{PackageChange, PackageRuntime};
use crate::post_commit::{ColumnName, IndexName, PostCommitAction, SchemaStepRef, TableName};
use crate::store;
use crate::version::{ODOO_SERIES, SemverVersion};
use crate::view_types::{ViewTypeInputs, view_type_factory};
use erp_data::load::{AddonFiles, DataNotice, FileReadError, NoticeKind};

mod installed_identity;
mod uninstall_impact;
pub(crate) fn keep_identity_tables_linked() {
    installed_identity::keep_linked();
}
pub use uninstall_impact::{
    DroppedModelImpact, ImpactFingerprint, MAX_UNINSTALL_CANONICAL_BYTES,
    MAX_UNINSTALL_IMPACT_BYTES, MAX_UNINSTALL_TARGETS, MetadataImpact,
    UNINSTALL_STATEMENT_DEADLINE_MS, UninstallCompositionBlocker, UninstallImpact,
};

/// Largest data file the runner reads (bytes), and the default of
/// [`ModuleSystemBuilder::max_data_file_bytes`]. Data files are untrusted input.
pub const MAX_DATA_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Most data and demo files one module lists for one operation, and the default of
/// [`ModuleSystemBuilder::max_data_files`].
pub const MAX_DATA_FILES: usize = 512;

/// Most bytes of data files one operation reads (all its modules together), and the default of
/// [`ModuleSystemBuilder::max_operation_data_bytes`].
pub const MAX_OPERATION_DATA_BYTES: u64 = 1024 * 1024 * 1024;

/// Most SQL migration files one operation selects (all its modules together), and most version
/// folders one module's `migrations/` directory holds; the default of
/// [`ModuleSystemBuilder::max_migration_files`].
pub const MAX_MIGRATION_FILES: usize = 4096;

/// First key of ERPForAll's advisory locks (`pg_advisory_lock(int4, int4)`): `"ERPF"` in ASCII.
///
/// The second keys taken so far, so none is reused: 1 [`OPERATION_LOCK`], 2 [`POST_COMMIT_LOCK`],
/// 3 the node registry's readiness lock (`erp_orm::node::READINESS_LOCK`: readiness transitions,
/// registrations and rollout windows of one database), 4 the authentication collection's lock
/// (`erp_security::auth::gc::GC_LOCK`, one batch at a time), 5 web asset publication
/// (`erp_addon_web::assets::store::ASSET_LOCK`), 6 node feature registration
/// (`erp_orm::node::feature::FEATURE_REGISTRY_LOCK`), 7 the attachment garbage collection's
/// hold/sweep exclusion (`erp_addon_base::attachments::gc::GC_HOLD_LOCK`).
pub const LOCK_CLASS: i32 = 0x4552_5046;

pub const OPERATION_LOCK: (i32, i32) = (LOCK_CLASS, 1);

/// The advisory lock of the post-commit phase: a session lock held while the steps run, so two
/// operations finishing together never run the same step twice.
pub const POST_COMMIT_LOCK: (i32, i32) = (LOCK_CLASS, 2);

/// Longest key of a post-commit step (bytes).
pub const MAX_POST_COMMIT_KEY_BYTES: usize = 64;

/// How many times a step may fail before it is parked: a failed step runs again in later phases
/// until its `attempts` reach this number, then it stays `failed` and no phase runs it until an
/// operation schedules it again ([`PendingState::Parked`]). A pending step interrupted
/// mid-phase is not counted as failed and runs again whatever its attempts.
pub const MAX_POST_COMMIT_ATTEMPTS: i32 = 3;

/// Most passes over the queue one post-commit phase makes. A pass runs the steps it finds that
/// the phase has not run; the queue is read again after each pass, so steps scheduled meanwhile
/// run too. A queue that keeps growing (a step that schedules another, or operations scheduling
/// steps as fast as they run) would keep the phase, and the connection and lock it holds, busy
/// for ever: the phase stops with [`PostCommitStop::PassLimit`] and leaves the rest pending.
pub const MAX_POST_COMMIT_PASSES: usize = 8;

/// Longest wait for a database lock inside a module operation and in the post-commit phase, and
/// the default of [`ModuleSystemBuilder::lock_timeout`] (`lock_timeout`, design note m2-s1).
pub const MAX_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest statement of a module operation, and the default of
/// [`ModuleSystemBuilder::statement_timeout`].
pub const MAX_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Longest post-commit step (index builds on large tables run there), and the default of
/// [`ModuleSystemBuilder::post_commit_statement_timeout`].
pub const MAX_POST_COMMIT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// Longest wait of a plain load for another operation before its forced update, and the default
/// of [`ModuleSystemBuilder::operation_wait_timeout`].
pub const MAX_OPERATION_WAIT: Duration = Duration::from_secs(30 * 60);

/// Most rows an error lists ([`Error::InvalidStoredStates`]); the rest are left out.
pub const MAX_REPORTED_ROWS: i64 = 20;

/// The client action returned by an immediate operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClientAction {
    /// `{'type': 'ir.actions.act_url', 'target': 'self', 'url': '/erp'}`.
    ActUrl {
        /// The URL to open.
        url: &'static str,
        /// `self`: replace the current page.
        target: &'static str,
    },
}

impl ClientAction {
    /// The native home action returned when no configuration step is open.
    pub fn reload_home() -> Self {
        Self::ActUrl {
            url: "/erp",
            target: "self",
        }
    }
}

/// The key of a post-commit step within its module: 1 to [`MAX_POST_COMMIT_KEY_BYTES`] bytes of
/// `a-z`, `0-9`, `_`, `.`, `-`.
///
/// ```
/// use erp_modules::system::PostCommitKey;
/// assert_eq!(PostCommitKey::parse("refresh.stats-1").unwrap().as_str(), "refresh.stats-1");
/// assert!(PostCommitKey::parse("Upper").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PostCommitKey(String);

impl PostCommitKey {
    /// Check and wrap a key.
    ///
    /// # Errors
    /// `Programming` (naming the key and the rule) for an empty, longer or otherwise invalid key.
    pub fn parse(key: &str) -> erp_fields::Result<Self> {
        let ok = !key.is_empty()
            && key.len() <= MAX_POST_COMMIT_KEY_BYTES
            && key
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'));
        if ok {
            Ok(Self(key.to_owned()))
        } else {
            Err(erp_fields::Error::programming(format!(
                "invalid post-commit key {key:?}: 1 to {MAX_POST_COMMIT_KEY_BYTES} bytes of a-z, \
                 0-9, '_', '.', '-'"
            )))
        }
    }

    /// The key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for PostCommitKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PostCommitKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One post-commit step.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PostCommitStep {
    /// The module that scheduled the step.
    pub module: ModuleName,
    /// The step's key within the module.
    pub key: PostCommitKey,
}

impl PostCommitStep {
    /// The step `key` of `module`.
    pub fn new(module: ModuleName, key: PostCommitKey) -> Self {
        Self { module, key }
    }
}

/// A post-commit step that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PostCommitFailure {
    /// The step.
    pub step: PostCommitStep,
    /// The database's message, or why a finished index step was not accepted.
    pub error: String,
    /// The SQLSTATE, when the database gave one.
    pub sqlstate: Option<String>,
    /// True when this failure was the step's [`MAX_POST_COMMIT_ATTEMPTS`]th: the step is parked
    /// and no later phase runs it until it is scheduled again.
    pub parked: bool,
}

/// Why the post-commit phase stopped before running every pending step.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PostCommitStop {
    /// Another post-commit phase holds [`POST_COMMIT_LOCK`]: it runs the pending steps, also
    /// those committed after it started (a phase reads the queue again after each pass, and once
    /// more after it releases the lock, until it finds no step it has not run). When a phase ran
    /// steps and then finds the lock taken by another, it stops with `Busy` too: the other runs
    /// the rest.
    ///
    /// A holder that is not a running phase exists: a phase whose future was dropped while a
    /// step ran leaves that statement running on the server (the server notices the closed
    /// connection only when the statement ends), and the lock stays with it until then.
    /// `holder_pid` is the server process that holds the lock, to look it up in
    /// `pg_stat_activity` (`None` when the holder ended meanwhile).
    Busy {
        /// The backend process id of the lock's holder.
        holder_pid: Option<i32>,
    },
    /// No connection, an unreadable step row, or a status that could not be recorded. Steps
    /// reported as done or failed before it are accurate; the others stay pending.
    Failed {
        /// What failed.
        message: String,
        /// The SQLSTATE, when the database gave one.
        sqlstate: Option<String>,
    },
    /// The phase made [`MAX_POST_COMMIT_PASSES`] passes over the queue and steps it has not run
    /// are still there (the queue keeps growing while it runs). They stay pending; run the phase
    /// again.
    PassLimit {
        /// The passes made.
        passes: usize,
    },
}

/// What the post-commit phase did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
#[must_use = "post-commit failures are only reported here: check `failed` and `stopped`"]
pub struct PostCommitReport {
    /// The steps that ran, in order.
    pub done: Vec<PostCommitStep>,
    /// Steps that failed; they stay `failed` and run again in the next phase, until they have
    /// failed [`MAX_POST_COMMIT_ATTEMPTS`] times (then [`PostCommitFailure::parked`] is set).
    pub failed: Vec<PostCommitFailure>,
    /// Steps scheduled again (with the same statement or another) or removed while they ran:
    /// that run's result was not recorded. A rescheduled step runs again in the same phase (and is
    /// listed again, in `done` or `failed`).
    pub superseded: Vec<PostCommitStep>,
    /// Steps whose queue row another transaction held locked (usually an operation scheduling or
    /// removing the step) when the phase wanted to start or record them: skipped, the row is
    /// left as it is. A step that ran and could not be recorded for this reason also ran.
    pub skipped_row_locked: Vec<PostCommitStep>,
    /// Rows of the queue that did not pass the checks of scheduling (edited in the table): not
    /// run, marked failed with the reason.
    pub invalid: Vec<InvalidPostCommitRow>,
    /// Set when the phase stopped early; the steps not listed above stay pending.
    pub stopped: Option<PostCommitStop>,
    /// What each schema step in `done` reported (column comments a backfill kept). A step the
    /// ORM found nothing left to do for is listed with an empty report.
    pub schema: Vec<SchemaStepDone>,
}

/// A schema step of a large table that ran, with what it reported
/// ([`PostCommitReport::schema`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SchemaStepDone {
    /// The queue step.
    pub step: PostCommitStep,
    /// What the ORM step left as it found it ([`SchemaStepReport::kept_comments`]).
    pub report: SchemaStepReport,
    /// Whether the step ran or found nothing to do.
    pub outcome: SchemaStepOutcome,
}

/// What a done schema step did ([`SchemaStepDone::outcome`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaStepOutcome {
    /// The ORM planned the step again and ran it.
    Ran,
    /// The ORM planned the step again and the catalog showed nothing left of its work (an
    /// inline synchronization, or an earlier run, did it). Logged at `warn` level with the
    /// module, key and table: it is also what a row on a table the work no longer needs looks
    /// like.
    NothingLeft,
}

/// What a schema row's run gives back.
struct SchemaResult {
    report: SchemaStepReport,
    outcome: SchemaStepOutcome,
}

impl PostCommitReport {
    /// True when every pending step ran and none failed.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty() && self.invalid.is_empty() && self.stopped.is_none()
    }
}

/// A row of `ir_module_post_commit` the module system could not have written (a module name or
/// key outside the rules, a statement the scheduling checks refuse, an unknown state): no phase
/// can run it. Listed ([`PendingState::Invalid`], [`PostCommitReport::invalid`]) so a load
/// reports it instead of refusing to start; correct or delete the row.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InvalidPostCommitRow {
    /// The row's id.
    pub id: i64,
    /// The row's module, as stored.
    pub module: String,
    /// The row's key, as stored.
    pub key: String,
    /// What is wrong with it.
    pub reason: String,
}

/// The result of a committed operation.
///
/// The operation committed even when the post-commit phase failed: check
/// [`OperationOutcome::post_commit`] (`...await?` alone does not). Switch to
/// [`OperationOutcome::orm`] and drop the ORM the call was made with: that one still has the
/// registry of the module set before the operation (models of installed modules missing,
/// models of removed ones still present).
#[non_exhaustive]
#[must_use = "use `outcome.orm` (the old ORM has the old registry) and check `outcome.post_commit`"]
pub struct OperationOutcome {
    /// Immutable source entries used by the final registry, in dependency/load order.
    admitted_addons: Vec<AddonEntry>,
    /// The ORM of the installed set after the operation; replaces the caller's ORM.
    pub orm: Orm,
    /// What the client should do next.
    pub action: ClientAction,
    /// Modules installed, in install order.
    pub installed: Vec<ModuleName>,
    /// Modules upgraded, in update order.
    pub upgraded: Vec<ModuleName>,
    /// Modules uninstalled.
    pub uninstalled: Vec<ModuleName>,
    /// Installed modules present in no addon source.
    pub unavailable: Vec<ModuleName>,
    /// Modules left out of the update (not scheduled, so not an error), with the reasons.
    pub dropped: DropReport,
    /// The post-commit phase.
    pub post_commit: PostCommitReport,
    pub data_notices: Vec<DataNotice>,
    /// Generated company restrictions rewritten because the operation asked for it
    /// ([`ModuleSystem::button_immediate_upgrade_restoring`]), one notice per row.
    pub restored_restrictions: Vec<crate::company_access::GeneratedRestrictionRestored>,
}

impl OperationOutcome {
    /// Final addon entries captured and admitted by the committed operation, in load order.
    /// Host services consume their [`AddonEntry::admitted_files`] instead of rediscovering or
    /// reopening live directories after lifecycle admission. Package client maps remain in the
    /// corresponding entry. Source admission does not confer request authorization.
    pub fn admitted_addons(&self) -> &[AddonEntry] {
        &self.admitted_addons
    }
}

impl std::fmt::Debug for OperationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationOutcome")
            .field("action", &self.action)
            .field("installed", &self.installed)
            .field("upgraded", &self.upgraded)
            .field("uninstalled", &self.uninstalled)
            .field("unavailable", &self.unavailable)
            .field("dropped", &self.dropped)
            .field("post_commit", &self.post_commit)
            .field("data_notices", &self.data_notices)
            .field("restored_restrictions", &self.restored_restrictions)
            .finish_non_exhaustive()
    }
}

/// What the forced update of a plain load did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ForcedUpdate {
    /// Modules installed, in install order.
    pub installed: Vec<ModuleName>,
    /// Modules upgraded, in update order.
    pub upgraded: Vec<ModuleName>,
    /// Modules uninstalled.
    pub uninstalled: Vec<ModuleName>,
    /// What the forced update's data loading reported ([`OperationOutcome::data_notices`]).
    pub data_notices: Vec<DataNotice>,
}

/// The state of a post-commit step that has not run successfully yet; each state carries what
/// belongs to it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PendingState {
    /// Scheduled, not run yet (or its phase was interrupted).
    Pending {
        /// The step.
        step: PostCommitStep,
    },
    /// Ran and failed; it runs again in the next phase.
    Failed {
        /// The step.
        step: PostCommitStep,
        /// The database's message, or why a finished index step was not accepted.
        error: String,
        /// The SQLSTATE, when the database gave one.
        sqlstate: Option<String>,
    },
    /// Failed [`MAX_POST_COMMIT_ATTEMPTS`] times: stays `failed`, and no phase runs it until an
    /// operation schedules it again.
    Parked {
        /// The step.
        step: PostCommitStep,
        /// The last failure's message.
        error: String,
        /// The last failure's SQLSTATE, when the database gave one.
        sqlstate: Option<String>,
    },
    /// The stored row could not have been written by the module system (see
    /// [`InvalidPostCommitRow`]).
    Invalid(InvalidPostCommitRow),
}

/// A post-commit step waiting for a phase ([`ModuleSystem::pending_post_commit`],
/// [`LoadOutcome::post_commit_pending`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PendingPostCommit {
    /// The row's id in `ir_module_post_commit`.
    pub id: i64,
    /// Pending, failed, parked or invalid, with the step or the raw row.
    pub state: PendingState,
    /// How many times a phase started this step since it was last scheduled (a step that failed
    /// and runs every phase shows it).
    pub attempts: i64,
}

impl PendingPostCommit {
    /// The step, unless the row is invalid.
    pub fn step(&self) -> Option<&PostCommitStep> {
        match &self.state {
            PendingState::Pending { step }
            | PendingState::Failed { step, .. }
            | PendingState::Parked { step, .. } => Some(step),
            PendingState::Invalid(_) => None,
        }
    }
}

/// The result of a plain load.
///
/// A load never runs post-commit steps: they may build indexes for hours. The steps waiting are
/// listed in [`LoadOutcome::post_commit_pending`]; run them with
/// [`ModuleSystem::run_post_commit`] (typically in a background task once the server serves).
#[non_exhaustive]
#[must_use = "use `outcome.orm` and check `outcome.post_commit_pending`"]
pub struct LoadOutcome {
    /// Immutable source entries consumed by the final registry, in dependency/load order.
    admitted_addons: Vec<AddonEntry>,
    /// The ORM of the installed set (the forced update's ORM when one ran: the registry is built
    /// once).
    pub orm: Orm,
    /// Installed modules present in no addon source: the registry loads without
    /// them and the health check reports them.
    pub unavailable: Vec<ModuleName>,
    /// Modules left out of the graph and why.
    pub drops: DropReport,
    /// Set when pending operations forced an update run.
    pub forced_update: Option<ForcedUpdate>,
    /// Post-commit steps waiting after the load, in scheduling order: left by an interrupted or
    /// skipped phase, failed earlier, or scheduled by the forced update. The load did not run
    /// them.
    pub post_commit_pending: Vec<PendingPostCommit>,
}

impl LoadOutcome {
    /// Source entries captured for this load's final registry, in load order. A forced update
    /// retains its committed operation's entries. Hosts use these immutable bytes for startup
    /// UI admission, without rediscovery or a live filesystem reread. This getter preserves
    /// ordinary load's existing durable-receipt policy; it is not identity enrollment.
    pub fn admitted_addons(&self) -> &[AddonEntry] {
        &self.admitted_addons
    }
}

impl std::fmt::Debug for LoadOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadOutcome")
            .field("unavailable", &self.unavailable)
            .field("drops", &self.drops)
            .field("forced_update", &self.forced_update)
            .field("post_commit_pending", &self.post_commit_pending)
            .finish_non_exhaustive()
    }
}

/// Options of [`ModuleSystem::initialize`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct InitOptions {
    demo: bool,
    skip_auto_install: bool,
}

impl InitOptions {
    /// No demo data, auto-install on.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load demo data (`--with-demo`).
    pub fn with_demo(mut self, demo: bool) -> Self {
        self.demo = demo;
        self
    }

    /// Install only `base` (`--skip-auto-install`).
    pub fn with_skip_auto_install(mut self, skip: bool) -> Self {
        self.skip_auto_install = skip;
        self
    }
}

/// Explicit owner-selected source of a forward migration's installed preimage.
enum PreviousIdentity {
    Release(BTreeMap<ModuleName, String>),
    EnrolledLegacy,
}

/// Builds a [`ModuleSystem`].
#[must_use = "call .build()"]
pub struct ModuleSystemBuilder {
    pool: Pool,
    required_core_tables: Vec<&'static erp_orm::CoreTableRequirement>,
    index: AddonIndex,
    code: Vec<&'static AddonDescriptor>,
    native_artifact_identities: BTreeMap<ModuleName, String>,
    identity_previous: Option<PreviousIdentity>,
    decls: Vec<&'static ModelDecl>,
    impls: Vec<&'static MethodImpl>,
    constraints: Vec<&'static ConstraintImpl>,
    hooks: Vec<&'static HookImpl>,
    linked_hooks: bool,
    migrations: Vec<&'static MigrationImpl>,
    linked_migrations: bool,
    loader: Option<Arc<dyn DataLoader>>,
    schema_only_loader: bool,
    config: OrmConfig,
    signal: Option<Signal>,
    max_data_file_bytes: u64,
    max_data_files: usize,
    max_operation_data_bytes: u64,
    max_migration_files: usize,
    timeouts: Timeouts,
    schema_sync: SchemaSyncOptions,
    package_runtime: Option<Arc<dyn PackageRuntime>>,
    lifecycle_hooks: Vec<Arc<dyn LifecycleHook>>,
    view_types: Vec<ViewTypeContribution>,
}

/// The database timeouts of module operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timeouts {
    lock: Duration,
    statement: Duration,
    post_commit_statement: Duration,
    operation_wait: Duration,
}

impl Timeouts {
    const DEFAULT: Self = Self {
        lock: MAX_LOCK_TIMEOUT,
        statement: MAX_STATEMENT_TIMEOUT,
        post_commit_statement: MAX_POST_COMMIT_STATEMENT_TIMEOUT,
        operation_wait: MAX_OPERATION_WAIT,
    };

    /// Each between 1 ms and its maximum (limits can only be lowered; zero would mean "no limit"
    /// to PostgreSQL).
    fn check(&self) -> Result<()> {
        for (name, value, max) in [
            (LimitKind::LockTimeout, self.lock, MAX_LOCK_TIMEOUT),
            (
                LimitKind::StatementTimeout,
                self.statement,
                MAX_STATEMENT_TIMEOUT,
            ),
            (
                LimitKind::PostCommitStatementTimeout,
                self.post_commit_statement,
                MAX_POST_COMMIT_STATEMENT_TIMEOUT,
            ),
            (
                LimitKind::OperationWaitTimeout,
                self.operation_wait,
                MAX_OPERATION_WAIT,
            ),
        ] {
            if millis(value) == 0 || value > max {
                return Err(Error::InvalidTimeout { name, value, max });
            }
        }
        Ok(())
    }
}

/// Whole milliseconds, saturating (every limit is far below `u64::MAX` ms).
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The limit in force when a caller's session already has `current_ms` (0: none) and the module
/// system wants `configured`: limits are only lowered, never raised, and "none" is replaced.
fn lowered(current_ms: u64, configured: Duration) -> Duration {
    if current_ms == 0 {
        configured
    } else {
        configured.min(Duration::from_millis(current_ms))
    }
}

/// Set the lock and statement timeouts (milliseconds, 0 for none) of the rest of `tx`
/// (`SET LOCAL`).
async fn set_timeouts(
    tx: &mut Transaction,
    lock_ms: u64,
    statement_ms: u64,
) -> erp_fields::Result<()> {
    let lock = format!("{lock_ms}ms");
    let statement = format!("{statement_ms}ms");
    tx.query(
        "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
        &[&lock, &statement],
    )
    .await?;
    Ok(())
}

/// What the session had when the operation's transaction began.
struct CallerSession {
    lock_ms: u64,
    statement_ms: u64,
    /// The identity of the transaction.
    guard: Guard,
}

/// What identifies an operation's transaction: `transaction_timestamp()` as an ISO-8601 UTC
/// string (compared as `timestamptz`, so `TimeZone` and `DateStyle` of the session, which a hook
/// may `SET`, do not matter), and whether it holds [`OPERATION_LOCK`]. A different timestamp
/// later means another transaction runs on the session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Guard {
    began: String,
    holds_lock: bool,
}

/// The session's `lock_timeout` and `statement_timeout` (the pool's connection settings, a
/// role's or database's defaults) and the transaction's start time. Read as `reset_val`: the ORM
/// sets its own limits with `SET LOCAL` in the same message as `BEGIN`, so the current
/// value is the ORM's, and what the operation must lower to is what the session had before.
async fn read_caller_session(tx: &mut Transaction) -> Result<CallerSession> {
    let rows = tx
        .query(
            "SELECT (SELECT reset_val::int8 FROM pg_catalog.pg_settings WHERE name = 'lock_timeout'), \
                    (SELECT reset_val::int8 FROM pg_catalog.pg_settings WHERE name = 'statement_timeout'), \
                    to_char(transaction_timestamp() AT TIME ZONE 'UTC', \
                            'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
            &[],
        )
        .await?;
    let row = rows.first().ok_or_else(|| Error::InvariantViolation {
        message: "reading the session timeouts returned no row".to_owned(),
    })?;
    let shape = |e: tokio_postgres::Error| Error::InvariantViolation {
        message: format!("unexpected row of the session timeouts: {e}"),
    };
    let ms = |value: i64| {
        u64::try_from(value).map_err(|_| Error::InvariantViolation {
            message: format!("the session reports a negative timeout ({value} ms)"),
        })
    };
    Ok(CallerSession {
        lock_ms: ms(row.try_get(0).map_err(shape)?)?,
        statement_ms: ms(row.try_get(1).map_err(shape)?)?,
        guard: Guard {
            began: row.try_get(2).map_err(shape)?,
            holds_lock: false,
        },
    })
}

impl ModuleSystemBuilder {
    /// Require linked core DDL registrations, including the authentication tables.
    pub fn require_core_tables(
        mut self,
        tables: impl IntoIterator<Item = &'static erp_orm::CoreTableRequirement>,
    ) -> Self {
        self.required_core_tables.extend(tables);
        self
    }

    /// The runtime of package addons: it gives
    /// the registry the models of an addon read from a `.erpaddon` package, the ORM its
    /// WebAssembly method implementations, and sees every operation before it commits. Without
    /// one a package addon is data-only, as before.
    pub fn package_runtime(mut self, runtime: Arc<dyn PackageRuntime>) -> Self {
        self.package_runtime = Some(runtime);
        self
    }

    /// Append a trusted transactional admission service.
    ///
    /// Services run after [`PackageRuntime::before_commit`], in registration order, for
    /// initialization and every update operation. They see the final effective addon sources
    /// and share the operation's transaction. An error from either package admission or any
    /// service rolls the whole operation back; package/WASM admission is never replaced.
    /// Plain startup loads use separate compatibility admission and do not call these services.
    pub fn lifecycle_hook(mut self, hook: Arc<dyn LifecycleHook>) -> Self {
        self.lifecycle_hooks.push(hook);
        self
    }

    /// Native H18 view-type contributions, each owned by the actual descriptor of its
    /// native addon. Every ORM this module system builds publishes the table of the contributions
    /// whose owner is installed in that generation; a package addon as owner is refused as
    /// unsupported until M2b.
    pub fn view_types(
        mut self,
        contributions: impl IntoIterator<Item = ViewTypeContribution>,
    ) -> Self {
        self.view_types.extend(contributions);
        self
    }

    /// Native addon descriptors (`erp_sdk::addon!`): addons without one are data-only.
    pub fn code(mut self, addons: impl IntoIterator<Item = &'static AddonDescriptor>) -> Self {
        self.code.extend(addons);
        self
    }

    /// Per-addon identities generated from the linked native addon sources at build time.
    /// This trusted composition input must never come from browser request data.
    pub fn native_artifact_identities(mut self, identities: BTreeMap<ModuleName, String>) -> Self {
        self.native_artifact_identities = identities;
        self
    }

    /// Verified previous release receipts for an explicit operator-run forward migration.
    /// Existing receipts are checked against these bytes while candidate files are consumed;
    /// migration never re-stamps receipts. Never set this on a serving/request module system.
    pub fn identity_previous(mut self, identities: BTreeMap<ModuleName, String>) -> Self {
        self.identity_previous = Some(PreviousIdentity::Release(identities));
        self
    }

    /// Use the complete enrolled installed receipt set for a legacy previous release.
    /// Only an explicit owner CLI that verified a pre-identity release may select this mode.
    /// Receipts are resolved under lifecycle ownership, never from candidate files.
    pub fn identity_previous_enrolled(mut self) -> Self {
        self.identity_previous = Some(PreviousIdentity::EnrolledLegacy);
        self
    }

    /// Model declarations built at runtime (tests, per-database overlays); a registry gets those
    /// whose addon is in its set.
    pub fn decls(mut self, decls: impl IntoIterator<Item = &'static ModelDecl>) -> Self {
        self.decls.extend(decls);
        self
    }

    /// Method implementations built at runtime (tests); a registry uses those of its addons.
    pub fn impls(mut self, impls: impl IntoIterator<Item = &'static MethodImpl>) -> Self {
        self.impls.extend(impls);
        self
    }

    /// Constraint declarations built at runtime (tests); a registry uses those of its addons.
    pub fn constraints(
        mut self,
        constraints: impl IntoIterator<Item = &'static ConstraintImpl>,
    ) -> Self {
        self.constraints.extend(constraints);
        self
    }

    /// Hook implementations given explicitly (tests, link roots).
    pub fn hooks(mut self, hooks: impl IntoIterator<Item = &'static HookImpl>) -> Self {
        self.hooks.extend(hooks);
        self
    }

    /// Whether the hooks linked into [`HOOKS`] are used (default true).
    pub fn linked_hooks(mut self, on: bool) -> Self {
        self.linked_hooks = on;
        self
    }

    /// Native migration implementations given explicitly (tests, link roots).
    pub fn migrations(
        mut self,
        migrations: impl IntoIterator<Item = &'static MigrationImpl>,
    ) -> Self {
        self.migrations.extend(migrations);
        self
    }

    /// Whether the migrations linked into [`MIGRATIONS`] are used (default true).
    pub fn linked_migrations(mut self, on: bool) -> Self {
        self.linked_migrations = on;
        self
    }

    /// Replace the data loader. The default is [`ErpDataLoader`]; tests that only
    /// record what the module system hands over give a [`crate::FileLoader`] or their own
    /// [`DataLoader`].
    pub fn data_loader(mut self, loader: Arc<dyn DataLoader>) -> Self {
        self.loader = Some(loader);
        self.schema_only_loader = false;
        self
    }

    /// Run without a data loader: a module with data or demo files then cannot be installed or
    /// upgraded ([`Error::DataLoaderMissing`]), and an uninstall that finds records the modules
    /// own in models that stay is refused ([`Error::UninstallOwnedRecords`]).
    pub fn no_data_loader(mut self) -> Self {
        self.loader = None;
        self.schema_only_loader = true;
        self
    }

    /// ORM tunables of every registry the system builds.
    pub fn config(mut self, config: OrmConfig) -> Self {
        self.config = config;
        self
    }

    /// Cross-process change signals: every operation that installs,
    /// upgrades or uninstalls a module bumps the registry generation in its own transaction, so
    /// the other workers of the database rebuild their registry (the generation is not bumped
    /// when the operation fails and rolls back, or when it changes no module). The signal table
    /// is created by [`ModuleSystem::initialize`]; a database initialized without a signal needs
    /// `Transaction::install_signal` first.
    pub fn signal(mut self, signal: Signal) -> Self {
        self.signal = Some(signal);
        self
    }

    /// Largest data file read (bytes): between 1 and [`MAX_DATA_FILE_BYTES`] (the default);
    /// [`ModuleSystemBuilder::build`] refuses other values.
    pub fn max_data_file_bytes(mut self, limit: u64) -> Self {
        self.max_data_file_bytes = limit;
        self
    }

    /// Most data and demo files one module may list: between 1 and [`MAX_DATA_FILES`] (the
    /// default); a module that lists more fails its operation. [`ModuleSystemBuilder::build`]
    /// refuses other values.
    pub fn max_data_files(mut self, limit: usize) -> Self {
        self.max_data_files = limit;
        self
    }

    /// Most bytes of data files one operation reads, all its modules together: between 1 and
    /// [`MAX_OPERATION_DATA_BYTES`] (the default). [`ModuleSystemBuilder::build`] refuses other
    /// values.
    pub fn max_operation_data_bytes(mut self, limit: u64) -> Self {
        self.max_operation_data_bytes = limit;
        self
    }

    /// Most SQL migration files one operation selects, all its modules together, and most
    /// version folders one module's `migrations/` directory may hold: between 1 and
    /// [`MAX_MIGRATION_FILES`] (the default). A file past it fails the operation before it is
    /// read. The selected files' bytes count against
    /// [`ModuleSystemBuilder::max_operation_data_bytes`] with the data files.
    /// [`ModuleSystemBuilder::build`] refuses other values.
    pub fn max_migration_files(mut self, limit: usize) -> Self {
        self.max_migration_files = limit;
        self
    }

    /// `lock_timeout` of the operation transactions and of the post-commit phase: a statement
    /// waiting longer for a lock fails the operation with [`Error::LockNotAvailable`] (a post-commit
    /// step is marked `failed` and retried). Between 1 ms and [`MAX_LOCK_TIMEOUT`] (the default).
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.lock = timeout;
        self
    }

    /// `statement_timeout` of the operation transactions ([`Error::StatementCanceled`]). Between
    /// 1 ms and [`MAX_STATEMENT_TIMEOUT`] (the default).
    pub fn statement_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.statement = timeout;
        self
    }

    /// `statement_timeout` of each post-commit step. Between 1 ms and
    /// [`MAX_POST_COMMIT_STATEMENT_TIMEOUT`] (the default).
    pub fn post_commit_statement_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.post_commit_statement = timeout;
        self
    }

    /// How the schema updates of module operations treat large tables
    /// ([`SchemaSyncOptions`], default [`SchemaSyncOptions::new`]): the table size above which
    /// index builds, validations and fills leave the operation for post-commit schema steps, the
    /// batch sizes and the lock timeouts of those steps. The same options plan the steps and run
    /// them. A test sets a tiny `large_table_rows` to exercise the steps.
    pub fn schema_sync_options(mut self, options: SchemaSyncOptions) -> Self {
        self.schema_sync = options;
        self
    }

    /// How long [`ModuleSystem::load`] waits for another operation before its forced update
    /// ([`Error::OperationWaitTimeout`]). Between 1 ms and [`MAX_OPERATION_WAIT`] (the default).
    pub fn operation_wait_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.operation_wait = timeout;
        self
    }

    /// Check the addon set (sources, hooks, code descriptors) and build the system.
    ///
    /// # Errors
    /// [`Error::InvalidLimit`]; discovery errors; a hook named in a manifest without exactly one
    /// implementation, or an implementation no manifest names; a native descriptor whose
    /// dependencies differ from its manifest; the base registry failing to compose.
    pub fn build(mut self) -> Result<ModuleSystem> {
        if self.max_data_file_bytes == 0 || self.max_data_file_bytes > MAX_DATA_FILE_BYTES {
            return Err(Error::InvalidLimit {
                name: LimitKind::MaxDataFileBytes,
                value: self.max_data_file_bytes,
                max: MAX_DATA_FILE_BYTES,
            });
        }
        if self.max_data_files == 0 || self.max_data_files > MAX_DATA_FILES {
            return Err(Error::InvalidLimit {
                name: LimitKind::MaxDataFiles,
                value: u64::try_from(self.max_data_files).unwrap_or(u64::MAX),
                max: MAX_DATA_FILES as u64,
            });
        }
        if self.max_operation_data_bytes == 0
            || self.max_operation_data_bytes > MAX_OPERATION_DATA_BYTES
        {
            return Err(Error::InvalidLimit {
                name: LimitKind::MaxOperationDataBytes,
                value: self.max_operation_data_bytes,
                max: MAX_OPERATION_DATA_BYTES,
            });
        }
        if self.max_migration_files == 0 || self.max_migration_files > MAX_MIGRATION_FILES {
            return Err(Error::InvalidLimit {
                name: LimitKind::MaxMigrationFiles,
                value: u64::try_from(self.max_migration_files).unwrap_or(u64::MAX),
                max: MAX_MIGRATION_FILES as u64,
            });
        }
        self.timeouts.check()?;
        // claim the process's operation key now, so a taken key fails the build, not an operation
        operation_key()?;
        let entries = self.index.discover_entries()?;
        let mut code: HashMap<&'static str, &'static AddonDescriptor> = HashMap::new();
        for addon in self.code {
            if code.insert(addon.name, addon).is_some() {
                return Err(Error::AddonCodeMismatch {
                    module: ModuleName::new(addon.name)?,
                    message: "two code descriptors for this addon".to_owned(),
                });
            }
        }
        let mut hooks: Vec<&'static HookImpl> = self.hooks;
        if self.linked_hooks {
            hooks.extend(HOOKS.iter());
        }
        let mut migrations: Vec<&'static MigrationImpl> = self.migrations;
        if self.linked_migrations {
            migrations.extend(MIGRATIONS.iter());
        }
        let migrations = crate::migrations::validate_native(migrations)?;
        let parts = Arc::new(Parts {
            pool: self.pool.clone(),
            index: Mutex::new(self.index),
            code,
            native_artifact_identities: self.native_artifact_identities,
            identity_previous: self.identity_previous,
            data_only: Mutex::new(HashMap::new()),
            decls: self.decls,
            hooks,
            migrations,
            loader: self.loader,
            schema_only_loader: self.schema_only_loader,
            max_data_file_bytes: self.max_data_file_bytes,
            max_data_files: self.max_data_files,
            max_operation_data_bytes: self.max_operation_data_bytes,
            max_migration_files: self.max_migration_files,
            timeouts: self.timeouts,
            schema_sync: self.schema_sync,
            package_runtime: self.package_runtime,
            lifecycle_hooks: self.lifecycle_hooks,
            view_types: self.view_types,
        });
        parts.check_code(&entries)?;
        let registry = parts.compose(&[ModuleName::new("base")?], &entries)?;
        // Every runtime implementation is given once, here: `Orm::rebuild` carries them to each
        // stage registry, which uses those of its addons. The module system is the
        // module-install executor: it builds its root ORM with the lineage's base-install grant
        // (the only minting site in production) and keeps it for the load session of `base`.
        // The H18 view-type table is composed here and at every stage ORM (`orm_for`), before
        // the ORM can escape or any DDL/data runs; a rebuild inherits none of it.
        let view_types =
            parts.view_type_factory(&registry, &[ModuleName::new("base")?], &entries)?;
        let mut root = Orm::builder(registry, self.pool).generation_attachment_with(view_types);
        if let Some(signal) = self.signal {
            root = root.signal(signal);
        }
        let (root, base_install_grant) = root
            .require_core_tables(self.required_core_tables)
            .require_core_tables(crate::install::core_tables())
            // the job queue's core kinds and the capability that administers jobs live in
            // erp-security (the ORM cannot register them): require them here
            .require_job_kinds(erp_security::jobs::core_job_kinds().iter())
            .job_admin_capability(&erp_security::jobs::JOBS_ADMIN)
            // the capability behind with_user to another user (A4), also erp-security's
            .impersonate_capability(&erp_security::principal::PRINCIPAL_IMPERSONATE)
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .policy(StaticPolicy::SuperuserOnly)
            .config(self.config)
            .impls(base_models::IMPLS.iter().copied().chain(self.impls))
            .constraints(
                base_models::CONSTRAINTS
                    .iter()
                    .copied()
                    .chain(self.constraints),
            )
            .build_with_base_install_grant()?;
        Ok(ModuleSystem {
            parts,
            root,
            base_install_grant,
        })
    }
}

/// The module system of one database.
pub struct ModuleSystem {
    parts: Arc<Parts>,
    /// The ORM every operation starts from (registry: `base` and the module metadata). Its
    /// database identity is shared by every stage ORM; it carries the runtime
    /// implementations to them. Its caches are cleared whenever an operation transaction that
    /// started on it commits (`Transaction::switch_orm`).
    root: Orm,
    /// The base-install grant of `root`'s lineage (valid on every stage ORM, which shares the
    /// lineage): the load session gets a borrow of it to mark the transaction while `base`'s data
    /// files load (`Transaction::begin_base_install`). Owned here only, never cloned or handed
    /// out beyond a session's borrow.
    base_install_grant: BaseInstallGrant,
}

type DataOnlyDescriptorKey = (String, Vec<String>, bool);

/// Everything of a [`ModuleSystem`] except its root ORM (needed to build the root ORM).
struct Parts {
    pool: Pool,
    index: Mutex<AddonIndex>,
    code: HashMap<&'static str, &'static AddonDescriptor>,
    native_artifact_identities: BTreeMap<ModuleName, String>,
    identity_previous: Option<PreviousIdentity>,
    /// Descriptors made for data-only addons, one per (name, dependencies, policy), kept for the
    /// system's life (registries hold `&'static` descriptors).
    data_only: Mutex<HashMap<DataOnlyDescriptorKey, &'static AddonDescriptor>>,
    decls: Vec<&'static ModelDecl>,
    hooks: Vec<&'static HookImpl>,
    /// Checked at build ([`crate::migrations::validate_native`]).
    migrations: Vec<NativeMigration>,
    loader: Option<Arc<dyn DataLoader>>,
    schema_only_loader: bool,
    max_data_file_bytes: u64,
    max_data_files: usize,
    max_operation_data_bytes: u64,
    max_migration_files: usize,
    timeouts: Timeouts,
    schema_sync: SchemaSyncOptions,
    /// What turns a package addon into models and method implementations.
    package_runtime: Option<Arc<dyn PackageRuntime>>,
    lifecycle_hooks: Vec<Arc<dyn LifecycleHook>>,
    /// Native H18 view-type contributions published with every generation.
    view_types: Vec<ViewTypeContribution>,
}

/// The operation of a package addon's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageOp {
    Install,
    Upgrade,
    Uninstall,
}

impl PackageOp {
    fn method_name(self) -> &'static str {
        match self {
            Self::Install => "WasmLifecycle::install",
            Self::Upgrade => "WasmLifecycle::upgrade",
            Self::Uninstall => "WasmLifecycle::uninstall",
        }
    }
}

/// How the caller of a package lifecycle operation proves its authority.
pub(crate) enum PackageAccess<'a> {
    /// The operator environment (System, uid 1) of this module system.
    Operator(&'a Env),
    /// A trusted session checker, run inside the operation transaction.
    Session(&'a dyn crate::install::InstallAuthorizer),
}

/// Who may run module operations: the `System` principal with full
/// elevation (uid 1) until M3 provides `base.group_system`, in an environment made by this
/// module system's ORM lineage.
/// An
/// environment of another ORM (another database, or another lineage on the same pool) is refused
/// whatever its principal.
fn check_access(root: &Orm, caller: &Env, method: &'static str) -> Result<()> {
    let principal = caller.principal();
    // `with_user(SUPERUSER_ID)` from another user's environment is System/full/uid 1 too, but
    // its actor is the impersonated principal: that is `sudo`, not the superuser, and module
    // operations refuse it. The denied uid names the real caller.
    let system = caller.uid() == SUPERUSER_ID
        && principal.kind() == erp_orm::PrincipalKind::System
        && principal.elevation().is_full()
        && principal.actor().is_none();
    if !system {
        let uid = match principal.actor() {
            Some(erp_orm::Actor::Impersonator(original)) => original.subject().get(),
            Some(_) | None => caller.uid(),
        };
        return Err(Error::AccessDenied { method, uid });
    }
    if !caller.same_lineage(root) {
        return Err(Error::ForeignEnvironment { method });
    }
    Ok(())
}

/// The limits in force for the statements of an operation's transaction: the operation's
/// (lowered to the caller's session when that is lower), or those of a stage that lowered them
/// further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limits {
    lock: Duration,
    /// Zero when the session has no `statement_timeout` (only in the wait for the operation lock,
    /// where the module system sets none of its own).
    statement: Duration,
}

impl Limits {
    /// The limits while the ORM's schema step runs: it lowers both to its own bounds
    /// (`SCHEMA_LOCK_TIMEOUT`, `SCHEMA_STATEMENT_TIMEOUT`) and keeps lower values.
    fn during_schema_step(self) -> Self {
        Self {
            lock: self.lock.min(SCHEMA_LOCK_TIMEOUT_DURATION),
            statement: if self.statement.is_zero() {
                SCHEMA_STATEMENT_TIMEOUT_DURATION
            } else {
                self.statement.min(SCHEMA_STATEMENT_TIMEOUT_DURATION)
            },
        }
    }

    /// Type a lock that was not available (`55P03`) or a canceled statement (`57014`), naming
    /// the limit in force without claiming it was the cause (`NOWAIT` and `pg_cancel_backend`
    /// give the same codes; PostgreSQL's message, the source, says which), also inside step,
    /// purge and rollback wrappers. A serialization failure (`40001`) or deadlock (`40P01`) of
    /// the operation becomes [`Error::ConcurrentChange`] ([`Self::classify_with`] with
    /// `Concurrent::Lift`); any other error is returned as it is.
    fn classify(self, error: Error) -> Error {
        self.classify_with(error, Concurrent::Lift)
    }

    /// [`Self::classify`]; `concurrent` says whether a serialization failure or deadlock becomes
    /// [`Error::ConcurrentChange`] (the retry advice): not for the error of a reset after a
    /// failure, which is no part of the operation to retry (review A4-R2).
    fn classify_with(self, error: Error, concurrent: Concurrent) -> Error {
        if concurrent == Concurrent::Lift
            && let Some(conflict) = concurrent_change(&error)
        {
            return Error::ConcurrentChange {
                conflict,
                source: Box::new(error),
            };
        }
        match error {
            Error::Orm(e) => match e.sqlstate() {
                Some("55P03") => Error::LockNotAvailable {
                    lock_timeout: self.lock,
                    source: e,
                },
                Some("57014") => Error::StatementCanceled {
                    statement_timeout: self.statement,
                    source: e,
                },
                Some(_) | None => Error::Orm(e),
            },
            Error::StepFailed {
                module,
                step,
                file,
                source,
            } => match self.classify_with(*source, concurrent) {
                // the retry advice goes outermost; the step stays its source
                Error::ConcurrentChange { conflict, source } => Error::ConcurrentChange {
                    conflict,
                    source: Box::new(Error::StepFailed {
                        module,
                        step,
                        file,
                        source,
                    }),
                },
                source => Error::StepFailed {
                    module,
                    step,
                    file,
                    source: Box::new(source),
                },
            },
            Error::PurgeFailed { modules, source } => {
                match self.classify_with(*source, concurrent) {
                    // as for a step (review A4-R2)
                    Error::ConcurrentChange { conflict, source } => Error::ConcurrentChange {
                        conflict,
                        source: Box::new(Error::PurgeFailed { modules, source }),
                    },
                    source => Error::PurgeFailed {
                        modules,
                        source: Box::new(source),
                    },
                }
            }
            // a failed reset or rollback stays outermost: the database may need attention before
            // a retry; the operation's own error inside is classified
            Error::ResetFailed { error, reset } => Error::ResetFailed {
                error: Box::new(self.classify_with(*error, concurrent)),
                reset: Box::new(self.classify_with(*reset, Concurrent::Keep)),
            },
            Error::RollbackFailed { error, rollback } => Error::RollbackFailed {
                error: Box::new(self.classify_with(*error, concurrent)),
                rollback,
            },
            other => other,
        }
    }
}

/// Whether [`Limits::classify_with`] turns a conflict into the retry advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Concurrent {
    Lift,
    Keep,
}

/// The conflict of `error` when it is a serialization failure or a deadlock reported by the ORM
/// or the data loader: the operation lost to a concurrent transaction (a user editing a view
/// tree the operation's view check locks, review A3-D2) and can be run again.
fn concurrent_change(error: &Error) -> Option<crate::error::Conflict> {
    let orm = match error {
        Error::Orm(e) => e,
        Error::DataLoad(e) => match e.kind() {
            erp_data::load::LoadErrorKind::Orm(e) => e,
            _ => return None,
        },
        _ => return None,
    };
    if orm.kind() != erp_fields::ErrorKind::Concurrency {
        return None;
    }
    match orm.sqlstate() {
        Some("40001") => Some(crate::error::Conflict::SerializationFailure),
        Some("40P01") => Some(crate::error::Conflict::Deadlock),
        _ => None,
    }
}

/// The process's operation key, claimed on first use and kept private to this crate: only the
/// module system manages transactions as module operations (`erp_orm::OperationKey` describes
/// the trust boundary). A failed claim (other code took the key first) is sticky and reported
/// by every operation.
fn operation_key() -> Result<&'static OperationKey> {
    static KEY: std::sync::OnceLock<std::result::Result<OperationKey, String>> =
        std::sync::OnceLock::new();
    match KEY.get_or_init(|| OperationKey::claim().map_err(|e| e.message().to_owned())) {
        Ok(key) => Ok(key),
        Err(message) => Err(Error::Orm(erp_fields::Error::programming(message.clone()))),
    }
}

/// The transaction of one module operation, with the right to end it and what identifies it.
/// Hooks, data loaders and model methods get `&mut Transaction` only: they cannot commit, roll
/// back or switch it ([`Transaction::manage_operation`]).
struct OperationTx {
    tx: Transaction,
    control: OperationControl,
    /// The limits in force after the transaction began.
    limits: Limits,
    /// The transaction's identity: its start time, and the lock for a transaction under
    /// [`OPERATION_LOCK`]; checked after every hook and before the commit.
    guard: Guard,
}

impl OperationTx {
    /// A transaction on `orm` that only the module system ends, without the operation lock (a
    /// plain load's `_register_hook` pass).
    async fn plain(orm: &Orm) -> Result<Self> {
        let mut tx = orm.begin(SUPERUSER_ID).await?;
        let control = tx.manage_operation(operation_key()?)?;
        let mut optx = Self {
            tx,
            control,
            limits: Limits {
                lock: Duration::ZERO,
                statement: Duration::ZERO,
            },
            // replaced by the transaction's own identity just below
            guard: Guard {
                began: String::new(),
                holds_lock: false,
            },
        };
        match read_caller_session(&mut optx.tx).await {
            Ok(session) => {
                optx.guard = session.guard;
                Ok(optx)
            }
            Err(error) => Err(rollback_after(&mut optx, error).await),
        }
    }

    /// [`verify_intact`] for this transaction: the whole check, before the commit.
    async fn verify_intact(&mut self) -> Result<()> {
        verify_intact(&mut self.tx, &self.guard, Check::Full).await
    }

    /// Commit, after checking that the transaction is still the operation's.
    async fn commit(&mut self) -> Result<()> {
        self.verify_intact().await?;
        match self.tx.operation_commit_classified(&self.control).await {
            Ok(()) => Ok(()),
            Err(erp_orm::CommitError::RolledBack(error)) => Err(error.into()),
            // CommitError is non-exhaustive: only its explicit RolledBack variant proves
            // rollback. Unknown (or any future classification) requires reconciliation.
            Err(error) => Err(Error::CommitOutcomeUnknown {
                source: error.into_error(),
            }),
        }
    }

    async fn rollback(&mut self) -> erp_fields::Result<()> {
        self.tx.operation_rollback(&self.control).await
    }
}

/// How much [`verify_intact`] checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// The transaction is the one the operation began (one cheap comparison): after every hook,
    /// loader call and model method.
    Timestamp,
    /// Also that the backend still holds [`OPERATION_LOCK`] (a catalog lookup): before the
    /// commit.
    Full,
}

/// Whether the session still runs the transaction the operation began (`guard.began`: its
/// `transaction_timestamp()`, compared as `timestamptz`) and, for [`Check::Full`] on a
/// transaction under the lock, still holds [`OPERATION_LOCK`]: a hook that sent `COMMIT` (or
/// `ROLLBACK`, or `COMMIT; BEGIN`) through a raw statement ended it, and the statements since ran
/// in autocommit or in another transaction. Checked after every hook, loader call and model
/// method of an operation, so a transaction ended behind the module system's back stops the
/// operation at once instead of letting it write in autocommit.
async fn verify_intact(tx: &mut Transaction, guard: &Guard, check: Check) -> Result<()> {
    let full = check == Check::Full && guard.holds_lock;
    let rows = if full {
        tx.query(
            "SELECT transaction_timestamp() = $1::text::timestamptz, EXISTS (SELECT 1 FROM pg_locks \
             WHERE locktype = 'advisory' AND granted AND pid = pg_backend_pid() \
             AND classid::int8 = $2 AND objid::int8 = $3 AND objsubid = 2 \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))",
            &[
                &guard.began,
                &i64::from(OPERATION_LOCK.0),
                &i64::from(OPERATION_LOCK.1),
            ],
        )
        .await?
    } else {
        tx.query(
            "SELECT transaction_timestamp() = $1::text::timestamptz, true",
            &[&guard.began],
        )
        .await?
    };
    let row = rows.first().ok_or_else(|| Error::InvariantViolation {
        message: "the operation transaction check returned no row".to_owned(),
    })?;
    let shape = |e: tokio_postgres::Error| Error::InvariantViolation {
        message: format!("unexpected row of the operation transaction check: {e}"),
    };
    let same_transaction: bool = row.try_get(0).map_err(shape)?;
    let holds_lock: bool = row.try_get(1).map_err(shape)?;
    if !same_transaction {
        return Err(Error::OperationTransactionEnded {
            detail: "the session runs another transaction than the one the operation began",
        });
    }
    if !holds_lock {
        return Err(Error::OperationTransactionEnded {
            detail: "the session no longer holds the operation lock",
        });
    }
    Ok(())
}

/// [`rollback_after`] for a plain read-only transaction that no hook ran in.
async fn rollback_plain(tx: &mut Transaction, error: Error) -> Error {
    match tx.rollback().await {
        Ok(()) => error,
        Err(rollback) => Error::RollbackFailed {
            error: Box::new(error),
            rollback: Box::new(rollback.into()),
        },
    }
}

/// Roll back after `error`; a failing rollback is returned with it, never instead of it.
async fn rollback_after(optx: &mut OperationTx, error: Error) -> Error {
    // Classified commit already discarded the connection. An unknown answer is not proof of
    // rollback, including for catalog/load helpers using `end_transaction`.
    if matches!(error, Error::CommitOutcomeUnknown { .. }) {
        return error;
    }
    match optx.rollback().await {
        Ok(()) => error,
        Err(rollback) => Error::RollbackFailed {
            error: Box::new(error),
            rollback: Box::new(rollback.into()),
        },
    }
}

/// Commit on success, roll back on failure (keeping the original error).
async fn end_transaction<T>(optx: &mut OperationTx, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            if let Err(error) = optx.commit().await {
                return Err(rollback_after(optx, error).await);
            }
            Ok(value)
        }
        Err(error) => Err(rollback_after(optx, error).await),
    }
}

/// Take [`OPERATION_LOCK`] for the rest of `tx`, without waiting.
async fn lock_operation(tx: &mut Transaction) -> Result<()> {
    let rows = tx
        .query(
            "SELECT pg_try_advisory_xact_lock($1, $2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await?;
    let locked: bool = rows
        .first()
        .ok_or_else(|| Error::InvariantViolation {
            message: "pg_try_advisory_xact_lock returned no row".to_owned(),
        })?
        .try_get(0)
        .map_err(|e| Error::InvariantViolation {
            message: format!("pg_try_advisory_xact_lock: {e}"),
        })?;
    if locked {
        Ok(())
    } else {
        Err(Error::OperationInProgress)
    }
}

/// The failure path of an operation, run in a task of its own (see [`ModuleSystem::finish`]):
/// roll the operation's transaction back, reset the transient states (unless the request was
/// refused before anything ran) and return the error to report, with what went wrong in the
/// rollback or the reset attached.
async fn fail_operation(
    root: Orm,
    timeouts: Timeouts,
    mut optx: OperationTx,
    error: Error,
    reset_states: bool,
) -> Error {
    let limits = optx.limits;
    let rolled_back = optx.rollback().await;
    drop(optx);
    if let Err(rollback) = rolled_back {
        return limits.classify(Error::RollbackFailed {
            error: Box::new(error),
            rollback: Box::new(rollback.into()),
        });
    }
    if !reset_states {
        return limits.classify(error);
    }
    match reset_after_failure(&root, timeouts, &error).await {
        Ok(()) => limits.classify(error),
        Err(reset) => limits.classify(Error::ResetFailed {
            error: Box::new(error),
            reset: Box::new(reset),
        }),
    }
}

/// Reset transient states after a failed operation, under the operation lock:
/// when another operation took the lock after the rollback, its states are left alone and
/// [`Error::OperationInProgress`] is returned (the caller reports both errors).
async fn reset_after_failure(root: &Orm, timeouts: Timeouts, error: &Error) -> Result<()> {
    // Nothing to reset when the operation could not even start.
    if matches!(
        error,
        Error::OperationInProgress
            | Error::OperationWaitTimeout { .. }
            | Error::DatabaseNotInitialized { .. }
            | Error::DatabaseAlreadyInitialized { .. }
    ) {
        return Ok(());
    }
    let mut optx = begin_operation(root, timeouts, LockWait::Refuse, false).await?;
    let limits = optx.limits;
    let result = async {
        if store::is_initialized(&mut optx.tx).await? {
            store::reset_transient_states(&mut optx.tx).await?;
        }
        Ok(())
    }
    .await;
    end_transaction(&mut optx, result)
        .await
        .map_err(|e| limits.classify(e))
}

/// Whether an operation waits for [`OPERATION_LOCK`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockWait {
    /// Refused at once while another operation holds it.
    Refuse,
    /// Wait up to the operation wait timeout (the forced update of a plain load).
    Wait,
}

/// Open the operation transaction and take the operation lock: refused at once,
/// without waiting, while another operation holds it. The lock is held to the end of the
/// transaction.
///
/// With [`LockWait::Wait`] (the forced update of a plain load), it waits for the lock up to the
/// operation wait timeout instead ([`Error::OperationWaitTimeout`]; only `lock_timeout` is set
/// for the wait, so a canceled wait stays what it is). Then the operation's `lock_timeout` and
/// `statement_timeout` apply to the rest of the transaction, each lowered to the session's own
/// when that is lower. `installing_signal` for the transaction that initializes the database: it
/// creates the signal table, so it cannot read it at `BEGIN`.
async fn begin_operation(
    root: &Orm,
    timeouts: Timeouts,
    wait: LockWait,
    installing_signal: bool,
) -> Result<OperationTx> {
    begin_operation_with_catalog_preflight(
        root,
        timeouts,
        wait,
        installing_signal,
        false,
        erp_orm::Isolation::RepeatableRead,
    )
    .await
}

// Authorized uninstall requires a same-connection check before the lazy BEGIN signal read.
// Existing lifecycle entry points retain their previous opening behavior.
async fn begin_operation_with_catalog_preflight(
    root: &Orm,
    timeouts: Timeouts,
    wait: LockWait,
    installing_signal: bool,
    native_source_preflight: bool,
    isolation: erp_orm::Isolation,
) -> Result<OperationTx> {
    // `ReadCommitted` is for the confirmed uninstall only: its planner takes ACCESS EXCLUSIVE
    // membership locks after the opening statements, and a REPEATABLE READ snapshot taken
    // before the lock wait would not see a writer that committed during it. With every writer
    // fenced out by the lock, per-statement snapshots are consistent and current. Such an
    // operation uses raw statements only (the record API refuses ReadCommitted loudly).
    let mut tx = root
        .begin_opts(erp_orm::BeginOptions::new(SUPERUSER_ID).isolation(isolation))
        .await?;
    if native_source_preflight {
        tx.preflight_native_catalog_sources().await?;
    }
    // The right to end the transaction stays here; hooks only see `&mut Transaction`.
    let control = tx.manage_operation(operation_key()?)?;
    let started = async {
        if installing_signal {
            tx.open_installing_signal().await?;
        }
        let session = read_caller_session(&mut tx).await?;
        let in_force = Limits {
            lock: lowered(session.lock_ms, timeouts.lock),
            statement: lowered(session.statement_ms, timeouts.statement),
        };
        match wait {
            LockWait::Refuse => lock_operation(&mut tx).await?,
            LockWait::Wait => {
                let limit = lowered(session.lock_ms, timeouts.operation_wait);
                set_timeouts(&mut tx, millis(limit), session.statement_ms).await?;
                let waited = tx
                    .query(
                        "SELECT pg_advisory_xact_lock($1, $2)",
                        &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
                    )
                    .await;
                match waited {
                    Ok(_) => {}
                    // Only the lock timeout is set for the wait: 55P03 is the limit running out.
                    // A 57014 is someone canceling the wait (or the session's own statement
                    // timeout) and keeps its source.
                    Err(e) if e.sqlstate() == Some("55P03") => {
                        return Err(Error::OperationWaitTimeout { limit });
                    }
                    Err(e) => {
                        let waiting = Limits {
                            lock: limit,
                            statement: Duration::from_millis(session.statement_ms),
                        };
                        return Err(waiting.classify(e.into()));
                    }
                }
            }
        }
        set_timeouts(&mut tx, millis(in_force.lock), millis(in_force.statement)).await?;
        Ok::<_, Error>((in_force, session.guard))
    }
    .await;
    match started {
        Ok((limits, guard)) => Ok(OperationTx {
            tx,
            control,
            limits,
            guard: Guard {
                holds_lock: true,
                ..guard
            },
        }),
        Err(error) => {
            let mut failed = OperationTx {
                tx,
                control,
                limits: Limits {
                    lock: timeouts.lock,
                    statement: timeouts.statement,
                },
                guard: Guard {
                    began: String::new(),
                    holds_lock: false,
                },
            };
            Err(rollback_after(&mut failed, error).await)
        }
    }
}

/// Wrap the error of one step of `module` (lead review P): the caller learns the module, the step
/// and the file; the step's own error stays the source.
fn step_error(
    module: &ModuleName,
    step: ModuleStep,
    file: Option<&str>,
) -> impl FnOnce(Error) -> Error {
    let module = module.clone();
    let file = file.map(str::to_owned);
    move |source| Error::StepFailed {
        module,
        step,
        file,
        source: Box::new(source),
    }
}

/// Run blocking file system work (the migration scripts of a development folder) off the async
/// workers, as the data file reads do.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::InvariantViolation {
            message: format!("the migration file reader task failed: {e}"),
        })?
}

/// The selected migration steps of one module, per stage, and the installed version they
/// receive.
#[derive(Debug)]
struct StagedSteps {
    installed: SemverVersion,
    pre: Vec<MigrationStep>,
    post: Vec<MigrationStep>,
    end: Vec<MigrationStep>,
}

impl StagedSteps {
    fn new(installed: SemverVersion) -> Self {
        Self {
            installed,
            pre: Vec::new(),
            post: Vec::new(),
            end: Vec::new(),
        }
    }

    fn push(&mut self, step: MigrationStep) {
        match step.stage {
            MigrationStage::Pre => self.pre.push(step),
            MigrationStage::Post => self.post.push(step),
            MigrationStage::End => self.end.push(step),
        }
    }
}

/// How an operation failed.
enum Failure {
    /// The request was refused before anything ran: nothing to reset.
    Refused(Error),
    /// The run failed: transient states are reset.
    Failed(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

impl From<erp_fields::Error> for Failure {
    fn from(error: erp_fields::Error) -> Self {
        Self::Failed(error.into())
    }
}

/// Whether an operation runs the post-commit phase after it commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterCommit {
    /// Run it (immediate operations, initialization).
    RunPostCommit,
    /// Leave the steps pending (the forced update of a plain load, R3 of the delta review: a load
    /// never blocks on index builds).
    LeavePending,
}

/// What one operation asks for.
#[derive(Debug, Clone)]
enum Request {
    Install(Vec<ModuleName>),
    Upgrade(Vec<ModuleName>),
    /// Like `Upgrade`, and rewrite generated company restrictions that were changed since they
    /// were generated (`--restore-generated-restrictions`, design `m3-company-a1.md` §6.3).
    UpgradeRestoring(Vec<ModuleName>),
    /// Like `Upgrade`, but the data loads in `init` mode.
    Reinit(Vec<ModuleName>),
    /// Set `demo` on every module and load the demo files of every installed module, nothing
    /// else: `force_demo`.
    ForceDemo,
    Uninstall(Vec<ModuleName>),
    /// Run whatever is scheduled (plain load with pending states, initialization).
    Pending,
}

/// Per-operation state of the runner.
struct Run<'s> {
    package_identity_upgrade: Option<crate::wasm_lifecycle::PackageIdentityUpgrade>,
    system: &'s ModuleSystem,
    entries: IndexMap<ModuleName, AddonEntry>,
    /// Final registry addons, in dependency/load order, set only after a successful update.
    active: Vec<AddonEntry>,
    installed: Vec<ModuleName>,
    upgraded: Vec<ModuleName>,
    uninstalled: Vec<ModuleName>,
    unavailable: Vec<ModuleName>,
    /// Demo flag of a new database's `base` ('s first round).
    base_demo: bool,
    /// Modules left out of the update graph that were not scheduled (scheduled ones are an
    /// error).
    dropped: DropReport,
    /// The operation transaction's identity ([`verify_intact`]), set when the update run starts.
    guard: Option<Guard>,
    /// Bytes of data files read so far by this operation (against
    /// [`ModuleSystemBuilder::max_operation_data_bytes`]).
    data_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// What this operation's SQL migrations may select and read; its bytes go to `data_bytes`.
    migration_budget: MigrationBudget,
    /// Upgraded modules load their data in `init` mode (`Request::Reinit`).
    reinit: bool,
    /// Load only the demo files and leave the schema alone (`Request::ForceDemo`): every module
    /// of the run is already installed, so its tables, data and views are in place.
    demo_only: bool,
    /// Rewrite changed generated company restrictions instead of failing
    /// (`Request::UpgradeRestoring`).
    restore_generated: bool,
    /// The generated restrictions rewritten by this operation
    /// ([`OperationOutcome::restored_restrictions`]).
    restored: Vec<crate::company_access::GeneratedRestrictionRestored>,
    /// What the data loading reported ([`OperationOutcome::data_notices`]).
    notices: Vec<DataNotice>,
}

impl Parts {
    /// The addon index under its lock.
    fn with_index<T>(&self, f: impl FnOnce(&mut AddonIndex) -> T) -> T {
        // A poisoned lock means a panic while the index was borrowed; its caches are rebuilt
        // from disk on demand, so the index stays usable.
        let mut index = self.index.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut index)
    }

    fn discover(&self) -> Result<IndexMap<ModuleName, AddonEntry>> {
        let entries = self.with_index(AddonIndex::discover_entries)?;
        self.check_code(&entries)?;
        Ok(entries)
    }

    /// Hooks pair with manifests; native descriptors agree with their manifests.
    fn check_code(&self, entries: &IndexMap<ModuleName, AddonEntry>) -> Result<()> {
        for entry in entries.values() {
            let manifest = entry.manifest();
            let module = manifest.name();
            let name = module.as_str();
            if let Some(addon) = self.code.get(name) {
                if addon.is_exact_only() != manifest.exact_only() {
                    return Err(Error::AddonCodeMismatch {
                        module: module.clone(),
                        message: "the code and manifest.toml disagree on exact_only".to_owned(),
                    });
                }
                let code: BTreeSet<&str> = addon.depends.iter().copied().collect();
                let declared: BTreeSet<&str> =
                    manifest.depends().iter().map(ModuleName::as_str).collect();
                if code != declared {
                    return Err(Error::AddonCodeMismatch {
                        module: module.clone(),
                        message: format!(
                            "the code declares depends {code:?} but manifest.toml declares {declared:?}"
                        ),
                    });
                }
            }
            for (kind, named) in [
                (HookKind::PreInit, manifest.pre_init_hook()),
                (HookKind::PostInit, manifest.post_init_hook()),
                (HookKind::Uninstall, manifest.uninstall_hook()),
            ] {
                let impls: Vec<&&HookImpl> = self
                    .hooks
                    .iter()
                    .filter(|h| h.addon == name && h.kind == kind)
                    .collect();
                match named {
                    None => {
                        if let Some(orphan) = impls.first() {
                            return Err(Error::HookOrphan {
                                module: module.clone(),
                                kind,
                                name: orphan.name.to_owned(),
                                location: orphan.location(),
                            });
                        }
                    }
                    Some(hook) => {
                        if let Some(orphan) = impls.iter().find(|h| h.name != hook.as_str()) {
                            return Err(Error::HookOrphan {
                                module: module.clone(),
                                kind,
                                name: orphan.name.to_owned(),
                                location: orphan.location(),
                            });
                        }
                        match impls.as_slice() {
                            [] => {
                                return Err(Error::HookMissing {
                                    module: module.clone(),
                                    kind,
                                    name: hook.as_str().to_owned(),
                                });
                            }
                            [_] => {}
                            [first, second, ..] => {
                                return Err(Error::HookDuplicate {
                                    module: module.clone(),
                                    kind,
                                    name: hook.as_str().to_owned(),
                                    first: first.location(),
                                    second: second.location(),
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The implementation of hook `kind` of `module`: `None` when the manifest names none. A
    /// named hook without an implementation is refused when the system is built and at every
    /// discovery ([`Parts::check_code`]); meeting one here is a broken invariant, not a skip.
    fn hook(
        &self,
        module: &ModuleName,
        kind: HookKind,
        manifest: &Manifest,
    ) -> Result<Option<&'static HookImpl>> {
        let named = match kind {
            HookKind::PreInit => manifest.pre_init_hook(),
            HookKind::PostInit => manifest.post_init_hook(),
            HookKind::Uninstall => manifest.uninstall_hook(),
        };
        let Some(named) = named else {
            return Ok(None);
        };
        self.hooks
            .iter()
            .copied()
            .find(|h| h.addon == module.as_str() && h.kind == kind && h.name == named.as_str())
            .map(Some)
            .ok_or_else(|| Error::InvariantViolation {
                message: format!(
                    "module {module} names {kind} '{}' in manifest.toml but no implementation \
                     of it is registered (checked when the module system was built)",
                    named.as_str()
                ),
            })
    }

    /// The descriptor of a module: `base`/`web` from the registry, native code, or one made for a
    /// data-only addon.
    fn descriptor(
        &self,
        module: &ModuleName,
        entries: &IndexMap<ModuleName, AddonEntry>,
    ) -> Result<&'static AddonDescriptor> {
        // A native framework addon has the same name as the pure registry placeholder, but
        // its descriptor is the contributing owner's identity. Prefer it when
        // explicitly registered. Data/package-only roots retain the canonical framework owner.
        if let Some(addon) = self.code.get(module.as_str()) {
            return Ok(addon);
        }
        match module.as_str() {
            "base" => return Ok(&BASE_ADDON),
            "web" => return Ok(&WEB_ADDON),
            _ => {}
        }
        let entry = entries
            .get(module)
            .ok_or_else(|| Error::ModuleUnavailable {
                module: module.clone(),
            })?;
        let depends: Vec<String> = entry
            .manifest()
            .depends()
            .iter()
            .map(ModuleName::to_string)
            .collect();
        let key = (module.to_string(), depends, entry.manifest().exact_only());
        // A poisoned lock means a panic while a descriptor was being added; entries are inserted
        // whole after they are built, so the map is consistent and stays usable.
        let mut made = self
            .data_only
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(addon) = made.get(&key) {
            return Ok(addon);
        }
        // Leaked once per (name, dependencies, policy): registries need `&'static` descriptors, and the
        // set is bounded by the addons the system ever saw.
        let name: &'static str = Box::leak(key.0.clone().into_boxed_str());
        let deps: Vec<&'static str> = key
            .1
            .iter()
            .map(|d| &*Box::leak(d.clone().into_boxed_str()))
            .collect();
        let addon: &'static AddonDescriptor = Box::leak(Box::new(AddonDescriptor {
            name,
            depends: Box::leak(deps.into_boxed_slice()),
            exact_only: key.2,
        }));
        made.insert(key, addon);
        Ok(addon)
    }

    /// The factory that publishes the H18 view types of `registry`, composed from the actual
    /// selected module set `modules` and the dependency closure of their descriptors (the
    /// manifest-checked dependencies), not from the registry's implicit descriptors alone.
    fn view_type_factory(
        &self,
        registry: &Arc<Registry>,
        modules: &[ModuleName],
        entries: &IndexMap<ModuleName, AddonEntry>,
    ) -> Result<GenerationAttachmentFactory<PublishedViewTypes, ViewTypeInputs>> {
        crate::view_types::refuse_package_owners(&self.view_types, |name| {
            ModuleName::new(name)
                .ok()
                .and_then(|module| entries.get(&module))
                .is_some_and(|entry| matches!(entry.source_kind(), AddonSourceKind::Package { .. }))
        })?;
        let mut descriptors: Vec<&'static AddonDescriptor> = vec![
            &erp_data::model_data::ADDON,
            self.descriptor(&ModuleName::new("base")?, entries)?,
            self.descriptor(&ModuleName::new("web")?, entries)?,
        ];
        for module in modules {
            let descriptor = self.descriptor(module, entries)?;
            if !descriptors
                .iter()
                .any(|known| known.name == descriptor.name)
            {
                descriptors.push(descriptor);
            }
        }
        let pairs: Vec<(&str, &[&str])> = descriptors
            .iter()
            .map(|descriptor| (descriptor.name, descriptor.depends))
            .collect();
        let closure = crate::view_types::closure_of(&pairs)?;
        Ok(view_type_factory(
            registry,
            self.view_types.clone(),
            closure,
        ))
    }

    /// The registry of a module set (plain-load order is the registry's own).
    fn compose(
        &self,
        modules: &[ModuleName],
        entries: &IndexMap<ModuleName, AddonEntry>,
    ) -> Result<Arc<Registry>> {
        let mut addons: Vec<&'static AddonDescriptor> = vec![&erp_data::model_data::ADDON];
        for module in modules {
            addons.push(self.descriptor(module, entries)?);
        }
        // `web` is an always-present framework addon (RegistryBuilder's baseline). When its
        // native code is registered, use that actual descriptor even in the base-install root;
        // otherwise RegistryBuilder would inject an equal-named placeholder and owner binding
        // would fail. Ordinary linked addons remain absent until selected in `modules`.
        if !addons.iter().any(|addon| addon.name == "web") {
            addons.push(self.descriptor(&ModuleName::new("web")?, entries)?);
        }
        let names: BTreeSet<&str> = addons
            .iter()
            .map(|a| a.name)
            .chain(["base", "web"])
            .collect();
        let decls: Vec<&'static ModelDecl> = base_models::DECLS
            .iter()
            .copied()
            .chain([&erp_data::IR_MODEL_DATA])
            .chain(
                self.decls
                    .iter()
                    .copied()
                    .filter(|d| names.contains(d.addon)),
            )
            .collect();
        // `models/*.toml` of the data-only addons in this set: the same
        // declarations a native addon's `#[model]` produces, read from the addon folder on
        // every composition so an upgrade sees edited files. Only dev-folder addons have a
        // folder; a native addon declares its models in code and its `models/` is not read.
        let mut overlay = erp_registry::Overlay::new();
        for module in modules {
            if matches!(module.as_str(), "base" | "web") || self.code.contains_key(module.as_str())
            {
                continue;
            }
            match entries.get(module).map(AddonEntry::source_kind) {
                Some(AddonSourceKind::DevFolder(dir)) => {
                    let defs = match entries.get(module).and_then(AddonEntry::artifact_snapshot) {
                        Some(snapshot) => {
                            crate::model_files::read_snapshot_model_files(snapshot, module)?
                        }
                        None => crate::read_model_files(dir, module)?,
                    };
                    overlay
                        .add_addon_defs(defs)
                        .map_err(|source| Error::Registry {
                            addon: module.clone(),
                            source,
                        })?;
                }
                Some(AddonSourceKind::Package { .. }) => {
                    if let Some(runtime) = &self.package_runtime {
                        let defs = runtime.model_defs(module)?;
                        overlay
                            .add_addon_defs(defs)
                            .map_err(|source| Error::Registry {
                                addon: module.clone(),
                                source,
                            })?;
                    }
                }
                Some(AddonSourceKind::Embedded) | None => {}
            }
        }
        Ok(Arc::new(
            RegistryBuilder::new(addons)
                .with_decls(decls)
                .overlay(&overlay)
                .build()?,
        ))
    }
}

impl ModuleSystem {
    /// Start building the module system of the database behind `pool`.
    pub fn builder(pool: Pool, index: AddonIndex) -> ModuleSystemBuilder {
        ModuleSystemBuilder {
            pool,
            required_core_tables: Vec::new(),
            index,
            code: Vec::new(),
            native_artifact_identities: BTreeMap::new(),
            identity_previous: None,
            decls: Vec::new(),
            impls: Vec::new(),
            constraints: Vec::new(),
            hooks: Vec::new(),
            linked_hooks: true,
            migrations: Vec::new(),
            linked_migrations: true,
            loader: Some(Arc::new(ErpDataLoader::new())),
            schema_only_loader: true,
            config: OrmConfig::default(),
            signal: None,
            max_data_file_bytes: MAX_DATA_FILE_BYTES,
            max_data_files: MAX_DATA_FILES,
            max_operation_data_bytes: MAX_OPERATION_DATA_BYTES,
            max_migration_files: MAX_MIGRATION_FILES,
            timeouts: Timeouts::DEFAULT,
            schema_sync: SchemaSyncOptions::new(),
            package_runtime: None,
            lifecycle_hooks: Vec::new(),
            view_types: Vec::new(),
        }
    }

    /// The environment the entry points accept as caller: the `System` principal (uid 1) with the
    /// given context, made by this module system's own ORM. Environments of another ORM, even of
    /// the same database, are refused ([`Error::ForeignEnvironment`]); the ORM an outcome returns
    /// (`outcome.orm.env(SUPERUSER_ID, context)`) makes accepted ones too.
    ///
    /// **Whoever can call this runs module operations as the superuser:** a `ModuleSystem` must
    /// not be reachable from addon code or from an RPC handler. Keep it in the server's startup
    /// and administration code (the `erp module` command), never in a registry, a request state
    /// or anything a hook receives. An environment made by `with_user(SUPERUSER_ID)` from another
    /// user's environment is refused ([`Error::AccessDenied`]); this one is the superuser itself.
    pub fn system_env(&self, context: erp_orm::Context) -> Env {
        self.root.env(SUPERUSER_ID, context)
    }

    /// The addon index (for `erp dev` reloads: change sources, then call
    /// [`AddonIndex::invalidate_cache`]).
    pub fn with_index<T>(&self, f: impl FnOnce(&mut AddonIndex) -> T) -> T {
        self.parts.with_index(f)
    }

    /// Discover the addons on the blocking pool: it reads the manifests of the addon folders (no
    /// blocking I/O on the async runtime).
    async fn discover(&self) -> Result<IndexMap<ModuleName, AddonEntry>> {
        let parts = Arc::clone(&self.parts);
        tokio::task::spawn_blocking(move || parts.discover())
            .await
            .map_err(|e| Error::InvariantViolation {
                message: format!("the addon discovery task failed: {e}"),
            })?
    }

    /// The ORM of a module set, in the root ORM's database identity (with its runtime
    /// implementations).
    fn orm_for(
        &self,
        modules: &[ModuleName],
        entries: &IndexMap<ModuleName, AddonEntry>,
    ) -> Result<Orm> {
        let registry = self.parts.compose(modules, entries)?;
        let view_types = self.parts.view_type_factory(&registry, modules, entries)?;
        let Some(runtime) = &self.parts.package_runtime else {
            return Ok(self
                .root
                .rebuild(registry)
                .generation_attachment_with(view_types)
                .build()?);
        };
        // A package's typed model references are checked against the registry it will run in, so
        // an install or upgrade that cannot work is refused (and rolled back) by name here.
        runtime.check_model_refs(&registry, modules)?;
        // The callable list of a WebAssembly addon is read from the registry's exposure table
        // (which the WebAssembly layers do not change: they cannot be `#[rpc]`), so the registry is
        // built once plain to read it, and again with the WebAssembly layers.
        // The computes are in the plain build too: a package's computed field needs its compute
        // to be part of a registry at all. Its constraints are not (their methods are the
        // WebAssembly layers, which only the second build has).
        let checks = runtime.checks(modules)?;
        let plain = self
            .root
            .rebuild(Arc::clone(&registry))
            .computes(checks.computes.iter().copied())
            .generation_attachment_with(self.parts.view_type_factory(&registry, modules, entries)?)
            .build()?;
        let methods = runtime.wasm_methods(&plain, modules)?;
        if methods.is_empty() && checks.is_empty() {
            return Ok(plain);
        }
        Ok(self
            .root
            .rebuild(registry)
            .wasm_methods(methods)
            .computes(checks.computes)
            .constraints(checks.constraints)
            .generation_attachment_with(view_types)
            .build()?)
    }

    /// The migration steps selected for `module`, upgraded from `installed` to `target`, per
    /// stage: its native migrations and the SQL scripts of its development folder.
    /// Only the selected version folders are read, each selected file charged to the
    /// operation's `budget` before it is read. `None` when the module has no migrations.
    ///
    /// # Errors
    /// [`Error::InvalidModuleRow`] when the module has migrations but its row stores no installed
    /// version (they cannot be selected without it); the errors of
    /// [`crate::migrations::migration_version_folders`] and
    /// [`crate::migrations::read_version_folder`].
    async fn migrations_for(
        &self,
        module: &ModuleName,
        entries: &IndexMap<ModuleName, AddonEntry>,
        installed: Option<SemverVersion>,
        target: SemverVersion,
        budget: &MigrationBudget,
    ) -> Result<Option<StagedSteps>> {
        let natives: Vec<&NativeMigration> = self
            .parts
            .migrations
            .iter()
            .filter(|m| &m.module == module)
            .collect();
        // SQL scripts come from a development folder only: an embedded or packaged addon has no
        // file tree to read, its migrations are native ones (as its data files, which
        // `read_data_bytes` refuses for these sources).
        let dir: Option<PathBuf> = match entries.get(module).map(AddonEntry::source_kind) {
            Some(AddonSourceKind::DevFolder(dir)) => Some(dir.clone()),
            Some(AddonSourceKind::Embedded | AddonSourceKind::Package { .. }) | None => None,
        };
        let snapshot = entries
            .get(module)
            .and_then(AddonEntry::artifact_snapshot)
            .cloned();
        let snapshot_folders = match &snapshot {
            Some(snapshot) => Some(crate::migrations::snapshot_version_folders(
                snapshot,
                module,
                self.parts.max_migration_files,
            )?),
            None => None,
        };
        let has_sql = if let Some(folders) = &snapshot_folders {
            !folders.is_empty()
        } else {
            match &dir {
                Some(dir) => {
                    let (dir, name) = (dir.clone(), module.clone());
                    let max_folders = self.parts.max_migration_files;
                    !blocking(move || {
                        crate::migrations::migration_version_folders(&dir, &name, max_folders)
                    })
                    .await?
                    .is_empty()
                }
                None => false,
            }
        };
        if natives.is_empty() && !has_sql {
            return Ok(None);
        }
        let Some(installed) = installed else {
            return Err(Error::InvalidModuleRow {
                module: module.clone(),
                message: "the module is upgraded and has migrations, but its row stores no \
                          installed version (latest_version), so they cannot be selected"
                    .to_owned(),
            });
        };
        let mut staged = StagedSteps::new(installed);
        for native in natives {
            if crate::migrations::should_run_migration(&native.version, &installed, &target) {
                staged.push(MigrationStep::native(native));
            }
        }
        if let (Some(snapshot), Some(folders)) = (snapshot, snapshot_folders) {
            for folder in folders {
                if crate::migrations::should_run_migration(&folder.version, &installed, &target) {
                    for step in crate::migrations::read_snapshot_version_folder(
                        &snapshot,
                        module,
                        &folder,
                        self.parts.max_data_file_bytes,
                        budget,
                    )? {
                        staged.push(step);
                    }
                }
            }
        } else if let Some(dir) = dir {
            let name = module.clone();
            let limit = self.parts.max_data_file_bytes;
            let max_folders = self.parts.max_migration_files;
            let budget = budget.clone();
            let steps = blocking(move || {
                let mut steps = Vec::new();
                for folder in
                    crate::migrations::migration_version_folders(&dir, &name, max_folders)?
                {
                    if crate::migrations::should_run_migration(&folder.version, &installed, &target)
                    {
                        steps.extend(crate::migrations::read_version_folder(
                            &dir, &name, &folder, limit, &budget,
                        )?);
                    }
                }
                Ok(steps)
            })
            .await?;
            for step in steps {
                staged.push(step);
            }
        }
        Ok(Some(staged))
    }

    /// [`begin_operation`] on the root ORM with the configured timeouts.
    async fn begin_operation(&self, wait: LockWait) -> Result<OperationTx> {
        begin_operation(&self.root, self.parts.timeouts, wait, false).await
    }

    async fn require_initialized(&self, tx: &mut Transaction) -> Result<()> {
        if store::is_initialized(tx).await? {
            Ok(())
        } else {
            Err(Error::DatabaseNotInitialized {
                database: store::database_name(tx).await?,
            })
        }
    }

    /// A transaction of the root ORM as the superuser, for reads that belong to the module
    /// system's own tables (the caller rolls it back).
    pub(crate) async fn begin_read(&self) -> Result<Transaction> {
        Ok(self.root.begin(SUPERUSER_ID).await?)
    }

    /// The module catalog as stored (no manifests attached), read in a transaction of its own
    /// that is rolled back.
    pub(crate) async fn read_catalog(&self) -> Result<Catalog> {
        let mut tx = self.root.begin(SUPERUSER_ID).await?;
        let loaded = async {
            self.require_initialized(&mut tx).await?;
            store::load(&mut tx).await
        }
        .await;
        tx.rollback().await?;
        loaded
    }

    async fn load_catalog(
        &self,
        tx: &mut Transaction,
        entries: &IndexMap<ModuleName, AddonEntry>,
    ) -> Result<(Catalog, UpdateListResult)> {
        let mut catalog = store::load(tx).await?;
        let synced = catalog.update_list(entries.values().map(AddonEntry::manifest))?;
        Ok((catalog, synced))
    }

    // ---- scheduling (MOD §4.2.1–4.2.3, 4.2.5) --------------------------------------------

    /// Run a scheduling method on the catalog and store the result in its own transaction,
    /// under the operation lock: a running operation's states are never overwritten
    /// ([`Error::OperationInProgress`] instead).
    async fn schedule<T>(&self, f: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
        let entries = self.discover().await?;
        let mut optx = self.begin_operation(LockWait::Refuse).await?;
        let limits = optx.limits;
        let result = async {
            self.require_initialized(&mut optx.tx).await?;
            let (mut catalog, _) = self.load_catalog(&mut optx.tx, &entries).await?;
            let value = f(&mut catalog)?;
            store::save(&mut optx.tx, &mut catalog).await?;
            Ok(value)
        }
        .await;
        end_transaction(&mut optx, result)
            .await
            .map_err(|e| limits.classify(e))
    }

    /// `button_install`: mark modules and their dependencies `to install`.
    ///
    /// # Errors
    /// [`Error::AccessDenied`] for a non-superuser;
    /// [`Error::OperationInProgress`] while an operation runs; the scheduling errors of
    /// [`Catalog::button_install`].
    pub async fn button_install(
        &self,
        caller: &Env,
        names: &[ModuleName],
        options: &InstallOptions,
    ) -> Result<ModuleAction> {
        check_access(&self.root, caller, "button_install")?;
        self.schedule(|c| c.button_install(names, options)).await
    }

    /// `button_upgrade`.
    ///
    /// # Errors
    /// As [`ModuleSystem::button_install`].
    pub async fn button_upgrade(
        &self,
        caller: &Env,
        names: &[ModuleName],
        options: &InstallOptions,
    ) -> Result<ModuleAction> {
        check_access(&self.root, caller, "button_upgrade")?;
        self.schedule(|c| c.button_upgrade(names, options)).await
    }

    /// `button_uninstall`.
    ///
    /// # Errors
    /// As [`ModuleSystem::button_install`].
    pub async fn button_uninstall(
        &self,
        caller: &Env,
        names: &[ModuleName],
    ) -> Result<ModuleAction> {
        check_access(&self.root, caller, "button_uninstall")?;
        self.schedule(|c| c.button_uninstall(names)).await
    }

    /// `button_reset_state`: transient states back to stable ones.
    ///
    /// # Errors
    /// As [`ModuleSystem::button_install`].
    pub async fn button_reset_state(&self, caller: &Env) -> Result<Vec<ModuleName>> {
        check_access(&self.root, caller, "button_reset_state")?;
        self.schedule(|c| Ok(c.reset_state())).await
    }

    /// `update_list`: sync the module rows with the addon sources.
    ///
    /// # Errors
    /// As [`ModuleSystem::button_install`].
    pub async fn update_list(&self, caller: &Env) -> Result<UpdateListResult> {
        check_access(&self.root, caller, "update_list")?;
        let entries = self.discover().await?;
        let mut optx = self.begin_operation(LockWait::Refuse).await?;
        let limits = optx.limits;
        let result = async {
            self.require_initialized(&mut optx.tx).await?;
            let (mut catalog, synced) = self.load_catalog(&mut optx.tx, &entries).await?;
            store::save(&mut optx.tx, &mut catalog).await?;
            Ok(synced)
        }
        .await;
        end_transaction(&mut optx, result)
            .await
            .map_err(|e| limits.classify(e))
    }

    // ---- immediate operations (MOD §4.2.4) ----------------------------------------------

    /// `button_immediate_install`: schedule and install in one transaction.
    /// The shared engine heap-pins its large inner lifecycle future, so callers
    /// may await directly without a lifecycle-specific boxing workaround.
    ///
    /// The returned [`OperationOutcome::orm`] has the registry of the new installed set: switch
    /// to it and drop the ORM you called with (its registry is the one before the install).
    ///
    /// # Errors
    /// [`Error::AccessDenied`]; [`Error::OperationInProgress`] while another operation runs;
    /// scheduling errors; [`Error::StepFailed`] naming the module and step (hook, schema,
    /// `init()`, data file) that failed; [`Error::LockNotAvailable`] / [`Error::StatementCanceled`];
    /// after any error nothing of the operation persists and transient states are reset.
    /// A failing post-commit step is not an error: it is in
    /// [`OperationOutcome::post_commit`].
    pub async fn button_immediate_install(
        &self,
        caller: &Env,
        names: &[ModuleName],
    ) -> Result<OperationOutcome> {
        check_access(&self.root, caller, "button_immediate_install")?;
        self.operation(
            Request::Install(names.to_vec()),
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
        )
        .await
    }

    /// Install addons through a primary-session checker (and its 2026-10-27
    /// amendment): linked native addons, and data-only addons found in the configured addon
    /// directory. The request carries names only.
    /// # Errors
    /// Authentication, authority, lifecycle or publication failure;
    /// [`Error::AddonCodeMismatch`] for a name that is neither linked nor an installable
    /// data-only addon of the addon directory.
    pub async fn install_authorized(
        &self,
        names: &[ModuleName],
        authorizer: &dyn crate::install::InstallAuthorizer,
    ) -> Result<OperationOutcome> {
        if names.is_empty() || names.len() > 64 {
            return Err(erp_fields::Error::validation(
                "Install requires between 1 and 64 addon names",
            )
            .into());
        }
        let entries = self.discover().await?;
        let mut seen = BTreeSet::new();
        for name in names {
            if !seen.insert(name) {
                return Err(Error::DuplicateModuleRow {
                    module: name.clone(),
                });
            }
            self.require_human_install(name, &entries)?;
        }
        self.operation_with_authorizer(
            Request::Install(names.to_vec()),
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
            Some(authorizer),
            false,
        )
        .await
    }

    /// Upgrade installed linked addons through the trusted primary-session checker.
    ///
    /// Authorization is rechecked under the global lifecycle lock. Every upgraded
    /// dependent must be linked; newly installed dependencies use human-install admission.
    /// This engine seam is not exposed by HTTP. Browser delivery additionally requires
    /// the installed-artifact identity safeguards.
    ///
    /// # Errors
    /// Validation for malformed requests, uninstalled targets, pending operations or
    /// unlinked upgrades; authentication/authority and lifecycle errors are preserved.
    /// Transactional failure rolls back state and audit; post-commit failures remain
    /// explicit in [`OperationOutcome::post_commit`].
    ///
    /// ```no_run
    /// # async fn upgrade(system: &erp_modules::ModuleSystem, names: &[erp_modules::ModuleName],
    /// # checker: &dyn erp_modules::install::InstallAuthorizer)
    /// # -> Result<erp_modules::OperationOutcome, erp_modules::Error> {
    /// system.upgrade_authorized(names, checker).await
    /// # }
    /// ```
    pub async fn upgrade_authorized(
        &self,
        names: &[ModuleName],
        authorizer: &dyn crate::install::InstallAuthorizer,
    ) -> Result<OperationOutcome> {
        if names.is_empty() || names.len() > 64 {
            return Err(erp_fields::Error::validation(
                "Upgrade requires between 1 and 64 addon names",
            )
            .into());
        }
        let mut seen = BTreeSet::new();
        if names.iter().any(|name| !seen.insert(name)) {
            return Err(
                erp_fields::Error::validation("Upgrade requires unique addon names").into(),
            );
        }
        self.operation_with_authorizer(
            Request::Upgrade(names.to_vec()),
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
            Some(authorizer),
            false,
        )
        .await
    }

    /// The addons a browser installation may offer: linked native addons and the data-only
    /// addons of the addon directory, each with its manifest, plus the addons of the directory
    /// that are refused and why (they are not offered; the caller reports them).
    ///
    /// # Errors
    /// The errors of addon discovery (a broken manifest anywhere in the directory fails the
    /// whole discovery, as it does for every other operation).
    pub async fn human_catalog(&self) -> Result<HumanCatalog> {
        let entries = self.discover().await?;
        let mut offered = Vec::new();
        let mut refused = Vec::new();
        for entry in entries.values() {
            let name = entry.name();
            if self.parts.code.contains_key(name.as_str()) {
                offered.push(Arc::clone(entry.manifest()));
            } else {
                match data_only_refusal(entry) {
                    None => offered.push(Arc::clone(entry.manifest())),
                    Some(reason) => refused.push((name.clone(), reason)),
                }
            }
        }
        offered.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(HumanCatalog { offered, refused })
    }

    /// A native addon must be linked; a data-only addon must be installable from the addon
    /// directory (the explicit second branch's amendment).
    fn require_human_install(
        &self,
        name: &ModuleName,
        entries: &IndexMap<ModuleName, AddonEntry>,
    ) -> Result<()> {
        if self.parts.code.contains_key(name.as_str()) {
            return self.require_native_install(name);
        }
        let mismatch = |message: String| Error::AddonCodeMismatch {
            module: name.clone(),
            message,
        };
        let Some(entry) = entries.get(name) else {
            return Err(mismatch(
                "Human installation needs a linked native addon or a data-only addon in the \
                 addon directory; this name is neither"
                    .to_owned(),
            ));
        };
        match data_only_refusal(entry) {
            None => Ok(()),
            Some(reason) => Err(mismatch(format!(
                "Human installation of an addon that is not linked requires a data-only addon: {reason}"
            ))),
        }
    }

    fn require_native_install(&self, name: &ModuleName) -> Result<()> {
        if !self.parts.code.contains_key(name.as_str()) {
            return Err(Error::AddonCodeMismatch {
                module: name.clone(),
                message: "Human installation requires locally linked native addon code".to_owned(),
            });
        }
        Ok(())
    }

    /// `button_immediate_upgrade`. Switch to [`OperationOutcome::orm`] afterwards.
    ///
    /// # Errors
    /// As [`ModuleSystem::button_immediate_install`].
    pub async fn button_immediate_upgrade(
        &self,
        caller: &Env,
        names: &[ModuleName],
    ) -> Result<OperationOutcome> {
        check_access(&self.root, caller, "button_immediate_upgrade")?;
        self.operation(
            Request::Upgrade(names.to_vec()),
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
        )
        .await
    }

    /// `button_immediate_upgrade` that also rewrites generated company restrictions changed since
    /// they were generated (`erp addon update --restore-generated-restrictions`); each rewritten
    /// row is reported in [`OperationOutcome::restored_restrictions`].
    ///
    /// # Errors
    /// As [`ModuleSystem::button_immediate_upgrade`].
    pub async fn button_immediate_upgrade_restoring(
        &self,
        caller: &Env,
        names: &[ModuleName],
    ) -> Result<OperationOutcome> {
        check_access(&self.root, caller, "button_immediate_upgrade")?;
        self.operation(
            Request::UpgradeRestoring(names.to_vec()),
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
        )
        .await
    }

    /// A
    /// module whose demo data fails keeps `demo = false`, is rolled back alone and is reported in
    /// [`OperationOutcome::data_notices`] as [`NoticeKind::DemoFailed`]; the rest still load.
    /// Irreversible: nothing removes demo records afterwards. Switch to [`OperationOutcome::orm`]
    /// afterwards.
    ///
    /// # Errors
    /// As [`ModuleSystem::button_immediate_upgrade`]; a failure that is not a demo file's own
    /// mistake (the database refusing a statement) fails the whole operation. Refused before
    /// anything changes with [`crate::Error::ForceDemoPending`] (a module operation is scheduled),
    /// [`crate::Error::ForceDemoVersionMismatch`] (a manifest version differs from the installed one) or
    /// [`crate::Error::ForceDemoMissingDependency`] (an installed addon depends on an uninstalled one).
    pub async fn force_demo(&self, caller: &Env) -> Result<OperationOutcome> {
        check_access(&self.root, caller, "force_demo")?;
        self.operation(
            Request::ForceDemo,
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
        )
        .await
    }

    /// [`ModuleSystem::force_demo`] for the browser: the primary-session checker rechecks the
    /// administrator under the lifecycle lock and the run is audited as
    /// `force_demo` in `erp_module_lifecycle_audit`.
    ///
    /// # Errors
    /// As [`ModuleSystem::force_demo`]; authentication and authority errors of the checker;
    /// scheduled module operations left pending refuse the request.
    pub async fn force_demo_authorized(
        &self,
        authorizer: &dyn crate::install::InstallAuthorizer,
    ) -> Result<OperationOutcome> {
        self.operation_with_authorizer(
            Request::ForceDemo,
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
            Some(authorizer),
            false,
        )
        .await
    }

    /// No migration scripts run; the modules stay
    /// `installed`. Switch to [`OperationOutcome::orm`] afterwards.
    ///
    /// # Errors
    /// As [`ModuleSystem::button_immediate_upgrade`].
    pub async fn reinit(&self, caller: &Env, names: &[ModuleName]) -> Result<OperationOutcome> {
        check_access(&self.root, caller, "reinit")?;
        self.operation(
            Request::Reinit(names.to_vec()),
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
        )
        .await
    }

    /// `button_immediate_uninstall`. Switch to [`OperationOutcome::orm`]
    /// afterwards: the old ORM still has the removed models, whose tables are gone.
    ///
    /// # Errors
    /// As [`ModuleSystem::button_immediate_install`]; [`Error::UninstallOwnedRecords`] /
    /// [`Error::PurgeIncomplete`] when records of the modules would be orphaned;
    /// [`Error::SchemaDropBlocked`] when other objects use the removed tables or columns.
    pub async fn button_immediate_uninstall(
        &self,
        caller: &Env,
        names: &[ModuleName],
    ) -> Result<OperationOutcome> {
        check_access(&self.root, caller, "button_immediate_uninstall")?;
        self.operation(
            Request::Uninstall(names.to_vec()),
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
        )
        .await
    }

    /// The engine heap-pins its large inner lifecycle future; callers may await
    /// directly. Addon callbacks retain responsibility for their own stack requirements.
    ///
    /// Initialize an empty database: base tables, one row per known
    /// addon with its category chain and external ids, the auto-install seed, then the install
    /// run, all in one transaction.
    ///
    /// # Errors
    /// [`Error::DatabaseAlreadyInitialized`]; [`Error::OperationInProgress`]; any error of the
    /// install run (nothing persists).
    pub async fn initialize(&self, options: InitOptions) -> Result<OperationOutcome> {
        // A small caller-facing future even when the transactional body or its
        // post-commit phase grows. Keep execution and cancellation in this task.
        Box::pin(self.initialize_inner(options)).await
    }

    async fn initialize_inner(&self, options: InitOptions) -> Result<OperationOutcome> {
        let mut optx =
            begin_operation(&self.root, self.parts.timeouts, LockWait::Refuse, true).await?;
        let limits = optx.limits;
        let result = async {
            if store::is_initialized(&mut optx.tx).await? {
                return Err(Error::DatabaseAlreadyInitialized {
                    database: store::database_name(&mut optx.tx).await?,
                });
            }
            // indexes need it, and the schema step refuses them without it). A no-op
            // when it exists already.
            // S12: in the schema the ORM creates its tables (and so its trigram indexes) in, not
            // wherever the session's search_path would put it.
            let schema = current_schema(&mut optx.tx).await?;
            optx.tx
                .batch(&format!(
                    "CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA {}",
                    erp_sql::quote(&schema)
                ))
                .await
                .map_err(|e| extension_error("pg_trgm", e))?;
            erp_orm::core_ddl_run(&mut optx.tx, erp_orm::CorePhase::BeforeModels).await?;
            optx.tx
                .init_models(&[])
                .await
                .map_err(|e| limits.during_schema_step().classify(e.into()))?;
            let deferred =
                erp_orm::core_ddl_run(&mut optx.tx, erp_orm::CorePhase::AfterModels).await?;
            erp_orm::core_ddl_refuse_deferred(&deferred)?;
            // The queue runs with the owner's rights: nobody else may write it. (A new table has
            // no PUBLIC privileges; this keeps that true under default privileges set on the
            // schema.)
            optx.tx
                .batch("REVOKE ALL ON ir_module_post_commit FROM PUBLIC")
                .await?;
            // Columns this
            // statement leaves out take their database default, such as base's fail-closed
            // `share` (`db_default`), or stay NULL.
            optx.tx
                .batch(
                    "INSERT INTO res_users (id, name, login, active) \
                     VALUES (1, 'System', '__system__', false)",
                )
                .await?;
            optx.tx
                .batch("SELECT setval(pg_get_serial_sequence('res_users', 'id'), 1)")
                .await?;
            let (entries, _) = self.discover_admitted().await?;
            let mut catalog = Catalog::new();
            catalog.update_list(entries.values().map(AddonEntry::manifest))?;
            catalog.seed_initial(
                &InstallOptions::new().with_skip_auto_install(options.skip_auto_install),
            )?;
            store::save(&mut optx.tx, &mut catalog).await?;
            let mut run = Run::new(self, entries, options.demo);
            let orm = run.update(&mut optx, &mut catalog).await?;
            self.signal_registry_change(&mut optx.tx, &run).await?;
            run.before_commit(&mut optx.tx).await?;
            optx.commit().await?;
            Ok((orm, run))
        }
        .await;
        self.finish(
            optx,
            result.map_err(Failure::from),
            AfterCommit::RunPostCommit,
        )
        .await
    }

    /// A plain load of the installed set (no update): the registry of every installed module
    /// present in a source, `_register_hook` once per model. Pending operations
    /// with installable manifests force an update run first: it waits for an
    /// operation in progress (up to [`ModuleSystemBuilder::operation_wait_timeout`]), and its ORM
    /// is the load's (one registry build, one `_register_hook` per model).
    ///
    /// A load never runs post-commit steps, so startup never waits for an index build: the steps
    /// waiting (left by an interrupted or skipped phase, failed earlier, or scheduled by the
    /// forced update) are listed in [`LoadOutcome::post_commit_pending`]. Run them with
    /// [`ModuleSystem::run_post_commit`], e.g. in a background task once the server serves.
    ///
    /// # Errors
    /// [`Error::DatabaseNotInitialized`]; [`Error::InconsistentStates`] when an
    /// installed module depends on a module that is not installed (nothing is loaded);
    /// [`Error::OperationWaitTimeout`]; the errors of the forced update; registry errors.
    pub async fn load(&self) -> Result<LoadOutcome> {
        self.load_inner().await
    }

    async fn load_inner(&self) -> Result<LoadOutcome> {
        // Retain immutable source bytes without adding lifecycle receipt matching to plain
        // loads. Native source attestation remains part of discovery admission.
        let (entries, _hashes) = self.discover_admitted().await?;
        let mut tx = self.root.begin(SUPERUSER_ID).await?;
        let prepared = async {
            self.require_initialized(&mut tx).await?;
            let mut catalog = store::load(&mut tx).await?;
            for entry in entries.values() {
                if catalog.get(entry.manifest().name().as_str()).is_some() {
                    catalog.attach_manifest(Arc::clone(entry.manifest()))?;
                }
            }
            Ok::<_, Error>(catalog)
        }
        .await;
        let catalog = match prepared {
            Ok(v) => v,
            Err(e) => return Err(rollback_plain(&mut tx, e).await),
        };
        let forced = catalog
            .rows()
            .any(|r| r.state().is_transient() && r.manifest().is_some_and(|m| m.installable()));
        if forced {
            // Read only: nothing to commit.
            tx.rollback().await?;
            drop(tx);
            // The forced update re-reads the states under the operation lock (another operation
            // may have run them meanwhile) and builds the final registry: it is the load's. Its
            // post-commit steps stay pending, like the others.
            let outcome = self
                .operation(Request::Pending, LockWait::Wait, AfterCommit::LeavePending)
                .await?;
            let post_commit_pending = self.pending_post_commit().await?;
            warn_pending_after_load(&post_commit_pending);
            return Ok(LoadOutcome {
                admitted_addons: outcome.admitted_addons,
                orm: outcome.orm,
                unavailable: outcome.unavailable,
                drops: outcome.dropped,
                forced_update: Some(ForcedUpdate {
                    installed: outcome.installed,
                    upgraded: outcome.upgraded,
                    uninstalled: outcome.uninstalled,
                    data_notices: outcome.data_notices,
                }),
                post_commit_pending,
            });
        }
        let pending = pending_steps(&mut tx).await;
        let post_commit_pending = match pending {
            Ok(v) => v,
            Err(e) => return Err(rollback_plain(&mut tx, e).await),
        };
        // Read only: nothing to commit.
        tx.rollback().await?;
        drop(tx);
        check_consistent(&catalog, LoadMode::Load)?;
        let GraphInput {
            states,
            manifests,
            unavailable,
        } = graph_input(&catalog, |s| {
            matches!(
                s,
                ModuleState::Installed | ModuleState::ToUpgrade | ModuleState::ToRemove
            )
        });
        let (graph, drops) = ModuleGraph::build(&manifests, &states, LoadMode::Load)?;
        let loaded = graph.plain_load_names();
        let admitted_addons = ordered_addon_entries(&loaded, &entries)?;
        let orm = self.orm_for(&loaded, &entries)?;
        let mut optx = OperationTx::plain(&orm).await?;
        let hooked = register_hooks(&mut optx.tx, &optx.guard).await;
        end_transaction(&mut optx, hooked).await?;
        drop(optx);
        warn_pending_after_load(&post_commit_pending);
        Ok(LoadOutcome {
            admitted_addons,
            orm,
            unavailable,
            drops,
            forced_update: None,
            post_commit_pending,
        })
    }

    /// The post-commit steps waiting for a phase (`pending` or `failed`), in scheduling order: the
    /// health check of the post-commit queue. Read in its own read-only transaction.
    ///
    /// # Errors
    /// [`Error::DatabaseNotInitialized`]; database errors; [`Error::InvariantViolation`] for a
    /// row the module system could not have written (a module name or state out of range).
    pub async fn pending_post_commit(&self) -> Result<Vec<PendingPostCommit>> {
        let mut tx = self.root.begin(SUPERUSER_ID).await?;
        let read = async {
            self.require_initialized(&mut tx).await?;
            pending_steps(&mut tx).await
        }
        .await;
        match read {
            Ok(steps) => {
                tx.rollback().await?;
                Ok(steps)
            }
            Err(e) => Err(rollback_plain(&mut tx, e).await),
        }
    }

    async fn operation(
        &self,
        request: Request,
        wait: LockWait,
        after: AfterCommit,
    ) -> Result<OperationOutcome> {
        self.operation_with_authorizer(request, wait, after, None, false)
            .await
    }

    /// One operation of the lifecycle of a package addon ([`crate::wasm_lifecycle`]), run through
    /// the same pipeline as `button_immediate_*`. The caller is checked before anything else:
    /// an operator environment by [`check_access`], a session by the trusted authorizer, inside the
    /// operation transaction under the lifecycle lock. Either way the
    /// core tables are ensured after authority succeeded, and the actor stays on the transaction
    /// for the package runtime's audit row.
    pub(crate) async fn package_operation(
        &self,
        op: PackageOp,
        names: &[ModuleName],
        access: PackageAccess<'_>,
    ) -> Result<OperationOutcome> {
        let request = match op {
            PackageOp::Install => Request::Install(names.to_vec()),
            PackageOp::Upgrade => Request::Upgrade(names.to_vec()),
            PackageOp::Uninstall => Request::Uninstall(names.to_vec()),
        };
        let authorizer = match access {
            PackageAccess::Operator(caller) => {
                check_access(&self.root, caller, op.method_name())?;
                None
            }
            PackageAccess::Session(authorizer) => Some(authorizer),
        };
        self.operation_with_authorizer(
            request,
            LockWait::Refuse,
            AfterCommit::RunPostCommit,
            authorizer,
            true,
        )
        .await
    }

    /// Whether `caller` may run package lifecycle operations as the operator (System, uid 1, this
    /// module system's lineage): the same rule as the native immediate operations, checked early
    /// so a refused caller never reaches package code.
    pub(crate) fn check_operator(&self, caller: &Env, method: &'static str) -> Result<()> {
        check_access(&self.root, caller, method)
    }

    async fn operation_with_authorizer(
        &self,
        request: Request,
        wait: LockWait,
        after: AfterCommit,
        authorizer: Option<&dyn crate::install::InstallAuthorizer>,
        package_lifecycle: bool,
    ) -> Result<OperationOutcome> {
        // All immediate, authorized and package callers share the same heap-owned
        // operation state, including failure cleanup and post-commit processing.
        Box::pin(self.operation_with_authorizer_inner(
            request,
            wait,
            after,
            authorizer,
            package_lifecycle,
        ))
        .await
    }

    async fn operation_with_authorizer_inner(
        &self,
        request: Request,
        wait: LockWait,
        after: AfterCommit,
        authorizer: Option<&dyn crate::install::InstallAuthorizer>,
        package_lifecycle: bool,
    ) -> Result<OperationOutcome> {
        // Native human install/upgrade uses its own closure admission and audit. A package
        // lifecycle operation with a session shares authority checks and actor binding;
        // its package runtime owns admission and audit.
        let human_native_lifecycle = authorizer.is_some() && !package_lifecycle;
        let entries = self.discover().await?;
        if !package_lifecycle && let Request::Install(names) = &request {
            refuse_wasm_dev_folders(&entries, names)?;
        }
        let mut optx = self.begin_operation(wait).await?;
        let result: std::result::Result<_, Failure> = async {
            self.require_initialized(&mut optx.tx).await?;
            let authorization = if let Some(authorizer) = authorizer {
                let checked = authorizer
                    .authorize(&mut optx.tx)
                    .await
                    .map_err(|e| Failure::Refused(e.into()))?;
                optx.verify_intact().await.map_err(Failure::Refused)?;
                optx.tx
                    .operation_bind_actor(&optx.control, checked.principal.clone())
                    .map_err(|e| Failure::Refused(e.into()))?;
                Some(checked)
            } else {
                None
            };
            // Every lifecycle entry point converges framework-owned tables, including native
            // operator upgrades of databases created by older binaries. Operator authority was
            // checked by the public entry point; session authority was checked above. A forced
            // Pending run consumes previously scheduled work through this same owner pipeline.
            // Keep the additive DDL under both lifecycle and schema locks, in this operation's
            // transaction, so a later admission or lifecycle failure rolls it back as well.
            // Idempotent core DDL preserves existing singleton progress (such as the GC cursor).
            optx.tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await?;
            erp_orm::core_ddl_run(&mut optx.tx, erp_orm::CorePhase::BeforeModels).await?;
            // An entry deferred here is completed by the schema step that creates its model
            // table; one still deferred before the commit is refused below.
            erp_orm::core_ddl_run(&mut optx.tx, erp_orm::CorePhase::AfterModels).await?;
            // Staging changes discovery, not the live package. Validate its immutable old
            // preimage against durable storage before consuming candidate content or effects.
            let package_identity_upgrade = if package_lifecycle {
                crate::wasm_lifecycle::identity_upgrade()?
            } else {
                None
            };
            if let Some(transition) = &package_identity_upgrade {
                installed_identity::validate_package_upgrade(&mut optx.tx, transition).await?;
            }
            let (entries, _) = self.discover_admitted().await?;
            let (mut catalog, _) = self.load_catalog(&mut optx.tx, &entries).await?;
            // A browser installation must not execute unrelated work left scheduled by CLI.
            if authorization.is_some() && catalog.rows().any(|row| row.state().is_transient()) {
                return Err(Failure::Refused(
                    erp_fields::Error::validation(match &request {
                        Request::Install(_) => "Pending module operations must be resolved before human installation",
                        _ => "Pending module operations must be resolved before human lifecycle operation",
                    })
                    .into(),
                ));
            }
            let force_demo_requested = vec![ModuleName::new("base")?];
            if human_native_lifecycle && let Request::Upgrade(names) = &request {
                for name in names {
                    if catalog.state(name.as_str()) != Some(ModuleState::Installed) {
                        return Err(Failure::Refused(erp_fields::Error::validation(
                            format!("Upgrade requires installed addon '{name}'"),
                        ).into()));
                    }
                }
            }
            // The companies' countries arrive with res.company (M3): unknown until then, so the
            // auto-install closure refuses country-restricted modules instead of skipping them
            // (`Error::CompanyCountriesUnknown`).
            let options = InstallOptions::new();
            let scheduled = match &request {
                Request::Install(names) => catalog.button_install(names, &options).map(drop),
                Request::Upgrade(names)
                | Request::UpgradeRestoring(names)
                | Request::Reinit(names) => catalog.button_upgrade(names, &options).map(drop),
                Request::ForceDemo => catalog.force_demo().map(drop),
                Request::Uninstall(names) => catalog.button_uninstall(names).map(drop),
                Request::Pending => Ok(()),
            };
            scheduled.map_err(Failure::Refused)?;
            if human_native_lifecycle {
                for row in catalog.rows() {
                    // Only scheduled installs require install closure admission here.
                    if row.state() == ModuleState::ToInstall {
                        self.require_human_install(row.name(), &entries).map_err(Failure::Refused)?;
                    }
                }
            }
            store::save(&mut optx.tx, &mut catalog).await?;
            let mut run = Run::new(self, entries, false);
            run.package_identity_upgrade = package_identity_upgrade;
            run.reinit = matches!(request, Request::Reinit(_) | Request::ForceDemo);
            run.demo_only = matches!(request, Request::ForceDemo);
            run.restore_generated = matches!(request, Request::UpgradeRestoring(_));
            let orm = run.update(&mut optx, &mut catalog).await?;
            self.signal_registry_change(&mut optx.tx, &run).await?;
            run.before_commit(&mut optx.tx).await?;
            let deferred = erp_orm::core_ddl_pending(&mut optx.tx).await?;
            erp_orm::core_ddl_refuse_deferred(&deferred)?;
            if let (true, Some(checked)) = (human_native_lifecycle, authorization) {
                let (operation, requested) = match &request {
                    Request::Install(requested) => {
                        crate::install::record_audit(&mut optx.tx, &checked, requested, &run.installed).await?;
                        ("install", requested)
                    }
                    Request::Upgrade(requested) => ("upgrade", requested),
                    Request::ForceDemo => ("force_demo", &force_demo_requested),
                    _ => return Err(Failure::Refused(erp_fields::Error::programming(
                        "Unsupported authorized lifecycle operation",
                    ).into())),
                };
                crate::install::record_lifecycle_audit(
                    &mut optx.tx, &checked, operation, requested, &run.installed, &run.upgraded,
                ).await?;
            }
            optx.commit().await?;
            Ok((orm, run))
        }
        .await;
        self.finish(optx, result, after).await
    }

    /// Bump the registry generation in the operation's transaction when it installed, upgraded
    /// or uninstalled a module: the other workers rebuild their registry once it
    /// commits, and a rollback leaves the counter alone. The claim is [`SelfApplied::No`]: this
    /// process keeps serving its current registry until the caller swaps in the ORM of the
    /// outcome, so it must still refresh when the generation moves (a schema change inside the
    /// run claimed `Yes` for itself; the stronger `No` wins). No signal configured, nothing to
    /// bump.
    async fn signal_registry_change(&self, tx: &mut Transaction, run: &Run<'_>) -> Result<()> {
        if self.root.signal().is_none()
            || (run.installed.is_empty() && run.upgraded.is_empty() && run.uninstalled.is_empty())
        {
            return Ok(());
        }
        tx.signal_bump(&SignalKey::Registry, SelfApplied::No)
            .await?;
        Ok(())
    }

    /// After an operation: on success, the post-commit phase (unless `after` leaves it); on
    /// known failure, roll back, reset transient states in a separate transaction
    /// and return the error. An unknown commit bypasses recovery and requires
    /// durable-state reconciliation, with no post-commit execution or automatic replay.
    ///
    /// The failure path runs in a task of its own and is awaited: a caller that drops the
    /// operation's future (a request deadline, a canceled task) cannot stop it between the
    /// rollback and the reset and leave modules `to install`, `to upgrade` or `to remove`.
    async fn finish(
        &self,
        optx: OperationTx,
        result: std::result::Result<(Orm, Run<'_>), Failure>,
        after: AfterCommit,
    ) -> Result<OperationOutcome> {
        match result {
            Ok((orm, run)) => {
                // The committed transaction's connection goes back to the pool first: the phase
                // takes a connection of its own and may run for hours (with a pool of one
                // connection, holding this one would leave the phase without any).
                drop(optx);
                // The operation is committed: a post-commit problem is reported, not raised.
                let post_commit = match after {
                    AfterCommit::RunPostCommit => self.run_post_commit(&orm).await,
                    AfterCommit::LeavePending => PostCommitReport::default(),
                };
                Ok(OperationOutcome {
                    admitted_addons: run.active,
                    orm,
                    action: ClientAction::reload_home(),
                    installed: run.installed,
                    upgraded: run.upgraded,
                    uninstalled: run.uninstalled,
                    unavailable: run.unavailable,
                    dropped: run.dropped,
                    post_commit,
                    data_notices: run.notices,
                    restored_restrictions: run.restored,
                })
            }
            Err(failure) => {
                let (error, reset_states) = match failure {
                    Failure::Refused(error) => (error, false),
                    Failure::Failed(error) => (error, true),
                };
                // The database may have committed the operation and its audit. In particular,
                // never reset another request's transient states or run post-commit work here.
                // The classified commit path has discarded the unusable connection already.
                if matches!(error, Error::CommitOutcomeUnknown { .. }) {
                    return Err(error);
                }
                let summary = error.to_string();
                let root = self.root.clone();
                let timeouts = self.parts.timeouts;
                let task = tokio::spawn(fail_operation(root, timeouts, optx, error, reset_states));
                match task.await {
                    Ok(error) => Err(error),
                    Err(join) => Err(Error::InvariantViolation {
                        message: format!(
                            "the task that rolls back a failed module operation and resets the \
                             transient module states ended abnormally ({join}); the module states \
                             may still be to install, to upgrade or to remove. The operation had \
                             failed with: {summary}"
                        ),
                    }),
                }
            }
        }
    }

    /// Run the pending and failed post-commit steps, each in its own autocommit
    /// statement, under [`POST_COMMIT_LOCK`] and the post-commit timeouts. Immediate operations
    /// call it after they commit; after [`ModuleSystem::load`], call it yourself (a load leaves
    /// the steps pending, [`LoadOutcome::post_commit_pending`]).
    ///
    /// **`orm` must be the ORM of the current module set** ([`OperationOutcome::orm`],
    /// [`LoadOutcome::orm`]). The schema steps of large tables (Amendment 2) are planned
    /// again from its registry and the catalog and computed with it; with an ORM from before the
    /// operation, a step on a table it has no model for fails (`Programming`, named in the
    /// report), and one on a column it does not know would find nothing to do. The other steps
    /// do not use it.
    ///
    /// It can run for as long as the steps take (up to
    /// [`ModuleSystemBuilder::post_commit_statement_timeout`] each, which is also how long a step
    /// waits for a lock; the bookkeeping statements between steps wait up to
    /// [`ModuleSystemBuilder::lock_timeout`]). A step may run more than once (a failed step runs
    /// again in the next phase, an interrupted one is pending again): steps must be idempotent
    /// ([`schedule_post_commit`]).
    ///
    /// The phase uses a connection of its own and closes it at the end, whatever happened: a
    /// step's session settings (`SET ROLE`, `search_path`, an open `BEGIN`) never reach a pooled
    /// connection.
    ///
    /// Dropping the future stops the phase but not the statement that was running: the
    /// connection is closed, and the server ends the session when that statement returns, so a
    /// long `CREATE INDEX CONCURRENTLY` keeps running and keeps [`POST_COMMIT_LOCK`] until it
    /// ends (then the step stays pending, or invalid-index cleanup runs on its next attempt). A
    /// phase started meanwhile finds the lock held and returns [`PostCommitStop::Busy`] with the
    /// holder's backend pid (also logged), which `pg_stat_activity` identifies and
    /// `pg_cancel_backend` can cancel. When the dropped future is an operation's, the
    /// [`OperationOutcome`] is lost although the operation committed: read the module states and
    /// [`ModuleSystem::pending_post_commit`] instead, and use the ORM from
    /// [`ModuleSystem::load`].
    ///
    /// Never fails: what went wrong is in the report, with the steps that ran before it, and
    /// logged (`tracing`) for operators. [`PostCommitStop::Busy`] means another phase holds the
    /// lock and runs the steps, including those committed while it runs: a phase reads the queue
    /// again after each pass and once more after it releases the lock, until no step is left that
    /// it has not run (each step runs once per scheduling).
    ///
    /// ```ignore
    /// let loaded = system.load().await?;
    /// if !loaded.post_commit_pending.is_empty() {
    ///     let system = Arc::clone(&system);
    ///     tokio::spawn(async move {
    ///         let report = system.run_post_commit(&loaded.orm).await;
    ///         // report.failed / report.stopped say what did not run
    ///     });
    /// }
    /// ```
    pub async fn run_post_commit(&self, orm: &Orm) -> PostCommitReport {
        let report = self.post_commit_phase(orm).await;
        // Diagnostics only (the report is the channel): an absent SQLSTATE logs as "".
        for failure in &report.failed {
            tracing::error!(
                module = %failure.step.module,
                key = %failure.step.key,
                sqlstate = failure.sqlstate.as_deref().unwrap_or(""),
                parked = failure.parked,
                "post-commit step failed: {}",
                failure.error
            );
        }
        match &report.stopped {
            None => {}
            Some(PostCommitStop::Busy { holder_pid }) => {
                tracing::warn!(
                    holder_pid = holder_pid.unwrap_or(0),
                    "post-commit phase skipped: another phase holds the lock (holder_pid 0: the \
                     holder ended meanwhile)"
                );
            }
            Some(PostCommitStop::Failed { message, sqlstate }) => tracing::error!(
                sqlstate = sqlstate.as_deref().unwrap_or(""),
                "post-commit phase stopped: {message}"
            ),
            Some(PostCommitStop::PassLimit { passes }) => tracing::warn!(
                passes,
                "post-commit phase stopped after {passes} passes: steps are still being \
                 scheduled; the rest stay pending"
            ),
        }
        report
    }

    async fn post_commit_phase(&self, orm: &Orm) -> PostCommitReport {
        let mut report = PostCommitReport::default();
        // the schema steps are planned and computed with this ORM: it must be this module
        // system's own (the same rule as for the environments of `check_access`)
        if !orm.same_lineage(&self.root) {
            report.stopped = Some(PostCommitStop::Failed {
                message: "run_post_commit: the ORM is not of this module system's lineage (it \
                          belongs to another module system or another database); pass \
                          OperationOutcome::orm or LoadOutcome::orm"
                    .to_owned(),
                sqlstate: None,
            });
            return report;
        }
        let client = match self.parts.pool.get().await {
            Ok(client) => client,
            Err(e) => {
                report.stopped = Some(PostCommitStop::Failed {
                    message: format!("could not get a connection: {e}"),
                    sqlstate: None,
                });
                return report;
            }
        };
        // Detached from the pool: dropped when this function ends (however it ends, also when
        // the future is dropped), it closes the connection instead of going back to the pool.
        let client = deadpool_postgres::Object::take(client);
        let client = &client;
        let timeouts = self.parts.timeouts;
        let schema = match phase_schema(client).await {
            Ok(schema) => schema,
            Err(stop) => {
                report.stopped = Some(stop);
                return report;
            }
        };
        let limits = PhaseLimits {
            bookkeeping: timeouts.lock,
            step: timeouts.post_commit_statement,
            schema,
        };
        // Session settings: each step is its own autocommit statement. The session ends with
        // the phase. Statements between steps wait up to the short `lock_timeout`; a step
        // waits for its locks as long as its statement may run (see `run_stored_step`).
        let statement = format!("{}ms", millis(limits.step));
        let set = client
            .query_one(
                "SELECT set_config('statement_timeout', $1, false)",
                &[&statement],
            )
            .await;
        if let Err(e) = set {
            report.stopped = Some(post_commit_stop("setting the post-commit timeouts", &e));
            return report;
        }
        if let Err(stop) = set_session_lock_timeout(client, limits.bookkeeping).await {
            report.stopped = Some(stop);
            return report;
        }
        let runner = SchemaRunner {
            orm,
            options: &self.parts.schema_sync,
            pool_size: self.parts.pool.status().max_size,
        };
        // Steps run by this phase, by (id, revision): each runs at most once per scheduling.
        let mut attempted: HashSet<(i64, i32)> = HashSet::new();
        let mut passes = 0_usize;
        loop {
            let locked = client
                .query_one(
                    "SELECT pg_try_advisory_lock($1, $2)",
                    &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
                )
                .await
                .and_then(|row| row.try_get::<_, bool>(0));
            match locked {
                Ok(true) => {}
                Ok(false) => {
                    report.stopped = Some(PostCommitStop::Busy {
                        holder_pid: post_commit_lock_holder(client).await,
                    });
                    break;
                }
                Err(e) => {
                    report.stopped = Some(post_commit_stop("taking the post-commit lock", &e));
                    break;
                }
            }
            let ran = run_post_commit_steps(
                client,
                &limits,
                &runner,
                &mut attempted,
                &mut passes,
                &mut report,
            )
            .await;
            let unlocked = release_post_commit_lock(client).await;
            match (ran, unlocked) {
                (Ok(()), Ok(())) => {}
                (Ok(()), Err(stop)) | (Err(stop), Ok(())) => {
                    report.stopped = Some(stop);
                    break;
                }
                (Err(stop), Err(unlock)) => {
                    // The phase failed and the lock could not be released either: both are
                    // reported (the first is the primary cause).
                    tracing::error!(
                        "releasing the post-commit lock failed after the phase stopped: {}",
                        describe_stop(&unlock)
                    );
                    report.stopped = Some(stop_with_also(stop, &unlock));
                    break;
                }
            }
            // An operation committing after the last read of the queue but before the release
            // found the lock held (`Busy`) and left its steps to this phase: read the queue once
            // more now that the lock is free. Steps committed after this read are seen by their
            // own operation's phase, which then gets the lock (or by the phase holding it, which
            // reads again after its own release).
            match unattempted_steps(client, &attempted).await {
                Ok(steps) if steps.is_empty() => break,
                Ok(_) => {}
                Err(stop) => {
                    report.stopped = Some(stop);
                    break;
                }
            }
        }
        report
    }
}

/// What a phase needs to run the schema steps: the ORM of the current module set and the options
/// the steps were planned with.
struct SchemaRunner<'a> {
    orm: &'a Orm,
    options: &'a SchemaSyncOptions,
    /// The most connections the pool holds. The phase holds one of its own while a schema step
    /// plans and computes with others, so a pool of one would wait for itself.
    pool_size: usize,
}

/// The waits and the schema of a post-commit phase.
#[derive(Debug, Clone)]
struct PhaseLimits {
    /// `current_schema()` of the phase's session, read once: the schema the ORM creates its
    /// tables in, so the one the statements, the index lookups and the invalid-index guard of
    /// every step name.
    schema: String,
    /// `lock_timeout` of the statements between steps (reading and recording the queue).
    bookkeeping: Duration,
    /// `lock_timeout` while a step's statement runs: as long as the statement may run
    /// (`statement_timeout`), so a build waiting for old transactions to finish is not cut off by
    /// the short wait.
    step: Duration,
}

/// The session's `current_schema()`, read once per phase.
async fn phase_schema(
    client: &deadpool_postgres::ClientWrapper,
) -> std::result::Result<String, PostCommitStop> {
    let row = client
        .query_one("SELECT current_schema()::text", &[])
        .await
        .map_err(|e| post_commit_stop("reading the current schema", &e))?;
    let schema: Option<String> = row.try_get(0).map_err(|e| PostCommitStop::Failed {
        message: format!("unexpected current_schema() result: {e}"),
        sqlstate: None,
    })?;
    schema.ok_or_else(|| PostCommitStop::Failed {
        message: "the post-commit session has no current schema (empty search_path)".to_owned(),
        sqlstate: None,
    })
}

/// A stop in words, for a message that combines two.
fn describe_stop(stop: &PostCommitStop) -> String {
    match stop {
        PostCommitStop::Busy { holder_pid } => match holder_pid {
            Some(pid) => format!("another phase holds the post-commit lock (backend {pid})"),
            None => "another phase holds the post-commit lock".to_owned(),
        },
        PostCommitStop::Failed { message, .. } => message.clone(),
        PostCommitStop::PassLimit { passes } => {
            format!("the phase stopped after {passes} passes with steps left")
        }
    }
}

/// The stop of a phase that hit a second failure (releasing the lock, recording a result):
/// `stop`, with the other in its message (a `PassLimit` or `Busy` stop becomes a `Failed` one
/// saying both).
fn stop_with_also(stop: PostCommitStop, also: &PostCommitStop) -> PostCommitStop {
    let (message, sqlstate) = match stop {
        PostCommitStop::Failed { message, sqlstate } => (message, sqlstate),
        other => (describe_stop(&other), None),
    };
    PostCommitStop::Failed {
        message: format!("{message}; and {}", describe_stop(also)),
        sqlstate,
    }
}

/// Set the session's `lock_timeout`.
async fn set_session_lock_timeout(
    client: &deadpool_postgres::ClientWrapper,
    limit: Duration,
) -> std::result::Result<(), PostCommitStop> {
    let lock = format!("{}ms", millis(limit));
    client
        .query_one("SELECT set_config('lock_timeout', $1, false)", &[&lock])
        .await
        .map(drop)
        .map_err(|e| post_commit_stop("setting the post-commit lock timeout", &e))
}

/// The backend holding [`POST_COMMIT_LOCK`], `None` when it ended (or the lookup failed: the
/// holder is diagnostics only).
async fn post_commit_lock_holder(client: &deadpool_postgres::ClientWrapper) -> Option<i32> {
    let row = client
        .query_opt(
            "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
             AND classid::int8 = $1 AND objid::int8 = $2 AND objsubid = 2 \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
             LIMIT 1",
            &[
                &i64::from(POST_COMMIT_LOCK.0),
                &i64::from(POST_COMMIT_LOCK.1),
            ],
        )
        .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("looking up the holder of the post-commit lock failed: {e}");
            return None;
        }
    };
    match row.try_get::<_, i32>(0) {
        Ok(pid) => Some(pid),
        Err(e) => {
            tracing::warn!("unexpected pid column in pg_locks: {e}");
            None
        }
    }
}

/// A load never runs post-commit steps: say so when it leaves some (the caller runs them with
/// [`ModuleSystem::run_post_commit`]; [`LoadOutcome::post_commit_pending`] lists them).
fn warn_pending_after_load(pending: &[PendingPostCommit]) {
    match pending.len() {
        0 => {}
        1 => tracing::warn!(
            "the load left 1 post-commit step pending; run ModuleSystem::run_post_commit"
        ),
        n => tracing::warn!(
            "the load left {n} post-commit steps pending; run ModuleSystem::run_post_commit"
        ),
    }
}

/// The states of an `ir_module_post_commit` row (the selection of `ir.module.post_commit.state`,
/// [`base_models::POST_COMMIT_STATES`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostCommitState {
    Pending,
    Done,
    Failed,
}

impl PostCommitState {
    /// Every state, in the order of the selection.
    const ALL: [Self; 3] = [Self::Pending, Self::Done, Self::Failed];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// The states of a step that has not run successfully yet.
const OPEN_STATES: &[&str] = &[
    PostCommitState::Pending.as_str(),
    PostCommitState::Failed.as_str(),
];

/// A row of `ir_module_post_commit` read for listing or running.
struct StoredStep {
    /// The step, when the module name and the key are valid.
    step: Option<PostCommitStep>,
    action: Option<PostCommitAction>,
    /// What the checks of scheduling refuse in the row; empty for a valid row.
    problems: Vec<String>,
}

impl StoredStep {
    /// The step and action of a valid row; the problems joined otherwise.
    fn into_valid(self) -> std::result::Result<(PostCommitStep, PostCommitAction), String> {
        match (self.step, self.action) {
            (Some(step), Some(action)) if self.problems.is_empty() => Ok((step, action)),
            _ => Err(self.problems.join("; ")),
        }
    }
}

/// The action columns of a queue row, as stored.
struct StoredColumns<'a> {
    kind: &'a str,
    table: &'a str,
    index: Option<&'a str>,
    spec: Option<&'a str>,
    /// The work a stored cursor belongs to.
    cursor_fingerprint: Option<&'a str>,
}

/// Run the checks of scheduling on a stored row: module name, key and the action (kind, table,
/// index, strictly parsed spec).
fn read_stored_step(module: &str, key: &str, columns: &StoredColumns<'_>) -> StoredStep {
    let mut problems: Vec<String> = Vec::new();
    let module = match ModuleName::new(module) {
        Ok(module) => Some(module),
        Err(e) => {
            problems.push(format!("module {module:?}: {e}"));
            None
        }
    };
    let key = match PostCommitKey::parse(key) {
        Ok(key) => Some(key),
        Err(e) => {
            problems.push(e.message().to_owned());
            None
        }
    };
    let action = match PostCommitAction::from_stored(
        columns.kind,
        columns.table,
        columns.index,
        columns.spec,
    ) {
        Ok(action) => Some(action),
        Err(reason) => {
            problems.push(format!("invalid post-commit action: {reason}"));
            None
        }
    };
    if let Some(fingerprint) = columns.cursor_fingerprint
        && let Err(e) = StepFingerprint::parse(fingerprint)
    {
        problems.push(format!("invalid cursor fingerprint: {}", e.message()));
    }
    StoredStep {
        step: module
            .zip(key)
            .map(|(module, key)| PostCommitStep { module, key }),
        action,
        problems,
    }
}

/// The steps waiting for a phase, in scheduling order. A row the module system could not have
/// written is listed as [`PendingState::Invalid`] (a load reports it and goes on); only a row of
/// the wrong shape (a missing column) is an error.
async fn pending_steps(tx: &mut Transaction) -> Result<Vec<PendingPostCommit>> {
    let rows = tx
        .query(
            "SELECT id, module, key, kind, table_name, index_name, spec, state, error, sqlstate, \
             attempts, cursor_fingerprint FROM ir_module_post_commit WHERE state = ANY($1) \
             ORDER BY id",
            &[&OPEN_STATES],
        )
        .await
        .map_err(|e| {
            if e.sqlstate() == Some("42703") {
                Error::InvariantViolation {
                    message: format!("{OLDER_BASE}: {}", e.message()),
                }
            } else {
                e.into()
            }
        })?;
    let mut steps = Vec::with_capacity(rows.len());
    for row in &rows {
        let shape = |e: tokio_postgres::Error| Error::InvariantViolation {
            message: format!("unexpected ir_module_post_commit row: {e}"),
        };
        let id: i64 = row.try_get(0).map_err(shape)?;
        let module: String = row.try_get(1).map_err(shape)?;
        let key: String = row.try_get(2).map_err(shape)?;
        let kind: String = row.try_get(3).map_err(shape)?;
        let table: String = row.try_get(4).map_err(shape)?;
        let index: Option<String> = row.try_get(5).map_err(shape)?;
        let spec: Option<String> = row.try_get(6).map_err(shape)?;
        let state: String = row.try_get(7).map_err(shape)?;
        let error: Option<String> = row.try_get(8).map_err(shape)?;
        let sqlstate: Option<String> = row.try_get(9).map_err(shape)?;
        let attempts: i32 = row.try_get(10).map_err(shape)?;
        let cursor_fingerprint: Option<String> = row.try_get(11).map_err(shape)?;
        let stored = read_stored_step(
            &module,
            &key,
            &StoredColumns {
                kind: &kind,
                table: &table,
                index: index.as_deref(),
                spec: spec.as_deref(),
                cursor_fingerprint: cursor_fingerprint.as_deref(),
            },
        );
        let invalid = |reason: String| {
            PendingState::Invalid(InvalidPostCommitRow {
                id,
                module: module.clone(),
                key: key.clone(),
                reason,
            })
        };
        let state = match stored.into_valid() {
            Err(reason) => invalid(reason),
            Ok((step, _)) if state == PostCommitState::Pending.as_str() => {
                PendingState::Pending { step }
            }
            Ok((step, _)) if state == PostCommitState::Failed.as_str() => match error {
                Some(error) if attempts >= MAX_POST_COMMIT_ATTEMPTS => PendingState::Parked {
                    step,
                    error,
                    sqlstate,
                },
                Some(error) => PendingState::Failed {
                    step,
                    error,
                    sqlstate,
                },
                None => invalid("the step is failed but no error was recorded".to_owned()),
            },
            Ok(_) => invalid(format!(
                "the step has state '{state}', not pending or failed"
            )),
        };
        steps.push(PendingPostCommit {
            id,
            state,
            attempts: i64::from(attempts),
        });
    }
    Ok(steps)
}

/// What an operation or a phase says when the queue table lacks a column or a check the schema
/// steps need: it was created by a `base` older than Amendment 2.
const OLDER_BASE: &str = "ir_module_post_commit is from an older base: upgrade module base first";

fn post_commit_stop(what: &str, e: &tokio_postgres::Error) -> PostCommitStop {
    let (message, sqlstate) = db_message(e);
    // 42703: a column the queue code names does not exist
    let message = if sqlstate.as_deref() == Some("42703") {
        format!("{OLDER_BASE}: {message}")
    } else {
        message
    };
    PostCommitStop::Failed {
        message: format!("{what}: {message}"),
        sqlstate,
    }
}

/// The database's message and SQLSTATE of an error.
fn db_message(e: &tokio_postgres::Error) -> (String, Option<String>) {
    match e.as_db_error() {
        Some(db) => (db.message().to_owned(), Some(db.code().code().to_owned())),
        None => (e.to_string(), e.code().map(|c| c.code().to_owned())),
    }
}

/// Why one post-commit step failed.
struct StepFailure {
    message: String,
    sqlstate: Option<String>,
}

impl From<tokio_postgres::Error> for StepFailure {
    fn from(e: tokio_postgres::Error) -> Self {
        let (message, sqlstate) = db_message(&e);
        Self { message, sqlstate }
    }
}

/// Release [`POST_COMMIT_LOCK`] held by the phase's session.
async fn release_post_commit_lock(
    client: &deadpool_postgres::ClientWrapper,
) -> std::result::Result<(), PostCommitStop> {
    let released = client
        .query_one(
            "SELECT pg_advisory_unlock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .and_then(|row| row.try_get::<_, bool>(0))
        .map_err(|e| post_commit_stop("releasing the post-commit lock", &e))?;
    if released {
        Ok(())
    } else {
        Err(PostCommitStop::Failed {
            message: "releasing the post-commit lock: the session did not hold it".to_owned(),
            sqlstate: None,
        })
    }
}

/// A step row read for a run, as stored (checked before it runs).
struct StepRow {
    id: i64,
    revision: i32,
    module: String,
    key: String,
    kind: String,
    table: String,
    index: Option<String>,
    spec: Option<String>,
    /// Where the last committed batch of a batched schema step ended.
    cursor: Option<i64>,
    /// The work that cursor belongs to ([`erp_orm::SchemaStep::fingerprint`]).
    cursor_fingerprint: Option<String>,
}

/// The pending steps and the failed ones that are not parked yet
/// ([`MAX_POST_COMMIT_ATTEMPTS`]) this phase has not run yet (a step scheduled again has a new
/// revision, so it runs again), in scheduling order.
async fn unattempted_steps(
    client: &deadpool_postgres::ClientWrapper,
    attempted: &HashSet<(i64, i32)>,
) -> std::result::Result<Vec<StepRow>, PostCommitStop> {
    let rows = client
        .query(
            "SELECT id, revision, module, key, kind, table_name, index_name, spec, \"cursor\", cursor_fingerprint \
             FROM ir_module_post_commit \
             WHERE state = $1 OR (state = $2 AND attempts < $3) ORDER BY id",
            &[
                &PostCommitState::Pending.as_str(),
                &PostCommitState::Failed.as_str(),
                &MAX_POST_COMMIT_ATTEMPTS,
            ],
        )
        .await
        .map_err(|e| post_commit_stop("reading ir_module_post_commit", &e))?;
    let mut steps = Vec::new();
    for row in rows {
        let read = |e: tokio_postgres::Error| PostCommitStop::Failed {
            message: format!("unexpected ir_module_post_commit row: {e}"),
            sqlstate: None,
        };
        let id: i64 = row.try_get(0).map_err(read)?;
        let revision: i32 = row.try_get(1).map_err(read)?;
        if attempted.contains(&(id, revision)) {
            continue;
        }
        steps.push(StepRow {
            id,
            revision,
            module: row.try_get(2).map_err(read)?,
            key: row.try_get(3).map_err(read)?,
            kind: row.try_get(4).map_err(read)?,
            table: row.try_get(5).map_err(read)?,
            index: row.try_get(6).map_err(read)?,
            spec: row.try_get(7).map_err(read)?,
            cursor: row.try_get(8).map_err(read)?,
            cursor_fingerprint: row.try_get(9).map_err(read)?,
        });
    }
    Ok(steps)
}

/// Count a pass over the queue that found steps to run; the pass after [`MAX_POST_COMMIT_PASSES`]
/// is refused with [`PostCommitStop::PassLimit`].
fn count_pass(passes: &mut usize) -> std::result::Result<(), PostCommitStop> {
    if *passes >= MAX_POST_COMMIT_PASSES {
        return Err(PostCommitStop::PassLimit { passes: *passes });
    }
    *passes += 1;
    Ok(())
}

/// Run steps in scheduling order until no step is left that this phase has not run: the queue is
/// read again after each pass, so steps committed meanwhile run too. Each step runs at most once
/// per revision (`attempted`).
async fn run_post_commit_steps(
    client: &deadpool_postgres::ClientWrapper,
    limits: &PhaseLimits,
    runner: &SchemaRunner<'_>,
    attempted: &mut HashSet<(i64, i32)>,
    passes: &mut usize,
    report: &mut PostCommitReport,
) -> std::result::Result<(), PostCommitStop> {
    loop {
        let steps = unattempted_steps(client, attempted).await?;
        if steps.is_empty() {
            return Ok(());
        }
        count_pass(passes)?;
        for step in steps {
            attempted.insert((step.id, step.revision));
            run_stored_step(client, limits, runner, step, report).await?;
        }
    }
}

/// How a run's result was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recorded {
    /// On the step's current revision.
    Yes,
    /// Not recorded: the row was changed meanwhile (scheduled again, removed).
    Superseded,
    /// Not recorded: another transaction holds the row locked.
    RowLocked,
}

/// Record a run's result on the step's current revision, without waiting for a row another
/// transaction holds (an operation scheduling the step again or removing it). `count_attempt`
/// also counts a run that never started (an invalid row), so it parks after
/// [`MAX_POST_COMMIT_ATTEMPTS`] like any failing step.
async fn record_step_result(
    client: &deadpool_postgres::ClientWrapper,
    id: i64,
    revision: i32,
    state: PostCommitState,
    failure: Option<&StepFailure>,
    count_attempt: bool,
) -> std::result::Result<Recorded, PostCommitStop> {
    let message = failure.map(|f| f.message.as_str());
    let sqlstate = failure.and_then(|f| f.sqlstate.as_deref());
    let counted = i32::from(count_attempt);
    let recorded = client
        .execute(
            "UPDATE ir_module_post_commit p SET state = $2, error = $3, sqlstate = $4, \
             attempts = p.attempts + $7 \
             FROM (SELECT id FROM ir_module_post_commit \
                   WHERE id = $1 AND revision = $5 AND state = ANY($6) \
                   FOR UPDATE SKIP LOCKED) t \
             WHERE p.id = t.id",
            &[
                &id,
                &state.as_str(),
                &message,
                &sqlstate,
                &revision,
                &OPEN_STATES,
                &counted,
            ],
        )
        .await
        .map_err(|e| post_commit_stop(&format!("recording step {id}"), &e))?;
    if recorded > 0 {
        return Ok(Recorded::Yes);
    }
    unrecorded_reason(client, id, revision).await
}

/// Why an update of the step's row by (id, revision) changed nothing: the row is there, in the
/// same revision and open (`SKIP LOCKED` skipped it: another transaction holds it), or it was
/// changed or removed.
async fn unrecorded_reason(
    client: &deadpool_postgres::ClientWrapper,
    id: i64,
    revision: i32,
) -> std::result::Result<Recorded, PostCommitStop> {
    let row = client
        .query_opt(
            "SELECT true FROM ir_module_post_commit \
             WHERE id = $1 AND revision = $2 AND state = ANY($3)",
            &[&id, &revision, &OPEN_STATES],
        )
        .await
        .map_err(|e| post_commit_stop(&format!("looking up step {id}"), &e))?;
    Ok(if row.is_some() {
        Recorded::RowLocked
    } else {
        Recorded::Superseded
    })
}

/// Put a step's outcome in the report.
fn push_outcome(
    report: &mut PostCommitReport,
    step: PostCommitStep,
    outcome: std::result::Result<Option<SchemaResult>, StepFailure>,
    parked: bool,
) {
    match outcome {
        Ok(schema) => {
            if let Some(schema) = schema {
                report.schema.push(SchemaStepDone {
                    step: step.clone(),
                    report: schema.report,
                    outcome: schema.outcome,
                });
            }
            report.done.push(step);
        }
        Err(failure) => report.failed.push(PostCommitFailure {
            step,
            error: failure.message,
            sqlstate: failure.sqlstate,
            parked,
        }),
    }
}

/// Run one step read from the queue and record its result.
async fn run_stored_step(
    client: &deadpool_postgres::ClientWrapper,
    limits: &PhaseLimits,
    runner: &SchemaRunner<'_>,
    row: StepRow,
    report: &mut PostCommitReport,
) -> std::result::Result<(), PostCommitStop> {
    let StepRow {
        id,
        revision,
        module,
        key,
        kind,
        table,
        index,
        spec,
        cursor,
        cursor_fingerprint,
    } = row;
    // The row may have been changed in the table directly: it must still pass the checks of
    // scheduling, or it is not run (marked failed with the reason, counted as an attempt so it
    // is parked after a few phases, and reported as invalid).
    let stored = read_stored_step(
        &module,
        &key,
        &StoredColumns {
            kind: &kind,
            table: &table,
            index: index.as_deref(),
            spec: spec.as_deref(),
            cursor_fingerprint: cursor_fingerprint.as_deref(),
        },
    );
    let (step, action) = match stored.into_valid() {
        Ok(checked) => checked,
        Err(reason) => {
            let failure = StepFailure {
                message: reason.clone(),
                sqlstate: None,
            };
            record_step_result(
                client,
                id,
                revision,
                PostCommitState::Failed,
                Some(&failure),
                true,
            )
            .await?;
            report.invalid.push(InvalidPostCommitRow {
                id,
                module,
                key,
                reason,
            });
            return Ok(());
        }
    };
    if matches!(action, PostCommitAction::Schema(_)) && runner.pool_size < 2 {
        return Err(PostCommitStop::Failed {
            message: format!(
                "schema steps need a connection pool of at least 2 connections (the phase holds \
                 one while a step plans and computes with others); this pool has {}: raise the \
                 pool size and run the phase again",
                runner.pool_size
            ),
            sqlstate: None,
        });
    }
    // Count the run first, on the revision read: a step scheduled again, removed or locked by the
    // operation doing so meanwhile is not run (the new revision runs in this phase when it is
    // committed).
    let claimed = client
        .query_opt(
            "UPDATE ir_module_post_commit p SET attempts = p.attempts + 1 \
             FROM (SELECT id FROM ir_module_post_commit \
                   WHERE id = $1 AND revision = $2 AND state = ANY($3) \
                   FOR UPDATE SKIP LOCKED) t \
             WHERE p.id = t.id RETURNING p.attempts",
            &[&id, &revision, &OPEN_STATES],
        )
        .await
        .map_err(|e| {
            post_commit_stop(&format!("starting step {}/{}", step.module, step.key), &e)
        })?;
    let Some(claimed) = claimed else {
        match unrecorded_reason(client, id, revision).await? {
            Recorded::RowLocked => report.skipped_row_locked.push(step),
            Recorded::Superseded | Recorded::Yes => report.superseded.push(step),
        }
        return Ok(());
    };
    let attempts: i32 = claimed.try_get(0).map_err(|e| PostCommitStop::Failed {
        message: format!(
            "unexpected attempts of step {}/{}: {e}",
            step.module, step.key
        ),
        sqlstate: None,
    })?;
    set_session_lock_timeout(client, limits.step).await?;
    let resume = StoredRun {
        id,
        revision,
        cursor,
        cursor_fingerprint,
    };
    let outcome =
        run_post_commit_step(client, &limits.schema, &action, runner, &resume, limits).await;
    // The outcome is logged before the bookkeeping timeout is restored and the result recorded:
    // a failing statement after the step must not lose what the step did.
    match &outcome {
        Ok(Some(SchemaResult {
            outcome: SchemaStepOutcome::NothingLeft,
            ..
        })) => tracing::warn!(
            module = %step.module,
            key = %step.key,
            table = %table,
            attempts,
            "schema step found nothing left to do; recording it as done"
        ),
        Ok(_) => tracing::info!(
            module = %step.module,
            key = %step.key,
            attempts,
            "post-commit step ran; recording it"
        ),
        Err(failure) => tracing::warn!(
            module = %step.module,
            key = %step.key,
            attempts,
            sqlstate = failure.sqlstate.as_deref().unwrap_or(""),
            "post-commit step failed: {}; recording it",
            failure.message
        ),
    }
    let restored = set_session_lock_timeout(client, limits.bookkeeping).await;
    let state = match outcome {
        Ok(_) => PostCommitState::Done,
        Err(_) => PostCommitState::Failed,
    };
    let parked = outcome.is_err() && attempts >= MAX_POST_COMMIT_ATTEMPTS;
    // Only the run of the step's current revision records it: a step scheduled again (same
    // statement or not) or removed meanwhile is not recorded; a rescheduled one runs again.
    // Recorded even when the restore failed (the connection may still work), so the outcome is
    // kept whenever it can be.
    let recorded = record_step_result(client, id, revision, state, outcome.as_ref().err(), false)
        .await
        .map_err(|stop| match stop {
            PostCommitStop::Failed { message, sqlstate } => PostCommitStop::Failed {
                message: format!("recording step {}/{}: {message}", step.module, step.key),
                sqlstate,
            },
            other => other,
        });
    // Where the step goes in the report once its result is known to be recorded or not.
    let place = |report: &mut PostCommitReport, recorded: Recorded| match recorded {
        Recorded::Yes => push_outcome(report, step, outcome, parked),
        Recorded::Superseded => {
            tracing::info!(
                module = %step.module,
                key = %step.key,
                "post-commit step scheduled again or removed while it ran: its result was not \
                 recorded"
            );
            report.superseded.push(step);
        }
        Recorded::RowLocked => {
            tracing::info!(
                module = %step.module,
                key = %step.key,
                "post-commit step ran but its row is locked by another transaction: its result \
                 was not recorded"
            );
            report.skipped_row_locked.push(step);
        }
    };
    match (restored, recorded) {
        (Ok(()), Ok(recorded)) => {
            place(report, recorded);
            Ok(())
        }
        // The step's result is in the report whenever it was recorded, also when the restore of
        // the timeout failed.
        (Err(stop), Ok(recorded)) => {
            place(report, recorded);
            Err(stop)
        }
        (Ok(()), Err(stop)) => Err(stop),
        (Err(stop), Err(recording)) => Err(stop_with_also(stop, &recording)),
    }
}

/// What the catalog says about index `index` in the phase's schema (the one the ORM creates its
/// tables in, so the one an index step builds in).
struct IndexState {
    /// `pg_index.indisvalid`.
    valid: bool,
    /// Whether the index is on the step's table (`pg_index.indrelid`).
    on_table: bool,
}

/// Why the state of an index could not be read.
enum IndexLookup {
    /// The query failed.
    Database(tokio_postgres::Error),
    /// The index exists in the schema, but the step's table does not.
    TableMissing { table: String, schema: String },
}

impl From<tokio_postgres::Error> for IndexLookup {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Database(e)
    }
}

impl From<IndexLookup> for StepFailure {
    fn from(e: IndexLookup) -> Self {
        match e {
            IndexLookup::Database(e) => e.into(),
            IndexLookup::TableMissing { table, schema } => Self {
                message: format!("table {table} does not exist in schema {schema}"),
                sqlstate: None,
            },
        }
    }
}

/// The state of index `index` when it exists in `schema`; `on_table` compares its table with
/// `table` of the same schema.
///
/// # Errors
/// [`IndexLookup::TableMissing`] when the index exists but `table` does not in `schema` (the
/// index cannot be on it, and saying so is clearer than "not on table"); database errors.
async fn index_state(
    client: &deadpool_postgres::ClientWrapper,
    schema: &str,
    index: &IndexName,
    table: &TableName,
) -> std::result::Result<Option<IndexState>, IndexLookup> {
    let rows = client
        .query(
            "SELECT i.indisvalid, \
                    i.indrelid = to_regclass(quote_ident($3) || '.' || quote_ident($2)), \
                    to_regclass(quote_ident($3) || '.' || quote_ident($2)) IS NOT NULL \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = $1 AND n.nspname = $3",
            &[&index.as_str(), &table.as_str(), &schema],
        )
        .await?;
    // (schema, name) is unique in pg_class: at most one row.
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let table_exists: bool = row.try_get(2)?;
    if !table_exists {
        return Err(IndexLookup::TableMissing {
            table: table.to_string(),
            schema: schema.to_owned(),
        });
    }
    Ok(Some(IndexState {
        valid: row.try_get(0)?,
        on_table: row.try_get(1)?,
    }))
}

/// One step. An index step first drops the invalid index an interrupted build left (so
/// `CREATE INDEX CONCURRENTLY IF NOT EXISTS` builds it again), but only when that index is on the
/// step's table: an index of that name on another table is not touched and fails the step. The
/// statement is built from the validated action and runs alone (simple protocol, outside any
/// transaction block); then the index must exist, be valid and be on the table (a statement that
/// did not build it, or built it invalid, fails the step). Everything names the phase's `schema`.
async fn run_post_commit_step(
    client: &deadpool_postgres::ClientWrapper,
    schema: &str,
    action: &PostCommitAction,
    runner: &SchemaRunner<'_>,
    stored: &StoredRun,
    limits: &PhaseLimits,
) -> std::result::Result<Option<SchemaResult>, StepFailure> {
    if let PostCommitAction::Schema(step) = action {
        return run_schema_row(client, step, runner, stored, limits)
            .await
            .map(Some);
    }
    let sql = action.sql(schema).ok_or_else(|| StepFailure {
        message: "the step has no statement to run".to_owned(),
        sqlstate: None,
    })?;
    let (Some(index), table) = (action.index(), action.table()) else {
        client.batch_execute(&sql).await?;
        return Ok(None);
    };
    match index_state(client, schema, index, table).await? {
        Some(state) if !state.on_table => {
            return Err(StepFailure {
                message: format!(
                    "index {index} exists in schema {schema} but not on table {table}: it is not \
                     touched"
                ),
                sqlstate: None,
            });
        }
        Some(IndexState { valid: false, .. }) => {
            client
                .batch_execute(&format!(
                    "DROP INDEX CONCURRENTLY IF EXISTS {}.{}",
                    erp_sql::quote(schema),
                    erp_sql::quote(index.as_str())
                ))
                .await?;
        }
        Some(_) | None => {}
    }
    client.batch_execute(&sql).await?;
    match index_state(client, schema, index, table).await? {
        Some(IndexState {
            valid: true,
            on_table: true,
        }) => Ok(None),
        Some(IndexState {
            on_table: false, ..
        }) => Err(StepFailure {
            message: format!("index {index} is not on table {table} after the step"),
            sqlstate: None,
        }),
        Some(_) => Err(StepFailure {
            message: format!("index {index} is invalid after the step"),
            sqlstate: None,
        }),
        None => Err(StepFailure {
            message: format!("the step did not create index {index}"),
            sqlstate: None,
        }),
    }
}

/// The stored identity of a running queue row: its id and revision, and the cursor it had.
struct StoredRun {
    id: i64,
    revision: i32,
    cursor: Option<i64>,
    cursor_fingerprint: Option<String>,
}

impl From<erp_fields::Error> for StepFailure {
    /// The error and its sources, outermost first, with the SQLSTATE of the outermost.
    fn from(e: erp_fields::Error) -> Self {
        let mut message = e.message().to_owned();
        let mut source = std::error::Error::source(&e);
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        Self {
            message,
            sqlstate: e.sqlstate().map(str::to_owned),
        }
    }
}

/// Stores the cursor of a batched schema step, and the fingerprint of the work it belongs to, in
/// its queue row after each committed batch. It writes only the row of the revision that runs. A
/// row another transaction holds locked (an operation scheduling the step again) is waited for
/// only as long as the bookkeeping `lock_timeout`; a wait that gives up leaves the stored cursor
/// behind the work, which is safe (a cursor that lags only repeats a scan), and the step goes on.
/// A row that is gone, scheduled again or closed fails the step: its cursor belongs to other work.
struct CursorStore<'a> {
    client: &'a deadpool_postgres::ClientWrapper,
    id: i64,
    revision: i32,
    fingerprint: StepFingerprint,
    /// `lock_timeout` for the store, and the one the step runs under (restored after it).
    bookkeeping: Duration,
    step: Duration,
    /// A store gave up on a lock: the rest of the run stores nothing (one wait per run, not one
    /// per batch).
    gave_up: bool,
}

impl StepProgress for CursorStore<'_> {
    async fn committed(&mut self, cursor: StepCursor) -> erp_fields::Result<()> {
        let database =
            |message: String| erp_fields::Error::new(erp_fields::ErrorKind::Database, message);
        if self.gave_up {
            return Ok(());
        }
        let stop_message = |stop: PostCommitStop| match stop {
            PostCommitStop::Failed { message, .. } => message,
            other => describe_stop(&other),
        };
        set_session_lock_timeout(self.client, self.bookkeeping)
            .await
            .map_err(|stop| database(stop_message(stop)))?;
        let stored = self
            .client
            .execute(
                "UPDATE ir_module_post_commit p SET \"cursor\" = $3, cursor_fingerprint = $5 \
                 FROM (SELECT id FROM ir_module_post_commit \
                       WHERE id = $1 AND revision = $2 AND state = ANY($4) \
                       FOR NO KEY UPDATE) t \
                 WHERE p.id = t.id",
                &[
                    &self.id,
                    &self.revision,
                    &cursor.get(),
                    &OPEN_STATES,
                    &self.fingerprint.as_str(),
                ],
            )
            .await;
        set_session_lock_timeout(self.client, self.step)
            .await
            .map_err(|stop| database(stop_message(stop)))?;
        match stored {
            Ok(1) => Ok(()),
            Ok(_) => Err(database(format!(
                "queue row {} (revision {}) was scheduled again, removed or closed: cursor {} \
                 was not stored and the step stops",
                self.id,
                self.revision,
                cursor.get()
            ))),
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE) => {
                self.gave_up = true;
                tracing::warn!(
                    row = self.id,
                    cursor = cursor.get(),
                    "the queue row is locked by another transaction: no more cursors are stored \
                     in this run (the stored one stays behind the work) and the step goes on"
                );
                Ok(())
            }
            Err(e) => Err(database(format!(
                "storing cursor {} of queue row {}: {}",
                cursor.get(),
                self.id,
                db_message(&e).0
            ))),
        }
    }
}

/// Run the schema step a queue row refers to. The row is never trusted: the ORM plans the step
/// again from its registry and the catalog. Nothing left to do (the work ran, or an inline sync
/// did it) is success, reported as [`SchemaStepOutcome::NothingLeft`]; work the registry of the
/// given ORM does not declare fails the step. A batched step starts after the row's cursor when
/// the row's fingerprint is the planned step's (a cursor stored for other work, such as a smaller
/// set of backfill fields, is not resumed) and stores the new cursor after each committed batch.
async fn run_schema_row(
    client: &deadpool_postgres::ClientWrapper,
    step: &SchemaStepRef,
    runner: &SchemaRunner<'_>,
    stored: &StoredRun,
    limits: &PhaseLimits,
) -> std::result::Result<SchemaResult, StepFailure> {
    let recorded: Vec<&str> = step.fields().iter().map(ColumnName::as_str).collect();
    let replanned = runner
        .orm
        .plan_schema_step(step.table().as_str(), step.key(), runner.options, &recorded)
        .await?;
    let planned = match replanned {
        Replanned::Pending(planned) => planned,
        Replanned::Done => {
            return Ok(SchemaResult {
                report: SchemaStepReport::default(),
                outcome: SchemaStepOutcome::NothingLeft,
            });
        }
        Replanned::NotDeclared { what } => {
            return Err(StepFailure {
                message: format!(
                    "schema step '{}' is not declared by the registry of the ORM given to the \
                     post-commit phase ({what}): pass the ORM of the current module set \
                     (OperationOutcome::orm, LoadOutcome::orm)",
                    step.key()
                ),
                sqlstate: None,
            });
        }
        // `Replanned` may grow: an answer this module system does not know is not "done"
        other => {
            return Err(StepFailure {
                message: format!(
                    "schema step '{}': unknown re-plan answer {other:?}",
                    step.key()
                ),
                sqlstate: None,
            });
        }
    };
    let fingerprint = planned.fingerprint();
    let stored_fingerprint = stored
        .cursor_fingerprint
        .as_deref()
        .map(StepFingerprint::parse)
        .transpose()?;
    let resume = match (stored.cursor, stored_fingerprint) {
        (None, _) => None,
        (Some(cursor), Some(stored_fingerprint)) if stored_fingerprint == fingerprint => {
            let after = StepCursor::new(cursor).map_err(|e| StepFailure {
                message: format!(
                    "queue row {}: the stored cursor is not a record id: {}",
                    stored.id,
                    e.message()
                ),
                sqlstate: None,
            })?;
            Some(Resume::new(fingerprint.clone(), after))
        }
        (Some(cursor), other) => {
            tracing::warn!(
                row = stored.id,
                cursor,
                stored_fingerprint = other.as_ref().map_or("none", StepFingerprint::as_str),
                planned_fingerprint = %fingerprint,
                "the stored cursor belongs to other work: the step starts at the beginning"
            );
            None
        }
    };
    let mut progress = CursorStore {
        client,
        id: stored.id,
        revision: stored.revision,
        fingerprint,
        bookkeeping: limits.bookkeeping,
        step: limits.step,
        gave_up: false,
    };
    let report = runner
        .orm
        .run_schema_step_from(&planned, runner.options, resume.as_ref(), &mut progress)
        .await?;
    Ok(SchemaResult {
        report,
        outcome: SchemaStepOutcome::Ran,
    })
}

/// The start of the queue key of every schema step.
const SCHEMA_KEY_PREFIX: &str = "schema.";

/// FNV-1a, 64 bits: a hash that never changes between releases (a queue key must stay the same
/// when the step is planned again).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The queue key (within the module) of a schema step: `schema.<step key>`, or, when that is
/// longer than [`MAX_POST_COMMIT_KEY_BYTES`], the first 39 bytes of the step key and the hash of
/// all of it. The row's `spec` keeps the whole step key.
fn schema_queue_key(step: &SchemaStepRef) -> erp_fields::Result<PostCommitKey> {
    let full = format!("{SCHEMA_KEY_PREFIX}{}", step.key());
    if full.len() <= MAX_POST_COMMIT_KEY_BYTES {
        return PostCommitKey::parse(&full);
    }
    // the step key is ASCII (checked by SchemaStepRef) and longer than 57 bytes here
    let head = step.key().get(..39).ok_or_else(|| {
        erp_fields::Error::programming(format!(
            "schema step key {:?} cannot be shortened",
            step.key()
        ))
    })?;
    PostCommitKey::parse(&format!(
        "{SCHEMA_KEY_PREFIX}{head}.{:016x}",
        fnv1a(step.key().as_bytes())
    ))
}

/// Queue the steps a schema update left for after the commit, one row per step, owned by
/// `module`: scheduled again (same module and key) they replace the row, bump its revision and
/// reset its cursor and attempts.
async fn queue_schema_steps(
    tx: &mut Transaction,
    module: &ModuleName,
    sync: SchemaSync,
) -> erp_fields::Result<()> {
    for step in sync.into_steps() {
        let reference = SchemaStepRef::from_step(&step)?;
        let key = schema_queue_key(&reference)?;
        let action = PostCommitAction::Schema(reference);
        insert_post_commit(tx, module, &key, &action).await?;
    }
    Ok(())
}

/// Schedule a post-commit step from inside an operation (a hook, the runner): it runs after the
/// operation commits, in its own autocommit statement. The step is a structured
/// [`PostCommitAction`] (build an index, analyze a table); there is no way to give it SQL text,
/// because the queue is read back and run with the module system's database rights. A step with
/// the same (module, key) is replaced and runs again.
///
/// **A step must be idempotent and may run more than once.** A step that fails stays queued and
/// runs again in the next post-commit phase, after every operation, until it succeeds or has
/// failed [`MAX_POST_COMMIT_ATTEMPTS`] times (the number of runs is `attempts` in
/// [`PendingPostCommit`], the last error in [`PendingState::Failed`]); then it is parked
/// ([`PendingState::Parked`]) and runs again only when an operation schedules it again; a phase that is interrupted may have run a statement whose result
/// was never recorded, so it runs again too. Both actions are: `CREATE INDEX ... IF NOT EXISTS`
/// and `ANALYZE`.
///
/// An index step: when an earlier build was interrupted or failed, PostgreSQL keeps an invalid
/// index that `IF NOT EXISTS` would accept; the runner drops an invalid index of that name
/// before the statement (when it is on the step's table), so a retry builds it again. A
/// concurrent build holds `SHARE UPDATE EXCLUSIVE` on its table while it runs: a module
/// operation altering that table meanwhile waits up to its `lock_timeout` and then fails with
/// [`Error::LockNotAvailable`] (run it again once the build is done). The build itself waits for
/// old transactions as long as the phase's statement timeout allows. Uninstalling a module that
/// drops the table the step names removes the step with it.
///
/// ```ignore
/// let module = ModuleName::new("library")?;
/// let analyze = PostCommitAction::analyze(TableName::parse("library_book")?);
/// let key = PostCommitKey::parse("analyze_books")?;
/// schedule_post_commit(tx, &module, &key, &analyze).await?;
/// ```
///
/// # Errors
/// `Access` unless `tx` acts as the `System` principal with full elevation; `Programming` unless
/// `tx` is the transaction of a module operation (managed by the module system, so the step
/// cannot be scheduled from any other transaction); database errors. A key outside the rule of
/// [`PostCommitKey`] is refused by [`PostCommitKey::parse`].
pub async fn schedule_post_commit(
    tx: &mut Transaction,
    module: &ModuleName,
    key: &PostCommitKey,
    action: &PostCommitAction,
) -> erp_fields::Result<()> {
    require_system(&tx.env(), "schedule_post_commit")?;
    if !tx.is_operation_managed() {
        return Err(erp_fields::Error::programming(
            "schedule_post_commit: post-commit steps are scheduled from the transaction of a \
             module operation (a hook or data loader run by the module system); this transaction \
             is not one",
        ));
    }
    // the module system's own schema update queues these; a hook that could replace one could
    // point it at other work
    if matches!(action, PostCommitAction::Schema(_)) {
        return Err(erp_fields::Error::programming(
            "schedule_post_commit: schema steps are queued by the module system's schema update; \
             a hook cannot schedule one",
        ));
    }
    if key.as_str().starts_with(SCHEMA_KEY_PREFIX) {
        return Err(erp_fields::Error::programming(format!(
            "schedule_post_commit: step keys starting with \"{SCHEMA_KEY_PREFIX}\" are reserved \
             for the schema steps the module system queues; got \"{key}\""
        )));
    }
    insert_post_commit(tx, module, key, action).await
}

/// Longest role name PostgreSQL keeps (bytes).
const MAX_ROLE_BYTES: usize = 63;

/// Take every privilege on the post-commit queue (`ir_module_post_commit`) away from the request
/// role: the queue is read and run with the module system's own rights, so the role that serves
/// requests must not insert, change or read its rows. calls this when it sets up the
/// database roles, next to `Transaction::grant_signal_request_role`. Calling it twice changes
/// nothing. (Initialization already revokes everything from `PUBLIC`.)
///
/// Only the named role is restricted. The table owner is refused (it needs the table, and an
/// owner can grant itself everything back); a role that is a member of the owner and a superuser
/// keep full access whatever this revokes, so the request role must be neither.
///
/// # Errors
/// `Access` unless `tx` acts as the `System` principal with full elevation; `Programming` when
/// `role` is empty, longer than 63 bytes or contains a NUL byte, when it owns the queue, or when
/// the queue is missing (initialize the database first); `Database` when a statement fails
/// (unknown role, or this role may not revoke).
pub async fn revoke_post_commit_access(tx: &mut Transaction, role: &str) -> erp_fields::Result<()> {
    require_system(&tx.env(), "revoke_post_commit_access")?;
    if role.is_empty() || role.len() > MAX_ROLE_BYTES || role.contains('\0') {
        return Err(erp_fields::Error::programming(format!(
            "invalid role name {role:?}: 1 to {MAX_ROLE_BYTES} bytes without NUL"
        )));
    }
    let rows = tx
        .query(
            "SELECT pg_get_userbyid(c.relowner)::text FROM pg_class c \
             WHERE c.oid = to_regclass('ir_module_post_commit')",
            &[],
        )
        .await?;
    let owner: String = match rows.first() {
        Some(row) => row.try_get(0).map_err(|e| {
            erp_fields::Error::programming(format!(
                "unexpected owner of the post-commit queue: {e}"
            ))
        })?,
        None => {
            return Err(erp_fields::Error::programming(
                "the post-commit queue ir_module_post_commit is missing: initialize the database \
                 before revoking the request role",
            ));
        }
    };
    if owner == role {
        return Err(erp_fields::Error::programming(format!(
            "role {role:?} owns ir_module_post_commit: the request role must be another role"
        )));
    }
    tx.batch(&format!(
        "REVOKE ALL ON ir_module_post_commit FROM {}",
        erp_sql::quote(role)
    ))
    .await
}

/// Refuse unless `env` acts as the `System` principal with full elevation (the module system's
/// own principal); `call` names what was refused.
pub(crate) fn require_system(env: &Env, call: &str) -> erp_fields::Result<()> {
    let principal = env.principal();
    if principal.kind() == erp_orm::PrincipalKind::System && principal.elevation().is_full() {
        Ok(())
    } else {
        Err(erp_fields::Error::access(format!(
            "{call}: only the System principal with full elevation may do this (this one acts as \
             {} {})",
            principal.kind().as_str(),
            env.uid()
        )))
    }
}

/// Queue a step. Scheduling it again replaces it and counts a new revision (the `int4` column
/// raises `22003` instead of wrapping around) and starts its attempts over.
async fn insert_post_commit(
    tx: &mut Transaction,
    module: &ModuleName,
    key: &PostCommitKey,
    action: &PostCommitAction,
) -> erp_fields::Result<()> {
    let stored = action.stored();
    // A queue key that two different schema steps map to (their step keys hash alike) would let
    // the second replace the first: the replacement happens only for the same step key.
    let written = tx
        .execute(
            "INSERT INTO ir_module_post_commit \
             (module, key, kind, table_name, index_name, spec, state, revision, attempts) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 1, 0) ON CONFLICT (module, key) DO UPDATE \
             SET kind = EXCLUDED.kind, table_name = EXCLUDED.table_name, \
             index_name = EXCLUDED.index_name, spec = EXCLUDED.spec, \
             state = $7, error = NULL, sqlstate = NULL, attempts = 0, \"cursor\" = NULL, \
             cursor_fingerprint = NULL, \
             revision = ir_module_post_commit.revision + 1 \
             WHERE ir_module_post_commit.kind <> 'schema_step' OR EXCLUDED.kind <> 'schema_step' \
                OR ir_module_post_commit.spec::jsonb ->> 'key' = EXCLUDED.spec::jsonb ->> 'key'",
            &[
                &module.as_str(),
                &key.as_str(),
                &stored.kind,
                &stored.table,
                &stored.index,
                &stored.spec,
                &PostCommitState::Pending.as_str(),
            ],
        )
        .await
        .map_err(|e| {
            // a column or check the new queue code needs is missing: an older `base`
            // 23514 only from the two checks that list the kinds, which an older `base` has
            // without `schema_step`; any other check of the queue table is a current one
            let queue_check = std::error::Error::source(&e)
                .and_then(|s| s.downcast_ref::<tokio_postgres::Error>())
                .and_then(|pg| pg.as_db_error())
                .and_then(|db| db.constraint())
                .is_some_and(|name| {
                    name == "ir_module_post_commit_kind_check"
                        || name == "ir_module_post_commit_kind_columns_check"
                });
            let older = e.sqlstate() == Some("42703")
                || (e.sqlstate() == Some("23514")
                    && queue_check
                    && matches!(action, PostCommitAction::Schema(_)));
            if older {
                erp_fields::Error::new(e.kind(), format!("{OLDER_BASE}: {}", e.message()))
                    .with_sqlstate(e.sqlstate().map(str::to_owned))
                    .with_source(e)
            } else {
                e
            }
        })?;
    if written == 0 {
        return Err(erp_fields::Error::programming(format!(
            "post-commit queue key \"{key}\" of module {module} already holds another schema \
             step: the two step keys map to the same queue key and the first one is kept"
        )));
    }
    Ok(())
}

/// Keep the same immutable entries the registry consumed, never reconstruct source metadata
/// from a later discovery pass. Missing metadata is an invariant failure, not a dropped addon.
fn ordered_addon_entries(
    names: &[ModuleName],
    entries: &IndexMap<ModuleName, AddonEntry>,
) -> Result<Vec<AddonEntry>> {
    names
        .iter()
        .map(|name| {
            entries
                .get(name)
                .cloned()
                .ok_or_else(|| Error::InvariantViolation {
                    message: format!("active module {name} has no admitted source entry"),
                })
        })
        .collect()
}

/// Graph input from the catalog: states and manifests of the rows whose state `keep` accepts;
/// rows without a manifest are unavailable and listed apart.
struct GraphInput {
    states: IndexMap<ModuleName, ModuleState>,
    manifests: IndexMap<ModuleName, Arc<Manifest>>,
    unavailable: Vec<ModuleName>,
}

fn graph_input(catalog: &Catalog, keep: impl Fn(ModuleState) -> bool) -> GraphInput {
    let mut input = GraphInput {
        states: IndexMap::new(),
        manifests: IndexMap::new(),
        unavailable: Vec::new(),
    };
    for row in catalog.rows().filter(|r| keep(r.state())) {
        match row.manifest() {
            Some(m) => {
                input.states.insert(row.name().clone(), row.state());
                input.manifests.insert(row.name().clone(), Arc::clone(m));
            }
            None => input.unavailable.push(row.name().clone()),
        }
    }
    input
}

/// `_register_hook` once on every model of the transaction's registry, in registry order. A
/// failure names the model and the module declaring it ([`ModuleStep::RegisterHook`]).
async fn register_hooks(tx: &mut Transaction, guard: &Guard) -> Result<()> {
    let registry = Arc::clone(tx.registry());
    for model in registry.models() {
        let step = ModuleStep::RegisterHook { model: model.name };
        if let Err(e) = model_hooks::call_register_hook(tx, model.name).await {
            let module = declaring_module(model)?;
            return Err(step_error(&module, step, None)(e.into()));
        }
        // a raw COMMIT in `_register_hook` ends the transaction: stop at the model that did it
        if let Err(e) = verify_intact(tx, guard, Check::Timestamp).await {
            let module = declaring_module(model)?;
            return Err(step_error(&module, step, None)(e));
        }
    }
    Ok(())
}

/// The module whose declaration creates `model` (`_name`, not an extension).
fn declaring_module(model: &erp_registry::ModelSpec) -> Result<ModuleName> {
    let decl = model
        .mro
        .iter()
        .rev()
        .find(|d| d.name == model.name && !d.is_extension())
        .ok_or_else(|| Error::InvariantViolation {
            message: format!("model {} has no declaration creating it", model.name),
        })?;
    ModuleName::new(decl.addon)
}

fn affected_models<'r>(
    registry: &'r Registry,
    module: &ModuleName,
) -> Vec<&'r erp_registry::ModelSpec> {
    registry
        .models()
        .iter()
        .filter(|m| m.mro.iter().any(|d| d.addon == module.as_str()))
        .collect()
}

/// A module still to come in this run (`later`) creates the model's table or owns one of its
/// stored columns or table objects. The registry of an update run holds every module from the
/// start, while the reference syncs at a module's step only what the modules loaded so far
/// declare: syncing now would change that module's
/// table before its pre-migrations ran. The model waits for that module's step, where it is
/// among the affected models again. Declarations without columns (the `web` extension of `base`
/// every model inherits, a non-stored field) do not hold a model back.
fn waits_for_later(model: &erp_registry::ModelSpec, later: &[ModuleName]) -> bool {
    let is_later = |addon: &str| later.iter().any(|m| m.as_str() == addon);
    model
        .fields
        .iter()
        .any(|f| f.store && f.modules.iter().any(|a| is_later(a)))
        || model.mro.iter().any(|d| {
            is_later(d.addon)
                && ((d.name == model.name && !d.is_extension()) || !d.table_objects.is_empty())
        })
}

/// Installed modules (or ones to upgrade or remove) whose dependency is not installed: an
/// inconsistent database, checked against the whole catalog (lead review A; the graph only sees
/// the rows of its states, so it would drop the dependent as "missing dependency" instead). In an
/// update run a dependency `to install` is installed by the run itself; in a plain load it is not.
/// Dependencies without a row, and rows without a source, are reported by the graph.
fn check_consistent(catalog: &Catalog, mode: LoadMode) -> Result<()> {
    let mut inconsistent: Vec<InconsistentDependency> = Vec::new();
    for row in catalog.rows() {
        let dependent = match row.state() {
            ModuleState::Installed | ModuleState::ToUpgrade | ModuleState::ToRemove => true,
            ModuleState::Uninstalled
            | ModuleState::Uninstallable
            | ModuleState::ToInstall
            | ModuleState::Unavailable => false,
        };
        let Some(manifest) = row.manifest().filter(|_| dependent) else {
            continue;
        };
        for dependency in manifest.depends() {
            let Some(state) = catalog.state(dependency.as_str()) else {
                continue;
            };
            let missing = match state {
                ModuleState::Uninstalled | ModuleState::Uninstallable => true,
                ModuleState::ToInstall => mode == LoadMode::Load,
                ModuleState::Installed | ModuleState::ToUpgrade | ModuleState::ToRemove => false,
                // Never stored: the catalog refuses it.
                ModuleState::Unavailable => {
                    return Err(Error::UnavailableIsNotStored {
                        module: dependency.clone(),
                    });
                }
            };
            if missing {
                inconsistent.push(InconsistentDependency::new(
                    row.name().clone(),
                    dependency.clone(),
                    state,
                ));
            }
        }
    }
    if inconsistent.is_empty() {
        Ok(())
    } else {
        inconsistent.sort();
        Err(Error::InconsistentStates {
            modules: inconsistent,
        })
    }
}

impl<'s> Run<'s> {
    fn new(
        system: &'s ModuleSystem,
        entries: IndexMap<ModuleName, AddonEntry>,
        base_demo: bool,
    ) -> Self {
        let data_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let migration_budget = MigrationBudget::new(
            system.parts.max_migration_files,
            Arc::clone(&data_bytes),
            system.parts.max_operation_data_bytes,
        );
        Self {
            system,
            entries,
            active: Vec::new(),
            package_identity_upgrade: None,
            installed: Vec::new(),
            upgraded: Vec::new(),
            uninstalled: Vec::new(),
            unavailable: Vec::new(),
            base_demo,
            dropped: DropReport::default(),
            guard: None,
            data_bytes,
            migration_budget,
            reinit: false,
            demo_only: false,
            restore_generated: false,
            restored: Vec::new(),
            notices: Vec::new(),
        }
    }

    /// Compose package admission with trusted host services. Every callback sees the same
    /// operation transaction; no publication is visible unless the later commit succeeds.
    async fn before_commit(&self, tx: &mut Transaction) -> Result<()> {
        let change = PackageChange {
            installed: &self.installed,
            upgraded: &self.upgraded,
            uninstalled: &self.uninstalled,
        };
        if let Some(runtime) = &self.system.parts.package_runtime {
            runtime.before_commit(tx, change).await?;
            self.checkpoint(tx).await?;
        }
        if !self.system.parts.lifecycle_hooks.is_empty() {
            let active: Vec<&AddonEntry> = self.active.iter().collect();
            let context = LifecycleChange::new(change, &active);
            for hook in &self.system.parts.lifecycle_hooks {
                hook.before_commit(tx, context).await?;
                self.checkpoint(tx).await?;
            }
        }
        Ok(())
    }

    /// Check that the operation's transaction is still the one it began, after code of a hook,
    /// loader or model method ran in it.
    async fn checkpoint(&self, tx: &mut Transaction) -> Result<()> {
        let guard = self
            .guard
            .as_ref()
            .ok_or_else(|| Error::InvariantViolation {
                message: "a checkpoint ran before the operation's transaction was identified"
                    .to_owned(),
            })?;
        verify_intact(tx, guard, Check::Timestamp).await
    }

    /// Run hook `kind` of `module` when its manifest names one, wrapping its error.
    async fn hook(
        &self,
        tx: &mut Transaction,
        module: &ModuleName,
        kind: HookKind,
        manifest: &Manifest,
    ) -> Result<()> {
        if let Some(hook) = self.system.parts.hook(module, kind, manifest)? {
            let step = ModuleStep::Hook {
                kind,
                name: hook.name,
            };
            (hook.run)(tx)
                .await
                .map_err(|e| step_error(module, step, None)(e.into()))?;
            self.checkpoint(tx)
                .await
                .map_err(step_error(module, step, None))?;
        }
        Ok(())
    }

    /// Run migration steps of `stage` for `module`.
    async fn run_migration_steps(
        &self,
        tx: &mut Transaction,
        module: &ModuleName,
        stage: MigrationStage,
        steps: Vec<MigrationStep>,
        installed_version: &SemverVersion,
    ) -> Result<()> {
        if steps.is_empty() {
            return Ok(());
        }
        let versions: Vec<SemverVersion> = steps.iter().map(|s| s.version).collect();
        let ordered_versions = crate::migrations::order_migration_versions(stage, versions);
        for ver in ordered_versions {
            let mut ver_steps: Vec<&MigrationStep> =
                steps.iter().filter(|s| s.version == ver).collect();
            // by name; equal names (a native step and a SQL file, or `1.1` and `20.0.1.1`
            // folders) by origin, so the order never depends on the directory listing
            ver_steps.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.origin.cmp(&b.origin)));
            for step in ver_steps {
                let step_desc = ModuleStep::Migration { stage };
                step.run(tx, installed_version).await.map_err(step_error(
                    module,
                    step_desc,
                    Some(&step.origin),
                ))?;
                self.checkpoint(tx).await.map_err(step_error(
                    module,
                    step_desc,
                    Some(&step.origin),
                ))?;
            }
        }
        Ok(())
    }

    /// The update run: load the installed set, then install/upgrade in update order, then
    /// uninstall, then `_register_hook` on the final registry. Returns the final ORM; the
    /// transaction is on it.
    async fn update(&mut self, optx: &mut OperationTx, catalog: &mut Catalog) -> Result<Orm> {
        let limits = optx.limits;
        self.guard = Some(optx.guard.clone());
        let OperationTx { tx, control, .. } = optx;
        let existing: Vec<_> = catalog
            .rows()
            .filter(|row| {
                matches!(
                    row.state(),
                    ModuleState::Installed | ModuleState::ToUpgrade | ModuleState::ToRemove
                )
            })
            .map(|row| row.name().clone())
            .collect();
        let content_hashes = installed_identity::entry_hashes(&self.system.parts, &self.entries)?;
        let mut validation_hashes = content_hashes.clone();
        if let Some(transition) = &self.package_identity_upgrade {
            if content_hashes.get(&transition.name) != Some(&transition.current) {
                return Err(erp_fields::Error::validation(
                    "Staged package identity changed before upgrade",
                )
                .into());
            }
            validation_hashes.insert(transition.name.clone(), transition.previous.clone());
        }
        // A scheduled module whose source vanished cannot run its step: an error,
        // not an "unavailable" note while it keeps its transient state.
        let mut vanished: Vec<(ModuleName, crate::graph::DropReason)> = catalog
            .rows()
            .filter(|r| r.state().is_transient() && r.manifest().is_none())
            .map(|r| (r.name().clone(), crate::graph::DropReason::Unavailable))
            .collect();
        if !vanished.is_empty() {
            vanished.sort_by(|a, b| a.0.cmp(&b.0));
            return Err(Error::DroppedUpdateModules { modules: vanished });
        }
        let previous = match &self.system.parts.identity_previous {
            Some(PreviousIdentity::Release(hashes)) => Some(hashes.clone()),
            Some(PreviousIdentity::EnrolledLegacy) => {
                Some(installed_identity::enrolled_receipts(tx, &existing).await?)
            }
            None => None,
        };
        let installed_preimages = installed_identity::validate_names(
            tx,
            &existing,
            &validation_hashes,
            previous.as_ref(),
        )
        .await?;
        installed_identity::validate_sources(tx, &existing, &self.entries).await?;
        if previous.is_some() {
            // A forward release publishes the complete candidate. Every changed addon that
            // remains installed must pass through upgrade before that publication, even when
            // the requested upgrade selected an unrelated addon. Existing removal semantics
            // remain with the uninstall path below.
            for name in &existing {
                if catalog.state(name.as_str()) == Some(ModuleState::Installed)
                    && content_hashes.get(name) != installed_preimages.get(name)
                {
                    return Err(erp_fields::Error::validation(format!(
                        "Addon '{name}' candidate content changed but is not selected for forward upgrade"
                    ))
                    .into());
                }
            }
        }
        check_consistent(catalog, LoadMode::Update)?;
        let GraphInput {
            states,
            manifests,
            unavailable,
        } = graph_input(catalog, |s| {
            matches!(
                s,
                ModuleState::Installed
                    | ModuleState::ToUpgrade
                    | ModuleState::ToRemove
                    | ModuleState::ToInstall
            )
        });
        self.unavailable = unavailable;
        let (graph, drops) = ModuleGraph::build(&manifests, &states, LoadMode::Update)?;
        self.dropped = drops;
        let plain = graph.plain_load_names();
        // The registry before any install: the modules not being installed whose dependencies
        // are all in it. An installed module that gains a dependency installed by this run
        // ('s corner case, a new dependency in `button_upgrade`) joins at its step.
        // Plain-load order puts dependencies first, so one pass decides.
        // `in_registry` is the set; `current` the same modules in plain-load order.
        let mut in_registry: HashSet<ModuleName> = HashSet::new();
        for name in &plain {
            let node = graph.get(name).ok_or_else(|| Error::InvariantViolation {
                message: format!("module {name} of the load order is not in the graph"),
            })?;
            if node.state() != ModuleState::ToInstall
                && node.depends().iter().all(|d| in_registry.contains(d))
            {
                in_registry.insert(name.clone());
            }
        }
        let in_order = |set: &HashSet<ModuleName>| -> Vec<ModuleName> {
            plain.iter().filter(|n| set.contains(*n)).cloned().collect()
        };
        let mut current = in_order(&in_registry);
        let mut orm = self.system.orm_for(&current, &self.entries)?;
        tx.operation_switch_orm(control, &orm).await?;

        // One load session for the whole run: it keeps what the end-of-run cleanup needs.
        let system = self.system;
        let mut session: Option<Box<dyn LoadSession + '_>> =
            system.parts.loader.as_ref().map(|loader| {
                let files = RunFiles::new(
                    &self.entries,
                    system.parts.max_data_file_bytes,
                    system.parts.max_operation_data_bytes,
                    Arc::clone(&self.data_bytes),
                );
                loader.session(
                    SessionContext::new(Arc::new(files), ODOO_SERIES)
                        .with_base_install_grant(&system.base_install_grant),
                )
            });
        let mut updated: Vec<ModuleName> = Vec::new();
        let mut queued_end: Vec<(ModuleName, SemverVersion, Vec<MigrationStep>)> = Vec::new();
        // the modules of the operation that load data, in order: the load state's pending
        // modules are those of them not loaded yet
        let loading: Vec<ModuleName> = graph
            .phase_order()
            .into_iter()
            .filter(|n| matches!(n.state(), ModuleState::ToInstall | ModuleState::ToUpgrade))
            .map(|n| n.name().clone())
            .collect();
        if let Some(session) = session.as_deref_mut() {
            session.begin_modules(&loading)?;
        }

        let mut pending = loading.as_slice();
        for node in graph.phase_order() {
            let module = node.name().clone();
            let manifest = Arc::clone(node.manifest());
            if !in_registry.contains(&module) {
                // pre_init runs on the registry without the module.
                if node.state() == ModuleState::ToInstall {
                    self.hook(tx, &module, HookKind::PreInit, &manifest).await?;
                }
                in_registry.insert(module.clone());
                current = in_order(&in_registry);
                orm = self.system.orm_for(&current, &self.entries)?;
                tx.operation_switch_orm(control, &orm).await?;
            }
            match node.state() {
                ModuleState::ToInstall => {
                    self.update_schema(tx, limits, &module, pending_after(pending, &module)?)
                        .await?;
                    let demo = self.demo_for(catalog, &module, &manifest)?;
                    let demo = self
                        .load_files(
                            tx,
                            session.as_deref_mut(),
                            &module,
                            &manifest,
                            ConvertMode::Init,
                            demo,
                        )
                        .await?;
                    if let Some(session) = session.as_deref_mut() {
                        // an install checks every view it writes: the names are not needed
                        let _names: LoadedNames = session.finish_module(&module)?;
                    }
                    pending = pending_after(pending, &module)?;
                    self.hook(tx, &module, HookKind::PostInit, &manifest)
                        .await?;
                    self.finish_module(catalog, &module, &manifest, demo)?;
                    updated.push(module.clone());
                    self.installed.push(module);
                }
                ModuleState::ToUpgrade => {
                    let row =
                        catalog
                            .get(module.as_str())
                            .ok_or_else(|| Error::InvariantViolation {
                                message: format!("module {module} of the update has no row"),
                            })?;
                    let demo = row.demo();
                    let installed_version = row.latest_version();
                    if !self.reinit {
                        // the version the upgrade installs. Never absent here: an incompatible
                        // manifest version makes the module not installable (manifest.rs, version
                        // adaptation), and the update graph drops a not-installable module
                        // before the run (`Error::DroppedUpdateModules`)
                        let target_version =
                            manifest.version().semver().copied().ok_or_else(|| {
                                Error::InvariantViolation {
                                    message: format!(
                                        "module {module} is upgraded, but its manifest version \
                                         '{}' is incompatible; the update graph drops such \
                                         modules",
                                        manifest.version().raw()
                                    ),
                                }
                            })?;
                        let mut staged = self
                            .system
                            .migrations_for(
                                &module,
                                &self.entries,
                                row.latest_version(),
                                target_version,
                                &self.migration_budget,
                            )
                            .await?;
                        if let Some(staged) = staged.as_mut() {
                            let pre = std::mem::take(&mut staged.pre);
                            self.run_migration_steps(
                                tx,
                                &module,
                                MigrationStage::Pre,
                                pre,
                                &staged.installed,
                            )
                            .await?;
                        }
                        self.update_schema(tx, limits, &module, pending_after(pending, &module)?)
                            .await?;
                        let mode = ConvertMode::Update;
                        let demo = self
                            .load_files(tx, session.as_deref_mut(), &module, &manifest, mode, demo)
                            .await?;
                        let loaded = match session.as_deref_mut() {
                            Some(session) => match session.finish_module(&module)? {
                                LoadedNames::Tracked(names) => Loaded::Tracked(names),
                                LoadedNames::Untracked => Loaded::Untracked,
                            },
                            // no loader: no data was loaded, no view written
                            None => Loaded::NoLoader,
                        };
                        pending = pending_after(pending, &module)?;
                        if let Some(staged) = staged.as_mut() {
                            let post = std::mem::take(&mut staged.post);
                            self.run_migration_steps(
                                tx,
                                &module,
                                MigrationStage::Post,
                                post,
                                &staged.installed,
                            )
                            .await?;
                        }
                        let pending_names = pending.iter().map(|m| m.as_str().to_owned()).collect();
                        validate_module_views(tx, &orm, &module, loaded, pending_names).await?;
                        if let Some(staged) = staged
                            && !staged.end.is_empty()
                        {
                            queued_end.push((module.clone(), staged.installed, staged.end));
                        }
                        self.finish_module(catalog, &module, &manifest, demo)?;
                    } else {
                        if !self.demo_only {
                            self.update_schema(
                                tx,
                                limits,
                                &module,
                                pending_after(pending, &module)?,
                            )
                            .await?;
                        }
                        let mode = ConvertMode::Init;
                        let demo = self
                            .load_files(tx, session.as_deref_mut(), &module, &manifest, mode, demo)
                            .await?;
                        if let Some(session) = session.as_deref_mut() {
                            let _names: LoadedNames = session.finish_module(&module)?;
                        }
                        pending = pending_after(pending, &module)?;
                        if self.demo_only {
                            // a demo-only run never changes the installed version: no migration
                            // or schema step ran for the manifest's
                            let version =
                                installed_version.ok_or_else(|| Error::InvariantViolation {
                                    message: format!(
                                        "module {module} is reloaded for demo data without an \
                                         installed version"
                                    ),
                                })?;
                            catalog.mark_installed(&module, version, demo)?;
                        } else {
                            self.finish_module(catalog, &module, &manifest, demo)?;
                        }
                    }
                    updated.push(module.clone());
                    self.upgraded.push(module);
                }
                // Loaded as they are; `to remove` is handled after the loop.
                ModuleState::Installed | ModuleState::ToRemove => {}
                ModuleState::Uninstalled
                | ModuleState::Uninstallable
                | ModuleState::Unavailable => {
                    return Err(Error::InvariantViolation {
                        message: format!(
                            "module {module} is '{}' in the update graph",
                            node.state()
                        ),
                    });
                }
            }
        }

        // Execute queued end migrations across all upgraded modules
        for (module, installed_version, end_steps) in queued_end {
            self.run_migration_steps(
                tx,
                &module,
                MigrationStage::End,
                end_steps,
                &installed_version,
            )
            .await?;
        }

        // The end-of-run cleanup of what the updated modules no longer load, before the uninstall.
        if let Some(session) = session.as_deref_mut() {
            // A forced demo load reloads no data file, so the cleanup of "records the module no
            // longer loads" would delete every data record of every module: it must not run.
            if !updated.is_empty() && !self.demo_only {
                session.process_end(tx, &updated).await?;
                tx.flush().await?;
                self.checkpoint(tx).await?;
            }
            let notices = session.take_notices();
            log_notices(&notices);
            self.notices.extend(notices);
        }

        // Uninstall: hooks in reverse load order on the registry that still has
        // the modules, then the data loader's purge on that registry, then the schema of the
        // registry without them.
        let removing: Vec<ModuleName> = plain
            .iter()
            .filter(|n| states.get(*n) == Some(&ModuleState::ToRemove))
            .cloned()
            .collect();
        if !removing.is_empty() {
            // The schema lock first, before the uninstall hooks and the purge (review D3): the
            // order is operation lock, schema lock, then table locks. Taken only after the
            // purge's row locks, a worker synchronizing the schema could hold it and wait for
            // them while this operation waits for it.
            tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await?;
            let removing_set: HashSet<&ModuleName> = removing.iter().collect();
            for module in removing.iter().rev() {
                let manifest =
                    Arc::clone(
                        manifests
                            .get(module)
                            .ok_or_else(|| Error::ModuleUnavailable {
                                module: module.clone(),
                            })?,
                    );
                self.hook(tx, module, HookKind::Uninstall, &manifest)
                    .await?;
                tx.flush().await?;
            }
            if let Some(loader) = &self.system.parts.loader {
                // One call for all of them (their records may reference each other): the error
                // names all of them.
                let notices =
                    loader
                        .purge(tx, &removing)
                        .await
                        .map_err(|source| Error::PurgeFailed {
                            modules: removing.clone(),
                            source: Box::new(source),
                        })?;
                log_notices(&notices);
                self.notices.extend(notices);
                tx.flush().await?;
                self.checkpoint(tx)
                    .await
                    .map_err(|source| Error::PurgeFailed {
                        modules: removing.clone(),
                        source: Box::new(source),
                    })?;
            }
            let old = Arc::clone(tx.registry());
            current.retain(|n| !removing_set.contains(n));
            orm = self.system.orm_for(&current, &self.entries)?;
            tx.operation_switch_orm(control, &orm).await?;
            let names: Vec<String> = removing.iter().map(ModuleName::to_string).collect();
            refuse_orphaning_uninstall(tx, &removing, &names, self.system.parts.loader.is_some())
                .await?;
            let dropped_tables = drop_removed_schema(tx, &old, &removing).await?;
            tx.execute(
                "DELETE FROM ir_model_data WHERE module = ANY($1)",
                &[&names],
            )
            .await?;
            // A removed module's steps would otherwise run (or fail) after every operation, and
            // so would the index steps of other modules on the tables that were dropped.
            tx.execute(
                "DELETE FROM ir_module_post_commit WHERE module = ANY($1)",
                &[&names],
            )
            .await?;
            delete_steps_of_dropped_tables(tx, &dropped_tables).await?;
            for module in &removing {
                catalog.mark_uninstalled(module)?;
            }
            self.uninstalled = removing;
        }

        let guard = self
            .guard
            .clone()
            .ok_or_else(|| Error::InvariantViolation {
                message: "the update ran without an identified operation transaction".to_owned(),
            })?;
        self.restored =
            crate::company_access::emit(tx, tx.registry().clone().as_ref(), self.restore_generated)
                .await?;
        register_hooks(tx, &guard).await?;
        for name in &self.installed {
            let entry = self
                .entries
                .get(name)
                .ok_or_else(|| Error::ModuleUnavailable {
                    module: name.clone(),
                })?;
            let hash = content_hashes
                .get(name)
                .ok_or_else(|| Error::ModuleUnavailable {
                    module: name.clone(),
                })?;
            installed_identity::insert_receipt(tx, entry, hash).await?;
        }
        if let Some(transition) = &self.package_identity_upgrade {
            if !self.upgraded.contains(&transition.name) {
                return Err(
                    erp_fields::Error::programming("Staged package was not upgraded").into(),
                );
            }
            let entry =
                self.entries
                    .get(&transition.name)
                    .ok_or_else(|| Error::ModuleUnavailable {
                        module: transition.name.clone(),
                    })?;
            installed_identity::commit_package_upgrade(tx, transition, entry).await?;
        }
        if self.system.parts.identity_previous.is_some() {
            installed_identity::record_forward_migrations(
                tx,
                &self.upgraded,
                &installed_preimages,
                &content_hashes,
            )
            .await?;
        }
        installed_identity::remove_receipts(tx, &self.uninstalled).await?;
        installed_identity::verify_entries_async(&self.entries).await?;
        store::save(tx, catalog).await?;
        self.active = ordered_addon_entries(&current, &self.entries)?;
        Ok(orm)
    }

    /// Create or alter the tables of the module's affected models, then `init()` on each.
    /// `later` are the modules of the run still to come after `module`; a model
    /// one of them still changes waits for its step ([`waits_for_later`]).
    async fn update_schema(
        &self,
        tx: &mut Transaction,
        limits: Limits,
        module: &ModuleName,
        later: &[ModuleName],
    ) -> Result<()> {
        if module.as_str() == "base" {
            check_stored_states(tx)
                .await
                .map_err(step_error(module, ModuleStep::Schema, None))?;
        }
        let registry = Arc::clone(tx.registry());
        let (models, waiting): (Vec<_>, Vec<_>) = affected_models(&registry, module)
            .into_iter()
            .partition(|m| !waits_for_later(m, later));
        // a waiting model is synced at a later module's step only if that module is among its
        // contributors, so none is ever left unsynced
        if let Some(model) = waiting.iter().find(|m| {
            !m.mro
                .iter()
                .any(|d| later.iter().any(|l| l.as_str() == d.addon))
        }) {
            return Err(Error::InvariantViolation {
                message: format!(
                    "model {} waits for a later module of the run that does not contribute to it",
                    model.name
                ),
            });
        }
        let ids: Vec<erp_registry::ModelId> = models
            .iter()
            .filter(|m| m.has_generic_table())
            .map(|m| m.id)
            .collect();
        // an install or upgrade is the one place SQL objects may change (A6)
        let options = self.system.parts.schema_sync.lifecycle(true);
        let declares_sql_objects = registry
            .sql_objects()
            .objects()
            .iter()
            .any(|o| o.owner() == module.as_str());
        if ids.is_empty() && declares_sql_objects {
            // no table model to sync, but SQL objects to install (plan D9)
            tx.init_sql_objects(&options).await.map_err(|e| {
                step_error(module, ModuleStep::Schema, None)(
                    limits.during_schema_step().classify(e.into()),
                )
            })?;
        }
        if !ids.is_empty() {
            // The schema step lowers the timeouts to its own bounds: a lock or statement error
            // reports the limit it ran under. Work that scans or rewrites a table above the
            // threshold of `schema_sync_options` is not done here: it comes back as steps, queued
            // below in this same transaction and run after the commit (Amendment 2).
            let sync = tx.init_models_with(&ids, &options).await.map_err(|e| {
                step_error(module, ModuleStep::Schema, None)(
                    limits.during_schema_step().classify(e.into()),
                )
            })?;
            queue_schema_steps(tx, module, sync)
                .await
                .map_err(|e| step_error(module, ModuleStep::Schema, None)(e.into()))?;
            // A core table that refers to a model's table is completed once that table exists:
            // on an upgrade the table of a new model is created by the step above.
            let deferred = erp_orm::core_ddl_pending(tx)
                .await
                .map_err(|e| step_error(module, ModuleStep::Schema, None)(e.into()))?;
            if !deferred.is_empty() {
                erp_orm::core_ddl_run_deferred(tx, &deferred)
                    .await
                    .map_err(|e| step_error(module, ModuleStep::Schema, None)(e.into()))?;
            }
            self.checkpoint(tx)
                .await
                .map_err(step_error(module, ModuleStep::Schema, None))?;
        }
        // Native model/field metadata and their external IDs must exist before addon
        // init hooks and security data refer to them (partial).
        // Use the staged registry and the same lifecycle transaction; never seed a
        // parallel metadata store or publish reflection from an uncommitted stage.
        erp_orm::reflection::reflect_models(tx)
            .await
            .map_err(|e| step_error(module, ModuleStep::Schema, None)(e.into()))?;
        self.checkpoint(tx)
            .await
            .map_err(step_error(module, ModuleStep::Schema, None))?;
        for model in models {
            let step = ModuleStep::ModelInit { model: model.name };
            model_hooks::call_init(tx, model.name)
                .await
                .map_err(|e| step_error(module, step, None)(e.into()))?;
            self.checkpoint(tx)
                .await
                .map_err(step_error(module, step, None))?;
        }
        Ok(())
    }

    /// Demo for a module being installed (`ModuleNode.demo_installable`,
    /// ): all its dependencies have demo.
    fn demo_for(
        &self,
        catalog: &Catalog,
        module: &ModuleName,
        manifest: &Manifest,
    ) -> Result<bool> {
        if module.as_str() == "base" {
            return Ok(self.base_demo);
        }
        if manifest.depends().is_empty() {
            return Err(Error::InvariantViolation {
                message: format!("module {module} has no dependency; only base may have none"),
            });
        }
        let mut all = true;
        for dep in manifest.depends() {
            let row = catalog
                .get(dep.as_str())
                .ok_or_else(|| Error::UnknownDependency {
                    module: module.clone(),
                    dependency: dep.clone(),
                })?;
            all &= row.demo();
        }
        Ok(all)
    }

    fn finish_module(
        &self,
        catalog: &mut Catalog,
        module: &ModuleName,
        manifest: &Manifest,
        demo: bool,
    ) -> Result<()> {
        let version =
            manifest
                .version()
                .semver()
                .copied()
                .ok_or_else(|| Error::ModuleNotInstallable {
                    module: module.clone(),
                })?;
        catalog.mark_installed(module, version, demo)?;
        Ok(())
    }

    /// Data files (and demo files when `demo`) through the load session.
    ///
    /// The data files load in the operation's transaction. The demo files load together inside
    /// one savepoint: when they fail on their data, the savepoint and the
    /// session roll back, the module installs without demo data, and the failure is a
    /// [`NoticeKind::DemoFailed`] (and an `ir.demo_failure` row when that model exists). Any
    /// other failure fails the operation. Returns whether the module has demo data afterwards.
    async fn load_files(
        &mut self,
        tx: &mut Transaction,
        session: Option<&mut (dyn LoadSession + '_)>,
        module: &ModuleName,
        manifest: &Manifest,
        mode: ConvertMode,
        demo: bool,
    ) -> Result<bool> {
        let demo_files: &[String] = if demo { manifest.demo() } else { &[] };
        // a forced demo load (`Request::ForceDemo`) leaves the data files alone: they loaded when
        // the module was installed, and `noupdate` records must not be rewritten by it
        let data_files: &[String] = if self.demo_only { &[] } else { manifest.data() };
        let count = data_files.len() + demo_files.len();
        if count == 0 {
            return Ok(demo);
        }
        let limit = self.system.parts.max_data_files;
        if count > limit {
            return Err(Error::TooManyDataFiles {
                module: module.clone(),
                count,
                limit,
            });
        }
        let Some(session) = session else {
            return Err(Error::DataLoaderMissing {
                module: module.clone(),
            });
        };
        let entry = self
            .entries
            .get(module)
            .ok_or_else(|| Error::ModuleUnavailable {
                module: module.clone(),
            })?
            .clone();
        self.load_list(
            tx,
            &mut *session,
            &entry,
            module,
            mode,
            DataKind::Data,
            data_files,
        )
        .await?;
        if demo_files.is_empty() {
            return Ok(demo);
        }
        let mark = session.mark();
        let mut failure: Option<Error> = None;
        let outcome = tx
            .savepoint(async |tx| {
                match self
                    .load_list(
                        tx,
                        &mut *session,
                        &entry,
                        module,
                        mode,
                        DataKind::Demo,
                        demo_files,
                    )
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        failure = Some(error);
                        // rolls the savepoint back; the runner decides what the failure means
                        Err(erp_fields::Error::programming(DEMO_FAILED))
                    }
                }
            })
            .await;
        match (outcome, failure) {
            (Ok(()), _) => Ok(true),
            (Err(error), None) => Err(error.into()),
            // Not the closure's own marker: the savepoint's rollback failed too (the connection is
            // gone). Both errors are returned; this is never "installed without demo data".
            (Err(error), Some(demo))
                if error.kind() != erp_fields::ErrorKind::Programming
                    || error.message() != DEMO_FAILED =>
            {
                Err(Error::Orm(erp_fields::Error::new(
                    error.kind(),
                    format!(
                        "the demo files of module {module} failed and the savepoint could not \
                         be rolled back: {}; the demo failure was: {}",
                        error.message(),
                        error_chain(&demo)
                    ),
                )))
            }
            (Err(_), Some(error)) if !is_data_failure(&error) => Err(error),
            (Err(_), Some(error)) => {
                session.rollback_to(mark)?;
                let text = error_chain(&error);
                tracing::warn!(
                    target: "erp::modules",
                    "Module {module} demo data failed to install, installed without demo data: {text}"
                );
                self.record_demo_failure(tx, module, &text)
                    .await
                    .map_err(|source| Error::DemoFailureNotRecorded {
                        module: module.clone(),
                        demo_error: text.clone(),
                        source: Box::new(source),
                    })?;
                self.notices.push(DataNotice::new(
                    module.as_str(),
                    first_demo_file(&error, demo_files),
                    NoticeKind::DemoFailed { error: text },
                ));
                Ok(false)
            }
        }
    }

    /// The `ir.demo_failure` row of a module whose demo data failed, when the
    /// registry has that model (no addon brings it yet).
    async fn record_demo_failure(
        &self,
        tx: &mut Transaction,
        module: &ModuleName,
        text: &str,
    ) -> Result<()> {
        let env = tx.env();
        if env.registry().model("ir.demo_failure").is_none() {
            return Ok(());
        }
        let rows = tx
            .query(
                "SELECT id FROM ir_module_module WHERE name = $1",
                &[&module.as_str()],
            )
            .await?;
        let id: i64 = rows
            .first()
            .ok_or_else(|| Error::InvariantViolation {
                message: format!("module {module} has no row to record its demo failure"),
            })?
            .try_get(0)
            .map_err(|e| Error::InvariantViolation {
                message: format!("unexpected ir_module_module row: {e}"),
            })?;
        let mut values = erp_fields::ValueMap::new();
        values.insert("module_id".to_owned(), erp_fields::Value::Int(id));
        values.insert("error".to_owned(), erp_fields::Value::str(text));
        env.records("ir.demo_failure")?
            .sudo(true)
            .create_values(tx, &[values.into_iter().collect()])
            .await?;
        Ok(())
    }

    /// The files of one manifest list, in order. A file listed twice loads twice and is noticed;
    /// `.js` is ignored; other extensions fail.
    #[allow(
        clippy::too_many_arguments,
        reason = "one call site; the arguments are the step's context"
    )]
    async fn load_list(
        &mut self,
        tx: &mut Transaction,
        session: &mut (dyn LoadSession + '_),
        entry: &AddonEntry,
        module: &ModuleName,
        mode: ConvertMode,
        kind: DataKind,
        paths: &[String],
    ) -> Result<()> {
        let step = match kind {
            DataKind::Data => ModuleStep::DataFile,
            DataKind::Demo => ModuleStep::DemoFile,
        };
        let mut seen: HashSet<&str> = HashSet::new();
        for path in paths {
            if !seen.insert(path.as_str()) {
                self.notices.push(DataNotice::new(
                    module.as_str(),
                    path.as_str(),
                    NoticeKind::RepeatedFile,
                ));
            }
            let file = DataFile::new(module.clone(), path.clone(), kind, mode);
            match self.load_file(tx, &mut *session, entry, &file).await {
                Ok(()) => {
                    self.checkpoint(tx)
                        .await
                        .map_err(step_error(module, step, Some(path)))?
                }
                // These already name the module and the file.
                Err(
                    e @ (Error::DataFileUnsupported { .. }
                    | Error::DataSourceUnsupported { .. }
                    | Error::PackageDataRefused { .. }
                    | Error::DataFileSymlink { .. }
                    | Error::DataFileHardLinked { .. }
                    | Error::DataFileTooLarge { .. }
                    | Error::OperationDataTooLarge { .. }
                    | Error::DataLoaderStoppedEarly { .. }),
                ) => return Err(e),
                Err(e) => return Err(step_error(module, step, Some(path))(e)),
            }
        }
        Ok(())
    }

    /// Add `len` bytes to what this operation has read; refuse when the total passes
    /// [`ModuleSystemBuilder::max_operation_data_bytes`].
    fn count_data_bytes(&self, module: &ModuleName, path: &str, len: usize) -> Result<()> {
        charge_data_bytes(
            &self.data_bytes,
            self.system.parts.max_operation_data_bytes,
            module,
            path,
            len,
        )
    }

    /// One data or demo file: read it and hand it to the loader by its extension.
    async fn load_file(
        &mut self,
        tx: &mut Transaction,
        session: &mut (dyn LoadSession + '_),
        entry: &AddonEntry,
        file: &DataFile,
    ) -> Result<()> {
        let module = file.module();
        let path = file.path();
        let extension = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("js") => {
                // valid, not loaded; not even read
                self.notices.push(DataNotice::new(
                    module.as_str(),
                    path,
                    NoticeKind::IgnoredFile,
                ));
                Ok(())
            }
            Some("xml") => {
                let content = self.read_counted(entry, module, path).await?;
                let mut ops = XmlDataReader::new(&content, path, module.as_str(), file.noupdate());
                if is_package(entry) {
                    // a package may only declare views, actions, menus: checked before any
                    // operation of the file runs
                    let registry = Arc::clone(tx.registry());
                    let mut checked = check_package_operations(
                        module,
                        path,
                        package_data_of(entry, module)?,
                        &field_class_in(&registry),
                        &mut ops,
                    )?;
                    return run_loader(session, tx, file, &mut checked).await;
                }
                run_loader(session, tx, file, &mut ops).await
            }
            Some("csv") => {
                let content = self.read_counted(entry, module, path).await?;
                let ops: Vec<DataOperation> =
                    match erp_data::parse_csv(&content, path, module.as_str(), file.mode())? {
                        erp_data::CsvImportResult::Loaded(import) => {
                            vec![import.into_operation(file.noupdate())]
                        }
                        erp_data::CsvImportResult::Empty => Vec::new(),
                        erp_data::CsvImportResult::Skipped { reason } => {
                            tracing::error!(target: "erp::modules", "Module {module}: {reason}");
                            self.notices.push(DataNotice::new(
                                module.as_str(),
                                path,
                                NoticeKind::SkippedFile { reason },
                            ));
                            return Ok(());
                        }
                        other => {
                            return Err(Error::InvariantViolation {
                                message: format!("unexpected CSV file content {other:?}"),
                            });
                        }
                    };
                let mut ops = ops.into_iter().map(Ok::<DataOperation, ParseError>);
                if is_package(entry) {
                    let registry = Arc::clone(tx.registry());
                    let mut checked = check_package_operations(
                        module,
                        path,
                        package_data_of(entry, module)?,
                        &field_class_in(&registry),
                        &mut ops,
                    )?;
                    return run_loader(session, tx, file, &mut checked).await;
                }
                run_loader(session, tx, file, &mut ops).await
            }
            Some("sql") if is_package(entry) => Err(Error::PackageDataRefused {
                module: module.clone(),
                path: path.to_owned(),
                line: 0,
                what: "SQL data files are not available to a package".to_owned(),
            }),
            Some("sql") => {
                // A COMMIT inside it is caught by the checkpoint.
                let content = self.read_counted(entry, module, path).await?;
                tx.flush().await?;
                // the file may `SET` session state (a role, the search path): the connection must
                // not go back to the pool with it
                tx.taint_connection();
                tx.batch(&content).await?;
                // the file may have changed anything behind the ORM's cache
                tx.invalidate_all().await?;
                Ok(())
            }
            _ => Err(Error::DataFileUnsupported {
                module: module.clone(),
                path: path.to_owned(),
            }),
        }
    }

    /// The text of a data file, counted against the operation's data budget.
    async fn read_counted(
        &self,
        entry: &AddonEntry,
        module: &ModuleName,
        path: &str,
    ) -> Result<String> {
        let content =
            read_data_file(entry, module, path, self.system.parts.max_data_file_bytes).await?;
        self.count_data_bytes(module, path, content.len())?;
        Ok(content)
    }
}

/// Classify fields by the registry of the operation (the package's own models are in it).
fn field_class_in(registry: &Arc<Registry>) -> impl Fn(&str, &str) -> Option<FieldClass> + use<> {
    let registry = Arc::clone(registry);
    move |model, field| {
        registry
            .model(model)
            .and_then(|m| m.field(field))
            .map(|f| match f.ty {
                erp_fields::FieldType::One2many { .. } => FieldClass::One2many,
                erp_fields::FieldType::Many2many { .. } => FieldClass::Many2many,
                _ => FieldClass::Scalar,
            })
    }
}

/// The data of a package entry whose files are being loaded.
fn package_data_of<'a>(
    entry: &'a AddonEntry,
    module: &ModuleName,
) -> Result<&'a crate::PackageData> {
    entry
        .package_data()
        .map(|d| &**d)
        .ok_or_else(|| Error::DataSourceUnsupported {
            module: module.clone(),
            source_kind: SourceKind::Package,
        })
}

/// Whether `entry` is a package addon (its data files are untrusted code-adjacent input and get
/// the narrower rules of [`check_package_operations`]).
fn is_package(entry: &AddonEntry) -> bool {
    matches!(entry.source_kind(), AddonSourceKind::Package { .. })
}

/// The models whose records a package's data files may declare.
const PACKAGE_DATA_MODELS: [&str; 4] = [
    "ir.ui.view",
    "ir.actions.act_window",
    "ir.ui.menu",
    "ir.access",
];

/// How the registry being installed classifies a field, for the checks of a package's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldClass {
    One2many,
    Many2many,
    Scalar,
}

/// Looks `(model, field)` up in the registry of the operation; `None` for an unknown field.
type FieldClassOf<'a> = &'a dyn Fn(&str, &str) -> Option<FieldClass>;

/// The columns an `ir.access` CSV of a package may have (an exact list: `.id` and `*/.id` would
/// update a row by its database id; `domain` is left out, so a package grants or restricts whole
/// operations on its own models; any other column is not part of what a package may write).
const ACCESS_COLUMNS: [&str; 8] = [
    "id",
    "name",
    "model_id/id",
    "model_id:id",
    "group_id/id",
    "group_id:id",
    "operation",
    "active",
];

/// Read the operations of one data file of a package and refuse everything outside what a package
/// may declare (security reviews r1 and r2). The principle: check what the loader will
/// apply, not the shape of the text.
///
/// * records of [`PACKAGE_DATA_MODELS`] and menu items only; `<function>`, `<delete>`, `<template>`,
///   `<asset>`, XML access records and SQL are refused by name; `context` and `uid` are refused on
///   every element (they change the loader's environment);
/// * every record id, menu item id and CSV `id` belongs to the package itself (unqualified, or
///   qualified with the package's name): a package never updates a record of another addon.
///   `inherit_id` and `ref()` to other modules stay allowed; a `<field>` name is given once;
/// * a view's `model`, an action's `res_model` name a model the package declares or extends. On a
///   model it only extends: extension views only (`inherit_id`, `mode` absent or `extension`), no
///   `priority`, no `binding_model_id`. No `qweb` view and no `key`;
/// * field values are classified by the registry's field type. A one2many gets no value at all
///   (a child record or a link would write another model's rows); a many2many takes a
///   list of 3-element link (4), clear (5) and set (6) commands whose code is a literal integer;
///   any other field takes literals, `ref()`, and lists and tuples of them;
/// * access rows (CSV) have only the allowed columns and grant only on the package's own models
///   (`<package>.model_...`).
///
/// This is an AST check, not a check of the evaluated value: the loader evaluates expressions
/// itself and offers no hook between evaluation and write, so the rules above are strict enough
/// that every accepted expression has one evaluated form (a literal list, a literal tuple, a
/// `ref()` result, a literal number or string) and a list is treated exactly like a tuple.
///
/// The operations are returned in order for the loader; a parse error stays where it was and ends
/// the list.
fn check_package_operations(
    module: &ModuleName,
    path: &str,
    data: &crate::PackageData,
    class_of: FieldClassOf<'_>,
    ops: &mut crate::loader::Operations<'_>,
) -> Result<std::vec::IntoIter<std::result::Result<DataOperation, ParseError>>> {
    let rules = PackageRules {
        module,
        path,
        data,
        class_of,
    };
    let mut out = Vec::new();
    for op in ops {
        let operation = match op {
            Ok(operation) => operation,
            Err(error) => {
                out.push(Err(error));
                break;
            }
        };
        rules.operation(&operation)?;
        out.push(Ok(operation));
    }
    Ok(out.into_iter())
}

struct PackageRules<'a> {
    module: &'a ModuleName,
    path: &'a str,
    data: &'a crate::PackageData,
    class_of: FieldClassOf<'a>,
}

impl PackageRules<'_> {
    fn refuse(&self, line: usize, what: String) -> Error {
        Error::PackageDataRefused {
            module: self.module.clone(),
            path: self.path.to_owned(),
            line,
            what,
        }
    }

    /// The model is one the package declares or extends.
    fn own_or_extended(&self, model: &str) -> bool {
        self.data.declared.contains(model) || self.data.extended.contains(model)
    }

    /// An id the package may write: unqualified, or qualified with its own name.
    fn own_id(&self, line: usize, id: &erp_data::XmlId) -> Result<()> {
        match id.module() {
            Some(m) if m != self.module.as_str() => Err(self.refuse(
                line,
                format!(
                    "id '{m}.{}' belongs to module '{m}': a package may only write records with ids of its own ({}.)",
                    id.name(),
                    self.module
                ),
            )),
            _ => Ok(()),
        }
    }

    /// `context` and `uid` change the environment the loader runs a record in (it carries the
    /// state that drives view validation), on `<data>`, `<record>`, `<menuitem>`: none is allowed.
    fn plain_env(&self, line: usize, env: &erp_data::OpEnv) -> Result<()> {
        if env.uid().is_some() || !env.context_chain().is_empty() {
            return Err(self.refuse(
                line,
                "`context` and `uid` are not allowed on any element of a package's data".to_owned(),
            ));
        }
        Ok(())
    }

    fn operation(&self, operation: &DataOperation) -> Result<()> {
        let line = operation.line();
        self.plain_env(line, operation.env())?;
        match operation {
            DataOperation::MenuItem(item) => self.menu_item(item),
            DataOperation::Record(record) => self.record(record),
            DataOperation::Csv(csv) => self.access_csv(line, csv.import()),
            DataOperation::Delete(_) => {
                Err(self.refuse(line, "<delete> is not allowed".to_owned()))
            }
            DataOperation::Function(_) => {
                Err(self.refuse(line, "<function> is not allowed".to_owned()))
            }
            DataOperation::Template(_) => {
                Err(self.refuse(line, "<template> is not allowed".to_owned()))
            }
            DataOperation::Asset(_) => Err(self.refuse(line, "<asset> is not allowed".to_owned())),
            other => Err(self.refuse(line, format!("operation {other:?} is not allowed"))),
        }
    }

    fn menu_item(&self, item: &erp_data::MenuItemOp) -> Result<()> {
        self.plain_env(item.line(), item.env())?;
        self.own_id(item.line(), item.id())?;
        item.children().iter().try_for_each(|c| self.menu_item(c))
    }

    fn access_csv(&self, line: usize, import: &erp_data::CsvImport) -> Result<()> {
        if import.model() != "ir.access" {
            return Err(self.refuse(
                line,
                format!(
                    "a CSV file for model '{}' is not allowed (only ir.access)",
                    import.model()
                ),
            ));
        }
        if let Some(bad) = import
            .fields()
            .iter()
            .find(|f| !ACCESS_COLUMNS.contains(&f.as_str()))
        {
            return Err(self.refuse(
                line,
                format!(
                    "access CSV column '{bad}' is not allowed (allowed: {})",
                    ACCESS_COLUMNS.join(", ")
                ),
            ));
        }
        let column = |names: &[&str]| {
            import
                .fields()
                .iter()
                .position(|f| names.contains(&f.as_str()))
        };
        let Some(model_column) = column(&["model_id/id", "model_id:id"]) else {
            return Err(self.refuse(
                line,
                "an access CSV needs a model_id/id column naming the model".to_owned(),
            ));
        };
        let Some(id_column) = column(&["id"]) else {
            return Err(self.refuse(
                line,
                "an access CSV needs an id column (a row without one cannot be removed with the package)"
                    .to_owned(),
            ));
        };
        let own_prefix = format!("{}.model_", self.module);
        for row in import.rows() {
            let cell = |i: usize| row.values().get(i).map_or("", String::as_str);
            let id = cell(id_column);
            if id.is_empty() {
                return Err(self.refuse(row.line(), "an access row needs an id".to_owned()));
            }
            if let Some((m, _)) = id.split_once('.')
                && m != self.module.as_str()
            {
                return Err(self.refuse(
                    row.line(),
                    format!(
                        "access row id '{id}' belongs to module '{m}': a package may only write rows with ids of its own"
                    ),
                ));
            }
            let target = cell(model_column);
            let own = target.starts_with(&own_prefix)
                || (!target.contains('.') && target.starts_with("model_"));
            if !own {
                return Err(self.refuse(
                    row.line(),
                    format!(
                        "access row for '{target}': a package may only grant access on models it declares itself ({own_prefix}...)"
                    ),
                ));
            }
        }
        Ok(())
    }

    fn record(&self, record: &erp_data::RecordOp) -> Result<()> {
        let line = record.line();
        let model = record.model();
        self.plain_env(line, record.env())?;
        if model == "ir.access" {
            return Err(self.refuse(
                line,
                "access rows must be in a CSV file (ir.access.csv), where the models they grant on can be checked".to_owned(),
            ));
        }
        if !PACKAGE_DATA_MODELS.contains(&model) {
            return Err(self.refuse(line, format!("a record of model '{model}' is not allowed")));
        }
        match record.id() {
            Some(id) => self.own_id(line, id)?,
            None => {
                return Err(self.refuse(
                    line,
                    format!("a record of '{model}' needs an id (it could not be removed with the package)"),
                ));
            }
        }
        // the checks below read a field once; the loader keeps the last of a repeated name
        let mut seen = BTreeSet::new();
        for field in record.fields() {
            if !seen.insert(field.name()) {
                return Err(self.refuse(
                    field.line(),
                    format!("field '{}' is given twice in one record", field.name()),
                ));
            }
        }
        let mut seen = BTreeSet::new();
        for field in record.translations() {
            if !seen.insert((field.name(), field.lang())) {
                return Err(self.refuse(
                    field.line(),
                    format!(
                        "translation of '{}' is given twice in one record",
                        field.name()
                    ),
                ));
            }
        }
        for field in record.fields().iter().chain(record.translations()) {
            self.field(model, field)?;
        }
        let text_of = |name: &str| match record.field(name).map(erp_data::FieldOp::value) {
            Some(erp_data::FieldValue::Text(t)) => Ok(Some(t.trim().to_owned())),
            Some(_) => Err(self.refuse(
                line,
                format!("field '{name}' of a {model} must be written as plain text"),
            )),
            None => Ok(None),
        };
        match model {
            "ir.ui.view" => {
                let Some(target) = text_of("model")? else {
                    return Err(self.refuse(line, "a view needs a `model` field".to_owned()));
                };
                if !self.own_or_extended(&target) {
                    return Err(self.refuse(
                        line,
                        format!("view of model '{target}': a package may only add views of models it declares or extends"),
                    ));
                }
                if text_of("type")?.as_deref() == Some("qweb") || record.field("key").is_some() {
                    return Err(self.refuse(
                        line,
                        "qweb views and view keys are not allowed in a package".to_owned(),
                    ));
                }
                if !self.data.declared.contains(&target) {
                    // a model the package only extends: it may extend the screens, not replace them
                    let parent_is_ref = match record
                        .field("inherit_id")
                        .map(erp_data::FieldOp::value)
                    {
                        Some(erp_data::FieldValue::Ref(_)) => true,
                        // exactly `ref('<str>')`: anything else (`False`, `0`, empty text) coerces
                        // to no parent, which makes the view a primary view
                        Some(erp_data::FieldValue::Eval { expr, .. }) => matches!(
                            expr.parsed(),
                            Ok(erp_expr::Expr::Call { func, args, keywords })
                                if matches!(&**func, erp_expr::Expr::Name(n) if n == "ref")
                                    && keywords.is_empty()
                                    && matches!(args.as_slice(), [erp_expr::Expr::Literal(erp_expr::Literal::Str(_))])
                        ),
                        _ => false,
                    };
                    if !parent_is_ref {
                        return Err(self.refuse(
                            line,
                            format!("'{target}' is a model the package only extends: only extension views are allowed on it, and `inherit_id` must be `ref=\"...\"` or `eval=\"ref('...')\"` (a false or empty value makes a primary view)"),
                        ));
                    }
                    if !matches!(text_of("mode")?.as_deref(), None | Some("extension")) {
                        return Err(self.refuse(
                            line,
                            format!("view on '{target}', a model the package only extends: `mode` must be absent or `extension`"),
                        ));
                    }
                    if record.field("priority").is_some() {
                        return Err(self.refuse(
                            line,
                            format!("`priority` on a view of '{target}', a model the package only extends, could reorder the screens of another addon"),
                        ));
                    }
                }
            }
            "ir.actions.act_window" => {
                let Some(target) = text_of("res_model")? else {
                    return Err(self.refuse(line, "an action needs a `res_model` field".to_owned()));
                };
                if !self.own_or_extended(&target) {
                    return Err(self.refuse(
                        line,
                        format!("action on model '{target}': a package may only add actions for models it declares or extends"),
                    ));
                }
                if let Some(binding) = record.field("binding_model_id") {
                    // the full xmlid of the `ir.model` row of a model this package declares
                    let allowed =
                        self.data.declared.contains(&target)
                            && match binding.value() {
                                erp_data::FieldValue::Ref(x) => {
                                    let ours = x.module().is_none_or(|m| m == self.module.as_str());
                                    ours && self.data.declared.iter().any(|m| {
                                        x.name() == format!("model_{}", m.replace('.', "_"))
                                    })
                                }
                                _ => false,
                            };
                    if !allowed {
                        return Err(self.refuse(
                            binding.line(),
                            "`binding_model_id` must be a ref() to `<package>.model_...` of a model the package declares, on an action for that model".to_owned(),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn field(&self, model: &str, field: &erp_data::FieldOp) -> Result<()> {
        use erp_data::FieldValue as V;
        let line = field.line();
        let name = field.name();
        let Some(class) = (self.class_of)(model, name) else {
            return Err(self.refuse(
                line,
                format!("'{name}' is not a field of '{model}' in the registry being installed"),
            ));
        };
        match (class, field.value()) {
            (FieldClass::One2many, _) => Err(self.refuse(
                line,
                format!("field '{name}' is a one2many: a package may not give it a value (a child record or a link would write rows of another model)"),
            )),
            (FieldClass::Many2many, V::Eval { expr, .. }) => match expr.parsed() {
                Ok(parsed) => self.commands(line, name, parsed),
                Err(error) => Err(self.refuse(
                    line,
                    format!("field '{name}': eval `{expr}` does not parse: {error}"),
                )),
            },
            (FieldClass::Many2many, _) => Err(self.refuse(
                line,
                format!("field '{name}' is a many2many: it takes a list of link (4), clear (5) and set (6) commands"),
            )),
            (FieldClass::Scalar, value) => match value {
                V::Text(_)
                | V::Ref(_)
                | V::Int(_)
                | V::Float(_)
                | V::Xml(_)
                | V::Html(_)
                | V::CsvCell(_) => Ok(()),
                V::Eval { expr, .. } => self.eval(line, name, expr),
                V::List(items) | V::Tuple(items) => {
                    items.iter().try_for_each(|v| self.value(line, name, v))
                }
                V::Search { .. } => Err(self.refuse(
                    line,
                    format!("field '{name}': `search` lookups are not allowed in a package; use ref()"),
                )),
                V::SubRecords(_) => Err(self.refuse(
                    line,
                    format!("field '{name}': nested records are not allowed in a package"),
                )),
                _ => Err(self.refuse(
                    line,
                    format!("field '{name}': files named by a field (type file/bytes/base64, file=) are not allowed in a package"),
                )),
            },
        }
    }

    fn value(&self, line: usize, name: &str, value: &erp_data::ValueOp) -> Result<()> {
        use erp_data::ValueOp as V;
        match value {
            V::Text(_) | V::Int(_) | V::Float(_) | V::EmptyList | V::EmptyTuple => Ok(()),
            V::Eval { expr, .. } => self.eval(line, name, expr),
            _ => Err(self.refuse(
                line,
                format!(
                    "field '{name}': this value form (search, file) is not allowed in a package"
                ),
            )),
        }
    }

    fn eval(&self, line: usize, name: &str, source: &erp_data::Expr) -> Result<()> {
        match source.parsed() {
            Ok(parsed) => self.expr(line, name, parsed),
            Err(error) => Err(self.refuse(
                line,
                format!("field '{name}': eval `{source}` does not parse: {error}"),
            )),
        }
    }

    /// The value of a many2many field: a list (or tuple) of commands, each a list or tuple of
    /// exactly three elements whose first is the literal integer 4 (link), 5 (clear) or 6 (set).
    /// A list is read exactly like a tuple, and a command code that is not a bare integer literal
    /// (`+2`, `-(-2)`, a name, a call) is refused: the loader turns either container into the
    /// same command, so only the evaluated code matters.
    fn commands(&self, line: usize, name: &str, expr: &erp_expr::Expr) -> Result<()> {
        use erp_expr::{Expr, Literal};
        let no = |why: String| {
            Err(self.refuse(
                line,
                format!("field '{name}': {why} (a many2many takes a list of 3-element link (4, id, 0), clear (5, 0, 0) and set (6, 0, ids) commands)"),
            ))
        };
        let (Expr::List(items) | Expr::Tuple(items)) = expr else {
            return no("the value is not a list of commands".to_owned());
        };
        for item in items {
            let (Expr::List(parts) | Expr::Tuple(parts)) = item else {
                return no("a command is not a list or tuple".to_owned());
            };
            if parts.len() != 3 {
                return no(format!("a command has {} elements, not 3", parts.len()));
            }
            match &parts[0] {
                Expr::Literal(Literal::Int(4..=6)) => {}
                Expr::Literal(Literal::Int(n)) => {
                    return no(format!("x2many command {n} is not allowed"));
                }
                _ => return no("the command code is not a plain integer literal".to_owned()),
            }
            parts[1..]
                .iter()
                .try_for_each(|e| self.expr(line, name, e))?;
        }
        Ok(())
    }

    /// Only literals, `ref('...')`, and lists and tuples of them.
    fn expr(&self, line: usize, name: &str, expr: &erp_expr::Expr) -> Result<()> {
        use erp_expr::{Expr, Literal};
        let no = |why: &str| {
            Err(self.refuse(
                line,
                format!("field '{name}': {why} (a package's eval may only use literals, ref('...') and lists and tuples of them)"),
            ))
        };
        match expr {
            Expr::Literal(_) => Ok(()),
            Expr::Unary { operand, .. } => self.expr(line, name, operand),
            Expr::List(items) | Expr::Tuple(items) => {
                items.iter().try_for_each(|e| self.expr(line, name, e))
            }
            Expr::Call {
                func,
                args,
                keywords,
            } if matches!(&**func, Expr::Name(n) if n == "ref")
                && keywords.is_empty()
                && matches!(args.as_slice(), [Expr::Literal(Literal::Str(_))]) =>
            {
                Ok(())
            }
            Expr::Dict(_) => no("a dict (a command payload) is not allowed"),
            Expr::Call { .. } => no("only ref('...') may be called"),
            _ => no("this expression form is not allowed"),
        }
    }
}

fn log_notices(notices: &[DataNotice]) {
    for notice in notices {
        tracing::warn!(target: "erp::modules", "{notice}");
    }
}

/// The marker error the demo savepoint's closure returns to roll the savepoint back.
const DEMO_FAILED: &str = "the demo files failed";

/// Add `len` bytes to the operation's `counter`; refuse when the total passes `limit`. The data
/// files and the files that data fields name share one counter.
pub(crate) fn charge_data_bytes(
    counter: &std::sync::atomic::AtomicU64,
    limit: u64,
    module: &ModuleName,
    path: &str,
    len: usize,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    let refused = || Error::OperationDataTooLarge {
        module: module.clone(),
        path: path.to_owned(),
        limit,
    };
    let len = u64::try_from(len).map_err(|_| refused())?;
    // one atomic step, so reads that run side by side cannot pass the limit together
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
            total.checked_add(len).filter(|total| *total <= limit)
        })
        .map(drop)
        .map_err(|_| refused())
}

/// The method of `ir.ui.view` that checks a module's `noupdate` views after its upgrade.
const VALIDATE_MODULE_VIEWS: &str = "_validate_module_views";

/// What the session reports about the names a module loaded.
enum Loaded {
    /// The run has no data loader.
    NoLoader,
    /// The loader does not track the names it loaded.
    Untracked,
    /// The names the loader loaded for the module, sorted.
    Tracked(Vec<String>),
}

/// Consume exactly the finished module from the shared data-load order. Both installs and
/// upgrades advance this borrowed suffix, so a later upgrade never treats an earlier install
/// as pending. No per-module search through the already-updated modules is needed.
fn pending_after<'a>(pending: &'a [ModuleName], finished: &ModuleName) -> Result<&'a [ModuleName]> {
    let Some((next, rest)) = pending.split_first() else {
        return Err(Error::InvariantViolation {
            message: format!("finished module {finished} after the data load order was exhausted"),
        });
    };
    if next != finished {
        return Err(Error::InvariantViolation {
            message: format!(
                "finished module {finished}, but the next module in the data load order is {next}"
            ),
        });
    }
    Ok(rest)
}

/// Call `ir.ui.view._validate_module_views(module, names, pending)` when the registry has the
/// view model, with the names the operation loaded for `module` and the modules of the operation
/// whose data has not loaded yet (`pending`: their views are not combined, review F-2). A run without data loader loaded nothing,
/// so it checks nothing (before the method is looked up, review A2-R7).
///
/// # Errors
/// [`Error::InvariantViolation`] when the registry's `ir.ui.view` has no
/// `_validate_module_views`, or when the loader does not track the names a module loaded; the
/// check's own error.
async fn validate_module_views(
    tx: &mut Transaction,
    orm: &Orm,
    module: &ModuleName,
    loaded: Loaded,
    pending: Vec<String>,
) -> Result<()> {
    let Some(view) = orm.registry().model("ir.ui.view") else {
        return Ok(());
    };
    if matches!(loaded, Loaded::NoLoader) {
        return Ok(());
    }
    if orm
        .dispatch()
        .implementations(view.id, VALIDATE_MODULE_VIEWS)
        .is_empty()
    {
        return Err(Error::InvariantViolation {
            message: format!(
                "the registry's ir.ui.view has no {VALIDATE_MODULE_VIEWS}: the noupdate views of \
                 module {module} cannot be checked after its upgrade"
            ),
        });
    }
    let names = match loaded {
        Loaded::Tracked(names) => names,
        // returned before the method lookup
        Loaded::NoLoader => return Ok(()),
        Loaded::Untracked => {
            return Err(Error::InvariantViolation {
                message: format!(
                    "the data loader does not report the external ids it loaded: the noupdate \
                     views of module {module} cannot be checked after its upgrade"
                ),
            });
        }
    };
    if names.is_empty() {
        return Ok(());
    }
    let views = tx.env().records("ir.ui.view")?;
    views
        .call::<(String, Vec<String>, Vec<String>), ()>(
            tx,
            VALIDATE_MODULE_VIEWS,
            (module.as_str().to_owned(), names, pending),
        )
        .await?;
    Ok(())
}

/// Whether a failure of the demo files is the demo data's own fault (the savepoint rolls back and
/// the module installs without demo data) rather than the operation's (it fails).
/// A
/// `Programming` error is the data's (an unknown field, a wrong value type) unless it is marked
/// as a broken contract between layers (`erp_fields::Error::invariant`: a load state the loader
/// did not write, a record layer's invariant), which fails the operation (review F-1).
fn is_data_failure(error: &Error) -> bool {
    use erp_fields::ErrorKind;
    let orm = |e: &erp_fields::Error| {
        if e.is_invariant_violation() {
            return false;
        }
        match e.kind() {
            ErrorKind::Validation
            | ErrorKind::UserError
            | ErrorKind::Missing
            | ErrorKind::Access => true,
            ErrorKind::Programming => true,
            ErrorKind::Database => e
                .sqlstate()
                .is_some_and(|s| s.starts_with("22") || s.starts_with("23")),
            // Unknown/future kinds fail the install; only the allowlisted data kinds above
            // may be absorbed by the demo savepoint.
            _ => false,
        }
    };
    match error {
        Error::StepFailed { source, .. } => is_data_failure(source),
        Error::DataParse(_) | Error::DataFileTooLarge { .. } => true,
        Error::DataLoad(e) => {
            use erp_data::load::FileReadError;
            use erp_data::load::LoadErrorKind as K;
            match e.kind() {
                K::Orm(inner) => orm(inner),
                // an expression that fails is the data's mistake; one that needs a name the host
                // has not bound yet, or that passes a resource limit, is not
                K::Eval { error, .. } => !matches!(
                    error,
                    erp_expr::ExprError::Unavailable { .. }
                        | erp_expr::ExprError::LimitExceeded { .. }
                ),
                // the demo data's own mistakes
                K::Parse(_)
                | K::InvalidExternalId(_)
                | K::ExternalIdNotFound { .. }
                | K::ModelMissing { .. }
                | K::CsvImport { .. }
                | K::NotAFieldValue { .. }
                | K::TypeMismatch { .. }
                | K::NotAUser { .. }
                | K::MissingForeignRecord { .. }
                | K::UninstalledModule { .. }
                | K::NotOne2many { .. }
                | K::NotDataCallable { .. }
                | K::UnknownMethod { .. }
                | K::FunctionArguments { .. }
                | K::DeleteWrongModel { .. }
                | K::SearchNeedsModel
                | K::FileNotText { .. } => true,
                // a file the addon lacks or that is too large is the addon's; any other read
                // failure (permissions, a failing disk) is the host's and fails the operation
                K::File(FileReadError::NotFound { .. } | FileReadError::TooLarge { .. }) => true,
                K::File(_) => false,
                // a missing feature is a defect of the loader, not of the demo data
                K::Unsupported { .. } | K::ModelArrivesLater { .. } | K::Internal(_) => false,
                // a kind nobody has classified yet fails the operation until someone does
                _ => false,
            }
        }
        Error::Orm(e) => orm(e),
        _ => false,
    }
}

/// An error and its sources, one per line, outermost first.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut lines = vec![error.to_string()];
    let mut next = error.source();
    while let Some(source) = next {
        lines.push(source.to_string());
        next = source.source();
    }
    lines.join("\n")
}

/// The demo file a failure names (the step error carries it), else the first demo file.
fn first_demo_file(error: &Error, demo_files: &[String]) -> String {
    match error {
        Error::StepFailed {
            file: Some(file), ..
        } => file.clone(),
        // the demo files are not empty where this runs (a module without demo files returns first)
        _ => demo_files
            .first()
            .cloned()
            .unwrap_or_else(|| "<demo files>".to_owned()),
    }
}

/// Hand one file's operations to the load session and check its work: a parse error it did not
/// return, or operations it left unread, fail the operation.
async fn run_loader(
    session: &mut (dyn LoadSession + '_),
    tx: &mut Transaction,
    file: &DataFile,
    operations: &mut crate::loader::Operations<'_>,
) -> Result<()> {
    let mut checked = Checked::new(operations);
    session.load(tx, file, &mut checked).await?;
    match checked.finish() {
        Ok(()) => Ok(()),
        Err(LoaderVerdict::Parse(error)) => Err(Error::DataParse(error)),
        Err(LoaderVerdict::StoppedEarly) => Err(Error::DataLoaderStoppedEarly {
            module: file.module().clone(),
            path: file.path().to_owned(),
        }),
    }
}

/// Uninstall must not leave records behind: after the data loader's purge (when there is one),
/// external ids of the removed modules that point at models still in the registry name records
/// nobody removed. Deleting only the external ids would orphan them (review H5, lead review B):
/// without a loader the error names (which implements the purge,/030), with
/// one it says the purge left them.
async fn refuse_orphaning_uninstall(
    tx: &mut Transaction,
    removing: &[ModuleName],
    names: &[String],
    purged: bool,
) -> Result<()> {
    let registry = Arc::clone(tx.registry());
    // what "orphaned" means: a row that names a record, of a model that is not the module
    // system's own reflection (the schema removal handles those)
    let schema: Vec<&str> = erp_data::load::SCHEMA_MODELS.to_vec();
    let rows = tx
        .query(
            "SELECT module, model, count(*) FROM ir_model_data WHERE module = ANY($1) \
             AND res_id IS NOT NULL AND model <> ALL($2) \
             GROUP BY module, model ORDER BY module, model",
            &[&names, &schema],
        )
        .await?;
    let mut owned: BTreeMap<String, Vec<OwnedRecords>> = BTreeMap::new();
    for row in &rows {
        let shape = |e: tokio_postgres::Error| Error::InvariantViolation {
            message: format!("unexpected ir_model_data row: {e}"),
        };
        let module: String = row.try_get(0).map_err(shape)?;
        let model: String = row.try_get(1).map_err(shape)?;
        let count: i64 = row.try_get(2).map_err(shape)?;
        if registry.model(&model).is_some() {
            owned
                .entry(module)
                .or_default()
                .push(OwnedRecords::new(model, count));
        }
    }
    for module in removing {
        if let Some(models) = owned.remove(module.as_str()) {
            return Err(if purged {
                Error::PurgeIncomplete {
                    module: module.clone(),
                    models,
                }
            } else {
                Error::UninstallOwnedRecords {
                    module: module.clone(),
                    models,
                }
            });
        }
    }
    Ok(())
}

/// Read a data file of a dev-folder addon: at most `limit` bytes, no symbolic link anywhere
/// under the addon directory, the resolved path inside the resolved addon directory, on the
/// blocking pool (no blocking I/O on the async runtime).
async fn read_data_file(
    entry: &AddonEntry,
    module: &ModuleName,
    path: &str,
    limit: u64,
) -> Result<String> {
    let bytes = read_data_bytes(entry, module, path, limit).await?;
    String::from_utf8(bytes).map_err(|e| Error::Io {
        path: PathBuf::from(path),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })
}

/// [`read_data_file`] without the text decoding.
async fn read_data_bytes(
    entry: &AddonEntry,
    module: &ModuleName,
    path: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    if let Some(snapshot) = entry.artifact_snapshot() {
        return snapshot_file(snapshot, module, path, limit).map(<[u8]>::to_vec);
    }
    let dir: PathBuf = match entry.source_kind() {
        AddonSourceKind::DevFolder(dir) => dir.clone(),
        AddonSourceKind::Embedded => {
            return Err(Error::DataSourceUnsupported {
                module: module.clone(),
                source_kind: SourceKind::Embedded,
            });
        }
        AddonSourceKind::Package { .. } => {
            // the files of the verified archive, already in memory; a package registered without
            // them has no data files to read
            let Some(data) = entry.package_data() else {
                return Err(Error::DataSourceUnsupported {
                    module: module.clone(),
                    source_kind: SourceKind::Package,
                });
            };
            return package_file(&data.files, module, path, limit).map(<[u8]>::to_vec);
        }
    };
    let module = module.clone();
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_data_file_blocking(&dir, &module, &path, limit))
        .await
        .map_err(|e| Error::InvariantViolation {
            message: format!("the data file reader task failed: {e}"),
        })?
}

/// Preserve the filesystem reader's directory-vs-missing distinction over admitted bytes.
fn snapshot_file<'a>(
    snapshot: &'a crate::artifact_identity::ArtifactSnapshot,
    module: &ModuleName,
    path: &str,
    limit: u64,
) -> Result<&'a [u8]> {
    if let Some(component) = path
        .split('/')
        .find(|component| crate::artifact_paths::excluded(component))
    {
        return Err(erp_fields::Error::validation(format!(
            "Addon '{module}' data file '{path}' references excluded packaging entry '{component}'"
        ))
        .into());
    }
    if snapshot.directories().contains(path) {
        return Err(Error::Io {
            path: snapshot.root_path().join(path),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"),
        });
    }
    package_file(snapshot.files(), module, path, limit)
}

/// A data file of a package addon, from the verified archive held in memory: `path` is a plain
/// relative path (no `.`, `..`, root) naming a file the package lists, at most `limit` bytes. A
/// package has no links, so none of the dev-folder walk applies.
fn package_file<'a>(
    files: &'a crate::PackageFiles,
    module: &ModuleName,
    path: &str,
    limit: u64,
) -> Result<&'a [u8]> {
    let plain = !path.is_empty()
        && !path.contains('\0')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(Error::InvariantViolation {
            message: format!(
                "data file path '{path}' of module {module} is not relative to the package"
            ),
        });
    }
    let Some(bytes) = files.get(path) else {
        return Err(Error::Io {
            path: PathBuf::from(path),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the package has no such file",
            ),
        });
    };
    if u64::try_from(bytes.len()).map_or(true, |len| len > limit) {
        return Err(Error::DataFileTooLarge {
            module: module.clone(),
            path: path.to_owned(),
            limit,
        });
    }
    Ok(bytes)
}

/// [`read_data_bytes`] without reading the content.
async fn check_data_file(
    entry: &AddonEntry,
    module: &ModuleName,
    path: &str,
    limit: u64,
) -> Result<()> {
    if let Some(snapshot) = entry.artifact_snapshot() {
        return snapshot_file(snapshot, module, path, limit).map(drop);
    }
    if let (AddonSourceKind::Package { .. }, Some(data)) =
        (entry.source_kind(), entry.package_data())
    {
        return package_file(&data.files, module, path, limit).map(drop);
    }
    let AddonSourceKind::DevFolder(dir) = entry.source_kind() else {
        return Err(Error::DataSourceUnsupported {
            module: module.clone(),
            source_kind: match entry.source_kind() {
                AddonSourceKind::Embedded => SourceKind::Embedded,
                _ => SourceKind::Package,
            },
        });
    };
    let (dir, module, path) = (dir.clone(), module.clone(), path.to_owned());
    tokio::task::spawn_blocking(move || check_data_file_blocking(&dir, &module, &path, limit))
        .await
        .map_err(|e| Error::InvariantViolation {
            message: format!("the data file reader task failed: {e}"),
        })?
}

/// The files of the addons of one operation, for the data loader (file-valued fields): the same
/// bounded reader as the data files themselves, and the same operation budget.
struct RunFiles {
    entries: IndexMap<ModuleName, AddonEntry>,
    limit: u64,
    operation_limit: u64,
    counter: Arc<std::sync::atomic::AtomicU64>,
}

impl RunFiles {
    fn new(
        entries: &IndexMap<ModuleName, AddonEntry>,
        limit: u64,
        operation_limit: u64,
        counter: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            entries: entries.clone(),
            limit,
            operation_limit,
            counter,
        }
    }

    /// The entry of `module` and the checked relative `path`.
    fn locate(
        &self,
        module: &str,
        path: &str,
    ) -> std::result::Result<(ModuleName, &AddonEntry), FileReadError> {
        let name = ModuleName::new(module).map_err(|e| FileReadError::Failed(e.to_string()))?;
        // a path a data file names is the author's input, not a checked manifest entry
        let plain = !path.is_empty()
            && !path.contains('\0')
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
        if !plain {
            return Err(FileReadError::Failed(format!(
                "{module}/{path}: a file named by data must be a relative path inside the module \
                 (no '..', no leading '/')"
            )));
        }
        let Some(entry) = self.entries.get(&name) else {
            return Err(FileReadError::Failed(format!(
                "module {module} is not in the addon sources of this operation"
            )));
        };
        Ok((name, entry))
    }
}

/// A reader failure as the loader's: module-relative paths only (the absolute path of the addon
/// directory is not for a data file's author or the database).
fn file_read_error(error: Error, module: &str, path: &str) -> FileReadError {
    match error {
        Error::DataFileTooLarge { limit, .. } => FileReadError::TooLarge {
            module: module.to_owned(),
            path: path.to_owned(),
            limit,
        },
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            FileReadError::NotFound {
                module: module.to_owned(),
                path: path.to_owned(),
            }
        }
        Error::Io { source, .. } => {
            FileReadError::Failed(format!("{module}/{path}: {}", source.kind()))
        }
        other => FileReadError::Failed(other.to_string()),
    }
}

impl AddonFiles for RunFiles {
    fn read<'a>(
        &'a self,
        module: &'a str,
        path: &'a str,
    ) -> erp_orm::BoxFuture<'a, std::result::Result<Vec<u8>, FileReadError>> {
        Box::pin(async move {
            let (name, entry) = self.locate(module, path)?;
            let bytes = read_data_bytes(entry, &name, path, self.limit)
                .await
                .map_err(|e| file_read_error(e, module, path))?;
            charge_data_bytes(
                &self.counter,
                self.operation_limit,
                &name,
                path,
                bytes.len(),
            )
            .map_err(|e| FileReadError::Failed(e.to_string()))?;
            Ok(bytes)
        })
    }

    fn check<'a>(
        &'a self,
        module: &'a str,
        path: &'a str,
    ) -> erp_orm::BoxFuture<'a, std::result::Result<(), FileReadError>> {
        Box::pin(async move {
            let (name, entry) = self.locate(module, path)?;
            check_data_file(entry, &name, path, self.limit)
                .await
                .map_err(|e| file_read_error(e, module, path))
        })
    }
}

/// The bytes of a data file, read with every check of [`open_data_file_blocking`].
fn read_data_file_blocking(
    dir: &Path,
    module: &ModuleName,
    path: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    let (file, full) = open_data_file_blocking(dir, module, path, limit)?;
    let Some(bytes) = crate::safe_read::read_capped(file, limit)
        .map_err(|source| Error::Io { path: full, source })?
    else {
        return Err(Error::DataFileTooLarge {
            module: module.clone(),
            path: path.to_owned(),
            limit,
        });
    };
    Ok(bytes)
}

/// Check that a data file could be read, without reading its content: the same walk, link and
/// regular-file checks, and its size against `limit` from the metadata.
fn check_data_file_blocking(dir: &Path, module: &ModuleName, path: &str, limit: u64) -> Result<()> {
    open_data_file_blocking(dir, module, path, limit).map(drop)
}

/// Open a data file of an addon after the checks: every component a real directory or file, the
/// resolved file inside the addon, a regular file with no extra hard links, at most `limit`
/// bytes by its metadata.
fn open_data_file_blocking(
    dir: &Path,
    module: &ModuleName,
    path: &str,
    limit: u64,
) -> Result<(std::fs::File, PathBuf)> {
    let io = |full: &Path, source: std::io::Error| Error::Io {
        path: full.to_path_buf(),
        source,
    };
    let symlink = || Error::DataFileSymlink {
        module: module.clone(),
        path: path.to_owned(),
    };
    // Every component below the addon directory must be a real directory or file. The manifest
    // already refuses absolute paths and `..`, and the loader's file reader refuses
    // them in names written in data files before it gets here; anything else is a broken
    // invariant.
    let mut full = dir.to_path_buf();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => full.push(part),
            Component::CurDir => continue,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::InvariantViolation {
                    message: format!(
                        "data file path '{path}' of module {module} is not relative to the addon"
                    ),
                });
            }
        }
        let meta = std::fs::symlink_metadata(&full).map_err(|e| io(&full, e))?;
        if meta.file_type().is_symlink() {
            return Err(symlink());
        }
    }
    // And the resolved file lies inside the resolved addon directory (lead review J): a link
    // swapped in after the walk above still cannot lead outside the addon.
    let root = std::fs::canonicalize(dir).map_err(|e| io(dir, e))?;
    let resolved = std::fs::canonicalize(&full).map_err(|e| io(&full, e))?;
    if !resolved.starts_with(&root) {
        return Err(symlink());
    }
    let not_regular = || {
        io(
            &full,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"),
        )
    };
    // A FIFO or a device would block the open or the read: only regular files, checked before
    // the open, and again on the open file (a file swapped in between).
    let meta = std::fs::symlink_metadata(&resolved).map_err(|e| io(&full, e))?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    let file = crate::safe_read::open_nofollow(&resolved).map_err(|e| io(&full, e))?;
    let meta = file.metadata().map_err(|e| io(&full, e))?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    // S9: a hard link to a file outside the addon looks like any other file of it.
    if let Some(links) = crate::safe_read::extra_hard_links(&meta) {
        return Err(Error::DataFileHardLinked {
            module: module.clone(),
            path: path.to_owned(),
            links,
        });
    }
    if meta.len() > limit {
        return Err(Error::DataFileTooLarge {
            module: module.clone(),
            path: path.to_owned(),
            limit,
        });
    }
    Ok((file, full))
}

/// Rows of the base metadata tables whose state the tables' check constraints would refuse (a
/// database written before the constraint existed), named before the schema update of `base`
/// adds the constraint. `ir_module_module` is checked when the catalog is read (every state is
/// parsed there); this covers `ir_module_post_commit`.
async fn check_stored_states(tx: &mut Transaction) -> Result<()> {
    const TABLE: &str = "ir_module_post_commit";
    // Checked against the model's selection by a test (`post_commit_states_match_the_model`).
    const ALLOWED: &[&str] = &[
        PostCommitState::ALL[0].as_str(),
        PostCommitState::ALL[1].as_str(),
        PostCommitState::ALL[2].as_str(),
    ];
    let exists = tx
        .query("SELECT to_regclass($1) IS NOT NULL", &[&TABLE])
        .await?;
    let exists: bool = exists
        .first()
        .ok_or_else(|| Error::InvariantViolation {
            message: "to_regclass returned no row".to_owned(),
        })?
        .try_get(0)
        .map_err(|e| Error::InvariantViolation {
            message: format!("to_regclass: {e}"),
        })?;
    if !exists {
        return Ok(());
    }
    let allowed: Vec<&str> = ALLOWED.to_vec();
    let rows = tx
        .query(
            "SELECT id, state::text FROM ir_module_post_commit \
             WHERE state IS NULL OR NOT (state = ANY($1)) ORDER BY id LIMIT $2",
            &[&allowed, &MAX_REPORTED_ROWS],
        )
        .await?;
    let mut bad = Vec::with_capacity(rows.len());
    for row in &rows {
        let shape = |e: tokio_postgres::Error| Error::InvariantViolation {
            message: format!("unexpected {TABLE} row: {e}"),
        };
        let id: i64 = row.try_get(0).map_err(shape)?;
        let state: Option<String> = row.try_get(1).map_err(shape)?;
        // A NULL state is named as NULL (the column is required, so only an older database has
        // one): the message must show it, not hide it as an empty text.
        let state = state.unwrap_or_else(|| "NULL".to_owned());
        bad.push(StoredStateRow::new(id, state));
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidStoredStates {
            table: TABLE,
            allowed: ALLOWED,
            rows: bad,
        })
    }
}

/// The stored many2many relation tables of a registry, each with the field that uses it
/// (`model.field`; the first when several fields share a table).
fn relation_owners(registry: &Registry) -> BTreeMap<String, String> {
    let mut owners = BTreeMap::new();
    for model in registry.models() {
        for field in model.fields.iter().filter(|f| f.store) {
            if let erp_fields::FieldType::Many2many {
                relation: Some(relation),
                ..
            } = &field.ty
            {
                owners
                    .entry(relation.table.clone())
                    .or_insert_with(|| format!("{}.{}", model.name, field.name));
            }
            // Every other type has no relation table (a non-stored many2many has none either).
        }
    }
    owners
}

/// What uses `table` in `registry`: a model's table, or a many2many relation table (described as
/// `relation of <model>.<field>`); `None` when nothing does.
fn table_user(
    registry: &Registry,
    relations: &BTreeMap<String, String>,
    table: &str,
) -> Option<String> {
    registry
        .models()
        .iter()
        .find(|m| m.has_table() && m.table == table)
        .map(|m| format!("model {}", m.name))
        .or_else(|| {
            relations
                .get(table)
                .map(|field| format!("the relation table of field {field}"))
        })
}

/// Remove the schema of the uninstalled modules: columns of fields that left their
/// model, then in one statement the tables of models that left the registry and the many2many
/// relation tables no field of the new registry uses. Never `CASCADE` (lead review C): when
/// another object (a view, a foreign key, an index of another module) uses one of them,
/// PostgreSQL refuses and the uninstall fails with [`Error::SchemaDropBlocked`] naming it.
/// `DROP COLUMN` itself removes the indexes and constraints of its table that use the column,
/// also those using columns that stay: those are refused first ([`refuse_shared_column_objects`]),
/// unless the removed modules declared them. Records the modules created in other models'
/// tables are purged by the data loader first.
///
/// The schema lock ([`erp_sql::ddl::SCHEMA_LOCK`]) is taken by the uninstall step before
/// its hooks and the purge, after the operation lock (and again here, which is free: the lock is
/// the transaction's already), so a schema synchronization of another process never runs between
/// the checks and the drops; the checks and the drops name tables the same way, in
/// the current schema. Returns the tables dropped.
async fn drop_removed_schema(
    tx: &mut Transaction,
    old: &Registry,
    removing: &[ModuleName],
) -> Result<Vec<String>> {
    let new = Arc::clone(tx.registry());
    // (table, column) of the columns to drop from tables that stay.
    let mut columns: Vec<(String, String)> = Vec::new();
    // Unquoted names: checked for existence first, quoted in the statement. `owners` says who
    // used each table before the uninstall.
    let mut tables: Vec<String> = Vec::new();
    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    let mut dropped = DroppedColumns::default();
    let schema = current_schema(tx).await?;
    let qualified = |table: &str| format!("{}.{}", erp_sql::quote(&schema), erp_sql::quote(table));
    for model in old.models().iter().filter(|m| m.has_generic_table()) {
        match new.model(model.name).filter(|m| m.has_generic_table()) {
            None => {
                tables.push(model.table.clone());
                owners.insert(model.table.clone(), format!("model {}", model.name));
            }
            Some(kept) => {
                let kept_columns: BTreeSet<&str> = kept.column_fields().map(|f| f.name).collect();
                for field in model.column_fields() {
                    if !kept_columns.contains(field.name)
                        && kept.field(field.name).is_none()
                        && !is_dropped_custom_field(&new, model, field)
                    {
                        columns.push((kept.table.clone(), field.name.to_owned()));
                        dropped.tables.push(qualified(&kept.table));
                        dropped.columns.push(field.name.to_owned());
                    }
                }
                // Table objects the removed modules declared on the kept table go with them.
                for object in &model.table_objects {
                    if kept.table_object(&object.full_name).is_none() {
                        dropped
                            .own_objects
                            .insert((kept.table.clone(), object.full_name.clone()));
                    }
                }
            }
        }
    }
    let kept_relations = relation_owners(&new);
    for (relation, field) in relation_owners(old) {
        if !kept_relations.contains_key(&relation) {
            owners.insert(
                relation.clone(),
                format!("the relation table of field {field}"),
            );
            tables.push(relation);
        }
    }
    // A table a model or relation that stays still maps to is never dropped: refused, naming both
    // users (two models sharing a `_table` with one removed, or a removed model whose table is
    // one of the module system's own).
    for table in &tables {
        if let Some(kept) = table_user(&new, &kept_relations, table) {
            return Err(Error::SharedSchemaTable {
                modules: removing.to_vec(),
                table: table.clone(),
                dropped: owners
                    .get(table)
                    .cloned()
                    .ok_or_else(|| Error::InvariantViolation {
                        message: format!(
                            "no owner was recorded for the table {table} being dropped"
                        ),
                    })?,
                kept,
            });
        }
    }
    if columns.is_empty() && tables.is_empty() {
        return Ok(tables);
    }
    // Operation lock first (taken at the start of the operation), then this one, then the
    // tables: the order every schema change of the module system follows.
    tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await?;
    refuse_shared_column_objects(tx, &dropped, removing).await?;
    refuse_missing_objects(tx, &schema, &tables, &columns, removing).await?;
    let mut statements: Vec<String> = columns
        .iter()
        .map(|(table, column)| {
            format!(
                "ALTER TABLE {} DROP COLUMN {}",
                qualified(table),
                erp_sql::quote(column)
            )
        })
        .collect();
    if !tables.is_empty() {
        let quoted: Vec<String> = tables.iter().map(|t| qualified(t)).collect();
        // One statement: foreign keys between the removed tables do not block each other.
        statements.push(format!("DROP TABLE {}", quoted.join(", ")));
    }
    match tx.batch(&statements.join(";\n")).await {
        Ok(()) => {
            forget_generated_provenance(tx, &schema, &tables, &columns).await?;
            Ok(tables)
        }
        Err(e) => {
            // `2BP01` (dependent objects still exist): PostgreSQL's detail names them. Without
            // the detail the database error is returned as it is.
            let blocked = (e.sqlstate() == Some("2BP01"))
                .then(|| std::error::Error::source(&e))
                .flatten()
                .and_then(|s| s.downcast_ref::<tokio_postgres::Error>())
                .and_then(tokio_postgres::Error::as_db_error)
                .and_then(|db| {
                    db.detail()
                        .map(|detail| (db.message().to_owned(), detail.replace('\n', "; ")))
                });
            match blocked {
                Some((message, detail)) => Err(Error::SchemaDropBlocked {
                    modules: removing.to_vec(),
                    message,
                    detail,
                }),
                None => Err(e.into()),
            }
        }
    }
}

/// The error of `CREATE EXTENSION <name>`: for a missing privilege (`42501`), the message names
/// what the role needs; any other error is returned as it is.
fn extension_error(name: &str, error: erp_fields::Error) -> Error {
    if error.sqlstate() != Some("42501") {
        return error.into();
    }
    Error::Orm(
        erp_fields::Error::new(
            error.kind(),
            format!(
                "creating the PostgreSQL extension {name} was refused (insufficient privilege): \
                 {name} is a trusted extension, so the owner of the database may create it \
                 (CREATE privilege on the database); run the initialization as the database \
                 owner, or create the extension as a superuser first with `CREATE EXTENSION \
                 {name}`"
            ),
        )
        .with_sqlstate(error.sqlstate().map(str::to_owned))
        .with_source(error),
    )
}

/// The schema the ORM's tables are in (`current_schema()`).
/// Remove the generated-column provenance of the dropped tables and columns in the uninstall's
/// transaction. A provenance row whose column is gone is catalog drift, so leaving it
/// would make every later schema synchronization of that table, and a reinstall, refuse.
async fn forget_generated_provenance(
    tx: &mut Transaction,
    schema: &str,
    tables: &[String],
    columns: &[(String, String)],
) -> Result<()> {
    const TABLE: &str = erp_orm::schema_generated::TABLE_NAME;
    let exists = tx
        .query(
            "SELECT to_regclass(quote_ident($1) || '.' || quote_ident($2)) IS NOT NULL",
            &[&schema, &TABLE],
        )
        .await?;
    let exists: bool = exists
        .first()
        .ok_or_else(|| Error::InvariantViolation {
            message: "to_regclass returned no row".to_owned(),
        })?
        .try_get(0)
        .map_err(|e| Error::InvariantViolation {
            message: format!("to_regclass: {e}"),
        })?;
    if !exists {
        return Ok(());
    }
    let column_tables: Vec<String> = columns.iter().map(|(table, _)| table.clone()).collect();
    let column_names: Vec<String> = columns.iter().map(|(_, column)| column.clone()).collect();
    let tables: Vec<String> = tables.to_vec();
    let schema = schema.to_owned();
    tx.query(
        &format!(
            "DELETE FROM {}.{} WHERE schema_name = $1 AND (table_name = ANY($2) \
             OR (table_name, column_name) IN (SELECT * FROM unnest($3::text[], $4::text[])))",
            erp_sql::quote(&schema),
            erp_sql::quote(TABLE)
        ),
        &[&schema, &tables, &column_tables, &column_names],
    )
    .await?;
    Ok(())
}

async fn current_schema(tx: &mut Transaction) -> Result<String> {
    let rows = tx.query("SELECT current_schema()::text", &[]).await?;
    let schema: Option<String> = rows
        .first()
        .ok_or_else(|| Error::InvariantViolation {
            message: "current_schema() returned no row".to_owned(),
        })?
        .try_get(0)
        .map_err(|e| Error::InvariantViolation {
            message: format!("current_schema(): {e}"),
        })?;
    schema.ok_or_else(|| Error::InvariantViolation {
        message: "the session has no current schema (search_path names none that exists)"
            .to_owned(),
    })
}

/// Remove the queued steps on a table the uninstall dropped: they could only fail from now on,
/// after every operation.
async fn delete_steps_of_dropped_tables(tx: &mut Transaction, tables: &[String]) -> Result<()> {
    if tables.is_empty() {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM ir_module_post_commit WHERE table_name = ANY($1)",
        &[&tables],
    )
    .await?;
    Ok(())
}

/// Whether `field` of `model` (in the registry before the uninstall) is a custom field the new
/// registry dropped (`ManualFieldPolicy::DropBroken`, e.g. related to a field of a removed
/// module): its column stays, like the columns of every dropped custom field. The
/// drop is reported on the model that declared the field; models inheriting it, directly or
/// through other models, lose it too.
fn is_dropped_custom_field(
    new: &Registry,
    model: &erp_registry::ModelSpec,
    field: &erp_registry::FieldSpec,
) -> bool {
    field.manual
        && new.dropped_fields().iter().any(|d| {
            // `inherits` walks the whole `_inherit` ancestry, so a model inheriting through an
            // intermediate model is covered too
            d.field() == field.name && model.inherits(d.model())
        })
}

/// Refuse to remove the schema when tables or columns the uninstall drops are missing (removed
/// outside ERPForAll): [`Error::SchemaTablesMissing`] / [`Error::SchemaColumnsMissing`] name them
/// all, instead of PostgreSQL's `42P01` / `42703` for the first one. `tables` are the tables to
/// drop, `columns` the (table, column) pairs to drop from tables that stay (those tables must
/// exist too); names are unquoted, in `schema`.
async fn refuse_missing_objects(
    tx: &mut Transaction,
    schema: &str,
    tables: &[String],
    columns: &[(String, String)],
    removing: &[ModuleName],
) -> Result<()> {
    let to_drop: BTreeSet<&str> = tables
        .iter()
        .map(String::as_str)
        .chain(columns.iter().map(|(t, _)| t.as_str()))
        .collect();
    let to_check: Vec<&str> = to_drop.into_iter().collect();
    let column_tables: Vec<&str> = columns.iter().map(|(t, _)| t.as_str()).collect();
    let column_names: Vec<&str> = columns.iter().map(|(_, c)| c.as_str()).collect();
    let rows = tx
        .query(
            "SELECT 'table', t FROM unnest($1::text[]) AS t \
             WHERE NOT EXISTS (SELECT 1 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = t AND n.nspname = $2) \
             UNION ALL \
             SELECT 'column', u.t || '.' || u.c FROM unnest($3::text[], $4::text[]) AS u(t, c) \
             WHERE EXISTS (SELECT 1 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = u.t AND n.nspname = $2) \
               AND NOT EXISTS (SELECT 1 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = u.t AND n.nspname = $2 AND a.attname = u.c \
                   AND a.attnum > 0 AND NOT a.attisdropped) \
             ORDER BY 1, 2",
            &[&to_check, &schema, &column_tables, &column_names],
        )
        .await?;
    let mut missing_tables = Vec::new();
    let mut missing_columns = Vec::new();
    for row in &rows {
        let shape = |e: tokio_postgres::Error| Error::InvariantViolation {
            message: format!("unexpected row of the missing-object check: {e}"),
        };
        let kind: String = row.try_get(0).map_err(shape)?;
        let name: String = row.try_get(1).map_err(shape)?;
        match kind.as_str() {
            "table" => missing_tables.push(name),
            "column" => missing_columns.push(name),
            other => {
                return Err(Error::InvariantViolation {
                    message: format!("the missing-object check returned kind '{other}'"),
                });
            }
        }
    }
    if !missing_tables.is_empty() {
        Err(Error::SchemaTablesMissing {
            modules: removing.to_vec(),
            tables: missing_tables,
        })
    } else if !missing_columns.is_empty() {
        Err(Error::SchemaColumnsMissing {
            modules: removing.to_vec(),
            columns: missing_columns,
        })
    } else {
        Ok(())
    }
}

/// The columns an uninstall drops from tables that stay, and the table objects (by table and
/// name) the removed modules declared on those tables.
#[derive(Default)]
struct DroppedColumns {
    /// Quoted table names, parallel to `columns`.
    tables: Vec<String>,
    columns: Vec<String>,
    own_objects: BTreeSet<(String, String)>,
}

/// Indexes and constraints of a table that use a column being dropped together with a column
/// that stays (R8 of the delta review): `DROP COLUMN` would remove them silently, so the
/// uninstall is refused, naming each. Objects the removed modules declared, and objects using only
/// dropped columns, go with the columns. One catalog query (`pg_depend`).
async fn refuse_shared_column_objects(
    tx: &mut Transaction,
    dropped: &DroppedColumns,
    removing: &[ModuleName],
) -> Result<()> {
    if dropped.columns.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "WITH dropped AS ( \
                 SELECT to_regclass(u.t)::oid AS rel, u.c AS col \
                 FROM unnest($1::text[], $2::text[]) AS u(t, c) \
             ), uses AS ( \
                 SELECT d.classid, d.objid, d.refobjid AS rel, a.attname::text AS col, \
                        (d.refobjid, a.attname::text) IN (SELECT rel, col FROM dropped) AS gone \
                 FROM pg_depend d \
                 JOIN pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid \
                 WHERE d.refclassid = 'pg_class'::regclass \
                   AND d.refobjid IN (SELECT rel FROM dropped) \
                   AND d.refobjsubid > 0 \
                   AND d.classid IN ('pg_class'::regclass, 'pg_constraint'::regclass) \
             ) \
             SELECT CASE WHEN u.classid = 'pg_class'::regclass THEN 'index' \
                         ELSE 'constraint' END, \
                    coalesce(c.relname, k.conname)::text, t.relname::text, \
                    array_agg(DISTINCT u.col ORDER BY u.col) \
             FROM uses u \
             JOIN pg_class t ON t.oid = u.rel \
             LEFT JOIN pg_class c ON u.classid = 'pg_class'::regclass AND c.oid = u.objid \
             LEFT JOIN pg_constraint k \
                    ON u.classid = 'pg_constraint'::regclass AND k.oid = u.objid \
             GROUP BY u.classid, u.objid, t.relname, c.relname, k.conname \
             HAVING bool_or(u.gone) AND NOT bool_and(u.gone) \
             ORDER BY 1, 2, 3",
            &[&dropped.tables, &dropped.columns],
        )
        .await?;
    let mut blocking: Vec<String> = Vec::new();
    for row in &rows {
        let shape = |e: tokio_postgres::Error| Error::InvariantViolation {
            message: format!("unexpected catalog row of a dropped column's dependents: {e}"),
        };
        let kind: String = row.try_get(0).map_err(shape)?;
        let name: String = row.try_get(1).map_err(shape)?;
        let table: String = row.try_get(2).map_err(shape)?;
        let columns: Vec<String> = row.try_get(3).map_err(shape)?;
        if dropped.own_objects.contains(&(table.clone(), name.clone())) {
            continue;
        }
        blocking.push(format!("{kind} {name} on {table} ({})", columns.join(", ")));
    }
    if blocking.is_empty() {
        Ok(())
    } else {
        Err(Error::SchemaDropBlocked {
            modules: removing.to_vec(),
            message: "cannot drop columns that other objects use together with columns that \
                      stay"
                .to_owned(),
            detail: blocking.join("; "),
        })
    }
}

/// Longest database name PostgreSQL keeps (`NAMEDATALEN - 1` bytes); longer names would be
/// truncated silently, so they are refused.
pub const MAX_DATABASE_NAME_BYTES: usize = 63;

/// ```
/// use erp_modules::system::DatabaseName;
/// assert_eq!(DatabaseName::parse("erp_prod-1").unwrap().as_str(), "erp_prod-1");
/// assert!(DatabaseName::parse("-nope").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DatabaseName(String);

impl DatabaseName {
    /// Check and wrap a database name.
    ///
    /// # Errors
    /// [`Error::InvalidDatabaseName`] with the rule the name breaks.
    pub fn parse(name: &str) -> Result<Self> {
        let invalid = |reason: String| Error::InvalidDatabaseName {
            name: name.to_owned(),
            reason,
        };
        let mut chars = name.chars();
        match chars.next() {
            None => return Err(invalid("the name is empty".to_owned())),
            Some(first) if !first.is_ascii_alphanumeric() => {
                return Err(invalid(
                    "the first character must be a letter or a digit (a-z, A-Z, 0-9)".to_owned(),
                ));
            }
            Some(_) => {}
        }
        let rest = chars.as_str();
        if rest.is_empty() {
            return Err(invalid("the name needs at least two characters".to_owned()));
        }
        if let Some(bad) = rest
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
        {
            return Err(invalid(format!(
                "{bad:?} is not allowed: use a-z, A-Z, 0-9, '_', '.', '-'"
            )));
        }
        if name.len() > MAX_DATABASE_NAME_BYTES {
            return Err(invalid(format!(
                "the name is {} bytes; PostgreSQL keeps {MAX_DATABASE_NAME_BYTES} and would \
                 truncate the rest",
                name.len()
            )));
        }
        Ok(Self(name.to_owned()))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for DatabaseName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DatabaseName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// # Errors
/// [`Error::InvalidDatabaseName`].
pub fn validate_database_name(name: &str) -> Result<DatabaseName> {
    DatabaseName::parse(name)
}

/// Create an empty database for ERPForAll: `C` collation from
/// `template0`, UTF-8. Then build a [`ModuleSystem`] on it and call [`ModuleSystem::initialize`]
/// (which installs `pg_trgm`, `base` and the auto-install set). `maintenance` is a pool on an
/// administration database of the same cluster.
///
/// Creation and initialization are separate steps, as in PostgreSQL: when
/// [`ModuleSystem::initialize`] fails, the database exists but is empty
/// ([`Error::DatabaseNotInitialized`] on load); initialize it again or drop it.
///
/// # Errors
/// [`Error::DatabaseExists`]; database errors. (An invalid name cannot get here: it is refused by
/// [`DatabaseName::parse`].)
pub async fn create_database(maintenance: &Pool, name: &DatabaseName) -> Result<()> {
    let client = maintenance.get().await.map_err(|e| {
        Error::Orm(
            erp_fields::Error::new(
                erp_fields::ErrorKind::Database,
                "could not get a connection to the maintenance database",
            )
            .with_source(e),
        )
    })?;
    let db_error = |e: tokio_postgres::Error| {
        Error::Orm(
            erp_fields::Error::new(erp_fields::ErrorKind::Database, format!("{e}"))
                .with_sqlstate(e.code().map(|c| c.code().to_owned())),
        )
    };
    let exists = client
        .query(
            "SELECT 1 FROM pg_database WHERE datname = $1",
            &[&name.as_str()],
        )
        .await
        .map_err(db_error)?;
    if !exists.is_empty() {
        return Err(Error::DatabaseExists { name: name.clone() });
    }
    let sql = format!(
        "CREATE DATABASE {} ENCODING 'unicode' LC_COLLATE 'C' TEMPLATE template0",
        erp_sql::quote(name.as_str())
    );
    match client.batch_execute(&sql).await {
        Ok(()) => Ok(()),
        // Created by someone else between the check and the statement: `42P04`, or `23505` on
        // `pg_database`'s unique index when both CREATE DATABASE statements ran at once.
        Err(e) if matches!(e.code().map(|c| c.code()), Some("42P04" | "23505")) => {
            Err(Error::DatabaseExists { name: name.clone() })
        }
        Err(e) => Err(db_error(e)),
    }
}

/// The addons of a browser installation catalogue (amendment).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HumanCatalog {
    /// Manifests of the addons that may be installed from the browser, by name.
    pub offered: Vec<Arc<Manifest>>,
    /// Addons of the directory that are not offered, with the reason.
    pub refused: Vec<(ModuleName, String)>,
}

/// Most entries read while checking an addon folder, and its deepest nesting: a hostile
/// folder cannot make the check run long.
const MAX_ADDON_ENTRIES: usize = 4096;
const MAX_ADDON_DEPTH: usize = 8;
/// The directories of a data-only addon that hold data files (`.xml`, `.csv`).
const DATA_DIRS: [&str; 4] = ["views", "data", "demo", "security"];

/// Why an addon that is not linked cannot be installed from the browser, or `None` when it is a
/// data-only addon of the addon directory (amendment).
///
/// This is an allowlist. The folder may hold only: `manifest.toml`; `models/*.toml`; `.xml` and
/// `.csv` files under `views/`, `data/`, `demo/` and `security/`; and `README*` text files. Every
/// other entry is refused, with the path and the rule in the message: symlinks, `.sql` files,
/// `migrations/` (their SQL runs at upgrade), `static/` (browser code needs the asset pipeline),
/// `Cargo.toml`, `src/`, `build.rs`, and any other name or file kind. The manifest must name no
/// native hook, and every `data`/`demo` path must be a relative `.xml`/`.csv` path inside the
/// folder.
/// The reason a dev folder is not a native addon when its manifest has a `[wasm]` section: it is
/// the source of a package addon, installed from its `.erpaddon` with `erp addon install`.
const WASM_DEV_FOLDER: &str = "manifest.toml has a [wasm] section: this folder is the source of a WebAssembly \
     package addon, not a native or data-only addon; build and package it and install the \
     .erpaddon with `erp addon install`";

/// Refuse a native install of an addon whose only source is a dev folder with a `[wasm]` section.
/// (The lifecycle of a package installs the package entry, which wins over such a folder.)
fn refuse_wasm_dev_folders(
    entries: &IndexMap<ModuleName, AddonEntry>,
    names: &[ModuleName],
) -> Result<()> {
    for name in names {
        if let Some(entry) = entries.get(name)
            && matches!(entry.source_kind(), AddonSourceKind::DevFolder(_))
            && entry.manifest().wasm().is_some()
        {
            return Err(Error::WasmAddon {
                addon: name.clone(),
                reason: crate::error::WasmReason::State(WASM_DEV_FOLDER.to_owned()),
            });
        }
    }
    Ok(())
}

fn data_only_refusal(entry: &AddonEntry) -> Option<String> {
    let AddonSourceKind::DevFolder(dir) = entry.source_kind() else {
        return Some("it does not come from the configured addon directory".to_owned());
    };
    if entry.manifest().wasm().is_some() {
        return Some(WASM_DEV_FOLDER.to_owned());
    }
    let manifest = entry.manifest();
    for (key, hook) in [
        ("pre_init_hook", manifest.pre_init_hook()),
        ("post_init_hook", manifest.post_init_hook()),
        ("uninstall_hook", manifest.uninstall_hook()),
    ] {
        if hook.is_some() {
            return Some(format!(
                "manifest.toml declares `{key}`: a hook needs native code (rule: no native hooks)"
            ));
        }
    }
    if !manifest.installable() {
        return Some("manifest.toml says installable = false".to_owned());
    }
    for (key, paths) in [("data", manifest.data()), ("demo", manifest.demo())] {
        for path in paths {
            let relative = Path::new(path);
            let inside = relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
                && relative.components().count() >= 2;
            if !inside {
                return Some(format!(
                    "manifest.toml `{key}` path `{path}` leaves the addon folder (rule: data and demo paths are relative paths inside the folder)"
                ));
            }
            let first = relative
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str());
            if !first.is_some_and(|f| DATA_DIRS.contains(&f)) {
                return Some(format!(
                    "manifest.toml `{key}` path `{path}` is outside views/, data/, demo/ and security/ (rule: data files live in those directories)"
                ));
            }
            if !matches!(
                relative.extension().and_then(|e| e.to_str()),
                Some("xml" | "csv")
            ) {
                return Some(format!(
                    "manifest.toml `{key}` path `{path}` is not an .xml or .csv file (rule: data files are .xml or .csv)"
                ));
            }
        }
    }
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Some(format!(
                "{} is not a plain directory (rule: no symlinks)",
                dir.display()
            ));
        }
        Err(e) => return Some(format!("{} cannot be read: {e}", dir.display())),
    }
    let mut seen = 0usize;
    check_addon_dir(dir, dir, 0, &mut seen)
}

/// The refusal rule for a folder holding a Rust crate: it is a native addon, and this binary was
/// not built with it, so neither the native nor the data-only route can install it.
const NOT_LINKED_NATIVE: &str = "a native addon's Rust crate: this binary does not link the addon";

/// The allowlist walk of [`data_only_refusal`]. `top` is the addon folder, `at` the directory read.
fn check_addon_dir(top: &Path, at: &Path, depth: usize, seen: &mut usize) -> Option<String> {
    let listing = match fs::read_dir(at) {
        Ok(listing) => listing,
        Err(e) => return Some(format!("{} cannot be read: {e}", at.display())),
    };
    for item in listing {
        let item = match item {
            Ok(item) => item,
            Err(e) => return Some(format!("{} cannot be read: {e}", at.display())),
        };
        *seen += 1;
        if *seen > MAX_ADDON_ENTRIES || depth >= MAX_ADDON_DEPTH {
            return Some(format!(
                "{} is too large or too deep (rule: at most {MAX_ADDON_ENTRIES} entries, {MAX_ADDON_DEPTH} levels)",
                top.display()
            ));
        }
        let path = item.path();
        let shown = path
            .strip_prefix(top)
            .unwrap_or(&path)
            .display()
            .to_string();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => return Some(format!("{shown} cannot be read: {e}")),
        };
        if meta.file_type().is_symlink() {
            return Some(format!("{shown} is a symlink (rule: no symlinks)"));
        }
        let Some(name) = item.file_name().to_str().map(str::to_owned) else {
            return Some(format!(
                "{shown} has a name that is not UTF-8 (rule: allowlisted names only)"
            ));
        };
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let in_dir = at
            .strip_prefix(top)
            .ok()
            .and_then(|p| p.components().next())
            .and_then(|c| c.as_os_str().to_str().map(str::to_owned));
        if extension == "sql" {
            return Some(format!("{shown} is a SQL script (rule: no .sql files)"));
        }
        let reason = |rule: &str| Some(format!("{shown} is not allowed (rule: {rule})"));
        match in_dir.as_deref() {
            None => {
                // top level
                if meta.is_dir() {
                    match name.as_str() {
                        "models" => {}
                        d if DATA_DIRS.contains(&d) => {}
                        "migrations" => {
                            return reason("no migrations/ directory: its SQL runs at upgrade");
                        }
                        "static" => {
                            return reason(
                                "no static/ directory: browser code needs the asset pipeline",
                            );
                        }
                        "src" => return reason(NOT_LINKED_NATIVE),
                        _ => {
                            return reason(
                                "only models/, views/, data/, demo/ and security/ directories",
                            );
                        }
                    }
                } else if meta.is_file() {
                    let readme = name.starts_with("README")
                        && matches!(extension, "md" | "rst" | "txt" | "");
                    match name.as_str() {
                        "manifest.toml" => {}
                        "Cargo.toml" | "build.rs" => return reason(NOT_LINKED_NATIVE),
                        _ if readme => {}
                        _ => return reason("only manifest.toml and README files at the top level"),
                    }
                } else {
                    return reason("only regular files and directories");
                }
            }
            Some("models") => {
                if !(meta.is_file() && extension == "toml") || depth != 1 {
                    return reason("models/ holds only .toml files");
                }
            }
            Some(_) => {
                if meta.is_file() {
                    if !matches!(extension, "xml" | "csv") {
                        return reason("data directories hold only .xml and .csv files");
                    }
                } else if !meta.is_dir() {
                    return reason("only regular files and directories");
                }
            }
        }
        if meta.is_dir()
            && let Some(reason) = check_addon_dir(top, &path, depth + 1, seen)
        {
            return Some(reason);
        }
    }
    None
}

#[cfg(test)]
mod drop_schema_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use erp_registry::{
        AddonDescriptor, DropReason, FieldDecl, FieldDef, KindDef, ManualFieldPolicy, ModelDecl,
        Overlay, RegistryBuilder,
    };
    use erp_testkit::TestDb;

    use super::*;

    static T9_A: AddonDescriptor = AddonDescriptor {
        name: "t9_a",
        depends: &["base"],
        exact_only: false,
    };
    static T9_B: AddonDescriptor = AddonDescriptor {
        name: "t9_b",
        depends: &["t9_a"],
        exact_only: false,
    };
    /// `t9.thing` of `t9_a`, `t9.copy` inheriting it under a new name (its own table, with the
    /// parent's custom fields), `t9.grand` inheriting `t9.copy` (the field reaches it through an
    /// intermediate model), and `t9_b`'s extension of `t9.thing` adding `b_code`.
    static DECLS: [ModelDecl; 4] = [
        ModelDecl::new("t9_a", "t9.thing")
            .description("Thing")
            .fields(const { &[FieldDecl::char("name")] }),
        ModelDecl::new("t9_a", "t9.copy")
            .description("Copy")
            .inherit(&["t9.thing"]),
        ModelDecl::new("t9_a", "t9.grand")
            .description("Grand")
            .inherit(&["t9.copy"]),
        ModelDecl::extension("t9_b", "t9.thing", &["t9.thing"])
            .fields(const { &[FieldDecl::char("b_code")] }),
    ];

    /// The columns of `table`, sorted.
    async fn columns(tx: &mut Transaction, table: &str) -> Vec<String> {
        tx.query(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 ORDER BY 1",
            &[&table],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
    }

    /// review (9): a custom field the new registry drops (`DropBroken`) keeps its
    /// column when its model stays: `x_b_code` is related to `t9_b`'s `b_code`, so uninstalling
    /// `t9_b` drops it from the registry, and the uninstall removes `b_code` only. A custom field
    /// the new registry keeps (`x_note`) is untouched too. The same holds on `t9.copy`, which has
    /// the parent's fields in its own table, and on `t9.grand`, which inherits `t9.copy` (the drop
    /// is reported on `t9.thing` only).
    #[tokio::test]
    async fn uninstall_keeps_the_columns_of_dropped_custom_fields() {
        let mut overlay = Overlay::new();
        overlay
            .add_manual_field(
                "t9.thing",
                FieldDef::new("x_b_code", KindDef::Char { size: None })
                    .related("b_code")
                    .store(true),
            )
            .unwrap();
        overlay
            .add_manual_field(
                "t9.thing",
                FieldDef::new("x_note", KindDef::Char { size: None }),
            )
            .unwrap();
        let old = Arc::new(
            RegistryBuilder::new([&T9_A, &T9_B])
                .linked(false)
                .with_decls(DECLS.iter())
                .overlay(&overlay)
                .build()
                .unwrap(),
        );
        let new = Arc::new(
            RegistryBuilder::new([&T9_A])
                .linked(false)
                .with_decls(DECLS[..3].iter())
                .overlay(&overlay)
                .manual_fields(ManualFieldPolicy::DropBroken)
                .build()
                .unwrap(),
        );
        let dropped: Vec<(&str, &str, &DropReason)> = new
            .dropped_fields()
            .iter()
            .map(|d| (d.model(), d.field(), d.reason()))
            .collect();
        assert_eq!(
            dropped,
            [(
                "t9.thing",
                "x_b_code",
                &DropReason::Related {
                    path: "b_code".into()
                }
            )]
        );

        let db = TestDb::create().await.unwrap();
        let old_orm = Orm::builder(Arc::clone(&old), db.pool().clone())
            .build()
            .unwrap();
        let mut tx = old_orm.begin(SUPERUSER_ID).await.unwrap();
        tx.batch("CREATE EXTENSION IF NOT EXISTS pg_trgm")
            .await
            .unwrap();
        tx.init_models(&[]).await.unwrap();
        tx.commit().await.unwrap();

        let new_orm = old_orm.rebuild(new).build().unwrap();
        let mut tx = old_orm.begin(SUPERUSER_ID).await.unwrap();
        tx.switch_orm(&new_orm).await.unwrap();
        drop_removed_schema(&mut tx, &old, &[ModuleName::new("t9_b").unwrap()])
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let mut tx = new_orm.begin(SUPERUSER_ID).await.unwrap();
        for table in ["t9_thing", "t9_copy", "t9_grand"] {
            let kept = columns(&mut tx, table).await;
            assert!(kept.iter().any(|c| c == "x_b_code"), "{table}: {kept:?}");
            assert!(kept.iter().any(|c| c == "x_note"), "{table}: {kept:?}");
            assert!(!kept.iter().any(|c| c == "b_code"), "{table}: {kept:?}");
        }
        tx.rollback().await.unwrap();
        drop(tx);
        drop((old_orm, new_orm));
        db.cleanup().await.unwrap();
    }

    static T8_A: AddonDescriptor = AddonDescriptor {
        name: "t8_a",
        depends: &["base"],
        exact_only: false,
    };
    static T8_B: AddonDescriptor = AddonDescriptor {
        name: "t8_b",
        depends: &["t8_a"],
        exact_only: false,
    };
    /// `t8_a` declares `flag` without a column; `t8_b` makes it stored and adds `gone`.
    static DECLS_FLAG: [ModelDecl; 2] = [
        ModelDecl::new("t8_a", "t8.thing")
            .description("Thing")
            .fields(const { &[FieldDecl::char("flag").store(false)] }),
        ModelDecl::extension("t8_b", "t8.thing", &["t8.thing"])
            .fields(const { &[FieldDecl::char("flag").store(true), FieldDecl::char("gone")] }),
    ];

    /// review A8: a field a module that stays still declares by name keeps its
    /// column when it is no longer stored (the uninstalled module made it stored): the column
    /// of `flag` stays, the column of `gone`, declared by the uninstalled module only, goes.
    #[tokio::test]
    async fn uninstall_keeps_the_column_of_a_field_that_stays_declared() {
        let old = Arc::new(
            RegistryBuilder::new([&T8_A, &T8_B])
                .linked(false)
                .with_decls(DECLS_FLAG.iter())
                .build()
                .unwrap(),
        );
        let new = Arc::new(
            RegistryBuilder::new([&T8_A])
                .linked(false)
                .with_decls(DECLS_FLAG[..1].iter())
                .build()
                .unwrap(),
        );
        assert!(
            old.model("t8.thing")
                .unwrap()
                .column_fields()
                .any(|f| f.name == "flag")
        );
        assert!(
            !new.model("t8.thing")
                .unwrap()
                .column_fields()
                .any(|f| f.name == "flag")
        );
        assert!(new.model("t8.thing").unwrap().field("flag").is_some());

        let db = TestDb::create().await.unwrap();
        let old_orm = Orm::builder(Arc::clone(&old), db.pool().clone())
            .build()
            .unwrap();
        let mut tx = old_orm.begin(SUPERUSER_ID).await.unwrap();
        tx.init_models(&[]).await.unwrap();
        tx.commit().await.unwrap();

        let new_orm = old_orm.rebuild(new).build().unwrap();
        let mut tx = old_orm.begin(SUPERUSER_ID).await.unwrap();
        tx.switch_orm(&new_orm).await.unwrap();
        let dropped = drop_removed_schema(&mut tx, &old, &[ModuleName::new("t8_b").unwrap()])
            .await
            .unwrap();
        assert!(dropped.is_empty(), "no table was dropped: {dropped:?}");
        tx.commit().await.unwrap();

        let mut tx = new_orm.begin(SUPERUSER_ID).await.unwrap();
        let kept = columns(&mut tx, "t8_thing").await;
        assert!(kept.iter().any(|c| c == "flag"), "{kept:?}");
        assert!(!kept.iter().any(|c| c == "gone"), "{kept:?}");
        tx.rollback().await.unwrap();
        drop(tx);
        drop((old_orm, new_orm));
        db.cleanup().await.unwrap();
    }

    static T7_A: AddonDescriptor = AddonDescriptor {
        name: "t7_a",
        depends: &["base"],
        exact_only: false,
    };
    static T7_B: AddonDescriptor = AddonDescriptor {
        name: "t7_b",
        depends: &["t7_a"],
        exact_only: false,
    };
    /// `t7_b` adds the column `gone` to `t7.thing` and its own model `t7.extra`.
    static DECLS_PROVENANCE: [ModelDecl; 3] = [
        ModelDecl::new("t7_a", "t7.thing")
            .description("Thing")
            .fields(const { &[FieldDecl::char("keep")] }),
        ModelDecl::extension("t7_b", "t7.thing", &["t7.thing"])
            .fields(const { &[FieldDecl::char("gone")] }),
        ModelDecl::new("t7_b", "t7.extra")
            .description("Extra")
            .fields(const { &[FieldDecl::char("note")] }),
    ];

    /// (lead review, MUST 1): the uninstall removes the
    /// generated-column provenance of the columns and tables it drops in its own transaction, and
    /// keeps the rows of columns that stay.
    #[tokio::test]
    async fn uninstall_removes_the_generated_provenance_of_dropped_columns_and_tables() {
        let old = Arc::new(
            RegistryBuilder::new([&T7_A, &T7_B])
                .linked(false)
                .with_decls(DECLS_PROVENANCE.iter())
                .build()
                .unwrap(),
        );
        let new = Arc::new(
            RegistryBuilder::new([&T7_A])
                .linked(false)
                .with_decls(DECLS_PROVENANCE[..1].iter())
                .build()
                .unwrap(),
        );
        let db = TestDb::create().await.unwrap();
        let old_orm = Orm::builder(Arc::clone(&old), db.pool().clone())
            .build()
            .unwrap();
        let mut tx = old_orm.begin(SUPERUSER_ID).await.unwrap();
        tx.init_models(&[]).await.unwrap();
        tx.batch(erp_orm::schema_generated::GENERATED_DDL)
            .await
            .unwrap();
        // Rows as the schema synchronization writes them; only their identity matters here.
        tx.batch(
            "INSERT INTO erp_schema_generated (table_name, column_name, format_version, \
             declaration_signature, catalog_expression, catalog_type, catalog_kind) VALUES \
             ('t7_thing', 'keep', 2, 's', 'e', 'varchar', 's'), \
             ('t7_thing', 'gone', 2, 's', 'e', 'varchar', 's'), \
             ('t7_extra', 'note', 2, 's', 'e', 'varchar', 's')",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let new_orm = old_orm.rebuild(new).build().unwrap();
        let mut tx = old_orm.begin(SUPERUSER_ID).await.unwrap();
        tx.switch_orm(&new_orm).await.unwrap();
        let dropped = drop_removed_schema(&mut tx, &old, &[ModuleName::new("t7_b").unwrap()])
            .await
            .unwrap();
        assert_eq!(dropped, ["t7_extra"]);
        tx.commit().await.unwrap();

        let mut tx = new_orm.begin(SUPERUSER_ID).await.unwrap();
        let rows = tx
            .query(
                "SELECT table_name || '.' || column_name FROM erp_schema_generated ORDER BY 1",
                &[],
            )
            .await
            .unwrap();
        let left: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        assert_eq!(left, ["t7_thing.keep"]);
        tx.rollback().await.unwrap();
        drop(tx);
        drop((old_orm, new_orm));
        db.cleanup().await.unwrap();
    }
}

#[cfg(test)]
mod unit_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn pending_modules_are_the_unconsumed_load_order_suffix() {
        let names: Vec<ModuleName> = ["installed_first", "upgraded_next", "upgraded_last"]
            .into_iter()
            .map(|name| ModuleName::new(name).unwrap())
            .collect();
        let after_install = pending_after(&names, &names[0]).unwrap();
        assert_eq!(after_install, &names[1..]);
        let after_upgrade = pending_after(after_install, &names[1]).unwrap();
        assert_eq!(after_upgrade, &names[2..]);
        assert!(pending_after(after_upgrade, &names[2]).unwrap().is_empty());
    }

    #[test]
    fn pending_module_order_mismatches_are_invariants() {
        let first = ModuleName::new("first").unwrap();
        let second = ModuleName::new("second").unwrap();
        let names = [first];
        for (pending, expected) in [
            (
                names.as_slice(),
                "Internal invariant broken: finished module second, but the next module in the data load order is first",
            ),
            (
                &[][..],
                "Internal invariant broken: finished module second after the data load order was exhausted",
            ),
        ] {
            let error = pending_after(pending, &second).unwrap_err();
            assert!(matches!(error, Error::InvariantViolation { .. }));
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn invariant_marker_precedes_the_demo_error_kind() {
        use erp_data::load::{LoadError, LoadErrorKind};
        for kind in [
            erp_fields::ErrorKind::Validation,
            erp_fields::ErrorKind::UserError,
            erp_fields::ErrorKind::Missing,
            erp_fields::ErrorKind::Access,
        ] {
            let error = erp_fields::Error::new(kind, "broken contract")
                .with_condition(erp_fields::Condition::InvariantViolation);
            assert_eq!(error.kind(), kind);
            assert_eq!(error.message(), "broken contract");
            assert!(error.is_invariant_violation());
            assert!(!is_data_failure(&Error::DataLoad(
                LoadError::without_place(LoadErrorKind::Orm(error))
            )));
        }
    }

    /// Which kinds of loader error the demo savepoint absorbs: the data's own mistakes yes; a
    /// missing feature, a loader defect, an expression that needs an unbound host name and a
    /// resource limit no.
    #[test]
    fn the_absorbed_loader_kinds_are_exactly_the_datas_own_mistakes() {
        use erp_data::load::{LoadError, LoadErrorKind as K};
        let load = |kind: K| Error::DataLoad(LoadError::without_place(kind));
        let eval = |error: erp_expr::ExprError| {
            load(K::Eval {
                expr: "x".to_owned(),
                error,
            })
        };
        // absorbed
        assert!(is_data_failure(&eval(erp_expr::ExprError::name_error("x"))));
        assert!(is_data_failure(&load(K::SearchNeedsModel)));
        assert!(is_data_failure(&load(K::ExternalIdNotFound {
            xid: "m.x".to_owned()
        })));
        // not absorbed
        assert!(!is_data_failure(&eval(erp_expr::ExprError::unavailable(
            "obj", "the host"
        ))));
        assert!(!is_data_failure(&eval(erp_expr::ExprError::limit(
            "too deep"
        ))));
        assert!(!is_data_failure(&load(K::Unsupported {
            what: "x".to_owned(),
            slice: "M9".to_owned()
        })));
        assert!(!is_data_failure(&load(K::ModelArrivesLater {
            what: "<menuitem>".to_owned(),
            model: "ir.ui.menu".to_owned(),
            slice: "a later release".to_owned()
        })));
        assert!(!is_data_failure(&load(K::Internal("broken".to_owned()))));
        assert!(!is_data_failure(&load(K::Orm(
            erp_fields::Error::internal("broken invariant")
        ))));
        assert!(!is_data_failure(&Error::Orm(erp_fields::Error::internal(
            "broken invariant"
        ))));
        // a Programming error is the data's, unless it is a broken contract between layers
        // (review F-1)
        assert!(is_data_failure(&load(K::Orm(
            erp_fields::Error::programming("Invalid field 'x' on 'test.cat'")
        ))));
        assert!(!is_data_failure(&load(K::Orm(
            erp_fields::Error::invariant("the load state is not the loader's")
        ))));
        assert!(!is_data_failure(&Error::Orm(erp_fields::Error::invariant(
            "the load state is not the loader's"
        ))));
    }

    /// A demo `file=` that fails to read for a reason other than "not found" or "too large" (a
    /// permission, a failing disk) is the host's failure: the operation fails, the demo data is
    /// not blamed.
    #[test]
    fn a_host_read_failure_is_not_a_demo_data_failure() {
        use erp_data::load::{FileReadError, LoadError, LoadErrorKind};
        let load = |error: FileReadError| {
            Error::DataLoad(LoadError::without_place(LoadErrorKind::File(error)))
        };
        assert!(is_data_failure(&load(FileReadError::NotFound {
            module: "m".to_owned(),
            path: "a".to_owned()
        })));
        assert!(!is_data_failure(&load(FileReadError::Failed(
            "m/a: permission denied".to_owned()
        ))));
    }

    /// The states the queue code binds are the selection of the model and the values its check
    /// constraint lists.
    #[test]
    fn post_commit_states_match_the_model() {
        let selection: Vec<&str> = base_models::POST_COMMIT_STATES
            .iter()
            .map(|(value, _)| *value)
            .collect();
        let used: Vec<&str> = PostCommitState::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(used, selection);
        // the open states are exactly the ones a step can still run from
        assert_eq!(OPEN_STATES, ["pending", "failed"]);
        // no wildcard: a new state does not compile until it is placed here and in ALL
        for state in PostCommitState::ALL {
            match state {
                PostCommitState::Pending | PostCommitState::Failed => {
                    assert!(OPEN_STATES.contains(&state.as_str()));
                }
                PostCommitState::Done => assert!(!OPEN_STATES.contains(&state.as_str())),
            }
        }
    }

    #[test]
    fn the_pass_budget_stops_the_ninth_pass() {
        let mut passes = 0;
        for _ in 0..MAX_POST_COMMIT_PASSES {
            count_pass(&mut passes).unwrap();
        }
        assert_eq!(passes, MAX_POST_COMMIT_PASSES);
        assert_eq!(
            count_pass(&mut passes).unwrap_err(),
            PostCommitStop::PassLimit {
                passes: MAX_POST_COMMIT_PASSES
            }
        );
        assert_eq!(
            passes, MAX_POST_COMMIT_PASSES,
            "a refused pass is not counted"
        );
    }

    #[test]
    fn post_commit_keys_are_bounded_and_restricted() {
        let long_enough = "a".repeat(MAX_POST_COMMIT_KEY_BYTES);
        assert_eq!(
            PostCommitKey::parse(&long_enough).unwrap().as_str(),
            long_enough
        );
        let too_long = "a".repeat(MAX_POST_COMMIT_KEY_BYTES + 1);
        for bad in ["", "Upper", "a b", "a/b", "é", too_long.as_str()] {
            let err = PostCommitKey::parse(bad).unwrap_err();
            assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                err.message(),
                format!(
                    "invalid post-commit key {bad:?}: 1 to {MAX_POST_COMMIT_KEY_BYTES} bytes of \
                     a-z, 0-9, '_', '.', '-'"
                )
            );
        }
        for good in ["a", "0", "a_b.c-d", "refresh.stats-1"] {
            assert_eq!(PostCommitKey::parse(good).unwrap().to_string(), good);
        }
    }

    fn lock_error() -> Error {
        Error::Orm(
            erp_fields::Error::lock("could not obtain lock")
                .with_sqlstate(Some("55P03".to_owned())),
        )
    }

    fn cancel_error() -> Error {
        Error::Orm(
            erp_fields::Error::db("canceling statement").with_sqlstate(Some("57014".to_owned())),
        )
    }

    /// A5: the schema step lowers both limits to its own bounds and keeps lower ones, and an error
    /// is typed with the limits of the stage it came from.
    #[test]
    fn errors_report_the_limits_in_force() {
        let operation = Limits {
            lock: Duration::from_secs(30),
            statement: Duration::from_secs(30 * 60),
        };
        let schema = operation.during_schema_step();
        assert_eq!(schema.lock, Duration::from_secs(10));
        assert_eq!(schema.statement, Duration::from_secs(5 * 60));
        let lower = Limits {
            lock: Duration::from_millis(200),
            statement: Duration::from_secs(60),
        };
        assert_eq!(lower.during_schema_step(), lower);

        assert!(matches!(
            operation.classify(lock_error()),
            Error::LockNotAvailable { lock_timeout, .. } if lock_timeout == Duration::from_secs(30)
        ));
        assert!(matches!(
            schema.classify(lock_error()),
            Error::LockNotAvailable { lock_timeout, .. } if lock_timeout == Duration::from_secs(10)
        ));
        assert!(matches!(
            schema.classify(cancel_error()),
            Error::StatementCanceled { statement_timeout, .. }
                if statement_timeout == Duration::from_secs(5 * 60)
        ));
        // through a step wrapper, with the limits of the stage that wrapped it
        let wrapped = Error::StepFailed {
            module: ModuleName::new("m").unwrap(),
            step: ModuleStep::Schema,
            file: None,
            source: Box::new(lock_error()),
        };
        let Error::StepFailed { source, .. } = schema.classify(wrapped) else {
            panic!("the wrapper is kept");
        };
        assert!(matches!(
            *source,
            Error::LockNotAvailable { lock_timeout, .. } if lock_timeout == Duration::from_secs(10)
        ));
        // an error that is neither stays as it is
        assert!(matches!(
            operation.classify(Error::Orm(erp_fields::Error::programming("x"))),
            Error::Orm(_)
        ));
    }

    /// A serialization failure or deadlock in a module operation (a user changed a view tree the
    /// operation's view check locks, review A3-D2) is reported as a concurrent change
    /// to retry, outermost, keeping where it happened as the source: directly, from the data
    /// loader, through a step and through the purge wrapper (review A4-R2). The error of a reset
    /// after a failure gets no retry advice, and a failed reset stays outermost.
    #[test]
    fn a_concurrent_change_asks_for_a_retry() {
        use crate::error::Conflict;
        let operation = Limits {
            lock: Duration::from_secs(30),
            statement: Duration::from_secs(30 * 60),
        };
        let conflict = |state: &str| {
            erp_fields::Error::new(
                erp_fields::ErrorKind::Concurrency,
                "could not serialize access due to concurrent update",
            )
            .with_sqlstate(Some(state.to_owned()))
        };
        let loader = |state: &str| {
            Error::DataLoad(erp_data::load::LoadError::without_place(
                erp_data::load::LoadErrorKind::Orm(conflict(state)),
            ))
        };
        let serialization = "A module operation conflicted with a concurrent transaction \
                             (serialization failure, SQLSTATE 40001): nothing of it was \
                             committed; retry the module operation";
        let direct = operation.classify(Error::Orm(conflict("40001")));
        assert!(direct.is_concurrent_change());
        assert!(matches!(
            &direct,
            Error::ConcurrentChange {
                conflict: Conflict::SerializationFailure,
                ..
            }
        ));
        assert_eq!(direct.to_string(), serialization);
        let loaded = operation.classify(loader("40P01"));
        assert_eq!(
            loaded.to_string(),
            "A module operation conflicted with a concurrent transaction (deadlock, SQLSTATE \
             40P01): nothing of it was committed; retry the module operation"
        );
        let Error::ConcurrentChange {
            conflict: c,
            source,
        } = &loaded
        else {
            panic!("{loaded:?}");
        };
        assert_eq!(c.sqlstate(), "40P01");
        assert!(matches!(**source, Error::DataLoad(_)), "{source:?}");
        // through a step
        let lifted = operation.classify(Error::StepFailed {
            module: ModuleName::new("m").unwrap(),
            step: ModuleStep::DataFile,
            file: Some("data/views.xml".to_owned()),
            source: Box::new(loader("40001")),
        });
        assert_eq!(lifted.to_string(), serialization);
        let Error::ConcurrentChange { source, .. } = &lifted else {
            panic!("{lifted:?}");
        };
        let Error::StepFailed { module, source, .. } = &**source else {
            panic!("the step is kept as the source: {source:?}");
        };
        assert_eq!(module.as_str(), "m");
        assert!(matches!(**source, Error::DataLoad(_)), "{source:?}");
        // through the purge (an uninstall)
        let purged = operation.classify(Error::PurgeFailed {
            modules: vec![ModuleName::new("m").unwrap()],
            source: Box::new(loader("40001")),
        });
        assert_eq!(purged.to_string(), serialization);
        let Error::ConcurrentChange { source, .. } = &purged else {
            panic!("{purged:?}");
        };
        assert!(matches!(**source, Error::PurgeFailed { .. }), "{source:?}");
        // a failed reset stays outermost; its own conflict gets no retry advice
        let reset = operation.classify(Error::ResetFailed {
            error: Box::new(Error::Orm(erp_fields::Error::programming("x"))),
            reset: Box::new(Error::Orm(conflict("40001"))),
        });
        assert!(!reset.is_concurrent_change());
        let Error::ResetFailed { reset, .. } = &reset else {
            panic!("{reset:?}");
        };
        assert!(matches!(**reset, Error::Orm(_)), "{reset:?}");
        // another ORM error stays as it is
        assert!(matches!(
            operation.classify(Error::Orm(erp_fields::Error::programming("x"))),
            Error::Orm(_)
        ));
    }

    /// A14: a refused `CREATE EXTENSION` names the privilege the role lacks and keeps the
    /// database's error as the source; any other error passes through.
    #[test]
    fn a_refused_extension_names_the_privilege() {
        let refused = erp_fields::Error::db("permission denied to create extension \"pg_trgm\"")
            .with_sqlstate(Some("42501".to_owned()));
        let Error::Orm(named) = extension_error("pg_trgm", refused) else {
            panic!("an ORM error is kept");
        };
        assert_eq!(named.kind(), erp_fields::ErrorKind::Database);
        assert_eq!(named.sqlstate(), Some("42501"));
        assert_eq!(
            named.message(),
            "creating the PostgreSQL extension pg_trgm was refused (insufficient privilege): \
             pg_trgm is a trusted extension, so the owner of the database may create it (CREATE \
             privilege on the database); run the initialization as the database owner, or create \
             the extension as a superuser first with `CREATE EXTENSION pg_trgm`"
        );
        let source = std::error::Error::source(&named).unwrap();
        assert_eq!(
            source.to_string(),
            "permission denied to create extension \"pg_trgm\""
        );
        let other = erp_fields::Error::db("boom").with_sqlstate(Some("XX000".to_owned()));
        assert!(matches!(
            extension_error("pg_trgm", other),
            Error::Orm(e) if e.message() == "boom"
        ));
    }

    /// A6: only lower, and none (0) is replaced.
    #[test]
    fn limits_only_lower() {
        let configured = Duration::from_secs(30);
        assert_eq!(lowered(0, configured), configured);
        assert_eq!(lowered(5_000, configured), Duration::from_secs(5));
        assert_eq!(lowered(30_000, configured), configured);
        assert_eq!(lowered(600_000, configured), configured);
    }

    /// The queue key of a schema step is the step key behind `schema.` when that fits, else a
    /// prefix and the hash of the whole key: always a valid [`PostCommitKey`], the same for the
    /// same step, different for steps that share a long prefix.
    #[test]
    fn schema_queue_keys_are_valid_stable_and_distinct() {
        let table = TableName::parse("sale_order").unwrap();
        let short =
            SchemaStepRef::new(table.clone(), "backfill.sale_order", &["name_size"]).unwrap();
        assert_eq!(
            schema_queue_key(&short).unwrap().as_str(),
            "schema.backfill.sale_order"
        );
        let long_table = TableName::parse(&"t".repeat(63)).unwrap();
        let key = |column: &str| {
            let full = format!("not_null.{long_table}.{column}");
            let step = SchemaStepRef::new(long_table.clone(), &full, &[]).unwrap();
            schema_queue_key(&step).unwrap()
        };
        let (a, b) = (key(&"a".repeat(63)), key(&"b".repeat(63)));
        assert!(a.as_str().len() <= MAX_POST_COMMIT_KEY_BYTES);
        assert!(a.as_str().starts_with("schema.not_null."));
        assert_ne!(a, b);
        assert_eq!(a, key(&"a".repeat(63)));
    }

    /// The cursor store waits for a row another transaction holds only as long as the bookkeeping
    /// `lock_timeout`; when that gives up it stores nothing and returns `Ok` (the step goes on),
    /// and it stores nothing more for the rest of the run, even once the row is free.
    #[tokio::test]
    async fn a_cursor_store_that_gives_up_on_a_lock_stops_storing_for_the_run() {
        use erp_testkit::TestDb;

        let db = TestDb::create().await.unwrap();
        let setup = db.client().await.unwrap();
        setup
            .batch_execute(
                "CREATE TABLE ir_module_post_commit (id bigserial PRIMARY KEY, revision int4, \
                 state text, \"cursor\" int8, cursor_fingerprint text); \
                 INSERT INTO ir_module_post_commit (revision, state) VALUES (1, 'pending')",
            )
            .await
            .unwrap();
        drop(setup);
        let holder = deadpool_postgres::Object::take(db.client().await.unwrap());
        holder
            .batch_execute("BEGIN; SELECT id FROM ir_module_post_commit WHERE id = 1 FOR UPDATE")
            .await
            .unwrap();
        let client = deadpool_postgres::Object::take(db.client().await.unwrap());
        let mut store = CursorStore {
            client: &client,
            id: 1,
            revision: 1,
            fingerprint: StepFingerprint::parse("backfill:t:a").unwrap(),
            bookkeeping: Duration::from_millis(100),
            step: Duration::from_secs(60),
            gave_up: false,
        };
        let stored = async |db: &TestDb| {
            let reader = db.client().await.unwrap();
            let row = reader
                .query_one(
                    "SELECT \"cursor\", cursor_fingerprint FROM ir_module_post_commit",
                    &[],
                )
                .await
                .unwrap();
            (
                row.get::<_, Option<i64>>(0),
                row.get::<_, Option<String>>(1),
            )
        };
        // the row is held: the store gives up, returns Ok, nothing is stored
        store.committed(StepCursor::new(5).unwrap()).await.unwrap();
        assert!(store.gave_up);
        assert_eq!(stored(&db).await, (None, None));
        // the row is free again, but this run stores nothing more
        holder.batch_execute("ROLLBACK").await.unwrap();
        store.committed(StepCursor::new(6).unwrap()).await.unwrap();
        assert_eq!(stored(&db).await, (None, None));
        // a new run stores
        let mut next = CursorStore {
            client: &client,
            id: 1,
            revision: 1,
            fingerprint: StepFingerprint::parse("backfill:t:a").unwrap(),
            bookkeeping: Duration::from_millis(100),
            step: Duration::from_secs(60),
            gave_up: false,
        };
        next.committed(StepCursor::new(7).unwrap()).await.unwrap();
        assert_eq!(
            stored(&db).await,
            (Some(7), Some("backfill:t:a".to_owned()))
        );
        drop((holder, client));
        db.cleanup().await.unwrap();
    }
}

#[cfg(test)]
mod data_only_allowlist_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;
    use crate::manifest::Manifest;

    fn folder(manifest: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, AddonEntry) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sample");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("manifest.toml"), manifest).unwrap();
        for (path, content) in files {
            let full = dir.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        let parsed = Manifest::from_toml("sample", manifest).unwrap();
        let entry = AddonEntry::new(
            Arc::new(parsed),
            AddonSourceKind::DevFolder(dir.clone()),
            dir.display().to_string(),
        );
        (root, entry)
    }

    const PLAIN: &str =
        "name='S'\ndepends=['base']\ndata=['views/v.xml','security/a.csv']\ndemo=['demo/d.xml']\n";
    const GOOD: [(&str, &str); 6] = [
        ("models/10_m.toml", ""),
        ("views/v.xml", ""),
        ("security/a.csv", ""),
        ("demo/d.xml", ""),
        ("README.md", ""),
        ("data/x.csv", ""),
    ];

    fn refusal(manifest: &str, files: &[(&str, &str)]) -> String {
        let (_root, entry) = folder(manifest, files);
        data_only_refusal(&entry).expect("must be refused")
    }

    #[test]
    fn a_plain_data_only_folder_is_accepted() {
        let (_root, entry) = folder(PLAIN, &GOOD);
        assert_eq!(data_only_refusal(&entry), None);
    }

    #[test]
    fn sql_anywhere_is_refused() {
        let mut files = GOOD.to_vec();
        files.push(("data/evil.sql", "drop table x"));
        let reason = refusal(PLAIN, &files);
        assert!(
            reason.contains("data/evil.sql") && reason.contains(".sql"),
            "{reason}"
        );
    }

    #[test]
    fn migrations_directory_is_refused() {
        let mut files = GOOD.to_vec();
        files.push(("migrations/1.1.0/post-x.py", ""));
        let reason = refusal(PLAIN, &files);
        assert!(
            reason.contains("migrations") && reason.contains("rule"),
            "{reason}"
        );
    }

    #[test]
    fn static_directory_is_refused() {
        let mut files = GOOD.to_vec();
        files.push(("static/src/x.js", ""));
        let reason = refusal(PLAIN, &files);
        assert!(
            reason.contains("static") && reason.contains("rule"),
            "{reason}"
        );
    }

    #[test]
    fn rust_crate_files_are_refused() {
        for (path, needle) in [
            ("Cargo.toml", "Cargo.toml"),
            ("src/lib.rs", "src"),
            ("build.rs", "build.rs"),
        ] {
            let mut files = GOOD.to_vec();
            files.push((path, ""));
            let reason = refusal(PLAIN, &files);
            assert!(
                reason.contains(needle) && reason.contains("rule"),
                "{path}: {reason}"
            );
        }
    }

    #[test]
    fn a_symlink_is_refused() {
        let (_root, entry) = folder(PLAIN, &GOOD);
        let AddonSourceKind::DevFolder(dir) = entry.source_kind() else {
            unreachable!()
        };
        std::os::unix::fs::symlink("/etc/passwd", dir.join("data/link.csv")).unwrap();
        let reason = data_only_refusal(&entry).unwrap();
        assert!(
            reason.contains("data/link.csv") && reason.contains("symlink"),
            "{reason}"
        );
    }

    /// The manifest parser already refuses escaping paths, so discovery fails before the
    /// catalogue sees the addon; `data_only_refusal` repeats the rule as defense in depth.
    #[test]
    fn a_manifest_path_that_escapes_the_folder_is_refused() {
        for bad in ["../other/data.xml", "/etc/data.xml", "views/../../x.xml"] {
            let manifest = format!("name='S'\ndepends=['base']\ndata=['{bad}']\n");
            let error = Manifest::from_toml("sample", &manifest)
                .unwrap_err()
                .to_string();
            assert!(error.contains(bad), "{error}");
        }
    }

    #[test]
    fn manifest_data_must_be_xml_or_csv() {
        let reason = refusal("name='S'\ndepends=['base']\ndata=['data/x.sql']\n", &GOOD);
        assert!(
            reason.contains("data/x.sql") && reason.contains(".xml or .csv"),
            "{reason}"
        );
    }

    #[test]
    fn unknown_names_and_file_kinds_are_refused() {
        for path in [
            "notes.txt",
            "extra/data.xml",
            "data/blob.bin",
            "models/readme.md",
            "data/script.js",
        ] {
            let mut files = GOOD.to_vec();
            files.push((path, ""));
            let reason = refusal(PLAIN, &files);
            // a refused directory is named by itself, not by the file inside it
            let named = path
                .split('/')
                .next()
                .filter(|_| path == "extra/data.xml")
                .unwrap_or(path);
            assert!(
                reason.contains(named) && reason.contains("rule"),
                "{path}: {reason}"
            );
        }
    }

    #[test]
    fn the_shipped_sample_is_accepted() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons/sample_library");
        let text = fs::read_to_string(dir.join("manifest.toml")).unwrap();
        let entry = AddonEntry::new(
            Arc::new(Manifest::from_toml("sample_library", &text).unwrap()),
            AddonSourceKind::DevFolder(dir),
            "addons/sample_library",
        );
        assert_eq!(data_only_refusal(&entry), None);
    }
}

#[cfg(test)]
mod package_data_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;

    fn module() -> ModuleName {
        ModuleName::new("pkg").unwrap()
    }

    /// A package that declares `pkg.thing` and extends `lib.book`.
    fn data() -> crate::PackageData {
        let mut data = crate::PackageData::default();
        data.declared.insert("pkg.thing".to_owned());
        data.extended.insert("lib.book".to_owned());
        data
    }

    /// The registry's field types, as far as these tests need them.
    fn class_of(_model: &str, field: &str) -> Option<FieldClass> {
        match field {
            "group_ids" | "groups_id" => Some(FieldClass::Many2many),
            "child_id" | "child_ids" | "inherit_children_ids" | "view_ids" => {
                Some(FieldClass::One2many)
            }
            "nonesuch" => None,
            _ => Some(FieldClass::Scalar),
        }
    }

    fn xml(text: &str) -> std::result::Result<usize, String> {
        let mut reader = XmlDataReader::new(text, "views/v.xml", "pkg", false);
        check_package_operations(&module(), "views/v.xml", &data(), &class_of, &mut reader)
            .map(|ops| ops.count())
            .map_err(|e| e.to_string())
    }

    fn csv(name: &str, text: &str) -> std::result::Result<usize, String> {
        let erp_data::CsvImportResult::Loaded(import) =
            erp_data::parse_csv(text, name, "pkg", ConvertMode::Init).unwrap()
        else {
            panic!("not loaded")
        };
        let mut ops = std::iter::once(Ok::<DataOperation, ParseError>(
            import.into_operation(false),
        ));
        check_package_operations(&module(), name, &data(), &class_of, &mut ops)
            .map(|ops| ops.count())
            .map_err(|e| e.to_string())
    }

    fn refused(text: &str, needle: &str) {
        let err = xml(text).expect_err(needle);
        assert!(err.contains(needle), "{needle}: {err}");
        assert!(err.contains("views/v.xml"), "{err}");
        assert!(err.contains("line "), "{err}");
    }

    fn menu_groups(eval: &str) -> String {
        format!(
            r#"<erp><record id="m" model="ir.ui.menu"><field name="group_ids" eval="{eval}"/></record></erp>"#
        )
    }

    #[test]
    fn views_actions_menus_and_own_access_rows_pass() {
        let views = r#"<erp>
            <record id="v" model="ir.ui.view">
                <field name="model">lib.book</field>
                <field name="inherit_id" ref="lib.book_form"/>
                <field name="mode">extension</field>
            </record>
            <record id="p" model="ir.ui.view">
                <field name="model">pkg.thing</field>
                <field name="priority">5</field>
            </record>
            <record id="a" model="ir.actions.act_window">
                <field name="res_model">pkg.thing</field>
                <field name="binding_model_id" ref="pkg.model_pkg_thing"/>
            </record>
            <record id="a2" model="ir.actions.act_window">
                <field name="res_model">lib.book</field>
            </record>
            <record id="m" model="ir.ui.menu">
                <field name="name">n</field>
                <field name="group_ids" eval="[(4, ref('base.group_user'), 0), (5, 0, 0), [6, 0, [ref('base.group_user')]]]"/>
            </record>
            <menuitem id="mi" name="Menu" action="lib.some_action" parent="base.menu_x">
                <menuitem id="pkg.child" name="Child"/>
            </menuitem>
        </erp>"#;
        assert_eq!(xml(views), Ok(6));
        let access = "id,name,model_id/id,group_id/id,operation,active\n\
                      a1,A,pkg.model_pkg_thing,base.group_user,r,1\n\
                      pkg.a2,B,model_pkg_other,base.group_user,crud,1\n";
        assert_eq!(csv("security/ir.access.csv", access), Ok(1));
    }

    #[test]
    fn everything_else_is_refused_by_name_with_the_file_and_line() {
        for (text, needle) in [
            (
                r#"<erp><record id="u" model="res.users"/></erp>"#,
                "'res.users' is not allowed",
            ),
            (
                r#"<erp><record id="c" model="ir.cron"/></erp>"#,
                "'ir.cron' is not allowed",
            ),
            (
                r#"<erp><record id="g" model="ir.access"/></erp>"#,
                "access rows must be in a CSV file",
            ),
            (r#"<erp><function model="x" name="y"/></erp>"#, "<function>"),
            (
                r#"<erp><delete model="ir.ui.view" id="x"/></erp>"#,
                "<delete>",
            ),
            (
                r#"<erp><template id="t"><p/></template></erp>"#,
                "<template>",
            ),
            (
                r#"<erp><asset id="a" name="n"><bundle>web.assets</bundle><path>x.js</path></asset></erp>"#,
                "<asset>",
            ),
        ] {
            refused(text, needle);
        }
        let grab = "id,name,model_id/id,group_id/id,operation\n\
                    g,G,base.model_res_users,base.group_user,r\n";
        let err = csv("security/ir.access.csv", grab).unwrap_err();
        assert!(err.contains("base.model_res_users"), "{err}");
        let err = csv("data/res.partner.csv", "id,name\nx,X\n").unwrap_err();
        assert!(err.contains("not allowed"), "{err}");
        let err = csv(
            "security/ir.access.csv",
            "id,name,group_id/id,operation\ng,G,base.group_user,r\n",
        )
        .unwrap_err();
        assert!(err.contains("model_id/id"), "{err}");
    }

    /// H1: ids of another module are never written, and the access CSV has an exact column list
    /// (no `.id`: the importer would update the row with that database id).
    #[test]
    fn a_package_never_writes_a_record_of_another_module() {
        refused(
            r#"<erp><menuitem id="base.menu_administration" name="Hijacked"/></erp>"#,
            "belongs to module 'base'",
        );
        refused(
            r#"<erp><record id="lib.book_form" model="ir.ui.view"><field name="model">lib.book</field></record></erp>"#,
            "belongs to module 'lib'",
        );
        refused(
            r#"<erp><record id="lib.act" model="ir.actions.act_window"><field name="res_model">lib.book</field></record></erp>"#,
            "belongs to module 'lib'",
        );
        refused(
            r#"<erp><menuitem id="mi" name="M"><menuitem id="base.child" name="C"/></menuitem></erp>"#,
            "belongs to module 'base'",
        );
        refused(
            r#"<erp><record model="ir.ui.view"><field name="model">lib.book</field></record></erp>"#,
            "needs an id",
        );
        let err = csv(
            "security/ir.access.csv",
            "id,name,model_id/id,group_id/id,operation\n\
             base.access_x,X,pkg.model_pkg_thing,base.group_user,r\n",
        )
        .unwrap_err();
        assert!(
            err.contains("base.access_x") && err.contains("belongs to module 'base'"),
            "{err}"
        );
        assert!(err.contains("security/ir.access.csv"), "{err}");
        for column in [
            ".id",
            "model_id/.id",
            "group_id/.id",
            "domain",
            "generated",
            "perm_read",
        ] {
            let text = format!(
                "{column},id,name,model_id/id,group_id/id,operation\n7,a,A,pkg.model_pkg_thing,base.group_user,r\n"
            );
            let err = csv("security/ir.access.csv", &text).unwrap_err();
            assert!(
                err.contains(&format!("column '{column}' is not allowed")),
                "{column}: {err}"
            );
        }
    }

    /// H2: only link, clear and set, as 3-element commands with a literal code, on many2many
    /// fields; nothing at all on a one2many.
    #[test]
    fn x2many_values_are_checked_by_field_type_and_command() {
        for (eval, needle) in [
            // the dict forms
            (
                "[(1, ref('base.group_system'), {'user_ids': [(4, 2, 0)]})]",
                "command 1",
            ),
            ("[(0, 0, {'name': 'x'})]", "command 0"),
            ("[(2, 5, 0)]", "command 2"),
            ("[(3, 5, 0)]", "command 3"),
            ("[(4, 1, {'a': 1})]", "dict"),
            // the list form is read exactly like the tuple form
            ("[[2, ref('base.group_system'), 0]]", "command 2"),
            ("[[0, 0, {'name': 'x'}]]", "command 0"),
            // a code that only evaluates to a command
            (
                "[(+2, ref('base.group_system'), 0)]",
                "plain integer literal",
            ),
            ("[(-(-2), 5, 0)]", "plain integer literal"),
            ("[(True, 5, 0)]", "plain integer literal"),
            ("[(2.0, 5, 0)]", "plain integer literal"),
            // shape
            ("[(4, ref('base.group_user'))]", "2 elements, not 3"),
            ("[(5,)]", "1 elements, not 3"),
            ("[(4, 1, 0, 0)]", "4 elements, not 3"),
            ("ref('base.group_user')", "not a list of commands"),
            ("[ref('base.group_user')]", "not a list or tuple"),
            ("[Command.create({'a': 1})]", "not a list or tuple"),
            ("[(6, 0, [x])]", "this expression form"),
        ] {
            refused(&menu_groups(eval), needle);
        }
        // `ref=` on a many2many always fails at load: refused here
        refused(
            r#"<erp><record id="m" model="ir.ui.menu"><field name="group_ids" ref="base.group_user"/></record></erp>"#,
            "is a many2many",
        );
        // a plain text or number is not a many2many value
        refused(
            r#"<erp><record id="m" model="ir.ui.menu"><field name="group_ids">3</field></record></erp>"#,
            "is a many2many",
        );
        refused(
            r#"<erp><record id="m" model="ir.ui.menu"><field name="nonesuch">3</field></record></erp>"#,
            "not a field of 'ir.ui.menu'",
        );
    }

    /// H2: a one2many gets no value (a link writes the inverse many2one of any record; a child
    /// record creates rows).
    #[test]
    fn a_one2many_gets_no_value() {
        refused(
            r#"<erp><record id="m" model="ir.ui.menu"><field name="child_id" eval="[(4, ref('base.menu_administration'), 0)]"/></record></erp>"#,
            "'child_id' is a one2many",
        );
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">lib.book</field><field name="inherit_id" ref="lib.f"/>
               <field name="inherit_children_ids" eval="[(4, ref('base.view_x'), 0)]"/></record></erp>"#,
            "'inherit_children_ids' is a one2many",
        );
        refused(
            r#"<erp><record id="a" model="ir.actions.act_window"><field name="res_model">pkg.thing</field>
               <field name="view_ids" eval="[(6, 0, [ref('base.v')])]"/></record></erp>"#,
            "'view_ids' is a one2many",
        );
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">pkg.thing</field>
               <field name="child_ids"><record id="c" model="ir.ui.view"/></field></record></erp>"#,
            "'child_ids' is a one2many",
        );
        refused(
            r#"<erp><record id="m" model="ir.ui.menu"><field name="x" search="[('id','>',0)]" model="res.users"/></record></erp>"#,
            "search",
        );
    }

    /// M1: views and actions stay on the package's models, once per field name, and a model the
    /// package only extends gets extension views only.
    #[test]
    fn views_and_actions_stay_on_the_packages_models() {
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">res.partner</field></record></erp>"#,
            "model 'res.partner'",
        );
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="name">n</field></record></erp>"#,
            "needs a `model`",
        );
        // the first `model` is ours, the last one (what the loader keeps) is not
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">lib.book</field><field name="inherit_id" ref="lib.f"/>
               <field name="model">res.partner</field></record></erp>"#,
            "'model' is given twice",
        );
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">lib.book</field><field name="inherit_id" ref="lib.f"/>
               <field name="mode">primary</field></record></erp>"#,
            "`mode` must be absent or `extension`",
        );
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">lib.book</field></record></erp>"#,
            "only extension views",
        );
        // an `inherit_id` that is present but false or empty makes a primary view
        for value in [
            r#"<field name="inherit_id" eval="False"/>"#,
            r#"<field name="inherit_id" eval="0"/>"#,
            r#"<field name="inherit_id" eval="None"/>"#,
            r#"<field name="inherit_id"></field>"#,
            r#"<field name="inherit_id">lib.book_form</field>"#,
        ] {
            refused(
                &format!(
                    r#"<erp><record id="v" model="ir.ui.view"><field name="model">lib.book</field>{value}</record></erp>"#
                ),
                "inherit_id` must be",
            );
        }
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">lib.book</field><field name="inherit_id" eval="ref('a') or False"/></record></erp>"#,
            "expression form is not allowed",
        );
        // while both reference forms pass
        for value in [
            r#"<field name="inherit_id" ref="lib.book_form"/>"#,
            r#"<field name="inherit_id" eval="ref('lib.book_form')"/>"#,
        ] {
            assert_eq!(
                xml(&format!(
                    r#"<erp><record id="v" model="ir.ui.view"><field name="model">lib.book</field>{value}</record></erp>"#
                )),
                Ok(1)
            );
        }
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">lib.book</field><field name="inherit_id" ref="lib.f"/>
               <field name="priority">1</field></record></erp>"#,
            "`priority`",
        );
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">pkg.thing</field><field name="type">qweb</field></record></erp>"#,
            "qweb views",
        );
        refused(
            r#"<erp><record id="v" model="ir.ui.view"><field name="model">pkg.thing</field><field name="key">web.layout</field></record></erp>"#,
            "view keys",
        );
        refused(
            r#"<erp><record id="a" model="ir.actions.act_window"><field name="res_model">res.partner</field></record></erp>"#,
            "action on model 'res.partner'",
        );
        refused(
            r#"<erp><record id="a" model="ir.actions.act_window"><field name="name">n</field></record></erp>"#,
            "needs a `res_model`",
        );
        // a binding: only on an action for a model the package declares, full xmlid
        for target in [
            "base.model_res_partner",
            "lib.model_lib_book",
            "other.model_pkg_thing",
        ] {
            let text = format!(
                r#"<erp><record id="a" model="ir.actions.act_window"><field name="res_model">pkg.thing</field><field name="binding_model_id" ref="{target}"/></record></erp>"#
            );
            refused(&text, "binding_model_id");
        }
        refused(
            r#"<erp><record id="a" model="ir.actions.act_window"><field name="res_model">lib.book</field><field name="binding_model_id" ref="pkg.model_pkg_thing"/></record></erp>"#,
            "binding_model_id",
        );
    }

    /// N1: `context` and `uid` change the environment the loader runs in.
    #[test]
    fn context_and_uid_are_refused_on_every_element() {
        for text in [
            r#"<erp><data context="{'a': 1}"><record id="v" model="ir.ui.menu"/></data></erp>"#,
            r#"<erp><data uid="base.user_admin"><record id="v" model="ir.ui.menu"/></data></erp>"#,
            r#"<erp><record id="v" model="ir.ui.menu" context="{'a': 1}"/></erp>"#,
            r#"<erp><record id="v" model="ir.ui.menu" uid="base.user_admin"/></erp>"#,
        ] {
            let err = xml(text).expect_err(text);
            assert!(
                err.contains("`context` and `uid` are not allowed"),
                "{text}: {err}"
            );
        }
    }

    #[test]
    fn a_package_file_is_read_from_memory_within_its_limit() {
        let mut files = crate::PackageFiles::new();
        files.insert("views/v.xml".to_owned(), b"<erp/>".to_vec());
        assert_eq!(
            package_file(&files, &module(), "views/v.xml", 100).unwrap(),
            b"<erp/>"
        );
        let too_big = package_file(&files, &module(), "views/v.xml", 3).unwrap_err();
        assert!(
            matches!(too_big, Error::DataFileTooLarge { .. }),
            "{too_big:?}"
        );
        let missing = package_file(&files, &module(), "views/none.xml", 100).unwrap_err();
        assert!(
            matches!(&missing, Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound),
            "{missing:?}"
        );
        for escape in ["../x.xml", "/etc/passwd", "views/../../x", ""] {
            assert!(
                package_file(&files, &module(), escape, 100).is_err(),
                "{escape}"
            );
        }
    }
}
