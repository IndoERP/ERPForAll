//! Explicit server deployment configuration and protected secret-file loading.
use crate::{
    conn::ConnectionLimits,
    proxy::ProxyGate,
    redirect::RedirectPolicy,
    startup_limits::{NativeAccounting, RawStartupLimits, StartupLimitError, StartupLimits},
};
use serde::Deserialize;
use std::{
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
    /// Configuration or secret file cannot be read securely.
    #[error("server configuration or protected secret file is invalid")]
    Invalid,
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
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    database: String,
    database_url_file: PathBuf,
    owner_database_url_file: PathBuf,
    node_database_url_file: Option<PathBuf>,
    audit_reader_database_url_file: Option<PathBuf>,
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
    audit_read_policy: Option<toml::Value>,
    /// The attachment store table. Its keys are validated by `erp-blob`, which
    /// reads this same file; here it is only allowed to exist.
    attachments: Option<toml::Value>,
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
    audit_read_policy: erp_rpc::ReadAuditPolicy,
    attachments: String,
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
    /// Load strict TOML and private mode-600 regular files, refusing symlink secret paths.
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
        let raw: RawConfig = toml::from_str(&text).map_err(|_| ConfigError::Invalid)?;
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
        Ok(Self {
            database: raw.database,
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
            attachments,
            audit_read_policy: read_audit_policy(raw.audit_read_policy)?,
        })
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
            // Canonical policy is described in the registered RpcConfig section.
            audit_read_policy: _,
            // judged by the attachment topology check, not by settings agreement
            attachments: _,
        } = self;
        out.shared_str("server.database", database)?;
        out.shared_str("server.public_origin", origin)?;
        out.shared_duration("server.shutdown_deadline", *shutdown_deadline)?;
        out.shared_duration("server.http_header_deadline", connections.header_deadline())?;
        out.shared_duration("server.http_keep_alive_idle", connections.idle())?;
        out.shared_duration("server.http_max_connection_age", connections.max_age())?;
        // Each node may connect through its own role, listener, proxy peer and local files.
        // The canonical external origin remains shared: it participates in CSRF policy.
        for name in [
            "server.runtime_connection",
            "server.owner_connection",
            "server.node_connection",
            "server.audit_reader_connection",
            "server.node_name",
            "server.bind",
            "server.addons_path",
            "server.assets_manifest",
            "server.startup_limits",
            "server.jobs_run_in_app",
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
        .custom_flags(libc::O_NOFOLLOW)
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
