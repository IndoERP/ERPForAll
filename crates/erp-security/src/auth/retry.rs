//! The one retry rule of authentication's short transactions (the database review's isolation
//! and retry matrix; rust review round 2 S-5): run the work again when it fails with one of the
//! SQLSTATEs the caller names, at most [`DEADLOCK_TRIES`] times in all, sleeping 5 – 25 ms
//! between tries. Every other outcome, and the last conflict, is returned as is. Each conflict a
//! retry absorbs is counted in its step's counter ([`DeadlockRetries`], rust review round 3
//! RD-5) and logged at `warn` with its step, at most once a second per step (round 4 C4-4), so a
//! run of absorbed deadlocks shows without one line per conflict.
//!
//! Every short write transaction of authentication runs READ COMMITTED and names [`DEADLOCK`]
//! only (no serialization failure happens there). The steps that retry (database review round
//! 3 DD-1, rust review round 3 RD-6): the login's phase B, both the failure count and the
//! success statement (one statement each: a 40P01 rolls all of it back, so a rerun counts once
//! and writes one session); `rotate`; `touch`; `logout`; the deletion after a refused session
//! check; the sessions a login ends; each collection batch. Read-only steps take no row lock and
//! do not retry: the login's phase A and the settings reload.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use erp_fields::Result;

use super::warn::WarnLimit;

/// Deadlocks a retry absorbed (the work then ran again), by the step that met them (monotonic;
/// [`crate::auth::AuthStats::deadlock_retries`]). A deadlock after the last try is not absorbed:
/// it is returned, and counted where the step counts its failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct DeadlockRetries {
    /// The failure count of a login (phase B of a refusal).
    pub failure_write: u64,
    /// The success statement of a login (phase B of a success).
    pub success: u64,
    /// `Auth::rotate`.
    pub rotate: u64,
    /// `Auth::touch`.
    pub touch: u64,
    /// `Auth::logout`.
    pub logout: u64,
    /// The deletion of a session after a refused check.
    pub refusal_delete: u64,
    /// The sessions a login ends (over the cap, or the others after a rehash).
    pub end_others: u64,
    /// A collection batch.
    pub gc: u64,
}

/// One retried step: the conflicts it absorbed (all counted) and the rate limit of its `warn`
/// line (at most one a second per step, round 4 C4-4).
#[derive(Debug, Default)]
pub(crate) struct RetryStep {
    absorbed: AtomicU64,
    warn: WarnLimit,
}

impl RetryStep {
    /// The conflicts this step absorbed so far.
    pub(crate) fn absorbed(&self) -> u64 {
        self.absorbed.load(Ordering::Relaxed)
    }
}

/// The steps behind [`DeadlockRetries`].
#[derive(Debug, Default)]
pub(crate) struct RetryCounters {
    pub(crate) failure_write: RetryStep,
    pub(crate) success: RetryStep,
    pub(crate) rotate: RetryStep,
    pub(crate) touch: RetryStep,
    pub(crate) logout: RetryStep,
    pub(crate) refusal_delete: RetryStep,
    pub(crate) end_others: RetryStep,
    pub(crate) gc: RetryStep,
}

impl RetryCounters {
    pub(crate) fn read(&self) -> DeadlockRetries {
        DeadlockRetries {
            failure_write: self.failure_write.absorbed(),
            success: self.success.absorbed(),
            rotate: self.rotate.absorbed(),
            touch: self.touch.absorbed(),
            logout: self.logout.absorbed(),
            refusal_delete: self.refusal_delete.absorbed(),
            end_others: self.end_others.absorbed(),
            gc: self.gc.absorbed(),
        }
    }
}

/// Tries of one piece of work that meets a retried conflict, the first included.
pub const DEADLOCK_TRIES: u32 = 3;

/// A deadlock (40P01): what READ COMMITTED work retries.
pub const DEADLOCK: &[&str] = &["40P01"];

/// Run `work` until it does not fail with a SQLSTATE of `retried`, at most [`DEADLOCK_TRIES`]
/// times. Each conflict absorbed (the work runs again) is counted in `step`; the first of each
/// second is logged at `warn` naming `name` and the step's count so far (round 4 C4-4: a run of
/// deadlocks writes one line a second per step, not one per conflict). `work` makes the future of one try (a closure returning a future, not
/// an async closure: the future of an async closure is not provably `Send` for every lifetime,
/// and the callers' futures must be, for `tokio::spawn`).
///
/// # Errors
///
/// The work's error: a conflict after the last try, or any other error at once.
pub(crate) async fn retry<T, F, Fut>(
    retried: &[&str],
    step: &RetryStep,
    name: &str,
    mut work: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut tries = 1;
    loop {
        match work().await {
            Err(e)
                if e.sqlstate().is_some_and(|code| retried.contains(&code))
                    && tries < DEADLOCK_TRIES =>
            {
                let total = step.absorbed.fetch_add(1, Ordering::Relaxed) + 1;
                let wait = backoff();
                if step.warn.admit() {
                    tracing::warn!(
                        target: "erp::auth",
                        "{name}: conflict on try {tries} of {DEADLOCK_TRIES}, running it again in \
                         {} ms: {e} ({total} absorbed by this step so far; this line comes at most \
                         once a second per step)",
                        wait.as_millis()
                    );
                }
                tries += 1;
                tokio::time::sleep(wait).await;
            }
            other => return other,
        }
    }
}

/// 5 – 25 ms, jittered from the clock: the jitter only spreads retries apart, it has no
/// cryptographic use, so it cannot fail (a clock before 1970 gives the plain 5 ms).
fn backoff() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    Duration::from_millis(5 + u64::from(nanos % 21))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, reason = "tests")]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use erp_fields::Error;

    use super::super::warn::capture;
    use super::*;

    fn with_state(code: &str) -> Error {
        Error::db("conflict").with_sqlstate(Some(code.to_owned()))
    }

    /// A retried conflict runs the work [`DEADLOCK_TRIES`] times, then returns the last one; the
    /// conflicts absorbed (all but the last) are counted (rust review round 3 RD-5).
    #[tokio::test]
    async fn conflicts_are_retried_three_times_at_most() {
        let calls = AtomicU32::new(0);
        let step = RetryStep::default();
        let e = retry(DEADLOCK, &step, "test", || async {
            calls.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(with_state("40P01"))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), DEADLOCK_TRIES);
        assert_eq!(e.sqlstate(), Some("40P01"));
        assert_eq!(step.absorbed(), u64::from(DEADLOCK_TRIES - 1));
    }

    /// A serialization failure is retried only when named; any other error returns at once and
    /// counts nothing; one absorbed conflict counts one.
    #[tokio::test]
    async fn only_named_states_are_retried() {
        let calls = AtomicU32::new(0);
        let step = RetryStep::default();
        let e = retry(DEADLOCK, &step, "test", || async {
            calls.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(with_state("40001"))
        })
        .await
        .unwrap_err();
        assert_eq!(
            (calls.load(Ordering::Relaxed), e.sqlstate()),
            (1, Some("40001"))
        );
        assert_eq!(step.absorbed(), 0);
        let calls = AtomicU32::new(0);
        let ok = retry(&["40P01", "40001"], &step, "test", || async {
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(with_state("40001"))
            } else {
                Ok::<u8, Error>(7)
            }
        })
        .await
        .unwrap();
        assert_eq!((calls.load(Ordering::Relaxed), ok), (2, 7));
        assert_eq!(step.absorbed(), 1);
    }

    /// Work that meets `conflicts` deadlocks, then succeeds.
    async fn absorb(step: &RetryStep, name: &str, conflicts: u32) {
        let calls = AtomicU32::new(0);
        retry(DEADLOCK, step, name, || async {
            if calls.fetch_add(1, Ordering::Relaxed) < conflicts {
                Err(with_state("40P01"))
            } else {
                Ok(())
            }
        })
        .await
        .unwrap();
    }

    /// Round 4 C4-4: the retry's `warn` line comes at most once a second per step, and every
    /// conflict is still counted. Four conflicts of one step in one second (two runs of two)
    /// write one line, the first, carrying the count at that point; a second step in the same
    /// second writes its own line. The counts are exact: 4 and 1.
    #[tokio::test]
    async fn the_retry_warning_comes_once_a_second_per_step() {
        let started = std::time::Instant::now();
        let (lines, _guard) = capture::start();
        let (first, second) = (RetryStep::default(), RetryStep::default());
        absorb(&first, "first step", 2).await;
        absorb(&first, "first step", 2).await;
        absorb(&second, "second step", 1).await;
        // the limit is per second: the steps above must share one (five backoffs, at most 125 ms)
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the host stalled for a second; the case needs the conflicts in one second"
        );
        let lines: Vec<String> = lines
            .take()
            .into_iter()
            .map(|line| strip_wait(&line))
            .collect();
        assert_eq!(
            lines,
            [
                "first step: conflict on try 1 of 3, running it again in N ms: conflict (1 absorbed \
                 by this step so far; this line comes at most once a second per step)",
                "second step: conflict on try 1 of 3, running it again in N ms: conflict (1 \
                 absorbed by this step so far; this line comes at most once a second per step)",
            ]
        );
        assert_eq!((first.absorbed(), second.absorbed()), (4, 1));
    }

    /// `line` with its jittered wait (5 – 25 ms) replaced by `N`.
    fn strip_wait(line: &str) -> String {
        let (head, rest) = line.split_once(" again in ").unwrap();
        let (ms, tail) = rest.split_once(" ms: ").unwrap();
        let ms: u64 = ms.parse().unwrap();
        assert!((5..=25).contains(&ms), "{line}");
        format!("{head} again in N ms: {tail}")
    }

    #[test]
    fn the_wait_is_five_to_twenty_five_ms() {
        let wait = backoff();
        assert!((5..=25).contains(&wait.as_millis()), "{wait:?}");
    }
}
