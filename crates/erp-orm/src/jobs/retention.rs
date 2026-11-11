//! The queue's own upkeep as core schedules (6, D14): history older than
//! `jobs.history_days` is trimmed, and dead letters older than `jobs.dead_retention_days` move to
//! history as `dead_expired`, in batches of at most 10,000 rows per transaction.

use std::time::Duration;

use erp_fields::{Error, Result};

use super::{
    ChunkResult, HandlerRef, HistoryPolicy, JobContext, JobError, JobHandler, JobKey, JobKindDecl,
    PrincipalPolicy, RecurringSpec, RunOutcome, TimeoutClass, When,
};
use crate::principal::{PrincipalKind, SUPERUSER_ID};
use crate::{BoxFuture, Orm, Transaction};

/// Most rows one retention transaction removes.
pub const RETENTION_BATCH: u32 = 10_000;

/// A fixed-interval schedule the core keeps (6).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoreSchedule {
    pub kind: &'static str,
    pub interval: Duration,
}

/// The schedules [`ensure_core_schedules`] keeps.
pub(crate) const CORE_SCHEDULES: &[CoreSchedule] = &[
    CoreSchedule {
        kind: "core.jobs_history_retention",
        interval: Duration::from_secs(3_600),
    },
    CoreSchedule {
        kind: "core.jobs_dead_expiry",
        interval: Duration::from_secs(3_600),
    },
];

struct HistoryRetention;
struct DeadExpiry;

fn interval_of(kind: &str) -> Duration {
    CORE_SCHEDULES
        .iter()
        .find(|s| s.kind == kind)
        .map_or(Duration::from_secs(3_600), |s| s.interval)
}

impl JobHandler for HistoryRetention {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let days = ctx.inner.orm.job_settings().history_days();
            loop {
                let report = ctx
                    .chunk(move |tx| {
                        Box::pin(async move {
                            let purged = tx.job_purge_history(days, RETENTION_BATCH).await?;
                            Ok(ChunkResult::more(
                                serde_json::json!({ "purged": purged }),
                                purged,
                                None,
                            ))
                        })
                    })
                    .await?;
                if report.processed < u64::from(RETENTION_BATCH) || ctx.budget_exhausted() {
                    break;
                }
            }
            Ok(RunOutcome::RescheduleAfter(interval_of(
                "core.jobs_history_retention",
            )))
        })
    }
}

impl JobHandler for DeadExpiry {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let days = ctx.inner.orm.job_settings().dead_retention_days();
            loop {
                let report = ctx
                    .chunk(move |tx| {
                        Box::pin(async move {
                            let expired = tx.job_expire_dead(days, RETENTION_BATCH).await?;
                            if expired > 0 {
                                tracing::warn!(
                                    target: "erp::jobs",
                                    expired,
                                    days,
                                    "dead-letter jobs passed jobs.dead_retention_days and moved to history as dead_expired"
                                );
                            }
                            Ok(ChunkResult::more(
                                serde_json::json!({ "expired": expired }),
                                expired,
                                None,
                            ))
                        })
                    })
                    .await?;
                if report.processed < u64::from(RETENTION_BATCH) || ctx.budget_exhausted() {
                    break;
                }
            }
            Ok(RunOutcome::RescheduleAfter(interval_of(
                "core.jobs_dead_expiry",
            )))
        })
    }
}

static HISTORY_RETENTION: HistoryRetention = HistoryRetention;
static DEAD_EXPIRY: DeadExpiry = DeadExpiry;

/// The declaration of `core.jobs_history_retention`. `erp-orm` cannot register it itself (the
/// workspace allows `linkme` registrations only in the crates above it); `erp-security`
/// registers both core kinds with `register_job_kind!` and lists them for
/// [`OrmBuilder::require_job_kinds`](crate::OrmBuilder::require_job_kinds).
pub const fn history_retention_decl() -> JobKindDecl {
    JobKindDecl::new(
        "core",
        "core.jobs_history_retention",
        HandlerRef::Native(&HISTORY_RETENTION),
    )
    .principal(PrincipalPolicy::System)
    .history(HistoryPolicy::None)
    .timeouts(TimeoutClass::Maintenance)
}

/// The declaration of `core.jobs_dead_expiry` (see [`history_retention_decl`]).
pub const fn dead_expiry_decl() -> JobKindDecl {
    JobKindDecl::new(
        "core",
        "core.jobs_dead_expiry",
        HandlerRef::Native(&DEAD_EXPIRY),
    )
    .principal(PrincipalPolicy::System)
    .history(HistoryPolicy::None)
    .timeouts(TimeoutClass::Maintenance)
}

const PURGE_HISTORY_SQL: &str = "\
DELETE FROM erp_job_run WHERE ctid IN (
   SELECT ctid FROM erp_job_run
    WHERE finished_at < clock_timestamp() - make_interval(days => $1::int4)
    LIMIT $2::int8)";

/// Item failures whose job is neither queued nor in history any more (a job that kept no history,
/// or whose history was just purged).
const PURGE_ORPHAN_FAILURES_SQL: &str = "\
DELETE FROM erp_job_item_failure WHERE ctid IN (
   SELECT f.ctid FROM erp_job_item_failure f
    WHERE NOT EXISTS (SELECT 1 FROM erp_job j WHERE j.job_uuid = f.job_uuid)
      AND NOT EXISTS (SELECT 1 FROM erp_job_run r WHERE r.job_uuid = f.job_uuid)
    LIMIT $1::int8)";

const EXPIRE_DEAD_SQL: &str = "\
WITH gone AS (
  DELETE FROM erp_job WHERE id IN (
     SELECT id FROM erp_job
      WHERE state = 'dead' AND dead_at < clock_timestamp() - make_interval(days => $1::int4)
      ORDER BY dead_at LIMIT $2::int8 FOR UPDATE SKIP LOCKED)
  RETURNING job_uuid, kind, kind_version, attempts, enqueued_at, started_at, items_done,
            items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id,
            last_error_code),
failures AS (
  DELETE FROM erp_job_item_failure f USING gone WHERE f.job_uuid = gone.job_uuid RETURNING 1),
history AS (
  INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, started_at,
                           items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user,
                           correlation_id, last_error_code)
  SELECT job_uuid, kind, kind_version, 'dead_expired', attempts, enqueued_at, started_at,
         items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id,
         last_error_code
    FROM gone
  RETURNING 1)
SELECT count(*)::int8 FROM history";

impl Transaction {
    fn require_system_for(&self, call: &str) -> Result<()> {
        if self.env().principal().kind() == PrincipalKind::System {
            Ok(())
        } else {
            Err(Error::access(format!(
                "{call} is the queue's own upkeep and is for the system principal"
            )))
        }
    }

    /// Remove up to `batch` history rows finished more than `days` days ago; returns how many.
    ///
    /// # Errors
    ///
    /// `Access` unless the caller is the system principal; the database's errors.
    pub async fn job_purge_history(&mut self, days: u32, batch: u32) -> Result<u64> {
        self.require_system_for("job_purge_history")?;
        let days = i32::try_from(days).unwrap_or(i32::MAX);
        let batch = i64::from(batch.min(RETENTION_BATCH));
        let removed = self.execute(PURGE_HISTORY_SQL, &[&days, &batch]).await?;
        // the failure rows that went with those histories (and any a history-less kind left)
        self.execute(PURGE_ORPHAN_FAILURES_SQL, &[&batch]).await?;
        Ok(removed)
    }

    /// Move up to `batch` dead letters older than `days` days to history as `dead_expired`;
    /// returns how many (each leaves a history row, nothing is dropped silently).
    ///
    /// # Errors
    ///
    /// `Access` unless the caller is the system principal; the database's errors.
    pub async fn job_expire_dead(&mut self, days: u32, batch: u32) -> Result<u64> {
        self.require_system_for("job_expire_dead")?;
        let days = i32::try_from(days).unwrap_or(i32::MAX);
        let batch = i64::from(batch.min(RETENTION_BATCH));
        let rows = self.query(EXPIRE_DEAD_SQL, &[&days, &batch]).await?;
        let count: i64 = rows
            .first()
            .ok_or_else(|| Error::internal("dead-letter expiry returned no count"))?
            .try_get(0)
            .map_err(|e| Error::internal("the expiry count did not decode").with_source(e))?;
        u64::try_from(count).map_err(|_| Error::internal("a negative expiry count"))
    }
}

/// Make sure every core schedule has its recurring row (idempotent; a row that exists is kept
/// as it is). Called when a runner starts.
///
/// # Errors
///
/// The database's errors.
pub async fn ensure_core_schedules(orm: &Orm) -> Result<()> {
    let registered = super::enqueue::kind_map();
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    let env = tx.env();
    let work = async {
        for schedule in CORE_SCHEDULES {
            if !registered.contains_key(schedule.kind) {
                continue;
            }
            let exists = tx
                .query(
                    "SELECT 1 FROM erp_job WHERE kind = $1 AND dedup_key = $1 AND state <> 'dead'",
                    &[&schedule.kind],
                )
                .await?;
            if !exists.is_empty() {
                continue;
            }
            let spec = RecurringSpec::new(schedule.kind, JobKey::new(schedule.kind)?)
                .first_run(When::After(schedule.interval));
            tx.job_upsert_recurring(&env, spec).await?;
        }
        Ok::<(), Error>(())
    }
    .await;
    match work {
        Ok(()) => tx.commit().await,
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}
