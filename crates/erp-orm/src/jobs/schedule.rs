// SPDX-License-Identifier: MIT OR Apache-2.0
//! Recurring jobs an addon declares on its own step kind (`JobKindDecl::schedule`).
//!
//! A scheduled kind has exactly one job row per database, keyed by the kind's name (dedup and
//! singleton key), created when its addon is installed or upgraded and removed when it is
//! uninstalled ([`sync_addon_schedules`], called by the module system inside the operation's
//! transaction). Every node that runs jobs may claim it; the queue's lease and the unique key
//! make one run per due time, whatever the number of nodes.
//!
//! A run (an *occurrence*) calls the kind's step method chunk by chunk like any step kind, with
//! the payload `{"scheduled_for": <RFC 3339>, "previous": <RFC 3339 or null>}`: the instant this
//! occurrence stands for and the one the last completed occurrence stood for. When the step
//! reports `finished`, the row is not deleted: its finishing chunk stores the next due time and,
//! for a kind that keeps history, writes one `done` history row, and the run reschedules the row.
//! Occurrences missed while no runner ran collapse into one, whose `scheduled_for` is the latest
//! missed instant and whose `previous` names the last one that completed, so the step can cover
//! the gap. Failures follow the kind's retry and dead-letter rules; a dead-lettered schedule
//! stops until an operator requeues it, and then resumes the same occurrence.

use std::time::Duration;

use erp_fields::{Error, Result};
use serde_json::{Value, json};

use super::context::JobContext;
use super::{
    HandlerRef, HistoryPolicy, JobError, JobKey, JobKindDecl, PrincipalPolicy, RecurringSpec,
    RunOutcome, StepArgs, When,
};
use crate::principal::{PrincipalKind, UserId};
use crate::{Env, Transaction};

/// Shortest interval of [`Recurrence::Every`]: the runner polls in seconds, not faster.
pub const MIN_SCHEDULE_INTERVAL: Duration = Duration::from_secs(60);
/// Longest interval of [`Recurrence::Every`]; longer periods are calendar schedules.
pub const MAX_SCHEDULE_INTERVAL: Duration = Duration::from_secs(31 * 86_400);

const MICROS_PER_SEC: i64 = 1_000_000;

/// When a scheduled kind is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recurrence {
    /// Every `interval`, at the instants `offset + k * interval` counted from the Unix epoch
    /// (so every 3600 s is at the top of each UTC hour, whichever node computes it).
    Every {
        /// The period, [`MIN_SCHEDULE_INTERVAL`]`..=`[`MAX_SCHEDULE_INTERVAL`], whole seconds.
        interval: Duration,
        /// Shift of the instants, below `interval`, whole seconds.
        offset: Duration,
    },
    /// Every day at `hour:minute` wall-clock time in the IANA zone `time_zone`. A time that a
    /// daylight-saving change skips runs at the same distance after the gap; a repeated time
    /// runs once, at its first occurrence.
    DailyAt {
        /// 0..=23.
        hour: u8,
        /// 0..=59.
        minute: u8,
        /// An IANA zone name such as `"Asia/Jakarta"` or `"UTC"`.
        time_zone: &'static str,
    },
}

/// The schedule of a step kind: its recurrence and the technical user it runs as.
///
/// ```
/// use std::time::Duration;
/// use erp_orm::jobs::{HandlerRef, JobKindDecl, Schedule};
///
/// static NIGHTLY: JobKindDecl = JobKindDecl::new(
///     "stock",
///     "stock.nightly_recount",
///     HandlerRef::Step { model: "stock.recount", method: "run_step" },
/// )
/// .schedule(Schedule::daily_at(1, 0, "Asia/Jakarta", "stock.user_scheduler"));
/// assert!(NIGHTLY.schedule.is_some());
/// let hourly = Schedule::every(Duration::from_secs(3_600), "stock.user_scheduler");
/// assert_eq!(hourly.run_as, "stock.user_scheduler");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Schedule {
    /// When it is due.
    pub recurrence: Recurrence,
    /// The external id (`<module>.<name>`) of the active internal `res.users` record the job
    /// runs as, usually a technical user the addon ships in its data. Never the system.
    pub run_as: &'static str,
}

impl Schedule {
    /// Every `interval`, aligned to the Unix epoch.
    pub const fn every(interval: Duration, run_as: &'static str) -> Self {
        Self::every_from(interval, Duration::ZERO, run_as)
    }

    /// Every `interval`, at `offset` after each epoch-aligned instant (every 86 400 s with an
    /// offset of 3 600 s is 01:00 UTC each day).
    pub const fn every_from(interval: Duration, offset: Duration, run_as: &'static str) -> Self {
        Self {
            recurrence: Recurrence::Every { interval, offset },
            run_as,
        }
    }

    /// Every day at `hour:minute` in `time_zone`.
    pub const fn daily_at(
        hour: u8,
        minute: u8,
        time_zone: &'static str,
        run_as: &'static str,
    ) -> Self {
        Self {
            recurrence: Recurrence::DailyAt {
                hour,
                minute,
                time_zone,
            },
            run_as,
        }
    }
}

fn whole_micros(d: Duration) -> Option<i64> {
    (d.subsec_nanos() == 0)
        .then(|| i64::try_from(d.as_secs()).ok())
        .flatten()
        .and_then(|s| s.checked_mul(MICROS_PER_SEC))
}

fn zone(name: &str) -> Result<jiff::tz::TimeZone> {
    jiff::tz::TimeZone::get(name)
        .map_err(|_| Error::programming(format!("'{name}' is not a known IANA time zone")))
}

fn time_error(e: &jiff::Error) -> Error {
    Error::internal(format!("a schedule instant is out of range: {e}"))
}

impl Recurrence {
    /// The checks of registry publication, as a message naming the rule.
    fn check(&self) -> std::result::Result<(), String> {
        match *self {
            Self::Every { interval, offset } => {
                if !(MIN_SCHEDULE_INTERVAL..=MAX_SCHEDULE_INTERVAL).contains(&interval)
                    || whole_micros(interval).is_none()
                {
                    return Err(format!(
                        "interval {interval:?} is outside {}..={} s or not whole seconds",
                        MIN_SCHEDULE_INTERVAL.as_secs(),
                        MAX_SCHEDULE_INTERVAL.as_secs()
                    ));
                }
                if offset >= interval || whole_micros(offset).is_none() {
                    return Err(format!(
                        "offset {offset:?} must be whole seconds below the interval {interval:?}"
                    ));
                }
                Ok(())
            }
            Self::DailyAt {
                hour,
                minute,
                time_zone,
            } => {
                if hour > 23 || minute > 59 {
                    return Err(format!("{hour:02}:{minute:02} is not a time of day"));
                }
                zone(time_zone)
                    .map(|_| ())
                    .map_err(|e| e.message().to_owned())
            }
        }
    }

    /// The latest due instant at or before `t` (Unix microseconds).
    pub(crate) fn latest_at_or_before(&self, t: i64) -> Result<i64> {
        match *self {
            Self::Every { interval, offset } => {
                let i = whole_micros(interval).ok_or_else(|| {
                    Error::programming("a schedule interval is not whole seconds")
                })?;
                let o = whole_micros(offset)
                    .ok_or_else(|| Error::programming("a schedule offset is not whole seconds"))?;
                Ok((t - o).div_euclid(i) * i + o)
            }
            Self::DailyAt {
                hour,
                minute,
                time_zone,
            } => {
                let tz = zone(time_zone)?;
                let date = jiff::Timestamp::from_microsecond(t)
                    .map_err(|e| time_error(&e))?
                    .to_zoned(tz.clone())
                    .date();
                let today = daily_instant(date, hour, minute, &tz)?;
                if today <= t {
                    Ok(today)
                } else {
                    daily_instant(
                        date.yesterday().map_err(|e| time_error(&e))?,
                        hour,
                        minute,
                        &tz,
                    )
                }
            }
        }
    }

    /// The first due instant strictly after `t` (Unix microseconds).
    pub(crate) fn next_after(&self, t: i64) -> Result<i64> {
        let latest = self.latest_at_or_before(t)?;
        match *self {
            Self::Every { interval, .. } => {
                let i = whole_micros(interval).ok_or_else(|| {
                    Error::programming("a schedule interval is not whole seconds")
                })?;
                Ok(latest + i)
            }
            Self::DailyAt {
                hour,
                minute,
                time_zone,
            } => {
                let tz = zone(time_zone)?;
                let date = jiff::Timestamp::from_microsecond(latest)
                    .map_err(|e| time_error(&e))?
                    .to_zoned(tz.clone())
                    .date();
                daily_instant(
                    date.tomorrow().map_err(|e| time_error(&e))?,
                    hour,
                    minute,
                    &tz,
                )
            }
        }
    }
}

/// `hour:minute` of `date` in `tz` as Unix microseconds (compatible disambiguation).
fn daily_instant(
    date: jiff::civil::Date,
    hour: u8,
    minute: u8,
    tz: &jiff::tz::TimeZone,
) -> Result<i64> {
    let hour = i8::try_from(hour).map_err(|_| Error::programming("hour out of range"))?;
    let minute = i8::try_from(minute).map_err(|_| Error::programming("minute out of range"))?;
    let zoned = date
        .at(hour, minute, 0, 0)
        .to_zoned(tz.clone())
        .map_err(|e| time_error(&e))?;
    Ok(zoned.timestamp().as_microsecond())
}

/// Whether `id` is an external id `<module>.<name>`.
fn is_external_id(id: &str) -> bool {
    id.split_once('.').is_some_and(|(module, name)| {
        !module.is_empty()
            && !name.is_empty()
            && !name.contains('.')
            && id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'.')
    })
}

/// The schedule checks of registry publication for `decl` (called by `kind::validate`).
pub(super) fn validate(decl: &JobKindDecl, schedule: &Schedule) -> Result<()> {
    let name = decl.name;
    if !matches!(decl.handler, HandlerRef::Step { .. }) {
        return Err(Error::programming(format!(
            "job kind '{name}' declares a schedule but is not a step kind: a native handler \
             reschedules itself (RunOutcome::Reschedule)"
        )));
    }
    if decl.principal != PrincipalPolicy::Caller {
        return Err(Error::programming(format!(
            "job kind '{name}' declares a schedule and the System principal: a scheduled kind \
             runs as the technical user its schedule names"
        )));
    }
    if !is_external_id(schedule.run_as) {
        return Err(Error::programming(format!(
            "job kind '{name}' schedules its runs as '{}', which is not an external id \
             '<module>.<name>' of a res.users record",
            schedule.run_as
        )));
    }
    schedule
        .recurrence
        .check()
        .map_err(|rule| Error::programming(format!("job kind '{name}' schedule: {rule}")))
}

/// The error of enqueueing a scheduled kind by hand.
pub(super) fn refuse_enqueue(kind: &str) -> Error {
    Error::programming(format!(
        "job kind '{kind}' is a scheduled kind: its one job row is kept by the module system \
         from its schedule and it cannot be enqueued"
    ))
}

fn now_micros() -> i64 {
    jiff::Timestamp::now().as_microsecond()
}

fn rfc3339(micros: i64) -> std::result::Result<String, JobError> {
    jiff::Timestamp::from_microsecond(micros)
        .map(|t| t.to_string())
        .map_err(|_| JobError::permanent("schedule_instant", "a schedule instant is out of range"))
}

/// The progress a scheduled row stores between runs; the step's own progress is in `step`.
#[derive(Debug, Clone, Default, PartialEq)]
struct Stored {
    /// The occurrence in flight (a run that continues it), microseconds.
    current: Option<i64>,
    /// The last completed occurrence.
    previous: Option<i64>,
    /// When the next occurrence is due.
    next: Option<i64>,
    /// Items the occurrence in flight processed so far.
    items: u64,
    /// The step's progress inside the occurrence in flight.
    step: Option<Value>,
}

impl Stored {
    fn parse(progress: Option<&Value>) -> Self {
        let Some(p) = progress else {
            return Self::default();
        };
        Self {
            current: p["current"].as_i64(),
            previous: p["previous"].as_i64(),
            next: p["next"].as_i64(),
            items: p["items"].as_u64().unwrap_or(0),
            step: p.get("step").filter(|s| !s.is_null()).cloned(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "current": self.current,
            "previous": self.previous,
            "next": self.next,
            "items": self.items,
            "step": self.step,
        })
    }
}

/// One history row for a completed occurrence of a kind that keeps history.
const OCCURRENCE_HISTORY_SQL: &str = "\
INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, started_at,
                         items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user,
                         correlation_id, last_error_code)
SELECT job_uuid, kind, kind_version, 'done', GREATEST(attempts, 1), enqueued_at, started_at,
       $2::int8, 0, run_as_kind, run_as_user, enqueued_by_user, correlation_id, last_error_code
  FROM erp_job WHERE id = $1";

/// The run of a scheduled step kind: one occurrence (or the rest of one), then the reschedule.
pub(super) async fn run_scheduled_step(
    ctx: &mut JobContext<'static>,
    schedule: &'static Schedule,
    model: &'static str,
    method: &'static str,
) -> std::result::Result<RunOutcome, JobError> {
    let recurrence = schedule.recurrence;
    let stored = Stored::parse(ctx.progress.as_ref());
    let now = now_micros();
    let occurrence = match stored.current {
        Some(current) => current,
        None => {
            if let Some(next) = stored.next.filter(|next| *next > now) {
                // the previous run stored its next due time but could not reschedule the row
                // (a crash between the two): wait for it instead of running early
                let at = erp_fields::TimestampTz::from_unix_micros(next).map_err(JobError::from)?;
                return Ok(RunOutcome::Reschedule(at));
            }
            // a missed occurrence collapses into the latest one due
            recurrence
                .latest_at_or_before(now)
                .map_err(JobError::from)?
        }
    };
    let next = recurrence.next_after(occurrence).map_err(JobError::from)?;
    let payload = json!({
        "scheduled_for": rfc3339(occurrence)?,
        "previous": stored.previous.map(rfc3339).transpose()?,
    });
    let keep_history = ctx.claimed.decl.history == HistoryPolicy::Keep;
    let job_id = ctx.claimed.job.id();
    let mut state = if stored.current.is_some() {
        stored
    } else {
        // a new occurrence starts without step progress
        Stored {
            current: Some(occurrence),
            items: 0,
            step: None,
            ..stored
        }
    };
    loop {
        let args = StepArgs {
            payload: payload.clone(),
            progress: state.step.clone(),
            attempt: ctx.claimed.attempt,
        };
        let before = state.clone();
        let report = ctx
            .chunk(move |tx| {
                let args = args.clone();
                let before = before.clone();
                Box::pin(async move {
                    let mut chunk = super::drive::call_step(tx, model, method, args).await?;
                    let items = before.items.saturating_add(chunk.processed);
                    let stored = if chunk.finished {
                        if keep_history {
                            let items = i64::try_from(items).unwrap_or(i64::MAX);
                            tx.execute(OCCURRENCE_HISTORY_SQL, &[&job_id, &items])
                                .await
                                .map_err(JobError::from)?;
                        }
                        Stored {
                            current: None,
                            previous: Some(occurrence),
                            next: Some(next),
                            items: 0,
                            step: None,
                        }
                    } else {
                        Stored {
                            items,
                            step: Some(chunk.progress.clone()),
                            ..before
                        }
                    };
                    // the row stays: the finishing chunk stores the next due time instead of
                    // completing the job
                    chunk.progress = stored.to_json();
                    chunk.finished = false;
                    Ok(chunk)
                })
            })
            .await?;
        state = Stored::parse(ctx.progress.as_ref());
        if state.current.is_none() {
            tracing::info!(
                target: "erp::jobs",
                kind = ctx.claimed.decl.name,
                job = %ctx.claimed.job.uuid(),
                scheduled_for = occurrence,
                next,
                "scheduled job occurrence completed"
            );
            let at = erp_fields::TimestampTz::from_unix_micros(next).map_err(JobError::from)?;
            return Ok(RunOutcome::Reschedule(at));
        }
        if report.processed == 0 || ctx.cancel_token().is_cancelled() || ctx.budget_exhausted() {
            return Ok(RunOutcome::Continue);
        }
    }
}

/// Keep the job rows of the scheduled kinds linked into this binary in step with an addon
/// change, inside the module operation's transaction `tx` (run by the system principal `env`):
///
/// - for each scheduled kind of an addon in `kept` (installed or upgraded): resolve the user of
///   its schedule (an active internal `res.users` record named by external id), and create its
///   row, due at the next instant, when it has no live row; a live row that runs as another user
///   is replaced. A live row of the same user is left as it is (its due time, and an operator's
///   hold, stay).
/// - for each scheduled kind of an addon in `removed` (uninstalled): delete its rows, dead
///   letters included.
///
/// Nothing is read or written when no changed addon has a scheduled kind.
///
/// # Errors
///
/// `Access` unless `env` is the system principal; `Programming` when the user of a schedule is
/// missing, inactive or not internal; `UserError` when a row to replace or remove is running;
/// the database's errors.
pub async fn sync_addon_schedules(
    tx: &mut Transaction,
    env: &Env,
    kept: &[&str],
    removed: &[&str],
) -> Result<()> {
    let scheduled: Vec<(&'static JobKindDecl, &'static Schedule)> = super::kind::linked()
        .into_iter()
        .filter(|decl| kept.contains(&decl.addon) || removed.contains(&decl.addon))
        .filter_map(|decl| decl.schedule.as_ref().map(|s| (decl, s)))
        .collect();
    if scheduled.is_empty() {
        return Ok(());
    }
    if env.principal().kind() != PrincipalKind::System {
        return Err(Error::access(
            "the schedules of addon job kinds are kept by the system principal only",
        ));
    }
    for (decl, _) in scheduled.iter().filter(|(d, _)| removed.contains(&d.addon)) {
        remove_rows(tx, decl).await?;
    }
    for (decl, schedule) in scheduled.iter().filter(|(d, _)| kept.contains(&d.addon)) {
        keep_row(tx, env, decl, schedule).await?;
    }
    Ok(())
}

async fn remove_rows(tx: &mut Transaction, decl: &JobKindDecl) -> Result<()> {
    let running = tx
        .query(
            "SELECT 1 FROM erp_job WHERE kind = $1 AND state = 'running' \
             AND lease_until > clock_timestamp()",
            &[&decl.name],
        )
        .await?;
    if !running.is_empty() {
        return Err(Error::user(format!(
            "the scheduled job '{}' of addon '{}' is running; uninstall the addon again when it \
             has finished",
            decl.name, decl.addon
        )));
    }
    let removed = tx
        .execute("DELETE FROM erp_job WHERE kind = $1", &[&decl.name])
        .await?;
    tracing::info!(
        target: "erp::jobs",
        kind = decl.name,
        addon = decl.addon,
        removed,
        "scheduled job rows removed with their addon"
    );
    Ok(())
}

async fn keep_row(
    tx: &mut Transaction,
    env: &Env,
    decl: &'static JobKindDecl,
    schedule: &Schedule,
) -> Result<()> {
    let user = resolve_run_as(tx, decl, schedule).await?;
    let live = tx
        .query(
            "SELECT run_as_kind, run_as_user FROM erp_job \
             WHERE kind = $1 AND dedup_key = $1 AND state <> 'dead'",
            &[&decl.name],
        )
        .await?;
    if let Some(row) = live.first() {
        let kind: String = row.try_get(0).map_err(undecodable)?;
        let stored: i64 = row.try_get(1).map_err(undecodable)?;
        if kind == PrincipalKind::User.as_str() && stored == user.get() {
            return Ok(());
        }
    }
    let first = schedule.recurrence.next_after(now_micros())?;
    let spec = RecurringSpec::new(decl.name, JobKey::new(decl.name)?)
        .first_run(When::At(erp_fields::TimestampTz::from_unix_micros(first)?))
        .run_as(user);
    tx.job_upsert_recurring(env, spec).await?;
    tracing::info!(
        target: "erp::jobs",
        kind = decl.name,
        addon = decl.addon,
        run_as = user.get(),
        first_due = first,
        "scheduled job row kept"
    );
    Ok(())
}

async fn resolve_run_as(
    tx: &mut Transaction,
    decl: &JobKindDecl,
    schedule: &Schedule,
) -> Result<UserId> {
    let (module, name) = schedule.run_as.split_once('.').ok_or_else(|| {
        Error::programming(format!(
            "job kind '{}' runs as '{}', which is not an external id",
            decl.name, schedule.run_as
        ))
    })?;
    let rows = tx
        .query(
            "SELECT u.id, u.active, u.share FROM ir_model_data d \
             JOIN res_users u ON u.id = d.res_id \
             WHERE d.module = $1 AND d.name = $2 AND d.model = 'res.users'",
            &[&module, &name],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::programming(format!(
            "job kind '{}' runs as '{}', but no res.users record has that external id; ship the \
             technical user in the addon's data",
            decl.name, schedule.run_as
        ))
    })?;
    let id: i64 = row.try_get(0).map_err(undecodable)?;
    let active: bool = row.try_get(1).map_err(undecodable)?;
    let share: bool = row.try_get(2).map_err(undecodable)?;
    if !active || share {
        return Err(Error::programming(format!(
            "job kind '{}' runs as '{}' (user {id}), which must be an active internal user \
             (active and not share)",
            decl.name, schedule.run_as
        )));
    }
    UserId::new(id).map_err(|e| Error::programming(e.to_string()))
}

fn undecodable(source: tokio_postgres::Error) -> Error {
    Error::internal("a scheduled job row did not decode").with_source(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600 * MICROS_PER_SEC;
    const DAY: i64 = 24 * HOUR;

    fn ts(text: &str) -> i64 {
        text.parse::<jiff::Timestamp>().unwrap().as_microsecond()
    }

    #[test]
    fn an_interval_is_aligned_to_the_epoch_plus_its_offset() {
        let hourly = Schedule::every(Duration::from_secs(3_600), "a.u").recurrence;
        let t = ts("2026-03-01T10:20:00Z");
        assert_eq!(
            hourly.latest_at_or_before(t).unwrap(),
            ts("2026-03-01T10:00:00Z")
        );
        assert_eq!(hourly.next_after(t).unwrap(), ts("2026-03-01T11:00:00Z"));
        let exact = ts("2026-03-01T10:00:00Z");
        assert_eq!(hourly.latest_at_or_before(exact).unwrap(), exact);
        assert_eq!(hourly.next_after(exact).unwrap(), exact + HOUR);
        let one_am = Schedule::every_from(
            Duration::from_secs(86_400),
            Duration::from_secs(3_600),
            "a.u",
        )
        .recurrence;
        assert_eq!(
            one_am
                .latest_at_or_before(ts("2026-03-01T00:30:00Z"))
                .unwrap(),
            ts("2026-02-28T01:00:00Z")
        );
        assert_eq!(
            one_am.next_after(ts("2026-03-01T00:30:00Z")).unwrap(),
            ts("2026-03-01T01:00:00Z")
        );
    }

    #[test]
    fn a_daily_time_follows_the_wall_clock_of_its_zone_across_daylight_saving() {
        let daily = Schedule::daily_at(1, 30, "Europe/Amsterdam", "a.u").recurrence;
        // winter: UTC+1
        assert_eq!(
            daily
                .latest_at_or_before(ts("2026-03-28T12:00:00Z"))
                .unwrap(),
            ts("2026-03-28T00:30:00Z")
        );
        // the clocks go forward on 2026-03-29 at 02:00: 01:30 still exists (UTC+1), the day
        // after it is UTC+2
        assert_eq!(
            daily.next_after(ts("2026-03-28T00:30:00Z")).unwrap(),
            ts("2026-03-29T00:30:00Z")
        );
        assert_eq!(
            daily.next_after(ts("2026-03-29T00:30:00Z")).unwrap(),
            ts("2026-03-29T23:30:00Z")
        );
        // before today's time: yesterday's
        assert_eq!(
            daily
                .latest_at_or_before(ts("2026-03-30T23:00:00Z"))
                .unwrap(),
            ts("2026-03-29T23:30:00Z")
        );
        let jakarta = Schedule::daily_at(0, 0, "Asia/Jakarta", "a.u").recurrence;
        let t = ts("2026-06-01T18:00:00Z");
        assert_eq!(
            jakarta.latest_at_or_before(t).unwrap(),
            ts("2026-06-01T17:00:00Z")
        );
        assert_eq!(
            jakarta.next_after(t).unwrap(),
            ts("2026-06-01T17:00:00Z") + DAY
        );
    }

    #[test]
    fn a_skipped_wall_clock_time_runs_after_the_gap() {
        // 02:30 does not exist in Amsterdam on 2026-03-29: it runs at 03:30 local (01:30 UTC)
        let daily = Schedule::daily_at(2, 30, "Europe/Amsterdam", "a.u").recurrence;
        assert_eq!(
            daily.next_after(ts("2026-03-28T01:30:00Z")).unwrap(),
            ts("2026-03-29T01:30:00Z")
        );
        assert_eq!(
            daily.next_after(ts("2026-03-29T01:30:00Z")).unwrap(),
            ts("2026-03-30T00:30:00Z")
        );
    }

    #[test]
    fn publication_checks_the_recurrence_and_the_user() {
        let step = HandlerRef::Step {
            model: "a.m",
            method: "run",
        };
        let check = |schedule: Schedule| {
            let decl = JobKindDecl::new("a", "a.k", step).schedule(schedule);
            validate(&decl, &schedule).map_err(|e| e.message().to_owned())
        };
        assert!(check(Schedule::every(Duration::from_secs(60), "a.user")).is_ok());
        assert!(check(Schedule::daily_at(23, 59, "UTC", "a.user")).is_ok());
        assert!(
            check(Schedule::every(Duration::from_secs(59), "a.user"))
                .unwrap_err()
                .contains("outside 60..=2678400 s")
        );
        assert!(
            check(Schedule::every(Duration::from_millis(60_500), "a.user"))
                .unwrap_err()
                .contains("not whole seconds")
        );
        assert!(
            check(Schedule::every_from(
                Duration::from_secs(60),
                Duration::from_secs(60),
                "a.user"
            ))
            .unwrap_err()
            .contains("below the interval")
        );
        assert!(
            check(Schedule::daily_at(24, 0, "UTC", "a.user"))
                .unwrap_err()
                .contains("is not a time of day")
        );
        assert!(
            check(Schedule::daily_at(1, 0, "Mars/Olympus", "a.user"))
                .unwrap_err()
                .contains("not a known IANA time zone")
        );
        for bad in ["user", "a.", ".u", "a.b.c", "A.user"] {
            assert!(
                check(Schedule::daily_at(1, 0, "UTC", bad))
                    .unwrap_err()
                    .contains("is not an external id"),
                "{bad}"
            );
        }
    }

    #[test]
    fn stored_progress_round_trips() {
        let stored = Stored {
            current: Some(5),
            previous: Some(1),
            next: None,
            items: 7,
            step: Some(json!({"n": 2})),
        };
        assert_eq!(Stored::parse(Some(&stored.to_json())), stored);
        assert_eq!(Stored::parse(None), Stored::default());
    }
}
