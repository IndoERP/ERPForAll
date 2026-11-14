//! What a running job's handler sees: chunks, per-item isolation, external steps and
//! checkpoints (design §5.3 to §5.5).
//!
//! A chunk is one transaction: it opens for the stored principal, takes the fence (renews the
//! lease, checks the epoch, locks the job's row, reads the cancel flag), runs the handler's work,
//! and commits the job's new progress together with that work. The fence's row lock lasts to the
//! commit, so nobody can reap, claim or change the job while the chunk runs, and the checkpoint
//! (or completion) that follows cannot miss its row.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use erp_fields::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_postgres::types::{ToSql, Type};

use super::claim::{ClaimedJob, claim_options};
use super::runner::Inner;
use super::transition::{DONE_DROP_SQL, DONE_KEEP_SQL};
use super::{HistoryPolicy, JobError, JobKindDecl, JobRef, KindIsolation, PrincipalPolicy};
use crate::principal::{PrincipalKind, SUPERUSER_ID};
use crate::transaction::{CommitError, is_retryable};
use crate::{
    BeginOptions, BoxFuture, CorrelationId, FinalStatement, Isolation, OperationIdentity,
    PrincipalHint, Timeouts, Transaction,
};

/// Attempts of one chunk on serialization failures and deadlocks before they count as a
/// transient failure of the job (2).
const MAX_CHUNK_ATTEMPTS: u32 = 5;

/// What a chunk's work returns: the job's new progress and how far it got.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ChunkResult<P> {
    /// Replaces the stored progress; at most `jobs.progress_max_bytes` once serialized.
    pub progress: P,
    /// Items done by this chunk.
    pub processed: u64,
    /// Items left, when known.
    pub remaining: Option<u64>,
    /// True when the job is complete after this chunk: the chunk commits its completion.
    pub finished: bool,
}

impl<P> ChunkResult<P> {
    /// More work remains.
    pub fn more(progress: P, processed: u64, remaining: Option<u64>) -> Self {
        Self {
            progress,
            processed,
            remaining,
            finished: false,
        }
    }

    /// The job is complete after this chunk.
    pub fn done(progress: P, processed: u64) -> Self {
        Self {
            progress,
            processed,
            remaining: Some(0),
            finished: true,
        }
    }
}

/// What a committed chunk reports back to the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChunkReport {
    /// Items done by the chunk.
    pub processed: u64,
    /// Items that failed individually and were recorded ([`JobContext::chunk_items`]).
    pub failed: u64,
    /// Items left, when the chunk said.
    pub remaining: Option<u64>,
    /// The chunk completed the job: it is gone from the queue.
    pub finished: bool,
    /// Statements the chunk's transaction sent, the fence and the checkpoint or completion
    /// included.
    pub statements: u64,
    /// Times the chunk ran again after a serialization failure or deadlock.
    pub retries: u32,
}

/// Tells an external step that its job lost its lease or was cancelled. Poll it between items:
/// the step stops writing as soon as it is set.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
    lease_lost: Arc<AtomicBool>,
}

impl CancelToken {
    /// Whether the step must stop: the lease was lost or an operator cancelled the job.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Whether the lease was lost (another worker owns the job).
    pub fn lease_lost(&self) -> bool {
        self.lease_lost.load(Ordering::Acquire)
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn lose_lease(&self) {
        self.lease_lost.store(true, Ordering::Release);
        self.cancel();
    }
}

/// The identity of an item of a [`JobContext::chunk_items`] chunk, as stored with its failure:
/// 1..=200 bytes.
pub trait ItemKey {
    /// The key text.
    fn item_key(&self) -> String;
}

impl ItemKey for i64 {
    fn item_key(&self) -> String {
        self.to_string()
    }
}

impl ItemKey for u64 {
    fn item_key(&self) -> String {
        self.to_string()
    }
}

impl ItemKey for i32 {
    fn item_key(&self) -> String {
        self.to_string()
    }
}

impl ItemKey for String {
    fn item_key(&self) -> String {
        self.clone()
    }
}

impl ItemKey for &str {
    fn item_key(&self) -> String {
        (*self).to_owned()
    }
}

/// The work of a [`JobContext::chunk_items`] chunk: the items, and what the job's progress
/// becomes when they are done.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ItemsChunk<'a, I, P> {
    items: &'a [I],
    progress: P,
    remaining: Option<u64>,
    finished: bool,
}

impl<'a, I, P> ItemsChunk<'a, I, P> {
    /// A chunk of `items` after which the progress is `progress`.
    pub fn new(items: &'a [I], progress: P) -> Self {
        Self {
            items,
            progress,
            remaining: None,
            finished: false,
        }
    }

    /// Items left after this chunk.
    #[must_use]
    pub fn remaining(mut self, remaining: u64) -> Self {
        self.remaining = Some(remaining);
        self
    }

    /// The job is complete after this chunk.
    #[must_use]
    pub fn finished(mut self) -> Self {
        self.finished = true;
        self.remaining = Some(0);
        self
    }
}

/// An item that failed on its own.
#[derive(Debug, Clone)]
pub(crate) struct ItemFailure {
    key: String,
    code: String,
    message: String,
}

/// What a chunk body produced, before the job's statements.
pub(crate) struct BodyOutput {
    progress: serde_json::Value,
    processed: u64,
    remaining: Option<u64>,
    finished: bool,
    failures: Vec<ItemFailure>,
}

/// The work of one chunk, whatever its shape.
pub(crate) trait ChunkBody: Send {
    fn run<'a>(
        &'a mut self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, std::result::Result<BodyOutput, JobError>>;
}

struct ClosureBody<F, P> {
    f: F,
    _progress: PhantomData<fn() -> P>,
}

impl<F, P> ChunkBody for ClosureBody<F, P>
where
    F: for<'t> FnMut(
            &'t mut Transaction,
        ) -> BoxFuture<'t, std::result::Result<ChunkResult<P>, JobError>>
        + Send,
    P: Serialize + Send,
{
    fn run<'a>(
        &'a mut self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, std::result::Result<BodyOutput, JobError>> {
        Box::pin(async move {
            let result = (self.f)(tx).await?;
            let progress = serde_json::to_value(&result.progress).map_err(|_| {
                JobError::permanent(
                    "progress_unserializable",
                    "the progress of a chunk could not be serialized to JSON",
                )
            })?;
            Ok(BodyOutput {
                progress,
                processed: result.processed,
                remaining: result.remaining,
                finished: result.finished,
                failures: Vec::new(),
            })
        })
    }
}

struct ItemsBody<'a, I, P, F> {
    chunk: ItemsChunk<'a, I, P>,
    f: F,
}

impl<I, P, F> ChunkBody for ItemsBody<'_, I, P, F>
where
    I: ItemKey + Clone + Sync + Send,
    P: Serialize + Send,
    F: for<'t> FnMut(
            &'t mut Transaction,
            Vec<I>,
        ) -> BoxFuture<'t, std::result::Result<(), JobError>>
        + Send,
{
    fn run<'a>(
        &'a mut self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, std::result::Result<BodyOutput, JobError>> {
        Box::pin(async move {
            let items = self.chunk.items;
            let progress = serde_json::to_value(&self.chunk.progress).map_err(|_| {
                JobError::permanent(
                    "progress_unserializable",
                    "the progress of a chunk could not be serialized to JSON",
                )
            })?;
            let mut failures: Vec<ItemFailure> = Vec::new();
            let mut first_error: Option<JobError> = None;
            // optimistic: the whole chunk set-based, inside a savepoint so a failure leaves the
            // transaction usable for the per-item pass
            let whole = savepoint_job(tx, |tx| (self.f)(tx, items.to_vec())).await;
            let mut done = items.len();
            if let Err(error) = whole {
                if !is_item_attributable(&error) {
                    return Err(error);
                }
                done = 0;
                for item in items {
                    match savepoint_job(tx, |tx| (self.f)(tx, vec![item.clone()])).await {
                        Ok(()) => done += 1,
                        Err(error) if is_item_attributable(&error) => {
                            let (code, message) = error
                                .stored()
                                .unwrap_or(("item", "the item failed".to_owned()));
                            failures.push(ItemFailure {
                                key: item.item_key(),
                                code: super::error::bounded_code(code),
                                message: super::error::bounded(message).into_string(),
                            });
                            first_error.get_or_insert(error);
                        }
                        Err(error) => return Err(error),
                    }
                }
                if done == 0 {
                    // every item failed: that is a failure of the chunk, not a success with no
                    // items (3)
                    return Err(first_error.unwrap_or_else(|| {
                        JobError::permanent("item", "every item of the chunk failed")
                    }));
                }
            }
            Ok(BodyOutput {
                progress,
                processed: done as u64,
                remaining: self.chunk.remaining,
                finished: self.chunk.finished,
                failures,
            })
        })
    }
}

/// Run `work` in a savepoint, returning its [`JobError`] unchanged after the rollback to the
/// savepoint (the savepoint API carries `erp_fields::Error`, so the job error rides beside it).
async fn savepoint_job<'t, W>(tx: &'t mut Transaction, work: W) -> std::result::Result<(), JobError>
where
    W: for<'a> FnOnce(&'a mut Transaction) -> BoxFuture<'a, std::result::Result<(), JobError>>,
{
    let mut carried: Option<JobError> = None;
    let outcome = tx
        .savepoint(async |tx| match work(tx).await {
            Ok(()) => Ok(()),
            Err(error) => {
                carried = Some(error);
                Err(Error::user("a job item failed"))
            }
        })
        .await;
    match (outcome, carried) {
        (Ok(()), _) => Ok(()),
        (Err(_), Some(error)) => Err(error),
        (Err(error), None) => Err(JobError::from(error)),
    }
}

/// Whether a failure belongs to the items of a chunk (a constraint, a check or a data error, or
/// a failure the handler marked [`JobError::Item`]) rather than to the chunk (a lost
/// connection, a lock timeout, a coding error).
pub(crate) fn is_item_attributable(error: &JobError) -> bool {
    match error {
        JobError::Item { .. } => true,
        JobError::Permanent { source, .. } | JobError::Transient { source, .. } => source
            .as_deref()
            .is_some_and(|e| {
                matches!(e.sqlstate(), Some(state) if state.starts_with("22") || state.starts_with("23"))
                    || matches!(
                        e.kind(),
                        erp_fields::ErrorKind::Validation
                            | erp_fields::ErrorKind::UserError
                            | erp_fields::ErrorKind::Missing
                    )
            }),
        _ => false,
    }
}

/// A chunk attempt's failure: run it again, or report to the runner.
enum ChunkFailure {
    Retry(Error),
    Job(JobError),
}

impl From<JobError> for ChunkFailure {
    fn from(error: JobError) -> Self {
        // a serialization failure inside the work is retried like one of a statement
        if let JobError::Transient {
            source: Some(source),
            ..
        } = &error
            && is_retryable(source.as_ref())
        {
            return Self::Retry(source.as_ref().clone_for_retry());
        }
        Self::Job(error)
    }
}

/// `Error` is not `Clone`; a retry only needs its kind, code and message.
trait CloneForRetry {
    fn clone_for_retry(&self) -> Error;
}

impl CloneForRetry for Error {
    fn clone_for_retry(&self) -> Error {
        Error::new(self.kind(), self.message().to_owned())
            .with_sqlstate(self.sqlstate().map(str::to_owned))
    }
}

/// The running job, as its handler sees it.
pub struct JobContext<'r> {
    pub(crate) inner: Arc<Inner>,
    pub(crate) claimed: ClaimedJob,
    pub(crate) identity: OperationIdentity,
    pub(crate) started: Instant,
    pub(crate) chunks: u32,
    pub(crate) progress: Option<serde_json::Value>,
    pub(crate) completed: bool,
    pub(crate) rank: i16,
    pub(crate) processed_total: u64,
    pub(crate) failed_total: u64,
    pub(crate) cancel: CancelToken,
    _runner: PhantomData<&'r ()>,
}

impl std::fmt::Debug for JobContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobContext")
            .field("job", &self.claimed.job.uuid().to_string())
            .field("kind", &self.claimed.decl.name)
            .field("attempt", &self.claimed.attempt)
            .field("chunks", &self.chunks)
            .finish_non_exhaustive()
    }
}

const FENCE_SQL: &str = "\
UPDATE erp_job SET lease_until = clock_timestamp() + make_interval(secs => $3::float8)
 WHERE id = $1 AND lease_epoch = $2 AND state = 'running' AND lease_until > clock_timestamp()
RETURNING cancel_requested";

const SINGLETON_LOCK_SQL: &str = "SELECT pg_advisory_xact_lock(1703, hashtext($1::text))";

const CHECKPOINT_SQL: &str = "\
UPDATE erp_job SET progress = $3::jsonb, items_done = items_done + $4::int8,
       items_failed = items_failed + $5::int8, items_remaining = $6::int8, attempts = 0,
       failure_rank = $7::int2
 WHERE id = $1 AND lease_epoch = $2 AND state = 'running'";

const FAILURES_SQL: &str = "\
WITH existing AS (SELECT count(*) AS c FROM erp_job_item_failure WHERE job_uuid = $1)
INSERT INTO erp_job_item_failure (job_uuid, item_key, error_code, error)
SELECT $1, f.k, f.c, f.m FROM unnest($2::text[], $3::text[], $4::text[]) AS f(k, c, m)
 WHERE (SELECT c FROM existing) + $5::int8 <= $6::int8
ON CONFLICT (job_uuid, item_key) DO UPDATE
   SET error_code = EXCLUDED.error_code, error = EXCLUDED.error, failed_at = clock_timestamp()
RETURNING 1";

/// The renewal of an external step's lease: also the cancel flag read.
pub(crate) const RENEW_SQL: &str = "\
UPDATE erp_job SET lease_until = clock_timestamp() + make_interval(secs => $3::float8)
 WHERE id = $1 AND lease_epoch = $2 AND state = 'running' AND lease_until > clock_timestamp()
RETURNING cancel_requested";

/// A checkpoint outside a chunk: fence and progress in one statement.
const SHORT_CHECKPOINT_SQL: &str = "\
UPDATE erp_job SET progress = $4::jsonb, items_done = items_done + $5::int8, items_remaining = $6::int8,
       attempts = 0, lease_until = clock_timestamp() + make_interval(secs => $3::float8)
 WHERE id = $1 AND lease_epoch = $2 AND state = 'running' AND lease_until > clock_timestamp()
RETURNING cancel_requested";

impl<'r> JobContext<'r> {
    pub(crate) fn new(inner: Arc<Inner>, claimed: ClaimedJob) -> Result<Self> {
        let identity = match claimed.correlation {
            Some(root) => OperationIdentity::new(CorrelationId::parse(&root.to_string())?)?,
            None => OperationIdentity::generate()?,
        };
        let progress = claimed.progress.clone();
        Ok(Self {
            inner,
            claimed,
            identity,
            started: Instant::now(),
            chunks: 0,
            progress,
            completed: false,
            rank: 0,
            processed_total: 0,
            failed_total: 0,
            cancel: CancelToken::default(),
            _runner: PhantomData,
        })
    }

    /// Immutable effective policy captured when this run was claimed.
    pub fn settings(&self) -> &super::JobSettings {
        self.claimed.settings.settings()
    }

    /// Committed policy revision captured when this run was claimed.
    pub fn settings_revision(&self) -> u64 {
        self.claimed.settings.revision()
    }

    /// The job.
    pub fn job(&self) -> JobRef {
        self.claimed.job
    }

    /// The job's kind.
    pub fn kind(&self) -> &'static JobKindDecl {
        self.claimed.decl
    }

    /// The attempt number of this run, from 1.
    pub fn attempt(&self) -> u32 {
        self.claimed.attempt
    }

    /// Whether the previous run of this job ended because its lease expired (the reaper returned
    /// it): a run that timed out or whose process died.
    pub fn previous_run_expired(&self) -> bool {
        self.claimed.last_error_code.as_deref() == Some("lease_expired")
    }

    /// The version the job was enqueued at (what the handler must be able to read).
    pub fn kind_version(&self) -> u16 {
        self.claimed.kind_version
    }

    /// The job's payload, read as `T`.
    ///
    /// # Errors
    ///
    /// [`JobError::Payload`] when it does not have the shape `T` expects; the message names the
    /// kind, never the payload.
    pub fn payload<T: DeserializeOwned>(&self) -> std::result::Result<T, JobError> {
        serde_json::from_value(self.claimed.payload.clone()).map_err(|_| JobError::Payload {
            job: self.claimed.job,
            message: format!(
                "the payload does not have the shape a '{}' job reads",
                self.claimed.decl.name
            )
            .into(),
        })
    }

    /// The progress the last committed chunk stored, read as `T` (`None` before the first).
    ///
    /// # Errors
    ///
    /// [`JobError::Payload`] when it does not have the shape `T` expects.
    pub fn progress<T: DeserializeOwned>(&self) -> std::result::Result<Option<T>, JobError> {
        match &self.progress {
            None => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|_| JobError::Payload {
                    job: self.claimed.job,
                    message: format!(
                        "the stored progress does not have the shape a '{}' job reads",
                        self.claimed.decl.name
                    )
                    .into(),
                }),
        }
    }

    /// Time left of the run's budget (`jobs.run_budget_secs`); zero when it is used up.
    pub fn budget_left(&self) -> Duration {
        self.claimed
            .settings
            .settings()
            .run_budget()
            .saturating_sub(self.started.elapsed())
    }

    pub fn budget_exhausted(&self) -> bool {
        let settings = self.claimed.settings.settings();
        self.chunks >= settings.max_chunks_per_run()
            && self.started.elapsed() >= settings.run_budget()
    }

    /// Chunks committed in this run.
    pub fn chunks_run(&self) -> u32 {
        self.chunks
    }

    /// Items done in this run, counting the committed chunks.
    pub fn items_processed(&self) -> u64 {
        self.processed_total
    }

    /// The failure rank the job gets when this run puts it back to `ready` (the ordering key
    /// that sends healthy jobs before failing ones; `ir.cron` sets its failure count).
    pub fn set_failure_rank(&mut self, rank: u16) {
        self.rank = i16::try_from(rank).unwrap_or(i16::MAX);
    }

    /// The token an external step polls.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Whether a chunk has already completed the job (`finished`): nothing more may be done.
    pub fn is_completed(&self) -> bool {
        self.completed
    }

    /// Run one transactional chunk: the closure gets a transaction opened for the job's stored
    /// principal, after the fence. Its effects and the job's new progress commit together or not
    /// at all. A serialization failure or deadlock runs the closure again (up to five times, so
    /// it must not have effects outside the transaction); `finished` completes the job in the same
    /// commit.
    ///
    /// ```ignore
    /// let report = ctx
    ///     .chunk(|tx| Box::pin(async move {
    ///         let done = recount_batch(tx).await?;
    ///         Ok(ChunkResult::more(serde_json::json!({"next": done.next}), done.rows, None))
    ///     }))
    ///     .await?;
    /// ```
    ///
    /// # Errors
    ///
    /// [`JobError::LeaseLost`] when the lease or epoch moved on (nothing was written),
    /// [`JobError::Cancelled`] when an operator cancelled the job,
    /// [`JobError::PrincipalRevoked`] when the stored principal can no longer act, the closure's
    /// own error, and [`JobError::Transient`] for database failures.
    pub async fn chunk<F, P>(&mut self, f: F) -> std::result::Result<ChunkReport, JobError>
    where
        F: for<'t> FnMut(
                &'t mut Transaction,
            )
                -> BoxFuture<'t, std::result::Result<ChunkResult<P>, JobError>>
            + Send,
        P: Serialize + Send,
    {
        let mut body = ClosureBody {
            f,
            _progress: PhantomData,
        };
        self.run_chunk(&mut body).await
    }

    /// Run one transactional chunk over `items.items`, set-based first. When the chunk fails with
    /// an error that belongs to its items (a constraint or data error, or [`JobError::Item`]),
    /// the failed attempt is rolled back to a savepoint and the closure runs once more with one
    /// savepoint per item, called with a one-item vector: a failing item is rolled back, recorded
    /// in `erp_job_item_failure` and counted, the others commit with the progress. A chunk in
    /// which every item fails is a failure of the chunk (so the job retries or dead-letters
    /// like any failing run). More than `jobs.max_item_failures_per_job` recorded failures
    /// dead-letter the job ([`JobError::ItemFailuresExceeded`]).
    ///
    /// # Errors
    ///
    /// As [`JobContext::chunk`], plus [`JobError::ItemFailuresExceeded`].
    pub async fn chunk_items<I, P, F>(
        &mut self,
        items: ItemsChunk<'_, I, P>,
        f: F,
    ) -> std::result::Result<ChunkReport, JobError>
    where
        I: ItemKey + Clone + Sync + Send,
        P: Serialize + Send,
        F: for<'t> FnMut(
                &'t mut Transaction,
                Vec<I>,
            ) -> BoxFuture<'t, std::result::Result<(), JobError>>
            + Send,
    {
        let mut body = ItemsBody { chunk: items, f };
        self.run_chunk(&mut body).await
    }

    async fn run_chunk(
        &mut self,
        body: &mut dyn ChunkBody,
    ) -> std::result::Result<ChunkReport, JobError> {
        if self.completed {
            return Err(JobError::permanent(
                "chunk_after_completion",
                "a chunk was started after a chunk completed the job",
            ));
        }
        let mut retries = 0u32;
        loop {
            match self.chunk_attempt(body, retries).await {
                Ok(report) => return Ok(report),
                Err(ChunkFailure::Retry(error)) if retries + 1 < MAX_CHUNK_ATTEMPTS => {
                    retries += 1;
                    let bound = 20u64 << (retries - 1).min(5);
                    tokio::time::sleep(Duration::from_millis(crate::transaction::jitter(bound)))
                        .await;
                    drop(error);
                }
                Err(ChunkFailure::Retry(error)) => return Err(JobError::from(error)),
                Err(ChunkFailure::Job(error)) => return Err(error),
            }
        }
    }

    fn begin_options(&self) -> std::result::Result<BeginOptions, JobError> {
        let decl = self.claimed.decl;
        let settings = self.claimed.settings.settings();
        let (lock, statement, transaction) = settings.chunk_timeouts(decl.timeouts);
        let timeouts = Timeouts::new(lock, statement, transaction).map_err(JobError::from)?;
        let isolation = match decl.isolation {
            KindIsolation::RepeatableRead => Isolation::RepeatableRead,
            KindIsolation::ReadCommitted => Isolation::ReadCommitted,
        };
        let hint = match (decl.principal, self.claimed.principal.kind) {
            (PrincipalPolicy::System, _) | (_, PrincipalKind::System) => {
                PrincipalHint::user(SUPERUSER_ID)
            }
            (_, _) => PrincipalHint::user(self.claimed.principal.subject),
        };
        Ok(BeginOptions::new(hint)
            .operation_identity(self.identity.clone())
            .timeouts(timeouts)
            .isolation(isolation)
            .on_primary())
    }

    async fn chunk_attempt(
        &mut self,
        body: &mut dyn ChunkBody,
        retries: u32,
    ) -> std::result::Result<ChunkReport, ChunkFailure> {
        let job = self.claimed.job;
        let options = self.begin_options()?;
        let mut tx = match self.inner.orm.begin_opts(options).await {
            Ok(tx) => tx,
            Err(error) if error.kind() == erp_fields::ErrorKind::Access => {
                return Err(ChunkFailure::Job(JobError::PrincipalRevoked {
                    job,
                    reason: "subject_unavailable",
                }));
            }
            Err(error) if is_retryable(&error) => return Err(ChunkFailure::Retry(error)),
            Err(error) => return Err(ChunkFailure::Job(JobError::from(error))),
        };
        match self.chunk_in(&mut tx, body, retries).await {
            Ok(report) => Ok(report),
            Err(failure) => {
                if let Err(error) = tx.rollback().await {
                    tracing::warn!(
                        target: "erp::jobs",
                        job = %job.uuid(),
                        error = %error,
                        "the rollback of a failed chunk failed; the connection is discarded"
                    );
                }
                Err(failure)
            }
        }
    }

    /// Everything between `BEGIN` and the commit. A failure leaves the rollback to the caller.
    async fn chunk_in(
        &mut self,
        tx: &mut Transaction,
        body: &mut dyn ChunkBody,
        retries: u32,
    ) -> std::result::Result<ChunkReport, ChunkFailure> {
        let job = self.claimed.job;
        let id = job.id();
        let epoch = self.claimed.epoch;
        let lease = self.claimed.lease.as_secs_f64();
        let db = |error: Error| {
            if is_retryable(&error) {
                ChunkFailure::Retry(error)
            } else {
                ChunkFailure::Job(JobError::from(error))
            }
        };
        // the fence: renew the lease, check the epoch, lock the row, read the cancel flag
        let fence = tx
            .query(FENCE_SQL, &[&id, &epoch, &lease])
            .await
            .map_err(db)?;
        let Some(row) = fence.first() else {
            return Err(ChunkFailure::Job(JobError::LeaseLost { job }));
        };
        let cancel_requested: bool = row
            .try_get(0)
            .map_err(|e| db(Error::internal("the fence row did not decode").with_source(e)))?;
        if cancel_requested {
            self.cancel.cancel();
            return Err(ChunkFailure::Job(JobError::Cancelled { job }));
        }
        if let Some(key) = &self.claimed.singleton {
            tx.query(SINGLETON_LOCK_SQL, &[key]).await.map_err(db)?;
        }
        if self.claimed.principal.kind != PrincipalKind::System
            && self.claimed.decl.principal != PrincipalPolicy::System
        {
            let resolver = Arc::clone(&self.inner.resolver);
            match resolver.admit(tx, &self.claimed.principal).await {
                Ok(principal) => tx.replace_principal(principal),
                Err(refusal) => {
                    return Err(ChunkFailure::Job(JobError::PrincipalRevoked {
                        job,
                        reason: refusal.reason,
                    }));
                }
            }
        }
        let output = body.run(tx).await.map_err(ChunkFailure::from)?;
        let settings = self.claimed.settings.settings();
        let size = super::enqueue::jsonb_text_len(&output.progress);
        if size > settings.progress_max_bytes() {
            return Err(ChunkFailure::Job(JobError::permanent(
                "progress_too_large",
                format!(
                    "the progress of a chunk is {size} bytes; the limit is {} bytes \
                     (jobs.progress_max_bytes)",
                    settings.progress_max_bytes()
                ),
            )));
        }
        // one row per item key: a repeated key in a chunk (the same item listed twice) would make
        // the upsert touch a row twice (SQLSTATE 21000); the last failure of a key wins
        let mut unique: Vec<&ItemFailure> = Vec::with_capacity(output.failures.len());
        for failure in &output.failures {
            match unique.iter().position(|seen| seen.key == failure.key) {
                Some(at) => unique[at] = failure,
                None => unique.push(failure),
            }
        }
        let failed = unique.len() as u64;
        if !unique.is_empty() {
            let keys: Vec<&str> = unique.iter().map(|f| f.key.as_str()).collect();
            let codes: Vec<&str> = unique.iter().map(|f| f.code.as_str()).collect();
            let messages: Vec<&str> = unique.iter().map(|f| f.message.as_str()).collect();
            let count = i64::try_from(unique.len()).unwrap_or(i64::MAX);
            let cap = i64::from(settings.max_item_failures_per_job());
            let inserted = tx
                .query(
                    FAILURES_SQL,
                    &[&job.uuid(), &keys, &codes, &messages, &count, &cap],
                )
                .await
                .map_err(db)?;
            if inserted.len() < unique.len() {
                return Err(ChunkFailure::Job(JobError::ItemFailuresExceeded {
                    job,
                    limit: settings.max_item_failures_per_job(),
                }));
            }
        }
        let processed = i64::try_from(output.processed).unwrap_or(i64::MAX);
        let failed_i = i64::try_from(failed).unwrap_or(i64::MAX);
        let remaining = output
            .remaining
            .map(|r| i64::try_from(r).unwrap_or(i64::MAX));
        let rank = i32::from(self.rank);
        let statements = tx.query_count() + 1;
        let committed = if output.finished {
            let params: [(&(dyn ToSql + Sync), Type); 4] = [
                (&id, Type::INT8),
                (&epoch, Type::INT8),
                (&processed, Type::INT8),
                (&failed_i, Type::INT8),
            ];
            let sql = if self.claimed.decl.history == HistoryPolicy::Keep {
                DONE_KEEP_SQL
            } else {
                DONE_DROP_SQL
            };
            tx.commit_pipelined(Some(FinalStatement::new(sql, &params)))
                .await
        } else {
            let params: [(&(dyn ToSql + Sync), Type); 7] = [
                (&id, Type::INT8),
                (&epoch, Type::INT8),
                (&output.progress, Type::JSONB),
                (&processed, Type::INT8),
                (&failed_i, Type::INT8),
                (&remaining, Type::INT8),
                (&rank, Type::INT4),
            ];
            tx.commit_pipelined(Some(FinalStatement::new(CHECKPOINT_SQL, &params)))
                .await
        };
        match committed {
            Ok(()) => {}
            Err(CommitError::RolledBack(error)) => return Err(db(error)),
            Err(CommitError::Unknown(error)) => {
                return Err(ChunkFailure::Job(JobError::transient(
                    "commit_unknown",
                    format!(
                        "the commit of a chunk was not acknowledged and may or may not have \
                         been applied: {}",
                        error.message()
                    ),
                )));
            }
        }
        self.chunks += 1;
        self.processed_total += output.processed;
        self.failed_total += failed;
        self.progress = Some(output.progress);
        if output.finished {
            self.completed = true;
        }
        Ok(ChunkReport {
            processed: output.processed,
            failed,
            remaining: output.remaining,
            finished: output.finished,
            statements,
            retries,
        })
    }

    /// A short fenced transaction that only writes the job's progress and counters, for an
    /// external step that reports every `k` items. The lease renews with it.
    ///
    /// # Errors
    ///
    /// [`JobError::LeaseLost`] when the lease or epoch moved on, [`JobError::Cancelled`] when an
    /// operator cancelled the job, [`JobError::Permanent`] when the progress is over
    /// `jobs.progress_max_bytes` or does not serialize, and the database's errors as transient.
    pub async fn checkpoint<P: Serialize>(
        &mut self,
        progress: &P,
        processed: u64,
        remaining: Option<u64>,
    ) -> std::result::Result<(), JobError> {
        let job = self.claimed.job;
        let value = serde_json::to_value(progress).map_err(|_| {
            JobError::permanent(
                "progress_unserializable",
                "the progress of a checkpoint could not be serialized to JSON",
            )
        })?;
        let limit = self.claimed.settings.settings().progress_max_bytes();
        let size = super::enqueue::jsonb_text_len(&value);
        if size > limit {
            return Err(JobError::permanent(
                "progress_too_large",
                format!(
                    "the progress of a checkpoint is {size} bytes; the limit is {limit} bytes \
                     (jobs.progress_max_bytes)"
                ),
            ));
        }
        let id = job.id();
        let epoch = self.claimed.epoch;
        let lease = self.claimed.lease.as_secs_f64();
        let done = i64::try_from(processed).unwrap_or(i64::MAX);
        let left = remaining.map(|r| i64::try_from(r).unwrap_or(i64::MAX));
        let params: [(&(dyn ToSql + Sync), Type); 6] = [
            (&id, Type::INT8),
            (&epoch, Type::INT8),
            (&lease, Type::FLOAT8),
            (&value, Type::JSONB),
            (&done, Type::INT8),
            (&left, Type::INT8),
        ];
        let rows = self
            .inner
            .orm
            .one_flight(
                claim_options().map_err(JobError::from)?,
                FinalStatement::new(SHORT_CHECKPOINT_SQL, &params),
            )
            .await
            .map_err(|e| JobError::from(e.into_error()))?;
        let Some(row) = rows.first() else {
            self.cancel.lose_lease();
            return Err(JobError::LeaseLost { job });
        };
        let cancel_requested: bool = row.try_get(0).map_err(|e| {
            JobError::from(Error::internal("the checkpoint row did not decode").with_source(e))
        })?;
        if cancel_requested {
            self.cancel.cancel();
            return Err(JobError::Cancelled { job });
        }
        self.progress = Some(value);
        self.processed_total += processed;
        Ok(())
    }

    /// Run `f` outside any transaction (a mail relay, an HTTP call), renewing the lease every
    /// third of its length on its own connection. When a renewal finds the lease lost or the job
    /// cancelled, the token the step polls is set; the step stops writing and returns, and this
    /// call reports the loss. Record results with [`JobContext::checkpoint`] every few items:
    /// a crash repeats at most the items after the last checkpoint (at-least-once).
    ///
    /// # Errors
    ///
    /// [`JobError::LeaseLost`] or [`JobError::Cancelled`] when the renewals say so (even when the
    /// step returned a value), otherwise the step's own error.
    pub async fn external<F, R>(&mut self, f: F) -> std::result::Result<R, JobError>
    where
        F: AsyncFnOnce(CancelToken) -> std::result::Result<R, JobError>,
    {
        let token = self.cancel.clone();
        // owned by this call: a panic, a drop of the run (the job deadline aborts the task) or an
        // early return ends the renewals with it, so a run that was given up never keeps its
        // lease alive
        let renewer = RenewGuard(tokio::spawn(renew_loop(
            Arc::clone(&self.inner),
            self.claimed.job,
            self.claimed.epoch,
            self.claimed.lease,
            token.clone(),
        )));
        let result = f(token.clone()).await;
        renewer.stop().await;
        if token.lease_lost() {
            return Err(JobError::LeaseLost {
                job: self.claimed.job,
            });
        }
        if token.is_cancelled() {
            return Err(JobError::Cancelled {
                job: self.claimed.job,
            });
        }
        result
    }
}

/// The lease renewer of an external step; dropping it aborts the task.
struct RenewGuard(tokio::task::JoinHandle<()>);

impl RenewGuard {
    async fn stop(mut self) {
        self.0.abort();
        // the abort is joined so no renewal is in flight when the step's result is settled
        let _ = (&mut self.0).await;
    }
}

impl Drop for RenewGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Renew the lease of an external step every third of its length until aborted.
async fn renew_loop(
    inner: Arc<Inner>,
    job: JobRef,
    epoch: i64,
    lease: Duration,
    token: CancelToken,
) {
    let every = (lease / 3).max(Duration::from_millis(50));
    let id = job.id();
    let secs = lease.as_secs_f64();
    loop {
        tokio::time::sleep(every).await;
        let params: [(&(dyn ToSql + Sync), Type); 3] = [
            (&id, Type::INT8),
            (&epoch, Type::INT8),
            (&secs, Type::FLOAT8),
        ];
        let outcome = match claim_options() {
            Ok(options) => {
                inner
                    .orm
                    .one_flight(options, FinalStatement::new(RENEW_SQL, &params))
                    .await
            }
            Err(error) => Err(CommitError::RolledBack(error)),
        };
        match outcome {
            Ok(rows) => match rows.first() {
                None => {
                    token.lose_lease();
                    return;
                }
                Some(row) => match row.try_get::<_, bool>(0) {
                    Ok(true) => {
                        token.cancel();
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        // the flag is unreadable: stop the step rather than run on as if
                        // nobody cancelled
                        tracing::error!(
                            target: "erp::jobs",
                            job = %job.uuid(),
                            error = %error,
                            "the cancel flag of a job did not decode; the step is stopped"
                        );
                        token.cancel();
                        return;
                    }
                },
            },
            // a failed renewal is retried at the next tick; the lease absorbs it
            Err(error) => {
                tracing::warn!(
                    target: "erp::jobs",
                    job = %job.uuid(),
                    error = %error,
                    "a job lease renewal failed; retrying"
                );
            }
        }
    }
}
