//! ERPForAll module manifests, discovery, dependency graph, and lifecycle.

pub mod artifact_identity;
mod artifact_paths;
pub mod base_models;
pub mod company_access;
pub mod country;
pub mod data_loader;
pub mod dependencies;
pub mod discovery;
pub mod error;
pub mod graph;
pub mod hooks;
pub mod install;
pub mod lifecycle;
pub mod lifecycle_hook;
pub mod loader;
pub mod manifest;
mod migration_sql;
pub mod migrations;
pub mod model_files;
mod node_digest;
pub mod pack;
pub mod package;
pub mod package_runtime;
pub mod post_commit;
mod safe_read;
mod store;
pub mod system;
pub mod version;
mod view_types;
pub mod wasm_lifecycle;

pub use country::CountryCode;
pub use data_loader::ErpDataLoader;
pub use dependencies::ExternalDependencies;
pub use discovery::{
    AddonEntry, AddonIndex, AddonSourceKind, PackageData, PackageFiles, read_manifest_from_dir,
};
pub use error::{
    CategoryMember, Error, InconsistentDependency, LimitKind, ModuleOperation, ModuleStep,
    OwnedRecords, Result, SourceKind, StoredStateRow, WasmReason,
};
pub use graph::{DropReason, DropReport, LoadMode, ModuleGraph, ModuleNode, ModuleState};
pub use hooks::{HOOKS, HookFn, HookImpl, HookKind};
pub use lifecycle::{
    ActionTarget, ActionType, Catalog, CategoryPath, DependencyRow, InstallOptions, ModuleAction,
    ModuleInfo, ModuleRow, StoredModule, UpdateListResult,
};
pub use lifecycle_hook::{LifecycleChange, LifecycleHook};
pub use loader::{
    DataFile, DataKind, DataLoader, FileLoader, LoadSession, LoadedNames, Operations,
    SessionContext, SessionMark,
};
pub use manifest::{AutoInstall, HookName, Manifest, ModuleName, validate_module_name};
pub use migrations::{
    MIGRATIONS, MigrationFn, MigrationImpl, MigrationStage, order_migration_versions,
    parse_migration_folder_version, should_run_migration,
};
pub use model_files::{
    MAX_MODEL_FILE_BYTES, MAX_MODEL_FILES, MAX_MODEL_FILES_TOTAL_BYTES, MAX_MODELS_PER_ADDON,
    MAX_MODELS_PER_FILE, MODELS_DIR, addon_descriptor, parse_model_file, read_model_files,
};
pub use package::{
    AdminConsent, ConsentedPackage, ErpAddonPackage, ErpAddonPackageWriter, PackageLimits,
    PackageRecord, PackageSummary, WasmCapability, WasmSection, create_package_table_sql,
    delete_package, get_package_by_name, get_package_by_name_version, get_package_by_sha256,
    init_package_table, list_packages, store_package,
};
pub use package_runtime::{PackageChange, PackageChecks, PackageRuntime};
pub use post_commit::{
    CreateIndex, IndexColumn, IndexMethod, IndexName, PostCommitAction, SchemaStepRef, TableName,
};
pub use system::{
    ClientAction, DATABASE_CREATE_OPTIONS, DatabaseName, ForcedUpdate, HumanCatalog, InitOptions,
    InvalidPostCommitRow, LoadOutcome, MAX_DATA_FILES, MAX_OPERATION_DATA_BYTES,
    MAX_POST_COMMIT_ATTEMPTS, MAX_POST_COMMIT_PASSES, MAX_REPORTED_ROWS, ModuleSystem,
    ModuleSystemBuilder, OPERATION_LOCK, OperationOutcome, POST_COMMIT_LOCK, PendingPostCommit,
    PendingState, PostCommitFailure, PostCommitKey, PostCommitReport, PostCommitStep,
    PostCommitStop, SchemaStepDone, SchemaStepOutcome, create_database, revoke_post_commit_access,
    schedule_post_commit, validate_database_name,
};
pub use version::{ERP_SERIES, ModuleVersion, ODOO_SERIES, ODOO_SERIES_PREFIX, SemverVersion};
pub use view_types::{ViewTypeInputs, view_type_factory};

pub use system::{
    DroppedModelImpact, ImpactFingerprint, MAX_UNINSTALL_CANONICAL_BYTES,
    MAX_UNINSTALL_IMPACT_BYTES, MAX_UNINSTALL_TARGETS, MetadataImpact,
    UNINSTALL_STATEMENT_DEADLINE_MS, UninstallCompositionBlocker, UninstallImpact,
};

/// Typed count-only native uninstall guards (A7).
pub mod sql_uninstall;
