//! Command-line argument definitions for ERPForAll CLI.

use crate::error::CliError;
use clap::{Args, Parser, Subcommand, ValueEnum};
use erp_modules::ModuleName;
use erp_orm::{BuildTimeout, LockTimeout, QualifiedXmlId};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// Validated PostgreSQL database identifier (1..=63 ASCII alphanumeric characters or underscores).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DatabaseName(String);

impl DatabaseName {
    /// Construct a new `DatabaseName`, validating naming constraints.
    pub fn new(name: impl Into<String>) -> Result<Self, CliError> {
        let name = name.into();
        Self::validate(&name)?;
        Ok(Self(name))
    }

    /// Validate database identifier syntax.
    fn validate(name: &str) -> Result<(), CliError> {
        if name.is_empty() || name.len() > 63 {
            return Err(CliError::InvalidDatabaseName(name.to_string()));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(CliError::InvalidDatabaseName(name.to_string()));
        }
        Ok(())
    }

    /// Return reference to string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Extract inner String.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for DatabaseName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DatabaseName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for DatabaseName {
    type Err = CliError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

fn parse_module_name(s: &str) -> std::result::Result<ModuleName, String> {
    ModuleName::new(s).map_err(|e| e.to_string())
}

/// ERPForAll command-line interface.
#[derive(Parser, Debug, Clone, PartialEq, Eq)]
#[command(
    name = "erp",
    version,
    about = "ERPForAll command-line interface for development, module operations, and scaffolding."
)]
pub struct Cli {
    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Commands,
}

/// Top-level subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Commands {
    /// Serve the linked native application using explicit protected configuration.
    Server(ServerArgs),
    /// Inspect ownership, recover a contested store, or verify an operator-run peer.
    Attachments(AttachmentCommand),
    /// Scaffold a new addon skeleton
    Scaffold(ScaffoldArgs),

    /// Manage modules on a target database (install, upgrade, uninstall)
    Module(ModuleCommand),

    /// Build and package WASM addons (`erp addon build --target wasm`, `erp addon package`)
    Addon(AddonCommand),

    /// Launch development environment or development server
    Dev(DevArgs),

    /// Inspect and repair the access policy of a database (as the System principal)
    Access(AccessCommand),

    /// Database operations run by an operator (`erp db build-object`)
    Db(DbCommand),

    /// The audit trail (`erp audit maintain`)
    Audit(AuditCommand),

    /// Operator credential actions on the owner connection (`erp auth emergency-revoke`)
    Auth(AuthCommand),

    /// The nodes serving a database and its rollout window (`erp cluster nodes`,
    /// `erp cluster rollout open|close|status`)
    Cluster(ClusterCommand),
}

/// Arguments for `erp scaffold`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScaffoldArgs {
    /// Technical name of the addon (must match ^[a-zA-Z0-9_]{1,256}$)
    pub name: String,

    /// Destination directory (defaults to ./addons/<name>)
    #[arg(short, long)]
    pub dest: Option<PathBuf>,

    /// Create a data-only addon without Cargo.toml or Rust source files
    #[arg(long)]
    pub data_only: bool,

    /// Author name written to manifest.toml
    #[arg(long, default_value = "ERPForAll Contributor")]
    pub author: String,

    /// Short summary of the addon written to manifest.toml
    #[arg(long)]
    pub summary: Option<String>,
}

/// Module subcommand container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModuleCommand {
    /// Module action to perform
    #[command(subcommand)]
    pub action: ModuleAction,
}

/// Module actions (`install`, `upgrade`, `uninstall`).
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModuleAction {
    /// Install one or more modules into a target database
    Install(ModuleOpArgs),

    /// Upgrade one or more modules in a target database
    Upgrade(ModuleOpArgs),

    /// Uninstall one or more modules from a target database
    Uninstall(ModuleOpArgs),

    /// Explicitly enroll reviewed installed addon artifacts (owner credentials only).
    IdentityEnroll(IdentityEnrollArgs),

    /// Publish content identity only after a successful forward release activation.
    IdentityActivate(IdentityActivateArgs),
}

/// Explicit legacy or recovery enrollment; never used by the request runtime.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IdentityEnrollArgs {
    /// Protected native server configuration, including the owner connection.
    #[arg(long)]
    pub config: PathBuf,
    /// Audited operator identity.
    #[arg(long)]
    pub operator: String,
    /// Reason and provenance reviewed by the operator.
    #[arg(long)]
    pub note: String,
    /// Enroll the complete installed set from an explicitly reviewed legacy release.
    #[arg(long, requires = "legacy_release", conflicts_with = "modules")]
    pub installed: bool,
    /// Immutable OLD release with no per-addon identities; its files supply enrollment only.
    #[arg(long, requires = "installed", conflicts_with = "modules")]
    pub legacy_release: Option<PathBuf>,
    /// Installed addon names to enroll explicitly (ordinary recovery, not legacy bootstrap).
    #[arg(required_unless_present = "installed", value_parser = parse_module_name)]
    pub modules: Vec<ModuleName>,
}

/// Internal forward activation publication, separate from restore and rollback.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IdentityActivateArgs {
    /// Protected native server configuration.
    #[arg(long)]
    pub config: PathBuf,
    /// Verified active candidate release directory.
    #[arg(long)]
    pub release: PathBuf,
    /// Verified previous release directory.
    #[arg(long)]
    pub previous: PathBuf,
    /// Protected successful forward migration receipt.
    #[arg(long)]
    pub upgrade_receipt: PathBuf,
}

/// Arguments for module operations (`install`, `upgrade`, `uninstall`).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModuleOpArgs {
    /// Target PostgreSQL database name
    #[arg(short = 'd', long = "database")]
    pub database: DatabaseName,

    /// Technical module names to operate on
    #[arg(required = true, value_parser = parse_module_name)]
    pub modules: Vec<ModuleName>,

    /// Addon search paths (comma-separated or repeated)
    #[arg(
        long = "addons-path",
        value_delimiter = ',',
        default_value = "./addons"
    )]
    pub addons_path: Vec<PathBuf>,

    /// Validate and resolve module graph without executing database changes
    #[arg(long)]
    pub dry_run: bool,

    /// With `upgrade`: rewrite generated company restrictions that were changed since they were
    /// generated, and list each one, instead of refusing the update
    #[arg(long)]
    pub restore_generated_restrictions: bool,

    /// Verified previous release for an explicit forward migration; upgrade only.
    #[arg(long)]
    pub identity_previous: Option<PathBuf>,

    /// With `install`: the demo data choice (the database's stored choice decides without it).
    #[command(flatten)]
    pub demo: DemoArgs,
}

/// The demo data choice of `erp db init` and `erp module install`. The choice is stored on the
/// database (the `demo` flag of its modules, `base` first); a module installed later gets demo
/// data when every module it depends on has it.
#[derive(Args, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DemoArgs {
    /// Load demo data. `erp db init`: the new database is a demo database. `erp module install`
    /// on a database without demo data: the demo data of every installed addon is loaded first
    /// (irreversible), then the addons install with theirs
    #[arg(long, conflicts_with = "without_demo")]
    pub with_demo: bool,

    /// No demo data (the default of `erp db init`). `erp module install` refuses it on a
    /// database that has demo data: demo data cannot be switched off
    #[arg(long)]
    pub without_demo: bool,
}

impl DemoArgs {
    /// `Some(true)` for `--with-demo`, `Some(false)` for `--without-demo`, `None` for neither.
    pub fn choice(self) -> Option<bool> {
        if self.with_demo {
            Some(true)
        } else if self.without_demo {
            Some(false)
        } else {
            None
        }
    }
}

/// `erp addon` subcommand container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AddonCommand {
    /// Addon action to perform
    #[command(subcommand)]
    pub action: AddonAction,
}

/// `erp addon` actions.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddonAction {
    /// Build the addon's crate to a component for the given target and check what it describes
    Build(AddonBuildArgs),

    /// Pack an addon directory into a verified `.erpaddon` archive
    Package(AddonPackageArgs),

    /// Install a `.erpaddon` package into a running database, without a restart
    Install(AddonInstallArgs),

    /// Upgrade an installed package addon to a newer `.erpaddon` package
    Upgrade(AddonInstallArgs),

    /// Uninstall a package addon and its data
    Uninstall(AddonUninstallArgs),
}

/// Arguments for `erp addon install` and `erp addon upgrade`: the package, the database it goes
/// to, and what the administrator approves. Nothing is approved by default.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AddonInstallArgs {
    /// The `.erpaddon` package file
    pub file: PathBuf,

    /// Target database; it must be the database the configuration names
    #[arg(short = 'd', long = "database")]
    pub database: DatabaseName,

    /// Deployment configuration file of the server (the same file as `erp server --config`):
    /// the owner connection, the addon directories and the runtime role that is granted the
    /// package's tables
    #[arg(long)]
    pub config: PathBuf,

    /// The capabilities the administrator approves, exactly the ones the package requests
    /// (comma-separated); `none` approves nothing. There is no default: omitting the option is
    /// refused
    #[arg(long, required = true, value_delimiter = ',')]
    pub approve: Vec<String>,

    /// The SHA-256 digest of the package's client code (`static/dist/`) the administrator has
    /// reviewed; required when the package ships client code, refused when it does not
    #[arg(long)]
    pub ack_client_digest: Option<String>,
}

/// Arguments for `erp addon uninstall`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AddonUninstallArgs {
    /// Technical name of the package addon
    #[arg(value_parser = parse_module_name)]
    pub name: ModuleName,

    /// Target database; it must be the database the configuration names
    #[arg(short = 'd', long = "database")]
    pub database: DatabaseName,

    /// Deployment configuration file of the server
    #[arg(long)]
    pub config: PathBuf,
}

/// What `erp addon build` produces.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildTarget {
    /// A WebAssembly component (`wasm32-wasip2`), written to `wasm/<name>.wasm` of the addon
    Wasm,
}

/// Arguments for `erp addon build`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AddonBuildArgs {
    /// Addon directory, or technical name looked up in the addon search paths
    pub addon: String,

    /// Build target (the only one is `wasm`)
    #[arg(long, value_enum)]
    pub target: BuildTarget,

    /// Addon search paths for the name and for dependencies (comma-separated or repeated); the
    /// parent directory of the addon is always searched too
    #[arg(
        long = "addons-path",
        value_delimiter = ',',
        default_value = "./addons"
    )]
    pub addons_path: Vec<PathBuf>,

    /// Cargo target directory (defaults to the one cargo picks for the addon's crate)
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
}

/// Arguments for `erp addon package`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AddonPackageArgs {
    /// Addon directory, or technical name looked up in the addon search paths
    pub addon: String,

    /// Output file (defaults to ./<name>-<version>.erpaddon)
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Addon search paths (comma-separated or repeated)
    #[arg(
        long = "addons-path",
        value_delimiter = ',',
        default_value = "./addons"
    )]
    pub addons_path: Vec<PathBuf>,

    /// Component to pack instead of `<addon>/wasm/<name>.wasm` (the output of `erp addon build`)
    #[arg(long)]
    pub wasm: Option<PathBuf>,
}

/// Arguments for `erp dev`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DevArgs {
    /// Target PostgreSQL database name
    #[arg(short = 'd', long = "database")]
    pub database: Option<DatabaseName>,

    /// Addon search paths (comma-separated or repeated)
    #[arg(
        long = "addons-path",
        value_delimiter = ',',
        default_value = "./addons"
    )]
    pub addons_path: Vec<PathBuf>,

    /// HTTP port for the development server
    #[arg(short, long, default_value_t = 8069)]
    pub port: u16,
}

/// `erp access` subcommand container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccessCommand {
    /// Access action to perform
    #[command(subcommand)]
    pub action: AccessAction,
}

/// Access actions (`report`, `disable-rule`).
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccessAction {
    /// Print what the access policy of the database holds
    Report(AccessArgs),

    /// Disable (active = false) the access rule with this external id: the recovery path for a
    /// rule that cannot be compiled
    DisableRule(DisableRuleArgs),
}

/// The database an `erp access` action works on. The connection credentials come from the
/// `DATABASE_URL` environment variable (never from the command line); `-d` names the database.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccessArgs {
    /// Target PostgreSQL database name
    #[arg(short = 'd', long = "database")]
    pub database: DatabaseName,
}

/// Arguments for `erp access disable-rule`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DisableRuleArgs {
    /// The database.
    #[command(flatten)]
    pub db: AccessArgs,

    /// External id of the rule, `module.name`
    #[arg(value_parser = parse_xmlid)]
    pub xmlid: QualifiedXmlId,
}

/// `erp audit` subcommand container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuditCommand {
    /// Audit action to perform
    #[command(subcommand)]
    pub action: AuditAction,
}

/// Audit trail actions.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditAction {
    /// Install or verify the audit table and refresh its partition window (today plus 31 days),
    /// as the owner role. Idempotent; runtime grant publication does the same.
    Maintain(ServerArgs),
}

/// `erp auth` subcommand container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthCommand {
    /// Auth action to perform
    #[command(subcommand)]
    pub action: AuthAction,
}

/// Operator auth actions.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthAction {
    /// End API keys or sessions on the owner connection, with its own audit row in the same
    /// transaction. Refuses when the connection does not own the credential tables or when the
    /// audit store cannot take the row; there is no unaudited revocation.
    EmergencyRevoke(EmergencyRevokeArgs),
}

/// Why the operator revokes (`--reason`): the closed list the audit row can hold.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EmergencyReasonArg {
    /// A credential is known or suspected to be in the wrong hands.
    Compromised,
    /// A device or a stored credential was lost.
    Lost,
    /// The person or service left; their access ends.
    Offboarding,
    /// Any other incident response.
    Incident,
}

impl EmergencyReasonArg {
    /// The audit reason this argument stands for.
    pub fn reason(self) -> erp_orm::audit::EmergencyReason {
        match self {
            Self::Compromised => erp_orm::audit::EmergencyReason::Compromised,
            Self::Lost => erp_orm::audit::EmergencyReason::Lost,
            Self::Offboarding => erp_orm::audit::EmergencyReason::Offboarding,
            Self::Incident => erp_orm::audit::EmergencyReason::Incident,
        }
    }
}

/// Arguments for `erp auth emergency-revoke`: exactly one target group (`--key`, `--session` or
/// `--user`); `--user` needs `--keys`, `--sessions` or both. The command never takes a key secret,
/// prefix or name.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[command(
    group(
        clap::ArgGroup::new("target")
            .required(true)
            .multiple(false)
            .args(["key", "session", "user"])
    ),
    group(clap::ArgGroup::new("user_scope").multiple(true).args(["keys", "sessions"]))
)]
#[non_exhaustive]
pub struct EmergencyRevokeArgs {
    /// Deployment configuration file (its owner connection is used).
    #[arg(long)]
    pub config: PathBuf,
    /// Why the credentials are revoked.
    #[arg(long, value_enum)]
    pub reason: EmergencyReasonArg,
    /// API key ids to revoke (at most 500).
    #[arg(long = "key", num_args = 1.., value_name = "ID")]
    pub key: Vec<i64>,
    /// Session ids (UUIDs) to revoke (at most 500).
    #[arg(long = "session", num_args = 1.., value_name = "UUID")]
    pub session: Vec<String>,
    /// Revoke the credentials of this user id (needs `--keys`, `--sessions` or both).
    #[arg(long = "user", value_name = "UID", requires = "user_scope")]
    pub user: Option<i64>,
    /// With `--user`: revoke all the user's API keys (500 per transaction and audit row).
    // `requires = "user"` alone is not enough: clap counts a missing required argument as
    // satisfied when it conflicts with an argument that is present (here `--key`), so the
    // conflict with the other target groups is stated on the flag itself.
    #[arg(long, requires = "user", conflicts_with_all = ["key", "session"])]
    pub keys: bool,
    /// With `--user`: revoke all the user's sessions.
    #[arg(long, requires = "user", conflicts_with_all = ["key", "session"])]
    pub sessions: bool,
}

/// `erp db` subcommand container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DbCommand {
    /// Database action to perform
    #[command(subcommand)]
    pub action: DbAction,
}

/// Database actions run by an operator.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DbAction {
    /// Initialize native base/web and create the supplied administrator.
    Init(InitArgs),
    /// Publish native runtime privileges after a CLI module operation and verify them.
    PublishGrants(ServerArgs),
    /// Report the compiled runtime privilege contract without configuration or database access.
    RuntimeGrantContract,
    /// Build an exclusion constraint or WITHOUT OVERLAPS key on a table above the large-table
    /// threshold, where the install refuses it. Holds ACCESS EXCLUSIVE on the table for the
    /// build: run it in a maintenance window.
    BuildObject(BuildObjectArgs),
    /// Run one bounded cleanup of old rows of transient (wizard) models for one database.
    /// Nothing runs it automatically: an operator or the operator's scheduler calls
    /// this command.
    VacuumTransient(VacuumTransientArgs),
    /// Give a maintenance role its privileges on the vacuum state table (and no others), as the
    /// database owner. The role must be another role than the owner and the runtime role.
    GrantVacuumRole(GrantVacuumRoleArgs),
    /// Drop one database. Refused while a node of it is running.
    Drop(DbTargetArgs),
    /// Write one database to a new archive file (PostgreSQL custom format, mode 600). Refused
    /// while a node of it is running.
    Dump(DumpArgs),
    /// Create one database from an archive written by `erp db dump`. Refuses an existing
    /// database unless `--replace`.
    Restore(RestoreArgs),
    /// Make a copy safe to test on: scheduled jobs held, sessions, API keys and login throttle
    /// removed, every password replaced by one throwaway password, database secret replaced,
    /// `database.neutralized` set. Can be repeated. Refused while a node of it is running.
    Neutralize(NeutralizeArgs),
}

/// The database of `erp db drop | dump | restore | neutralize` and its connection.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DbTargetArgs {
    /// Deployment configuration file: its owner connection (host, port, role and password file)
    /// is used, its database name is not
    #[arg(long)]
    pub config: PathBuf,

    /// The database to act on
    #[arg(short = 'd', long = "database")]
    pub database: DatabaseName,

    /// Act even when a node of the database is running (its row in the node registry is not
    /// stopped and its lease has not expired); a drop then also ends the open sessions
    #[arg(long)]
    pub force_live: bool,
}

/// Arguments for `erp db dump`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DumpArgs {
    /// The database and its connection.
    #[command(flatten)]
    pub target: DbTargetArgs,

    /// The archive to write; it must not exist
    #[arg(long)]
    pub file: PathBuf,

    /// Directory of the PostgreSQL client programs (`pg_dump`); without it, `PATH` is searched
    #[arg(long, env = "ERP_PG_BIN")]
    pub pg_bin: Option<PathBuf>,
}

/// Arguments for `erp db restore`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RestoreArgs {
    /// The database and its connection.
    #[command(flatten)]
    pub target: DbTargetArgs,

    /// The archive written by `erp db dump`
    #[arg(long)]
    pub file: PathBuf,

    /// Drop an existing database of that name first
    #[arg(long)]
    pub replace: bool,

    /// Directory of the PostgreSQL client programs (`pg_restore`); without it, `PATH` is searched
    #[arg(long, env = "ERP_PG_BIN")]
    pub pg_bin: Option<PathBuf>,
}

/// Arguments for `erp db neutralize`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NeutralizeArgs {
    /// The database and its connection.
    #[command(flatten)]
    pub target: DbTargetArgs,

    /// Mode-600 regular file holding the throwaway password every user with a password gets
    /// (at least 12 bytes); it is never printed
    #[arg(long)]
    pub password_file: PathBuf,

    /// The database is a physical copy of its whole cluster (a base backup or a volume
    /// snapshot): it carries the recorded identity of the original, so neutralize refuses it
    /// without this option
    #[arg(long)]
    pub physical_copy: bool,

    /// Neutralize a database that has no recorded identity: such a database cannot be told
    /// from the original it may have been copied from, so neutralize refuses it without this
    /// option
    #[arg(long)]
    pub unrecorded_identity: bool,
}

/// Explicit server configuration; credentials are only read from private files.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServerArgs {
    /// Deployment configuration file.
    #[arg(long)]
    pub config: PathBuf,
}

/// Operator-controlled first administrator creation; no default credentials exist.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InitArgs {
    /// Deployment configuration file.
    #[arg(long)]
    pub config: PathBuf,
    /// Initial administrator login.
    #[arg(long)]
    pub admin_login: String,
    /// Mode-600 regular file containing the initial password.
    #[arg(long)]
    pub admin_password_file: PathBuf,
    /// The demo data choice of the new database (default: without).
    #[command(flatten)]
    pub demo: DemoArgs,
}

/// Arguments for `erp db build-object`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildObjectArgs {
    /// The database (credentials come from `DATABASE_URL`).
    #[command(flatten)]
    pub db: AccessArgs,

    /// The model that declares the object, e.g. `scale.booking`
    #[arg(long)]
    pub model: String,

    /// The declared name of the exclusion or key, e.g. `_no_double_booking`
    #[arg(long)]
    pub object: String,

    /// Seconds to wait for a lock (the schema lock, then the table lock) before giving up: 1 to
    /// 3600. While the build waits for its ACCESS EXCLUSIVE lock, every new reader and writer of
    /// the table queues behind it, so choose a short timeout on a busy table.
    #[arg(long = "lock-timeout", value_name = "SECS", default_value_t = LockTimeout::default(), value_parser = parse_lock_timeout)]
    pub lock_timeout: LockTimeout,

    /// Seconds each statement of the build may run before it is stopped and nothing is changed:
    /// 60 to 86400, and longer than --lock-timeout
    #[arg(long = "build-timeout", value_name = "SECS", default_value_t = BuildTimeout::default(), value_parser = parse_build_timeout)]
    pub build_timeout: BuildTimeout,
}

/// Arguments for `erp db vacuum-transient`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VacuumTransientArgs {
    /// Deployment configuration file (`server.toml`): it names the database, the owner
    /// connection and the addons path, from which the installed models are composed as the
    /// server composes them.
    #[arg(long)]
    pub config: PathBuf,

    /// The transient model to clean, e.g. `sample_library.lend.wizard`
    #[arg(long, required_unless_present = "all", conflicts_with = "all")]
    pub model: Option<String>,

    /// Clean every transient model of the registry, one after the other; stops at the first error
    #[arg(long)]
    pub all: bool,

    /// Rows selected per batch (1 to 1000, the default)
    #[arg(long = "max-batch-rows", value_name = "ROWS", value_parser = parse_batch_rows)]
    pub max_batch_rows: Option<u32>,

    /// Batches this call may run per model (1 to 100, the default)
    #[arg(long = "max-batches", value_name = "BATCHES", value_parser = parse_batches)]
    pub max_batches: Option<u32>,

    /// Rows this call may remove per model (1 to 100000, the default)
    #[arg(long = "max-rows", value_name = "ROWS", value_parser = parse_rows)]
    pub max_rows: Option<u64>,
}

impl VacuumTransientArgs {
    /// The engine options these arguments name; an unset cap keeps the engine's hard cap.
    ///
    /// ```
    /// use clap::Parser;
    /// use erp_cli::args::{Cli, Commands, DbAction};
    ///
    /// let cli = Cli::try_parse_from([
    ///     "erp", "db", "vacuum-transient", "--config", "server.toml", "--all", "--max-batch-rows", "200",
    ///     "--max-rows", "5000",
    /// ])?;
    /// let Commands::Db(cmd) = cli.command else { unreachable!() };
    /// let DbAction::VacuumTransient(args) = cmd.action else { unreachable!() };
    /// let options = args.options()?;
    /// assert_eq!((options.batch_rows(), options.rows()), (200, 5_000));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// `Programming` for a cap outside the engine's bounds (parsing already refuses those).
    pub fn options(&self) -> erp_fields::Result<erp_orm::VacuumOptions> {
        let mut options = erp_orm::VacuumOptions::default();
        if let Some(rows) = self.max_batch_rows {
            options = options.max_batch_rows(rows)?;
        }
        if let Some(batches) = self.max_batches {
            options = options.max_batches(batches)?;
        }
        if let Some(rows) = self.max_rows {
            options = options.max_rows(rows)?;
        }
        Ok(options)
    }
}

/// Arguments for `erp db grant-vacuum-role`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GrantVacuumRoleArgs {
    /// Deployment configuration file (`server.toml`).
    #[arg(long)]
    pub config: PathBuf,

    /// The maintenance role that runs the vacuum.
    #[arg(long)]
    pub role: String,
}

/// `erp cluster` subcommand container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClusterCommand {
    /// Cluster action to perform
    #[command(subcommand)]
    pub action: ClusterAction,
}

/// Cluster actions.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClusterAction {
    /// List the nodes registered in the database: name, role, state, freshness, build
    Nodes(AccessArgs),

    /// Open, close or show the rollout window that lets two versions serve side by side
    Rollout(RolloutCommand),

    /// Publish or operate digest-bound login selection admission (operator credentials).
    LoginSelection(LoginSelectionCommand),
}

/// Digest-bound login admission command container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoginSelectionCommand {
    /// Administrative operation.
    #[command(subcommand)]
    pub action: LoginSelectionAction,
}
/// Login admission administrative operations; all use operator credentials.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoginSelectionAction {
    /// Show authoritative enabled state, generation and approved exact digests.
    Status(AccessArgs),
    /// Approve a digest of a fresh capability-declaring member.
    Approve(LoginSelectionApproveArgs),
    /// Enable after legacy processes have been drained and fenced.
    Enable(LoginSelectionEnableArgs),
    /// Disable, waiting for protected finalizations before acknowledging completion.
    Disable(AccessArgs),
    /// Publish narrow helpers using an existing isolated NOLOGIN maintenance role.
    Publish(LoginSelectionPublishArgs),
}
/// Exact identity approval.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoginSelectionApproveArgs {
    /// Target database.
    #[command(flatten)]
    pub db: AccessArgs,
    /// Exact combined digest (64 hex digits).
    #[arg(long,value_parser=parse_digest)]
    pub digest: erp_orm::node::Digest,
}
/// Explicit external legacy cutover confirmation; the command cannot measure this precondition.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoginSelectionEnableArgs {
    /// Target database.
    #[command(flatten)]
    pub db: AccessArgs,
    /// Confirm legacy standalone processes drained, credentials fenced and existing sessions closed.
    #[arg(long, required = true)]
    pub confirm_legacy_drained_and_fenced: bool,
}
/// Existing roles for narrowly owned helpers.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoginSelectionPublishArgs {
    /// Target database.
    #[command(flatten)]
    pub db: AccessArgs,
    /// Existing isolated NOLOGIN role; never the operator or request role.
    #[arg(long,value_parser=parse_feature_role)]
    pub maintenance_role: String,
    /// Existing nonowner application request role.
    #[arg(long,value_parser=parse_feature_role)]
    pub request_role: String,
}
fn parse_feature_role(value: &str) -> std::result::Result<String, String> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("role must be 1 to 63 ASCII letters, digits or underscores".to_owned());
    }
    Ok(value.to_owned())
}

/// `erp cluster rollout` subcommand container.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RolloutCommand {
    /// Rollout action to perform
    #[command(subcommand)]
    pub action: RolloutAction,
}

/// Rollout window actions.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RolloutAction {
    /// Open a window from the version being replaced to the version rolled out (a window that
    /// is open is closed first)
    Open(RolloutOpenArgs),

    /// Close the open window
    Close(AccessArgs),

    /// Show the window that is not closed yet
    Status(AccessArgs),
}

/// Arguments for `erp cluster rollout open`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RolloutOpenArgs {
    /// The database (credentials come from `DATABASE_URL`).
    #[command(flatten)]
    pub db: AccessArgs,

    /// Digest of the version rolled out (64 hexadecimal digits, as `erp cluster nodes` shows it)
    #[arg(long, value_parser = parse_digest)]
    pub to: erp_orm::node::Digest,

    /// Digest of the version being replaced (default: the one digest of the ready nodes)
    #[arg(long, value_parser = parse_digest)]
    pub from: Option<erp_orm::node::Digest>,

    /// How long the window lasts: a number with a unit, `s`, `m` or `h` (1 minute to 24 hours),
    /// e.g. `30m`
    #[arg(long = "for", value_name = "DURATION", value_parser = parse_window)]
    pub duration: std::time::Duration,
}

fn parse_digest(s: &str) -> std::result::Result<erp_orm::node::Digest, String> {
    s.parse()
        .map_err(|e: erp_orm::node::NodeError| e.to_string())
}

/// `<n>s`, `<n>m` or `<n>h`; the range (1 minute to 24 hours) is checked by the database's
/// window rules, which name it.
fn parse_window(s: &str) -> std::result::Result<std::time::Duration, String> {
    let refuse = || format!("{s:?} is not a duration: a number with a unit s, m or h, e.g. 30m");
    let split = s.len().checked_sub(1).ok_or_else(refuse)?;
    let (number, unit) = s.split_at(split);
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(refuse());
    }
    let n: u64 = number.parse().map_err(|_| refuse())?;
    let secs = match unit {
        "s" => Some(n),
        "m" => n.checked_mul(60),
        "h" => n.checked_mul(3600),
        _ => None,
    }
    .ok_or_else(refuse)?;
    Ok(std::time::Duration::from_secs(secs))
}

fn parse_lock_timeout(s: &str) -> std::result::Result<LockTimeout, String> {
    s.parse()
        .map_err(|e: erp_fields::Error| e.message().to_owned())
}

fn parse_build_timeout(s: &str) -> std::result::Result<BuildTimeout, String> {
    s.parse()
        .map_err(|e: erp_fields::Error| e.message().to_owned())
}

fn parse_cap<T: FromStr + Copy>(
    s: &str,
    check: impl Fn(T) -> erp_fields::Result<()>,
) -> std::result::Result<T, String> {
    let value: T = s
        .parse()
        .map_err(|_| format!("'{s}' is not a whole number"))?;
    check(value).map_err(|e| e.message().to_owned())?;
    Ok(value)
}

fn parse_batch_rows(s: &str) -> std::result::Result<u32, String> {
    parse_cap(s, |v| {
        erp_orm::VacuumOptions::default()
            .max_batch_rows(v)
            .map(drop)
    })
}

fn parse_batches(s: &str) -> std::result::Result<u32, String> {
    parse_cap(s, |v| {
        erp_orm::VacuumOptions::default().max_batches(v).map(drop)
    })
}

fn parse_rows(s: &str) -> std::result::Result<u64, String> {
    parse_cap(s, |v| {
        erp_orm::VacuumOptions::default().max_rows(v).map(drop)
    })
}

fn parse_xmlid(s: &str) -> std::result::Result<QualifiedXmlId, String> {
    QualifiedXmlId::parse(s).map_err(|e| e.to_string())
}

impl ScaffoldArgs {
    /// Construct a new `ScaffoldArgs` with default parameters.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            dest: None,
            data_only: false,
            author: "ERPForAll Contributor".to_string(),
            summary: None,
        }
    }

    /// Set the destination directory.
    pub fn with_dest(mut self, dest: PathBuf) -> Self {
        self.dest = Some(dest);
        self
    }

    /// Set whether the addon is data-only.
    pub fn with_data_only(mut self, data_only: bool) -> Self {
        self.data_only = data_only;
        self
    }

    /// Set the author name.
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = author.into();
        self
    }

    /// Set the addon summary.
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }
}

impl ModuleOpArgs {
    /// Construct a new `ModuleOpArgs` with target database and module list.
    pub fn new(database: DatabaseName, modules: Vec<ModuleName>) -> Self {
        Self {
            database,
            modules,
            addons_path: vec![PathBuf::from("./addons")],
            dry_run: false,
            restore_generated_restrictions: false,
            identity_previous: None,
            demo: DemoArgs::default(),
        }
    }

    /// Set the `--restore-generated-restrictions` flag (upgrade only).
    pub fn with_restore_generated_restrictions(mut self, restore: bool) -> Self {
        self.restore_generated_restrictions = restore;
        self
    }

    /// Set addon search paths.
    pub fn with_addons_path(mut self, paths: Vec<PathBuf>) -> Self {
        self.addons_path = paths;
        self
    }

    /// Set dry-run validation flag.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
}

impl DevArgs {
    /// Construct a new `DevArgs` with default port 8069.
    pub fn new() -> Self {
        Self {
            database: None,
            addons_path: vec![PathBuf::from("./addons")],
            port: 8069,
        }
    }

    /// Set the target database.
    pub fn with_database(mut self, database: DatabaseName) -> Self {
        self.database = Some(database);
        self
    }

    /// Set the addon search paths.
    pub fn with_addons_path(mut self, paths: Vec<PathBuf>) -> Self {
        self.addons_path = paths;
        self
    }

    /// Set the HTTP port.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

impl Default for DevArgs {
    fn default() -> Self {
        Self::new()
    }
}

/// Audited attachment ownership operations on the owner connection.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AttachmentCommand {
    /// Inspect, claim, or read recovery status.
    #[command(subcommand)]
    pub action: AttachmentAction,
}
/// Attachment operator command; no arbitrary paths or namespaces are accepted.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum AttachmentAction {
    /// Probe the actual shared local filesystem from two hosts.
    ProbeStore(PeerArgs),
    /// Print the selected database/namespace confirmation token without changing ownership.
    Inspect(AttachmentTargetArgs),
    /// Establish chosen ownership and conditionally resolve a contest.
    Claim(AttachmentClaimArgs),
    /// Read a durable recovery operation's phase.
    Status(AttachmentStatusArgs),
}
/// Protected configuration and an explicit configured store key.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AttachmentTargetArgs {
    /// Protected server configuration.
    #[arg(long)]
    pub config: PathBuf,
    /// Configured attachment store key.
    #[arg(long)]
    pub store: String,
}
/// Closed operator decision reasons.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentReasonArg {
    /// Restore the original selected database.
    RestorePrimary,
    /// Retire an isolated competing clone.
    RetireClone,
}
/// Explicit force decision, or resumption of a previously audited decision.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AttachmentClaimArgs {
    /// Configured target.
    #[command(flatten)]
    pub target: AttachmentTargetArgs,
    /// Confirm that competing databases have been isolated and ownership is chosen explicitly.
    #[arg(long, required = true)]
    pub force: bool,
    /// Resume this durable operation UUID; never mint a replacement on ambiguous failure.
    #[arg(long,conflicts_with_all=["reason","confirm"])]
    pub resume: Option<String>,
    /// Audited operator reason for a new claim.
    #[arg(long, value_enum, required_unless_present = "resume")]
    pub reason: Option<AttachmentReasonArg>,
    /// Exact confirmation token printed by inspect.
    #[arg(long, required_unless_present = "resume")]
    pub confirm: Option<String>,
}
/// Read-only recovery phase lookup.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AttachmentStatusArgs {
    /// Configured target.
    #[command(flatten)]
    pub target: AttachmentTargetArgs,
    /// Durable operation UUID.
    #[arg(long)]
    pub operation: String,
}

/// Configuration and mode for the two-node peer probe.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeerArgs {
    /// Require explicit peer mode; local capability flags cannot admit shared deletion.
    #[arg(long, required = true)]
    pub peer: bool,
    /// Protected native server configuration.
    #[arg(long)]
    pub config: PathBuf,
    /// Configured shared local store key.
    #[arg(long)]
    pub store: String,
    /// Start, join or inspect an attempt.
    #[command(subcommand)]
    pub action: PeerAction,
}
/// Probe lifecycle action.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerAction {
    /// Invalidate previous evidence and print a fresh session for the other host to join.
    Start {
        /// Finite orchestration deadline, 30–300 seconds.
        #[arg(long,default_value_t=120,value_parser=clap::value_parser!(u32).range(30..=300))]
        timeout: u32,
    },
    /// Join a session printed by the other host.
    Join {
        /// Canonical session UUID.
        #[arg(value_parser=parse_peer_session)]
        session: String,
    },
    /// Inspect current durable state; this does not authorize cleanup.
    Status,
}
fn parse_peer_session(value: &str) -> std::result::Result<String, String> {
    erp_sdk::blob::Namespace::parse(value)
        .map(|n| n.to_string())
        .map_err(|_| "peer session must be a canonical UUID".into())
}
