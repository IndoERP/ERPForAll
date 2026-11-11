//! How a run leaves the `running` state: one statement guarded by the job's id
//! and lease epoch, in its own one-round-trip transaction. Zero rows updated means the lease was
//! lost: another worker owns the job now and nothing here may touch it.

use erp_fields::{Error, Result, TimestampTz};
use tokio_postgres::types::{ToSql, Type};

use super::HistoryPolicy;
use super::claim::{ClaimedJob, claim_options};
use super::error::{bounded, bounded_code};
use crate::transaction::CommitError;
use crate::{FinalStatement, Orm};

/// Where a job goes after a run (or a chunk that completes it).
#[derive(Debug, Clone)]
pub(crate) enum Transition {
    /// Back to `ready`.
    Ready {
        /// Due at this instant, or after `delay_secs` on the database clock.
        at: Option<TimestampTz>,
        delay_secs: f64,
        /// The new attempt counter; `None` keeps it.
        attempts: Option<i32>,
        rank: i16,
        /// Error code and message to record, `None` for a clean return.
        error: Option<(String, String)>,
    },
    /// A dead letter.
    Dead { code: String, message: String },
    /// Parked as `held` (an inactive schedule).
    Held { rank: i16 },
    /// Finished: history row (unless the kind keeps none), counters added.
    Done { processed: i64, failed: i64 },
    /// Cancelled by an operator.
    Cancelled,
}

const READY_SQL: &str = "\
UPDATE erp_job SET state = 'ready', lease_owner = NULL, lease_until = NULL,
       run_after = COALESCE($3::timestamptz, clock_timestamp() + make_interval(secs => $4::float8)),
       attempts = COALESCE($5::int4, attempts), failure_rank = $6::int2,
       last_error_code = $7::text, last_error = $8::text,
       last_error_at = CASE WHEN $7::text IS NULL THEN last_error_at ELSE clock_timestamp() END
 WHERE id = $1 AND lease_epoch = $2 AND state = 'running'
RETURNING 1";

const HELD_SQL: &str = "\
UPDATE erp_job SET state = 'held', lease_owner = NULL, lease_until = NULL, attempts = 0,
       failure_rank = $3::int2
 WHERE id = $1 AND lease_epoch = $2 AND state = 'running'
RETURNING 1";

const DEAD_SQL: &str = "\
UPDATE erp_job SET state = 'dead', lease_owner = NULL, lease_until = NULL,
       dead_at = clock_timestamp(), last_error_code = $3::text, last_error = $4::text,
       last_error_at = clock_timestamp()
 WHERE id = $1 AND lease_epoch = $2 AND state = 'running'
RETURNING 1";

/// The job leaves the queue with a history row. `$3` and `$4` are the items done and failed in
/// the chunk that completes it; its item-failure rows go too when no item failed.
pub(crate) const DONE_KEEP_SQL: &str = "\
WITH done AS (
  DELETE FROM erp_job WHERE id = $1 AND lease_epoch = $2 AND state = 'running'
  RETURNING job_uuid, kind, kind_version, attempts, enqueued_at, started_at, items_done,
            items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id,
            last_error_code),
cleanup AS (
  DELETE FROM erp_job_item_failure f USING done
   WHERE f.job_uuid = done.job_uuid AND done.items_failed + $4::int8 = 0
  RETURNING 1)
INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, started_at,
                         items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user,
                         correlation_id, last_error_code)
SELECT job_uuid, kind, kind_version, 'done', GREATEST(attempts, 1), enqueued_at, started_at,
       items_done + $3::int8, items_failed + $4::int8, run_as_kind, run_as_user,
       enqueued_by_user, correlation_id, last_error_code
  FROM done
RETURNING 1";

/// As [`DONE_KEEP_SQL`] for a kind that keeps no history.
pub(crate) const DONE_DROP_SQL: &str = "\
WITH done AS (
  DELETE FROM erp_job WHERE id = $1 AND lease_epoch = $2 AND state = 'running'
  RETURNING job_uuid, items_failed),
cleanup AS (
  DELETE FROM erp_job_item_failure f USING done
   WHERE f.job_uuid = done.job_uuid AND done.items_failed + $4::int8 = 0
  RETURNING 1)
SELECT 1 + $3::int8 * 0 FROM done";

const CANCEL_KEEP_SQL: &str = "\
WITH gone AS (
  DELETE FROM erp_job WHERE id = $1 AND lease_epoch = $2 AND state = 'running'
  RETURNING job_uuid, kind, kind_version, attempts, enqueued_at, started_at, items_done,
            items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id,
            last_error_code)
INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, started_at,
                         items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user,
                         correlation_id, last_error_code)
SELECT job_uuid, kind, kind_version, 'cancelled', attempts, enqueued_at, started_at,
       items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id,
       last_error_code
  FROM gone
RETURNING 1";

const CANCEL_DROP_SQL: &str = "\
DELETE FROM erp_job WHERE id = $1 AND lease_epoch = $2 AND state = 'running' RETURNING 1";

/// Apply `transition` to `job`. `Ok(true)` when the job moved, `Ok(false)` when the lease was
/// lost (no row matched).
pub(crate) async fn apply(orm: &Orm, job: &ClaimedJob, transition: &Transition) -> Result<bool> {
    let id = job.job.id();
    let epoch = job.epoch;
    let keep = job.decl.history == HistoryPolicy::Keep;
    let rows = match transition {
        Transition::Ready {
            at,
            delay_secs,
            attempts,
            rank,
            error,
        } => {
            let rank = i32::from(*rank);
            // bound the composed text here, once, so no caller can break the column's CHECK
            let stored: Option<(String, Box<str>)> = error
                .as_ref()
                .map(|(c, m)| (bounded_code(c), bounded(m.clone())));
            let code: Option<&str> = stored.as_ref().map(|(c, _)| c.as_str());
            let message: Option<&str> = stored.as_ref().map(|(_, m)| &**m);
            let params: [(&(dyn ToSql + Sync), Type); 8] = [
                (&id, Type::INT8),
                (&epoch, Type::INT8),
                (at, Type::TIMESTAMPTZ),
                (delay_secs, Type::FLOAT8),
                (attempts, Type::INT4),
                (&rank, Type::INT4),
                (&code, Type::TEXT),
                (&message, Type::TEXT),
            ];
            flight(orm, READY_SQL, &params).await?
        }
        Transition::Held { rank } => {
            let rank = i32::from(*rank);
            let params: [(&(dyn ToSql + Sync), Type); 3] =
                [(&id, Type::INT8), (&epoch, Type::INT8), (&rank, Type::INT4)];
            flight(orm, HELD_SQL, &params).await?
        }
        Transition::Dead { code, message } => {
            let code = bounded_code(code);
            let message = bounded(message.clone()).into_string();
            let params: [(&(dyn ToSql + Sync), Type); 4] = [
                (&id, Type::INT8),
                (&epoch, Type::INT8),
                (&code, Type::TEXT),
                (&message, Type::TEXT),
            ];
            flight(orm, DEAD_SQL, &params).await?
        }
        Transition::Done { processed, failed } => {
            let params: [(&(dyn ToSql + Sync), Type); 4] = [
                (&id, Type::INT8),
                (&epoch, Type::INT8),
                (processed, Type::INT8),
                (failed, Type::INT8),
            ];
            let sql = if keep { DONE_KEEP_SQL } else { DONE_DROP_SQL };
            flight(orm, sql, &params).await?
        }
        Transition::Cancelled => {
            let params: [(&(dyn ToSql + Sync), Type); 2] =
                [(&id, Type::INT8), (&epoch, Type::INT8)];
            let sql = if keep {
                CANCEL_KEEP_SQL
            } else {
                CANCEL_DROP_SQL
            };
            flight(orm, sql, &params).await?
        }
    };
    Ok(!rows.is_empty())
}

async fn flight(
    orm: &Orm,
    sql: &str,
    params: &[(&(dyn ToSql + Sync), Type)],
) -> Result<Vec<tokio_postgres::Row>> {
    orm.one_flight(claim_options()?, FinalStatement::new(sql, params))
        .await
        .map_err(|e: CommitError| match e {
            CommitError::Unknown(error) => Error::new(
                error.kind(),
                format!(
                    "a job transition may or may not have committed: {}",
                    error.message()
                ),
            ),
            CommitError::RolledBack(error) => error,
        })
}
