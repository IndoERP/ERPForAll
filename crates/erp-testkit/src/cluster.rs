//! Choosing the PostgreSQL cluster a new test database is created on.
//!
//! [`DATABASE_URLS_ENV`] (`ERP_TEST_DATABASE_URLS`) lists admin connection URLs in order of
//! preference, separated by whitespace. Before each test database is created the clusters are
//! probed in that order, and the first one whose client connections are below the threshold
//! ([`MAX_USAGE_ENV`], default [`DEFAULT_MAX_USAGE_PCT`] % of `max_connections`) is used; later
//! clusters are not contacted. A cluster that refuses the connection, times out, closes the
//! connection before answering (an ssh tunnel with nothing behind it, a cluster stopped for the
//! benchmark window) or reports that it is starting up, shutting down or out of connection slots is
//! *excluded*, with a log line. When every cluster is full or excluded, creation fails with
//! [`TestkitError::NoClusterAvailable`], which lists each cluster's state. Every other error (wrong
//! password, unknown admin database, SQL failure) fails at once: it is a configuration mistake, not
//! a busy cluster, and trying the next cluster would hide it. Each exclusion carries an
//! [`Unreachable`] with a typed [`UnreachableKind`] (refused, timed out, closed, …) and the message;
//! [`Cluster::probe`] reports the same for one cluster (`erp-testkit probe`, which `scripts/ci.sh`
//! runs before the tests so that a dead secondary cluster is seen even when the first one has room).
//!
//! **Headroom is a soft limit.** The probe counts the connections open at that instant; the pool a
//! new [`crate::TestDb`] opens afterwards (up to its pool size, lazily) is not there yet, so the
//! probe adds the pool size before comparing ([`UsageThreshold::admits`]). Creators that probe at
//! the same moment (parallel nextest processes, both CI runners, agents' runs) still all see the
//! same count. The threshold therefore only holds while the headroom above it covers them:
//! `(100 − threshold) % × max_connections ≥ concurrent creators × (pool size + 1)`; with
//! `erpforall-pg` at 500 connections and the default 75 %, that is 125 connections, or 25 creators
//! with the default pool of 4. Past that, `max_connections` is the hard limit: a probe that is
//! refused with SQLSTATE 53300 excludes the cluster, but a pool connection refused later fails the
//! test that asked for it.
//!
//! Without `ERP_TEST_DATABASE_URLS`, `DATABASE_URL` names the only cluster and is used as before,
//! without a headroom check. [`crate::TestDbBuilder::database_url`] and
//! [`crate::TestDbBuilder::cluster`] pin a database to one server the same way.
//!
//! Each choice is printed to stderr (`erp-testkit: …`, shown by nextest for failing tests) and, when
//! [`CLUSTER_LOG_ENV`] names a file, appended to it as one tab-separated line per test database
//! (`scripts/ci.sh` keeps that file next to the CI log).

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_postgres::config::Host;
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, Config};

use crate::db::{admin_config, connect};
use crate::error::{Result, TestkitError, pg_error};

/// Ordered list of admin connection URLs, separated by whitespace (first = preferred).
pub const DATABASE_URLS_ENV: &str = "ERP_TEST_DATABASE_URLS";

/// Highest share of `max_connections` (percent, 1–100) a cluster may already use and still get
/// new test databases.
pub const MAX_USAGE_ENV: &str = "ERP_TEST_DB_MAX_USAGE_PCT";

/// Labels (`host:port/database`, whitespace-separated) that `erp-testkit probe` found unreachable
/// before the tests (`scripts/ci.sh` sets it). Selection skips them: a cluster that comes back
/// later was never checked by the probe (its timeouts included, [`Cluster::probe_with_timeouts`]),
/// so it takes no test database in that run.
pub const PROBED_UNREACHABLE_ENV: &str = "ERP_TESTKIT_UNREACHABLE_CLUSTERS";

/// File that receives one line per created test database (cluster, usage, run id), if set.
pub const CLUSTER_LOG_ENV: &str = "ERP_TESTKIT_CLUSTER_LOG";

/// Threshold used when [`MAX_USAGE_ENV`] is not set.
pub const DEFAULT_MAX_USAGE_PCT: u8 = 75;

/// More entries than this in [`DATABASE_URLS_ENV`] is a configuration mistake.
const MAX_CLUSTERS: usize = 16;

/// Upper bound for one probe (connect + usage query). The connect timeout covers only the TCP
/// connect; an ssh tunnel accepts at once and may then stall, so the whole probe is bounded.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Client connections and the connection limit, read in one statement.
const USAGE_SQL: &str = "SELECT count(*) FILTER (WHERE backend_type = 'client backend')::int4, \
                         current_setting('max_connections')::int4 FROM pg_stat_activity";

/// One server that test databases can be created on: a parsed admin connection string plus its
/// place in the configured list. The label (`host:port/database`) never contains the user or the
/// password, so it is safe to print.
#[derive(Clone)]
pub struct Cluster {
    position: usize,
    count: usize,
    label: String,
    // Shared: a `Config` is large and clusters are cloned into every `TestDb`.
    config: Arc<Config>,
}

impl fmt::Debug for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The config carries the password: show only the label.
        f.debug_struct("Cluster")
            .field("position", &self.position)
            .field("count", &self.count)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cluster {}/{} ({})",
            self.position, self.count, self.label
        )
    }
}

impl Cluster {
    /// Parse one admin URL as entry `position` (1-based) of a list of `count`.
    pub(crate) fn parse(position: usize, count: usize, database_url: &str) -> Result<Self> {
        let config = admin_config(database_url)?;
        Ok(Self {
            position,
            count,
            label: label_of(&config),
            config: Arc::new(config),
        })
    }

    /// Parse a whitespace-separated list of admin URLs in the format of [`DATABASE_URLS_ENV`] (the
    /// counterpart of [`clusters_from_env`] for a list held elsewhere).
    ///
    /// # Errors
    /// [`TestkitError::InvalidDatabaseUrls`] naming the bad entry: empty list, more than 16
    /// entries, a key=value string, an invalid URL, no admin database, or the same server twice.
    pub fn parse_list(text: &str) -> Result<Vec<Self>> {
        parse_url_list(text)
    }

    /// `host:port/database` of the admin connection (no user, no password).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 1-based place in the configured list (1 for a pinned server).
    pub fn position(&self) -> usize {
        self.position
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    /// Connect and read this cluster's connection usage (one connection, closed again), within the
    /// same 15 s bound the selection uses.
    ///
    /// # Errors
    /// Anything that is not "unreachable" (wrong password, unknown admin database, SQL failure):
    /// [`TestkitError::ClusterConnect`], [`TestkitError::Postgres`] or
    /// [`TestkitError::InvalidUsage`].
    pub async fn probe(&self) -> Result<ClusterProbe> {
        Ok(match PgProber.probe(self).await? {
            ProbeOutcome::Ready { usage, .. } => ClusterProbe::Reachable(usage),
            ProbeOutcome::Unreachable { reason } => ClusterProbe::Unreachable(reason),
        })
    }

    /// Whether a database with this exact name exists on this cluster.
    ///
    /// # Errors
    /// Connection or query failures, as [`TestkitError::Connect`] or [`TestkitError::Postgres`].
    pub async fn database_exists(&self, name: &str) -> Result<bool> {
        let client = connect(&self.config).await?;
        let row = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
                &[&name],
            )
            .await
            .map_err(pg_error(format!("looking up database {name} on {self}")))?;
        row.try_get(0)
            .map_err(pg_error(format!("reading lookup of {name} on {self}")))
    }
}

/// `host:port[,host:port…]/database`, from the parsed config (never from the raw URL, which may
/// hold a password).
fn label_of(config: &Config) -> String {
    let ports = config.get_ports();
    let hosts: Vec<String> = config
        .get_hosts()
        .iter()
        .enumerate()
        .map(|(i, host)| {
            // libpq rule (the driver applies the same): a single port applies to every host, and
            // no port means 5432. This only builds the label, never the connection.
            let port = ports.get(i).or(ports.first()).copied().unwrap_or(5432);
            match host {
                Host::Tcp(name) => format!("{name}:{port}"),
                #[cfg(unix)]
                Host::Unix(path) => format!("{}:{port}", path.display()),
            }
        })
        .collect();
    let hosts = if hosts.is_empty() {
        "<default host>".to_owned()
    } else {
        hosts.join(",")
    };
    // `admin_config` has already refused a config without a database; the fallback is unreachable
    // and only keeps this label function total.
    format!("{hosts}/{}", config.get_dbname().unwrap_or_default())
}

/// Where new test databases go.
#[derive(Debug, Clone)]
pub(crate) enum Clusters {
    /// One server, used without a headroom check (`DATABASE_URL`, or pinned by the caller).
    Pinned(Cluster),
    /// Ordered candidates from [`DATABASE_URLS_ENV`], chosen by headroom.
    Ordered(Vec<Cluster>),
}

impl Clusters {
    /// From the environment: [`DATABASE_URLS_ENV`] when set, else `DATABASE_URL`.
    pub(crate) fn from_env() -> Result<Self> {
        match std::env::var(DATABASE_URLS_ENV) {
            Ok(text) => parse_url_list(&text).map(Self::Ordered),
            Err(std::env::VarError::NotPresent) => {
                let url = crate::db::database_url_from_env()?;
                Cluster::parse(1, 1, &url).map(Self::Pinned)
            }
            Err(std::env::VarError::NotUnicode(_)) => Err(TestkitError::InvalidDatabaseUrls {
                position: 0,
                reason: "the variable is not valid UTF-8".to_owned(),
            }),
        }
    }
}

/// The clusters new test databases may go to, in order: the entries of [`DATABASE_URLS_ENV`],
/// or the single server in `DATABASE_URL` when that list is not set.
///
/// # Errors
/// A malformed list, or neither variable set.
pub fn clusters_from_env() -> Result<Vec<Cluster>> {
    Ok(match Clusters::from_env()? {
        Clusters::Pinned(cluster) => vec![cluster],
        Clusters::Ordered(list) => list,
    })
}

/// Parse the whitespace-separated URL list of [`DATABASE_URLS_ENV`].
pub(crate) fn parse_url_list(text: &str) -> Result<Vec<Cluster>> {
    let urls: Vec<&str> = text.split_whitespace().collect();
    if urls.is_empty() {
        return Err(TestkitError::InvalidDatabaseUrls {
            position: 0,
            reason: "the list is empty".to_owned(),
        });
    }
    if urls.len() > MAX_CLUSTERS {
        return Err(TestkitError::InvalidDatabaseUrls {
            position: 0,
            reason: format!("{} entries, at most {MAX_CLUSTERS} allowed", urls.len()),
        });
    }
    let count = urls.len();
    let mut clusters: Vec<Cluster> = Vec::with_capacity(count);
    for (i, url) in urls.into_iter().enumerate() {
        let position = i + 1;
        // Key=value connection strings contain spaces and cannot be listed; require URLs.
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(TestkitError::InvalidDatabaseUrls {
                position,
                reason: "not a postgres:// or postgresql:// URL".to_owned(),
            });
        }
        let cluster = Cluster::parse(position, count, url).map_err(|error| match error {
            TestkitError::InvalidDatabaseUrl(_) => TestkitError::InvalidDatabaseUrls {
                position,
                reason: "not a valid PostgreSQL connection URL".to_owned(),
            },
            TestkitError::NestedDatabaseUrl => TestkitError::InvalidDatabaseUrls {
                position,
                reason: "database name contains another URL; separate admin URLs with whitespace, not commas".to_owned(),
            },
            TestkitError::MissingAdminDatabase => TestkitError::InvalidDatabaseUrls {
                position,
                reason: "names no admin database (e.g. `/erpforall`)".to_owned(),
            },
            other => other,
        })?;
        if let Some(earlier) = clusters.iter().find(|c| c.label == cluster.label) {
            return Err(TestkitError::InvalidDatabaseUrls {
                position,
                reason: format!("{} is already entry {}", cluster.label, earlier.position()),
            });
        }
        clusters.push(cluster);
    }
    Ok(clusters)
}

/// Highest connection usage, in percent of `max_connections`, at which a cluster still gets new
/// test databases: a cluster is used while `connections * 100 < percent * max_connections`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageThreshold(u8);

impl UsageThreshold {
    /// A threshold of `percent` (1–100).
    ///
    /// # Errors
    /// [`TestkitError::InvalidUsageThreshold`] outside 1–100.
    pub fn new(percent: u8) -> Result<Self> {
        if (1..=100).contains(&percent) {
            Ok(Self(percent))
        } else {
            Err(TestkitError::InvalidUsageThreshold {
                value: percent.to_string(),
            })
        }
    }

    /// The threshold in percent.
    pub fn percent(self) -> u8 {
        self.0
    }

    /// From [`MAX_USAGE_ENV`]; [`DEFAULT_MAX_USAGE_PCT`] only when the variable is absent.
    ///
    /// # Errors
    /// [`TestkitError::InvalidUsageThreshold`] when the variable is set to anything but an integer
    /// from 1 to 100 (including the empty string).
    pub fn from_env() -> Result<Self> {
        match std::env::var(MAX_USAGE_ENV) {
            Ok(text) => Self::parse(&text),
            Err(std::env::VarError::NotPresent) => Ok(Self(DEFAULT_MAX_USAGE_PCT)),
            Err(std::env::VarError::NotUnicode(raw)) => Err(TestkitError::InvalidUsageThreshold {
                value: raw.to_string_lossy().into_owned(),
            }),
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self> {
        let invalid = || TestkitError::InvalidUsageThreshold {
            value: text.to_owned(),
        };
        let percent: u8 = text.trim().parse().map_err(|_| invalid())?;
        Self::new(percent).map_err(|_| invalid())
    }

    /// Whether a cluster at `usage` may take another test database that will open up to `reserve`
    /// more connections (its pool size): `(connections + reserve) × 100 < percent ×
    /// max_connections`. See the module docs for why this is still a soft limit.
    pub fn admits(self, usage: Usage, reserve: u32) -> bool {
        (u64::from(usage.connections) + u64::from(reserve)) * 100
            < u64::from(self.0) * u64::from(usage.max_connections)
    }
}

/// Client connections of a cluster against its limit, as read from `pg_stat_activity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Usage {
    /// Client backends connected (including the probe itself).
    pub connections: u32,
    /// The server's `max_connections`.
    pub max_connections: u32,
}

impl Usage {
    /// Usage from its two parts.
    pub fn new(connections: u32, max_connections: u32) -> Self {
        Self {
            connections,
            max_connections,
        }
    }

    /// Whole percent used, rounded down (100 when `max_connections` is 0).
    pub fn percent(self) -> u64 {
        if self.max_connections == 0 {
            return 100;
        }
        u64::from(self.connections) * 100 / u64::from(self.max_connections)
    }
}

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} connections ({}%)",
            self.connections,
            self.max_connections,
            self.percent()
        )
    }
}

/// The kind of failure that makes a cluster unreachable right now, as opposed to misconfigured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnreachableKind {
    /// Nothing listens on the port (TCP connection refused): no server, or no ssh tunnel.
    Refused,
    /// The TCP connect or the whole probe took too long.
    TimedOut,
    /// The connection was accepted and closed before the server answered: an ssh tunnel whose far
    /// end has no server (container stopped) or refuses the forward.
    Closed,
    /// The connection was reset, aborted or broken during the handshake.
    Reset,
    /// No route to the host, network unreachable, or the address is not available.
    NoRoute,
    /// The server is starting up or shutting down (SQLSTATE 57P03).
    NotAccepting,
    /// The server has no free connection slot (SQLSTATE 53300).
    TooManyConnections,
    /// `erp-testkit probe` reported it unreachable before the tests ([`PROBED_UNREACHABLE_ENV`]);
    /// selection skips it without contacting it. The probe itself never reports this kind.
    ReportedUnreachable,
}

impl UnreachableKind {
    /// Stable lower-case name (`refused`, `timed-out`, `closed`, `reset`, `no-route`,
    /// `not-accepting`, `too-many-connections`, `reported-unreachable`), as `erp-testkit probe`
    /// prints it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::TimedOut => "timed-out",
            Self::Closed => "closed",
            Self::Reset => "reset",
            Self::NoRoute => "no-route",
            Self::NotAccepting => "not-accepting",
            Self::TooManyConnections => "too-many-connections",
            Self::ReportedUnreachable => "reported-unreachable",
        }
    }
}

impl fmt::Display for UnreachableKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a cluster is unreachable: the kind of failure and what the driver or server said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreachable {
    kind: UnreachableKind,
    message: String,
}

impl Unreachable {
    pub(crate) fn new(kind: UnreachableKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The kind of failure.
    pub fn kind(&self) -> UnreachableKind {
        self.kind
    }

    /// What the driver or server said (e.g. `Connection refused (os error 111)`).
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// The message alone; the kind is available from [`Unreachable::kind`].
impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// What [`Cluster::probe`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClusterProbe {
    /// Connected; the cluster's connection usage at that moment.
    Reachable(Usage),
    /// Not reachable or not accepting connections.
    Unreachable(Unreachable),
}

/// Why a cluster was passed over.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClusterState {
    /// Reachable, but at or above the threshold once the new database's pool is counted.
    Full(Usage),
    /// Not reachable or not accepting connections; the reason says which.
    Excluded {
        /// What the probe saw (connection refused, timed out, closed, starting up, …).
        reason: Unreachable,
    },
}

/// A cluster that was passed over, as listed in [`TestkitError::NoClusterAvailable`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SkippedCluster {
    /// `cluster <n>/<count> (<host:port/database>)`.
    pub cluster: String,
    /// Why it was passed over.
    pub state: ClusterState,
}

impl fmt::Display for SkippedCluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.state {
            ClusterState::Full(usage) => write!(f, "{}: full, {usage}", self.cluster),
            ClusterState::Excluded { reason } => write!(f, "{}: excluded, {reason}", self.cluster),
        }
    }
}

/// `a; b; c` for the error message.
pub(crate) fn render_skipped(skipped: &[SkippedCluster]) -> String {
    skipped
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// What probing one cluster found.
pub(crate) enum ProbeOutcome<C> {
    /// Connected; `client` is reused for the admin statements.
    Ready { client: C, usage: Usage },
    /// Not reachable or not accepting connections.
    Unreachable { reason: Unreachable },
}

/// Probes one cluster. The real implementation is [`PgProber`]; unit tests use a scripted one.
pub(crate) trait Prober {
    type Client;
    async fn probe(&mut self, cluster: &Cluster) -> Result<ProbeOutcome<Self::Client>>;
}

/// The cluster chosen for a new test database.
pub(crate) struct Chosen<C> {
    pub(crate) cluster: Cluster,
    pub(crate) client: C,
    pub(crate) usage: Usage,
}

/// Probe `clusters` in order and return the first one `threshold` admits with `reserve` more
/// connections (the new database's pool size). A cluster whose label is in `reported` (the probe
/// before the tests found it unreachable) is skipped without contacting it. Each pass-over is
/// reported to `log`; the remaining clusters are not probed once one is chosen.
pub(crate) async fn select<P: Prober>(
    clusters: &[Cluster],
    threshold: UsageThreshold,
    reserve: u32,
    reported: &[String],
    prober: &mut P,
    log: &mut (dyn FnMut(String) + Send),
) -> Result<Chosen<P::Client>> {
    let mut skipped = Vec::new();
    for cluster in clusters {
        if reported.iter().any(|label| label == cluster.label()) {
            let reason = Unreachable::new(
                UnreachableKind::ReportedUnreachable,
                format!(
                    "reported unreachable by erp-testkit probe before the tests ({PROBED_UNREACHABLE_ENV})"
                ),
            );
            log(format!("{cluster} excluded: {reason}"));
            skipped.push(SkippedCluster {
                cluster: cluster.to_string(),
                state: ClusterState::Excluded { reason },
            });
            continue;
        }
        match prober.probe(cluster).await? {
            ProbeOutcome::Ready { client, usage } if threshold.admits(usage, reserve) => {
                return Ok(Chosen {
                    cluster: cluster.clone(),
                    client,
                    usage,
                });
            }
            ProbeOutcome::Ready { usage, .. } => {
                log(format!(
                    "{cluster} skipped: {usage} plus a pool of {reserve}, threshold {}%",
                    threshold.percent()
                ));
                skipped.push(SkippedCluster {
                    cluster: cluster.to_string(),
                    state: ClusterState::Full(usage),
                });
            }
            ProbeOutcome::Unreachable { reason } => {
                log(format!("{cluster} excluded: {reason}"));
                skipped.push(SkippedCluster {
                    cluster: cluster.to_string(),
                    state: ClusterState::Excluded { reason },
                });
            }
        }
    }
    Err(TestkitError::NoClusterAvailable {
        threshold: threshold.percent(),
        skipped,
    })
}

/// Connection failures that mean "this cluster is not there right now" rather than "this
/// configuration is wrong", and which kind they are. `None` for every other I/O error.
pub(crate) fn unreachable_io_kind(kind: io::ErrorKind) -> Option<UnreachableKind> {
    match kind {
        io::ErrorKind::ConnectionRefused => Some(UnreachableKind::Refused),
        io::ErrorKind::TimedOut => Some(UnreachableKind::TimedOut),
        io::ErrorKind::UnexpectedEof => Some(UnreachableKind::Closed),
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected
        | io::ErrorKind::BrokenPipe => Some(UnreachableKind::Reset),
        io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::AddrNotAvailable => Some(UnreachableKind::NoRoute),
        // Any other I/O error (permission denied, invalid input, …) is not "the server is away"
        // and must fail the caller, so it is not mapped.
        _ => None,
    }
}

/// SQLSTATEs a server sends when it cannot take a connection right now, and which kind they are.
pub(crate) fn unreachable_sqlstate(code: &SqlState) -> Option<UnreachableKind> {
    if *code == SqlState::CANNOT_CONNECT_NOW {
        Some(UnreachableKind::NotAccepting)
    } else if *code == SqlState::TOO_MANY_CONNECTIONS {
        Some(UnreachableKind::TooManyConnections)
    } else {
        None
    }
}

/// Why a driver error means the cluster is unreachable, or `None` when it is any other failure.
pub(crate) fn unreachable_reason(error: &tokio_postgres::Error) -> Option<Unreachable> {
    if error.is_closed() {
        return Some(Unreachable::new(
            UnreachableKind::Closed,
            "connection closed before the server answered",
        ));
    }
    if let Some(db) = error.as_db_error()
        && let Some(kind) = unreachable_sqlstate(db.code())
    {
        return Some(Unreachable::new(
            kind,
            format!(
                "server refused the connection: {} (SQLSTATE {})",
                db.message(),
                db.code().code()
            ),
        ));
    }
    let mut cause: Option<&(dyn StdError + 'static)> = error.source();
    while let Some(inner) = cause {
        if let Some(io) = inner.downcast_ref::<io::Error>()
            && let Some(kind) = unreachable_io_kind(io.kind())
        {
            return Some(Unreachable::new(kind, io.to_string()));
        }
        cause = inner.source();
    }
    None
}

/// Probes real servers: connect, then read [`USAGE_SQL`], within [`PROBE_TIMEOUT`].
pub(crate) struct PgProber;

impl Prober for PgProber {
    type Client = Client;

    async fn probe(&mut self, cluster: &Cluster) -> Result<ProbeOutcome<Client>> {
        match tokio::time::timeout(PROBE_TIMEOUT, probe_once(cluster)).await {
            Ok(outcome) => outcome,
            Err(_) => Ok(ProbeOutcome::Unreachable {
                reason: Unreachable::new(
                    UnreachableKind::TimedOut,
                    format!("no answer within {} s", PROBE_TIMEOUT.as_secs()),
                ),
            }),
        }
    }
}

async fn probe_once(cluster: &Cluster) -> Result<ProbeOutcome<Client>> {
    let client = match connect(cluster.config()).await {
        Ok(client) => client,
        Err(TestkitError::Connect(error)) => {
            return match unreachable_reason(&error) {
                Some(reason) => Ok(ProbeOutcome::Unreachable { reason }),
                None => Err(TestkitError::ClusterConnect {
                    cluster: cluster.to_string(),
                    source: error,
                }),
            };
        }
        Err(other) => return Err(other),
    };
    let row = match client.query_one(USAGE_SQL, &[]).await {
        Ok(row) => row,
        Err(error) => {
            return match unreachable_reason(&error) {
                Some(reason) => Ok(ProbeOutcome::Unreachable { reason }),
                None => Err(pg_error(format!("reading connection usage on {cluster}"))(
                    error,
                )),
            };
        }
    };
    let action = || format!("reading connection usage on {cluster}");
    let connections: i32 = row.try_get(0).map_err(pg_error(action()))?;
    let max_connections: i32 = row.try_get(1).map_err(pg_error(action()))?;
    let (Ok(connections), Ok(max_connections)) =
        (u32::try_from(connections), u32::try_from(max_connections))
    else {
        return Err(TestkitError::InvalidUsage {
            cluster: cluster.to_string(),
            connections: i64::from(connections),
            max_connections: i64::from(max_connections),
        });
    };
    Ok(ProbeOutcome::Ready {
        client,
        usage: Usage::new(connections, max_connections),
    })
}

/// Print a testkit diagnostic line to stderr.
pub(crate) fn stderr_log(line: String) {
    eprintln!("erp-testkit: {line}");
}

/// The labels in [`PROBED_UNREACHABLE_ENV`]; none when it is absent or empty.
///
/// # Errors
/// [`TestkitError::Configuration`] when the variable is not valid UTF-8.
pub(crate) fn probed_unreachable_from_env() -> Result<Vec<String>> {
    match std::env::var(PROBED_UNREACHABLE_ENV) {
        Ok(text) => Ok(text.split_whitespace().map(str::to_owned).collect()),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(std::env::VarError::NotUnicode(_)) => Err(TestkitError::Configuration(format!(
            "{PROBED_UNREACHABLE_ENV} is not valid UTF-8"
        ))),
    }
}

/// The cluster log file from [`CLUSTER_LOG_ENV`]: `None` when the variable is absent.
///
/// # Errors
/// [`TestkitError::EmptyClusterLogPath`] when it is set but empty (unset it to disable the log).
pub(crate) fn cluster_log_from_env() -> Result<Option<PathBuf>> {
    cluster_log_path(std::env::var_os(CLUSTER_LOG_ENV))
}

/// [`cluster_log_from_env`] on a given value of the variable.
fn cluster_log_path(value: Option<std::ffi::OsString>) -> Result<Option<PathBuf>> {
    match value {
        None => Ok(None),
        Some(path) if path.is_empty() => Err(TestkitError::EmptyClusterLogPath),
        Some(path) => Ok(Some(PathBuf::from(path))),
    }
}

/// Record the cluster a test database was created on: stderr always, plus one tab-separated line
/// (`unix seconds, database, cluster label, usage or "pinned", run id or "-"`) appended to
/// `log_path` (from [`cluster_log_from_env`]) when there is one. A file it creates gets mode 0600.
pub(crate) fn record_choice(
    log_path: Option<&Path>,
    cluster: &Cluster,
    name: &str,
    usage: Option<Usage>,
    run_id: Option<&str>,
    now: u64,
) -> Result<()> {
    let usage_text = usage.map_or_else(|| "pinned".to_owned(), |u| u.to_string());
    stderr_log(format!("created {name} on {cluster}, {usage_text}"));
    let Some(path) = log_path else {
        return Ok(());
    };
    let usage_field = usage.map_or_else(
        || "pinned".to_owned(),
        |u| format!("{}/{}", u.connections, u.max_connections),
    );
    let line = format!(
        "{now}\t{name}\t{}\t{usage_field}\t{}\n",
        cluster.label(),
        run_id.unwrap_or("-")
    );
    let write = || -> io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // One write per line: O_APPEND keeps lines from parallel test processes whole.
        options.open(path)?.write_all(line.as_bytes())
    };
    write().map_err(|source| TestkitError::ClusterLog {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn clusters(urls: &[&str]) -> Vec<Cluster> {
        parse_url_list(&urls.join(" ")).expect("valid list")
    }

    fn invalid(text: &str) -> (usize, String) {
        match parse_url_list(text).expect_err("must be rejected") {
            TestkitError::InvalidDatabaseUrls { position, reason } => (position, reason),
            other => panic!("unexpected error kind: {other:?}"),
        }
    }

    /// A prober that answers from a script keyed by label and records what it was asked.
    #[derive(Default)]
    struct Scripted {
        answers: HashMap<String, Answer>,
        probed: Vec<String>,
    }

    #[derive(Clone)]
    enum Answer {
        Usage(u32, u32),
        Down(UnreachableKind, &'static str),
        Fail,
    }

    impl Scripted {
        fn with(mut self, label: &str, answer: Answer) -> Self {
            self.answers.insert(label.to_owned(), answer);
            self
        }
    }

    impl Prober for Scripted {
        type Client = String;
        async fn probe(&mut self, cluster: &Cluster) -> Result<ProbeOutcome<String>> {
            self.probed.push(cluster.label().to_owned());
            match self.answers.get(cluster.label()).cloned() {
                Some(Answer::Usage(used, max)) => Ok(ProbeOutcome::Ready {
                    client: format!("client of {}", cluster.label()),
                    usage: Usage::new(used, max),
                }),
                Some(Answer::Down(kind, message)) => Ok(ProbeOutcome::Unreachable {
                    reason: Unreachable::new(kind, message),
                }),
                Some(Answer::Fail) => Err(TestkitError::MissingAdminDatabase),
                None => panic!("no scripted answer for {}", cluster.label()),
            }
        }
    }

    fn run(
        list: &[Cluster],
        threshold: u8,
        prober: &mut Scripted,
    ) -> (Result<Chosen<String>>, Vec<String>) {
        run_with_reserve(list, threshold, 0, prober)
    }

    fn run_with_reserve(
        list: &[Cluster],
        threshold: u8,
        reserve: u32,
        prober: &mut Scripted,
    ) -> (Result<Chosen<String>>, Vec<String>) {
        let mut lines = Vec::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let threshold = UsageThreshold::new(threshold).expect("threshold");
        let result = runtime.block_on(select(list, threshold, reserve, &[], prober, &mut |l| {
            lines.push(l)
        }));
        (result, lines)
    }

    const A: &str = "postgres://u:secret@127.0.0.1:5540/erpforall";
    const B: &str = "postgres://u:secret@127.0.0.1:5542/erpforall";
    const C: &str = "postgresql://u:secret@db.example:6000/admin";

    #[test]
    fn list_parses_in_order_with_labels_and_positions() {
        let list = clusters(&[A, B, C]);
        let labels: Vec<&str> = list.iter().map(Cluster::label).collect();
        assert_eq!(
            labels,
            [
                "127.0.0.1:5540/erpforall",
                "127.0.0.1:5542/erpforall",
                "db.example:6000/admin"
            ]
        );
        let shown: Vec<String> = list.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "cluster 1/3 (127.0.0.1:5540/erpforall)",
                "cluster 2/3 (127.0.0.1:5542/erpforall)",
                "cluster 3/3 (db.example:6000/admin)"
            ]
        );
    }

    #[test]
    fn list_accepts_any_whitespace_including_newlines_and_crlf() {
        let list = parse_url_list(&format!("  {A}\r\n\t{B}\n")).expect("valid list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].position(), 2);
    }

    #[test]
    fn labels_and_debug_never_contain_credentials() {
        for cluster in clusters(&[A, B]) {
            for text in [
                cluster.label().to_owned(),
                cluster.to_string(),
                format!("{cluster:?}"),
            ] {
                assert!(!text.contains("secret"), "{text}");
                assert!(!text.contains("u:"), "{text}");
            }
        }
    }

    #[test]
    fn joined_urls_are_rejected_before_credentials_reach_diagnostics() {
        for text in [
            "postgres://first:dummy-one@127.0.0.1/db,postgres://second:dummy-two@127.0.0.1/other",
            "postgres://first:dummy-one@127.0.0.1/db,postgresql://second:dummy-two@127.0.0.1/other",
            "postgres://first:dummy-one@127.0.0.1/db%2Cpostgres%3A%2F%2Fsecond%3Adummy-two%40127.0.0.1%2Fother",
        ] {
            let reason = "database name contains another URL; separate admin URLs with whitespace, not commas";
            let list_error =
                parse_url_list(text).expect_err("joined list must fail before connection");
            assert!(
                matches!(&list_error, TestkitError::InvalidDatabaseUrls { position: 1, reason: actual } if actual == reason)
            );
            assert_eq!(
                list_error.to_string(),
                format!("ERP_TEST_DATABASE_URLS entry 1: {reason}")
            );
            let pinned_error =
                Cluster::parse(1, 1, text).expect_err("joined URL must fail before connection");
            assert!(matches!(pinned_error, TestkitError::NestedDatabaseUrl));
            assert_eq!(pinned_error.to_string(), reason);
            for error in [list_error, pinned_error] {
                let display = error.to_string();
                let debug = format!("{error:?}");
                let chain = crate::error::error_chain(&error);
                for shown in [display, debug, chain] {
                    assert!(!shown.contains("dummy-one"));
                    assert!(!shown.contains("dummy-two"));
                    assert!(!shown.contains("postgres://"));
                }
            }
        }
    }

    #[test]
    fn default_port_is_5432() {
        let list = clusters(&["postgres://u@db.example/erpforall"]);
        assert_eq!(list[0].label(), "db.example:5432/erpforall");
    }

    #[test]
    fn empty_list_is_an_error() {
        assert_eq!(invalid(""), (0, "the list is empty".to_owned()));
        assert_eq!(invalid(" \n\t "), (0, "the list is empty".to_owned()));
    }

    #[test]
    fn too_many_entries_are_an_error() {
        let urls: Vec<String> = (0..17)
            .map(|i| format!("postgres://u@127.0.0.1:{}/erpforall", 6000 + i))
            .collect();
        assert_eq!(
            invalid(&urls.join(" ")),
            (0, "17 entries, at most 16 allowed".to_owned())
        );
    }

    #[test]
    fn key_value_strings_and_bad_urls_are_errors_naming_the_entry() {
        assert_eq!(
            invalid(&format!("{A} host=127.0.0.1")),
            (2, "not a postgres:// or postgresql:// URL".to_owned())
        );
        assert_eq!(
            invalid(&format!("{A} {B} postgres://u@127.0.0.1:notaport/x")),
            (3, "not a valid PostgreSQL connection URL".to_owned())
        );
        assert_eq!(
            invalid("postgres://u@127.0.0.1:5540"),
            (1, "names no admin database (e.g. `/erpforall`)".to_owned())
        );
    }

    #[test]
    fn duplicate_servers_are_an_error() {
        // Same server, different credentials: still the same cluster.
        assert_eq!(
            invalid(&format!(
                "{A} {B} postgres://other@127.0.0.1:5540/erpforall"
            )),
            (3, "127.0.0.1:5540/erpforall is already entry 1".to_owned())
        );
    }

    #[test]
    fn threshold_parses_1_to_100_only() {
        assert_eq!(UsageThreshold::parse("75").expect("75").percent(), 75);
        assert_eq!(UsageThreshold::parse(" 1 ").expect("1").percent(), 1);
        assert_eq!(UsageThreshold::parse("100").expect("100").percent(), 100);
        for bad in ["", "0", "101", "256", "-5", "75%", "7.5", "seventy", "٧٥"] {
            match UsageThreshold::parse(bad).expect_err(bad) {
                TestkitError::InvalidUsageThreshold { value } => assert_eq!(value, bad),
                other => panic!("unexpected error kind for {bad:?}: {other:?}"),
            }
        }
        let error = UsageThreshold::parse("0").expect_err("0");
        assert_eq!(
            error.to_string(),
            "ERP_TEST_DB_MAX_USAGE_PCT must be an integer from 1 to 100, got \"0\""
        );
    }

    #[test]
    fn threshold_boundary_is_strict() {
        let t = UsageThreshold::new(75).expect("75");
        assert!(t.admits(Usage::new(149, 200), 0));
        assert!(!t.admits(Usage::new(150, 200), 0), "exactly 75% is full");
        assert!(!t.admits(Usage::new(500, 200), 0), "over the limit");
        assert!(
            !t.admits(Usage::new(0, 0), 0),
            "a zero limit admits nothing"
        );
        let all = UsageThreshold::new(100).expect("100");
        assert!(all.admits(Usage::new(199, 200), 0));
        assert!(!all.admits(Usage::new(200, 200), 0));
        // No overflow at the extremes.
        assert!(!all.admits(Usage::new(u32::MAX, u32::MAX), 0));
        assert!(all.admits(Usage::new(u32::MAX - 1, u32::MAX), 0));
        assert!(!all.admits(Usage::new(u32::MAX, u32::MAX), u32::MAX));
    }

    #[test]
    fn threshold_counts_the_pool_the_new_database_will_open() {
        let t = UsageThreshold::new(75).expect("75");
        // 145 + 4 = 149 < 150: admitted; 146 + 4 = 150: full.
        assert!(t.admits(Usage::new(145, 200), 4));
        assert!(!t.admits(Usage::new(146, 200), 4));
        // A pool larger than the whole headroom is never admitted, even on an idle cluster.
        assert!(!t.admits(Usage::new(1, 200), 150));
        assert!(t.admits(Usage::new(1, 200), 148));
    }

    #[test]
    fn usage_display() {
        assert_eq!(
            Usage::new(150, 200).to_string(),
            "150/200 connections (75%)"
        );
        assert_eq!(Usage::new(3, 0).to_string(), "3/0 connections (100%)");
    }

    #[test]
    fn first_cluster_with_headroom_wins_and_later_ones_are_not_probed() {
        let list = clusters(&[A, B, C]);
        let mut prober = Scripted::default()
            .with("127.0.0.1:5540/erpforall", Answer::Usage(10, 200))
            .with("127.0.0.1:5542/erpforall", Answer::Usage(0, 200))
            .with("db.example:6000/admin", Answer::Usage(0, 200));
        let (result, lines) = run(&list, 75, &mut prober);
        let chosen = result.expect("chosen");
        assert_eq!(chosen.cluster.position(), 1);
        assert_eq!(chosen.client, "client of 127.0.0.1:5540/erpforall");
        assert_eq!(chosen.usage, Usage::new(10, 200));
        assert_eq!(prober.probed, ["127.0.0.1:5540/erpforall"]);
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_full_cluster_falls_over_to_the_next() {
        let list = clusters(&[A, B]);
        let mut prober = Scripted::default()
            .with("127.0.0.1:5540/erpforall", Answer::Usage(160, 200))
            .with("127.0.0.1:5542/erpforall", Answer::Usage(5, 200));
        let (result, lines) = run(&list, 75, &mut prober);
        let chosen = result.expect("chosen");
        assert_eq!(chosen.cluster.position(), 2);
        assert_eq!(chosen.usage, Usage::new(5, 200));
        assert_eq!(
            lines,
            [
                "cluster 1/2 (127.0.0.1:5540/erpforall) skipped: 160/200 connections (80%) plus a pool of 0, threshold 75%"
            ]
        );
    }

    #[test]
    fn the_pool_size_can_push_a_cluster_over_the_threshold() {
        let list = clusters(&[A, B]);
        let mut prober = Scripted::default()
            .with("127.0.0.1:5540/erpforall", Answer::Usage(146, 200))
            .with("127.0.0.1:5542/erpforall", Answer::Usage(5, 200));
        let (result, lines) = run_with_reserve(&list, 75, 4, &mut prober);
        assert_eq!(result.expect("chosen").cluster.position(), 2);
        assert_eq!(
            lines,
            [
                "cluster 1/2 (127.0.0.1:5540/erpforall) skipped: 146/200 connections (73%) plus a pool of 4, threshold 75%"
            ]
        );
        // Without the pool, the same usage is admitted.
        let (result, lines) = run_with_reserve(&list, 75, 0, &mut prober);
        assert_eq!(result.expect("chosen").cluster.position(), 1);
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn an_unreachable_cluster_is_excluded_with_a_log_line() {
        let list = clusters(&[B, A]);
        let mut prober = Scripted::default()
            .with(
                "127.0.0.1:5542/erpforall",
                Answer::Down(
                    UnreachableKind::Refused,
                    "Connection refused (os error 111)",
                ),
            )
            .with("127.0.0.1:5540/erpforall", Answer::Usage(100, 200));
        let (result, lines) = run(&list, 75, &mut prober);
        assert_eq!(result.expect("chosen").cluster.position(), 2);
        assert_eq!(
            lines,
            ["cluster 1/2 (127.0.0.1:5542/erpforall) excluded: Connection refused (os error 111)"]
        );
    }

    /// A cluster the probe before the tests reported unreachable is skipped without being
    /// contacted, even if it would answer now: its timeouts were never checked (TEST.NODE-MACOS).
    #[test]
    fn a_cluster_reported_unreachable_by_the_probe_is_skipped_uncontacted() {
        let list = clusters(&[B, A]);
        let mut prober = Scripted::default()
            .with("127.0.0.1:5542/erpforall", Answer::Usage(0, 200))
            .with("127.0.0.1:5540/erpforall", Answer::Usage(100, 200));
        let reported = vec!["127.0.0.1:5542/erpforall".to_owned()];
        let mut lines = Vec::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let threshold = UsageThreshold::new(75).expect("threshold");
        let chosen = runtime
            .block_on(select(
                &list,
                threshold,
                0,
                &reported,
                &mut prober,
                &mut |l| lines.push(l),
            ))
            .expect("chosen");
        assert_eq!(chosen.cluster.position(), 2);
        assert_eq!(prober.probed, ["127.0.0.1:5540/erpforall"]);
        assert_eq!(
            lines,
            [
                "cluster 1/2 (127.0.0.1:5542/erpforall) excluded: reported unreachable by \
                 erp-testkit probe before the tests (ERP_TESTKIT_UNREACHABLE_CLUSTERS)"
            ]
        );
        // every cluster reported: an error listing each, nothing contacted
        let mut prober = Scripted::default();
        let all = vec![
            "127.0.0.1:5542/erpforall".to_owned(),
            "127.0.0.1:5540/erpforall".to_owned(),
        ];
        let error = runtime
            .block_on(select(&list, threshold, 0, &all, &mut prober, &mut |_| {}))
            .err()
            .expect("nothing left");
        assert!(prober.probed.is_empty());
        assert!(
            matches!(&error, TestkitError::NoClusterAvailable { skipped, .. } if skipped.len() == 2),
            "{error:?}"
        );
    }

    #[test]
    fn all_full_or_excluded_is_an_error_listing_every_cluster() {
        let list = clusters(&[A, B, C]);
        let mut prober = Scripted::default()
            .with("127.0.0.1:5540/erpforall", Answer::Usage(150, 200))
            .with(
                "127.0.0.1:5542/erpforall",
                Answer::Down(
                    UnreachableKind::Closed,
                    "connection closed before the server answered",
                ),
            )
            .with("db.example:6000/admin", Answer::Usage(499, 500));
        let (result, lines) = run(&list, 75, &mut prober);
        let error = match result {
            Err(error) => error,
            Ok(chosen) => panic!("expected an error, chose {}", chosen.cluster),
        };
        assert_eq!(lines.len(), 3, "{lines:?}");
        match &error {
            TestkitError::NoClusterAvailable { threshold, skipped } => {
                assert_eq!(*threshold, 75);
                assert_eq!(skipped.len(), 3);
                assert_eq!(skipped[0].state, ClusterState::Full(Usage::new(150, 200)));
                assert_eq!(
                    skipped[1].state,
                    ClusterState::Excluded {
                        reason: Unreachable::new(
                            UnreachableKind::Closed,
                            "connection closed before the server answered"
                        )
                    }
                );
            }
            other => panic!("unexpected error kind: {other:?}"),
        }
        assert_eq!(
            error.to_string(),
            "no test database cluster is below 75% of max_connections: \
             cluster 1/3 (127.0.0.1:5540/erpforall): full, 150/200 connections (75%); \
             cluster 2/3 (127.0.0.1:5542/erpforall): excluded, connection closed before the server answered; \
             cluster 3/3 (db.example:6000/admin): full, 499/500 connections (99%)"
        );
    }

    #[test]
    fn a_configuration_error_stops_at_once_without_trying_the_next_cluster() {
        let list = clusters(&[A, B]);
        let mut prober = Scripted::default()
            .with("127.0.0.1:5540/erpforall", Answer::Fail)
            .with("127.0.0.1:5542/erpforall", Answer::Usage(0, 200));
        let (result, _) = run(&list, 75, &mut prober);
        assert!(matches!(result, Err(TestkitError::MissingAdminDatabase)));
        assert_eq!(prober.probed, ["127.0.0.1:5540/erpforall"]);
    }

    #[test]
    fn repeated_selection_follows_changing_usage() {
        let list = clusters(&[A, B]);
        let mut prober = Scripted::default()
            .with("127.0.0.1:5540/erpforall", Answer::Usage(160, 200))
            .with("127.0.0.1:5542/erpforall", Answer::Usage(5, 200));
        assert_eq!(
            run(&list, 75, &mut prober)
                .0
                .expect("first")
                .cluster
                .position(),
            2
        );
        prober = prober.with("127.0.0.1:5540/erpforall", Answer::Usage(20, 200));
        assert_eq!(
            run(&list, 75, &mut prober)
                .0
                .expect("second")
                .cluster
                .position(),
            1
        );
    }

    #[test]
    fn io_kinds_that_mean_unreachable() {
        for (io_kind, kind) in [
            (io::ErrorKind::ConnectionRefused, UnreachableKind::Refused),
            (io::ErrorKind::TimedOut, UnreachableKind::TimedOut),
            (io::ErrorKind::UnexpectedEof, UnreachableKind::Closed),
            (io::ErrorKind::ConnectionReset, UnreachableKind::Reset),
            (io::ErrorKind::ConnectionAborted, UnreachableKind::Reset),
            (io::ErrorKind::NotConnected, UnreachableKind::Reset),
            (io::ErrorKind::BrokenPipe, UnreachableKind::Reset),
            (io::ErrorKind::HostUnreachable, UnreachableKind::NoRoute),
            (io::ErrorKind::NetworkUnreachable, UnreachableKind::NoRoute),
            (io::ErrorKind::AddrNotAvailable, UnreachableKind::NoRoute),
        ] {
            assert_eq!(unreachable_io_kind(io_kind), Some(kind), "{io_kind:?}");
        }
        for io_kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::InvalidData,
            io::ErrorKind::Other,
            io::ErrorKind::NotFound,
        ] {
            assert_eq!(unreachable_io_kind(io_kind), None, "{io_kind:?}");
        }
    }

    #[test]
    fn sqlstates_that_mean_unreachable() {
        assert_eq!(
            unreachable_sqlstate(&SqlState::CANNOT_CONNECT_NOW),
            Some(UnreachableKind::NotAccepting)
        );
        assert_eq!(
            unreachable_sqlstate(&SqlState::TOO_MANY_CONNECTIONS),
            Some(UnreachableKind::TooManyConnections)
        );
        for code in [
            SqlState::INVALID_PASSWORD,
            SqlState::INVALID_CATALOG_NAME,
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
        ] {
            assert_eq!(unreachable_sqlstate(&code), None, "{}", code.code());
        }
    }

    #[test]
    fn unreachable_kind_names_are_stable() {
        let names: Vec<&str> = [
            UnreachableKind::Refused,
            UnreachableKind::TimedOut,
            UnreachableKind::Closed,
            UnreachableKind::Reset,
            UnreachableKind::NoRoute,
            UnreachableKind::NotAccepting,
            UnreachableKind::TooManyConnections,
            UnreachableKind::ReportedUnreachable,
        ]
        .into_iter()
        .map(UnreachableKind::as_str)
        .collect();
        assert_eq!(
            names,
            [
                "refused",
                "timed-out",
                "closed",
                "reset",
                "no-route",
                "not-accepting",
                "too-many-connections",
                "reported-unreachable"
            ]
        );
        let reason = Unreachable::new(UnreachableKind::Refused, "Connection refused");
        assert_eq!(reason.kind(), UnreachableKind::Refused);
        assert_eq!(reason.message(), "Connection refused");
        assert_eq!(reason.to_string(), "Connection refused");
    }

    /// A fresh, empty directory under the system temp dir for one test.
    fn scratch_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "erp-testkit-{test}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn record_choice_appends_one_line_per_database_to_a_private_file() {
        let dir = scratch_dir("cluster-log");
        let path = dir.join("testdb.tsv");
        let list = clusters(&[A, B]);
        record_choice(
            Some(&path),
            &list[0],
            "erpforall_test_1_0_100",
            Some(Usage::new(12, 500)),
            Some("ci-run"),
            100,
        )
        .expect("first line");
        record_choice(
            Some(&path),
            &list[1],
            "erpforall_test_1_1_101",
            None,
            None,
            101,
        )
        .expect("second line");
        let text = std::fs::read_to_string(&path).expect("read log");
        assert_eq!(
            text,
            "100\terpforall_test_1_0_100\t127.0.0.1:5540/erpforall\t12/500\tci-run\n\
             101\terpforall_test_1_1_101\t127.0.0.1:5542/erpforall\tpinned\t-\n"
        );
        assert!(!text.contains("secret"), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        }
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

    #[test]
    fn record_choice_without_a_log_path_writes_no_file() {
        let list = clusters(&[A]);
        record_choice(None, &list[0], "erpforall_test_1_0_100", None, None, 100)
            .expect("stderr only");
    }

    #[test]
    fn an_unwritable_cluster_log_is_an_error_naming_the_file() {
        // A directory cannot be opened for appending, whoever runs the test (root included).
        let dir = scratch_dir("cluster-log-dir");
        let list = clusters(&[A]);
        let error = record_choice(
            Some(&dir),
            &list[0],
            "erpforall_test_1_0_100",
            None,
            None,
            100,
        )
        .expect_err("a directory is not a log file");
        match &error {
            TestkitError::ClusterLog { path, source } => {
                assert_eq!(path, &dir);
                assert_eq!(source.kind(), io::ErrorKind::IsADirectory);
            }
            other => panic!("unexpected error kind: {other:?}"),
        }
        assert_eq!(
            error.to_string(),
            format!("could not append to the cluster log {}", dir.display())
        );
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

    #[test]
    fn the_cluster_log_variable_absent_set_or_empty() {
        assert_eq!(cluster_log_path(None).expect("absent"), None);
        assert_eq!(
            cluster_log_path(Some("logs/testdb.tsv".into())).expect("set"),
            Some(PathBuf::from("logs/testdb.tsv"))
        );
        let error = cluster_log_path(Some("".into())).expect_err("empty");
        assert!(
            matches!(error, TestkitError::EmptyClusterLogPath),
            "unexpected error kind: {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "ERP_TESTKIT_CLUSTER_LOG is set but empty; unset it to disable the cluster log"
        );
    }
}
