//! Explicit server deployment configuration and protected secret-file loading.
use crate::{
    conn::ConnectionLimits,
    proxy::ProxyGate,
    redirect::RedirectPolicy,
    startup_limits::{NativeAccounting, RawStartupLimits, StartupLimitError, StartupLimits},
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fmt,
    fs::{File, OpenOptions},
    io::Read,
    net::{IpAddr, SocketAddr},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use zeroize::Zeroizing;

/// A configuration refusal that never includes secret input or database parser diagnostics.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("audit_retention_days must be an integer from 1 to 3650")]
    AuditRetentionDays,
    #[error("outbox_retention_days must be an integer from 1 to 36500")]
    OutboxRetentionDays,
    /// Configuration or secret file cannot be read securely.
    #[error("server configuration or protected secret file is invalid")]
    Invalid,
    /// Invalid node authentication settings, with sanitized setting diagnostics.
    #[error("{0}")]
    Auth(#[source] erp_fields::Error),
    /// The unused startup touch key must be removed before restart.
    #[error(
        "startup_limits.session.touch_interval_secs was replaced by ir.config_parameter sessions.touch_interval_seconds"
    )]
    RetiredTouchInterval,
    /// Invalid additive read-audit policy, with sanitized field diagnostics.
    #[error("{0}")]
    AuditReadPolicy(#[source] erp_fields::Error),
    /// A D14 scalar or cross-setting startup relationship was refused.
    #[error(transparent)]
    Limit(#[from] StartupLimitError),
    /// Serving requires a separate heartbeat role at the same tenant endpoint.
    #[error("server requires node_database_url_file with a dedicated heartbeat role")]
    NodeConnection,
    /// A supplied audit reader credential is insecure or crosses the deployment boundary.
    #[error(
        "audit reader requires a protected connection to the same database with a distinct role"
    )]
    AuditReaderConnection,
    /// A SQL-object role setting is malformed or aliases another configured identity.
    #[error(
        "server setting {0} requires a distinct PostgreSQL role name of 1..=63 bytes without NUL"
    )]
    SqlObjectRole(&'static str),
    /// The `[report]` table is not a table of named non-negative integers and a path.
    #[error("[report]: {0}")]
    Report(String),
    /// `field_key_file` is not a key file; the message names the line, never its content.
    #[error("{0}")]
    FieldKeys(#[source] erp_fields::Error),
    /// Callback names, targets and secret files are validated together at startup.
    #[error("invalid [callbacks] configuration or protected callback secret file")]
    Callbacks,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    database: String,
    sql_object_definer_role: Option<toml::Value>,
    sql_object_maintenance_role: Option<toml::Value>,
    auth: Option<toml::Value>,
    database_url_file: PathBuf,
    owner_database_url_file: PathBuf,
    node_database_url_file: Option<PathBuf>,
    audit_reader_database_url_file: Option<PathBuf>,
    /// The keys of encrypted fields (`erp_orm::field_encryption`), mode 600.
    field_key_file: Option<PathBuf>,
    node_name: Option<String>,
    shutdown_deadline_secs: Option<u64>,
    #[serde(default)]
    startup_limits: RawStartupLimits,
    bind: SocketAddr,
    public_origin: String,
    proxy_mode: ProxyMode,
    #[serde(default)]
    trusted_proxies: Vec<String>,
    proxy_token_file: Option<PathBuf>,
    addons_path: Vec<PathBuf>,
    assets_manifest: PathBuf,
    http_header_deadline_secs: Option<u64>,
    http_keep_alive_idle_secs: Option<u64>,
    http_max_connection_age_secs: Option<u64>,
    /// Run the job queue's runner in this process (4); absent means off.
    jobs_run_in_app: Option<bool>,
    attachment_maintenance_run_in_app: Option<bool>,
    mail_delivery_dir: Option<PathBuf>,
    outbox_retention_days: Option<u32>,
    audit_retention_days: Option<u16>,
    audit_witness_file: Option<PathBuf>,
    audit_read_policy: Option<toml::Value>,
    /// The attachment store table. Its keys are validated by `erp-blob`, which
    /// reads this same file; here it is only allowed to exist.
    attachments: Option<toml::Value>,
    /// The report render pool: `font_directory`, `cjk_font_directory` and the pool
    /// settings by name.
    /// Names and bounds are checked by the composition root, which knows the settings.
    report: Option<toml::Value>,
    /// Optional AI configuration; admitted by the addon at the CLI composition root.
    ai: Option<toml::Value>,
    #[serde(default)]
    callbacks: BTreeMap<String, crate::web::callback::RawCallback>,
    external_identity: Option<ExternalIdentityFiles>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalIdentityFiles {
    client_secret_file: PathBuf,
    signing_keys_file: PathBuf,
    provider: erp_security::auth::external::ExternalProviderConfig,
}
/// The `[report]` table: the font store and the render pool's settings, each `name = integer`.
#[derive(Debug, Clone, Default)]
pub struct ReportConfig {
    /// The shipped font store; `None`: the `report_fonts` directory of an addons path.
    pub font_directory: Option<PathBuf>,
    /// The optional Chinese, Japanese and Korean font package (`report_fonts_cjk`); `None`: not
    /// installed on this node, and a document that names one of its families is refused by name.
    pub cjk_font_directory: Option<PathBuf>,
    /// `(setting, value)` in file order.
    pub settings: Vec<(String, u64)>,
}
fn read_report(
    value: Option<toml::Value>,
    resolve: impl Fn(&Path) -> PathBuf,
) -> Result<ReportConfig, ConfigError> {
    let Some(value) = value else {
        return Ok(ReportConfig::default());
    };
    let table = value
        .as_table()
        .ok_or_else(|| ConfigError::Report("must be a table".to_owned()))?;
    let mut config = ReportConfig::default();
    for (key, value) in table {
        if key == "font_directory" || key == "cjk_font_directory" {
            let path = value
                .as_str()
                .ok_or_else(|| ConfigError::Report(format!("{key} must be a path")))?;
            let path = Some(resolve(Path::new(path)));
            if key == "font_directory" {
                config.font_directory = path;
            } else {
                config.cjk_font_directory = path;
            }
            continue;
        }
        let number = value
            .as_integer()
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| ConfigError::Report(format!("{key} takes a whole number, 0 or more")))?;
        config.settings.push((key.clone(), number));
    }
    Ok(config)
}
fn read_audit_policy(value: Option<toml::Value>) -> Result<erp_rpc::ReadAuditPolicy, ConfigError> {
    let fail =
        |message: &str| ConfigError::AuditReadPolicy(erp_fields::Error::programming(message));
    let Some(value) = value else {
        return Ok(erp_rpc::ReadAuditPolicy::default());
    };
    let table = value
        .as_table()
        .ok_or_else(|| fail("audit_read_policy must be a table"))?;
    if table
        .keys()
        .any(|key| key != "all_users" && key != "user_groups")
    {
        return Err(fail("audit_read_policy contains unknown keys"));
    }
    match (table.get("all_users"), table.get("user_groups")) {
        (Some(all), None) => match all.as_bool() {
            Some(true) => Ok(erp_rpc::ReadAuditPolicy::all_users()),
            Some(false) => Ok(erp_rpc::ReadAuditPolicy::default()),
            None => Err(fail("audit_read_policy.all_users must be a boolean")),
        },
        (None, Some(groups)) => {
            let message = "audit_read_policy.user_groups must be a list of positive group XML IDs";
            let groups = groups.as_array().ok_or_else(|| fail(message))?;
            if groups.len() > 256 {
                return Err(fail(
                    "audit_read_policy.user_groups has more than 256 entries",
                ));
            }
            let names = groups
                .iter()
                .map(|group| group.as_str().ok_or_else(|| fail(message)))
                .collect::<Result<Vec<_>, _>>()?;
            erp_rpc::ReadAuditPolicy::user_groups(&names).map_err(ConfigError::AuditReadPolicy)
        }
        (None, None) | (Some(_), Some(_)) => Err(fail(
            "audit_read_policy requires exactly one of all_users or user_groups",
        )),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProxyMode {
    Direct,
    Authenticated,
}
/// Validated configuration. Secret content is omitted from Debug and parser errors.
pub struct ServerConfig {
    database: String,
    sql_object_definer_role: Option<String>,
    sql_object_maintenance_role: Option<String>,
    auth: erp_security::auth::AuthConfig,
    runtime: tokio_postgres::Config,
    owner: tokio_postgres::Config,
    node: Option<tokio_postgres::Config>,
    audit_reader: Option<tokio_postgres::Config>,
    node_name: String,
    shutdown_deadline: Duration,
    startup_limits: StartupLimits,
    bind: SocketAddr,
    origin: String,
    proxy: ProxyGate,
    addons: Vec<PathBuf>,
    assets: PathBuf,
    connections: ConnectionLimits,
    jobs_run_in_app: bool,
    attachment_maintenance_run_in_app: bool,
    mail_delivery_dir: Option<PathBuf>,
    outbox_retention_days: Option<u32>,
    audit_retention_days: Option<u16>,
    audit_witness_file: Option<PathBuf>,
    audit_read_policy: erp_rpc::ReadAuditPolicy,
    attachments: String,
    report: ReportConfig,
    ai: Option<toml::Value>,
    callbacks: crate::web::callback::Callbacks,
    field_keys: Option<erp_orm::field_encryption::FieldKeys>,
    external_identity: Option<erp_security::auth::external::ExternalProvider>,
}
impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("database", &self.database)
            .field("bind", &self.bind)
            .field("origin", &self.origin)
            .field("connections", &self.connections)
            .field("node_name", &self.node_name)
            .field("shutdown_deadline", &self.shutdown_deadline)
            .field("startup_limits", &self.startup_limits)
            .finish_non_exhaustive()
    }
}
impl ServerConfig {
    /// Node-local input admission, prefilter capacity and hook execution bounds.
    /// Changes take effect after restart and are retained across registry rebuilds.
    pub fn auth(&self) -> erp_security::auth::AuthConfig {
        self.auth
    }

    /// Load strict TOML and private mode-600 regular files, refusing symlink secret paths. The
    /// keys of `field_key_file`, when set, are installed for the process
    /// ([`erp_orm::field_encryption::install`]).
    /// # Errors
    /// Invalid configuration, secret file, database binding or explicit proxy mode.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut text = String::new();
        File::open(path)
            .map_err(|_| ConfigError::Invalid)?
            .take(65_537)
            .read_to_string(&mut text)
            .map_err(|_| ConfigError::Invalid)?;
        if text.len() > 65_536 {
            return Err(ConfigError::Invalid);
        }
        let value: toml::Value = toml::from_str(&text).map_err(|_| ConfigError::Invalid)?;
        if value
            .get("startup_limits")
            .and_then(toml::Value::as_table)
            .is_some_and(|table| table.contains_key("session.touch_interval_secs"))
        {
            return Err(ConfigError::RetiredTouchInterval);
        }
        let raw: RawConfig = value.try_into().map_err(|_| ConfigError::Invalid)?;
        let auth = crate::auth_config::read(raw.auth)?;
        if raw.database.is_empty()
            || raw.database.len() > 63
            || !raw
                .database
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || raw.addons_path.is_empty()
            || raw.addons_path.len() > 32
        {
            return Err(ConfigError::Invalid);
        }
        let base = path.parent().ok_or(ConfigError::Invalid)?;
        let resolve = |value: &Path| {
            if value.is_absolute() {
                value.to_path_buf()
            } else {
                base.join(value)
            }
        };
        let runtime = connection(
            &read_secret(&resolve(&raw.database_url_file))?,
            &raw.database,
        )?;
        let owner = connection(
            &read_secret(&resolve(&raw.owner_database_url_file))?,
            &raw.database,
        )?;
        if runtime.get_user() == owner.get_user()
            || runtime.get_hosts() != owner.get_hosts()
            || runtime.get_ports() != owner.get_ports()
        {
            return Err(ConfigError::Invalid);
        }
        let node = match raw.node_database_url_file.as_deref() {
            Some(path) => {
                let connection = connection(&read_secret(&resolve(path))?, &raw.database)?;
                if connection.get_user() == runtime.get_user()
                    || connection.get_user() == owner.get_user()
                    || connection.get_hosts() != runtime.get_hosts()
                    || connection.get_ports() != runtime.get_ports()
                {
                    return Err(ConfigError::NodeConnection);
                }
                Some(connection)
            }
            None => None,
        };
        let audit_reader = match raw.audit_reader_database_url_file.as_deref() {
            Some(path) => {
                let invalid = |_| ConfigError::AuditReaderConnection;
                let value = read_secret(&resolve(path)).map_err(invalid)?;
                let reader = connection(&value, &raw.database).map_err(invalid)?;
                if reader.get_user() == runtime.get_user()
                    || reader.get_user() == owner.get_user()
                    || node
                        .as_ref()
                        .is_some_and(|node| reader.get_user() == node.get_user())
                    || reader.get_hosts() != runtime.get_hosts()
                    || reader.get_ports() != runtime.get_ports()
                {
                    return Err(ConfigError::AuditReaderConnection);
                }
                Some(reader)
            }
            None => None,
        };
        // installed for the whole process here, before any ORM is built from this
        // configuration: every command that loads it can open encrypted fields
        let field_keys = match raw.field_key_file.as_deref() {
            Some(path) => {
                let keys =
                    erp_orm::field_encryption::FieldKeys::parse(&read_secret(&resolve(path))?)
                        .map_err(ConfigError::FieldKeys)?;
                erp_orm::field_encryption::install(keys.clone()).map_err(ConfigError::FieldKeys)?;
                Some(keys)
            }
            None => None,
        };
        let mut used = vec![runtime.get_user(), owner.get_user()];
        used.extend(node.as_ref().map(tokio_postgres::Config::get_user));
        used.extend(audit_reader.as_ref().map(tokio_postgres::Config::get_user));
        let definer = sql_object_role(
            raw.sql_object_definer_role,
            "sql_object_definer_role",
            &used,
        )?;
        used.push(definer.as_deref());
        let maintenance = sql_object_role(
            raw.sql_object_maintenance_role,
            "sql_object_maintenance_role",
            &used,
        )?;
        if raw
            .outbox_retention_days
            .is_some_and(|days| !(1..=36500).contains(&days))
        {
            return Err(ConfigError::OutboxRetentionDays);
        }
        if raw
            .audit_retention_days
            .is_some_and(|days| !(1..=3650).contains(&days))
        {
            return Err(ConfigError::AuditRetentionDays);
        }
        // Absent values receive documented defaults; supplied invalid values never do.
        let node_name = match raw.node_name {
            Some(name) => name,
            None => "erp-server".to_owned(),
        };
        if node_name.is_empty()
            || node_name.len() > 63
            || !node_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(ConfigError::Invalid);
        }
        // 's default applies only to an absent option; malformed values already failed TOML admission.
        let shutdown_deadline_secs = raw.shutdown_deadline_secs.unwrap_or(45);
        if !(5..=3600).contains(&shutdown_deadline_secs) {
            return Err(ConfigError::Invalid);
        }
        let host = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::nothing()
                .with_memory(sysinfo::MemoryRefreshKind::nothing().with_ram()),
        );
        let startup_limits = raw
            .startup_limits
            .validate(host.total_memory(), NativeAccounting::current())?;
        RedirectPolicy::new(&raw.public_origin, &["list", "form"])
            .map_err(|_| ConfigError::Invalid)?;
        let origin = url::Url::parse(&raw.public_origin).map_err(|_| ConfigError::Invalid)?;
        let proxy = match raw.proxy_mode {
            ProxyMode::Direct => {
                if !raw.trusted_proxies.is_empty()
                    || raw.proxy_token_file.is_some()
                    || !raw.bind.ip().is_loopback()
                {
                    return Err(ConfigError::Invalid);
                }
                ProxyGate::direct()
            }
            ProxyMode::Authenticated => {
                if origin.scheme() != "https" {
                    return Err(ConfigError::Invalid);
                }
                let peers = raw
                    .trusted_proxies
                    .iter()
                    .map(|peer| trusted_address(peer))
                    .collect::<Result<Vec<_>, _>>()?;
                let token = read_secret(&resolve(
                    raw.proxy_token_file
                        .as_deref()
                        .ok_or(ConfigError::Invalid)?,
                ))?;
                ProxyGate::authenticated(peers, &token).map_err(|_| ConfigError::Invalid)?
            }
        };
        let attachments = match raw.attachments {
            Some(_) => text.clone(),
            None => {
                let directory = if base.is_absolute() {
                    base.to_path_buf()
                } else {
                    std::env::current_dir()
                        .map_err(|_| ConfigError::Invalid)?
                        .join(base)
                };
                let path = directory.join("attachments");
                let path = path.to_str().ok_or(ConfigError::Invalid)?;
                format!(
                    "[attachments]\nwrite_store = \"main\"\n[attachments.stores.main]\nkind = \"local\"\npath = {}\n",
                    toml::Value::String(path.to_owned())
                )
            }
        };
        let defaults = ConnectionLimits::default();
        let connections = ConnectionLimits::new(
            raw.http_header_deadline_secs
                .unwrap_or(defaults.header_deadline().as_secs()),
            raw.http_keep_alive_idle_secs
                .unwrap_or(defaults.idle().as_secs()),
            raw.http_max_connection_age_secs
                .unwrap_or(defaults.max_age().as_secs()),
        )
        .map_err(|_| ConfigError::Invalid)?;
        let external_identity = raw
            .external_identity
            .map(|files| {
                let mut signing_keys = Vec::new();
                File::open(resolve(&files.signing_keys_file))
                    .map_err(|_| ConfigError::Invalid)?
                    .take(65_537)
                    .read_to_end(&mut signing_keys)
                    .map_err(|_| ConfigError::Invalid)?;
                let secret = read_secret(&resolve(&files.client_secret_file))?;
                erp_security::auth::external::ExternalProvider::new(
                    files.provider,
                    secret,
                    &signing_keys,
                )
                .map_err(|_| ConfigError::Invalid)
            })
            .transpose()?;
        let callbacks = crate::web::callback::Callbacks::load(raw.callbacks, resolve)?;
        Ok(Self {
            database: raw.database,
            sql_object_definer_role: definer,
            sql_object_maintenance_role: maintenance,
            auth,
            runtime,
            owner,
            node,
            audit_reader,
            node_name,
            shutdown_deadline: Duration::from_secs(shutdown_deadline_secs),
            startup_limits,
            bind: raw.bind,
            origin: origin.origin().ascii_serialization(),
            proxy,
            addons: raw.addons_path.iter().map(|path| resolve(path)).collect(),
            assets: resolve(&raw.assets_manifest),
            connections,
            jobs_run_in_app: raw.jobs_run_in_app.unwrap_or(false),
            attachment_maintenance_run_in_app: raw
                .attachment_maintenance_run_in_app
                .unwrap_or(false),
            mail_delivery_dir: raw.mail_delivery_dir.as_deref().map(resolve),
            outbox_retention_days: raw.outbox_retention_days,
            audit_retention_days: raw.audit_retention_days,
            audit_witness_file: raw.audit_witness_file.as_deref().map(resolve),
            attachments,
            report: read_report(raw.report, resolve)?,
            ai: raw.ai,
            callbacks,
            audit_read_policy: read_audit_policy(raw.audit_read_policy)?,
            field_keys,
            external_identity,
        })
    }

    /// The keys of encrypted fields from `field_key_file`, `None` when the file is not set
    /// (installed for the process by [`ServerConfig::load`]).
    pub fn field_keys(&self) -> Option<&erp_orm::field_encryption::FieldKeys> {
        self.field_keys.as_ref()
    }
    /// Optional configured OpenID Connect issuer and browser login policy.
    pub fn external_identity(&self) -> Option<&erp_security::auth::external::ExternalProvider> {
        self.external_identity.as_ref()
    }
    pub(crate) fn callbacks(&self) -> &crate::web::callback::Callbacks {
        &self.callbacks
    }
    /// Validated additive read-audit policy shared by admission and serving RPC configuration.
    pub fn read_audit_policy(&self) -> &erp_rpc::ReadAuditPolicy {
        &self.audit_read_policy
    }

    /// The TOML text `erp_blob::AttachmentConfig::from_toml` reads: this file when it has an
    /// `[attachments]` table, otherwise the default store (lead ruling 3): a `local`
    /// directory named `attachments` next to the configuration file, for one node, with no
    /// further infrastructure. The directory is created (mode 0700) when the server opens it.
    pub fn attachments_toml(&self) -> &str {
        &self.attachments
    }
    /// Node-local AI configuration consumed by the optional addon, never shared or logged.
    pub fn ai(&self) -> Option<&toml::Value> {
        self.ai.as_ref()
    }

    /// The `[report]` table: font store and render pool settings, node-local.
    pub fn report(&self) -> &ReportConfig {
        &self.report
    }
    /// Operator-provisioned NOLOGIN owner of declared SECURITY DEFINER functions; unset grants nothing.
    pub fn sql_object_definer_role(&self) -> Option<&str> {
        self.sql_object_definer_role.as_deref()
    }
    /// Operator-provisioned maintenance login for declared SQL entry points; unset grants nothing.
    pub fn sql_object_maintenance_role(&self) -> Option<&str> {
        self.sql_object_maintenance_role.as_deref()
    }
    /// Configured database identity.
    pub fn database(&self) -> &str {
        &self.database
    }
    /// Configured listener. Kept fallible for the public startup interface.
    /// # Errors
    /// No error after successful configuration admission.
    pub fn bind(&self) -> Result<SocketAddr, ConfigError> {
        Ok(self.bind)
    }
    /// Canonical origin, independent of untrusted request headers.
    pub fn origin(&self) -> &str {
        &self.origin
    }
    /// Required request-level proxy authentication.
    pub fn proxy(&self) -> &ProxyGate {
        &self.proxy
    }
    /// Runtime connection settings. Ordinary ORM work must use this non-owner role.
    pub fn runtime_connection(&self) -> tokio_postgres::Config {
        self.runtime.clone()
    }
    /// Restricted lifecycle connection settings; never used for ordinary requests.
    pub fn owner_connection(&self) -> tokio_postgres::Config {
        self.owner.clone()
    }
    /// Dedicated heartbeat connection, required before a server exposes readiness.
    ///
    /// # Errors
    /// [`ConfigError::NodeConnection`] when no protected node connection was configured.
    pub fn node_connection(&self) -> Result<tokio_postgres::Config, ConfigError> {
        self.node.clone().ok_or(ConfigError::NodeConnection)
    }
    /// Optional heartbeat connection for initialization and grant-publication commands.
    /// Its absence is valid for commands that do not serve requests.
    pub fn configured_node_connection(&self) -> Option<tokio_postgres::Config> {
        self.node.clone()
    }
    /// Optional isolated audit reader credential. Trusted platform composition only: never use
    /// this connection for ordinary requests or expose it through addon state.
    pub fn configured_audit_reader_connection(&self) -> Option<tokio_postgres::Config> {
        self.audit_reader.clone()
    }
    /// Request-pool allocation after reserving the optional reader's one connection.
    pub fn request_pool_size(&self) -> usize {
        if self.audit_reader.is_some() { 7 } else { 8 }
    }
    /// Validated operator label for the process's node membership.
    pub fn node_name(&self) -> &str {
        &self.node_name
    }
    /// Time admitted work may drain after shutdown starts.
    pub fn shutdown_deadline(&self) -> Duration {
        self.shutdown_deadline
    }
    /// `jobs_run_in_app`: whether this process runs the job queue's runner (default off; a node
    /// setting,.4).
    pub fn jobs_run_in_app(&self) -> bool {
        self.jobs_run_in_app
    }

    /// Whether this node runs the fixed elevated attachment handlers (default off).
    /// Node configuration: selects this process's execution topology before opening its owner pool.
    pub fn attachment_maintenance_run_in_app(&self) -> bool {
        self.attachment_maintenance_run_in_app
    }

    /// Private local Maildir to receive outgoing message notices.
    pub fn mail_delivery_dir(&self) -> Option<&Path> {
        self.mail_delivery_dir.as_deref()
    }

    /// Shared outbox retention policy; absent disables automatic retirement.
    pub fn outbox_retention_days(&self) -> Option<u32> {
        self.outbox_retention_days
    }

    /// The operator's age policy. An absent value keeps every audit partition attached.
    pub fn audit_retention_days(&self) -> Option<u16> {
        self.audit_retention_days
    }

    /// Optional external witness inventory used to verify the chain before archival.
    pub fn audit_witness_file(&self) -> Option<&Path> {
        self.audit_witness_file.as_deref()
    }

    /// D14 scalar and byte-budget relationships admitted at startup.
    /// Getters do not imply runtime implementation of the corresponding future facilities.
    pub fn startup_limits(&self) -> &StartupLimits {
        &self.startup_limits
    }
    /// Operator-selected addon roots.
    pub fn addons(&self) -> &[PathBuf] {
        &self.addons
    }
    /// Manifest produced by the client compiler/build pipeline.
    pub fn assets_manifest(&self) -> &Path {
        &self.assets
    }
    /// HTTP/1 connection limits: `http_header_deadline_secs` (default 5),
    /// `http_keep_alive_idle_secs` (30) and `http_max_connection_age_secs` (120).
    pub fn connection_limits(&self) -> ConnectionLimits {
        self.connections
    }
}
impl erp_orm::node::SharedSettings for ServerConfig {
    fn section(&self) -> erp_orm::node::SectionName {
        erp_orm::node::SectionName::new("server")
    }
    fn describe(
        &self,
        out: &mut erp_orm::node::SettingsWriter<'_>,
    ) -> Result<(), erp_orm::node::NodeError> {
        let Self {
            database,
            sql_object_definer_role: _,
            sql_object_maintenance_role: _,
            auth: _,
            runtime: _,
            owner: _,
            node: _,
            audit_reader: _,
            node_name: _,
            shutdown_deadline,
            startup_limits: _,
            bind: _,
            origin,
            proxy,
            addons: _,
            assets: _,
            connections,
            jobs_run_in_app: _,
            attachment_maintenance_run_in_app: _,
            mail_delivery_dir: _,
            outbox_retention_days,
            audit_retention_days,
            audit_witness_file,
            // Canonical policy is described in the registered RpcConfig section.
            audit_read_policy: _,
            // judged by the attachment topology check, not by settings agreement
            attachments: _,
            // node-local: each node starts its own render workers with its own fonts and limits
            report: _,
            // Provider files belong to each node and never enter shared settings metadata.
            ai: _,
            // A node can opt in to the signed callback bindings it serves.
            callbacks: _,
            field_keys,
            external_identity,
        } = self;
        out.shared_str("server.database", database)?;
        out.shared_str("server.public_origin", origin)?;
        out.shared_duration("server.shutdown_deadline", *shutdown_deadline)?;
        out.shared_duration("server.http_header_deadline", connections.header_deadline())?;
        out.shared_duration("server.http_keep_alive_idle", connections.idle())?;
        out.shared_duration("server.http_max_connection_age", connections.max_age())?;
        if let Some(days) = outbox_retention_days {
            out.shared_u64("server.outbox_retention_days", u64::from(*days))?;
        }
        if let Some(days) = audit_retention_days {
            out.shared_u64("server.audit_retention_days", u64::from(*days))?;
        }
        if audit_witness_file.is_some() {
            out.shared_str("server.audit_witness", "file")?;
        }
        // every node of a database opens what the others seal: the same key set everywhere (only
        // a keyed digest of its fingerprint enters the settings digest)
        match field_keys {
            Some(keys) => out.shared_secret("server.field_keys", &keys.fingerprint())?,
            None => out.shared_str("server.field_keys", "none")?,
        }
        match external_identity {
            Some(provider) => out.shared_secret(
                "server.external_identity",
                provider.configuration_fingerprint(),
            )?,
            None => out.shared_str("server.external_identity", "none")?,
        }
        // Each node may connect through its own role, listener, proxy peer and local files.
        // The canonical external origin remains shared: it participates in CSRF policy.
        for name in [
            "server.runtime_connection",
            "server.owner_connection",
            "server.node_connection",
            "server.audit_reader_connection",
            "server.node_name",
            // Each process pays for these resources; database session policy is separate.
            "server.auth",
            "server.bind",
            "server.addons_path",
            "server.assets_manifest",
            "server.startup_limits",
            "server.jobs_run_in_app",
            "server.attachment_maintenance_run_in_app",
        ] {
            out.node(name)?;
        }
        // StartupLimits has its own exhaustive SharedSettings section; it must be registered too.
        proxy.describe_settings(out)
    }
}

fn connection(value: &str, database: &str) -> Result<tokio_postgres::Config, ConfigError> {
    if !(value.starts_with("postgres://") || value.starts_with("postgresql://"))
        || value.chars().any(char::is_whitespace)
    {
        return Err(ConfigError::Invalid);
    }
    let url = url::Url::parse(value).map_err(|_| ConfigError::Invalid)?;
    if url.host_str().is_none() || url.fragment().is_some() || url.username().is_empty() {
        return Err(ConfigError::Invalid);
    }
    let config: tokio_postgres::Config = value.parse().map_err(|_| ConfigError::Invalid)?;
    if config.get_dbname() != Some(database) {
        return Err(ConfigError::Invalid);
    }
    Ok(config)
}
fn trusted_address(value: &str) -> Result<IpAddr, ConfigError> {
    let (ip, prefix) = value
        .split_once('/')
        .map_or((value, None), |(ip, prefix)| (ip, Some(prefix)));
    let ip: IpAddr = ip.parse().map_err(|_| ConfigError::Invalid)?;
    if prefix.is_some_and(|prefix| prefix != if ip.is_ipv4() { "32" } else { "128" }) {
        return Err(ConfigError::Invalid);
    }
    Ok(ip)
}
/// Read a bounded UTF-8 secret from a mode-600 regular file without following symlinks.
/// A single terminal LF is removed. Errors never include the file's contents.
/// # Errors
/// Invalid permissions, file shape, encoding, size or empty content.
pub fn read_secret(path: &Path) -> Result<Zeroizing<String>, ConfigError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| ConfigError::Invalid)?;
    let metadata = file.metadata().map_err(|_| ConfigError::Invalid)?;
    if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 || metadata.len() > 4096 {
        return Err(ConfigError::Invalid);
    }
    let mut text = Zeroizing::new(String::new());
    file.take(4097)
        .read_to_string(&mut text)
        .map_err(|_| ConfigError::Invalid)?;
    if text.ends_with('\n') {
        text.pop();
    }
    if text.is_empty() || text.len() > 4096 {
        return Err(ConfigError::Invalid);
    }
    Ok(text)
}

fn sql_object_role(
    value: Option<toml::Value>,
    key: &'static str,
    used: &[Option<&str>],
) -> Result<Option<String>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let role = value.as_str().ok_or(ConfigError::SqlObjectRole(key))?;
    if role.is_empty() || role.len() > 63 || role.contains('\0') || used.contains(&Some(role)) {
        return Err(ConfigError::SqlObjectRole(key));
    }
    Ok(Some(role.to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;

    fn table(text: &str) -> Option<toml::Value> {
        Some(toml::from_str(text).unwrap())
    }

    /// like `font_directory` against the configuration file's directory, and a non-path is refused
    /// naming the key)
    #[test]
    fn cjk_font_directory_is_read_resolved_and_refused_when_not_a_path() {
        let base = Path::new("/etc/erp");
        let resolve = |value: &Path| {
            if value.is_absolute() {
                value.to_path_buf()
            } else {
                base.join(value)
            }
        };
        let config = read_report(
            table("font_directory = 'fonts'\ncjk_font_directory = 'cjk'\nworkers = 2\n"),
            resolve,
        )
        .unwrap();
        assert_eq!(config.cjk_font_directory, Some(base.join("cjk")));
        assert_eq!(config.font_directory, Some(base.join("fonts")));
        assert_eq!(config.settings, vec![("workers".to_owned(), 2)]);
        let absolute = read_report(table("cjk_font_directory = '/opt/cjk'\n"), resolve).unwrap();
        assert_eq!(absolute.cjk_font_directory, Some(PathBuf::from("/opt/cjk")));
        let refused = read_report(table("cjk_font_directory = 3\n"), resolve).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "[report]: cjk_font_directory must be a path"
        );
    }
}
