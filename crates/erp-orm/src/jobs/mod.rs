//! The durable job queue (design).
//!
//! A job is a row of `erp_job` written inside the business transaction that caused it, claimed
//! by any worker under a lease on the database clock, run in short fenced chunks that write
//! their own progress with their effects, and ended done (history row), retried with backoff,
//! or dead-lettered for an operator. Nothing here polls `ir_cron`; `ir.cron` is one kind of job
//! (`addons/base`).
//!
//! - [`JobKindDecl`] and [`register_job_kind!`](crate::register_job_kind) declare kinds;
//! - [`Transaction::enqueue`](crate::Transaction::enqueue) adds jobs;
//! - [`JobSettings`] holds every bounded setting;
//! - [`JobRunner`] claims and runs jobs;
//! - [`JobPrincipalResolver`] decides who a job acts as at every chunk.

mod admin;
mod backoff;
mod claim;
mod context;
mod drive;
mod enqueue;
mod error;
mod grant;
mod kind;
mod principal;
mod retention;
mod runner;
pub(crate) mod schema;
mod settings;
mod transition;
mod wake;

pub use admin::{CancelOutcome, MAX_CANCEL_KEYS, RecurringSpec};
pub use backoff::Backoff;
pub use claim::ClaimedJob;
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub use claim::claim_statement_for_tests;
pub use context::{CancelToken, ChunkReport, ChunkResult, ItemKey, ItemsChunk, JobContext};
pub use drive::{JobRunnerHandle, TickReport};
pub use enqueue::{EnqueueOutcome, EnqueueRequest, MAX_ENQUEUE_BATCH, OnDuplicate, RunAs, When};
pub use error::{JobError, MAX_CODE_BYTES, MAX_ERROR_BYTES};
pub use grant::{JOB_TABLES as JOB_TABLE_NAMES, JOB_UPDATABLE_COLUMNS};
pub(crate) use kind::linked as linked_kinds;
pub use kind::{
    HandlerRef, HistoryPolicy, JOB_KINDS, JobKindDecl, JobKindRequirement, KindIsolation,
    OutboxTrigger, PrincipalPolicy, SYSTEM_PRINCIPAL_ADDONS, TimeoutClass, TriggerCoalesce,
    is_valid_kind_name, validate as validate_kinds,
};
pub use principal::{
    JobPrincipalResolver, MAX_STORED_COMPANIES, PrincipalRefusal, StoredPrincipal,
};
pub use retention::{
    RETENTION_BATCH, dead_expiry_decl, ensure_core_schedules, history_retention_decl,
};
#[cfg(feature = "test-faults")]
pub use runner::RunnerHooks;
pub use runner::{JobMetrics, JobRunner, JobRunnerBuilder, LiveNodes, ReapReport};
pub use settings::{JobSettings, JobSettingsBuilder, Setting};
pub use wake::JOBS_CHANNEL;
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub use wake::reset_notify_state_for_tests;

use std::fmt;

use erp_fields::{Error, Result, Uuid};

use crate::BoxFuture;

/// ```
/// use erp_orm::jobs::Priority;
/// assert_eq!(Priority::new(0)?.get(), 0);
/// assert_eq!(Priority::DEFAULT.get(), 5);
/// assert_eq!(Priority::new(10).unwrap_err().message(), "job priority 10 is outside 0..=9");
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(u8);

impl Priority {
    /// The default priority, 5.
    pub const DEFAULT: Self = Self(5);

    /// A priority.
    ///
    /// # Errors
    ///
    /// `Validation` above 9.
    pub fn new(priority: u8) -> Result<Self> {
        if priority > 9 {
            return Err(error::invalid_priority(priority));
        }
        Ok(Self(priority))
    }

    /// The value, `0..=9`.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A validated singleton or dedup key: 1..=200 bytes, no NUL, no control characters.
///
/// ```
/// use erp_orm::jobs::JobKey;
/// assert_eq!(JobKey::new("base.cron:42")?.as_str(), "base.cron:42");
/// assert!(JobKey::new("").is_err());
/// assert!(JobKey::new("a\nb").is_err());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobKey(Box<str>);

/// Longest key, in bytes.
pub const MAX_KEY_BYTES: usize = 200;

impl JobKey {
    /// A key.
    ///
    /// # Errors
    ///
    /// `Validation` for an empty key, one over 200 bytes, or one with a NUL or control character.
    pub fn new(key: &str) -> Result<Self> {
        if key.is_empty() {
            return Err(error::invalid_key("it is empty"));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(error::invalid_key(&format!("it is {} bytes", key.len())));
        }
        if key.chars().any(char::is_control) {
            return Err(error::invalid_key("it contains a control character"));
        }
        Ok(Self(key.into()))
    }

    /// The key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A job: its row id and its external identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobRef {
    id: i64,
    uuid: Uuid,
}

impl JobRef {
    /// A job reference by row id and uuid.
    pub fn new(id: i64, uuid: Uuid) -> Self {
        Self { id, uuid }
    }

    /// The row id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The job's UUID, which is what operators and logs name.
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }
}

/// The states of a live job (`erp_job.state`). A finished job leaves the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Waiting for its `run_after` and a worker.
    Ready,
    /// Claimed; the lease says until when.
    Running,
    /// Parked by an operator or by an inactive schedule; never claimed.
    Held,
    /// Failed for good; waits for an operator.
    Dead,
}

impl JobState {
    /// The column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Held => "held",
            Self::Dead => "dead",
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self> {
        match text {
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "held" => Ok(Self::Held),
            "dead" => Ok(Self::Dead),
            other => Err(Error::internal(format!(
                "erp_job holds the unknown state '{other}'"
            ))),
        }
    }
}

/// How a run ends when the handler returns normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunOutcome {
    /// All work is done: the job leaves the queue (with a history row unless the kind keeps
    /// none).
    Done,
    /// The run budget is used but work remains: the job goes back to `ready` at once, behind
    /// every job already due at its priority, with its attempts reset.
    Continue,
    /// A recurring job: `ready` again at the given instant, attempts reset.
    Reschedule(erp_fields::TimestampTz),
    /// A recurring job: `ready` again this long from now on the database clock, attempts reset
    /// (the core schedules, whose recurrence is a fixed duration).
    RescheduleAfter(std::time::Duration),
    /// Park the job (`held`): a schedule that deactivated itself. Attempts reset.
    Hold,
}

/// What a step method receives for one chunk.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StepArgs {
    /// The job's payload.
    pub payload: serde_json::Value,
    /// The progress the previous chunk stored (`None` before the first).
    pub progress: Option<serde_json::Value>,
    /// The attempt number of this run, from 1.
    pub attempt: u32,
}

/// What a step method returns for one chunk.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct StepOutcome {
    /// Replaces the stored progress; at most `jobs.progress_max_bytes`.
    pub progress: serde_json::Value,
    /// Items done in this chunk.
    pub processed: u64,
    /// Items left, when known.
    pub remaining: Option<u64>,
    /// True when the job is complete after this chunk.
    pub finished: bool,
}

impl StepOutcome {
    /// More work remains after this chunk.
    pub fn more(progress: serde_json::Value, processed: u64, remaining: Option<u64>) -> Self {
        Self {
            progress,
            processed,
            remaining,
            finished: false,
        }
    }

    /// The job is complete after this chunk.
    pub fn done(progress: serde_json::Value, processed: u64) -> Self {
        Self {
            progress,
            processed,
            remaining: Some(0),
            finished: true,
        }
    }
}

/// A native job handler: the full [`JobContext`] with chunks and external steps. Core crates
/// and native addons only.
pub trait JobHandler: Send + Sync + 'static {
    /// Run the job until the budget is used or the work is done.
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>>;
}
