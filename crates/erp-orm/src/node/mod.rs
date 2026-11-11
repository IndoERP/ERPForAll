//! The node registry (design, spec
//! ).
//!
//! Every process that serves a database writes one row in that database's `erp_node` table and
//! renews it as a lease ([`NodeMembership`]). From those rows each process knows whether it may
//! serve (its own lease is fresh, and its configuration, build and addon digests equal those of
//! the ready replicas or an open [`Rollout`] window allows both), how many replicas of its role are
//! live ([`NodeView::divisor`]), and which other nodes are live ([`NodeView::fresh_peers`]).
//!
//! - Every timestamp is `now()` on the primary; a node uses its own clock only for its
//!   self-fence deadline ([`NodeAdmission::is_serving`]): a monotonic clock that keeps counting
//!   while the host is suspended (`CLOCK_BOOTTIME` on Linux, `CLOCK_MONOTONIC` on Apple
//!   platforms).
//! - A fresh lease never means exclusive ownership of anything: work that must run once (jobs,
//!   ) is fenced by its own epochs.
//! - A renewal is one statement in one round trip, committed asynchronously, a HOT update.
//! - A node that cannot renew turns unready and says why: a permanent failure at once, a
//!   transient one when its self-fence deadline passes. It never stays ready silently.
//!
//! The tables are installed by [`Orm::sync_schema`](crate::Orm::sync_schema) (core DDL), never
//! by a replica. The request role gets no privilege on them
//! ([`Transaction::grant_node_role`](crate::Transaction::grant_node_role) is for the heartbeat
//! role).

use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

mod clock;
mod digest;
mod error;
pub mod feature;
mod grant;
mod heartbeat;
mod identity;
mod membership;
mod options;
mod rollout;
mod sql;

pub use digest::{
    AddonContent, AddonEntryDigest, AddonsDigest, AddonsDigestBuilder, BuildIdentity, BuildKind,
    ConfigDigest, DeclaredSetting, Digest, MAX_SETTING_STR_BYTES, MAX_TREE_BYTES, MAX_TREE_DEPTH,
    MAX_TREE_ENTRIES, MAX_TREE_FILES, MIN_SHARED_SECRET_BYTES, ResolvedBuild, Scope, SectionDigest,
    SectionName, SettingsWriter, SharedSettings, TreeHasher,
};
pub use error::NodeError;
pub use feature::{
    FeatureBinding, FeatureKey, FeatureRollout, FeatureSet, FeatureSnapshot, FeatureStatus,
    FeatureVersion, LegacyEnrollment,
};
pub use identity::{NodeIdentity, NodeIdentityBuilder};
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub use membership::{FenceTiming, RegisterFault};
pub use membership::{NodeAdmission, NodeMembership};
pub use options::{NodeOptions, NodeOptionsBuilder};
pub use rollout::{NodeRow, Rollout, list_nodes};

/// The build identity of the crate that expands it (the server's link root): package name and
/// version, profile, and the commit and dirty flag from `ERP_BUILD_COMMIT` / `ERP_BUILD_DIRTY` at
/// compile time (design §8.3). Without a commit, or with a dirty tree, the build digest is the
/// SHA-256 of the running executable.
///
/// ```
/// let build = erp_orm::build_identity!();
/// let resolved = build.resolve().unwrap();
/// assert!(!resolved.label().is_empty());
/// ```
#[macro_export]
macro_rules! build_identity {
    () => {
        $crate::node::BuildIdentity::__from_build(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            cfg!(debug_assertions),
            option_env!("ERP_BUILD_COMMIT"),
            option_env!("ERP_BUILD_DIRTY"),
        )
    };
}

/// First key of ERPForAll's advisory locks (`"ERPF"`, as `erp_modules::system::LOCK_CLASS`).
pub const LOCK_CLASS: i32 = 0x4552_5046;

/// The transaction advisory lock of readiness transitions, rollout windows and registrations of
/// one database (design §7.2): `(class, key)` = (`"ERPF"`, 3). Keys 1 and 2 of the class are the
/// module system's (`erp_modules::system::OPERATION_LOCK`, `POST_COMMIT_LOCK`); the key space is
/// listed there.
pub const READINESS_LOCK: (i32, i32) = (LOCK_CLASS, 3);

/// The connection settings of a heartbeat pool (design §6.5): TCP keepalives, so a connection to
/// a database host that is gone (a crashed host, a dropped network path) fails within about two
/// intervals for operator settings instead of waiting for the kernel's default of hours. The server side of a stopped
/// client is bounded by the registry's transaction timeouts (every registry transaction sets
/// `idle_in_transaction_session_timeout` and `transaction_timeout`). TCP keepalive idle/interval
/// values have a one-second floor for kernel granularity; this does not change heartbeat
/// scheduling, lease durations or the configured TCP user timeout.
pub fn heartbeat_config(
    mut config: tokio_postgres::Config,
    options: &NodeOptions,
) -> tokio_postgres::Config {
    let interval = options.interval();
    // TCP_KEEPIDLE/TCP_KEEPINTVL have whole-second kernel granularity. Short test
    // heartbeats must not become zero-valued socket options on Linux (lead r1 S-5).
    let socket_floor = std::time::Duration::from_secs(1);
    config
        .keepalives(true)
        .keepalives_idle(interval.max(socket_floor))
        .keepalives_interval((interval / 4).max(socket_floor))
        .keepalives_retries(4)
        // unacknowledged data (a renewal sent into a dead path) fails after one interval too
        .tcp_user_timeout(interval);
    config
}

/// A heartbeat pool: one connection with [`heartbeat_config`]'s settings. A membership needs one
/// connection; request transactions never draw from this pool.
///
/// # Errors
///
/// [`NodeError::InvalidSetting`] when the pool cannot be built.
pub fn heartbeat_pool(
    config: tokio_postgres::Config,
    options: &NodeOptions,
) -> Result<deadpool_postgres::Pool, NodeError> {
    crate::pool::create_pool(heartbeat_config(config, options), 1).map_err(|e| {
        NodeError::invalid_setting("node.heartbeat_pool", format!("heartbeat pool: {e}"))
    })
}

/// Test seam: the `BEGIN` batch every registry transaction opens with (its statement bound, and
/// its transaction and idle bounds), for the test that the server ends such a transaction at its
/// bound.
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub fn registry_begin_for_tests(statement: Duration, transaction: Duration) -> String {
    sql::begin_sql(heartbeat::ms(statement), heartbeat::ms(transaction), false)
}

/// The node tables (design §4), created by [`Orm::sync_schema`](crate::Orm::sync_schema) in one
/// entry so the detail table's foreign key always finds its parent. Repeating converged node DDL
/// holds no locks on membership or feature tables; initial migration may need locks.
pub const NODE_DDL: &str = concat!(
    "\
CREATE TABLE IF NOT EXISTS erp_node (
    id             uuid        PRIMARY KEY,
    name           text        NOT NULL CHECK (name ~ '^[A-Za-z0-9._-]{1,63}$'),
    host           text        NOT NULL CHECK (octet_length(host) BETWEEN 1 AND 255
                                               AND host !~ '[\\x01-\\x1f\\x7f-\\x9f]'),
    pid            int4        NOT NULL CHECK (pid > 0),
    role           text        NOT NULL CHECK (role IN ('app', 'worker')),
    dev            boolean     NOT NULL,
    digest         bytea       NOT NULL CHECK (octet_length(digest) = 32),
    config_digest  bytea       NOT NULL CHECK (octet_length(config_digest) = 32),
    build_digest   bytea       NOT NULL CHECK (octet_length(build_digest) = 32),
    addons_digest  bytea       NOT NULL CHECK (octet_length(addons_digest) = 32),
    build_label    text        NOT NULL CHECK (octet_length(build_label) BETWEEN 1 AND 128),
    registered_at  timestamptz NOT NULL,
    state          text        NOT NULL
                   CHECK (state IN ('starting', 'ready', 'unready', 'stopping', 'stopped')),
    unready_reason text        CHECK (unready_reason IS NULL OR octet_length(unready_reason) <= 64),
    heartbeat_at   timestamptz NOT NULL,
    expires_at     timestamptz NOT NULL,
    beat           int8        NOT NULL CHECK (beat > 0),
    interval_ms    int8        NOT NULL CHECK (interval_ms > 0),
    lease_ms       int8        NOT NULL CHECK (lease_ms >= interval_ms),
    CHECK (expires_at >= heartbeat_at),
    CHECK ((state = 'unready') = (unready_reason IS NOT NULL))
) WITH (fillfactor = 50);
CREATE TABLE IF NOT EXISTS erp_node_detail (
    node_id  uuid  NOT NULL REFERENCES erp_node (id) ON DELETE CASCADE,
    kind     text  NOT NULL CHECK (kind IN ('config_sections', 'addons', 'build')),
    payload  bytea NOT NULL CHECK (octet_length(payload) <= 1048576),
    PRIMARY KEY (node_id, kind)
);
CREATE TABLE IF NOT EXISTS erp_node_rollout (
    id          int8        GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    from_digest bytea       NOT NULL CHECK (octet_length(from_digest) = 32),
    to_digest   bytea       NOT NULL CHECK (octet_length(to_digest) = 32),
    opened_by   text        NOT NULL DEFAULT session_user
                            CHECK (octet_length(opened_by) BETWEEN 1 AND 128),
    opened_role text        NOT NULL DEFAULT session_user,
    opened_at   timestamptz NOT NULL DEFAULT now(),
    until       timestamptz NOT NULL,
    closed_at   timestamptz,
    CHECK (from_digest <> to_digest),
    CHECK (until >= opened_at + interval '1 minute'),
    CHECK (until <= opened_at + interval '24 hours')
);
DO $$
BEGIN
    -- an index statement with IF NOT EXISTS would lock the table before it checks the name, for
    -- the whole schema transaction; the name lookup takes no lock
    IF to_regclass('erp_node_rollout_one_open') IS NULL THEN
        CREATE UNIQUE INDEX erp_node_rollout_one_open ON erp_node_rollout ((true))
            WHERE closed_at IS NULL;
    END IF;
END
$$",
    ";\n",
    include_str!("feature/schema.sql")
);

/// The node tables as a core table of `erp-orm` itself (listed in `core_ddl::ORM_DDL`: a
/// `core_ddl!` registration inside this crate would need the `#[link_section]` attribute, which
/// the workspace forbids outside macros of other crates).
pub(crate) const NODE_TABLES: crate::CoreDdl = crate::CoreDdl {
    owner: "erp-orm",
    name: "erp_node",
    purpose: "the node registry",
    tables: &[
        "erp_node",
        "erp_node_detail",
        "erp_node_rollout",
        "erp_node_feature",
        "erp_node_feature_digest",
    ],
    sql: NODE_DDL,
    after_models: None,
};

/// A node's identity: one `uuidv7()` from the database per process start, never configured,
/// never reused. `Display` is the canonical hyphenated form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId([u8; 16]);

impl NodeId {
    /// The id's bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Parse the text form a `uuid` column returns.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 36 || [8, 13, 18, 23].iter().any(|&i| bytes[i] != b'-') {
            return None;
        }
        let hex: Vec<u8> = bytes.iter().copied().filter(|b| *b != b'-').collect();
        if hex.len() != 32 {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
            out[i] = (digest::hex_value(pair[0])? << 4) | digest::hex_value(pair[1])?;
        }
        Some(Self(out))
    }

    /// The uuid version nibble (7 for ids from `uuidv7()`).
    pub fn version(&self) -> u8 {
        self.0[6] >> 4
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({self})")
    }
}

/// What a node does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NodeRole {
    /// Serves HTTP and RPC.
    App,
    /// Runs background work.
    Worker,
}

impl NodeRole {
    /// The column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Worker => "worker",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "app" => Some(Self::App),
            "worker" => Some(Self::Worker),
            _ => None,
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::App => 0,
            Self::Worker => 1,
        }
    }
}

impl fmt::Display for NodeRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `state` column of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NodeState {
    /// Registered, not ready yet (counted in the divisor while fresh).
    Starting,
    /// Ready (counted while fresh; defines the expected digest within one lease).
    Ready,
    /// Not ready because of its digest (not counted).
    Unready,
    /// Draining (not counted).
    Stopping,
    /// Shut down.
    Stopped,
}

impl NodeState {
    /// The column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Unready => "unready",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "starting" => Some(Self::Starting),
            "ready" => Some(Self::Ready),
            "unready" => Some(Self::Unready),
            "stopping" => Some(Self::Stopping),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }

    /// Whether a fresh row in this state counts in [`NodeView::divisor`].
    pub fn is_counted(self) -> bool {
        matches!(self, Self::Starting | Self::Ready)
    }
}

impl fmt::Display for NodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a heartbeat failure keeps readiness until the self-fence deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureClass {
    /// Connection loss, shutdown, serialization failure, deadlock, lock or statement timeout:
    /// readiness is kept until `serve_until`; retried after interval ÷ 4.
    Transient,
    /// Anything else (missing table, permission, check violation, bad data): unready at once;
    /// retried every interval.
    Permanent,
}

/// A failed renewal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatError {
    class: FailureClass,
    message: String,
    sqlstate: Option<String>,
}

impl HeartbeatError {
    pub(crate) fn new(class: FailureClass, message: String, sqlstate: Option<String>) -> Self {
        Self {
            class,
            message,
            sqlstate,
        }
    }

    /// Transient or permanent.
    pub fn class(&self) -> FailureClass {
        self.class
    }

    /// The message (the database's text, or what timed out).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The SQLSTATE when the server sent one.
    pub fn sqlstate(&self) -> Option<&str> {
        self.sqlstate.as_deref()
    }
}

impl fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.sqlstate {
            Some(code) => write!(f, "the renewal failed: {} (SQLSTATE {code})", self.message),
            None => write!(f, "the renewal failed: {}", self.message),
        }
    }
}

/// Where a membership stands.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Readiness {
    /// Registered; readiness not requested yet, or waiting for a renewal.
    Starting,
    /// Waiting out the settle delay (design §7.2 step 3).
    #[non_exhaustive]
    Settling {
        /// Database time left until the delay ends.
        remaining: Duration,
    },
    /// Ready: [`NodeMembership::is_serving`] is true while the lease is fresh.
    Ready,
    /// Not ready, and why.
    Unready(UnreadyReason),
    /// Shut down ([`NodeMembership::shutdown`]): the membership has ended.
    Stopped,
}

impl Readiness {
    /// Whether this is [`Readiness::Ready`].
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Why a membership is not ready.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnreadyReason {
    /// Its digest differs from the ready nodes' (or lies outside the open window).
    DigestMismatch(Mismatch),
    /// Two digests are ready and no window is open.
    TwoVersionsLive(TwoVersions),
    /// No renewal succeeded before the self-fence deadline.
    HeartbeatLost {
        /// The last failure, if one was seen (a starved task has none).
        last_error: Option<HeartbeatError>,
    },
    /// The database refused the renewal.
    HeartbeatRefused {
        /// The failure.
        error: HeartbeatError,
    },
    /// Another process holds this node id (sticky).
    IdentityConflict,
    /// Draining (sticky).
    Draining,
    /// A dev node (`erp dev`) is live in the database: a non-dev node never serves next to one
    /// (re-checked after every renewal).
    DevNodeLive(DevNodeLive),
    /// The heartbeat task has ended (it panicked, or its owner handle was dropped): nothing renews
    /// the row any more (sticky).
    HeartbeatEnded {
        /// Whether it panicked.
        panicked: bool,
    },
}

impl UnreadyReason {
    /// The `unready_reason` column value, for the reasons written to the row.
    pub(crate) fn code(&self) -> Option<&'static str> {
        match self {
            Self::DigestMismatch(_) => Some("digest_mismatch"),
            Self::TwoVersionsLive(_) => Some("two_versions_live"),
            Self::DevNodeLive(_) => Some("dev_node_live"),
            _ => None,
        }
    }
}

impl fmt::Display for UnreadyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DigestMismatch(m) => m.fmt(f),
            Self::TwoVersionsLive(t) => t.fmt(f),
            Self::HeartbeatLost {
                last_error: Some(e),
            } => write!(
                f,
                "no renewal succeeded before the self-fence deadline ({e}); new work is refused \
                 until one does"
            ),
            Self::HeartbeatLost { last_error: None } => f.write_str(
                "no renewal ran before the self-fence deadline (the heartbeat task is starved); \
                 new work is refused until one succeeds",
            ),
            Self::HeartbeatRefused { error } => {
                write!(f, "the database refused this node's heartbeat: {error}")
            }
            Self::IdentityConflict => f.write_str(
                "another process renews this node id: the process was probably cloned; restart it",
            ),
            Self::Draining => f.write_str("this node is draining"),
            Self::DevNodeLive(d) => d.fmt(f),
            Self::HeartbeatEnded { panicked: true } => {
                f.write_str("the heartbeat task panicked; new work is refused: restart the process")
            }
            Self::HeartbeatEnded { panicked: false } => {
                f.write_str("the heartbeat task ended; new work is refused: restart the process")
            }
        }
    }
}

/// How a newcomer's digest differs (design §7.4). No setting value and no secret digest is ever
/// part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    message: String,
    reference: Option<Digest>,
    sections: Vec<String>,
    addons: Vec<String>,
    build_differs: bool,
}

impl Mismatch {
    pub(crate) fn new(message: String) -> Self {
        Self {
            message,
            reference: None,
            sections: Vec::new(),
            addons: Vec::new(),
            build_differs: false,
        }
    }

    pub(crate) fn with_parts(
        mut self,
        reference: Digest,
        sections: Vec<String>,
        addons: Vec<String>,
        build_differs: bool,
    ) -> Self {
        self.reference = Some(reference);
        self.sections = sections;
        self.addons = addons;
        self.build_differs = build_differs;
        self
    }

    /// The whole message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The ready nodes' digest compared with (none for a window refusal).
    pub fn reference(&self) -> Option<Digest> {
        self.reference
    }

    /// Configuration sections that differ.
    pub fn sections(&self) -> &[String] {
        &self.sections
    }

    /// Addons whose version, kind or content differs, or that one side lacks.
    pub fn addons(&self) -> &[String] {
        &self.addons
    }

    /// Whether the build differs.
    pub fn build_differs(&self) -> bool {
        self.build_differs
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Live dev nodes in a database a non-dev node wants to serve (design §17 decision 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevNodeLive {
    message: String,
    nodes: Vec<String>,
}

impl DevNodeLive {
    /// `nodes`: each `name (host h, pid p)`, sorted.
    pub(crate) fn new(database: &str, nodes: Vec<String>) -> Self {
        let message = match nodes.as_slice() {
            [one] => format!(
                "database {database}: dev node {one} is live in this database: a non-dev node \
                 never serves next to an erp dev process; stop it first"
            ),
            many => format!(
                "database {database}: dev nodes {} are live in this database: a non-dev node \
                 never serves next to an erp dev process; stop them first",
                many.join(", ")
            ),
        };
        Self { message, nodes }
    }

    /// The live dev nodes, `name (host h, pid p)`.
    pub fn nodes(&self) -> &[String] {
        &self.nodes
    }
}

impl fmt::Display for DevNodeLive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// A failure of the background work of a membership, which runs again at its next turn: the
/// readiness transition and the details write of a re-inserted row after a renewal
/// ([`NodeView::last_transition_error`]), or the purge of dead rows
/// ([`NodeView::last_purge_error`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundError {
    message: String,
    sqlstate: Option<String>,
    failures: u32,
    failing_since: jiff::Timestamp,
}

impl BackgroundError {
    /// The first failure: `e`, now.
    pub(crate) fn of(e: &NodeError) -> Self {
        Self {
            message: e.to_string(),
            sqlstate: e.sqlstate().map(str::to_owned),
            failures: 1,
            failing_since: jiff::Timestamp::now(),
        }
    }

    /// The failure `e` of work whose last attempt failed with `previous` (`None`: it succeeded):
    /// one more consecutive failure, since the first of them.
    pub(crate) fn after(previous: Option<&Self>, e: &NodeError) -> Self {
        let mut error = Self::of(e);
        if let Some(previous) = previous {
            error.failures = previous.failures.saturating_add(1);
            error.failing_since = previous.failing_since;
        }
        error
    }

    /// The whole message (as the [`NodeError`] it came from).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The SQLSTATE when the server sent one.
    pub fn sqlstate(&self) -> Option<&str> {
        self.sqlstate.as_deref()
    }

    /// How many times in a row this work failed, this failure included.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// When the first of these consecutive failures happened (this host's wall clock).
    pub fn failing_since(&self) -> jiff::Timestamp {
        self.failing_since
    }
}

impl fmt::Display for BackgroundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Two or more digests among the ready nodes and no window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwoVersions {
    database: String,
    groups: Vec<(Digest, u32)>,
}

impl TwoVersions {
    pub(crate) fn new(database: String, mut groups: Vec<(Digest, u32)>) -> Self {
        groups.sort();
        Self { database, groups }
    }

    /// The digests with their ready node counts, sorted by digest.
    pub fn groups(&self) -> &[(Digest, u32)] {
        &self.groups
    }
}

impl fmt::Display for TwoVersions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "database {}: a rollout window is needed or ended while two versions are live: ",
            self.database
        )?;
        for (i, (digest, n)) in self.groups.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{digest} on {n} node{}", if *n == 1 { "" } else { "s" })?;
        }
        Ok(())
    }
}

/// Another fresh node as seen by the last renewal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerNode {
    id: NodeId,
    name: String,
    role: NodeRole,
    state: NodeState,
    digest: Digest,
}

impl PeerNode {
    /// Its id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Its name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its role.
    pub fn role(&self) -> NodeRole {
        self.role
    }

    /// Its state.
    pub fn state(&self) -> NodeState {
        self.state
    }

    /// Its digest.
    pub fn digest(&self) -> Digest {
        self.digest
    }
}

/// The open rollout window as seen by the last renewal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutWindow {
    from: Digest,
    to: Digest,
    until: jiff::Timestamp,
}

impl RolloutWindow {
    /// The version being replaced.
    pub fn from(&self) -> Digest {
        self.from
    }

    /// The version rolled out.
    pub fn to(&self) -> Digest {
        self.to
    }

    /// When it ends (database time).
    pub fn until(&self) -> jiff::Timestamp {
        self.until
    }

    pub(crate) fn allows(&self, d: &Digest) -> bool {
        *d == self.from || *d == self.to
    }
}

/// Fresh peers whose digest differs from a ready node's own (and lies outside any open window):
/// the ready node keeps serving and reports it (design §7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMismatch {
    groups: Vec<(Digest, u32)>,
}

impl PeerMismatch {
    /// The other digests with their fresh node counts, sorted by digest.
    pub fn groups(&self) -> &[(Digest, u32)] {
        &self.groups
    }
}

/// What a membership knows after its last renewal (or transition).
#[derive(Debug, Clone)]
pub struct NodeView {
    database: Arc<str>,
    node: NodeId,
    role: NodeRole,
    db_now: Option<jiff::Timestamp>,
    beat: i64,
    readiness: Readiness,
    /// The row's state is counted (`starting`, `ready`); the node counts itself only until
    /// `counted_until` (its self-fence deadline), checked when the view is read.
    counted: bool,
    counted_until: Option<clock::Tick>,
    live: [u32; 2],
    peers: Arc<[PeerNode]>,
    rollout: Option<RolloutWindow>,
    last_error: Option<HeartbeatError>,
    last_transition_error: Option<BackgroundError>,
    last_purge_error: Option<BackgroundError>,
    last_window_purge_error: Option<BackgroundError>,
    mismatch: Option<PeerMismatch>,
}

impl NodeView {
    /// The database.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// This node.
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// The database time of the last successful renewal or transition.
    pub fn db_now(&self) -> Option<jiff::Timestamp> {
        self.db_now
    }

    /// The beat this node last sent.
    pub fn beat(&self) -> i64 {
        self.beat
    }

    /// The readiness published with this view (see [`NodeMembership::readiness`] for the
    /// deadline-checked value).
    pub fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    /// Fresh rows of `role` in state `starting` or `ready`, this node included when its own row
    /// is one of them and its self-fence deadline has not passed now (checked at every call, so a
    /// view kept past the deadline stops counting the node); at least 1 (design §9).
    pub fn divisor(&self, role: NodeRole) -> NonZeroU32 {
        let counted = self.counted && self.counted_until.is_some_and(|at| clock::now() < at);
        let own = u32::from(counted && self.role == role);
        let n = self.live[role.slot()].saturating_add(own);
        // at least 1 by definition (design §9): 1 + (n - 1), saturating at 0
        NonZeroU32::MIN.saturating_add(n.saturating_sub(1))
    }

    /// Other fresh nodes, any state except `stopped`.
    pub fn fresh_peers(&self) -> &[PeerNode] {
        &self.peers
    }

    /// The open rollout window.
    pub fn rollout(&self) -> Option<&RolloutWindow> {
        self.rollout.as_ref()
    }

    /// The last heartbeat failure (cleared by a successful renewal).
    pub fn last_error(&self) -> Option<&HeartbeatError> {
        self.last_error.as_ref()
    }

    /// The last failure of the readiness transition or of the details write after a renewal
    /// (both are retried after the next renewal); cleared when that work next succeeds. A node
    /// that stays [`Readiness::Starting`] says why here. Every failure, also one of a transition a
    /// ready request ran, is logged where it is counted: a warning, an error from the third in a
    /// row; the success that clears it is logged too.
    pub fn last_transition_error(&self) -> Option<&BackgroundError> {
        self.last_transition_error.as_ref()
    }

    /// The last failure of the purge of dead node rows (design §6.3; it runs again at its next
    /// turn, about every retention ÷ 24); cleared when that step next succeeds. A purge that keeps
    /// failing (a missing privilege, dead nodes' details another transaction holds past the lock
    /// timeout) says so here, with its count of consecutive failures and when they began. The
    /// windows are purged in a step of their own ([`last_window_purge_error`](Self::last_window_purge_error)),
    /// whatever this one does.
    pub fn last_purge_error(&self) -> Option<&BackgroundError> {
        self.last_purge_error.as_ref()
    }

    /// The last turn the ended rollout windows were not purged, with the count of such turns in a
    /// row and when they began: skipped because a window row, or the window table itself (an
    /// `ACCESS EXCLUSIVE` lock during a migration), was held by another transaction past the
    /// purge's lock timeout (SQLSTATE `55P03`), or failed (a statement timeout `57014`, a lost
    /// connection). A skipped turn is logged as information the first time, a warning the second
    /// and an error from the third in a row; a failure is a warning, then an error from the third.
    /// Cleared when a window purge succeeds, which is logged.
    pub fn last_window_purge_error(&self) -> Option<&BackgroundError> {
        self.last_window_purge_error.as_ref()
    }

    /// Fresh peers with another digest, reported while this node is ready.
    pub fn digest_mismatch(&self) -> Option<&PeerMismatch> {
        self.mismatch.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a spec case: background failures are counted in a row, from the first of them, and a
    /// success starts the count again (round-2 review, finding 5).
    #[test]
    fn a_background_error_counts_consecutive_failures_from_the_first() {
        let e = NodeError::Timeout {
            database: "db".to_owned(),
            what: "the purge of dead rows",
            after: Duration::from_secs(1),
        };
        let first = BackgroundError::after(None, &e);
        assert_eq!(first.failures(), 1);
        let second = BackgroundError::after(Some(&first), &e);
        assert_eq!(second.failures(), 2);
        assert_eq!(second.failing_since(), first.failing_since());
        assert_eq!(second.message(), first.message());
        let third = BackgroundError::after(Some(&second), &e);
        assert_eq!(third.failures(), 3);
        assert_eq!(third.failing_since(), first.failing_since());
        // after a success (no previous failure) the count starts again
        assert_eq!(BackgroundError::after(None, &e).failures(), 1);
    }

    #[test]
    fn node_id_round_trips_the_uuid_text_form() {
        let text = "0192f3a1-7b2d-7c4e-9a10-3f2b1c0d9e8a";
        let id = NodeId::parse(text).unwrap();
        assert_eq!(id.to_string(), text);
        assert_eq!(id.version(), 7);
        assert!(NodeId::parse("0192f3a17b2d7c4e9a103f2b1c0d9e8a").is_none());
        assert!(NodeId::parse("0192f3a1-7b2d-7c4e-9a10-3f2b1c0d9e8z").is_none());
    }

    #[test]
    fn ddl_has_the_primary_keys_only_and_fillfactor_50() {
        assert!(NODE_DDL.contains(") WITH (fillfactor = 50);"));
        // no index on any column a renewal changes
        for column in [
            "state",
            "unready_reason",
            "heartbeat_at",
            "expires_at",
            "beat",
        ] {
            assert!(
                !NODE_DDL.contains(&format!("ON erp_node ({column}")),
                "{column} is indexed"
            );
        }
        assert_eq!(NODE_DDL.matches("CREATE INDEX").count(), 0);
        assert_eq!(NODE_DDL.matches("CREATE UNIQUE INDEX").count(), 1);
    }
}
