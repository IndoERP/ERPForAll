use crate::manifest::ModuleName;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Invalid module name '{name}': {reason}")]
    InvalidModuleName { name: String, reason: String },

    #[error("Module '{name}': manifest not found")]
    ManifestNotFound { name: ModuleName },

    #[error("Module {name}: invalid manifest: {message}")]
    InvalidManifest { name: ModuleName, message: String },

    #[error("Module {name}: invalid manifest: invalid version '{version}': {message}")]
    InvalidVersion {
        name: ModuleName,
        version: String,
        message: String,
    },

    #[error(
        "auto_install triggers must be dependencies, found non-dependencies [{}] for module {name}",
        format_module_list(.non_dependencies)
    )]
    AutoInstallTriggerMismatch {
        name: ModuleName,
        non_dependencies: Vec<ModuleName>,
    },

    #[error(
        "Unable to {operation} module \"{module}\" because an external dependency is not met: {dependency}"
    )]
    ExternalDependencyMissing {
        module: ModuleName,
        dependency: String,
        operation: ModuleOperation,
    },

    // ---- scheduling rules (MOD §4.2) -------------------------------------------------------
    /// Module names the catalog does not know.
    #[error("Unknown module(s): {}", format_module_list(.names))]
    UnknownModule { names: Vec<ModuleName> },

    #[error(
        "You try to install module \"{module}\" that depends on module \"{dependency}\".\nBut the latter module is not available in your system."
    )]
    UnknownDependency {
        module: ModuleName,
        dependency: ModuleName,
    },

    #[error(
        "You try to install module \"{module}\" that depends on module \"{dependency}\", which is not installable."
    )]
    DependencyNotInstallable {
        module: ModuleName,
        dependency: ModuleName,
    },

    /// Two module rows (or two manifests given to `update_list`) with the same name.
    #[error("Module \"{module}\" is listed twice")]
    DuplicateModuleRow { module: ModuleName },

    /// `unavailable` is computed when the graph is built, never stored.
    #[error("Module \"{module}\": the state 'unavailable' is never stored")]
    UnavailableIsNotStored { module: ModuleName },

    #[error("Invalid module category '{category}': {message}")]
    InvalidCategory { category: String, message: String },

    #[error("Module \"{module}\" is not installable.")]
    ModuleNotInstallable { module: ModuleName },

    /// The module has a row but no addon source provides it.
    #[error("Module \"{module}\" is not available in any addon source.")]
    ModuleUnavailable { module: ModuleName },

    #[error("Recursion error in modules dependencies!")]
    DependencyRecursion,

    /// `module` excludes `other` and both are installed or scheduled.
    #[error("Modules \"{module_display}\" and \"{other_display}\" are incompatible.")]
    IncompatibleModules {
        module: ModuleName,
        other: ModuleName,
        /// `module`'s display name (the manifest `name`).
        module_display: String,
        /// `other`'s display name.
        other_display: String,
    },

    #[error(
        "You are trying to install incompatible modules in category \"{}\":{}",
        .category.name(),
        format_category_modules(.modules)
    )]
    ExclusiveCategory {
        category: crate::lifecycle::CategoryPath,
        /// The modules of the category.
        modules: Vec<CategoryMember>,
    },

    /// A stored module row that the manifest-driven operations could not have produced (a
    /// database edited outside ERPForAll).
    #[error("Module \"{module}\": inconsistent stored row: {message}")]
    InvalidModuleRow { module: ModuleName, message: String },

    #[error("Invalid country code '{code}': expected two ASCII letters (ISO 3166-1 alpha-2)")]
    InvalidCountryCode { code: String },

    #[error(
        "Module categories \"{first}\" and \"{second}\" would share the external id 'base.{xml_id}'; rename one of them"
    )]
    CategoryIdCollision {
        first: crate::lifecycle::CategoryPath,
        second: crate::lifecycle::CategoryPath,
        xml_id: String,
    },

    #[error("Cannot upgrade module \u{201c}{module}\u{201d}. It is not installed.")]
    UpgradeNotInstalled { module: ModuleName },

    /// "Load demo data" refused: module operations are scheduled.
    #[error(
        "Cannot load demo data while module operations are pending ({}); apply or reset them first",
        format_module_list(.modules)
    )]
    ForceDemoPending { modules: Vec<ModuleName> },

    /// "Load demo data" refused: an installed addon's version on disk differs from
    /// the installed one, and loading demo data must not stamp a version without its upgrade.
    #[error(
        "Cannot load demo data: the version on disk differs from the installed version for [{}]; upgrade these addons first",
        format_module_list(.modules)
    )]
    ForceDemoVersionMismatch { modules: Vec<ModuleName> },

    /// "Load demo data" refused: an installed addon depends on an addon that is
    /// not installed (a dependency added on disk); install or upgrade first.
    #[error(
        "Cannot load demo data: installed addon {module} depends on {dependency}, which is not installed; upgrade {module} first"
    )]
    ForceDemoMissingDependency {
        module: ModuleName,
        dependency: ModuleName,
    },

    #[error(
        "You try to upgrade the module {module} that depends on the module: {dependency}.\nBut this module is not available in your system."
    )]
    UpgradeUnknownDependency {
        module: ModuleName,
        dependency: ModuleName,
    },

    #[error("Those modules cannot be uninstalled: {}", format_module_list(.modules))]
    CannotUninstall { modules: Vec<ModuleName> },

    #[error(
        "One or more of the selected modules have already been uninstalled, if you believe this to be an error, you may try again later or contact support."
    )]
    AlreadyUninstalled,

    #[error("Duplicate addon '{name}' found from multiple sources: '{source_a}' and '{source_b}'")]
    DuplicateAddon {
        name: ModuleName,
        source_a: String,
        source_b: String,
    },

    #[error("Missing state for module '{module}'")]
    MissingModuleState { module: ModuleName },

    /// Update mode: modules marked to install, upgrade or remove were left out of the graph
    /// .
    #[error(
        "Modules marked to install, upgrade or remove cannot be loaded:\n{}",
        format_drops(.modules)
    )]
    DroppedUpdateModules {
        modules: Vec<(ModuleName, crate::graph::DropReason)>,
    },

    /// Load mode: installed modules depend on modules that are not installed.
    #[error(
        "Inconsistent module states in the database:\n{}",
        format_inconsistent(.modules)
    )]
    InconsistentStates {
        /// The offending pairs, sorted.
        modules: Vec<InconsistentDependency>,
    },

    #[error("Module '{module}' depends on itself")]
    SelfDependency { module: ModuleName },

    #[error("Module '{module}' has duplicate dependency '{dependency}'")]
    DuplicateDependency {
        module: ModuleName,
        dependency: ModuleName,
    },

    #[error("Module '{module}' excludes itself")]
    SelfExclusion { module: ModuleName },

    #[error("Module '{module}' has duplicate exclusion '{exclusion}'")]
    DuplicateExclusion {
        module: ModuleName,
        exclusion: ModuleName,
    },

    #[error("Internal invariant broken: {message}")]
    InvariantViolation { message: String },

    /// The data loader's session was called out of order (`begin_modules` twice, `finish_module`
    /// before it, for a module not of the operation, or twice for one module): a bug of the
    /// caller, never the data's mistake. The loader's typed error is kept (review T-2).
    #[error("Internal invariant broken: {0}")]
    OperationOrder(#[source] erp_data::load::load_state::OperationOrderError),

    #[error("Discovery error at '{path}': {message}")]
    DiscoveryError { path: PathBuf, message: String },

    #[error("Discovery encountered {count} error(s):\n{}", format_discovery_errors(.errors))]
    DiscoveryErrors { count: usize, errors: Vec<Error> },

    // ---- operations ---------------------------------------------------------------
    /// Only the superuser runs module operations until M3 adds administrator groups
    /// .
    #[error(
        "Access denied to module.{method} for user {uid}: only the superuser may run module operations until M3 adds administrator groups"
    )]
    AccessDenied { method: &'static str, uid: i64 },

    /// The environment passed to a module operation was made by another ORM than the module
    /// system's (another database, or another lineage on the same pool).
    #[error(
        "Access denied to module.{method}: the environment does not belong to this module system's ORM"
    )]
    ForeignEnvironment { method: &'static str },

    /// Another module operation holds the operation lock.
    #[error(
        "ERPForAll is currently processing another module operation.\nPlease try again later or contact your system administrator."
    )]
    OperationInProgress,

    #[error(
        "Database {database} not initialized: initialize it with ModuleSystem::initialize (create_database makes the empty database); `erp module` never initializes one"
    )]
    DatabaseNotInitialized { database: String },

    #[error("Database {database} is already initialized")]
    DatabaseAlreadyInitialized { database: String },

    /// A generated company restriction was changed since a module operation generated it (design
    /// `m3-company-a1.md` §6.3 step 3).
    #[error(
        "Module operation refused: the generated company restriction of {model} ({declaration}) {problem} since it was generated; run the update with --restore-generated-restrictions to rewrite it"
    )]
    GeneratedRestrictionTampered {
        model: String,
        declaration: String,
        problem: crate::company_access::TamperProblem,
    },

    /// The read-back of the emitted restrictions does not match the declarations (step 7).
    #[error(
        "Module operation refused: the generated company restriction of {model} ({declaration}) {problem}"
    )]
    GeneratedRestrictionInvalid {
        model: String,
        declaration: String,
        problem: String,
    },

    /// Records of a model restricted by its owner have no owner (step 5).
    #[error(
        "Module operation refused: {model} restricts by its owner '{edge}', but {} record(s) have no owner: ids {}{}; give them an owner, then update again",
        .ids.len(),
        .ids.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
        if *.more { " and more" } else { "" }
    )]
    OrphanOwnerRows {
        model: String,
        edge: String,
        ids: Vec<i64>,
        more: bool,
    },

    /// The database did not acknowledge the module operation's commit. Durable state and audit
    /// must be reconciled before any retry; rollback, transient-state reset and post-commit
    /// execution cannot be inferred to be safe. The original database error remains the source.
    #[error(
        "Module operation commit outcome is unknown; reconcile durable state before retrying: {source}"
    )]
    CommitOutcomeUnknown {
        #[source]
        source: erp_fields::Error,
    },

    /// An ORM or database error; the inner kind is kept.
    #[error(transparent)]
    Orm(#[from] erp_fields::Error),

    /// The addon digest of the node registry could not be computed: a package without
    /// a recorded SHA-256, a dev folder with a symbolic link or over its bounds.
    #[error(transparent)]
    NodeDigest(#[from] erp_orm::node::NodeError),

    /// A data file that does not parse.
    #[error(transparent)]
    DataParse(#[from] erp_data::ParseError),

    /// The data loader could not load an operation of a data file. The message says where
    /// (file, line, the operation's start tag) and what; an ORM error behind it is its source.
    #[error(transparent)]
    DataLoad(#[from] erp_data::load::LoadError),

    #[error(
        "Module '{module}' names {kind} '{name}' in manifest.toml, but no #[hook] implementation of it is linked"
    )]
    HookMissing {
        module: ModuleName,
        kind: crate::hooks::HookKind,
        name: String,
    },

    #[error("Module '{module}': {kind} '{name}' is implemented twice: {first} and {second}")]
    HookDuplicate {
        module: ModuleName,
        kind: crate::hooks::HookKind,
        name: String,
        first: String,
        second: String,
    },

    #[error(
        "Module '{module}': the {kind} implementation '{name}' at {location} is not named in manifest.toml"
    )]
    HookOrphan {
        module: ModuleName,
        kind: crate::hooks::HookKind,
        name: String,
        location: String,
    },

    #[error(
        "Module '{module}': migration '{name}' for version {version} ({stage}) is implemented twice: {first} and {second}"
    )]
    MigrationDuplicate {
        module: ModuleName,
        version: crate::version::SemverVersion,
        stage: crate::migrations::MigrationStage,
        name: String,
        first: String,
        second: String,
    },

    /// A migration step returned an error: the native function's, or the SQL statement's of a
    /// `.sql` script. The source keeps its kind (and SQLSTATE).
    #[error("Module '{module}': migration '{name}' (version {version}, stage {stage}) failed")]
    MigrationFailed {
        module: ModuleName,
        version: crate::version::SemverVersion,
        stage: crate::migrations::MigrationStage,
        name: String,
        /// Boxed: the error enum stays small (`clippy::result_large_err`).
        #[source]
        source: Box<erp_fields::Error>,
    },

    /// A SQL migration script refused before any of it is sent (transaction control, an
    /// unsupported or unterminated construct, or a session that reads strings differently).
    #[error("Module \"{module}\": SQL migration '{path}' is refused: {refusal}")]
    MigrationSqlRefused {
        module: ModuleName,
        path: String,
        refusal: crate::migrations::MigrationSqlRefusal,
    },

    /// One more selected SQL migration file than one operation accepts (all its modules
    /// together); the file was not read.
    #[error(
        "Module \"{module}\": SQL migration '{path}' passes the {limit} migration files one operation selects (ModuleSystemBuilder::max_migration_files)"
    )]
    MigrationFilesTooMany {
        module: ModuleName,
        path: String,
        limit: usize,
    },

    /// A module's `migrations/` directory holds more version folders than one module may have.
    #[error(
        "Module \"{module}\": migrations/ holds more than {limit} version folders (ModuleSystemBuilder::max_migration_files)"
    )]
    MigrationFoldersTooMany { module: ModuleName, limit: usize },

    #[error("Module '{module}': its code does not match manifest.toml: {message}")]
    AddonCodeMismatch { module: ModuleName, message: String },

    #[error(
        "Module \"{module}\" lists data or demo files, but the data loader was disabled (ModuleSystemBuilder::no_data_loader)"
    )]
    DataLoaderMissing { module: ModuleName },

    /// A data file of `base` reached a load session that has no base-install grant
    /// ([`crate::SessionContext::base_install_grant`]): the transaction cannot be marked as
    /// loading the base module's data, so the file is refused rather than loaded without the
    /// mark. The module system always lends its grant; this is a loader built or driven outside it.
    #[error(
        "Module \"{module}\": data file '{path}': base data needs the base-install grant of the module-install executor (the ORM built by OrmBuilder::build_with_base_install_grant), and this load session has none"
    )]
    BaseInstallGrantMissing { module: ModuleName, path: String },

    #[error(
        "Module \"{module}\": can't load unknown file type {path} (expected .xml, .csv, .sql or .js)"
    )]
    DataFileUnsupported { module: ModuleName, path: String },

    #[error(
        "Module \"{module}\": data files of {source_kind} addons cannot be read yet (only dev-folder addons have data files on disk)"
    )]
    DataSourceUnsupported {
        module: ModuleName,
        source_kind: SourceKind,
    },

    /// A data file of a package addon holds something a package may not load: only
    /// views, window actions, menus and access rows of the package's own models are allowed.
    #[error(
        "Module \"{module}\": data file '{path}' line {line}: {what}; a package addon's data files may only declare views (also inherited ones), window actions, menus and access rows (CSV) for its own models"
    )]
    PackageDataRefused {
        module: ModuleName,
        path: String,
        line: usize,
        what: String,
    },

    /// A package addon was named as the owner of H18 view-type registrations. The guest
    /// registration contract has no WIT mapping yet (M2b), so the attempt is refused rather than
    /// accepted and ignored.
    #[error(
        "Module \"{module}\": H18 view-type registrations are not supported for package addons (Unsupported: the guest H18 registration contract is deferred to a later release)"
    )]
    GuestViewTypesUnsupported { module: ModuleName },

    #[error("Module \"{module}\": data file '{path}' is larger than {limit} bytes")]
    DataFileTooLarge {
        module: ModuleName,
        path: String,
        limit: u64,
    },

    /// A limit set on the builder outside its allowed range (limits can only be lowered, and
    /// zero never means unlimited).
    #[error("{name} must be between 1 and {max}, got {value}")]
    InvalidLimit {
        name: LimitKind,
        value: u64,
        max: u64,
    },

    /// A timeout set on the builder outside its allowed range (limits can only be lowered, and
    /// zero would mean "no limit" to PostgreSQL). Durations print in Rust's `Debug` form
    /// (`30s`, `10µs`).
    #[error("{name} must be between 1ms and {max:?}, got {value:?}")]
    InvalidTimeout {
        name: LimitKind,
        value: std::time::Duration,
        max: std::time::Duration,
    },

    /// The data loader returned `Ok` before the reader reached the end of the file.
    #[error("Module \"{module}\": the data loader returned before the end of data file '{path}'")]
    DataLoaderStoppedEarly { module: ModuleName, path: String },

    /// A data file that is a symbolic link or lies under one (untrusted input stays
    /// inside the addon).
    #[error(
        "Module \"{module}\": data file '{path}' is a symbolic link or lies under one; data files must be regular files inside the addon"
    )]
    DataFileSymlink { module: ModuleName, path: String },

    /// A module lists more data and demo files than the operation accepts.
    #[error(
        "Module \"{module}\" lists {count} data and demo files; an operation accepts at most {limit} per module (ModuleSystemBuilder::max_data_files)"
    )]
    TooManyDataFiles {
        module: ModuleName,
        count: usize,
        limit: usize,
    },

    /// The data files of one operation add up to more than the operation accepts.
    #[error(
        "Module \"{module}\": data file '{path}' passes the {limit} bytes of data files one operation reads (ModuleSystemBuilder::max_operation_data_bytes)"
    )]
    OperationDataTooLarge {
        module: ModuleName,
        path: String,
        limit: u64,
    },

    /// A data file with several hard links: it may be a file outside the addon (a hard link
    /// cannot be told from the original by its path).
    #[error(
        "Module \"{module}\": data file '{path}' has {links} hard links; data files must be regular files that belong to the addon only"
    )]
    DataFileHardLinked {
        module: ModuleName,
        path: String,
        links: u64,
    },

    /// Uninstall would orphan records the module created in models that stay installed: purging
    /// them is the data loader's purge (/030); this is the refusal when the system has no
    /// data loader.
    #[error(
        "Module \"{module}\" cannot be uninstalled yet: it owns records of models that stay installed ({}); no data loader is installed to remove them",
        format_owned(.models)
    )]
    UninstallOwnedRecords {
        module: ModuleName,
        /// The models and their number of external ids, sorted by model.
        models: Vec<OwnedRecords>,
    },

    /// The data loader's purge ([`crate::DataLoader::purge`]) left external ids of the removed
    /// modules that point at models staying installed: removing only the external ids would orphan
    /// the records.
    #[error(
        "Module \"{module}\" cannot be uninstalled: after the data loader's purge it still owns records of models that stay installed ({})",
        format_owned(.models)
    )]
    PurgeIncomplete {
        module: ModuleName,
        /// The models and their number of external ids, sorted by model.
        models: Vec<OwnedRecords>,
    },

    /// The data loader's purge ([`crate::DataLoader::purge`]) of the modules being uninstalled
    /// failed. The loader purges them together (records of one may reference another's), so the
    /// error names all of them; `source` is the loader's error.
    #[error("Modules {}: the data loader's purge before uninstall failed", format_module_list(.modules))]
    PurgeFailed {
        /// The modules being uninstalled, in load order.
        modules: Vec<ModuleName>,
        source: Box<Error>,
    },

    /// The demo data of a module failed (its demo files were rolled back) and recording the
    /// failure in `ir.demo_failure` failed too. The demo failure is named here, since the
    /// recording error alone would not say which module's demo it was about.
    #[error(
        "Module \"{module}\": its demo data failed ({demo_error}) and the failure could not be recorded"
    )]
    DemoFailureNotRecorded {
        module: ModuleName,
        demo_error: String,
        source: Box<Error>,
    },

    /// Removing the schema of uninstalled modules would remove objects that other modules (or
    /// the database administrator) created on their tables or columns: ERPForAll never removes
    /// them through `CASCADE`. PostgreSQL's message and detail name the objects.
    #[error(
        "Cannot remove the schema of modules {}: {message} ({detail}); remove the objects that use it first",
        format_module_list(.modules)
    )]
    SchemaDropBlocked {
        /// The modules being uninstalled.
        modules: Vec<ModuleName>,
        /// PostgreSQL's message (`cannot drop table … because other objects depend on it`).
        message: String,
        /// PostgreSQL's detail: the dependent objects.
        detail: String,
    },

    /// A table the uninstall would drop is still used by something that stays (two models with
    /// one `_table`, or a model on a table of the module system): nothing is dropped.
    #[error(
        "Cannot remove the schema of modules {}: table {table} is used by {dropped}, which is removed, and also by {kept}, which stays",
        format_module_list(.modules)
    )]
    SharedSchemaTable {
        /// The modules being uninstalled.
        modules: Vec<ModuleName>,
        /// The table.
        table: String,
        /// What used it and is removed (`model x.y`, `the relation table of field x.y.z`).
        dropped: String,
        /// What uses it and stays.
        kept: String,
    },

    /// Removing the schema of uninstalled modules found tables of theirs (model or many2many
    /// relation tables) missing from the database: something outside ERPForAll removed them.
    #[error(
        "Cannot remove the schema of modules {}: {}",
        format_module_list(.modules),
        format_missing_tables(.tables)
    )]
    SchemaTablesMissing {
        /// The modules being uninstalled.
        modules: Vec<ModuleName>,
        /// The missing tables, sorted.
        tables: Vec<String>,
    },

    /// Removing the schema of uninstalled modules found columns of theirs missing from tables that
    /// stay: something outside ERPForAll removed them.
    #[error(
        "Cannot remove the schema of modules {}: columns {} do not exist (they were removed outside ERPForAll)",
        format_module_list(.modules),
        .columns.join(", ")
    )]
    SchemaColumnsMissing {
        /// The modules being uninstalled.
        modules: Vec<ModuleName>,
        /// The missing columns as `table.column`, sorted.
        columns: Vec<String>,
    },

    /// One step of a module's install, upgrade or uninstall failed; `source` is the step's own
    /// error (its kind is kept). The message names the module, the step and the file; the step's
    /// own message is the source's (walk [`std::error::Error::source`] to print the chain).
    #[error("Module \"{module}\": {step}{} failed", format_file(.file))]
    StepFailed {
        module: ModuleName,
        step: ModuleStep,
        /// The data file, for the data steps.
        file: Option<String>,
        source: Box<Error>,
    },

    /// A statement of a module operation could not get a database lock (SQLSTATE 55P03,
    /// `lock_not_available`): it waited longer than the operation's `lock_timeout`
    /// ([`crate::system::ModuleSystemBuilder::lock_timeout`]), or it asked for the lock with
    /// `NOWAIT`. The source (PostgreSQL's message) says which.
    #[error(
        "A statement of a module operation could not get a database lock (SQLSTATE 55P03; lock_timeout is {})",
        format_limit(*.lock_timeout)
    )]
    LockNotAvailable {
        /// The `lock_timeout` in force for the failed statement (the operation's, or the lower
        /// one of the schema step or of the caller's session).
        lock_timeout: Duration,
        source: erp_fields::Error,
    },

    /// A statement of a module operation lost to a concurrent transaction (a serialization
    /// failure, SQLSTATE `40001`, or a deadlock, `40P01`): for example a user edited a view of a
    /// tree the operation's view check locks (review A3-D2). The operation's transaction
    /// was rolled back, nothing of it committed; running it again is safe. The source says where
    /// (step, data file, line) and keeps PostgreSQL's error.
    #[error(
        "A module operation conflicted with a concurrent transaction ({conflict}): nothing of it was committed; retry the module operation"
    )]
    ConcurrentChange {
        /// Which conflict.
        conflict: Conflict,
        /// The operation's own error, holding the `40001`/`40P01` database error: the step,
        /// purge or rollback wrapper it came through, with the database error inside. Built only
        /// by the module system's error classification (`Limits::classify_with` in
        /// `system.rs`), which lifts a serialization failure or deadlock to this variant
        /// outermost; the variant's fields are public as every variant's, so a caller building
        /// one by hand is not prevented by the type (review T-11).
        source: Box<Error>,
    },

    /// A statement of a module operation was canceled (SQLSTATE 57014, `query_canceled`): it ran
    /// longer than the operation's `statement_timeout`
    /// ([`crate::system::ModuleSystemBuilder::statement_timeout`]), or someone canceled it
    /// (`pg_cancel_backend`). The source (PostgreSQL's message) says which.
    #[error(
        "A statement of a module operation was canceled (SQLSTATE 57014; statement_timeout is {})",
        format_limit(*.statement_timeout)
    )]
    StatementCanceled {
        /// The `statement_timeout` in force for the failed statement; zero when none was set
        /// (the statement was canceled by someone).
        statement_timeout: Duration,
        source: erp_fields::Error,
    },

    /// Rows of a base metadata table hold a state the table's check constraint does not accept:
    /// a database written before the constraint existed. Named before the schema update adds the
    /// constraint, instead of PostgreSQL's generic check violation.
    #[error(
        "Table {table} has rows whose state is not one of {} (ids and states: {}); correct or delete them, then run the operation again",
        .allowed.join(", "),
        format_stored_states(.rows)
    )]
    InvalidStoredStates {
        /// The table.
        table: &'static str,
        /// The states the constraint accepts.
        allowed: &'static [&'static str],
        /// The offending rows, by id (at most [`crate::system::MAX_REPORTED_ROWS`]).
        rows: Vec<StoredStateRow>,
    },

    /// A plain load found pending operations and waited longer than
    /// [`crate::system::ModuleSystemBuilder::operation_wait_timeout`] for another operation.
    #[error("A plain load waited longer than {} for another module operation to finish", format_limit(*.limit))]
    OperationWaitTimeout {
        /// The wait that ran out: the operation wait timeout, or the lower `lock_timeout` of the
        /// caller's session.
        limit: Duration,
    },

    /// A hook, data loader or model method ended the operation's transaction (a raw `COMMIT`,
    /// `ROLLBACK` or a new `BEGIN` sent through the transaction): the operation lock or the
    /// transaction itself is no longer the operation's, so what the operation did before may be
    /// committed and what it did after may not be. The operation stops and transient module states
    /// are reset; check the database.
    #[error(
        "The transaction of a module operation was ended by a hook, data loader or model method ({detail}); the database may hold a partial change"
    )]
    OperationTransactionEnded { detail: &'static str },

    /// The auto-install closure reached a country-restricted module, but the companies' countries
    /// are not known: they come from `res.company`, which arrives with M3. The
    /// message gives the remedy every entry point accepts (naming the module); a [`Catalog`]
    /// caller can also pass the countries with `InstallOptions::with_company_countries`, which
    /// the `ModuleSystem` operations do not take.
    ///
    /// [`Catalog`]: crate::lifecycle::Catalog
    #[error(
        "Module \"{module}\" auto-installs only for companies in {}, but the companies' countries are not known until res.company arrives (M3); install \"{module}\" by name in the same operation to include it",
        format_countries(.countries)
    )]
    CompanyCountriesUnknown {
        module: ModuleName,
        /// The module's countries, sorted.
        countries: Vec<crate::country::CountryCode>,
    },

    /// `button_uninstall` of a module no addon source provides: its tables are only known from
    /// its code.
    #[error(
        "Module \"{module}\" cannot be uninstalled: it is not available in any addon source (restore its source first)"
    )]
    UninstallUnavailable { module: ModuleName },

    /// `button_uninstall` of a module whose manifest says `installable = false`: the graph leaves
    /// it out, so the runner could not remove it.
    #[error("Module \"{module}\" cannot be uninstalled: its manifest says installable = false")]
    UninstallNotInstallable { module: ModuleName },

    /// The operation failed and resetting transient states afterwards failed too: both errors.
    /// The operation's error is the source (the chain continues with it); the reset's is in the
    /// message.
    #[error(
        "A module operation failed, and resetting transient module states afterwards failed too: {reset}"
    )]
    ResetFailed {
        #[source]
        error: Box<Error>,
        reset: Box<Error>,
    },

    /// `ROLLBACK` after a failed step failed too (the connection was closed instead, which rolls
    /// back on the server): both errors. The operation's error is the source; the rollback's is
    /// in the message.
    #[error("A module operation failed, and rolling it back failed too: {rollback}")]
    RollbackFailed {
        #[source]
        error: Box<Error>,
        rollback: Box<Error>,
    },

    #[error("Invalid db_name='{name}': {reason}")]
    InvalidDatabaseName { name: String, reason: String },

    #[error("Database {name} already exists")]
    DatabaseExists { name: crate::system::DatabaseName },

    /// A refused `models/*.toml` file or declaration; `line` is 1-based when known.
    #[non_exhaustive]
    #[error("Model file '{}': {message}", format_location(.path, *.line))]
    ModelFile {
        path: PathBuf,
        line: Option<usize>,
        message: String,
        /// The registry error behind `message`, when the registry refused the declaration.
        #[source]
        source: Option<Box<erp_fields::Error>>,
    },

    /// Registry metadata refused an addon's runtime declaration (size limits, intern budget).
    #[non_exhaustive]
    #[error("Module {addon}: {source}")]
    Registry {
        addon: ModuleName,
        #[source]
        source: erp_fields::Error,
    },

    #[error("I/O error reading '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Invalid capability '{capability}': {reason}")]
    InvalidCapability { capability: String, reason: String },

    #[error("Package integrity error: {message}")]
    PackageIntegrity { message: String },

    #[error("Package format error: {message}")]
    PackageFormat { message: String },

    /// An archive bound was exceeded while reading an untrusted package.
    #[non_exhaustive]
    #[error(
        "Package limit '{limit}' exceeded: observed at least {observed_at_least}, maximum {max}"
    )]
    PackageLimitExceeded {
        limit: &'static str,
        max: u64,
        observed_at_least: u64,
    },

    /// The administrator consent does not match the package it is presented with.
    #[non_exhaustive]
    #[error("Consent for addon '{addon}' refused: {message}")]
    ConsentMismatch { addon: ModuleName, message: String },

    /// A runtime install, upgrade or uninstall of a WebAssembly addon was refused or failed
    /// . Nothing of it persists. Constructible by [`crate::wasm_lifecycle::ComponentLoader`]
    /// implementations, so the variant itself is not `non_exhaustive` (the enum is).
    #[error("WebAssembly addon '{addon}': {reason}")]
    WasmAddon {
        addon: ModuleName,
        reason: WasmReason,
    },

    #[error("Addon '{addon}' requests unapproved capabilities: [{}]", .unapproved.join(", "))]
    UnapprovedCapabilities {
        addon: ModuleName,
        unapproved: Vec<String>,
    },

    #[error(
        "Installation or upgrade of addon '{addon}' carrying client code in static/dist/ requires administrator acknowledgement bound to SHA-256 digest '{expected_digest}', but got {provided_digest:?}; unacknowledged client files: [{}]",
        .client_files.join(", ")
    )]
    UnacknowledgedClientCode {
        addon: ModuleName,
        client_files: Vec<String>,
        expected_digest: String,
        provided_digest: Option<String>,
    },
}

fn format_location(path: &std::path::Path, line: Option<usize>) -> String {
    match line {
        Some(line) => format!("{}:{line}", path.display()),
        None => path.display().to_string(),
    }
}

/// The conflict of an [`Error::ConcurrentChange`] (review A4-R3: two values, not a string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Conflict {
    /// SQLSTATE `40001`.
    SerializationFailure,
    /// SQLSTATE `40P01`.
    Deadlock,
}

impl Conflict {
    /// The SQLSTATE.
    pub fn sqlstate(self) -> &'static str {
        match self {
            Self::SerializationFailure => "40001",
            Self::Deadlock => "40P01",
        }
    }
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self {
            Self::SerializationFailure => "serialization failure",
            Self::Deadlock => "deadlock",
        };
        write!(f, "{what}, SQLSTATE {}", self.sqlstate())
    }
}

impl Error {
    /// Whether the operation lost to a concurrent transaction and can be run again as it is
    /// ([`Error::ConcurrentChange`], review A4-R2).
    pub fn is_concurrent_change(&self) -> bool {
        matches!(self, Self::ConcurrentChange { .. })
    }

    /// A [`Error::ModelFile`] error.
    pub(crate) fn model_file(
        path: &std::path::Path,
        line: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self::ModelFile {
            path: path.to_path_buf(),
            line,
            message: message.into(),
            source: None,
        }
    }

    /// A [`Error::ModelFile`] error for a declaration the registry refused, keeping its error as
    /// the source (its kind stays inspectable).
    pub(crate) fn model_file_refused(
        path: &std::path::Path,
        line: Option<usize>,
        source: erp_fields::Error,
    ) -> Self {
        Self::ModelFile {
            path: path.to_path_buf(),
            line,
            message: source.message().to_owned(),
            source: Some(Box::new(source)),
        }
    }

    /// The same error naming `path` instead of the relative name it was parsed under.
    pub(crate) fn with_model_file_path(self, path: &std::path::Path) -> Self {
        match self {
            Self::ModelFile {
                line,
                message,
                source,
                ..
            } => Self::ModelFile {
                path: path.to_path_buf(),
                line,
                message,
                source,
            },
            other => other,
        }
    }
}

fn format_module_list(modules: &[ModuleName]) -> String {
    modules
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A limit as the message shows it: whole milliseconds, or "none" for zero (no limit was set).
fn format_limit(limit: Duration) -> String {
    if limit.is_zero() {
        "none".to_owned()
    } else {
        format!("{} ms", limit.as_millis())
    }
}

fn format_missing_tables(tables: &[String]) -> String {
    match tables {
        [one] => format!("table {one} does not exist (it was removed outside ERPForAll)"),
        many => format!(
            "tables {} do not exist (they were removed outside ERPForAll)",
            many.join(", ")
        ),
    }
}

fn format_drops(drops: &[(ModuleName, crate::graph::DropReason)]) -> String {
    drops
        .iter()
        .map(|(m, r)| format!("  - {m}: {r}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_inconsistent(rows: &[InconsistentDependency]) -> String {
    rows.iter()
        .map(|r| {
            format!(
                "  - {} is installed but its dependency {} is '{}'",
                r.module, r.dependency, r.dependency_state
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_category_modules(rows: &[CategoryMember]) -> String {
    rows.iter()
        .map(|m| format!("\n- {} ({})", m.display_name, m.state.label()))
        .collect::<String>()
}

fn format_countries(countries: &[crate::country::CountryCode]) -> String {
    countries
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_file(file: &Option<String>) -> String {
    match file {
        Some(file) => format!(" '{file}'"),
        None => String::new(),
    }
}

fn format_owned(models: &[OwnedRecords]) -> String {
    models
        .iter()
        .map(|o| format!("{}: {}", o.model, o.count))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_stored_states(rows: &[StoredStateRow]) -> String {
    rows.iter()
        .map(|r| format!("{} '{}'", r.id, r.state))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Records of one model that a module being uninstalled still owns through external ids
/// ([`Error::UninstallOwnedRecords`], [`Error::PurgeIncomplete`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct OwnedRecords {
    /// The model (it stays installed).
    pub model: String,
    /// The number of the module's external ids pointing at it.
    pub count: i64,
}

impl OwnedRecords {
    pub(crate) fn new(model: String, count: i64) -> Self {
        Self { model, count }
    }
}

/// A module of an exclusive category ([`Error::ExclusiveCategory`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CategoryMember {
    /// The technical name.
    pub module: ModuleName,
    /// The display name (the manifest `name`), which the message shows.
    pub display_name: String,
    /// The module's state.
    pub state: crate::graph::ModuleState,
}

impl CategoryMember {
    pub(crate) fn new(
        module: ModuleName,
        display_name: String,
        state: crate::graph::ModuleState,
    ) -> Self {
        Self {
            module,
            display_name,
            state,
        }
    }
}

/// The limit named in [`Error::InvalidLimit`] and [`Error::InvalidTimeout`]; each variant is
/// named like the `ModuleSystemBuilder` method that sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitKind {
    /// `ModuleSystemBuilder::max_data_file_bytes`.
    MaxDataFileBytes,
    /// `ModuleSystemBuilder::max_data_files`.
    MaxDataFiles,
    /// `ModuleSystemBuilder::max_operation_data_bytes`.
    MaxOperationDataBytes,
    /// `ModuleSystemBuilder::max_migration_files`.
    MaxMigrationFiles,
    /// `ModuleSystemBuilder::lock_timeout`.
    LockTimeout,
    /// `ModuleSystemBuilder::statement_timeout`.
    StatementTimeout,
    /// `ModuleSystemBuilder::post_commit_statement_timeout`.
    PostCommitStatementTimeout,
    /// `ModuleSystemBuilder::operation_wait_timeout`.
    OperationWaitTimeout,
}

impl LimitKind {
    /// The builder method's name, as the message shows it.
    pub fn name(self) -> &'static str {
        match self {
            Self::MaxDataFileBytes => "max_data_file_bytes",
            Self::MaxDataFiles => "max_data_files",
            Self::MaxOperationDataBytes => "max_operation_data_bytes",
            Self::MaxMigrationFiles => "max_migration_files",
            Self::LockTimeout => "lock_timeout",
            Self::StatementTimeout => "statement_timeout",
            Self::PostCommitStatementTimeout => "post_commit_statement_timeout",
            Self::OperationWaitTimeout => "operation_wait_timeout",
        }
    }
}

impl std::fmt::Display for LimitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A row with a state outside its table's stored states ([`Error::InvalidStoredStates`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StoredStateRow {
    /// The row's id.
    pub id: i64,
    /// Its state.
    pub state: String,
}

impl StoredStateRow {
    pub(crate) fn new(id: i64, state: String) -> Self {
        Self { id, state }
    }
}

fn format_discovery_errors(errors: &[Error]) -> String {
    errors
        .iter()
        .map(|e| format!("  - {e}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// An installed module whose dependency is not installed (load mode, [`Error::InconsistentStates`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct InconsistentDependency {
    /// The installed module (or one with a pending upgrade or removal).
    pub module: ModuleName,
    /// Its dependency.
    pub dependency: ModuleName,
    /// The dependency's state (`uninstalled` or `to install`).
    pub dependency_state: crate::graph::ModuleState,
}

impl InconsistentDependency {
    pub(crate) fn new(
        module: ModuleName,
        dependency: ModuleName,
        dependency_state: crate::graph::ModuleState,
    ) -> Self {
        Self {
            module,
            dependency,
            dependency_state,
        }
    }
}

/// The step of a module that failed ([`Error::StepFailed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModuleStep {
    /// A hook named in the manifest.
    Hook {
        /// Which hook.
        kind: crate::hooks::HookKind,
        /// Its name in the manifest.
        name: &'static str,
    },
    /// Creating or altering the tables of the module's affected models.
    Schema,
    /// `init()` of one affected model.
    ModelInit {
        /// The model.
        model: &'static str,
    },
    /// A data file (`file` names it).
    DataFile,
    /// A demo file (`file` names it).
    DemoFile,
    /// `_register_hook()` of one model, after the registry is complete; the error names the module
    /// that declares the model (`_name`), whichever override failed.
    RegisterHook {
        /// The model.
        model: &'static str,
    },
    /// A migration script step (pre, post, or end).
    Migration {
        /// Which stage: pre, post, end.
        stage: crate::migrations::MigrationStage,
    },
}

impl std::fmt::Display for ModuleStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hook { kind, name } => write!(f, "{kind} '{name}'"),
            Self::Schema => f.write_str("schema update"),
            Self::ModelInit { model } => write!(f, "init() of {model}"),
            Self::DataFile => f.write_str("data file"),
            Self::DemoFile => f.write_str("demo file"),
            Self::RegisterHook { model } => write!(f, "_register_hook() of {model}"),
            Self::Migration { stage } => write!(f, "{stage} migration"),
        }
    }
}

/// The kind of addon source named in [`Error::DataSourceUnsupported`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceKind {
    /// Compiled into the binary.
    Embedded,
    /// An `.erpaddon` package in the database store.
    Package,
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Embedded => "embedded",
            Self::Package => "package",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModuleOperation {
    /// Installing the module.
    Install,
    /// Upgrading the module.
    Upgrade,
}

impl std::fmt::Display for ModuleOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
        })
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Why a WebAssembly addon lifecycle operation was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum WasmReason {
    /// The package has no `[wasm]` section or no `wasm/<name>.wasm` component.
    #[error("the package is not a WebAssembly addon: {0}")]
    NotWasm(String),
    /// A dependency is not installed (the lifecycle never installs dependencies on its own).
    #[error("dependencies not installed: [{}]", .0.join(", "))]
    DependsNotInstalled(Vec<String>),
    /// What `describe()` says differs from the manifest.
    #[error("describe() disagrees with the manifest: {0}")]
    DescribeMismatch(String),
    /// A contribution or attribute this build cannot run yet.
    #[error("not supported in this build: {0}")]
    Unsupported(String),
    /// A manifest capability this host cannot provide yet.
    #[error("capability '{0}' is approved but this host cannot enforce it yet")]
    CapabilityUnavailable(String),
    /// Other installed addons depend on it.
    #[error("installed addons depend on it: [{}]", .0.join(", "))]
    HasDependents(Vec<String>),
    /// The operation would also change modules the request did not name.
    #[error("the operation would also change [{}], which was not requested", .0.join(", "))]
    UnexpectedChange(Vec<String>),
    /// The addon is already installed / not installed / not upgradable as asked.
    #[error("{0}")]
    State(String),
    /// A stored package no longer matches what was installed.
    #[error("the stored package is not what was installed: {0}")]
    StoredPackage(String),
    /// The component could not be loaded or run.
    #[error("component: {0}")]
    Component(String),
    /// `describe()` lists more models, fields or methods than one package may contribute.
    #[error("describe() is over the limits of one package: {0}")]
    TooLarge(String),
    /// A data file the manifest lists is missing from the archive or is of a kind a package
    /// cannot carry.
    #[error("data file: {0}")]
    DataFile(String),
    /// A typed model reference (`model_ref!` with fields) names a model or field the installed
    /// registry does not have, or one of another kind or comodel.
    #[error("a typed model reference does not match the installed models: {0}")]
    ModelRef(String),
}
