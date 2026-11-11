//! `ir.cron` and `ir.cron.trigger` on the job queue (spec).
//!
//! `create`, `write`, `unlink` and `trigger`
//! keep that row in the same transaction; nothing polls `ir_cron`. The job runs the server action
//! as the cron's user ([`kind`]), reports progress with [`commit_progress`], and reschedules
//! itself with the recurrence of [`recurrence`].

pub mod kind;
pub mod progress;
pub mod recurrence;

pub use progress::commit_progress;

use erp_sdk::jobs::{JobKey, RecurringSpec, When};
use erp_sdk::prelude::*;
use erp_sdk::{Cell, Operation, PrincipalKind, SUPERUSER_ID, TimestampTz, UserId};

/// The job kind of a scheduled action.
pub const CRON_KIND: &str = "base.cron";

/// The key of the job row of cron `id`.
pub fn job_key(id: i64) -> Result<JobKey> {
    JobKey::new(&format!("{CRON_KIND}:{id}"))
}

/// `ir.cron`: a recurring server action.
#[model(
    name = "ir.cron",
    description = "Scheduled Action",
    order = "cron_name, id",
    inherits = [("ir.actions.server", "ir_actions_server_id")],
    company_access = unrestricted(reason = "scheduled actions are system configuration; the reference ERP's ir.cron has no company rule"),
    allow_sudo_commands = false
)]
pub struct IrCron {
    /// The server action it runs.
    #[field(
        many2one,
        comodel = "ir.actions.server",
        required = true,
        ondelete = "restrict",
        string = "Server action"
    )]
    pub ir_actions_server_id: Option<RecId>,
    /// The name, copied from the action (searchable and orderable on this table).
    #[field(char, string = "Name")]
    pub cron_name: Option<Arc<str>>,
    /// Who the action runs as.
    #[field(
        many2one,
        comodel = "res.users",
        required = true,
        string = "Scheduler User"
    )]
    pub user_id: Option<RecId>,
    /// Inactive schedules keep their job row, held.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Repeat every this many units.
    #[field(integer, default = 1, required = true, string = "Interval Number")]
    pub interval_number: i32,
    /// The unit of the interval.
    #[field(
        selection,
        selection = [
            ("minutes", "Minutes"), ("hours", "Hours"), ("days", "Days"),
            ("weeks", "Weeks"), ("months", "Months")
        ],
        default = "months",
        required = true,
        string = "Interval Unit"
    )]
    pub interval_type: Option<Arc<str>>,
    /// Next planned execution.
    #[field(datetime, required = true, string = "Next Execution Date")]
    pub nextcall: Option<DateTime>,
    /// The last successful run.
    #[field(datetime, string = "Last Execution Date")]
    pub lastcall: Option<DateTime>,
    /// 0 runs first, 9 last.
    #[field(integer, default = 5, string = "Priority")]
    pub priority: i32,
    /// Consecutive failures; reset on success.
    #[field(integer, default = 0, string = "Failure Count")]
    pub failure_count: i32,
    /// When the first of the current failures happened.
    #[field(datetime, string = "First Failure Date")]
    pub first_failure_date: Option<DateTime>,
}

/// `ir.cron.trigger`: an extra run of a cron at a given time.
#[model(
    name = "ir.cron.trigger",
    description = "Triggered Action",
    allow_sudo_commands = false
)]
pub struct IrCronTrigger {
    /// The cron.
    #[field(many2one, comodel = "ir.cron", required = true, ondelete = "cascade")]
    pub cron_id: Option<RecId>,
    /// When it runs.
    #[field(datetime, required = true, string = "Call At")]
    pub call_at: Option<DateTime>,
}

/// The refusal while's elevation hook does not exist (lead decision Q8).
fn needs_enqueue_as() -> Error {
    Error::access(
        "choosing another user than your own for a scheduled action needs the 'jobs.enqueue_as' \
         elevation, which is recorded by the audit trail of a later release; that hook is not available \
         yet, so the change is refused",
    )
}

fn running_error() -> Error {
    Error::user(
        "Record cannot be modified right now: This cron task is currently being executed and \
         may not be modified Please try again in a few minutes",
    )
}

fn ids_of(records: &Records) -> Result<Vec<i64>> {
    records
        .rec_ids()
        .iter()
        .map(|id| {
            crate::group_graph::row_id(*id).ok_or_else(|| {
                Error::programming("a scheduled action has no database row; save it first")
            })
        })
        .collect()
}

async fn lock_jobs(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    let keys: Vec<String> = ids.iter().map(|id| format!("{CRON_KIND}:{id}")).collect();
    match tx
        .query(
            "SELECT 1 FROM erp_job WHERE kind = $1 AND dedup_key = ANY($2) FOR UPDATE NOWAIT",
            &[&CRON_KIND, &keys],
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if error.sqlstate() == Some("55P03") => Err(running_error()),
        Err(error) => Err(error),
    }
}

/// Closed by default (Q8): after the change, no cron it touched may belong to another user than
/// the caller unless the caller is the system principal. Reads the stored `user_id`, so no form
/// of the value (an id, a null, a number or text, a default, a later hook) slips past a check
/// of the incoming cell.
async fn refuse_other_users(tx: &mut Transaction, env: &Env, ids: &[i64]) -> Result<()> {
    if env.principal().kind() == PrincipalKind::System || ids.is_empty() {
        return Ok(());
    }
    let other = tx
        .query(
            "SELECT 1 FROM ir_cron WHERE id = ANY($1) AND user_id IS DISTINCT FROM $2",
            &[&ids, &env.uid()],
        )
        .await?;
    if other.is_empty() {
        Ok(())
    } else {
        Err(needs_enqueue_as())
    }
}

/// Whether the job row of `id` is running on a live lease.
async fn is_running(tx: &mut Transaction, id: i64) -> Result<bool> {
    let key = format!("{CRON_KIND}:{id}");
    let rows = tx
        .query(
            "SELECT 1 FROM erp_job WHERE kind = $1 AND dedup_key = $2 AND state = 'running' \
             AND lease_until > clock_timestamp()",
            &[&CRON_KIND, &key],
        )
        .await?;
    Ok(!rows.is_empty())
}

/// Make the job row of cron `id` match the cron: due at the earlier of `nextcall` and its
/// earliest trigger, the cron user's principal, held when inactive.
pub async fn sync_job(tx: &mut Transaction, env: &Env, id: i64) -> Result<()> {
    let rows = tx
        .query(
            "SELECT c.user_id, c.active, c.priority, \
             (extract(epoch FROM LEAST(c.nextcall, \
                (SELECT min(t.call_at) FROM ir_cron_trigger t WHERE t.cron_id = c.id)) \
              AT TIME ZONE 'UTC') * 1000000)::int8 \
             FROM ir_cron c WHERE c.id = $1",
            &[&id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let user: i64 = row.get(0);
    let active: bool = row.get(1);
    let priority: i32 = row.get(2);
    let due: i64 = row.get(3);
    let at = TimestampTz::from_unix_micros(due)?;
    let user = UserId::new(user).map_err(|e| Error::validation(e.to_string()))?;
    let spec = RecurringSpec::new(CRON_KIND, job_key(id)?)
        .first_run(When::At(at))
        .priority(erp_sdk::jobs::Priority::new(
            u8::try_from(priority.clamp(0, 9)).unwrap_or(5),
        )?)
        .held(!active)
        .run_as(user)
        .payload(&erp_sdk::serde_json::json!({ "cron_id": id }))?;
    let system = env.with_user(SUPERUSER_ID.get());
    tx.job_upsert_recurring(&system, spec).await?;
    Ok(())
}

#[methods(IrCron)]
impl IrCron {
    /// Default the user and usage, refuse another user than the caller's (until), then
    /// give every new cron its job row.
    #[method(override)]
    async fn create(
        this: &RecordSet<IrCron>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.as_records().env().clone();
        let system = env.principal().kind() == PrincipalKind::System;
        for vals in &mut vals_list {
            match vals.get("user_id") {
                Some(Cell::Id(user)) if *user == env.uid() || system => {}
                // any other form of the value is refused for a caller that is not the system
                Some(_) if !system => return Err(needs_enqueue_as()),
                Some(_) => {}
                None => {
                    *vals = std::mem::take(vals).set_cell("user_id", Cell::Id(env.uid()));
                }
            }
            if !vals.contains("nextcall") {
                *vals = std::mem::take(vals).set_cell("nextcall", Cell::DateTime(DateTime::now()));
            }
            if !vals.contains("usage") {
                *vals = std::mem::take(vals).set_cell("usage", Cell::Str("ir_cron".into()));
            }
            if let Some(Cell::Str(name)) = vals.get("name").cloned() {
                *vals = std::mem::take(vals).set_cell("cron_name", Cell::Str(name));
            }
        }
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        tx.flush_model(created.spec().id).await?;
        let new_ids = ids_of(&created)?;
        refuse_other_users(tx, &env, &new_ids).await?;
        for id in new_ids {
            sync_job(tx, &env, id).await?;
        }
        Ok(created)
    }

    /// Refuse while the cron runs, keep the job row in step with what changed.
    #[method(override)]
    async fn write(
        this: &RecordSet<IrCron>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let env = this.as_records().env().clone();
        let ids = ids_of(this.as_records())?;
        lock_jobs(tx, &ids).await?;
        for id in &ids {
            if is_running(tx, *id).await? {
                return Err(running_error());
            }
        }
        if vals.contains("user_id")
            && !matches!(vals.get("user_id"), Some(Cell::Id(user)) if *user == env.uid())
            && env.principal().kind() != PrincipalKind::System
        {
            return Err(needs_enqueue_as());
        }
        let schedule = [
            "nextcall",
            "active",
            "priority",
            "user_id",
            "interval_number",
            "interval_type",
        ]
        .iter()
        .any(|f| vals.contains(f));
        sup.call(tx, this.as_records(), (vals,)).await?;
        if env.principal().kind() != PrincipalKind::System {
            tx.flush_model(this.as_records().spec().id).await?;
            refuse_other_users(tx, &env, &ids).await?;
        }
        if schedule {
            tx.flush_model(this.as_records().spec().id).await?;
            for id in ids {
                sync_job(tx, &env, id).await?;
            }
        }
        Ok(())
    }

    /// Refuse while the cron runs, then remove its job row.
    #[method(override)]
    async fn unlink(this: &RecordSet<IrCron>, tx: &mut Transaction, sup: Super) -> Result<()> {
        let ids = ids_of(this.as_records())?;
        lock_jobs(tx, &ids).await?;
        for id in &ids {
            if is_running(tx, *id).await? {
                return Err(running_error());
            }
        }
        sup.call(tx, this.as_records(), ()).await?;
        let keys: Vec<String> = ids.iter().map(|id| format!("{CRON_KIND}:{id}")).collect();
        tx.execute(
            "DELETE FROM erp_job WHERE kind = $1 AND dedup_key = ANY($2) AND state <> 'running'",
            &[&CRON_KIND, &keys],
        )
        .await?;
        Ok(())
    }

    /// Extra runs of this cron at `at` (microseconds since the Unix epoch, UTC; `None` is now),
    /// each rounded up to the end of a `coalesce` minute window when `coalesce` is positive.
    /// An inactive cron drops the triggers that are due already. Returns how many were made.
    #[method]
    async fn trigger(
        this: &RecordSet<IrCron>,
        tx: &mut Transaction,
        at: Option<Vec<i64>>,
        coalesce: i64,
    ) -> Result<i64> {
        let env = this.as_records().env().clone();
        let id = ids_of(this.as_records())?
            .first()
            .copied()
            .ok_or_else(|| Error::programming("trigger needs one scheduled action"))?;
        let now: i64 = tx
            .query(
                "SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::int8",
                &[],
            )
            .await?[0]
            .get(0);
        let active: bool = tx
            .query("SELECT active FROM ir_cron WHERE id = $1", &[&id])
            .await?
            .first()
            .is_some_and(|r| r.get::<_, bool>(0));
        let mut times = at.unwrap_or_else(|| vec![now]);
        if coalesce > 0 {
            let factor = coalesce.saturating_mul(60_000_000);
            for t in &mut times {
                *t = t.saturating_add(factor - 1) / factor * factor;
            }
        }
        if !active {
            times.retain(|t| *t > now);
        }
        if times.is_empty() {
            return Ok(0);
        }
        for t in &times {
            tx.execute(
                "INSERT INTO ir_cron_trigger (cron_id, call_at) \
                 VALUES ($1, to_timestamp($2::float8 / 1000000) AT TIME ZONE 'UTC')",
                &[&id, &(*t as f64)],
            )
            .await?;
        }
        let earliest = times.iter().copied().min().unwrap_or(now);
        let key = format!("{CRON_KIND}:{id}");
        tx.execute(
            "UPDATE erp_job SET run_after = LEAST(run_after, to_timestamp($3::float8 / 1000000)) \
             WHERE kind = $1 AND dedup_key = $2 AND state IN ('ready', 'held')",
            &[&CRON_KIND, &key, &(earliest as f64)],
        )
        .await?;
        if earliest <= now {
            tx.job_wake().await?;
        }
        let _ = env;
        Ok(i64::try_from(times.len()).unwrap_or(i64::MAX))
    }

    /// Run the cron as soon as a worker is free (the admin button). The caller needs write
    /// access to the scheduled action.
    #[method]
    async fn method_direct_trigger(this: &RecordSet<IrCron>, tx: &mut Transaction) -> Result<bool> {
        this.ensure_one()?;
        this.as_records().check_access(tx, Operation::Write).await?;
        let sudo = this.as_records().sudo(true);
        let created: i64 = sudo
            .call::<(Option<Vec<i64>>, i64), i64>(tx, "trigger", (None, 0))
            .await?;
        Ok(created > 0)
    }

    /// Tell an administrator something about this cron. The base only logs; the mail addon
    /// overrides it to send a message.
    #[method]
    async fn _notify_admin(
        _this: &RecordSet<IrCron>,
        _tx: &mut Transaction,
        message: String,
    ) -> Result<()> {
        tracing::warn!(target: "erp::cron", "{message}");
        Ok(())
    }
}
