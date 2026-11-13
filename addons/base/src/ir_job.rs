//! `ir.job` and `ir.job.run` models over `erp_job` and `erp_job_run`.
//!
//! An administrative projection of the durable job queue and finished runs:
//! - [`IrJob`]: active background jobs (`ready`, `running`, `held`, `dead`). Direct CRUD is refused;
//!   operators manage jobs through explicit action methods (`action_hold`, `action_resume`, `action_cancel`,
//!   `action_requeue`, `action_discard`), which enforce the `jobs.admin` capability.
//! - [`IrJobRun`]: completed execution history (`done`, `cancelled`, `dead_discarded`, `dead_expired`).

use std::sync::Arc;

use erp_sdk::jobs::JobRef;
use erp_sdk::prelude::*;
use erp_sdk::{DateTime, Error, Result, Uuid};

fn read_only_refusal(model: &str) -> Error {
    Error::user(format!(
        "{model} records are read-only; manage background jobs through operator actions"
    ))
}

/// The requested jobs as queue references. A job that finished or was removed since the list was
/// read is refused by id, so an operator never believes an action ran for it.
async fn live_jobs(tx: &mut Transaction, this: &RecordSet<IrJob>) -> Result<Vec<JobRef>> {
    let ids: Vec<i64> = this
        .as_records()
        .rec_ids()
        .iter()
        .map(|r| r.get())
        .collect();
    let rows = tx
        .query(
            "SELECT id, job_uuid FROM erp_job WHERE id = ANY($1)",
            &[&ids],
        )
        .await?;
    let found: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
    let missing: Vec<String> = ids
        .iter()
        .filter(|id| !found.contains(id))
        .map(ToString::to_string)
        .collect();
    if !missing.is_empty() {
        return Err(Error::user(format!(
            "job(s) {} already finished or removed",
            missing.join(", ")
        )));
    }
    Ok(rows
        .iter()
        .map(|row| JobRef::new(row.get(0), row.get::<_, Uuid>(1)))
        .collect())
}

/// `ir.job`: active queue jobs from `erp_job` via view `ir_job`.
#[model(
    name = "ir.job",
    description = "Queue Job",
    log_access = false,
    order = "priority asc, run_after asc, id desc"
)]
pub struct IrJob {
    /// External UUID identifier (text: the human UI has no uuid renderer).
    #[field(char, readonly = true, string = "Job UUID")]
    pub job_uuid: Option<Arc<str>>,
    /// Registered job kind.
    #[field(char, readonly = true, string = "Kind")]
    pub kind: Option<Arc<str>>,
    /// Schema/handler version of the kind.
    #[field(integer, default = 1, readonly = true, string = "Kind Version")]
    pub kind_version: i32,
    /// Lifecycle state.
    #[field(
        selection,
        selection = [
            ("ready", "Ready"),
            ("running", "Running"),
            ("held", "Held"),
            ("dead", "Dead")
        ],
        default = "ready",
        readonly = true,
        string = "State"
    )]
    pub state: Option<Arc<str>>,
    /// Multi-node acquisition ordering rank.
    #[field(integer, default = 0, readonly = true, string = "Failure Rank")]
    pub failure_rank: i32,
    /// Execution priority (0 highest, 9 lowest).
    #[field(integer, default = 5, readonly = true, string = "Priority")]
    pub priority: i32,
    /// Earliest execution timestamp.
    #[field(datetime, readonly = true, string = "Run After")]
    pub run_after: Option<DateTime>,
    /// Enqueued timestamp.
    #[field(datetime, readonly = true, string = "Enqueued At")]
    pub enqueued_at: Option<DateTime>,
    /// Running started timestamp.
    #[field(datetime, readonly = true, string = "Started At")]
    pub started_at: Option<DateTime>,
    /// Execution attempts count.
    #[field(integer, default = 0, readonly = true, string = "Attempts")]
    pub attempts: i32,
    /// Max retry attempts before dead-lettering.
    #[field(integer, default = 3, readonly = true, string = "Max Attempts")]
    pub max_attempts: i32,
    /// Lease expiration on database clock.
    #[field(datetime, readonly = true, string = "Lease Until")]
    pub lease_until: Option<DateTime>,
    /// Concurrency exclusion key.
    #[field(char, readonly = true, string = "Singleton Key")]
    pub singleton_key: Option<Arc<str>>,
    /// Deduplication key.
    #[field(char, readonly = true, string = "Dedup Key")]
    pub dedup_key: Option<Arc<str>>,
    /// Successful items count.
    #[field(integer, default = 0, readonly = true, string = "Items Done")]
    pub items_done: i32,
    /// Failed items count.
    #[field(integer, default = 0, readonly = true, string = "Items Failed")]
    pub items_failed: i32,
    /// Estimated items remaining.
    #[field(integer, readonly = true, string = "Items Remaining")]
    pub items_remaining: Option<i32>,
    /// Identity kind the job runs under.
    #[field(
        selection,
        selection = [
            ("user", "User"),
            ("agent", "Agent"),
            ("service", "Service"),
            ("system", "System")
        ],
        readonly = true,
        string = "Run As Kind"
    )]
    pub run_as_kind: Option<Arc<str>>,
    /// User the job runs under.
    #[field(
        many2one,
        comodel = "res.users",
        readonly = true,
        string = "Run As User"
    )]
    pub run_as_user: Option<RecId>,
    /// Company context the job runs under.
    #[field(
        many2one,
        comodel = "res.company",
        readonly = true,
        string = "Run As Company"
    )]
    pub run_as_company: Option<RecId>,
    /// User that enqueued the job.
    #[field(
        many2one,
        comodel = "res.users",
        readonly = true,
        string = "Enqueued By User"
    )]
    pub enqueued_by_user: Option<RecId>,
    /// Operator cancellation flag.
    #[field(boolean, default = false, readonly = true, string = "Cancel Requested")]
    pub cancel_requested: bool,
    /// Stable error code from the last failure.
    #[field(char, readonly = true, string = "Last Error Code")]
    pub last_error_code: Option<Arc<str>>,
    /// Detailed error message from the last failure.
    #[field(text, readonly = true, string = "Last Error")]
    pub last_error: Option<Arc<str>>,
    /// Timestamp of the last failure.
    #[field(datetime, readonly = true, string = "Last Error At")]
    pub last_error_at: Option<DateTime>,
    /// Timestamp when moved to dead state.
    #[field(datetime, readonly = true, string = "Dead At")]
    pub dead_at: Option<DateTime>,
}

#[methods(IrJob, addon = "base")]
impl IrJob {
    #[method(override)]
    async fn create(
        _this: &RecordSet<IrJob>,
        _tx: &mut Transaction,
        _vals_list: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        Err(read_only_refusal("ir.job"))
    }

    #[method(override)]
    async fn write(
        _this: &RecordSet<IrJob>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(read_only_refusal("ir.job"))
    }

    #[method(override)]
    async fn unlink(_this: &RecordSet<IrJob>, _tx: &mut Transaction, _sup: Super) -> Result<()> {
        Err(read_only_refusal("ir.job"))
    }

    #[method(override)]
    async fn upsert(
        _this: &RecordSet<IrJob>,
        _tx: &mut Transaction,
        _call: erp_sdk::UpsertCall,
        _sup: Super,
    ) -> Result<erp_sdk::UpsertResult> {
        Err(read_only_refusal("ir.job"))
    }

    /// Hold a ready job: parks execution until resumed.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_hold(this: &RecordSet<IrJob>, tx: &mut Transaction) -> Result<()> {
        let env = this.as_records().env().clone();
        for job in live_jobs(tx, this).await? {
            tx.job_hold(&env, job).await?;
        }
        tx.invalidate_records(this.as_records(), None).await?;
        Ok(())
    }

    /// Resume a held job: returns it to ready state.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_resume(this: &RecordSet<IrJob>, tx: &mut Transaction) -> Result<()> {
        let env = this.as_records().env().clone();
        for job in live_jobs(tx, this).await? {
            tx.job_resume(&env, job).await?;
        }
        tx.invalidate_records(this.as_records(), None).await?;
        Ok(())
    }

    /// Cancel a job: removes waiting jobs with history or requests cancellation of running jobs.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_cancel(this: &RecordSet<IrJob>, tx: &mut Transaction) -> Result<()> {
        let env = this.as_records().env().clone();
        for job in live_jobs(tx, this).await? {
            tx.job_cancel(&env, job).await?;
        }
        tx.invalidate_records(this.as_records(), None).await?;
        Ok(())
    }

    /// Requeue a dead-lettered job: resets attempts to 0 and returns it to ready state.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_requeue(this: &RecordSet<IrJob>, tx: &mut Transaction) -> Result<()> {
        let env = this.as_records().env().clone();
        for job in live_jobs(tx, this).await? {
            tx.job_requeue(&env, job).await?;
        }
        tx.invalidate_records(this.as_records(), None).await?;
        Ok(())
    }

    /// Discard a dead-lettered job: removes it and records dead_discarded history.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_discard(this: &RecordSet<IrJob>, tx: &mut Transaction) -> Result<()> {
        let env = this.as_records().env().clone();
        for job in live_jobs(tx, this).await? {
            tx.job_discard(&env, job).await?;
        }
        tx.invalidate_records(this.as_records(), None).await?;
        Ok(())
    }
}

/// `ir.job.run`: completed background job execution history from `erp_job_run` via view `ir_job_run`.
#[model(
    name = "ir.job.run",
    description = "Job Execution History",
    log_access = false,
    order = "finished_at desc, id desc"
)]
pub struct IrJobRun {
    /// External job UUID (text: the human UI has no uuid renderer).
    #[field(char, readonly = true, string = "Job UUID")]
    pub job_uuid: Option<Arc<str>>,
    /// Job kind.
    #[field(char, readonly = true, string = "Kind")]
    pub kind: Option<Arc<str>>,
    /// Schema/handler version.
    #[field(integer, default = 1, readonly = true, string = "Kind Version")]
    pub kind_version: i32,
    /// Terminal outcome.
    #[field(
        selection,
        selection = [
            ("done", "Done"),
            ("cancelled", "Cancelled"),
            ("dead_discarded", "Discarded"),
            ("dead_expired", "Expired")
        ],
        readonly = true,
        string = "Outcome"
    )]
    pub outcome: Option<Arc<str>>,
    /// Attempts consumed.
    #[field(integer, default = 0, readonly = true, string = "Attempts")]
    pub attempts: i32,
    /// Enqueued timestamp.
    #[field(datetime, readonly = true, string = "Enqueued At")]
    pub enqueued_at: Option<DateTime>,
    /// Execution started timestamp.
    #[field(datetime, readonly = true, string = "Started At")]
    pub started_at: Option<DateTime>,
    /// Execution finished timestamp.
    #[field(datetime, readonly = true, string = "Finished At")]
    pub finished_at: Option<DateTime>,
    /// Total items successfully processed.
    #[field(integer, default = 0, readonly = true, string = "Items Done")]
    pub items_done: i32,
    /// Total items failed.
    #[field(integer, default = 0, readonly = true, string = "Items Failed")]
    pub items_failed: i32,
    /// Principal kind the job executed under.
    #[field(char, readonly = true, string = "Run As Kind")]
    pub run_as_kind: Option<Arc<str>>,
    /// User the job executed under.
    #[field(
        many2one,
        comodel = "res.users",
        readonly = true,
        string = "Run As User"
    )]
    pub run_as_user: Option<RecId>,
    /// User that enqueued the job.
    #[field(
        many2one,
        comodel = "res.users",
        readonly = true,
        string = "Enqueued By User"
    )]
    pub enqueued_by_user: Option<RecId>,
    /// Terminal error code if failed/cancelled.
    #[field(char, readonly = true, string = "Last Error Code")]
    pub last_error_code: Option<Arc<str>>,
}

#[methods(IrJobRun, addon = "base")]
impl IrJobRun {
    #[method(override)]
    async fn create(
        _this: &RecordSet<IrJobRun>,
        _tx: &mut Transaction,
        _vals_list: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        Err(read_only_refusal("ir.job.run"))
    }

    #[method(override)]
    async fn write(
        _this: &RecordSet<IrJobRun>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(read_only_refusal("ir.job.run"))
    }

    #[method(override)]
    async fn unlink(_this: &RecordSet<IrJobRun>, _tx: &mut Transaction, _sup: Super) -> Result<()> {
        Err(read_only_refusal("ir.job.run"))
    }

    #[method(override)]
    async fn upsert(
        _this: &RecordSet<IrJobRun>,
        _tx: &mut Transaction,
        _call: erp_sdk::UpsertCall,
        _sup: Super,
    ) -> Result<erp_sdk::UpsertResult> {
        Err(read_only_refusal("ir.job.run"))
    }
}
