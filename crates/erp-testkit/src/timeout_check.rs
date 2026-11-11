//! Whether a test cluster cancels a statement at its timeout (TEST.NODE-MACOS).
//!
//! PostgreSQL raises `statement_timeout`, `lock_timeout`, `transaction_timeout` and
//! `idle_in_transaction_session_timeout` from one interval timer per backend (`SIGALRM`). Tests
//! that hold a lock and expect the blocked statement to end with SQLSTATE 57014 or 55P03 within a
//! short client grace (the node registry's tests allow 25 % of a bound of 62–250 ms) need that
//! timer to fire on time.
//!
//! [`Cluster::probe_with_timeouts`] measures two of those timeouts on the probe's own connection:
//! - `statement_timeout = 20ms` over `pg_sleep(1)`, nine times (the latch-wait path);
//! - `lock_timeout = 20ms` on a `pg_advisory_lock` that a second connection holds, nine times
//!   (the lock-wait path the node tests block on; the second connection is closed when the check
//!   ends, on every path, which releases the lock).
//!
//! `transaction_timeout` and `idle_in_transaction_session_timeout` use the same timer and are not
//! measured separately. Each sample's lateness is its time less the smallest round trip of the
//! attempt (the `SET`s that armed the samples) less the 20 ms. When the median of either kind is
//! above [`MAX_TIMEOUT_LATENESS`] the whole measurement is taken again after a 250 ms pause, at
//! most [`ATTEMPTS`] times in all; the first on-time attempt passes, and the cluster is refused
//! only when every attempt is late: a coalesced timer is late every time, a busy moment on a
//! loaded host or a tunnel is not. Every step has a client bound; a stall (an ssh tunnel that
//! stops forwarding) is an error naming the step, never a pass. `erp-testkit probe` (run by
//! `scripts/ci.sh` before the tests) runs it on every reachable cluster CI uses, tunnelled ones
//! included.

use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

use tokio_postgres::Client;
use tokio_postgres::error::SqlState;

use crate::cluster::{
    Cluster, ClusterProbe, PROBE_TIMEOUT, PgProber, ProbeOutcome, Prober, Unreachable,
    UnreachableKind, unreachable_reason,
};
use crate::db::connect;
use crate::error::{Result, TestkitError, pg_error};

/// The latest a cluster may raise a timeout after it is due (median of the samples of one
/// kind).
pub const MAX_TIMEOUT_LATENESS: Duration = Duration::from_millis(10);

/// The timeout each sample arms.
pub(crate) const ARMED: Duration = Duration::from_millis(20);
const STATEMENT_SAMPLES: usize = 9;
/// How many times the check measures before it refuses a cluster (the first on-time one passes).
pub const ATTEMPTS: usize = 3;
/// The pause before each re-measure, so a busy moment (a loaded host, a tunnel burst) can pass.
pub(crate) const REMEASURE_PAUSE: Duration = Duration::from_millis(250);
const LOCK_SAMPLES: usize = 9;
/// Client bound on each step of a sample (the `SET`, then the statement the server should cancel
/// after 20 ms).
pub(crate) const STEP_BOUND: Duration = Duration::from_secs(2);
/// First key of the advisory lock the lock samples wait on (`ERPT`); the second is the holder's
/// backend pid, so two checks at once never share a lock.
const LOCK_KEY: i32 = 0x4552_5054;

/// Which timeout a sample measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Statement,
    Lock,
}

impl Kind {
    pub(crate) fn setting(self) -> &'static str {
        match self {
            Self::Statement => "statement_timeout",
            Self::Lock => "lock_timeout",
        }
    }

    /// Whether `code` is the SQLSTATE this timeout raises (57014, 55P03).
    fn raised(self, code: &SqlState) -> bool {
        *code
            == match self {
                Self::Statement => SqlState::QUERY_CANCELED,
                Self::Lock => SqlState::LOCK_NOT_AVAILABLE,
            }
    }

    /// The batch that clears any inherited statement timeout before arming the sample. A lock
    /// sample keeps a 1 s `statement_timeout` as a backstop: a `lock_timeout` that never fires
    /// then ends as 57014, which is reported as the wrong SQLSTATE instead of hanging.
    fn arm(self) -> &'static str {
        match self {
            Self::Statement => {
                "SET statement_timeout = 0; SET lock_timeout = 0; SET statement_timeout = 20"
            }
            Self::Lock => {
                "SET statement_timeout = 0; SET statement_timeout = 1000; SET lock_timeout = 20"
            }
        }
    }
}

/// What [`Cluster::probe_with_timeouts`] measured on a cluster that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeoutCheck {
    measured: Measured,
    attempts: usize,
}

impl TimeoutCheck {
    /// How late each `statement_timeout` sample was raised, sorted.
    pub fn statement_samples(&self) -> &[Duration] {
        &self.measured.statement
    }

    /// How late each `lock_timeout` sample was raised, sorted.
    pub fn lock_samples(&self) -> &[Duration] {
        &self.measured.lock
    }

    /// The median of [`Self::statement_samples`].
    pub fn statement_median(&self) -> Duration {
        median(&self.measured.statement)
    }

    /// The median of [`Self::lock_samples`].
    pub fn lock_median(&self) -> Duration {
        median(&self.measured.lock)
    }

    /// Which attempt passed: 1, or up to [`ATTEMPTS`] when the earlier ones were late.
    pub fn attempts(&self) -> usize {
        self.attempts
    }
}

impl fmt::Display for TimeoutCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "statement_timeout {} late, lock_timeout {} late (medians of {} and {} samples, at \
             most {} allowed",
            millis(self.statement_median()),
            millis(self.lock_median()),
            self.measured.statement.len(),
            self.measured.lock.len(),
            millis(MAX_TIMEOUT_LATENESS)
        )?;
        if self.attempts > 1 {
            write!(
                f,
                "; attempt {} of {ATTEMPTS}, the earlier {} late",
                self.attempts,
                self.attempts - 1
            )?;
        }
        f.write_str(")")
    }
}

/// The lateness of one attempt, each kind sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Measured {
    statement: Vec<Duration>,
    lock: Vec<Duration>,
}

impl Measured {
    pub(crate) fn new(mut statement: Vec<Duration>, mut lock: Vec<Duration>) -> Self {
        statement.sort_unstable();
        lock.sort_unstable();
        Self { statement, lock }
    }

    fn late(&self) -> bool {
        median(&self.statement) > MAX_TIMEOUT_LATENESS || median(&self.lock) > MAX_TIMEOUT_LATENESS
    }
}

/// The upper median of sorted samples (zero for none; an attempt always has samples).
pub(crate) fn median(sorted: &[Duration]) -> Duration {
    sorted
        .get(sorted.len() / 2)
        .copied()
        .unwrap_or(Duration::ZERO)
}

/// `12.3 ms`.
pub(crate) fn millis(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

/// `a, b, c` in ms.
pub(crate) fn render_samples(samples: &[Duration]) -> String {
    samples
        .iter()
        .map(|d| millis(*d))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why a check stopped early: the cluster went away (reported as unreachable, like a failed
/// probe), or a real failure.
enum Stop {
    Unreachable(Unreachable),
    Failed(TestkitError),
}

impl From<TestkitError> for Stop {
    fn from(error: TestkitError) -> Self {
        Self::Failed(error)
    }
}

impl Cluster {
    /// [`Cluster::probe`], and when the cluster is reachable, measure on the probe's connection how
    /// late it raises `statement_timeout` and `lock_timeout` (module docs). A cluster whose
    /// connection is lost during the check is reported unreachable, like a failed probe.
    ///
    /// Bounded: the probe and the lock holder's connection by the probe's 15 s each, every step of
    /// a sample by 2 s (54 samples at most over three attempts, with two 250 ms pauses). About
    /// 0.5 s on a cluster with working timers.
    ///
    /// # Errors
    /// What [`Cluster::probe`] returns, and [`TestkitError::LateTimeouts`] (every attempt late),
    /// [`TestkitError::TimeoutCheckStalled`] (a step got no answer within 2 s),
    /// [`TestkitError::TimeoutNotRaised`] (a statement finished instead of being cancelled),
    /// [`TestkitError::CancelledEarly`] (cancelled before its timeout was due),
    /// [`TestkitError::ClusterConnect`] or [`TestkitError::Postgres`].
    pub async fn probe_with_timeouts(&self) -> Result<(ClusterProbe, Option<TimeoutCheck>)> {
        let (client, usage) = match PgProber.probe(self).await? {
            ProbeOutcome::Ready { client, usage } => (client, usage),
            ProbeOutcome::Unreachable { reason } => {
                return Ok((ClusterProbe::Unreachable(reason), None));
            }
        };
        match check(self, &client).await {
            Ok(check) => Ok((ClusterProbe::Reachable(usage), Some(check))),
            Err(Stop::Unreachable(reason)) => Ok((ClusterProbe::Unreachable(reason), None)),
            Err(Stop::Failed(error)) => Err(error),
        }
    }
}

/// Measure until an attempt is on time, at most [`ATTEMPTS`] times, pausing before each re-measure.
async fn check(cluster: &Cluster, client: &Client) -> std::result::Result<TimeoutCheck, Stop> {
    // dropped at the end of this function on every path: its session ends, its lock with it
    let (holder, holder_pid) = hold_lock(cluster).await?;
    let mut attempts = Vec::with_capacity(ATTEMPTS);
    while measure_again(&attempts) {
        if !attempts.is_empty() {
            tokio::time::sleep(REMEASURE_PAUSE).await;
        }
        attempts.push(measure(cluster, client, holder_pid).await?);
    }
    drop(holder);
    judge(cluster, attempts).map_err(Stop::Failed)
}

/// Whether the attempts so far call for another: none yet, or the last late with one left.
pub(crate) fn measure_again(attempts: &[Measured]) -> bool {
    attempts.len() < ATTEMPTS && attempts.last().is_none_or(Measured::late)
}

/// The verdict on the attempts, in the order taken: the first on-time one passes; a cluster is
/// refused only when all [`ATTEMPTS`] are late.
pub(crate) fn judge(cluster: &Cluster, mut attempts: Vec<Measured>) -> Result<TimeoutCheck> {
    if let Some(on_time) = attempts.iter().position(|a| !a.late()) {
        let measured = attempts.swap_remove(on_time);
        return Ok(TimeoutCheck {
            measured,
            attempts: on_time + 1,
        });
    }
    let late = attempts.len();
    let (Some(last), ATTEMPTS) = (attempts.pop(), late) else {
        return Err(TestkitError::Configuration(format!(
            "the timeout check on {cluster} stopped after {late} late attempts of {ATTEMPTS}"
        )));
    };
    Err(TestkitError::LateTimeouts {
        cluster: cluster.to_string(),
        earlier: attempts
            .iter()
            .map(|a| (median(&a.statement), median(&a.lock)))
            .collect(),
        statement: last.statement,
        lock: last.lock,
    })
}

/// `statement_timeout median X, lock_timeout median Y; ...` for the earlier attempts.
pub(crate) fn render_earlier(earlier: &[(Duration, Duration)]) -> String {
    earlier
        .iter()
        .map(|(statement, lock)| {
            format!(
                "statement_timeout median {}, lock_timeout median {}",
                millis(*statement),
                millis(*lock)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// A second connection holding the advisory lock the lock samples wait on, and its backend pid.
async fn hold_lock(cluster: &Cluster) -> std::result::Result<(Client, i32), Stop> {
    let holder = match tokio::time::timeout(PROBE_TIMEOUT, connect(cluster.config())).await {
        Err(_) => {
            return Err(Stop::Unreachable(Unreachable::new(
                UnreachableKind::TimedOut,
                format!(
                    "no answer within {} s opening the timeout check's second connection",
                    PROBE_TIMEOUT.as_secs()
                ),
            )));
        }
        Ok(Err(TestkitError::Connect(source))) => {
            return Err(match unreachable_reason(&source) {
                Some(reason) => Stop::Unreachable(reason),
                None => Stop::Failed(TestkitError::ClusterConnect {
                    cluster: cluster.to_string(),
                    source,
                }),
            });
        }
        Ok(other) => other?,
    };
    let sql = format!("SELECT pg_backend_pid(), pg_advisory_lock({LOCK_KEY}, pg_backend_pid())");
    let rows = bounded(
        cluster,
        "taking the advisory lock on the second connection",
        holder.query(&sql, &[]),
    )
    .await?
    .map_err(|e| {
        lost_or(
            cluster,
            "taking the advisory lock for the lock_timeout samples",
            e,
        )
    })?;
    let pid: i32 = rows
        .first()
        .ok_or_else(|| {
            Stop::Failed(TestkitError::Configuration(format!(
                "taking the advisory lock on {cluster} returned no row"
            )))
        })?
        .try_get(0)
        .map_err(|e| lost_or(cluster, "reading the lock holder's backend pid", e))?;
    Ok((holder, pid))
}

/// One attempt: the statement samples, then the lock samples, lateness against the attempt's
/// smallest round trip.
async fn measure(
    cluster: &Cluster,
    client: &Client,
    holder_pid: i32,
) -> std::result::Result<Measured, Stop> {
    let lock_sql = format!("SELECT pg_advisory_lock({LOCK_KEY}, {holder_pid})");
    let mut raw = Vec::with_capacity(STATEMENT_SAMPLES + LOCK_SAMPLES);
    for _ in 0..STATEMENT_SAMPLES {
        raw.push(sample(cluster, client, Kind::Statement, "SELECT pg_sleep(1)").await?);
    }
    for _ in 0..LOCK_SAMPLES {
        raw.push(sample(cluster, client, Kind::Lock, &lock_sql).await?);
    }
    // the session is the probe's: leave no timeout armed on it
    bounded(
        cluster,
        "clearing the timeouts",
        client.batch_execute("SET statement_timeout = 0; SET lock_timeout = 0"),
    )
    .await?
    .map_err(|e| lost_or(cluster, "clearing the timeouts", e))?;
    let base = raw
        .iter()
        .map(|s| s.round_trip)
        .min()
        .unwrap_or(Duration::ZERO);
    let of = |kind: Kind| -> Vec<Duration> {
        raw.iter()
            .filter(|s| s.kind == kind)
            .map(|s| lateness(s.elapsed, base))
            .collect()
    };
    Ok(Measured::new(of(Kind::Statement), of(Kind::Lock)))
}

/// One sample's raw times.
struct Raw {
    kind: Kind,
    round_trip: Duration,
    elapsed: Duration,
}

/// Arm the timeout (its round trip is a baseline candidate), then time the statement it should
/// cancel.
async fn sample(
    cluster: &Cluster,
    client: &Client,
    kind: Kind,
    sql: &str,
) -> std::result::Result<Raw, Stop> {
    let arming = match kind {
        Kind::Statement => "arming statement_timeout (SET)",
        Kind::Lock => "arming lock_timeout (SET)",
    };
    let start = Instant::now();
    bounded(cluster, arming, client.batch_execute(kind.arm()))
        .await?
        .map_err(|e| lost_or(cluster, arming, e))?;
    let round_trip = start.elapsed();
    let waiting = match kind {
        Kind::Statement => "waiting for statement_timeout to cancel pg_sleep(1)",
        Kind::Lock => "waiting for lock_timeout to cancel the advisory lock wait",
    };
    let start = Instant::now();
    let answer = bounded(cluster, waiting, client.simple_query(sql)).await?;
    let elapsed = start.elapsed();
    match answer {
        Err(error) if error.code().is_some_and(|code| kind.raised(code)) => {
            on_time_or_early(cluster, kind, elapsed)?;
            Ok(Raw {
                kind,
                round_trip,
                elapsed,
            })
        }
        Err(error) => Err(lost_or(cluster, waiting, error)),
        Ok(_) => Err(Stop::Failed(TestkitError::TimeoutNotRaised {
            cluster: cluster.to_string(),
            setting: kind.setting(),
            waited: elapsed,
        })),
    }
}

/// A cancel that came before the timeout was due was not the timer (an operator's
/// `pg_cancel_backend`, a pooler): the sample is void, and the check fails rather than count it.
pub(crate) fn on_time_or_early(cluster: &Cluster, kind: Kind, elapsed: Duration) -> Result<()> {
    if elapsed < ARMED {
        return Err(TestkitError::CancelledEarly {
            cluster: cluster.to_string(),
            setting: kind.setting(),
            after: elapsed,
        });
    }
    Ok(())
}

/// Time past the armed timeout and the baseline round trip; zero when earlier (a fast host whose
/// cancel answer beat the slowest `SET`).
pub(crate) fn lateness(elapsed: Duration, base: Duration) -> Duration {
    elapsed.saturating_sub(base).saturating_sub(ARMED)
}

/// `fut` within [`STEP_BOUND`], else [`TestkitError::TimeoutCheckStalled`] naming the step.
async fn bounded<T>(
    cluster: &Cluster,
    step: &'static str,
    fut: impl Future<Output = T>,
) -> std::result::Result<T, Stop> {
    tokio::time::timeout(STEP_BOUND, fut).await.map_err(|_| {
        Stop::Failed(TestkitError::TimeoutCheckStalled {
            cluster: cluster.to_string(),
            step,
            waited: STEP_BOUND,
        })
    })
}

/// A lost connection is the cluster going away (unreachable); anything else fails the check.
fn lost_or(cluster: &Cluster, action: &str, error: tokio_postgres::Error) -> Stop {
    match unreachable_reason(&error) {
        Some(reason) => Stop::Unreachable(reason),
        None => Stop::Failed(pg_error(format!("{action} on {cluster}"))(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster() -> Cluster {
        Cluster::parse(1, 2, "postgres://u:secret@127.0.0.1:5540/erpforall").expect("valid URL")
    }

    async fn assert_timeout_setting(client: &Client, setting: &str, milliseconds: u64) {
        let seconds = milliseconds as f64 / 1000.0;
        let matches: bool = client
            .query_one(
                "SELECT current_setting($1)::interval = make_interval(secs => $2)",
                &[&setting, &seconds],
            )
            .await
            .expect("read timeout setting")
            .get(0);
        assert!(matches, "{setting} does not equal {milliseconds} ms");
    }

    fn assert_timeout_cancellation(error: &tokio_postgres::Error, state: &SqlState, message: &str) {
        assert_eq!(
            error.code(),
            Some(state),
            "unexpected cancellation: {error}"
        );
        let database_error = error
            .as_db_error()
            .expect("server reported a database error");
        assert!(
            database_error.message().contains(message),
            "unexpected cancellation: {}",
            database_error.message()
        );
    }

    #[tokio::test]
    async fn statement_arming_clears_the_previous_sample_timeout_before_setup() {
        let url = crate::db::database_url_from_env().expect("DATABASE_URL must be set");
        let cluster = Cluster::parse(1, 1, &url).expect("valid DATABASE_URL");
        let client = crate::db::connect(cluster.config())
            .await
            .expect("connect to DATABASE_URL");

        // The real preceding sample cancels its query and leaves its session setting in place.
        let previous = sample(&cluster, &client, Kind::Statement, "SELECT pg_sleep(1)").await;
        assert!(previous.is_ok(), "complete the preceding statement sample");
        assert_eq!(
            Kind::Lock.arm().split_once("; ").map(|(first, _)| first),
            Some("SET statement_timeout = 0"),
            "lock arming must clear the previous sample timeout first"
        );

        // Exercise the real arming SQL with a controlled delay between its first statement and
        // the rest. The old order leaves the previous 20 ms timeout active during this setup.
        let (first, rest) = Kind::Statement
            .arm()
            .split_once("; ")
            .expect("arming batch has multiple statements");
        let delayed_arm = format!("{first}; SELECT pg_sleep(0.1); {rest}");
        let setup = client.batch_execute(&delayed_arm).await;
        if let Err(error) = &setup {
            assert_eq!(
                error.code(),
                Some(&SqlState::QUERY_CANCELED),
                "unexpected arming failure: {error}"
            );
            let database_error = error
                .as_db_error()
                .expect("server reported a database error");
            assert!(
                database_error.message().contains("statement timeout"),
                "unexpected cancellation: {}",
                database_error.message()
            );
        }
        setup.expect("the first arming statement must clear the previous timeout");

        let cancelled = client
            .simple_query("SELECT pg_sleep(1)")
            .await
            .expect_err("the newly armed statement timeout must still cancel its sample");
        assert_eq!(cancelled.code(), Some(&SqlState::QUERY_CANCELED));
        let database_error = cancelled
            .as_db_error()
            .expect("server reported a database error");
        assert!(
            database_error.message().contains("statement timeout"),
            "unexpected cancellation: {}",
            database_error.message()
        );
    }

    #[tokio::test]
    async fn statement_and_lock_samples_rearm_each_other_on_the_same_session() {
        let url = crate::db::database_url_from_env().expect("DATABASE_URL must be set");
        let cluster = Cluster::parse(1, 1, &url).expect("valid DATABASE_URL");
        let client = crate::db::connect(cluster.config())
            .await
            .expect("connect to DATABASE_URL");
        let holder = crate::db::connect(cluster.config())
            .await
            .expect("connect a lock holder");
        let holder_pid: i32 = holder
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("read lock-holder pid")
            .get(0);
        holder
            .batch_execute(&format!(
                "SELECT pg_advisory_lock({LOCK_KEY}, {holder_pid})"
            ))
            .await
            .expect("hold the isolated advisory lock");

        let previous_statement =
            sample(&cluster, &client, Kind::Statement, "SELECT pg_sleep(1)").await;
        assert!(previous_statement.is_ok(), "complete statement sample");
        assert_timeout_setting(&client, "statement_timeout", 20).await;

        // Exercise Statement→Lock setup with a delay after the explicit reset statement. This
        // checks the actual batch boundary against the previous sample's live session state.
        let (reset, rest) = Kind::Lock
            .arm()
            .split_once("; ")
            .expect("lock arming starts with the reset");
        assert_eq!(reset, "SET statement_timeout = 0");
        let delayed_lock_arm = format!("{reset}; SELECT pg_sleep(0.1); {rest}");
        client
            .batch_execute(&delayed_lock_arm)
            .await
            .expect("lock setup delay must run after clearing the prior timeout");
        assert_timeout_setting(&client, "statement_timeout", 1000).await;
        assert_timeout_setting(&client, "lock_timeout", 20).await;

        let lock_sql = format!("SELECT pg_advisory_lock({LOCK_KEY}, {holder_pid})");
        let lock_sample = sample(&cluster, &client, Kind::Lock, &lock_sql).await;
        assert!(lock_sample.is_ok(), "complete lock sample");
        assert_timeout_setting(&client, "statement_timeout", 1000).await;
        assert_timeout_setting(&client, "lock_timeout", 20).await;

        // If lock_timeout never fires, the lock sample's statement timeout must still end the
        // wait with its own SQLSTATE and server message rather than hang or appear as a lock pass.
        let backstop = client
            .simple_query("SELECT pg_sleep(2)")
            .await
            .expect_err("the lock sample statement backstop must cancel a long statement");
        assert_timeout_cancellation(&backstop, &SqlState::QUERY_CANCELED, "statement timeout");

        // Lock→Statement returns the shared session to the statement sample's settings and
        // still cancels the measured statement with the expected PostgreSQL error.
        let statement_sample =
            sample(&cluster, &client, Kind::Statement, "SELECT pg_sleep(1)").await;
        assert!(
            statement_sample.is_ok(),
            "complete statement sample after lock sample"
        );
        assert_timeout_setting(&client, "statement_timeout", 20).await;
        assert_timeout_setting(&client, "lock_timeout", 0).await;
    }

    fn ms(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|v| Duration::from_millis(*v)).collect()
    }

    fn on_time() -> Measured {
        Measured::new(
            ms(&[1, 400, 2, 3, 1, 2, 4, 2, 10]),
            ms(&[2, 3, 1, 2, 1, 3, 2, 1, 2]),
        )
    }

    fn coalesced() -> Measured {
        Measured::new(
            ms(&[145, 147, 144, 2, 146, 148, 150, 145, 149]),
            ms(&[146, 149, 151, 147, 150, 148, 149, 152, 145]),
        )
    }

    #[test]
    fn a_coalesced_timer_late_three_times_refuses_the_cluster_with_the_fix() {
        let error = judge(&cluster(), vec![coalesced(), coalesced(), coalesced()])
            .expect_err("late every time");
        let medians = (Duration::from_millis(146), Duration::from_millis(149));
        assert!(
            matches!(&error, TestkitError::LateTimeouts { earlier, .. }
                if earlier.as_slice() == [medians; 2]),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "cluster 1/2 (127.0.0.1:5540/erpforall) raises its timeouts late in all 3 attempts, \
             250.0 ms apart (earlier: statement_timeout median 146.0 ms, lock_timeout median \
             149.0 ms; statement_timeout median 146.0 ms, lock_timeout median 149.0 ms; last: \
             statement_timeout median 146.0 ms of 2.0 ms, 144.0 ms, 145.0 ms, 145.0 ms, 146.0 ms, \
             147.0 ms, 148.0 ms, 149.0 ms, 150.0 ms, lock_timeout median 149.0 ms of 145.0 ms, \
             146.0 ms, 147.0 ms, 148.0 ms, 149.0 ms, 149.0 ms, 150.0 ms, 151.0 ms, 152.0 ms; at \
             most 10.0 ms allowed): tests that expect a statement blocked \
             behind a lock to end with SQLSTATE 57014 or 55P03 within their grace fail on it. If \
             the host is macOS, start the postmaster from launchd with ProcessType Interactive"
        );
        assert!(!error.to_string().contains("secret"));
        assert_eq!(error.sqlstate(), None);
    }

    #[test]
    fn the_first_on_time_attempt_passes() {
        let check = judge(&cluster(), vec![coalesced(), on_time()]).expect("second on time");
        assert_eq!(check.attempts(), 2);
        assert_eq!(check.statement_median(), Duration::from_millis(2));
        assert_eq!(check.lock_median(), Duration::from_millis(2));
        assert_eq!(
            check.to_string(),
            "statement_timeout 2.0 ms late, lock_timeout 2.0 ms late (medians of 9 and 9 samples, \
             at most 10.0 ms allowed; attempt 2 of 3, the earlier 1 late)"
        );
        let check =
            judge(&cluster(), vec![coalesced(), coalesced(), on_time()]).expect("third on time");
        assert_eq!(check.attempts(), 3);
        assert!(
            check
                .to_string()
                .ends_with("; attempt 3 of 3, the earlier 2 late)"),
            "{check}"
        );
    }

    #[test]
    fn the_check_measures_again_only_after_a_late_attempt_and_at_most_three_times() {
        assert!(measure_again(&[]));
        assert!(!measure_again(&[on_time()]));
        assert!(measure_again(&[coalesced()]));
        assert!(!measure_again(&[coalesced(), on_time()]));
        assert!(measure_again(&[coalesced(), coalesced()]));
        assert!(!measure_again(&[coalesced(), coalesced(), coalesced()]));
        assert_eq!(REMEASURE_PAUSE, Duration::from_millis(250));
    }

    #[test]
    fn the_median_decides_and_the_limit_itself_passes() {
        // one late outlier (a busy host) does not make an attempt late; the limit is inclusive
        let check = judge(&cluster(), vec![on_time()]).expect("on time");
        assert_eq!(check.attempts(), 1);
        assert_eq!(
            check.statement_samples(),
            ms(&[1, 1, 2, 2, 2, 3, 4, 10, 400]).as_slice()
        );
        assert_eq!(
            check.lock_samples(),
            ms(&[1, 1, 1, 2, 2, 2, 2, 3, 3]).as_slice()
        );
        assert_eq!(
            check.to_string(),
            "statement_timeout 2.0 ms late, lock_timeout 2.0 ms late (medians of 9 and 9 samples, \
             at most 10.0 ms allowed)"
        );
        assert!(!Measured::new(ms(&[10, 10, 10]), ms(&[10])).late());
        // five late statement samples of nine make it late, whatever the four others say
        assert!(Measured::new(ms(&[0, 0, 0, 0, 11, 11, 11, 11, 11]), ms(&[0])).late());
        // a late lock median alone makes it late: the lock-wait path is checked on its own
        assert!(Measured::new(ms(&[1, 1, 1]), ms(&[1, 12, 30])).late());
    }

    #[test]
    fn late_attempts_short_of_three_are_an_error_not_a_pass() {
        for attempts in [vec![], vec![coalesced()], vec![coalesced(), coalesced()]] {
            let late = attempts.len();
            let error = judge(&cluster(), attempts).expect_err("must be measured again");
            assert_eq!(
                error.to_string(),
                format!(
                    "configuration error: the timeout check on cluster 1/2 \
                     (127.0.0.1:5540/erpforall) stopped after {late} late attempts of 3"
                )
            );
        }
    }

    #[test]
    fn a_cancel_before_the_timeout_was_due_voids_the_check() {
        let error = on_time_or_early(&cluster(), Kind::Lock, Duration::from_millis(5))
            .expect_err("5 ms is before the 20 ms timeout");
        assert_eq!(
            error.to_string(),
            "cluster 1/2 (127.0.0.1:5540/erpforall): a statement armed with lock_timeout = 20.0 ms \
             was cancelled after 5.0 ms, before its timeout was due: something else cancelled it \
             (pg_cancel_backend, a pooler), so the measurement is void"
        );
        assert!(on_time_or_early(&cluster(), Kind::Statement, ARMED).is_ok());
    }

    #[test]
    fn lateness_subtracts_the_baseline_and_saturates_on_a_fast_host() {
        let base = Duration::from_micros(300);
        assert_eq!(
            lateness(Duration::from_micros(23_300), base),
            Duration::from_millis(3)
        );
        // the cancel answer came back faster than the baseline allows: zero, never negative
        assert_eq!(lateness(Duration::from_millis(20), base), Duration::ZERO);
    }

    #[test]
    fn the_other_errors_name_the_setting_and_the_step() {
        let not_raised = TestkitError::TimeoutNotRaised {
            cluster: cluster().to_string(),
            setting: Kind::Statement.setting(),
            waited: Duration::from_millis(1002),
        };
        assert_eq!(
            not_raised.to_string(),
            "cluster 1/2 (127.0.0.1:5540/erpforall) did not cancel a statement at its \
             statement_timeout of 20.0 ms; the statement finished after 1002.0 ms: its server \
             timeouts do not work, and tests that rely on them cannot pass"
        );
        let stalled = TestkitError::TimeoutCheckStalled {
            cluster: cluster().to_string(),
            step: "arming lock_timeout (SET)",
            waited: STEP_BOUND,
        };
        assert_eq!(
            stalled.to_string(),
            "no answer within 2000.0 ms while arming lock_timeout (SET) on cluster 1/2 \
             (127.0.0.1:5540/erpforall): the connection stalled (an ssh tunnel that stopped \
             forwarding) or the server did not respond; this says nothing about its timers"
        );
    }
}
