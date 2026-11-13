//! The `base.cron` job kind: one run of a scheduled action (3,
//! 119..123).
//!
//! A run is a sequence of chunks, each one transaction opened as the cron's user: a *prepare*
//! chunk (clear the elapsed triggers, count a lease expiry as a timed-out run), then the server
//! action once per chunk until it reports nothing remains, the run budget is used, or it fails,
//! then a *settle* chunk that updates the failure counter, deactivates a cron that failed for
//! long enough, and computes the next call.

use std::time::{Duration, Instant};

use erp_sdk::jobs::{
    ChunkResult, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl, RunOutcome,
};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{BoxFuture, TimestampTz};

use super::progress::CronProgress;
use super::recurrence::{IntervalUnit, next_call};
use crate::ir_actions::ActionId;
use crate::server_action::{RunRequest, run_server_action};

/// Consecutive runs that time out without progress before the run counts as failed.
const CONSECUTIVE_TIMEOUT_FOR_FAILURE: u64 = 3;
/// Failures (and days) before a cron deactivates itself; both must be reached.
const MIN_FAILURE_COUNT_BEFORE_DEACTIVATION: i64 = 5;
const MIN_DELTA_BEFORE_DEACTIVATION_SECS: i64 = 7 * 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Nothing remains.
    FullyDone,
    /// Work remains; the cron runs again at once.
    PartiallyDone,
    /// The action failed.
    Failed,
}

pub fn after_chunk(done: u64, remaining: u64) -> Option<Completion> {
    if remaining == 0 {
        Some(Completion::FullyDone)
    } else if done == 0 {
        Some(Completion::PartiallyDone)
    } else {
        None
    }
}

/// What the failure counter does after a run: the new count and first
/// failure time (seconds since the epoch), whether the cron deactivates, and the warning to send
/// to an administrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureUpdate {
    /// The new `failure_count`.
    pub count: i64,
    /// The new `first_failure_date` (epoch seconds), `None` when cleared.
    pub first_failure: Option<i64>,
    /// The cron deactivates.
    pub deactivate: bool,
    /// An administrator warning, with the kind of it.
    pub warning: Option<Warning>,
}

/// An administrator warning about a failing cron.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warning {
    /// Failing and will be deactivated unless someone acts.
    WillBeDeactivated,
    /// Deactivated after failing too long.
    Deactivated,
}

/// Apply a run's completion to the failure counter, at `now` (epoch seconds).
pub fn update_failure(
    status: Completion,
    count: i64,
    first_failure: Option<i64>,
    now: i64,
) -> FailureUpdate {
    if status != Completion::Failed {
        return FailureUpdate {
            count: 0,
            first_failure: None,
            deactivate: false,
            warning: None,
        };
    }
    let count = count.saturating_add(1);
    let first = first_failure.unwrap_or(now);
    let elapsed = now.saturating_sub(first);
    if count >= MIN_FAILURE_COUNT_BEFORE_DEACTIVATION
        && elapsed > MIN_DELTA_BEFORE_DEACTIVATION_SECS
    {
        return FailureUpdate {
            count: 0,
            first_failure: None,
            deactivate: true,
            warning: Some(Warning::Deactivated),
        };
    }
    let low_count = elapsed > MIN_DELTA_BEFORE_DEACTIVATION_SECS
        && (MIN_FAILURE_COUNT_BEFORE_DEACTIVATION / 2..=MIN_FAILURE_COUNT_BEFORE_DEACTIVATION)
            .contains(&count);
    let often = count > MIN_FAILURE_COUNT_BEFORE_DEACTIVATION
        && elapsed > MIN_DELTA_BEFORE_DEACTIVATION_SECS / 2
        && count.to_string().trim_end_matches('0').len() == 1;
    FailureUpdate {
        count,
        first_failure: Some(first),
        deactivate: false,
        warning: (low_count || often).then_some(Warning::WillBeDeactivated),
    }
}

struct CronHandler;

fn micros_now_sql() -> &'static str {
    "SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::int8"
}

fn from_error(error: Error) -> JobError {
    JobError::from(error)
}

impl JobHandler for CronHandler {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move { run(ctx).await })
    }
}

async fn run(ctx: &mut JobContext<'_>) -> Result<RunOutcome, JobError> {
    let payload: Json = ctx.payload()?;
    let cron_id = payload["cron_id"]
        .as_i64()
        .ok_or_else(|| JobError::permanent("payload", "the job has no cron_id"))?;
    let previous: Option<Json> = ctx.progress()?;
    let mut timed_out = previous
        .as_ref()
        .and_then(|p| p["timed_out"].as_u64())
        .unwrap_or(0);
    timed_out = if ctx.previous_run_expired() {
        timed_out + 1
    } else {
        0
    };
    let made_progress = previous
        .as_ref()
        .and_then(|p| p["done"].as_u64())
        .unwrap_or(0)
        > 0;

    // prepare: elapsed triggers go; the timed-out counter is stored before the action runs
    ctx.chunk(move |tx| {
        Box::pin(async move {
            tx.execute(
                "DELETE FROM ir_cron_trigger WHERE cron_id = $1 AND call_at <= clock_timestamp()",
                &[&cron_id],
            )
            .await?;
            Ok(ChunkResult::more(
                json!({ "timed_out": timed_out, "done": 0 }),
                0,
                None,
            ))
        })
    })
    .await?;

    let failed_by_timeout = timed_out >= CONSECUTIVE_TIMEOUT_FOR_FAILURE && !made_progress;
    let mut status = if failed_by_timeout {
        tracing::error!(target: "erp::cron", cron_id, "scheduled action timed out");
        Some(Completion::Failed)
    } else {
        None
    };
    let mut deactivate = false;
    let mut loops = 0u32;
    while status.is_none() && !ctx.budget_exhausted() {
        let end = Instant::now() + ctx.budget_left();
        let outcome = ctx
            .chunk(move |tx| {
                Box::pin(async move {
                    CronProgress::start(tx, end);
                    run_action(tx, cron_id).await?;
                    let (done, remaining, wants_off) = CronProgress::take(tx);
                    Ok(ChunkResult::more(
                        json!({ "timed_out": 0, "done": done, "deactivate": wants_off }),
                        done,
                        Some(remaining),
                    ))
                })
            })
            .await;
        match outcome {
            Ok(report) => {
                let (done, remaining) = (report.processed, report.remaining.unwrap_or(0));
                if let Some(Json::Object(map)) = ctx.progress::<Json>()? {
                    deactivate |= map
                        .get("deactivate")
                        .and_then(Json::as_bool)
                        .unwrap_or(false);
                }
                if remaining > 0 && done == 0 && loops == 0 {
                    tracing::warn!(target: "erp::cron", cron_id, "scheduled action processed no record");
                }
                status = after_chunk(done, remaining);
                if status == Some(Completion::PartiallyDone) || status.is_none() {
                    // partial or looping: the deactivate flag only counts when the run is done
                    if status.is_some() {
                        deactivate = false;
                    }
                }
            }
            Err(
                error @ (JobError::LeaseLost { .. }
                | JobError::Cancelled { .. }
                | JobError::PrincipalRevoked { .. }
                | JobError::Payload { .. }),
            ) => return Err(error),
            Err(error) => {
                tracing::error!(target: "erp::cron", cron_id, error = %error, "scheduled action failed");
                status = Some(Completion::Failed);
            }
        }
        loops += 1;
    }
    let status = status.unwrap_or(Completion::PartiallyDone);

    // settle: failure counter, deactivation, next call
    ctx.chunk(move |tx| Box::pin(async move { settle(tx, cron_id, status, deactivate).await }))
        .await?;
    let settled: Json = ctx.progress()?.unwrap_or(Json::Null);
    let failure_count = settled["failure_count"]
        .as_u64()
        .ok_or_else(|| JobError::permanent("settle", "the settle chunk gave no failure counter"))?;
    ctx.set_failure_rank(u16::try_from(failure_count).unwrap_or(u16::MAX));
    if settled["deactivated"].as_bool().unwrap_or(false) {
        return Ok(RunOutcome::Hold);
    }
    match status {
        Completion::PartiallyDone => Ok(RunOutcome::Continue),
        Completion::FullyDone | Completion::Failed => {
            let next = settled["next_micros"].as_i64().ok_or_else(|| {
                JobError::permanent("settle", "the settle chunk gave no next call")
            })?;
            let at = TimestampTz::from_unix_micros(next).map_err(from_error)?;
            Ok(RunOutcome::Reschedule(at))
        }
    }
}

/// Run the server action of the cron as the chunk's user.
async fn run_action(tx: &mut Transaction, cron_id: i64) -> Result<()> {
    let rows = tx
        .query(
            "SELECT c.ir_actions_server_id, m.model FROM ir_cron c \
             JOIN ir_act_server s ON s.id = c.ir_actions_server_id \
             JOIN ir_model m ON m.id = s.model_id WHERE c.id = $1",
            &[&cron_id],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::missing(format!("the scheduled action {cron_id} no longer exists"))
    })?;
    let action: i64 = row.get(0);
    let model: String = row.get(1);
    let env = tx.env();
    let request = RunRequest::new(ActionId::new(action)?, &model, &[]);
    run_server_action(tx, &env, &request).await?;
    Ok(())
}

/// The settle chunk: counters, deactivation, the next call. Its result is in the progress.
async fn settle(
    tx: &mut Transaction,
    cron_id: i64,
    status: Completion,
    wants_deactivate: bool,
) -> std::result::Result<ChunkResult<Json>, JobError> {
    let now: i64 = tx.query(micros_now_sql(), &[]).await?[0].get(0);
    let rows = tx
        .query(
            "SELECT c.failure_count, \
             (extract(epoch FROM c.first_failure_date AT TIME ZONE 'UTC'))::int8, \
             (extract(epoch FROM c.nextcall AT TIME ZONE 'UTC') * 1000000)::int8, \
             c.interval_number, c.interval_type, c.active, \
             COALESCE(p.tz, 'UTC'), s.name \
             FROM ir_cron c JOIN ir_act_server s ON s.id = c.ir_actions_server_id \
             LEFT JOIN res_users u ON u.id = c.user_id \
             LEFT JOIN res_partner p ON p.id = u.partner_id WHERE c.id = $1",
            &[&cron_id],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| JobError::permanent("cron_missing", "the scheduled action was deleted"))?;
    let count: i32 = row.get(0);
    let first: Option<i64> = row.get(1);
    let nextcall: i64 = row.get(2);
    let number: i32 = row.get(3);
    let unit: String = row.get(4);
    let active: bool = row.get(5);
    let tz: String = row.get(6);
    let name: String = row.get(7);
    let update = update_failure(status, i64::from(count), first, now / 1_000_000);
    let mut deactivated =
        update.deactivate || (status == Completion::FullyDone && wants_deactivate);
    if !active {
        deactivated = true;
    }
    let (next, lastcall) = match status {
        Completion::PartiallyDone => (nextcall, None),
        _ => (
            next_call(
                nextcall,
                i64::from(number),
                IntervalUnit::parse(&unit).map_err(from_error)?,
                &tz,
                now,
            )
            .map_err(from_error)?,
            Some(now),
        ),
    };
    tx.execute(
        "UPDATE ir_cron SET failure_count = $2, \
         first_failure_date = CASE WHEN $3::int8 IS NULL THEN NULL \
              ELSE to_timestamp($3::float8) AT TIME ZONE 'UTC' END, \
         active = $4, \
         nextcall = to_timestamp($5::float8 / 1000000) AT TIME ZONE 'UTC', \
         lastcall = CASE WHEN $6::int8 IS NULL THEN lastcall \
              ELSE to_timestamp($6::float8 / 1000000) AT TIME ZONE 'UTC' END \
         WHERE id = $1",
        &[
            &cron_id,
            &i32::try_from(update.count).unwrap_or(i32::MAX),
            &update.first_failure,
            &!deactivated,
            &(next as f64),
            &lastcall,
        ],
    )
    .await?;
    if let Some(warning) = update.warning {
        let message = match warning {
            Warning::Deactivated => format!(
                "Cron job '{name}' ({cron_id}) has been deactivated after failing {MIN_FAILURE_COUNT_BEFORE_DEACTIVATION} times. More information can be found in the server logs around {}.",
                now / 1_000_000
            ),
            Warning::WillBeDeactivated => format!(
                "Cron job '{name}' ({cron_id}) is failing and will be deactivated if you don't take action. More information can be found in the server logs around {}.",
                now / 1_000_000
            ),
        };
        let records = tx.env().sudo(true).records("ir.cron")?;
        records
            .call::<(String,), ()>(tx, "_notify_admin", (message,))
            .await?;
    }
    // the next run: the earlier of the next call and the earliest trigger still to come
    let trigger: Option<i64> = tx
        .query(
            "SELECT (extract(epoch FROM min(call_at) AT TIME ZONE 'UTC') * 1000000)::int8 \
             FROM ir_cron_trigger WHERE cron_id = $1 AND call_at > clock_timestamp()",
            &[&cron_id],
        )
        .await?[0]
        .get(0);
    let due = trigger.map_or(next, |t| t.min(next));
    Ok(ChunkResult::more(
        json!({
            "timed_out": 0,
            "done": 0,
            "failure_count": update.count,
            "deactivated": deactivated,
            "next_micros": due
        }),
        0,
        None,
    ))
}

static HANDLER: CronHandler = CronHandler;

erp_sdk::jobs::register_job_kind!(
    BASE_CRON_KIND,
    JobKindDecl::new("base", super::CRON_KIND, HandlerRef::Native(&HANDLER)).max_attempts(100)
);

pub const PARTIAL_RESCHEDULE: Duration = Duration::ZERO;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chunk_outcome_table_is_references() {
        assert_eq!(after_chunk(0, 0), Some(Completion::FullyDone));
        assert_eq!(after_chunk(7, 0), Some(Completion::FullyDone));
        assert_eq!(after_chunk(0, 5), Some(Completion::PartiallyDone));
        assert_eq!(after_chunk(3, 5), None);
    }

    #[test]
    fn a_failure_increments_the_count_and_sets_the_first_failure_date_once() {
        let first = update_failure(Completion::Failed, 0, None, 1_000);
        assert_eq!(
            (first.count, first.first_failure, first.deactivate),
            (1, Some(1_000), false)
        );
        let second = update_failure(Completion::Failed, 1, Some(1_000), 2_000);
        assert_eq!((second.count, second.first_failure), (2, Some(1_000)));
    }

    #[test]
    fn success_resets_the_counter_and_the_date() {
        for status in [Completion::FullyDone, Completion::PartiallyDone] {
            let update = update_failure(status, 4, Some(5), 10);
            assert_eq!(
                (update.count, update.first_failure, update.deactivate),
                (0, None, false)
            );
        }
    }

    #[test]
    fn a_cron_deactivates_only_after_five_failures_and_seven_days() {
        let week = MIN_DELTA_BEFORE_DEACTIVATION_SECS;
        let early = update_failure(Completion::Failed, 4, Some(0), week - 1);
        assert!(!early.deactivate);
        let few = update_failure(Completion::Failed, 2, Some(0), week + 1);
        assert!(!few.deactivate);
        let both = update_failure(Completion::Failed, 4, Some(0), week + 1);
        assert_eq!(
            (both.deactivate, both.count, both.first_failure),
            (true, 0, None)
        );
        assert_eq!(both.warning, Some(Warning::Deactivated));
    }

    #[test]
    fn the_warning_before_deactivation_follows_references_two_rules() {
        let week = MIN_DELTA_BEFORE_DEACTIVATION_SECS;
        // a low count after the week: warned from the second failure
        let low = update_failure(Completion::Failed, 1, Some(0), week + 1);
        assert_eq!(low.warning, Some(Warning::WillBeDeactivated));
        // many failures after half the week warn only at 10, 20, ... 100, 200
        let ten = update_failure(Completion::Failed, 9, Some(0), week / 2 + 1);
        assert_eq!(ten.warning, Some(Warning::WillBeDeactivated));
        let eleven = update_failure(Completion::Failed, 10, Some(0), week / 2 + 1);
        assert_eq!(eleven.warning, None);
        let fresh = update_failure(Completion::Failed, 0, None, 5);
        assert_eq!(fresh.warning, None);
    }
}
