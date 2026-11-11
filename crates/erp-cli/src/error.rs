//! Typed errors and exit codes for ERPForAll CLI operations.

use erp_modules::ModuleName;
use std::path::PathBuf;
use std::process::ExitCode;

/// Result type for ERPForAll CLI operations.
pub type Result<T> = std::result::Result<T, CliError>;

fn format_cycle(cycle: &[ModuleName]) -> String {
    cycle
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn format_multiple_errors(errors: &[CliError]) -> String {
    let mut buf = format!("Addon discovery failed with {} error(s):", errors.len());
    for e in errors {
        buf.push_str(&format!("\n  - {e}"));
    }
    buf
}

/// The message of a module-system error followed by the messages of its sources
/// (`Module "x": schema update failed: <cause>`), because the CLI prints one line.
fn format_chain(error: &erp_modules::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        // Wrappers such as `Orm(#[error(transparent)])` repeat their source's message.
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

fn format_stop(stopped: &Option<String>) -> String {
    match stopped {
        Some(reason) => format!("; the phase stopped early: {reason}"),
        None => String::new(),
    }
}

/// Exit code of a module-system error.
///
/// - 1: the request was refused (unknown module, not installed, incompatible, not initialized
///   database, ...): fix the request and run again.
/// - 3: another module operation runs or a database lock/statement timeout hit: run again later.
/// - 4: the operation started and was rolled back (a hook, schema, data or purge step failed):
///   nothing persists.
/// - 2: anything else (database or I/O failures, broken invariants).
fn operation_exit_code(error: &erp_modules::Error) -> ExitCode {
    use erp_modules::Error as E;
    match error {
        E::UnknownModule { .. }
        | E::UnknownDependency { .. }
        | E::DependencyNotInstallable { .. }
        | E::ModuleNotInstallable { .. }
        | E::ModuleUnavailable { .. }
        | E::DependencyRecursion
        | E::IncompatibleModules { .. }
        | E::ExclusiveCategory { .. }
        | E::UpgradeNotInstalled { .. }
        | E::UpgradeUnknownDependency { .. }
        | E::CannotUninstall { .. }
        | E::AlreadyUninstalled
        | E::DatabaseNotInitialized { .. }
        | E::DatabaseAlreadyInitialized { .. }
        | E::UninstallOwnedRecords { .. }
        | E::UninstallUnavailable { .. }
        | E::UninstallNotInstallable { .. }
        | E::CompanyCountriesUnknown { .. }
        | E::ExternalDependencyMissing { .. }
        | E::AccessDenied { .. }
        | E::ForeignEnvironment { .. }
        | E::InvalidManifest { .. }
        | E::InvalidVersion { .. }
        | E::AutoInstallTriggerMismatch { .. }
        | E::HookMissing { .. }
        | E::HookDuplicate { .. }
        | E::HookOrphan { .. }
        | E::AddonCodeMismatch { .. }
        | E::DuplicateAddon { .. }
        | E::DataLoaderMissing { .. }
        | E::WasmAddon { .. }
        | E::ConsentMismatch { .. }
        | E::UnapprovedCapabilities { .. }
        | E::UnacknowledgedClientCode { .. }
        | E::PackageIntegrity { .. }
        | E::PackageFormat { .. }
        | E::PackageLimitExceeded { .. } => ExitCode::from(1),
        E::OperationInProgress
        | E::LockNotAvailable { .. }
        | E::StatementCanceled { .. }
        | E::OperationWaitTimeout { .. } => ExitCode::from(3),
        E::StepFailed { .. }
        | E::PurgeFailed { .. }
        | E::PurgeIncomplete { .. }
        | E::SchemaDropBlocked { .. }
        | E::SharedSchemaTable { .. }
        | E::SchemaTablesMissing { .. }
        | E::ResetFailed { .. }
        | E::RollbackFailed { .. } => ExitCode::from(4),
        // The database or the engine failing is a system error (2); a refusal of the ORM is the
        // caller's (1), as for `erp access`.
        E::Orm(inner) => orm_exit_code(inner),
        // `Error` is `#[non_exhaustive]`: a variant added later is a failure (2) until it is
        // classified here, never a success.
        _ => ExitCode::from(2),
    }
}

fn orm_exit_code(error: &erp_fields::Error) -> ExitCode {
    match error.kind() {
        erp_fields::ErrorKind::Missing
        | erp_fields::ErrorKind::Validation
        | erp_fields::ErrorKind::UserError
        | erp_fields::ErrorKind::Access
        | erp_fields::ErrorKind::AccessDenied => ExitCode::from(1),
        _ => ExitCode::from(2),
    }
}

/// Errors that can occur during CLI command execution.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CliError {
    /// An invalid technical module name was provided.
    #[error("Invalid module name '{name}': {reason}")]
    InvalidModuleName {
        /// The invalid module name string.
        name: String,
        /// Reason why the name is invalid.
        reason: String,
    },

    /// An invalid CLI argument or configuration value was provided.
    #[error("Invalid argument '{arg}': {reason}")]
    InvalidInput {
        /// Argument name or flag.
        arg: String,
        /// Reason why the input is invalid.
        reason: String,
    },

    /// The scaffold destination directory already exists and is not empty.
    #[error("Destination directory '{0}' already exists and is not empty")]
    DestinationNotEmpty(PathBuf),

    /// The scaffold destination exists but is a file, not a directory.
    #[error("Destination '{0}' exists and is a file, not a directory")]
    DestinationIsFile(PathBuf),

    /// The scaffold destination is a symlink.
    #[error("Destination '{0}' is a symlink; symlinks are not allowed as scaffold destinations")]
    DestinationIsSymlink(PathBuf),

    /// The scaffold destination is a special file (FIFO, socket, device node) rather than
    /// a regular file or directory.
    #[error(
        "Destination '{0}' is a special file (FIFO, socket, or device); not a valid scaffold destination"
    )]
    DestinationNotRegularFile(PathBuf),

    /// An invalid database identifier was provided.
    #[error(
        "Invalid database name '{0}': must be 1..=63 ASCII alphanumeric or underscore characters"
    )]
    InvalidDatabaseName(String),

    /// A requested module was not found in any addon search path or embedded set.
    #[error("Module '{0}' not found in addon search paths")]
    ModuleNotFound(ModuleName),

    /// A required module dependency is missing.
    #[error("Module '{module}' requires missing dependency '{dependency}'")]
    MissingDependency {
        /// Module requesting the dependency.
        module: ModuleName,
        /// Missing dependency module name.
        dependency: ModuleName,
    },

    /// A dependency cycle was detected.
    #[error("Dependency cycle detected among modules: {}", format_cycle(.0))]
    DependencyCycle(Vec<ModuleName>),

    /// Module dependency resolution or cycle detection failed.
    #[error("Module dependency resolution failed: {0}")]
    DependencyResolution(String),

    /// An I/O error occurred while operating on a file or directory.
    #[error("IO error at '{path}': {source}")]
    Io {
        /// Path where the I/O error occurred.
        path: PathBuf,
        /// Underlying std::io::Error.
        #[source]
        source: std::io::Error,
    },

    /// A module manifest error occurred.
    #[error("Manifest error in '{path}': {source}")]
    Manifest {
        /// Path to the manifest file or directory.
        path: PathBuf,
        /// Underlying erp-modules error (boxed: `erp_modules::Error` is large).
        #[source]
        source: Box<erp_modules::Error>,
    },

    /// An error occurred during module lifecycle execution.
    #[error("Execution error: {0}")]
    Execution(String),

    /// Multiple addon discovery errors. Preserves all underlying errors and reports
    /// exit code 2 (system/IO) when any error is an I/O error, otherwise exit code 1.
    #[error("{}", format_multiple_errors(.0))]
    MultipleDiscoveryErrors(Vec<CliError>),

    /// The module system refused or failed a live module operation (`erp module install`,
    /// `upgrade`, `uninstall`). The message carries the causes; the exit code tells a refusal (1)
    /// from a busy database (3) and from a rolled-back operation (4).
    #[error("{}", format_chain(.0))]
    ModuleOperation(#[source] Box<erp_modules::Error>),

    /// The module operation committed, but its post-commit phase did not finish cleanly; the
    /// steps that did not run stay pending and run again after the next module operation.
    #[error(
        "The operation committed, but its post-commit phase did not finish cleanly: {failed} step(s) failed{}",
        format_stop(.stopped)
    )]
    PostCommitIncomplete {
        /// Steps that failed.
        failed: usize,
        /// Why the phase stopped early, when it did.
        stopped: Option<String>,
    },

    /// A tool or toolchain component `erp addon build` needs is not installed.
    #[error("{tool} is not available ({detail}); install it with: {install}")]
    ToolchainMissing {
        /// What is missing (`the wasm32-wasip2 target`, `rustc`).
        tool: String,
        /// What was observed.
        detail: String,
        /// The command that installs it.
        install: String,
    },

    /// The addon has no `[wasm]` section in its manifest, so it is not a WASM addon.
    #[error(
        "Addon '{addon}' ({}) has no [wasm] section in manifest.toml: only WASM addons are built and packaged here",
        .path.display()
    )]
    NotWasmAddon {
        /// The addon.
        addon: ModuleName,
        /// Its manifest.
        path: PathBuf,
    },

    /// `cargo` failed or produced no component.
    #[error("Building addon '{addon}' failed: {detail}")]
    BuildFailed {
        /// The addon.
        addon: ModuleName,
        /// What happened.
        detail: String,
    },

    /// The component built, but the WASM host could not read its description.
    #[error("Addon '{addon}': the component could not be checked by the WASM host: {source}")]
    WasmHost {
        /// The addon.
        addon: ModuleName,
        /// The host's error.
        #[source]
        source: erp_wasm_host::HostError,
    },

    /// The component's `describe()` is not usable or misses addons it must contain.
    #[error("Addon '{addon}': {detail}")]
    DescribeMismatch {
        /// The addon.
        addon: ModuleName,
        /// What is wrong, naming the missing addons.
        detail: String,
    },

    /// `erp addon package` has no component to pack.
    #[error(
        "Addon '{addon}' declares [wasm] but '{}' does not exist; run `erp addon build {addon} --target wasm` first",
        .path.display()
    )]
    WasmBinaryMissing {
        /// The addon.
        addon: ModuleName,
        /// Where the component is expected.
        path: PathBuf,
    },

    /// The package writer or reader refused the archive.
    #[error("Package '{path}': {source}")]
    Package {
        /// The archive or addon directory concerned.
        path: PathBuf,
        /// The module system's error (boxed: large).
        #[source]
        source: Box<erp_modules::Error>,
    },

    /// The archive read back differs from what was written.
    #[error("Package '{path}' does not read back as written: {detail}")]
    PackageReadBack {
        /// The archive.
        path: PathBuf,
        /// The difference.
        detail: String,
    },

    /// An `erp access` action failed: the error of the access engine, the ORM or the database
    /// (boxed: `erp_fields::Error` is large).
    #[error("{0}")]
    Access(#[source] Box<erp_fields::Error>),
}

/// A node registry error (`erp cluster`, `erp dev -d`) carries its kind: a refusal (a rollout
/// window refused, `erp dev` on a served database) exits 1, a missing table or a database fault 2.
impl From<erp_orm::node::NodeError> for CliError {
    fn from(error: erp_orm::node::NodeError) -> Self {
        Self::Access(Box::new(error.into()))
    }
}

impl From<erp_fields::Error> for CliError {
    fn from(error: erp_fields::Error) -> Self {
        Self::Access(Box::new(error))
    }
}

impl CliError {
    /// Return the appropriate standard process exit code for this error.
    ///
    /// - Code 1: User, input, validation (including invalid database name `-d`), resolution, or missing module errors.
    /// - Code 2: System, I/O, usage, or internal execution errors.
    /// - Code 3: a live module operation found the database busy (another operation, lock or
    ///   statement timeout); run it again later.
    /// - Code 4: a live module operation failed in a step and was rolled back.
    /// - Code 5: a live module operation committed, but its post-commit phase did not finish.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::InvalidModuleName { .. }
            | Self::InvalidInput { .. }
            | Self::DestinationNotEmpty(_)
            | Self::DestinationIsFile(_)
            | Self::DestinationIsSymlink(_)
            | Self::DestinationNotRegularFile(_)
            | Self::InvalidDatabaseName(_)
            | Self::ModuleNotFound(_)
            | Self::MissingDependency { .. }
            | Self::DependencyCycle(_)
            | Self::DependencyResolution(_)
            | Self::Manifest { .. }
            | Self::ToolchainMissing { .. }
            | Self::NotWasmAddon { .. }
            | Self::BuildFailed { .. }
            | Self::WasmHost { .. }
            | Self::DescribeMismatch { .. }
            | Self::WasmBinaryMissing { .. }
            | Self::Package { .. } => ExitCode::from(1),
            // the archive just written does not read back: a fault of this tool, not of the input
            Self::PackageReadBack { .. } => ExitCode::from(2),
            Self::Io { .. } | Self::Execution(_) => ExitCode::from(2),
            Self::ModuleOperation(error) => operation_exit_code(error),
            Self::PostCommitIncomplete { .. } => ExitCode::from(5),
            // the caller's mistake (unknown or wrong external id) or a refusal: 1; the
            // database or the engine failing: 2
            Self::Access(error) => orm_exit_code(error),
            Self::MultipleDiscoveryErrors(errors) => {
                // Exit code 2 if any underlying error is I/O; otherwise 1.
                if errors.iter().any(|e| matches!(e, Self::Io { .. })) {
                    ExitCode::from(2)
                } else {
                    ExitCode::from(1)
                }
            }
        }
    }
}
