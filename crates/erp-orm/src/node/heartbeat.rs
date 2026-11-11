//! Registration, the heartbeat task and the readiness transition (design §6, §7, §10).

use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::{Object, Pool};
use tokio::sync::{mpsc, oneshot};
use tokio_postgres::Row;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{FromSql, ToSql, Type};

use super::clock::{self, Tick};
use super::digest::{AddonContent, Dec, Digest};
use super::identity::MembershipClaim;
use super::membership::{RegisterFault, Shared};
use super::sql::{self, Answer, Flight};
use super::{
    BackgroundError, DevNodeLive, FailureClass, HeartbeatError, Mismatch, NodeError, NodeId,
    NodeIdentity, NodeOptions, NodeRole, NodeState, NodeView, PeerMismatch, PeerNode,
    READINESS_LOCK, Readiness, RolloutWindow, TwoVersions, UnreadyReason,
};

/// Below this much time left before `serve_until`, an attempt gets a whole interval instead (the
/// node is fenced anyway; a tiny bound would only make the attempt fail).
const MIN_BOUND: Duration = Duration::from_millis(20);
/// Most addon names a mismatch message lists.
const LISTED_ADDONS: usize = 10;
/// The longest the heartbeat task sleeps at once. Its deadlines are on the membership clock,
/// which counts suspend, while the runtime's timers do not: a deadline that passed during a
/// suspend is acted on within this long after the wake-up (design §6.4).
const MAX_SLEEP: Duration = Duration::from_secs(1);
/// The longest a purge statement waits for a lock another transaction holds (at most a quarter of
/// the purge's bound): the windows then wait for the next turn (a skipped turn, counted), the dead
/// node rows' purge is reported as failed (design §6.3).
const PURGE_LOCK_TIMEOUT: Duration = Duration::from_millis(100);
/// From this many consecutive failures (or skipped turns) on, background work is logged as an
/// error: the transition and the details write after a renewal, and both purge steps.
const BACKGROUND_ERROR_AFTER: u32 = 3;
/// The purge steps, as the logs and the view's messages name them.
const PURGE_NODES: &str = "the purge of dead node rows";
const PURGE_WINDOWS: &str = "the purge of ended rollout windows";
/// The other background work, as the logs name it.
const TRANSITION: &str = "the readiness transition";
const DETAILS: &str = "the details write of a re-inserted row";

/// How loudly a failure of background work is logged, by its count of consecutive failures:
/// a warning, an error from [`BACKGROUND_ERROR_AFTER`] in a row. A skipped turn (the window purge
/// gave way to another transaction's lock) is not a failure the first time: information, then a
/// warning from the second, an error from the third.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loudness {
    Info,
    Warn,
    Error,
}

fn loudness(failures: u32, skipped: bool) -> Loudness {
    if failures >= BACKGROUND_ERROR_AFTER {
        Loudness::Error
    } else if skipped && failures < 2 {
        Loudness::Info
    } else {
        Loudness::Warn
    }
}

/// Log a failure of background work, as loud as its count says ([`loudness`]).
fn log_background(database: &str, work: &'static str, error: &BackgroundError, skipped: bool) {
    let failures = error.failures();
    let since = error.failing_since();
    match loudness(failures, skipped) {
        Loudness::Error => tracing::error!(target: "erp::node", database, work, error = %error,
            failures, %since, "background work keeps failing; it runs again at its next turn"),
        Loudness::Warn => tracing::warn!(target: "erp::node", database, work, error = %error,
            failures, %since, "background work failed; it runs again at its next turn"),
        Loudness::Info => tracing::info!(target: "erp::node", database, work, error = %error,
            "background work gave way to another transaction's lock; it runs again at its next \
             turn"),
    }
}

/// Record the outcome of one turn of background work in `slot` (the view's error for it): a
/// failure counts one more in a row and is logged; a success clears it, logged when it ends a run
/// of failures. `skippable`: a lock timeout (`55P03`) is a skipped turn, not a failure.
fn record_background<T>(
    database: &str,
    work: &'static str,
    slot: &mut Option<BackgroundError>,
    outcome: &Result<T, NodeError>,
    skippable: bool,
) {
    match outcome {
        Ok(_) => {
            if let Some(failed) = slot.take() {
                tracing::info!(target: "erp::node", database, work, failures = failed.failures(),
                    since = %failed.failing_since(), "background work succeeded again");
            }
        }
        Err(e) => {
            let skipped = skippable && e.sqlstate() == Some(SqlState::LOCK_NOT_AVAILABLE.code());
            let error = BackgroundError::after(slot.as_ref(), e);
            log_background(database, work, &error, skipped);
            *slot = Some(error);
        }
    }
}

/// What the handle asks its task.
pub(super) enum Command {
    RequestReady(oneshot::Sender<Result<Readiness, NodeError>>),
    Drain(oneshot::Sender<Result<(), NodeError>>),
    Shutdown(oneshot::Sender<Result<(), NodeError>>),
    #[cfg(feature = "test-faults")]
    RenewNow(oneshot::Sender<Result<NodeView, NodeError>>),
    #[cfg(feature = "test-faults")]
    Snapshot(oneshot::Sender<Result<State, NodeError>>),
    #[cfg(feature = "test-faults")]
    PanicForTests,
}

/// The membership's state, owned by its task.
#[derive(Debug, Clone)]
pub(super) struct State {
    beat: i64,
    registered_at_us: i64,
    /// Database time since which peers count this row (`None`: not counted).
    counted_since_us: Option<i64>,
    /// The expiry the last successful write recorded.
    last_expires_us: i64,
    /// The state the last successful write recorded.
    row_state: NodeState,
    readiness: Readiness,
    want_ready: bool,
    conflict: bool,
    /// The row was re-inserted without its detail rows; written after the next renewal.
    details_missing: bool,
    /// The self-fence deadline on the membership clock.
    serve_until: Option<Tick>,
    /// When the last write the database answered was sent: the durable base of the next
    /// asynchronous renewal's deadline (design §6.4).
    last_sent: Tick,
    failing_since: Option<Tick>,
    last_error: Option<HeartbeatError>,
    /// The last failure of the readiness transition (cleared when one writes its decision).
    transition_error: Option<BackgroundError>,
    /// The last failure of the details write (cleared when one succeeds).
    details_error: Option<BackgroundError>,
    /// The last failure of the purge of dead rows (cleared when one succeeds).
    purge_error: Option<BackgroundError>,
    /// The last turn the windows were not purged: skipped on a held lock, or failed (cleared when
    /// a window purge succeeds).
    window_purge_error: Option<BackgroundError>,
    /// When the next purge is due (design §6.3).
    next_purge: Tick,
    db_now_us: Option<i64>,
    peers: Arc<[PeerNode]>,
    live: [u32; 2],
    rollout: Option<RolloutWindow>,
    mismatch: Option<PeerMismatch>,
}

impl State {
    #[cfg(feature = "test-faults")]
    pub(super) fn serve_until(&self) -> Option<Tick> {
        self.serve_until
    }
}

/// Benchmark seam: the view of a ready node with no peers whose deadline is `serve_until`
/// ([`NodeAdmission::detached_for_benches`](super::NodeAdmission)).
#[cfg(any(feature = "test-faults", feature = "bench-seams"))]
pub(super) fn detached_view(
    database: &Arc<str>,
    identity: &NodeIdentity,
    serve_until: Tick,
) -> NodeView {
    let now = clock::now();
    let st = State {
        beat: 1,
        registered_at_us: 0,
        counted_since_us: Some(0),
        last_expires_us: 0,
        row_state: NodeState::Ready,
        readiness: Readiness::Ready,
        want_ready: true,
        conflict: false,
        details_missing: false,
        serve_until: Some(serve_until),
        last_sent: now,
        failing_since: None,
        last_error: None,
        transition_error: None,
        details_error: None,
        purge_error: None,
        window_purge_error: None,
        next_purge: now,
        db_now_us: None,
        peers: Arc::from([]),
        live: [1, 0],
        rollout: None,
        mismatch: None,
    };
    view_of(database, identity, &st)
}

/// A peer as read by a registration or transition (with its part digests for a mismatch report).
#[derive(Debug, Clone)]
struct PeerRow {
    node: PeerNode,
    fresh: bool,
    dev: bool,
    interval_ms: i64,
    host: String,
    pid: i32,
    config: Digest,
    build: Digest,
    addons: Digest,
    label: String,
}

/// A pooled connection that goes back to the pool only when the work on it finished
/// ([`recycle`](Self::recycle)). Dropped any other way (a client deadline passed, the task was
/// aborted mid-flight) it is discarded: it may still be inside a transaction that holds the
/// readiness lock, and a pool shared with other code must never hand that out (design §6.2).
pub(super) struct Conn(Option<Object>);

impl Conn {
    pub(super) fn new(object: Object) -> Self {
        Self(Some(object))
    }

    /// The work finished (committed or rolled back): back to the pool.
    pub(super) fn recycle(mut self) {
        drop(self.0.take());
    }
}

impl Deref for Conn {
    type Target = Object;

    fn deref(&self) -> &Object {
        match &self.0 {
            Some(object) => object,
            // taken only by `recycle` and `drop`, which consume the guard
            None => unreachable!("a connection guard used after it was released"),
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(object) = self.0.take() {
            drop(Object::take(object));
        }
    }
}

pub(super) fn timestamp(us: i64) -> Option<jiff::Timestamp> {
    jiff::Timestamp::from_microsecond(us).ok()
}

fn view_of(database: &Arc<str>, identity: &NodeIdentity, st: &State) -> NodeView {
    NodeView {
        database: Arc::clone(database),
        node: identity.id(),
        role: identity.role(),
        db_now: st.db_now_us.and_then(timestamp),
        beat: st.beat,
        readiness: st.readiness.clone(),
        // past the self-fence deadline the node does not count itself (its peers may already
        // have counted it out); applied whenever the view is read, not when it is published
        counted: st.row_state.is_counted(),
        counted_until: st.serve_until,
        live: st.live,
        peers: Arc::clone(&st.peers),
        rollout: st.rollout,
        last_error: st.last_error.clone(),
        last_transition_error: st
            .transition_error
            .clone()
            .or_else(|| st.details_error.clone()),
        last_purge_error: st.purge_error.clone(),
        last_window_purge_error: st.window_purge_error.clone(),
        mismatch: st.mismatch.clone(),
    }
}

pub(super) fn ms(d: Duration) -> u64 {
    // the bounds are at most a few minutes
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The self-fence deadline after a write sent at `sent_ns` whose lease the database recorded, in
/// nanoseconds of the membership clock; `durable_ns` is the send of the last write known to be
/// durable (this one after a synchronous commit, the one before it after an asynchronous one).
///
/// at least one interval before this write's own expiry
/// (`sent + lease - interval`), and never past the expiry of the last durable write
/// (`durable + lease`): an asynchronously committed renewal can be lost after its answer
/// (a crash of the primary), and peers then see the expiry of the one before it. The bound holds
/// for one lost renewal on a primary whose commits become durable; it assumes no standby that
/// lags behind acknowledged commits is promoted.
pub(super) fn serve_until_ns(sent_ns: u64, durable_ns: u64, options: &NodeOptions) -> u64 {
    let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    sent_ns
        .saturating_add(ns(options.fence_after()))
        .min(durable_ns.saturating_add(ns(options.lease())))
}

/// The client-side bound of a registry operation bounded by `bound` on the server.
pub(super) fn with_grace(bound: Duration) -> Duration {
    bound + (bound / 4).min(Duration::from_secs(1))
}

/// SQLSTATEs a renewal retries quickly (design §6.3): connection loss (class 08), insufficient
/// resources (class 53: too many connections, out of memory, a configured limit; but not a full
/// disk, which needs an operator), administrator and session ends (57P01–57P05), a cancelled
/// statement (57014), serialization and lock failures, and the registry's own in-transaction
/// timeouts (25P03, 25P04). A later attempt may succeed after any of them.
fn classify_code(code: &str) -> FailureClass {
    let transient = code.starts_with("08")
        || (code.starts_with("53") && code != "53100")
        || matches!(
            code,
            "57P01"
                | "57P02"
                | "57P03"
                | "57P05"
                | "57014"
                | "40001"
                | "40P01"
                | "55P03"
                | "25P03"
                | "25P04"
        );
    if transient {
        FailureClass::Transient
    } else {
        FailureClass::Permanent
    }
}

fn heartbeat_error(e: &tokio_postgres::Error) -> HeartbeatError {
    use std::error::Error as _;
    match e.as_db_error() {
        Some(db) => {
            let code = db.code().code();
            HeartbeatError::new(
                classify_code(code),
                db.message().to_owned(),
                Some(code.to_owned()),
            )
        }
        None => {
            let lost = e.is_closed() || e.source().is_some_and(|s| s.is::<std::io::Error>());
            let class = if lost {
                FailureClass::Transient
            } else {
                FailureClass::Permanent
            };
            HeartbeatError::new(class, e.to_string(), None)
        }
    }
}

fn pool_error(e: &deadpool_postgres::PoolError) -> HeartbeatError {
    match e {
        deadpool_postgres::PoolError::Backend(b) => heartbeat_error(b),
        // nothing reopens a closed pool: no later attempt can succeed
        deadpool_postgres::PoolError::Closed => HeartbeatError::new(
            FailureClass::Permanent,
            "could not get a database connection: the heartbeat pool is closed".to_owned(),
            None,
        ),
        deadpool_postgres::PoolError::NoRuntimeSpecified => HeartbeatError::new(
            FailureClass::Permanent,
            format!("could not get a database connection: {e}"),
            None,
        ),
        // a pool timeout or a failed hook: the database is not reachable through this pool now
        other => HeartbeatError::new(
            FailureClass::Transient,
            format!("could not get a database connection: {other}"),
            None,
        ),
    }
}

/// Roll back an aborted or unwanted transaction; a connection that cannot even do that is
/// discarded instead of going back to the pool.
pub(super) async fn rollback(conn: Conn) {
    if conn.batch_execute("ROLLBACK").await.is_ok() {
        conn.recycle();
    }
}

/// The rows of each request of a flight, and its first failure (the later requests of an
/// aborted transaction only repeat it). A failed request gets no rows.
pub(super) fn split(answers: Vec<Answer>) -> (Vec<Vec<Row>>, Option<tokio_postgres::Error>) {
    let mut first = None;
    let rows = answers
        .into_iter()
        .map(|a| match a {
            Ok(rows) => rows,
            Err(e) => {
                if first.is_none() {
                    first = Some(e);
                }
                Vec::new()
            }
        })
        .collect();
    (rows, first)
}

pub(super) fn rows_at(rows: &[Vec<Row>], i: usize) -> &[Row] {
    rows.get(i).map_or(&[], Vec::as_slice)
}

pub(super) fn get<'r, T: FromSql<'r>>(
    row: &'r Row,
    i: usize,
    database: &str,
    what: &'static str,
) -> Result<T, NodeError> {
    row.try_get(i)
        .map_err(|e| NodeError::undecodable(database, what, e))
}

pub(super) fn opt_digest_col(
    row: &Row,
    i: usize,
    database: &str,
    what: &'static str,
) -> Result<Option<Digest>, NodeError> {
    let bytes: Option<&[u8]> = get(row, i, database, what)?;
    match bytes {
        None => Ok(None),
        Some(b) => Digest::from_slice(b)
            .map(Some)
            .ok_or_else(|| NodeError::undecodable(database, what, "a digest is not 32 bytes")),
    }
}

pub(super) fn digest_col(
    row: &Row,
    i: usize,
    database: &str,
    what: &'static str,
) -> Result<Digest, NodeError> {
    opt_digest_col(row, i, database, what)?
        .ok_or_else(|| NodeError::undecodable(database, what, "a NULL digest"))
}

/// A peer from columns `at..`: id, name, role, state, digest (`None`: a NULL id).
fn peer_at(
    row: &Row,
    at: usize,
    database: &str,
    what: &'static str,
) -> Result<Option<PeerNode>, NodeError> {
    let id: Option<&str> = get(row, at, database, what)?;
    let Some(id) = id else { return Ok(None) };
    let id = NodeId::parse(id).ok_or_else(|| NodeError::undecodable(database, what, id))?;
    let name: String = get(row, at + 1, database, what)?;
    let role: &str = get(row, at + 2, database, what)?;
    let role = NodeRole::parse(role).ok_or_else(|| NodeError::undecodable(database, what, role))?;
    let state: &str = get(row, at + 3, database, what)?;
    let state =
        NodeState::parse(state).ok_or_else(|| NodeError::undecodable(database, what, state))?;
    let digest = digest_col(row, at + 4, database, what)?;
    Ok(Some(PeerNode {
        id,
        name,
        role,
        state,
        digest,
    }))
}

/// The open window from columns `at..`: from, to, until µs.
fn window_at(
    row: &Row,
    at: usize,
    database: &str,
    what: &'static str,
) -> Result<Option<RolloutWindow>, NodeError> {
    let from = opt_digest_col(row, at, database, what)?;
    let to = opt_digest_col(row, at + 1, database, what)?;
    let until: Option<i64> = get(row, at + 2, database, what)?;
    match (from, to, until) {
        (Some(from), Some(to), Some(until)) => Ok(Some(RolloutWindow {
            from,
            to,
            until: timestamp(until)
                .ok_or_else(|| NodeError::undecodable(database, what, "window end out of range"))?,
        })),
        (None, None, None) => Ok(None),
        _ => Err(NodeError::undecodable(
            database,
            what,
            "a partial rollout window",
        )),
    }
}

/// The rows of [`sql::PEERS_SQL`].
fn peer_rows(rows: &[Row], database: &str, what: &'static str) -> Result<Vec<PeerRow>, NodeError> {
    let mut peers = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(node) = peer_at(row, 0, database, what)? else {
            return Err(NodeError::undecodable(
                database,
                what,
                "a peer without an id",
            ));
        };
        peers.push(PeerRow {
            node,
            config: digest_col(row, 5, database, what)?,
            build: digest_col(row, 6, database, what)?,
            addons: digest_col(row, 7, database, what)?,
            label: get(row, 8, database, what)?,
            fresh: get(row, 9, database, what)?,
            dev: get(row, 10, database, what)?,
            interval_ms: get(row, 11, database, what)?,
            host: get(row, 12, database, what)?,
            pid: get(row, 13, database, what)?,
        });
    }
    Ok(peers)
}

/// Fresh peers and the per-role counts of the counted ones among them.
fn fresh_summary<'p>(
    peers: impl Iterator<Item = (&'p PeerNode, bool)>,
) -> (Arc<[PeerNode]>, [u32; 2]) {
    let mut live = [0u32; 2];
    let fresh: Vec<PeerNode> = peers
        .filter(|(_, fresh)| *fresh)
        .map(|(p, _)| {
            if p.state.is_counted() {
                let slot = &mut live[p.role.slot()];
                *slot = slot.saturating_add(1);
            }
            p.clone()
        })
        .collect();
    (fresh.into(), live)
}

/// The database name from the row of [`sql::CHECK_SQL`], or [`NodeError::TableMissing`]. No row
/// (the check itself failed) gives an empty name: the flight's failure is reported next.
pub(super) fn checked_database(
    check: Option<&Row>,
    what: &'static str,
) -> Result<String, NodeError> {
    let Some(check) = check else {
        return Ok(String::new());
    };
    let database: String = get(check, 0, "", what)?;
    for (i, table) in [
        (1, "erp_node"),
        (2, "erp_node_detail"),
        (3, "erp_node_rollout"),
    ] {
        let present: bool = get(check, i, &database, what)?;
        if !present {
            return Err(NodeError::TableMissing { database, table });
        }
    }
    Ok(database)
}

/// How often a renewal also purges dead rows: hourly with the default retention, more often for
/// a shorter one, never more than once per interval.
fn purge_every(options: &NodeOptions) -> Duration {
    (options.retention() / 24)
        .min(Duration::from_secs(3600))
        .max(options.interval())
}

/// What a registration does with this identity's own row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OwnRow {
    /// None: insert one.
    Absent,
    /// `stopped`, or expired (a crash, a lost shutdown): delete it, insert a new one.
    Replace,
    /// Fresh, `starting` at beat 1, and written by an earlier registration of this process whose
    /// commit answer was lost: keep it, start its lease again.
    Adopt,
}

/// Register `identity` in the database of `pool` (design §4, §17 decision 5): one transaction
/// under the readiness lock, in two flights. Returns the membership's claim on the database too
/// (one membership of an identity per process and database).
pub(super) async fn register(
    identity: Arc<NodeIdentity>,
    pool: &Pool,
    fault: Option<RegisterFault>,
) -> Result<(Shared, State, MembershipClaim), NodeError> {
    const WHAT: &str = "the registration";
    let options = *identity.options();
    let bound = options.interval();
    let after = with_grace(bound);
    let deadline = tokio::time::Instant::now() + after;
    let timed_out = |database: &str| NodeError::Timeout {
        database: database.to_owned(),
        what: WHAT,
        after,
    };
    let conn = match tokio::time::timeout_at(deadline, pool.get()).await {
        Ok(Ok(conn)) => Conn::new(conn),
        Ok(Err(e)) => return Err(NodeError::pool("", WHAT, e)),
        Err(_) => return Err(timed_out("")),
    };
    let id_text = identity.id().to_string();
    let lease_ms = options.lease_ms();
    let interval_ms = options.interval_ms();
    let begin = sql::begin_sql(ms(bound), ms(after), false);
    let (class, key) = READINESS_LOCK;
    let lock_params: [(&(dyn ToSql + Sync), Type); 2] = [
        (&class, sql::LOCK_TYPES[0].clone()),
        (&key, sql::LOCK_TYPES[1].clone()),
    ];
    let id_params: [(&(dyn ToSql + Sync), Type); 1] = [(&id_text, sql::ID_TYPES[0].clone())];
    let sent = clock::now();
    let answers = {
        let mut flight = Flight::default();
        flight
            .batch(&conn, &begin)
            .query(&conn, sql::CHECK_SQL, &[])
            .query(&conn, sql::LOCK_SQL, &lock_params)
            .query(&conn, sql::REGISTER_READ_SQL, &id_params)
            .query(&conn, sql::PEERS_SQL, &id_params)
            .query(&conn, sql::WINDOW_SQL, &[]);
        match tokio::time::timeout_at(deadline, flight.send()).await {
            Ok(answers) => answers,
            // the guard discards the connection: it may still be mid-statement
            Err(_) => return Err(timed_out("")),
        }
    };
    let (rows, failure) = split(answers);
    // the database name and the tables first: a missing table makes the later reads fail
    let database = match checked_database(rows_at(&rows, 1).first(), WHAT) {
        Ok(database) => database,
        Err(e) => {
            rollback(conn).await;
            return Err(e);
        }
    };
    if let Some(e) = failure {
        rollback(conn).await;
        return Err(NodeError::pg(&database, WHAT, e));
    }
    // a membership of this identity held in this process: refused before its row is looked at
    let Some(claim) = MembershipClaim::take(&identity, &database) else {
        rollback(conn).await;
        return Err(NodeError::AlreadyRegistered {
            database,
            node: identity.id(),
        });
    };
    let read = (|| -> Result<_, NodeError> {
        let Some(read) = rows_at(&rows, 3).first() else {
            return Err(NodeError::undecodable(
                &database,
                WHAT,
                "no answer to the read",
            ));
        };
        let own_state: Option<String> = get(read, 0, &database, WHAT)?;
        let own_beat: Option<i64> = get(read, 1, &database, WHAT)?;
        let own_fresh: Option<bool> = get(read, 2, &database, WHAT)?;
        let live_non_dev: Vec<String> = get(read, 3, &database, WHAT)?;
        let peers = peer_rows(rows_at(&rows, 4), &database, WHAT)?;
        let window = match rows_at(&rows, 5).first() {
            Some(row) => window_at(row, 1, &database, WHAT)?,
            None => None,
        };
        Ok((own_state, own_beat, own_fresh, live_non_dev, peers, window))
    })();
    let (own_state, own_beat, own_fresh, live_non_dev, peers, window) = match read {
        Ok(read) => read,
        Err(e) => {
            rollback(conn).await;
            return Err(e);
        }
    };
    let own = match (own_state.as_deref(), own_beat, own_fresh) {
        (None, _, _) => OwnRow::Absent,
        (Some(state), _, Some(fresh)) if state == NodeState::Stopped.as_str() || !fresh => {
            OwnRow::Replace
        }
        (Some(state), Some(1), Some(true))
            if state == NodeState::Starting.as_str() && identity.is_pending(&database) =>
        {
            OwnRow::Adopt
        }
        _ => {
            rollback(conn).await;
            return Err(NodeError::AlreadyRegistered {
                database,
                node: identity.id(),
            });
        }
    };
    if identity.is_dev() && !live_non_dev.is_empty() {
        rollback(conn).await;
        return Err(NodeError::DevOnLiveCluster {
            database,
            nodes: live_non_dev,
        });
    }
    #[cfg(feature = "test-faults")]
    let lose_reply = match fault {
        Some(RegisterFault::PauseBetweenFlights(gate)) => {
            // outside the deadline: the process is stopped, its timers too
            drop(gate.await);
            false
        }
        Some(RegisterFault::LoseCommitReply) => true,
        None => false,
    };
    #[cfg(not(feature = "test-faults"))]
    let lose_reply = match fault {
        Some(never) => match never {},
        None => false,
    };
    // the row and its details
    let kinds: Vec<&str> = identity.details().iter().map(|(k, _)| *k).collect();
    let payloads: Vec<&[u8]> = identity
        .details()
        .iter()
        .map(|(_, p)| p.as_slice())
        .collect();
    let digest = identity.digest();
    let config = identity.config().digest();
    let build = identity.build().digest();
    let addons = identity.addons().digest();
    let (name, host, pid, role, dev, label) = (
        identity.name(),
        identity.host(),
        identity.pid_i32(),
        identity.role().as_str(),
        identity.is_dev(),
        identity.build().label(),
    );
    let t = &sql::REGISTER_TYPES;
    let register_params: [(&(dyn ToSql + Sync), Type); 16] = [
        (&id_text, t[0].clone()),
        (&name, t[1].clone()),
        (&host, t[2].clone()),
        (&pid, t[3].clone()),
        (&role, t[4].clone()),
        (&dev, t[5].clone()),
        (&digest.as_bytes().as_slice(), t[6].clone()),
        (&config.as_bytes().as_slice(), t[7].clone()),
        (&build.as_bytes().as_slice(), t[8].clone()),
        (&addons.as_bytes().as_slice(), t[9].clone()),
        (&label, t[10].clone()),
        (&lease_ms, t[11].clone()),
        (&kinds, t[12].clone()),
        (&payloads, t[13].clone()),
        (&interval_ms, t[14].clone()),
        (identity.capabilities(), t[15].clone()),
    ];
    let adopt_params: [(&(dyn ToSql + Sync), Type); 2] = [
        (&id_text, sql::ID_LEASE_TYPES[0].clone()),
        (&lease_ms, sql::ID_LEASE_TYPES[1].clone()),
    ];
    // from here on the outcome may be unknown (a lost commit answer): a retry of this identity
    // may adopt what this attempt committed
    identity.mark_pending(&database);
    let answers = {
        let mut flight = Flight::default();
        match own {
            OwnRow::Adopt => {
                flight.query(&conn, sql::ADOPT_SQL, &adopt_params);
            }
            OwnRow::Replace => {
                flight.query(&conn, sql::DELETE_OWN_SQL, &id_params).query(
                    &conn,
                    sql::REGISTER_SQL,
                    &register_params,
                );
            }
            OwnRow::Absent => {
                flight.query(&conn, sql::REGISTER_SQL, &register_params);
            }
        }
        flight.batch(&conn, "COMMIT");
        match tokio::time::timeout_at(deadline, flight.send()).await {
            Ok(_) if lose_reply => return Err(timed_out(&database)),
            Ok(answers) => answers,
            Err(_) => return Err(timed_out(&database)),
        }
    };
    let (rows, failure) = split(answers);
    if let Some(e) = failure {
        // the server answered with an error: the transaction is rolled back, nothing is
        // committed (a lost connection leaves the outcome unknown, and the mark)
        if e.as_db_error().is_some() {
            identity.clear_pending(&database);
        }
        rollback(conn).await;
        return Err(NodeError::pg(&database, WHAT, e));
    }
    identity.clear_pending(&database);
    conn.recycle();
    let at = usize::from(own == OwnRow::Replace);
    let Some(written) = rows_at(&rows, at).first() else {
        return Err(NodeError::undecodable(
            &database,
            WHAT,
            "the write returned no row",
        ));
    };
    let registered_at_us: i64 = get(written, 0, &database, WHAT)?;
    let db_now_us: i64 = if own == OwnRow::Adopt {
        get(written, 1, &database, WHAT)?
    } else {
        registered_at_us
    };
    if own == OwnRow::Adopt {
        tracing::warn!(target: "erp::node", database = %database, node = %identity.id(),
            "the answer to an earlier registration of this process was lost after it committed: \
             its row is adopted");
    }
    let (fresh, live) = fresh_summary(peers.iter().map(|p| (&p.node, p.fresh)));
    // a synchronous commit: durable when answered
    let serve_until = Tick::from_nanos(serve_until_ns(sent.as_nanos(), sent.as_nanos(), &options));
    let state = State {
        beat: 1,
        registered_at_us,
        counted_since_us: Some(registered_at_us),
        last_expires_us: db_now_us.saturating_add(lease_ms.saturating_mul(1000)),
        row_state: NodeState::Starting,
        readiness: Readiness::Starting,
        want_ready: false,
        conflict: false,
        details_missing: false,
        serve_until: Some(serve_until),
        last_sent: sent,
        failing_since: None,
        last_error: None,
        transition_error: None,
        details_error: None,
        purge_error: None,
        window_purge_error: None,
        // the first scheduled round purges (housekeeping never rides a registration: a purge
        // that waits on a lock must not hold a node back from joining)
        next_purge: sent.saturating_add(options.interval()),
        db_now_us: Some(db_now_us),
        peers: fresh,
        live,
        rollout: window,
        mismatch: None,
    };
    let database: Arc<str> = database.into();
    let view = view_of(&database, &identity, &state);
    let shared = Shared::new(database, Arc::clone(&identity), view);
    shared.set_serve_until(serve_until);
    #[cfg(feature = "test-faults")]
    shared.set_last_sent(sent);
    tracing::info!(
        target: "erp::node",
        database = %shared.database,
        node = %identity.id(),
        name = identity.name(),
        role = %identity.role(),
        digest = %identity.digest(),
        dev = identity.is_dev(),
        "node registered"
    );
    Ok((shared, state, claim))
}

/// What one purge did: each step's outcome on its own (rows deleted, or why not).
struct PurgeOutcome {
    nodes: Result<i64, NodeError>,
    windows: Result<i64, NodeError>,
}

/// Send the purge (design §6.3): two independent transactions, the dead node rows and then the
/// ended windows, each with an asynchronous commit (a lost purge only runs again), half of
/// `bound` as its statement timeout and its own client deadline (that half and its grace), and a
/// `lock_timeout` of `lock`. One connection serves both; a step whose connection is lost or whose
/// rollback fails leaves the next step to take a fresh one. A failure of one step never stops the
/// other.
async fn purge(
    pool: Pool,
    database: Arc<str>,
    id_text: String,
    retention_ms: i64,
    lock: Duration,
    bound: Duration,
) -> PurgeOutcome {
    let step = bound / 2;
    let begin = sql::purge_begin_sql(ms(step), ms(with_grace(step)), ms(lock));
    let mut conn = None;
    let node_params: [(&(dyn ToSql + Sync), Type); 2] = [
        (&id_text, sql::PURGE_NODES_TYPES[0].clone()),
        (&retention_ms, sql::PURGE_NODES_TYPES[1].clone()),
    ];
    let nodes = purge_step(
        &pool,
        &mut conn,
        &database,
        PURGE_NODES,
        &begin,
        sql::PURGE_NODES_SQL,
        &node_params,
        step,
    )
    .await;
    let window_params: [(&(dyn ToSql + Sync), Type); 1] =
        [(&retention_ms, sql::PURGE_WINDOWS_TYPES[0].clone())];
    let windows = purge_step(
        &pool,
        &mut conn,
        &database,
        PURGE_WINDOWS,
        &begin,
        sql::PURGE_WINDOWS_SQL,
        &window_params,
        step,
    )
    .await;
    if let Some(conn) = conn {
        conn.recycle();
    }
    PurgeOutcome { nodes, windows }
}

/// One purge transaction in one flight, within `bound` and its grace on the client: the count in
/// its statement's first column. The connection is taken from `conn` (or the pool) and put back
/// there only when the transaction ended cleanly (committed, or rolled back after a server
/// error); a client timeout discards it (it may still be mid-statement).
#[allow(clippy::too_many_arguments, reason = "one private helper of `purge`")]
async fn purge_step(
    pool: &Pool,
    conn: &mut Option<Conn>,
    db: &str,
    what: &'static str,
    begin: &str,
    statement: &str,
    params: &[(&(dyn ToSql + Sync), Type)],
    bound: Duration,
) -> Result<i64, NodeError> {
    let after = with_grace(bound);
    let deadline = tokio::time::Instant::now() + after;
    let timed_out = || NodeError::Timeout {
        database: db.to_owned(),
        what,
        after,
    };
    let c = match conn.take() {
        Some(c) => c,
        None => match tokio::time::timeout_at(deadline, pool.get()).await {
            Ok(Ok(c)) => Conn::new(c),
            Ok(Err(e)) => return Err(NodeError::pool(db, what, e)),
            Err(_) => return Err(timed_out()),
        },
    };
    let answers = {
        let mut flight = Flight::default();
        flight
            .batch(&c, begin)
            .query(&c, statement, params)
            .batch(&c, "COMMIT");
        match tokio::time::timeout_at(deadline, flight.send()).await {
            Ok(answers) => answers,
            // it may still be mid-statement: the guard discards it
            Err(_) => return Err(timed_out()),
        }
    };
    let (rows, failure) = split(answers);
    if let Some(e) = failure {
        // a clean session for the next step, or none (the next step takes a fresh one)
        if matches!(
            tokio::time::timeout_at(deadline, c.batch_execute("ROLLBACK")).await,
            Ok(Ok(()))
        ) {
            *conn = Some(c);
        }
        return Err(NodeError::pg(db, what, e));
    }
    *conn = Some(c);
    let Some(row) = rows_at(&rows, 1).first() else {
        return Err(NodeError::undecodable(db, what, "no row"));
    };
    get(row, 0, db, what)
}

/// The outcome of one renewal attempt.
enum Renewal {
    Renewed {
        db_now_us: i64,
        inserted: bool,
        window: Option<RolloutWindow>,
        peers: Vec<(PeerNode, bool)>,
    },
    /// The row is ahead of every beat this process sent: another process renews it.
    Conflict,
    Failed(HeartbeatError),
}

enum Flow {
    Continue,
    Next(Tick),
    Stop,
}

/// What a transition decided.
enum Decision {
    Ready {
        since: i64,
    },
    Settling {
        since: i64,
        remaining: Duration,
    },
    Unready(UnreadyReason),
    /// A plain mismatch against one reference digest; described from the detail rows.
    Mismatch {
        reference: Digest,
    },
}

/// Publishes the end of the heartbeat task however it ends (a panic unwinds through it, an abort
/// drops it): admission stops at once, and the view says why.
struct EndGuard(Arc<Shared>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.0.task_ended(std::thread::panicking());
    }
}

/// The heartbeat task of one membership.
pub(super) struct Task {
    shared: Arc<Shared>,
    pool: Pool,
    rx: mpsc::Receiver<Command>,
    st: State,
    /// A command that arrived during housekeeping (which it cut short), handled next.
    pending: Option<Command>,
    /// Purges a command cut short since the task started (for the debug log).
    purge_cuts: u64,
}

impl Task {
    pub(super) fn new(
        shared: Arc<Shared>,
        pool: Pool,
        rx: mpsc::Receiver<Command>,
        st: State,
    ) -> Self {
        Self {
            shared,
            pool,
            rx,
            st,
            pending: None,
            purge_cuts: 0,
        }
    }

    fn db(&self) -> &str {
        &self.shared.database
    }

    fn options(&self) -> NodeOptions {
        *self.shared.identity.options()
    }

    fn view(&self) -> NodeView {
        view_of(&self.shared.database, &self.shared.identity, &self.st)
    }

    fn publish(&self) {
        self.shared.publish(self.view());
    }

    fn set_readiness(&mut self, readiness: Readiness) {
        self.shared.set_serving(readiness.is_ready());
        self.st.readiness = readiness;
    }

    fn conflict_error(&self) -> NodeError {
        NodeError::IdentityConflict {
            database: self.db().to_owned(),
            node: self.shared.identity.id(),
        }
    }

    pub(super) async fn run(mut self) {
        let _end = EndGuard(Arc::clone(&self.shared));
        let mut next = clock::now().saturating_add(self.options().interval());
        #[cfg(feature = "test-faults")]
        let mut paused = self.shared.paused.subscribe();
        loop {
            #[cfg(feature = "test-faults")]
            {
                // a paused task does nothing at all, as a starved runtime would
                while *paused.borrow_and_update() {
                    if paused.changed().await.is_err() {
                        return;
                    }
                }
            }
            // a command that cut a purge short goes first; the fence is checked at the next pass
            // (handling a command never admits work, and every round checks its own deadline)
            if let Some(cmd) = self.pending.take() {
                match self.handle(cmd).await {
                    Flow::Continue => {}
                    Flow::Next(at) => next = at,
                    Flow::Stop => return,
                }
                continue;
            }
            let now = clock::now();
            let fence_at = match (&self.st.readiness, self.st.serve_until) {
                (Readiness::Ready, Some(at)) => Some(at),
                _ => None,
            };
            let renewing = !self.st.conflict;
            let mut wake = MAX_SLEEP;
            if renewing {
                wake = wake.min(next.saturating_since(now));
            }
            if let Some(at) = fence_at {
                wake = wake.min(at.saturating_since(now));
            }
            #[cfg(feature = "test-faults")]
            let pause = paused.changed();
            #[cfg(not(feature = "test-faults"))]
            let pause = std::future::pending::<Result<(), ()>>();
            tokio::select! {
                biased;
                cmd = self.rx.recv() => {
                    let Some(cmd) = cmd else { return };
                    match self.handle(cmd).await {
                        Flow::Continue => {}
                        Flow::Next(at) => next = at,
                        Flow::Stop => return,
                    }
                }
                _ = pause => {}
                () = tokio::time::sleep(wake) => {
                    let now = clock::now();
                    if fence_at.is_some_and(|at| now >= at) {
                        self.fence();
                    }
                    if renewing && now >= next {
                        let (at, outcome) = self.round(true).await;
                        next = at;
                        if outcome.is_ok() {
                            self.housekeeping(next).await;
                        }
                    }
                }
            }
        }
    }

    async fn handle(&mut self, cmd: Command) -> Flow {
        match cmd {
            Command::RequestReady(reply) => {
                self.st.want_ready = !self.st.conflict;
                let outcome = self.transition().await;
                // the receiver is gone when the caller stopped waiting
                drop(reply.send(outcome));
                Flow::Continue
            }
            Command::Drain(reply) => {
                if !self.st.conflict {
                    self.st.want_ready = false;
                    self.set_readiness(Readiness::Unready(UnreadyReason::Draining));
                    self.publish();
                    tracing::info!(target: "erp::node", database = self.db(), "node draining");
                }
                // a synchronous commit: the drain is durable when it returns
                let (next, outcome) = self.round(false).await;
                drop(reply.send(outcome.map(|_| ())));
                Flow::Next(next)
            }
            Command::Shutdown(reply) => {
                let outcome = self.shutdown().await;
                drop(reply.send(outcome));
                Flow::Stop
            }
            #[cfg(feature = "test-faults")]
            Command::RenewNow(reply) => {
                let (next, outcome) = self.round(true).await;
                let renewed = outcome.is_ok();
                // answered before the purge, as a scheduled round's view is published before it
                drop(reply.send(outcome));
                if renewed {
                    self.housekeeping(next).await;
                }
                Flow::Next(next)
            }
            #[cfg(feature = "test-faults")]
            Command::Snapshot(reply) => {
                drop(reply.send(Ok(self.st.clone())));
                Flow::Continue
            }
            #[cfg(feature = "test-faults")]
            #[allow(clippy::panic, reason = "test seam: a heartbeat task that panics")]
            Command::PanicForTests => panic!("the heartbeat task panicked (test seam)"),
        }
    }

    /// The self-fence deadline passed without a renewal: unready, loudly.
    fn fence(&mut self) {
        if !self.st.readiness.is_ready() {
            return;
        }
        let last_error = self.st.last_error.clone();
        tracing::error!(
            target: "erp::node",
            database = self.db(),
            last_error = ?last_error.as_ref().map(ToString::to_string),
            "no renewal succeeded before the self-fence deadline: the node stops admitting work"
        );
        self.set_readiness(Readiness::Unready(UnreadyReason::HeartbeatLost {
            last_error,
        }));
        self.publish();
    }

    /// The row state a renewal writes for the current readiness (`None`: no renewal).
    fn row_state(&self) -> Option<(NodeState, Option<&'static str>)> {
        match &self.st.readiness {
            Readiness::Starting | Readiness::Settling { .. } => Some((NodeState::Starting, None)),
            Readiness::Ready => Some((NodeState::Ready, None)),
            Readiness::Stopped => None,
            Readiness::Unready(reason) => match reason {
                UnreadyReason::DigestMismatch(_)
                | UnreadyReason::TwoVersionsLive(_)
                | UnreadyReason::DevNodeLive(_) => Some((NodeState::Unready, reason.code())),
                // a node that renews again rejoins as a newcomer, counted from that renewal
                UnreadyReason::HeartbeatLost { .. } | UnreadyReason::HeartbeatRefused { .. } => {
                    Some((NodeState::Starting, None))
                }
                UnreadyReason::Draining => Some((NodeState::Stopping, None)),
                UnreadyReason::IdentityConflict | UnreadyReason::HeartbeatEnded { .. } => None,
            },
        }
    }

    fn needs_transition(&self) -> bool {
        matches!(
            self.st.readiness,
            Readiness::Starting
                | Readiness::Settling { .. }
                | Readiness::Unready(
                    UnreadyReason::DigestMismatch(_)
                        | UnreadyReason::TwoVersionsLive(_)
                        | UnreadyReason::DevNodeLive(_)
                )
        )
    }

    /// A synchronous commit after renewals flushed the WAL through its own record, so every
    /// earlier asynchronous renewal is durable now: the last one's deadline no longer needs the
    /// one before it (design §6.4).
    fn durable(&mut self) {
        let sent = self.st.last_sent.as_nanos();
        let at = Tick::from_nanos(serve_until_ns(sent, sent, &self.options()));
        if self.st.serve_until.is_some_and(|current| current < at) {
            self.st.serve_until = Some(at);
            self.shared.set_serve_until(at);
        }
    }

    /// One renewal and what follows it: the details of a re-inserted row, the pending transition,
    /// the peer report; the view is then published and returned. The purge, when due, runs after
    /// it ([`housekeeping`](Self::housekeeping)): neither the view nor the caller waits for it.
    async fn round(&mut self, async_commit: bool) -> (Tick, Result<NodeView, NodeError>) {
        let interval = self.options().interval();
        let idle = clock::now().saturating_add(Duration::from_secs(86_400));
        let Some((state, reason)) = self.row_state() else {
            return (idle, Err(self.conflict_error()));
        };
        let sent = clock::now();
        match self.renew(state, reason, async_commit).await {
            Renewal::Renewed {
                db_now_us,
                inserted,
                window,
                peers,
            } => {
                let durable = if async_commit {
                    self.st.last_sent
                } else {
                    sent
                };
                self.renewed(sent, durable, state, db_now_us, inserted, window, &peers);
                if self.st.details_missing {
                    match self.write_details().await {
                        Ok(()) => {
                            self.st.details_missing = false;
                            record_background(
                                &self.shared.database,
                                DETAILS,
                                &mut self.st.details_error,
                                &Ok::<(), NodeError>(()),
                                false,
                            );
                            self.durable();
                        }
                        Err(e) => {
                            record_background(
                                &self.shared.database,
                                DETAILS,
                                &mut self.st.details_error,
                                &Err::<(), _>(e),
                                false,
                            );
                        }
                    }
                }
                if self.st.want_ready && self.needs_transition() {
                    // `transition` counts and logs a failure where it records it
                    let _retried_at_the_next_renewal = self.transition().await;
                }
                self.check_peer_mismatch();
                let next = sent.saturating_add(interval);
                let view = self.view();
                self.shared.publish(view.clone());
                (next, Ok(view))
            }
            Renewal::Conflict => {
                self.on_conflict();
                (idle, Err(self.conflict_error()))
            }
            Renewal::Failed(error) => {
                let next = self.failed(error.clone());
                let e = NodeError::Heartbeat {
                    database: self.db().to_owned(),
                    error,
                };
                (next, Err(e))
            }
        }
    }

    /// The purge of dead rows and ended windows past the retention (design §4.2, §6.3), when it
    /// is due, after a successful round has published its view and answered its caller: in
    /// transactions of its own, so housekeeping never costs a node its lease nor delays what
    /// readers see. It must end before the next renewal is due at `next`: bounded by half an
    /// interval and by the time left (the client deadline adds a quarter of the bound); a round
    /// with too little time left keeps it due for the next round.
    ///
    /// Its two steps are independent: each one's failure is kept in the view on its own
    /// (`last_purge_error` for the dead node rows, `last_window_purge_error` for the windows),
    /// counted in a row and logged as [`loudness`] says; a window held past the lock timeout is a
    /// skipped turn, counted the same way. The view is published again only when one of them
    /// changed.
    ///
    /// A command that arrives meanwhile cuts the purge short and is handled next; the purge stays
    /// due. Its connection is discarded by the guard: the server ends the abandoned statement by
    /// its statement timeout (half the bound) at the latest, and a purge that still commits is
    /// harmless (it only deletes what a later purge would). Starvation bound: a cut purge runs
    /// again at the next round, so only commands arriving at least once per interval, each
    /// during the purge, could keep it from finishing; commands are the owner's (readiness, drain,
    /// shutdown), never request traffic.
    async fn housekeeping(&mut self, next: Tick) {
        let now = clock::now();
        if now < self.st.next_purge {
            return;
        }
        let options = self.options();
        let left = next.saturating_since(now);
        let bound = (left.saturating_mul(4) / 5).min(options.interval() / 2);
        if bound < MIN_BOUND {
            return;
        }
        self.st.next_purge = now.saturating_add(purge_every(&options));
        #[cfg(feature = "test-faults")]
        let lock = self
            .shared
            .purge_lock_timeout(PURGE_LOCK_TIMEOUT.min(bound / 4));
        #[cfg(not(feature = "test-faults"))]
        let lock = PURGE_LOCK_TIMEOUT.min(bound / 4);
        let job = purge(
            self.pool.clone(),
            Arc::clone(&self.shared.database),
            self.shared.identity.id().to_string(),
            options.retention_ms(),
            lock,
            bound,
        );
        let outcome = tokio::select! {
            biased;
            outcome = job => outcome,
            cmd = self.rx.recv() => {
                // the purge was dropped above (see the doc comment for what the server does)
                self.st.next_purge = now;
                self.purge_cuts = self.purge_cuts.saturating_add(1);
                tracing::debug!(target: "erp::node", database = self.db(), cuts = self.purge_cuts,
                    "a command cut the purge short; it runs again at the next round");
                // `None`: every handle is gone, and the loop ends at its next receive
                self.pending = cmd;
                return;
            }
        };
        let before = (
            self.st.purge_error.clone(),
            self.st.window_purge_error.clone(),
        );
        record_background(
            &self.shared.database,
            PURGE_NODES,
            &mut self.st.purge_error,
            &outcome.nodes,
            false,
        );
        record_background(
            &self.shared.database,
            PURGE_WINDOWS,
            &mut self.st.window_purge_error,
            &outcome.windows,
            true,
        );
        // the counts of the steps that ran (a failed step is logged above)
        let (nodes, windows) = (outcome.nodes.as_ref().ok(), outcome.windows.as_ref().ok());
        if nodes.is_some_and(|n| *n > 0) || windows.is_some_and(|n| *n > 0) {
            tracing::info!(target: "erp::node", database = self.db(), nodes = ?nodes,
                windows = ?windows,
                "purged dead node rows and ended rollout windows past the retention");
        }
        if (&self.st.purge_error, &self.st.window_purge_error) != (&before.0, &before.1) {
            self.publish();
        }
    }

    /// Send one renewal (design §6.2): one flight, bounded by the smaller of the interval and the
    /// time left before `serve_until`.
    async fn renew(
        &mut self,
        state: NodeState,
        reason: Option<&'static str>,
        async_commit: bool,
    ) -> Renewal {
        let identity = Arc::clone(&self.shared.identity);
        let options = *identity.options();
        let interval = options.interval();
        let now = clock::now();
        let bound = match self.st.serve_until {
            Some(at) if at > now.saturating_add(MIN_BOUND) => {
                interval.min(at.saturating_since(now))
            }
            _ => interval,
        };
        let after = with_grace(bound);
        let deadline = tokio::time::Instant::now() + after;
        let Some(next_beat) = self.st.beat.checked_add(1) else {
            return Renewal::Failed(HeartbeatError::new(
                FailureClass::Permanent,
                "the beat counter is exhausted".to_owned(),
                None,
            ));
        };
        let conn = match tokio::time::timeout_at(deadline, self.pool.get()).await {
            Ok(Ok(conn)) => Conn::new(conn),
            Ok(Err(e)) => return Renewal::Failed(pool_error(&e)),
            Err(_) => {
                return Renewal::Failed(HeartbeatError::new(
                    FailureClass::Transient,
                    format!("no connection within {} ms", ms(after)),
                    None,
                ));
            }
        };
        let id_text = identity.id().to_string();
        let (name, host, pid, role, dev, label) = (
            identity.name(),
            identity.host(),
            identity.pid_i32(),
            identity.role().as_str(),
            identity.is_dev(),
            identity.build().label(),
        );
        let digest = identity.digest();
        let config = identity.config().digest();
        let build = identity.build().digest();
        let addons = identity.addons().digest();
        let state_text = state.as_str();
        let lease_ms = options.lease_ms();
        let interval_ms = options.interval_ms();
        let beat = self.st.beat;
        let registered = self.st.registered_at_us;
        let t = &sql::RENEW_TYPES;
        let params: [(&(dyn ToSql + Sync), Type); 18] = [
            (&id_text, t[0].clone()),
            (&name, t[1].clone()),
            (&host, t[2].clone()),
            (&pid, t[3].clone()),
            (&role, t[4].clone()),
            (&dev, t[5].clone()),
            (&digest.as_bytes().as_slice(), t[6].clone()),
            (&config.as_bytes().as_slice(), t[7].clone()),
            (&build.as_bytes().as_slice(), t[8].clone()),
            (&addons.as_bytes().as_slice(), t[9].clone()),
            (&label, t[10].clone()),
            (&registered, t[11].clone()),
            (&state_text, t[12].clone()),
            (&reason, t[13].clone()),
            (&lease_ms, t[14].clone()),
            (&beat, t[15].clone()),
            (&interval_ms, t[16].clone()),
            (identity.capabilities(), t[17].clone()),
        ];
        let begin = sql::begin_sql(ms(bound), ms(after), async_commit);
        let sent = {
            let mut flight = Flight::default();
            flight
                .batch(&conn, &begin)
                .query(&conn, sql::RENEW_SQL, &params)
                .batch(&conn, "COMMIT");
            // from here on the row may hold the next beat whatever the outcome: the next attempt
            // fences against it (a row ahead of every beat this process sent is another's)
            self.st.beat = next_beat;
            tokio::time::timeout_at(deadline, flight.send()).await
        };
        let Ok(answers) = sent else {
            // it may still be mid-statement: the guard discards it
            return Renewal::Failed(HeartbeatError::new(
                FailureClass::Transient,
                format!("the renewal did not finish within {} ms", ms(after)),
                None,
            ));
        };
        let (rows, failure) = split(answers);
        if let Some(e) = failure {
            let error = heartbeat_error(&e);
            rollback(conn).await;
            return Renewal::Failed(error);
        }
        conn.recycle();
        match self.parse_renewal(rows_at(&rows, 1)) {
            Ok(r) => r,
            Err(e) => Renewal::Failed(HeartbeatError::new(
                FailureClass::Permanent,
                e.to_string(),
                None,
            )),
        }
    }

    fn parse_renewal(&self, rows: &[Row]) -> Result<Renewal, NodeError> {
        const WHAT: &str = "the renewal";
        let db = self.db();
        let Some(first) = rows.first() else {
            return Err(NodeError::undecodable(db, WHAT, "no row"));
        };
        let db_now_us: i64 = get(first, 0, db, WHAT)?;
        let inserted: Option<bool> = get(first, 1, db, WHAT)?;
        let Some(inserted) = inserted else {
            return Ok(Renewal::Conflict);
        };
        let window = window_at(first, 2, db, WHAT)?;
        let mut peers = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(peer) = peer_at(row, 5, db, WHAT)? {
                let fresh: bool = get(row, 10, db, WHAT)?;
                peers.push((peer, fresh));
            }
        }
        Ok(Renewal::Renewed {
            db_now_us,
            inserted,
            window,
            peers,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one call site; the values of one renewal"
    )]
    fn renewed(
        &mut self,
        sent: Tick,
        durable: Tick,
        written: NodeState,
        db_now_us: i64,
        inserted: bool,
        window: Option<RolloutWindow>,
        peers: &[(PeerNode, bool)],
    ) {
        let options = self.options();
        // the row had expired by the expiry this node last wrote: peers may have counted it out
        let rejoined = db_now_us >= self.st.last_expires_us;
        self.st.last_expires_us = db_now_us.saturating_add(options.lease_ms().saturating_mul(1000));
        let serve_until = Tick::from_nanos(serve_until_ns(
            sent.as_nanos(),
            durable.as_nanos(),
            &options,
        ));
        self.st.serve_until = Some(serve_until);
        self.shared.set_serve_until(serve_until);
        self.st.last_sent = sent;
        #[cfg(feature = "test-faults")]
        self.shared.set_last_sent(sent);
        self.st.row_state = written;
        if self.st.failing_since.take().is_some() {
            tracing::info!(target: "erp::node", database = self.db(), "node heartbeat restored");
        }
        self.st.last_error = None;
        self.st.db_now_us = Some(db_now_us);
        let (fresh, live) = fresh_summary(peers.iter().map(|(p, f)| (p, *f)));
        self.st.peers = fresh;
        self.st.live = live;
        self.st.rollout = window;
        if inserted {
            self.st.details_missing = true;
        }
        if inserted && !rejoined {
            tracing::warn!(target: "erp::node", database = self.db(),
                "this node's row was missing (lost in a failover, or deleted) and was re-inserted");
        }
        match &self.st.readiness {
            Readiness::Unready(
                UnreadyReason::HeartbeatLost { .. } | UnreadyReason::HeartbeatRefused { .. },
            ) => {
                if rejoined || self.st.counted_since_us.is_none() {
                    self.st.counted_since_us = Some(db_now_us);
                }
                self.set_readiness(Readiness::Starting);
            }
            Readiness::Ready if rejoined => {
                tracing::warn!(target: "erp::node", database = self.db(),
                    "this node's lease had expired; it rejoins through the readiness transition");
                self.st.counted_since_us = Some(db_now_us);
                self.set_readiness(Readiness::Starting);
            }
            Readiness::Starting | Readiness::Settling { .. } if rejoined => {
                self.st.counted_since_us = Some(db_now_us);
            }
            _ => {}
        }
        if !written.is_counted() {
            self.st.counted_since_us = None;
        }
    }

    /// Write the detail rows of a row the renewal re-inserted (a failover lost it, or retention
    /// purged it while this process was paused): a transaction of its own after the renewal, so
    /// a renewal never carries the payloads.
    async fn write_details(&self) -> Result<(), NodeError> {
        const WHAT: &str = "writing the node details";
        let db = self.db();
        let identity = &self.shared.identity;
        let bound = self.options().interval();
        let after = with_grace(bound);
        let id_text = identity.id().to_string();
        let kinds: Vec<&str> = identity.details().iter().map(|(k, _)| *k).collect();
        let payloads: Vec<&[u8]> = identity
            .details()
            .iter()
            .map(|(_, p)| p.as_slice())
            .collect();
        let t = &sql::DETAIL_INSERT_TYPES;
        let params: [(&(dyn ToSql + Sync), Type); 3] = [
            (&id_text, t[0].clone()),
            (&kinds, t[1].clone()),
            (&payloads, t[2].clone()),
        ];
        let deadline = tokio::time::Instant::now() + after;
        let timed_out = || NodeError::Timeout {
            database: db.to_owned(),
            what: WHAT,
            after,
        };
        let conn = match tokio::time::timeout_at(deadline, self.pool.get()).await {
            Ok(Ok(conn)) => Conn::new(conn),
            Ok(Err(e)) => return Err(NodeError::pool(db, WHAT, e)),
            Err(_) => return Err(timed_out()),
        };
        let begin = sql::begin_sql(ms(bound), ms(after), false);
        let answers = {
            let mut flight = Flight::default();
            flight
                .batch(&conn, &begin)
                .query(&conn, sql::DETAIL_INSERT_SQL, &params)
                .batch(&conn, "COMMIT");
            match tokio::time::timeout_at(deadline, flight.send()).await {
                Ok(answers) => answers,
                Err(_) => return Err(timed_out()),
            }
        };
        let (_, failure) = split(answers);
        if let Some(e) = failure {
            rollback(conn).await;
            return Err(NodeError::pg(db, WHAT, e));
        }
        conn.recycle();
        Ok(())
    }

    fn failed(&mut self, error: HeartbeatError) -> Tick {
        let now = clock::now();
        let interval = self.options().interval();
        let since = *self.st.failing_since.get_or_insert(now);
        if now.saturating_since(since) > interval || error.class() == FailureClass::Permanent {
            tracing::error!(target: "erp::node", database = self.db(), error = %error,
                class = ?error.class(), "node heartbeat failed");
        } else {
            tracing::warn!(target: "erp::node", database = self.db(), error = %error,
                class = ?error.class(), "node heartbeat failed; retrying");
        }
        self.st.last_error = Some(error.clone());
        let next = match error.class() {
            FailureClass::Permanent => {
                if !matches!(
                    self.st.readiness,
                    Readiness::Unready(UnreadyReason::Draining | UnreadyReason::IdentityConflict)
                ) {
                    self.set_readiness(Readiness::Unready(UnreadyReason::HeartbeatRefused {
                        error,
                    }));
                }
                now.saturating_add(interval)
            }
            FailureClass::Transient => now.saturating_add(interval / 4),
        };
        // a deadline that passed during the attempt applies now
        if self.st.readiness.is_ready() && self.shared.deadline_passed() {
            self.fence();
        }
        self.publish();
        next
    }

    fn on_conflict(&mut self) {
        tracing::error!(target: "erp::node", database = self.db(),
            node = %self.shared.identity.id(),
            "another process renews this node id: the process was probably cloned; it must restart");
        self.st.conflict = true;
        self.st.want_ready = false;
        self.set_readiness(Readiness::Unready(UnreadyReason::IdentityConflict));
        self.publish();
    }

    /// A ready node reports fresh peers with another digest (design §7.3), once per change.
    fn check_peer_mismatch(&mut self) {
        let own = self.shared.identity.digest();
        let mismatch = if self.st.readiness.is_ready() {
            let mut groups: BTreeMap<Digest, u32> = BTreeMap::new();
            for p in self.st.peers.iter() {
                let allowed = p.digest == own
                    || self
                        .st
                        .rollout
                        .is_some_and(|w| w.allows(&own) && w.allows(&p.digest));
                if !allowed {
                    let n = groups.entry(p.digest).or_insert(0);
                    *n = n.saturating_add(1);
                }
            }
            (!groups.is_empty()).then(|| PeerMismatch {
                groups: groups.into_iter().collect(),
            })
        } else {
            None
        };
        if mismatch != self.st.mismatch {
            match &mismatch {
                Some(m) => tracing::error!(target: "erp::node", database = self.db(),
                    peers = ?m.groups(),
                    "fresh nodes with another digest serve this database; this node keeps serving"),
                None => tracing::info!(target: "erp::node", database = self.db(),
                    "no fresh node with another digest any more"),
            }
            self.st.mismatch = mismatch;
        }
    }

    /// The readiness transition, from whichever path runs it (a command or the round after a
    /// renewal): a failure is counted in the view and logged here, as loud as its count says, and
    /// kept until a transition writes its decision, which logs the recovery
    /// ([`record_background`]).
    async fn transition(&mut self) -> Result<Readiness, NodeError> {
        let outcome = self.run_transition().await;
        let failing = self.st.transition_error.is_some();
        // a transition that had nothing to write leaves an earlier failure in place
        if !matches!(outcome, Ok((_, false))) {
            record_background(
                &self.shared.database,
                TRANSITION,
                &mut self.st.transition_error,
                &outcome,
                false,
            );
        }
        if outcome.is_err() || (failing && self.st.transition_error.is_none()) {
            self.publish();
        }
        outcome.map(|(readiness, _)| readiness)
    }

    /// The readiness transition (design §7.2): one transaction under the readiness lock. Returns
    /// the readiness and whether a decision was written.
    async fn run_transition(&mut self) -> Result<(Readiness, bool), NodeError> {
        const WHAT: &str = "the readiness transition";
        if self.st.conflict {
            return Err(self.conflict_error());
        }
        if !self.needs_transition() {
            return Ok((self.st.readiness.clone(), false));
        }
        // a lapsed lease is renewed first; the transition follows that renewal
        if self.st.serve_until.is_none_or(|at| at <= clock::now()) {
            return Ok((self.st.readiness.clone(), false));
        }
        let options = self.options();
        let db = self.db().to_owned();
        let bound = options.interval();
        let after = with_grace(bound);
        let deadline = tokio::time::Instant::now() + after;
        let timed_out = || NodeError::Timeout {
            database: db.clone(),
            what: WHAT,
            after,
        };
        let conn = match tokio::time::timeout_at(deadline, self.pool.get()).await {
            Ok(Ok(conn)) => Conn::new(conn),
            Ok(Err(e)) => return Err(NodeError::pool(&db, WHAT, e)),
            Err(_) => return Err(timed_out()),
        };
        let id_text = self.shared.identity.id().to_string();
        let (class, key) = READINESS_LOCK;
        let lock_params: [(&(dyn ToSql + Sync), Type); 2] = [
            (&class, sql::LOCK_TYPES[0].clone()),
            (&key, sql::LOCK_TYPES[1].clone()),
        ];
        let id_params: [(&(dyn ToSql + Sync), Type); 1] = [(&id_text, sql::ID_TYPES[0].clone())];
        let begin = sql::begin_sql(ms(bound), ms(after), false);
        let answers = {
            let mut flight = Flight::default();
            flight
                .batch(&conn, &begin)
                .query(&conn, sql::LOCK_SQL, &lock_params)
                .query(&conn, sql::OWN_SQL, &id_params)
                .query(&conn, sql::PEERS_SQL, &id_params)
                .query(&conn, sql::WINDOW_SQL, &[]);
            match tokio::time::timeout_at(deadline, flight.send()).await {
                Ok(answers) => answers,
                Err(_) => return Err(timed_out()),
            }
        };
        let (rows, failure) = split(answers);
        if let Some(e) = failure {
            rollback(conn).await;
            return Err(NodeError::pg(&db, WHAT, e));
        }
        let read = (|| -> Result<_, NodeError> {
            let Some(win) = rows_at(&rows, 4).first() else {
                return Err(NodeError::undecodable(
                    &db,
                    WHAT,
                    "no answer to the window read",
                ));
            };
            let db_now_us: i64 = get(win, 0, &db, WHAT)?;
            let window = window_at(win, 1, &db, WHAT)?;
            let peers = peer_rows(rows_at(&rows, 3), &db, WHAT)?;
            let own = match rows_at(&rows, 2).first() {
                Some(own) => {
                    let beat: i64 = get(own, 0, &db, WHAT)?;
                    let state: &str = get(own, 1, &db, WHAT)?;
                    let state = NodeState::parse(state)
                        .ok_or_else(|| NodeError::undecodable(&db, WHAT, state))?;
                    let expires_us: i64 = get(own, 2, &db, WHAT)?;
                    Some((beat, state, expires_us))
                }
                None => None,
            };
            Ok((db_now_us, window, peers, own))
        })();
        let (db_now_us, window, peers, own) = match read {
            Ok(read) => read,
            Err(e) => {
                rollback(conn).await;
                return Err(e);
            }
        };
        let Some((own_beat, own_state, own_expires_us)) = own else {
            // the row is gone (retention, or lost in a failover): the next renewal re-inserts it
            rollback(conn).await;
            return Ok((self.st.readiness.clone(), false));
        };
        if own_beat > self.st.beat {
            rollback(conn).await;
            self.on_conflict();
            return Err(self.conflict_error());
        }
        let decided = self.decide(
            &peers,
            window.as_ref(),
            db_now_us,
            own_state,
            own_expires_us,
        );
        let (readiness, state, counted_since) = match decided {
            Decision::Ready { since } => (Readiness::Ready, NodeState::Ready, Some(since)),
            Decision::Settling { since, remaining } => (
                Readiness::Settling { remaining },
                NodeState::Starting,
                Some(since),
            ),
            Decision::Unready(reason) => (Readiness::Unready(reason), NodeState::Unready, None),
            Decision::Mismatch { reference } => {
                match self.describe_mismatch(&conn, &peers, reference).await {
                    Ok(m) => (
                        Readiness::Unready(UnreadyReason::DigestMismatch(m)),
                        NodeState::Unready,
                        None,
                    ),
                    Err(e) => {
                        rollback(conn).await;
                        return Err(e);
                    }
                }
            }
        };
        let reason_code = match &readiness {
            Readiness::Unready(r) => r.code(),
            _ => None,
        };
        let state_text = state.as_str();
        let beat = self.st.beat;
        let t = &sql::TRANSITION_WRITE_TYPES;
        let write_params: [(&(dyn ToSql + Sync), Type); 4] = [
            (&id_text, t[0].clone()),
            (&beat, t[1].clone()),
            (&state_text, t[2].clone()),
            (&reason_code, t[3].clone()),
        ];
        let answers = {
            let mut flight = Flight::default();
            flight
                .query(&conn, sql::TRANSITION_WRITE_SQL, &write_params)
                .batch(&conn, "COMMIT");
            match tokio::time::timeout_at(deadline, flight.send()).await {
                Ok(answers) => answers,
                Err(_) => return Err(timed_out()),
            }
        };
        let (rows, failure) = split(answers);
        if let Some(e) = failure {
            rollback(conn).await;
            return Err(NodeError::pg(&db, WHAT, e));
        }
        conn.recycle();
        if rows_at(&rows, 0).is_empty() {
            // another process renewed between the read and the write
            self.on_conflict();
            return Err(self.conflict_error());
        }
        self.durable();
        // what the read saw also refreshes the view
        let (fresh, live) = fresh_summary(peers.iter().map(|p| (&p.node, p.fresh)));
        self.st.peers = fresh;
        self.st.live = live;
        self.st.rollout = window;
        self.st.db_now_us = Some(db_now_us);
        self.st.row_state = state;
        self.st.counted_since_us = counted_since;
        let changed = self.st.readiness != readiness;
        self.set_readiness(readiness.clone());
        if changed {
            match &readiness {
                Readiness::Ready => {
                    tracing::info!(target: "erp::node", database = %db, "node ready");
                }
                Readiness::Settling { remaining } => tracing::info!(target: "erp::node",
                    database = %db, remaining_ms = ms(*remaining), "node settling"),
                Readiness::Unready(reason) => tracing::error!(target: "erp::node",
                    database = %db, reason = %reason, "node stays out of readiness"),
                Readiness::Starting | Readiness::Stopped => {}
            }
        }
        self.check_peer_mismatch();
        self.publish();
        Ok((readiness, true))
    }

    /// Identity was checked; now the dev nodes, the digest, then the settle delay (design §7.2).
    fn decide(
        &self,
        peers: &[PeerRow],
        window: Option<&RolloutWindow>,
        db_now_us: i64,
        own_state: NodeState,
        own_expires_us: i64,
    ) -> Decision {
        let db = self.db();
        let identity = &self.shared.identity;
        let own = identity.digest();
        // a non-dev node never serves next to a live dev node (design §17 decision 5), and a dev
        // row never defines the digest non-dev nodes are expected to run
        if !identity.is_dev() {
            let mut live_dev: Vec<String> = peers
                .iter()
                .filter(|p| p.dev && p.fresh)
                .map(|p| format!("{} (host {}, pid {})", p.node.name, p.host, p.pid))
                .collect();
            if !live_dev.is_empty() {
                live_dev.sort();
                return Decision::Unready(UnreadyReason::DevNodeLive(DevNodeLive::new(
                    db, live_dev,
                )));
            }
        }
        // the reference set: ready peers seen within their last lease (the read's filter)
        let mut reference: BTreeMap<Digest, u32> = BTreeMap::new();
        for p in peers.iter().filter(|p| is_reference(identity, p)) {
            let n = reference.entry(p.node.digest).or_insert(0);
            *n = n.saturating_add(1);
        }
        match window {
            Some(w) if !w.allows(&own) => {
                return Decision::Unready(UnreadyReason::DigestMismatch(Mismatch::new(format!(
                    "database {db}: this node's digest {own} is outside the open rollout window \
                     from {} to {}",
                    w.from, w.to
                ))));
            }
            Some(w) => {
                if let Some(outside) = reference.keys().find(|d| !w.allows(d)) {
                    return Decision::Unready(UnreadyReason::DigestMismatch(Mismatch::new(
                        format!(
                            "database {db}: the ready nodes run {outside}, which is outside the \
                             open rollout window from {} to {}",
                            w.from, w.to
                        ),
                    )));
                }
            }
            None if reference.len() >= 2 => {
                return Decision::Unready(UnreadyReason::TwoVersionsLive(TwoVersions::new(
                    db.to_owned(),
                    reference.into_iter().collect(),
                )));
            }
            None => {
                if let Some(other) = reference.keys().find(|d| **d != own) {
                    return Decision::Mismatch { reference: *other };
                }
            }
        }
        let counted = own_state.is_counted() && own_expires_us > db_now_us;
        let since = match (counted, self.st.counted_since_us) {
            (true, Some(since)) => since,
            // not counted yet: counted from this write on
            _ => db_now_us,
        };
        // the slowest fresh peer renews (and so counts this node) within its own interval: the
        // settle delay is the largest interval among this node and its fresh peers
        let settle_ms = peers
            .iter()
            .filter(|p| p.fresh)
            .map(|p| p.interval_ms)
            .fold(self.options().interval_ms(), i64::max);
        let settle_end = since.saturating_add(settle_ms.saturating_mul(1000));
        let fresh_peer = peers.iter().any(|p| p.fresh);
        if fresh_peer && db_now_us < settle_end {
            let left = u64::try_from(settle_end.saturating_sub(db_now_us)).unwrap_or(0);
            Decision::Settling {
                since,
                remaining: Duration::from_micros(left),
            }
        } else {
            Decision::Ready { since }
        }
    }

    /// Name what differs from the reference peers (design §7.4), from one reference peer's
    /// detail rows. No setting value and no secret digest is part of the message.
    async fn describe_mismatch(
        &self,
        conn: &Object,
        peers: &[PeerRow],
        reference: Digest,
    ) -> Result<Mismatch, NodeError> {
        const WHAT: &str = "the readiness transition";
        let db = self.db();
        let identity = &self.shared.identity;
        let own = identity.digest();
        let group: Vec<&PeerRow> = peers
            .iter()
            .filter(|p| is_reference(identity, p) && p.node.digest == reference)
            .collect();
        let Some(sample) = group.first() else {
            return Err(NodeError::undecodable(db, WHAT, "no reference peer"));
        };
        let ids = vec![sample.node.id.to_string()];
        let params: [(&(dyn ToSql + Sync), Type); 1] = [(&ids, sql::DETAIL_TYPES[0].clone())];
        let rows = conn
            .query_typed(sql::DETAIL_SQL, &params)
            .await
            .map_err(|e| NodeError::pg(db, WHAT, e))?;
        let mut config_payload = None;
        let mut addons_payload = None;
        for row in &rows {
            let kind: &str = get(row, 1, db, WHAT)?;
            match kind {
                "config_sections" => config_payload = Some(get::<Vec<u8>>(row, 2, db, WHAT)?),
                "addons" => addons_payload = Some(get::<Vec<u8>>(row, 2, db, WHAT)?),
                // the build is reported by its label, never compared field by field
                _ => {}
            }
        }
        let config_differs = sample.config != identity.config().digest();
        let build_differs = sample.build != identity.build().digest();
        let addons_differs = sample.addons != identity.addons().digest();
        let sections = config_payload
            .as_deref()
            .and_then(decode_sections)
            .map(|theirs| differing_sections(identity, &theirs));
        let addons = addons_payload
            .as_deref()
            .and_then(decode_addons)
            .map(|theirs| differing_addons(identity, &theirs));
        let config_text = match (&sections, config_differs) {
            (_, false) => "configuration matches".to_owned(),
            (Some(names), true) if !names.is_empty() => {
                format!("configuration differs (sections: {})", names.join(", "))
            }
            (_, true) => {
                "configuration differs (the ready node's section list is missing or unreadable)"
                    .to_owned()
            }
        };
        let build_text = if build_differs {
            format!(
                "build differs ({} here, {} there)",
                identity.build().label(),
                sample.label
            )
        } else {
            "build matches".to_owned()
        };
        let addons_text = match (&addons, addons_differs) {
            (_, false) => "addons match".to_owned(),
            (Some(names), true) if !names.is_empty() => {
                let listed: Vec<&str> = names
                    .iter()
                    .take(LISTED_ADDONS)
                    .map(String::as_str)
                    .collect();
                let more = names.len().saturating_sub(LISTED_ADDONS);
                let tail = if more > 0 {
                    format!(", and {more} more")
                } else {
                    String::new()
                };
                format!("addons differ ({}{tail})", listed.join(", "))
            }
            (_, true) => {
                "addons differ (the ready node's addon list is missing or unreadable)".to_owned()
            }
        };
        let n = group.len();
        let message = format!(
            "database {db}: this node (digest {own}) differs from the {n} ready node{} with \
             digest {reference}: {config_text}; {build_text}; {addons_text}. Ship the same \
             configuration, build and addons, or open a rollout window: erp cluster rollout open \
             -d {db} --to {own}",
            if n == 1 { "" } else { "s" }
        );
        let sections = if config_differs {
            sections.unwrap_or_default()
        } else {
            Vec::new()
        };
        let addons = if addons_differs {
            addons.unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(Mismatch::new(message).with_parts(reference, sections, addons, build_differs))
    }

    /// Leave (design §6.4): the row written `stopped`, then the membership ends whatever the
    /// write's outcome (a row that was not written expires after its lease).
    async fn shutdown(&mut self) -> Result<(), NodeError> {
        let outcome = self.write_stopped().await;
        self.st.want_ready = false;
        self.st.row_state = NodeState::Stopped;
        self.set_readiness(Readiness::Stopped);
        self.st.live = [0; 2];
        self.publish();
        match &outcome {
            Ok(()) => tracing::info!(target: "erp::node", database = self.db(), "node shut down"),
            Err(e) => tracing::error!(target: "erp::node", database = self.db(), error = %e,
                "the node stopped without writing its row; the row expires after its lease"),
        }
        outcome
    }

    async fn write_stopped(&mut self) -> Result<(), NodeError> {
        const WHAT: &str = "the shutdown";
        self.shared.set_serving(false);
        if self.st.conflict {
            return Err(self.conflict_error());
        }
        let db = self.db().to_owned();
        let bound = self.options().interval();
        let after = with_grace(bound);
        let deadline = tokio::time::Instant::now() + after;
        let timed_out = || NodeError::Timeout {
            database: db.clone(),
            what: WHAT,
            after,
        };
        let conn = match tokio::time::timeout_at(deadline, self.pool.get()).await {
            Ok(Ok(conn)) => Conn::new(conn),
            Ok(Err(e)) => return Err(NodeError::pool(&db, WHAT, e)),
            Err(_) => return Err(timed_out()),
        };
        let id_text = self.shared.identity.id().to_string();
        let beat = self.st.beat;
        let t = &sql::ID_BEAT_TYPES;
        let params: [(&(dyn ToSql + Sync), Type); 2] =
            [(&id_text, t[0].clone()), (&beat, t[1].clone())];
        let id_params: [(&(dyn ToSql + Sync), Type); 1] = [(&id_text, sql::ID_TYPES[0].clone())];
        let begin = sql::begin_sql(ms(bound), ms(after), false);
        let answers = {
            let mut flight = Flight::default();
            flight
                .batch(&conn, &begin)
                .query(&conn, sql::SHUTDOWN_SQL, &params)
                .query(&conn, sql::OWN_SQL, &id_params)
                .batch(&conn, "COMMIT");
            match tokio::time::timeout_at(deadline, flight.send()).await {
                Ok(answers) => answers,
                Err(_) => return Err(timed_out()),
            }
        };
        let (rows, failure) = split(answers);
        if let Some(e) = failure {
            rollback(conn).await;
            return Err(NodeError::pg(&db, WHAT, e));
        }
        conn.recycle();
        if rows_at(&rows, 1).is_empty() {
            match rows_at(&rows, 2).first() {
                Some(own) => {
                    let row_beat: i64 = get(own, 0, &db, WHAT)?;
                    if row_beat > beat {
                        // a clone renews this node id: it must not be marked stopped
                        tracing::error!(target: "erp::node", database = %db,
                            node = %self.shared.identity.id(),
                            "the shutdown found the row ahead of this process's beat: another \
                             process renews this node id; nothing was written");
                        return Err(self.conflict_error());
                    }
                    return Err(NodeError::undecodable(
                        &db,
                        WHAT,
                        "the row at this node's beat was not updated",
                    ));
                }
                None => tracing::warn!(target: "erp::node", database = %db,
                    "the shutdown found no row (it was purged, or lost in a failover); nothing \
                     was written"),
            }
        }
        Ok(())
    }
}

/// Whether a ready peer defines the expected digest for `identity`: dev rows only for dev
/// identities (design §17 decision 5).
fn is_reference(identity: &NodeIdentity, p: &PeerRow) -> bool {
    p.node.state == NodeState::Ready && (identity.is_dev() || !p.dev)
}

fn decode_sections(payload: &[u8]) -> Option<Vec<(String, Digest)>> {
    let mut dec = Dec::new(payload);
    let n = dec.list()?;
    let mut out = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        out.push((dec.str()?.to_owned(), dec.hash()?));
    }
    dec.is_empty().then_some(out)
}

/// An addon as compared: version, kind, content digest.
type AddonKey = (String, String, Option<Digest>);

fn decode_addons(payload: &[u8]) -> Option<BTreeMap<String, AddonKey>> {
    let mut dec = Dec::new(payload);
    let n = dec.list()?;
    let mut out = BTreeMap::new();
    for _ in 0..n {
        let name = dec.str()?.to_owned();
        let version = dec.str()?.to_owned();
        let kind = dec.str()?.to_owned();
        let content = if kind == "embedded" {
            dec.str()?;
            None
        } else {
            Some(dec.hash()?)
        };
        out.insert(name, (version, kind, content));
    }
    dec.is_empty().then_some(out)
}

fn differing_sections(identity: &NodeIdentity, theirs: &[(String, Digest)]) -> Vec<String> {
    let mine: BTreeMap<&str, Digest> = identity
        .config()
        .sections()
        .iter()
        .map(|s| (s.name().as_str(), s.digest()))
        .collect();
    let theirs: BTreeMap<&str, Digest> = theirs.iter().map(|(n, d)| (n.as_str(), *d)).collect();
    let mut names: Vec<String> = mine
        .keys()
        .chain(theirs.keys())
        .filter(|n| mine.get(*n) != theirs.get(*n))
        .map(|n| (*n).to_owned())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn differing_addons(identity: &NodeIdentity, theirs: &BTreeMap<String, AddonKey>) -> Vec<String> {
    let mine: BTreeMap<String, AddonKey> = identity
        .addons()
        .entries()
        .iter()
        .map(|e| {
            let (kind, content) = match e.content() {
                AddonContent::Embedded => ("embedded", None),
                AddonContent::Package(d) => ("package", Some(d)),
                AddonContent::DevFolder(d) => ("dev_folder", Some(d)),
            };
            (
                e.name().to_owned(),
                (e.version().to_owned(), kind.to_owned(), content),
            )
        })
        .collect();
    let mut names: Vec<String> = mine
        .keys()
        .chain(theirs.keys())
        .filter(|n| mine.get(*n) != theirs.get(*n))
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    /// Not a spec case: background work is logged louder as its failures repeat; a skipped
    /// window turn is information the first time (review round 3, findings 1 and 9).
    #[test]
    fn background_work_is_logged_louder_as_its_failures_repeat() {
        assert_eq!(loudness(1, false), Loudness::Warn);
        assert_eq!(loudness(2, false), Loudness::Warn);
        assert_eq!(loudness(3, false), Loudness::Error);
        assert_eq!(loudness(1, true), Loudness::Info);
        assert_eq!(loudness(2, true), Loudness::Warn);
        assert_eq!(loudness(3, true), Loudness::Error);
        assert_eq!(loudness(u32::MAX, true), Loudness::Error);
    }

    fn opts() -> NodeOptions {
        NodeOptions::builder()
            .heartbeat_interval_secs(10)
            .lease_intervals(3)
            .build()
            .unwrap()
    }

    const S: u64 = 1_000_000_000;

    /// The formula behind on its own (the case is covered on a database by
    /// `tests/node.rs::a_lost_asynchronous_renewal_never_extends_admission`).
    #[test]
    fn a_lost_renewal_never_extends_admission() {
        let o = opts();
        // steady state: the renewal before was sent one interval earlier, both bounds agree
        assert_eq!(serve_until_ns(100 * S, 90 * S, &o), 120 * S);
        // renewal k sent late (25 s after k-1): if k is lost, peers see k-1's expiry (k-1 + 30 s)
        assert_eq!(serve_until_ns(125 * S, 100 * S, &o), 130 * S);
        // a synchronous commit made k durable itself: the fence is k + lease - interval
        assert_eq!(serve_until_ns(125 * S, 125 * S, &o), 145 * S);
    }

    #[test]
    fn overload_and_session_timeouts_are_transient_and_a_closed_pool_is_permanent() {
        for code in [
            "53300", "53200", "53400", "53000", "57P05", "25P03", "57014", "08006",
        ] {
            assert_eq!(classify_code(code), FailureClass::Transient, "{code}");
        }
        for code in ["53100", "23514", "42501", "42P01", "22003"] {
            assert_eq!(classify_code(code), FailureClass::Permanent, "{code}");
        }
        let closed = pool_error(&deadpool_postgres::PoolError::Closed);
        assert_eq!(closed.class(), FailureClass::Permanent);
        assert_eq!(
            closed.message(),
            "could not get a database connection: the heartbeat pool is closed"
        );
    }
}
