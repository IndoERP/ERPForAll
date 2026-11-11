//! Read routing: which pool serves a transaction,
//! the unit of work that keeps read-your-writes, and the counters that make every routing decision
//! observable.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use deadpool_postgres::Pool;
use erp_fields::{Condition, Error, ErrorKind};

/// The pool a transaction runs on ([`crate::Transaction::route`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Route {
    /// The primary pool (every write, and every read when no read pool is configured or the
    /// routing rules sent it back).
    Primary,
    /// The read pool ([`crate::OrmBuilder::read_pool`]).
    ReadPool,
}

/// What an unreachable or unusable read pool does to a read transaction
/// ([`crate::OrmBuilder::read_pool_failure`]). "Unusable" covers a checkout that fails, a
/// connection that dies between checkout and `BEGIN`, and a connection that fails the per-connection
/// read-only check ([`crate::Orm::check_read_pool`]). A pool that points at another database or an
/// unsupported server is a configuration error and never falls back, whatever this says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ReadPoolFailure {
    /// Fail with a typed error ([`Error::is_read_pool_unavailable`]). The default: a read pool that
    /// cannot be reached is an operator's problem to see, not a silent load shift to the primary.
    #[default]
    Fail,
    /// Begin the transaction on the primary instead. Counted in
    /// [`RoutingStats::read_pool_unavailable`] and logged with the cause (rate limited).
    Primary,
}

/// Why a read transaction ran on the primary although a read pool is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reason {
    /// The unit of work began a write transaction earlier (read-your-writes).
    UnitOfWorkWrote,
    /// The replica's signal generations were lower than the ones this process applied.
    ReplicaBehind(String),
    /// The replica's replay lag is over [`crate::OrmBuilder::read_pool_max_lag`] (or unknown).
    Lagging(String),
    /// The signal listener is down, still catching up or not running (and no
    /// [`crate::OrmBuilder::read_pool_max_lag`] bounds the replica): this process may not know of
    /// changes.
    ListenerDown,
    /// A signal refresh is running (or waiting for the apply lock): a policy pinned now could be
    /// newer than the replica's snapshot, so the transaction reads from the primary.
    RefreshRunning,
    /// The read pool could not serve (the cause is the pool's or the connection's error).
    ReadPoolUnavailable(String),
}

impl Reason {
    fn slot(&self) -> usize {
        match self {
            Self::UnitOfWorkWrote => 0,
            Self::ReplicaBehind(_) => 1,
            Self::Lagging(_) => 2,
            Self::ListenerDown => 3,
            Self::ReadPoolUnavailable(_) => 4,
            Self::RefreshRunning => 5,
        }
    }
}

/// The configured read pool.
pub(crate) struct ReadRoute {
    pub(crate) pool: Pool,
    pub(crate) on_failure: ReadPoolFailure,
    pub(crate) max_lag: Option<Duration>,
    /// Which connections of the pool passed [`crate::Orm::check_read_pool`]'s verification (same
    /// database and server as the primary, supported version, read-only), and the last failure.
    /// Verified per connection, on its first routed use: a pool opens connections lazily and
    /// replaces dead ones, so a failover, a DNS change or a pooler can put a later connection on
    /// another server.
    pub(crate) verify: VerifyCache,
}

/// How long a failed verification of a read-pool connection is remembered: within it, a
/// connection not verified yet is refused without the primary checkout and the catalog reads a
/// verification costs, so [`ReadPoolFailure::Primary`] does not re-verify on every read. After it
/// the next unverified connection is checked again (an operator's fix is then picked up).
pub(crate) const VERIFY_FAILURE_TTL: Duration = Duration::from_secs(5);

/// Verified connections remembered at most; past it the memory is dropped and connections are
/// verified again (a pool never holds this many; ids of connections it replaced pile up).
const VERIFIED_MAX: usize = 1024;

/// A failed verification, kept so it can be replayed to the reads that follow.
#[derive(Debug, Clone)]
pub(crate) enum CachedFailure {
    /// The pool cannot serve (not read-only): follows [`ReadPoolFailure`].
    Unavailable(String),
    /// A configuration error (another database, an unsupported server): kind and message.
    Fatal(ErrorKind, String),
}

#[derive(Default)]
struct VerifyState {
    verified: std::collections::HashSet<Instant>,
    failed: Option<(Instant, CachedFailure)>,
}

/// The per-connection verification memory of the read pool. A connection is identified by the
/// instant its pool created it (`deadpool` metrics): it does not change while the connection
/// lives, and a replacement connection has a new one.
#[derive(Default)]
pub(crate) struct VerifyCache {
    state: std::sync::Mutex<VerifyState>,
}

impl VerifyCache {
    fn state(&self) -> std::sync::MutexGuard<'_, VerifyState> {
        // a panic while the lock was held leaves a consistent map: keep serving
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the connection created at `id` passed the verification.
    pub(crate) fn is_verified(&self, id: Instant) -> bool {
        self.state().verified.contains(&id)
    }

    /// The failure of the last verification when it is younger than [`VERIFY_FAILURE_TTL`].
    pub(crate) fn recent_failure(&self) -> Option<CachedFailure> {
        self.state()
            .failed
            .as_ref()
            .filter(|(at, _)| at.elapsed() < VERIFY_FAILURE_TTL)
            .map(|(_, failure)| failure.clone())
    }

    pub(crate) fn record_ok(&self, id: Instant) {
        let mut state = self.state();
        if state.verified.len() >= VERIFIED_MAX {
            state.verified.clear();
        }
        state.verified.insert(id);
        state.failed = None;
    }

    pub(crate) fn record_failure(&self, failure: CachedFailure) {
        self.state().failed = Some((Instant::now(), failure));
    }
}

/// The longest a `ROLLBACK` on the read pool may take when a routed transaction moves to the
/// primary; a connection that does not answer in time is detached from the pool.
pub(crate) const REROUTE_ROLLBACK_TIMEOUT: Duration = Duration::from_secs(5);

/// The replay lag of a standby, measured with the routed `BEGIN`: whether the server is in
/// recovery, and the seconds since the last replayed transaction committed.
pub(crate) const LAG_SQL: &str = "SELECT pg_is_in_recovery(), \
     EXTRACT(EPOCH FROM (clock_timestamp() - pg_last_xact_replay_timestamp()))::float8";

/// A failed replay lag read at `BEGIN` (not a failed signal read).
pub(crate) fn lag_read_error(e: tokio_postgres::Error) -> Error {
    Error::new(
        ErrorKind::Database,
        format!("could not read the replica's replay lag: {e}"),
    )
    .with_source(e)
}

/// Whether the replica's replay lag (the rows of [`LAG_SQL`]) is over `max`: `Ok(Some(detail))`
/// when it is or cannot be measured, `Ok(None)` when within the bound.
pub(crate) fn lagging(
    rows: &[tokio_postgres::Row],
    max: Duration,
) -> erp_fields::Result<Option<String>> {
    let unreadable = |e: tokio_postgres::Error| {
        Error::new(
            ErrorKind::Database,
            "could not read the replica's replay lag",
        )
        .with_source(e)
    };
    let Some(row) = rows.first() else {
        return Err(Error::new(
            ErrorKind::Database,
            "the replica's replay lag read returned no row",
        ));
    };
    let in_recovery: bool = row.try_get(0).map_err(unreadable)?;
    let seconds: Option<f64> = row.try_get(1).map_err(unreadable)?;
    Ok(lag_verdict(in_recovery, seconds, max))
}

/// The decision of [`lagging`]. A server that is not in recovery (a read-only role of the
/// primary) has lag 0. A standby that has replayed nothing yet has an unknown lag, which counts
/// as over the bound (the safe direction: the primary serves).
pub(crate) fn lag_verdict(
    in_recovery: bool,
    seconds: Option<f64>,
    max: Duration,
) -> Option<String> {
    if !in_recovery {
        return None;
    }
    match seconds {
        None => Some("replay lag unknown (no transaction replayed yet)".to_owned()),
        Some(lag) if lag > max.as_secs_f64() => {
            Some(format!("replay lag {lag:.3}s over the bound {max:?}"))
        }
        Some(_) => None,
    }
}

const WARN_EVERY: Duration = Duration::from_secs(10);
const REASONS: usize = 8;
/// Warning slots after the [`Reason`] ones.
const ERROR_SLOT: usize = 6;
const PRIMARY_FAILED_SLOT: usize = 7;

/// Routing decisions since the process started (relaxed atomics: counters, not a protocol).
pub(crate) struct RoutingCounters {
    served: AtomicU64,
    unit_of_work_wrote: AtomicU64,
    replica_behind: AtomicU64,
    replica_lagging: AtomicU64,
    listener_down: AtomicU64,
    refresh_running: AtomicU64,
    read_pool_unavailable: AtomicU64,
    read_pool_timeouts: AtomicU64,
    read_pool_errors: AtomicU64,
    rollback_failures: AtomicU64,
    reroute_failures: AtomicU64,
    unscoped: AtomicU64,
    started: Instant,
    /// Per reason: milliseconds since `started` of the last warning plus one (0: never), and the
    /// events counted since.
    last_warn: [AtomicU64; REASONS],
    suppressed: [AtomicU64; REASONS],
}

impl Default for RoutingCounters {
    fn default() -> Self {
        Self {
            served: AtomicU64::new(0),
            unit_of_work_wrote: AtomicU64::new(0),
            replica_behind: AtomicU64::new(0),
            replica_lagging: AtomicU64::new(0),
            listener_down: AtomicU64::new(0),
            refresh_running: AtomicU64::new(0),
            read_pool_unavailable: AtomicU64::new(0),
            read_pool_timeouts: AtomicU64::new(0),
            read_pool_errors: AtomicU64::new(0),
            rollback_failures: AtomicU64::new(0),
            reroute_failures: AtomicU64::new(0),
            unscoped: AtomicU64::new(0),
            started: Instant::now(),
            last_warn: Default::default(),
            suppressed: Default::default(),
        }
    }
}

impl RoutingCounters {
    /// A read transaction began on the read pool and stays there.
    pub(crate) fn served(&self) {
        self.served.fetch_add(1, Ordering::Relaxed);
    }

    /// A routed read began outside any [`unit_of_work`]: it is its own unit and does not see the
    /// writes of the request it belongs to.
    pub(crate) fn unscoped(&self) {
        self.unscoped.fetch_add(1, Ordering::Relaxed);
    }

    /// A read transaction failed because the read pool could not serve ([`ReadPoolFailure::Fail`]),
    /// logged with its cause (rate limited).
    pub(crate) fn error(&self, cause: &str) {
        self.read_pool_errors.fetch_add(1, Ordering::Relaxed);
        self.warn_limited(
            ERROR_SLOT,
            || tracing::warn!(target: "erp::orm", error = %cause, "read pool unavailable: the read failed"),
        );
    }

    /// The primary pool failed while the read pool was being verified: the read fails with the
    /// primary's error. Not a read pool failure and not counted as one.
    pub(crate) fn primary_failed_during_verification(&self, cause: &str) {
        self.warn_limited(
            PRIMARY_FAILED_SLOT,
            || tracing::warn!(target: "erp::orm", error = %cause, "primary pool failed while verifying the read pool: the read failed"),
        );
    }

    /// The read pool timed out handing out a connection (saturated, not down).
    pub(crate) fn timeout(&self) {
        self.read_pool_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    /// A `ROLLBACK` on a read-pool connection failed: the connection was detached from the pool.
    pub(crate) fn rollback_failed(&self, error: &dyn std::fmt::Display, reason: &Reason) {
        self.rollback_failures.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            target: "erp::orm",
            error = %error,
            reason = ?reason,
            "ROLLBACK on the read pool failed; the connection was detached from the pool"
        );
    }

    /// The primary pool could not give a connection to a transaction leaving the read pool.
    pub(crate) fn reroute_failed(&self) {
        self.reroute_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a read transaction that began on the primary although a read pool is configured, and
    /// say why in the log (a warning per reason at most every ten seconds, with the number of
    /// events since the last one; the counters carry the volume).
    pub(crate) fn fell_back(&self, reason: &Reason) {
        let counter = match reason {
            Reason::UnitOfWorkWrote => &self.unit_of_work_wrote,
            Reason::ReplicaBehind(_) => &self.replica_behind,
            Reason::Lagging(_) => &self.replica_lagging,
            Reason::ListenerDown => &self.listener_down,
            Reason::RefreshRunning => &self.refresh_running,
            Reason::ReadPoolUnavailable(_) => &self.read_pool_unavailable,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        match reason {
            Reason::UnitOfWorkWrote => {}
            Reason::ReplicaBehind(keys) => self.warn_limited(reason.slot(), || {
                tracing::warn!(
                    target: "erp::orm",
                    keys = %keys,
                    "read pool is behind the applied signal generations: running on the primary"
                );
            }),
            Reason::Lagging(detail) => self.warn_limited(reason.slot(), || {
                tracing::warn!(
                    target: "erp::orm",
                    lag = %detail,
                    "read pool replay lag is over the configured bound: running on the primary"
                );
            }),
            Reason::ListenerDown => self.warn_limited(reason.slot(), || {
                tracing::warn!(
                    target: "erp::orm",
                    "signal listener is down, catching up or not running (and no read_pool_max_lag \
                     is set): running reads on the primary"
                );
            }),
            Reason::RefreshRunning => self.warn_limited(reason.slot(), || {
                tracing::warn!(
                    target: "erp::orm",
                    "a signal refresh is running: reads that began meanwhile run on the primary"
                );
            }),
            Reason::ReadPoolUnavailable(cause) => self.warn_limited(reason.slot(), || {
                tracing::warn!(
                    target: "erp::orm",
                    error = %cause,
                    "read pool unavailable: running on the primary (ReadPoolFailure::Primary)"
                );
            }),
        }
    }

    /// Run `log` for the first event of a slot and then at most every [`WARN_EVERY`]; the events
    /// in between are counted and reported with the next line.
    fn warn_limited(&self, slot: usize, log: impl FnOnce()) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let last = self.last_warn[slot].load(Ordering::Relaxed);
        let due = last == 0
            || now.saturating_sub(last - 1)
                >= u64::try_from(WARN_EVERY.as_millis()).unwrap_or(u64::MAX);
        if due
            && self.last_warn[slot]
                .compare_exchange(
                    last,
                    now.saturating_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            let skipped = self.suppressed[slot].swap(0, Ordering::Relaxed);
            if skipped > 0 {
                tracing::warn!(
                    target: "erp::orm",
                    skipped,
                    "routing events of the same kind since the last warning"
                );
            }
            log();
        } else {
            self.suppressed[slot].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self, read_pool: bool) -> RoutingStats {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        RoutingStats {
            read_pool,
            read_pool_served: load(&self.served),
            unit_of_work_wrote: load(&self.unit_of_work_wrote),
            replica_behind: load(&self.replica_behind),
            replica_lagging: load(&self.replica_lagging),
            listener_down: load(&self.listener_down),
            refresh_running: load(&self.refresh_running),
            read_pool_unavailable: load(&self.read_pool_unavailable),
            read_pool_timeouts: load(&self.read_pool_timeouts),
            read_pool_errors: load(&self.read_pool_errors),
            rollback_failures: load(&self.rollback_failures),
            reroute_failures: load(&self.reroute_failures),
            unscoped: load(&self.unscoped),
        }
    }
}

/// Routing counters ([`crate::Orm::routing_stats`]).
///
/// With a read pool configured, every read transaction ends as exactly one of
/// `read_pool_served`, a fallback to the primary (`unit_of_work_wrote`, `replica_behind`,
/// `replica_lagging`, `listener_down`, `refresh_running`, `read_pool_unavailable`) or `read_pool_errors`; a
/// transaction whose `BEGIN` then fails on the primary too is counted in none of them. The counters
/// are separate relaxed atomics: a snapshot taken while requests run can disagree across fields.
///
/// A process routes reads to the read pool only while it knows the changes it would have to
/// apply: with a read pool configured, a process whose signal listener is not running (never
/// started with [`crate::Orm::spawn_signal_listener`], reconnecting or catching up) sends its
/// reads to the primary and counts them in `listener_down`, unless
/// [`crate::OrmBuilder::read_pool_max_lag`] is set (the lag bound then stands in for the
/// listener). Start the listener, or set the bound.
///
/// What the guard compares is the signal generations (registry, policy, membership, named caches),
/// not row data: an ordinary data write committed on the primary by another request is invisible
/// to it (only the unit of work protects a request's own writes) unless
/// [`crate::OrmBuilder::read_pool_max_lag`] bounds the replica's replay lag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct RoutingStats {
    /// A read pool is configured. `false`: every transaction uses the primary.
    pub read_pool: bool,
    /// Read transactions served by the read pool (they stayed there).
    pub read_pool_served: u64,
    /// Read transactions sent to the primary because the unit of work had begun a write.
    pub unit_of_work_wrote: u64,
    /// Read transactions discarded on the read pool and run on the primary because the replica
    /// was behind the generations this process applied (schema, policy, membership or cache
    /// changes, not row data).
    pub replica_behind: u64,
    /// Read transactions run on the primary because the replica's replay lag was over
    /// [`crate::OrmBuilder::read_pool_max_lag`] or could not be measured.
    pub replica_lagging: u64,
    /// Read transactions run on the primary because the signal listener was down, still catching
    /// up or never started (with no [`crate::OrmBuilder::read_pool_max_lag`] set).
    pub listener_down: u64,
    /// Read transactions that began on the read pool and moved to the primary because a signal
    /// refresh was running when their policy was to be pinned (a genuine collision with an apply,
    /// not with another routed read).
    pub refresh_running: u64,
    /// Read transactions run on the primary because the read pool was unavailable
    /// ([`ReadPoolFailure::Primary`]).
    pub read_pool_unavailable: u64,
    /// Checkouts that timed out because the read pool was saturated (counted in addition to the
    /// outcome, `read_pool_unavailable` or `read_pool_errors`).
    pub read_pool_timeouts: u64,
    /// Read transactions that failed because the read pool was unavailable
    /// ([`ReadPoolFailure::Fail`]).
    pub read_pool_errors: u64,
    /// `ROLLBACK`s on the read pool that failed when a routed transaction moved to the primary;
    /// the connection was detached from the pool.
    pub rollback_failures: u64,
    /// Transactions that left the read pool and then could not get a primary connection.
    pub reroute_failures: u64,
    /// Read transactions served by the read pool that began outside any [`unit_of_work`]. They
    /// are their own unit of work and may not see a write of the same request.
    pub unscoped: u64,
}

/// What [`crate::Orm::check_read_pool`] found on a connection of the read pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadPoolStatus {
    /// The server is a standby (`pg_is_in_recovery()`): it cannot accept writes at all.
    pub in_recovery: bool,
    /// The session defaults to read-only transactions (`default_transaction_read_only`). Reported
    /// for information: any session can change it, so it is not a boundary.
    pub default_read_only: bool,
    /// The connection's role holds `INSERT`, `UPDATE` or `DELETE` on the ORM's signal table (the
    /// check stands for the ORM's tables: a role that may write that may write the rest).
    pub can_write: bool,
}

impl ReadPoolStatus {
    /// True when a write cannot succeed on this pool whatever the session asks for: a standby, or
    /// a role without write privileges. `default_transaction_read_only` does not count.
    pub fn is_read_only(&self) -> bool {
        self.in_recovery || !self.can_write
    }
}

/// The error of a read pool that cannot serve: a static message, the cause as its source. The
/// builder setting that changes the behaviour is in the log, not in a message a client may see.
pub(crate) fn unavailable_error(error: deadpool_postgres::PoolError) -> Error {
    Error::new(ErrorKind::Database, "read replica unavailable")
        .with_condition(Condition::ReadPoolUnavailable)
        .with_source(error)
}

/// [`unavailable_error`] for a failure that is not a pool error (the connection died after
/// checkout, or the pool failed the read-only check).
pub(crate) fn unavailable_with(cause: &str, source: Option<Error>) -> Error {
    let error = Error::new(ErrorKind::Database, "read replica unavailable")
        .with_condition(Condition::ReadPoolUnavailable);
    match source {
        Some(source) => error.with_source(source),
        None => error.with_source(CauseText(cause.to_owned())),
    }
}

#[derive(Debug)]
struct CauseText(String);

impl std::fmt::Display for CauseText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CauseText {}

/// The write mark shared by every transaction of one unit of work (sticky: the replica may not
/// have replayed a committed write yet).
pub(crate) type UnitFlag = Arc<AtomicBool>;

tokio::task_local! {
    static UNIT: UnitFlag;
}

/// Run `work` as one unit of work for read routing (a read goes to the read pool only
/// when the unit of work has no write). Once a write-intent transaction begins inside the scope,
/// every later read transaction of the scope uses the primary, so a unit of work reads its own
/// writes. Nested calls join the outer unit of work. [`crate::Orm::run`] and every RPC call are
/// one unit of work.
///
/// A transaction remembers the unit of work it was opened in: it keeps reading its mark when it
/// is used later or in another task. A task spawned inside the scope does not belong to it
/// (task-locals do not cross `tokio::spawn`): spawn it with [`spawn_in_unit_of_work`]. A read
/// that begins on the read pool outside any scope is counted ([`RoutingStats::unscoped`]). Only
/// write transactions begun through the [`crate::Orm`] mark the unit; a write through a raw pool
/// connection or another service's pool does not. Other requests are not ordered either: a
/// replica is only guaranteed current within a unit of work.
///
/// ```no_run
/// # async fn example(orm: &erp_orm::Orm) -> erp_fields::Result<()> {
/// use erp_orm::{BeginOptions, SUPERUSER_ID, unit_of_work};
/// unit_of_work(async {
///     let mut write = orm.begin(SUPERUSER_ID).await?; // marks the unit of work
///     write.commit().await?;
///     let read = orm
///         .begin_opts(BeginOptions::new(SUPERUSER_ID).read_only("res.partner.read"))
///         .await?; // runs on the primary
///     assert_eq!(read.route(), erp_orm::Route::Primary);
///     Ok(())
/// })
/// .await
/// # }
/// ```
pub async fn unit_of_work<F: Future>(work: F) -> F::Output {
    if UNIT.try_with(|_| ()).is_ok() {
        return work.await;
    }
    UNIT.scope(Arc::new(AtomicBool::new(false)), work).await
}

/// `tokio::spawn` that keeps the surrounding [`unit_of_work`]: the spawned task joins it (its
/// reads see the writes the unit already made, and its writes mark the unit). Outside any scope
/// it is a plain `tokio::spawn`.
pub fn spawn_in_unit_of_work<F>(work: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match current_unit() {
        Some(flag) => tokio::spawn(UNIT.scope(flag, work)),
        None => tokio::spawn(work),
    }
}

/// The surrounding unit of work's mark, if any.
pub(crate) fn current_unit() -> Option<UnitFlag> {
    UNIT.try_with(Arc::clone).ok()
}

/// A write-intent transaction begins: its unit of work (if any) has written.
pub(crate) fn mark_write(unit: Option<&UnitFlag>) {
    // outside any scope the transaction is its own unit of work: nothing to mark
    if let Some(unit) = unit {
        unit.store(true, Ordering::Release);
    }
}

/// Whether the unit of work has begun a write transaction.
pub(crate) fn unit_wrote(unit: Option<&UnitFlag>) -> bool {
    unit.is_some_and(|unit| unit.load(Ordering::Acquire))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_spawned_task_joins_the_unit_of_work() {
        unit_of_work(async {
            let inside = current_unit().unwrap();
            mark_write(Some(&inside));
            // a plain spawn loses the scope; the helper keeps it and sees the mark
            let plain = tokio::spawn(async { current_unit().is_none() });
            assert!(plain.await.unwrap());
            let kept = spawn_in_unit_of_work(async { unit_wrote(current_unit().as_ref()) });
            assert!(kept.await.unwrap());
            // a nested scope joins the outer one
            unit_of_work(async { assert!(unit_wrote(current_unit().as_ref())) }).await;
        })
        .await;
        let outside = spawn_in_unit_of_work(async { current_unit().is_none() });
        assert!(outside.await.unwrap());
    }

    #[test]
    fn the_lag_verdict_is_conservative() {
        let max = Duration::from_secs(2);
        assert_eq!(lag_verdict(false, None, max), None, "not a standby: no lag");
        assert_eq!(lag_verdict(false, Some(99.0), max), None);
        assert_eq!(lag_verdict(true, Some(1.5), max), None);
        assert_eq!(
            lag_verdict(true, Some(2.0), max),
            None,
            "the bound is allowed"
        );
        assert_eq!(
            lag_verdict(true, Some(2.5), max).as_deref(),
            Some("replay lag 2.500s over the bound 2s")
        );
        assert_eq!(
            lag_verdict(true, None, max).as_deref(),
            Some("replay lag unknown (no transaction replayed yet)")
        );
    }

    #[test]
    fn verification_is_remembered_per_connection_and_a_failure_for_a_while() {
        let cache = VerifyCache::default();
        let (a, b) = (Instant::now(), Instant::now() + Duration::from_nanos(1));
        assert!(!cache.is_verified(a));
        cache.record_ok(a);
        assert!(cache.is_verified(a));
        assert!(
            !cache.is_verified(b),
            "another connection is verified on its own"
        );
        assert!(cache.recent_failure().is_none());
        cache.record_failure(CachedFailure::Unavailable("not read-only".to_owned()));
        assert!(matches!(
            cache.recent_failure(),
            Some(CachedFailure::Unavailable(cause)) if cause == "not read-only"
        ));
        // older than the window: verified again
        cache.state().failed = Some((
            Instant::now() - VERIFY_FAILURE_TTL - Duration::from_millis(1),
            CachedFailure::Unavailable("stale".to_owned()),
        ));
        assert!(cache.recent_failure().is_none());
        // a connection that passes clears the failure
        cache.record_failure(CachedFailure::Unavailable("again".to_owned()));
        cache.record_ok(b);
        assert!(cache.recent_failure().is_none());
    }

    #[test]
    fn warnings_are_rate_limited_per_reason() {
        let counters = RoutingCounters::default();
        let logged = std::cell::Cell::new(0);
        for _ in 0..5 {
            counters.warn_limited(1, || logged.set(logged.get() + 1));
        }
        assert_eq!(
            logged.get(),
            1,
            "the first event logs, the rest are counted"
        );
        assert_eq!(counters.suppressed[1].load(Ordering::Relaxed), 4);
        counters.warn_limited(2, || logged.set(logged.get() + 1));
        assert_eq!(logged.get(), 2, "another reason has its own budget");
    }
}
