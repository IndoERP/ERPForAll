//! Durable coordination of the interim authentication collector.
//!
//! The server owns the interval task and supplies its current published [`Auth`] on each tick.
//! One logged singleton stores the next due time, sweep cursor and expiring run token. Every
//! scheduled GC batch checks and locks that token in its own transaction, fencing a process
//! that lost its lease. There is no permanent scheduler leader or pooled session lock.

use std::{fmt, time::Duration};

use deadpool_postgres::{ClientWrapper, Object, Pool};
use erp_fields::{Error, ErrorKind, Result};
use erp_orm::Transaction;

use super::{
    Auth,
    gc::{GcCursor, GcReport},
};

const INTERVAL: Duration = Duration::from_secs(300);
const DEADLINE: Duration = Duration::from_secs(30);
const CONTROL_DEADLINE: Duration = Duration::from_secs(5);
const LEASE_GRACE: Duration = Duration::from_secs(6);
const MISSING: &str =
    "authentication collection state is missing; run schema sync before serving login";
const LOST: &str = "authentication collection lease expired or was superseded";
// Called inside the same fenced transaction as the idle batch's effects. Finalization never
// writes this column: the in-memory report can lag a COMMIT whose response was cancelled.
const CHECKPOINT_SQL: &str = "UPDATE erp_auth_gc_state SET page=$1 \
    WHERE singleton AND lease_token=$2 AND lease_until > clock_timestamp()";
const IMPLAUSIBLE_LEASE: &str = "authentication collection lease is implausibly far in the future";
const IMPLAUSIBLE_DUE: &str = "authentication collection due time is implausibly far in the future";

/// The outcome of a scheduled attempt. Busy or not-due attempts did not run collection.
#[derive(Debug)]
#[non_exhaustive]
pub enum GcRun {
    /// The collector returned its report and completed cursor progress is durable.
    /// A report with `stopped_by_lock` did not complete this database-wide interval.
    Collected(GcReport),
    /// Another call on this instance is using its control connection, or another process
    /// holds an unexpired run lease or the fence of a still-finishing batch.
    Busy,
    /// A successful, uncontended collection established a database-wide interval that has
    /// not elapsed. Failed and lock-stopped collections leave the database due for retry.
    NotDue,
}

/// Durable scheduling of one database's authentication collector.
///
/// The pool must connect to the same primary database as the [`Auth`] passed to [`run`](Self::run),
/// using a role with SELECT and UPDATE on `erp_auth_gc_state`. Give it a dedicated pool of size
/// one so control work cannot consume the request pool. `probe` is required before login becomes
/// available. A server polls more often than the interval, so another replica can take over after
/// a failed process's lease expires. will replace the interim interval task.
#[non_exhaustive]
pub struct GcSchedule {
    pool: Pool,
    interval: Duration,
    deadline: Duration,
    // Taking a pool object out permanently frees its pool slot. This admission guard must
    // therefore cover the detached control client's whole lifetime, not just pool.get().
    control: tokio::sync::Mutex<()>,
}
struct ActiveRun {
    client: ClientWrapper,
    token: i64,
    report: GcReport,
}
enum Attempt {
    Active(ActiveRun),
    Skipped(GcRun),
}
enum ScheduleState {
    Busy,
    NotDue,
    Due,
}
impl fmt::Debug for GcSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcSchedule")
            .field("interval", &self.interval)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}
impl GcSchedule {
    /// Coordinate a collector every five minutes, with a 30-second run deadline.
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            interval: INTERVAL,
            deadline: DEADLINE,
            control: tokio::sync::Mutex::new(()),
        }
    }

    /// Verify state and least-privilege access before exposing login.
    ///
    /// # Errors
    /// Database when schema or privileges are unavailable; Programming when the singleton is
    /// absent, its cursor/token is invalid or a lease/due time is implausibly far ahead.
    /// ResourceExhausted when another call already owns
    /// this instance's control connection. Errors contain no connection configuration.
    pub async fn probe(&self) -> Result<()> {
        let _control = self.control.try_lock().map_err(|_| {
            Error::new(
                ErrorKind::ResourceExhausted,
                "authentication collection control is busy",
            )
        })?;
        let client = self.connection().await?;
        // Probe validates state and effective grants; only run uses its scheduling outcome.
        self.state(&client).await.map(|_| ())
    }

    /// Attempt the due run with the current registry's authentication service.
    ///
    /// # Errors
    /// The original collection error; ResourceExhausted on the run deadline; database/control
    /// errors; Concurrency if this run lost its fencing token. Cursor progress commits with each
    /// idle batch, so cancellation cannot discard earlier committed work. A finalization error
    /// is returned even when collection also failed. A cancellation
    /// releases no successor's lease: this run's lease expires on the primary's clock.
    pub async fn run(&self, auth: &Auth) -> Result<GcRun> {
        let _control = match self.control.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(GcRun::Busy),
        };
        let mut active = match self.begin().await? {
            Attempt::Active(active) => active,
            Attempt::Skipped(outcome) => return Ok(outcome),
        };
        let collected = match tokio::time::timeout(
            self.deadline,
            auth.run_gc(&mut active.report, Some(active.token)),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(run_deadline()),
        };
        self.finish(active, collected).await
    }

    async fn connection(&self) -> Result<ClientWrapper> {
        let object = tokio::time::timeout(CONTROL_DEADLINE, self.pool.get())
            .await
            .map_err(|_| {
                Error::new(
                    ErrorKind::ResourceExhausted,
                    "authentication collection control connection deadline exceeded",
                )
            })?
            .map_err(|error| {
                Error::db("could not get authentication collection control connection")
                    .with_source(error)
            })?;
        // Detached permanently: cancellation or a timed-out protocol never returns this
        // connection, or the timeout/application_name settings below, to the pool.
        let client = Object::take(object);
        control(client.batch_execute(
            "SET statement_timeout = '5s'; SET lock_timeout = '1s'; SET application_name = 'erp:auth-gc'"
        )).await?;
        Ok(client)
    }

    fn lease_secs(&self) -> Result<f64> {
        self.deadline
            .checked_add(LEASE_GRACE)
            .map(|duration| duration.as_secs_f64())
            .ok_or_else(|| {
                Error::programming("authentication collection lease duration overflowed")
            })
    }

    async fn state(&self, client: &ClientWrapper) -> Result<ScheduleState> {
        let lease_secs = self.lease_secs()?;
        let interval_secs = self.interval.as_secs_f64();
        let row = control(client.query_opt(
            "SELECT page, lease_token, lease_until > clock_timestamp(), \
             next_run_at > clock_timestamp(), \
             lease_until > clock_timestamp()+make_interval(secs => $1), \
             next_run_at > clock_timestamp()+make_interval(secs => $2), \
             has_table_privilege(current_user, 'erp_auth_gc_state', 'SELECT') \
             AND has_table_privilege(current_user, 'erp_auth_gc_state', 'UPDATE') \
             FROM erp_auth_gc_state WHERE singleton",
            &[&lease_secs, &interval_secs],
        ))
        .await?
        .ok_or_else(|| Error::programming(MISSING))?;
        let page: i64 = row.try_get(0).map_err(state_error)?;
        cursor(page)?;
        let token: i64 = row.try_get(1).map_err(state_error)?;
        if token < 0 || token == i64::MAX {
            return Err(Error::programming(
                "authentication collection lease token is invalid or exhausted",
            ));
        }
        let bad_lease: bool = row.try_get(4).map_err(state_error)?;
        let bad_due: bool = row.try_get(5).map_err(state_error)?;
        if bad_lease {
            return Err(Error::programming(IMPLAUSIBLE_LEASE));
        }
        if bad_due {
            return Err(Error::programming(IMPLAUSIBLE_DUE));
        }
        let allowed: bool = row.try_get(6).map_err(state_error)?;
        if !allowed {
            return Err(Error::new(
                ErrorKind::Access,
                "authentication collection needs SELECT and UPDATE on erp_auth_gc_state before serving login",
            ));
        }
        let busy: bool = row.try_get(2).map_err(state_error)?;
        let not_due: bool = row.try_get(3).map_err(state_error)?;
        Ok(if busy {
            ScheduleState::Busy
        } else if not_due {
            ScheduleState::NotDue
        } else {
            ScheduleState::Due
        })
    }

    async fn begin(&self) -> Result<Attempt> {
        let client = self.connection().await?;
        let lease_secs = self.lease_secs()?;
        // A still-finishing batch can retain this row after its lease expires. Acquire the
        // row without waiting, so that replica cannot turn a healthy peer's tick into a
        // lock-timeout failure. The candidate lock and epoch increment share one statement.
        let acquired = control(client.query_opt(
            "WITH candidate AS MATERIALIZED ( \
                 SELECT singleton FROM erp_auth_gc_state WHERE singleton \
                 FOR UPDATE SKIP LOCKED \
             ) \
             UPDATE erp_auth_gc_state AS state SET lease_token=state.lease_token+1, \
             lease_until=clock_timestamp()+make_interval(secs => $1) \
             FROM candidate WHERE state.singleton=candidate.singleton \
             AND state.next_run_at <= clock_timestamp() \
             AND state.lease_until <= clock_timestamp() \
             AND state.lease_token < 9223372036854775807 \
             RETURNING state.page, state.lease_token",
            &[&lease_secs],
        ))
        .await?;
        let Some(row) = acquired else {
            return Ok(Attempt::Skipped(match self.state(&client).await? {
                ScheduleState::Busy => GcRun::Busy,
                ScheduleState::NotDue => GcRun::NotDue,
                // A row still due was skipped because a batch holds its fence, or because
                // acquisition raced a completed run. Retry on a later tick without spinning.
                ScheduleState::Due => GcRun::Busy,
            }));
        };
        let page: i64 = row.try_get(0).map_err(state_error)?;
        let token: i64 = row.try_get(1).map_err(state_error)?;
        let report = GcReport {
            next: cursor(page)?,
            ..GcReport::default()
        };
        Ok(Attempt::Active(ActiveRun {
            client,
            token,
            report,
        }))
    }

    async fn finish(&self, active: ActiveRun, collected: Result<()>) -> Result<GcRun> {
        let ActiveRun {
            client,
            token,
            report,
        } = active;
        let interval_secs = self.interval.as_secs_f64();
        // A future due time is proof of a completed interval for other replicas. A failed or
        // advisory-lock-stopped collector must remain due, allowing an actual retry to heal it.
        let completed = collected.is_ok() && report.stopped_by_lock.is_none();
        let saved = control(client.execute(
            "UPDATE erp_auth_gc_state SET \
             next_run_at=CASE WHEN $3 THEN clock_timestamp()+make_interval(secs => $1) \
                 ELSE next_run_at END, lease_until='-infinity' \
             WHERE singleton AND lease_token=$2 AND lease_until > clock_timestamp()",
            &[&interval_secs, &token, &completed],
        ))
        .await
        .and_then(|rows| {
            if rows == 1 {
                Ok(())
            } else {
                Err(Error::new(ErrorKind::Concurrency, LOST))
            }
        });
        match (collected, saved) {
            (Ok(()), Ok(())) => Ok(GcRun::Collected(report)),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(save)) => {
                // and_also preserves the primary source. Keep the independent finalization
                // failure visible without logging either error's potentially sensitive text.
                tracing::warn!(kind=?save.kind(), sqlstate=secondary_sqlstate(&save),
                    "authentication collection finalization failed after collector failure");
                Err(error.and_also(&save))
            }
        }
    }

    #[cfg(test)]
    async fn run_with(
        &self,
        work: &impl AsyncFn(&mut GcReport, i64) -> Result<()>,
    ) -> Result<GcRun> {
        let _control = match self.control.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(GcRun::Busy),
        };
        let mut active = match self.begin().await? {
            Attempt::Active(active) => active,
            Attempt::Skipped(outcome) => return Ok(outcome),
        };
        let collected =
            match tokio::time::timeout(self.deadline, work(&mut active.report, active.token)).await
            {
                Ok(result) => result,
                Err(_) => Err(run_deadline()),
            };
        self.finish(active, collected).await
    }

    #[cfg(test)]
    fn with_timing_for_tests(mut self, interval: Duration, deadline: Duration) -> Self {
        self.interval = interval;
        self.deadline = deadline;
        self
    }
}

fn run_deadline() -> Error {
    Error::new(
        ErrorKind::ResourceExhausted,
        "authentication collection run exceeded its deadline",
    )
}
async fn control<T>(
    future: impl std::future::Future<Output = std::result::Result<T, tokio_postgres::Error>>,
) -> Result<T> {
    tokio::time::timeout(CONTROL_DEADLINE, future)
        .await
        .map_err(|_| {
            Error::new(
                ErrorKind::ResourceExhausted,
                "authentication collection control deadline exceeded",
            )
        })?
        .map_err(|error| {
            Error::db("authentication collection control statement failed").with_source(error)
        })
}
fn state_error(error: tokio_postgres::Error) -> Error {
    Error::db("authentication collection state has an invalid column").with_source(error)
}
fn cursor(page: i64) -> Result<GcCursor> {
    u32::try_from(page).map(GcCursor::at).map_err(|_| {
        Error::programming("authentication collection cursor is outside PostgreSQL's block range")
    })
}

/// Persist only a batch that will commit its effects in this transaction. The opening fence
/// already holds the row lock; this check also refuses an epoch that expired during the work.
pub(super) async fn checkpoint(tx: &mut Transaction, token: i64, next: GcCursor) -> Result<()> {
    let page = i64::from(next.page());
    let rows = tx.execute(CHECKPOINT_SQL, &[&page, &token]).await?;
    if rows == 1 {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::Concurrency, LOST))
    }
}

fn secondary_sqlstate(error: &Error) -> &str {
    let code = std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<tokio_postgres::Error>())
        .and_then(|source| source.code())
        .map(|code| code.code());
    bounded_sqlstate(code)
}
fn bounded_sqlstate(code: Option<&str>) -> &str {
    match code {
        Some(code)
            if code.len() == 5
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()) =>
        {
            code
        }
        Some(_) => "invalid", // diagnostics stay explicit, but arbitrary text never reaches a log
        None => "unavailable", // e.g. a client deadline, lost epoch or closed connection
    }
}

/// The row lock is held for the batch transaction. A new run cannot replace this token until
/// that batch commits/rolls back, and a process whose lease expired cannot start another batch.
pub(super) async fn fence(tx: &mut Transaction, token: i64) -> Result<()> {
    let rows = tx
        .query(
            "SELECT lease_token=$1 AND lease_until > clock_timestamp() \
         FROM erp_auth_gc_state WHERE singleton FOR UPDATE",
            &[&token],
        )
        .await?;
    let row = rows.first().ok_or_else(|| Error::programming(MISSING))?;
    let valid: bool = row.try_get(0).map_err(state_error)?;
    if valid {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::Concurrency, LOST))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "regression assertions"
    )]
    use super::*;
    use erp_fields::{Error, ErrorKind};
    use erp_testkit::TestDb;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    async fn fixture() -> (TestDb, deadpool_postgres::Pool) {
        let db = TestDb::create().await.unwrap();
        let c = db.client().await.unwrap();
        c.batch_execute(super::super::schema::GC_STATE_DDL)
            .await
            .unwrap();
        let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
        (db, pool)
    }
    fn error(e: Error, kind: ErrorKind, message: &str) {
        assert_eq!(e.kind(), kind);
        assert_eq!(e.message(), message);
    }

    #[tokio::test]
    async fn collector_cursor_is_durable_after_partial_failure() {
        let (h, pool) = fixture().await;
        let schedule = GcSchedule::new(pool.clone());
        schedule.probe().await.unwrap();
        let c = h.client().await.unwrap();
        let e = schedule
            .run_with(&async |report, token| {
                assert_eq!(report.next.page(), 0);
                report.next = super::super::gc::GcCursor::at(91);
                // The fake collector explicitly commits progress with the production
                // checkpoint statement, as the real idle batches do in their transactions.
                assert_eq!(
                    c.execute(CHECKPOINT_SQL, &[&i64::from(report.next.page()), &token])
                        .await
                        .unwrap(),
                    1
                );
                Err(Error::programming("injected collector failure"))
            })
            .await
            .unwrap_err();
        error(e, ErrorKind::Programming, "injected collector failure");
        let page: i64 = c
            .query_one("SELECT page FROM erp_auth_gc_state WHERE singleton", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(page, 91);
        c.batch_execute("UPDATE erp_auth_gc_state SET next_run_at='-infinity'")
            .await
            .unwrap();
        drop(schedule);
        let restarted = GcSchedule::new(pool);
        let outcome = restarted
            .run_with(&async |report, token| {
                assert_eq!(report.next.page(), 91);
                report.next = super::super::gc::GcCursor::at(92);
                assert_eq!(
                    c.execute(CHECKPOINT_SQL, &[&i64::from(report.next.page()), &token])
                        .await
                        .unwrap(),
                    1
                );
                Ok(())
            })
            .await
            .unwrap();
        match outcome {
            GcRun::Collected(report) => assert_eq!(report.next.page(), 92),
            other => panic!("expected completed collection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn another_collectors_live_lease_prevents_work_and_due_time_is_shared() {
        let (h, pool) = fixture().await;
        let schedule = GcSchedule::new(pool.clone());
        let c = h.client().await.unwrap();
        c.batch_execute(
            "UPDATE erp_auth_gc_state SET lease_until=clock_timestamp()+interval '20 seconds'",
        )
        .await
        .unwrap();
        let work = Arc::new(AtomicUsize::new(0));
        let outcome = schedule
            .run_with(&async |_: &mut GcReport, _token| {
                work.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(outcome, GcRun::Busy));
        assert_eq!(work.load(Ordering::SeqCst), 0);
        c.batch_execute("UPDATE erp_auth_gc_state SET lease_until='-infinity'")
            .await
            .unwrap();
        let outcome = schedule
            .run_with(&async |_: &mut GcReport, _token| {
                work.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(outcome, GcRun::Collected(_)));
        let other = GcSchedule::new(pool);
        let outcome = other
            .run_with(&async |_: &mut GcReport, _token| {
                work.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(outcome, GcRun::NotDue));
        assert_eq!(work.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn deadline_keeps_completed_progress_and_releases_the_run_lease() {
        let (h, pool) = fixture().await;
        let schedule = GcSchedule::new(pool.clone()).with_timing_for_tests(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        let c = h.client().await.unwrap();
        let e = schedule
            .run_with(&async |report, token| {
                report.next = super::super::gc::GcCursor::at(123);
                // Give this real checkpoint query setup allowance before the pending work.
                assert_eq!(
                    c.execute(CHECKPOINT_SQL, &[&i64::from(report.next.page()), &token])
                        .await
                        .unwrap(),
                    1
                );
                std::future::pending::<()>().await;
                Ok(())
            })
            .await
            .unwrap_err();
        error(
            e,
            ErrorKind::ResourceExhausted,
            "authentication collection run exceeded its deadline",
        );
        let page: i64 = c
            .query_one("SELECT page FROM erp_auth_gc_state WHERE singleton", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(page, 123);
        c.batch_execute("UPDATE erp_auth_gc_state SET next_run_at='-infinity'")
            .await
            .unwrap();
        let retry = GcSchedule::new(pool);
        // The prior lease was released before run returned; no sleep or retry hides a leak.
        assert!(matches!(
            retry
                .run_with(&async |_: &mut GcReport, _token| Ok(()))
                .await
                .unwrap(),
            GcRun::Collected(_)
        ));
    }

    #[tokio::test]
    async fn missing_singleton_is_a_clear_startup_failure() {
        let (h, pool) = fixture().await;
        let c = h.client().await.unwrap();
        c.execute("DELETE FROM erp_auth_gc_state", &[])
            .await
            .unwrap();
        let e = GcSchedule::new(pool).probe().await.unwrap_err();
        error(
            e,
            ErrorKind::Programming,
            "authentication collection state is missing; run schema sync before serving login",
        );
    }
    #[tokio::test]
    async fn one_instance_never_opens_another_control_connection_during_a_run() {
        let (_db, pool) = fixture().await;
        let schedule = GcSchedule::new(pool);
        let entered = tokio::sync::Notify::new();
        let release = tokio::sync::Notify::new();
        let work = AtomicUsize::new(0);
        let first_work = async |_: &mut GcReport, _token| {
            entered.notify_one();
            release.notified().await;
            Ok(())
        };
        let (first, (other, probe)) = tokio::join!(schedule.run_with(&first_work), async {
            entered.notified().await;
            let other = schedule
                .run_with(&async |_: &mut GcReport, _token| {
                    work.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await;
            let probe = schedule.probe().await;
            release.notify_one();
            (other, probe)
        });
        assert!(matches!(first.unwrap(), GcRun::Collected(_)));
        assert!(matches!(other.unwrap(), GcRun::Busy));
        assert_eq!(work.load(Ordering::SeqCst), 0);
        error(
            probe.unwrap_err(),
            ErrorKind::ResourceExhausted,
            "authentication collection control is busy",
        );
    }

    #[tokio::test]
    async fn slow_fence_holder_is_busy_without_failing_a_healthy_replica() {
        let (db, pool) = fixture().await;
        let mut holder = db.client().await.unwrap();
        let tx = holder.transaction().await.unwrap();
        tx.query_one(
            "SELECT singleton FROM erp_auth_gc_state WHERE singleton FOR UPDATE",
            &[],
        )
        .await
        .unwrap();
        // The committed state is due and its lease is expired. The live batch still owns the
        // fence, as a backend can after its caller timed out. A peer must not wait on it.
        let schedule = GcSchedule::new(pool);
        let outcome = schedule
            .run_with(&async |_: &mut GcReport, _| {
                panic!("a peer entered collection while another batch held its fence")
            })
            .await;
        tx.rollback().await.unwrap();
        assert!(matches!(outcome.unwrap(), GcRun::Busy));
        assert!(matches!(
            schedule
                .run_with(&async |_: &mut GcReport, _| Ok(()))
                .await
                .unwrap(),
            GcRun::Collected(_)
        ));
    }

    #[tokio::test]
    async fn implausible_future_lease_and_due_times_are_refused_by_probe_and_run() {
        let (db, pool) = fixture().await;
        let c = db.client().await.unwrap();
        let schedule = GcSchedule::new(pool);
        for (sql, message) in [
            (
                "UPDATE erp_auth_gc_state SET lease_until=clock_timestamp()+interval '1 hour'",
                "authentication collection lease is implausibly far in the future",
            ),
            (
                "UPDATE erp_auth_gc_state SET lease_until='-infinity', next_run_at=clock_timestamp()+interval '1 day'",
                "authentication collection due time is implausibly far in the future",
            ),
            (
                "UPDATE erp_auth_gc_state SET lease_until='infinity', next_run_at='-infinity'",
                "authentication collection lease is implausibly far in the future",
            ),
            (
                "UPDATE erp_auth_gc_state SET lease_until='-infinity', next_run_at='infinity'",
                "authentication collection due time is implausibly far in the future",
            ),
        ] {
            c.batch_execute(sql).await.unwrap();
            error(
                schedule.probe().await.unwrap_err(),
                ErrorKind::Programming,
                message,
            );
            error(
                schedule
                    .run_with(&async |_: &mut GcReport, _| {
                        panic!("invalid scheduler state must not run collection")
                    })
                    .await
                    .unwrap_err(),
                ErrorKind::Programming,
                message,
            );
        }
    }

    #[tokio::test]
    async fn failed_collection_does_not_postpone_retry() {
        let (db, pool) = fixture().await;
        let schedule = GcSchedule::new(pool);
        error(
            schedule
                .run_with(&async |_: &mut GcReport, _| {
                    Err(Error::programming("injected collector failure"))
                })
                .await
                .unwrap_err(),
            ErrorKind::Programming,
            "injected collector failure",
        );
        let c = db.client().await.unwrap();
        let due: bool = c
            .query_one(
                "SELECT next_run_at <= clock_timestamp() FROM erp_auth_gc_state WHERE singleton",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            due,
            "a failed collection must not postpone the next attempt as a successful run"
        );
        assert!(matches!(
            schedule
                .run_with(&async |_: &mut GcReport, _| Ok(()))
                .await
                .unwrap(),
            GcRun::Collected(_)
        ));
    }

    #[test]
    fn finalization_diagnostics_accept_only_bounded_sqlstate_codes() {
        for (code, expected) in [
            (Some("55P03"), "55P03"),
            (Some("P0001"), "P0001"),
            (Some("private diagnostic material"), "invalid"),
            (Some("abcde"), "invalid"),
            (None, "unavailable"),
        ] {
            assert_eq!(bounded_sqlstate(code), expected);
        }
    }
}
