//! Running claimed jobs and the runner's loop (D5, D6; design §5.3, §5.7).
//!
//! A run is one task per claimed job. Its handler's panic is caught at the task boundary and
//! counted as a transient failure, a hard deadline (`jobs.job_deadline_secs`) ends a run that
//! will not stop, and every way out of `running` is one epoch-guarded statement
//! ([`super::transition`]).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use erp_fields::{Error, Result};
use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::backoff::Backoff;
use super::claim::ClaimedJob;
use super::context::{ChunkResult, JobContext};
use super::runner::{Inner, JobRunner, default_backoff};
use super::transition::{Transition, apply};
use super::{HandlerRef, JobError, RunOutcome, StepArgs, StepOutcome};
use crate::Transaction;
use crate::dispatch::{call_method_json, chain_has_wasm};

/// What one [`JobRunner::tick`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TickReport {
    /// Jobs claimed and run to the end of their run.
    pub claimed: u32,
    /// Expired leases the reaper returned or dead-lettered in this tick.
    pub reaped: u32,
    /// Whether the node was held back (not ready), so nothing was claimed.
    pub held_back: bool,
}

/// Counts a job of a kind as running in this process for the length of its run.
struct RunningGuard {
    inner: Arc<Inner>,
    kind: &'static str,
}

impl RunningGuard {
    fn new(inner: Arc<Inner>, kind: &'static str) -> Self {
        *inner
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(kind)
            .or_insert(0) += 1;
        Self { inner, kind }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let mut running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = running.get_mut(self.kind) {
            *count = count.saturating_sub(1);
        }
    }
}

/// What a run leaves behind for the runner to act on.
struct RunState {
    completed: bool,
    rank: i16,
    /// A chunk committed in this run: the attempt counter restarted at that commit (
    /// D5.3), so a failure after it is the first attempt since progress.
    progressed: bool,
}

async fn drive(
    mut ctx: JobContext<'static>,
) -> (std::result::Result<RunOutcome, JobError>, RunState) {
    let result = match ctx.claimed.decl.handler {
        HandlerRef::Native(handler) => handler.run(&mut ctx).await,
        HandlerRef::Step { model, method } => run_step(&mut ctx, model, method).await,
    };
    let state = RunState {
        completed: ctx.completed,
        rank: ctx.rank,
        progressed: ctx.chunks > 0,
    };
    (result, state)
}

/// The chunk loop of a step kind: call the model method once per chunk until it says it is
/// finished or the run budget is used.
async fn run_step(
    ctx: &mut JobContext<'static>,
    model: &'static str,
    method: &'static str,
) -> std::result::Result<RunOutcome, JobError> {
    loop {
        let args = StepArgs {
            payload: ctx.claimed.payload.clone(),
            progress: ctx.progress.clone(),
            attempt: ctx.claimed.attempt,
        };
        let report = ctx
            .chunk(move |tx| {
                let args = args.clone();
                Box::pin(async move { call_step(tx, model, method, args).await })
            })
            .await?;
        if report.finished {
            return Ok(RunOutcome::Done);
        }
        if report.processed == 0 || ctx.cancel_token().is_cancelled() {
            // no progress: give the other due jobs their turn before this one tries again
            return Ok(RunOutcome::Continue);
        }
        if ctx.budget_exhausted() {
            return Ok(RunOutcome::Continue);
        }
    }
}

async fn call_step(
    tx: &mut Transaction,
    model: &'static str,
    method: &'static str,
    args: StepArgs,
) -> std::result::Result<ChunkResult<serde_json::Value>, JobError> {
    let records = tx.env().records(model).map_err(JobError::from)?;
    let outcome: StepOutcome = if chain_has_wasm(tx, records.model_id(), method) {
        let json = serde_json::to_string(&(args,)).map_err(|_| {
            JobError::permanent(
                "step_arguments",
                "the step arguments could not be serialized",
            )
        })?;
        let answer = call_method_json(tx, &records, method, &json)
            .await
            .map_err(JobError::from)?;
        serde_json::from_str(&answer).map_err(|_| {
            JobError::permanent(
                "step_outcome",
                format!("the step method '{model}.{method}' did not return a step outcome"),
            )
        })?
    } else {
        records
            .call::<(StepArgs,), StepOutcome>(tx, method, (args,))
            .await
            .map_err(JobError::from)?
    };
    Ok(ChunkResult {
        progress: outcome.progress,
        processed: outcome.processed,
        remaining: outcome.remaining,
        finished: outcome.finished,
    })
}

/// How long to wait before the next attempt of `claimed`, which just failed.
fn retry_delay(inner: &Inner, claimed: &ClaimedJob, attempt: u32) -> Result<Duration> {
    let backoff: Backoff = claimed
        .decl
        .backoff
        .unwrap_or_else(|| default_backoff(inner.orm.job_settings()));
    backoff.delay_random(attempt)
}

impl JobRunner {
    /// Run one claimed job to the end of its run: the handler, then the transition that takes the
    /// job out of `running`. Never panics and never returns an error: what cannot be written is
    /// logged and left to the lease (the reaper returns the job).
    #[doc(hidden)]
    pub async fn run_claimed(&self, claimed: ClaimedJob) {
        let inner = Arc::clone(&self.inner);
        let _running = RunningGuard::new(Arc::clone(&inner), claimed.decl.name);
        let settings = inner.orm.job_settings();
        if !claimed.decl.accepts_version(claimed.kind_version) {
            // an older binary in a rollout window: hand the job back, keep the attempt
            inner.counters.version_skips.fetch_add(1, Ordering::Relaxed);
            let attempts = i32::try_from(claimed.attempt.saturating_sub(1)).unwrap_or(0);
            let release = Transition::Ready {
                at: None,
                delay_secs: 30.0,
                attempts: Some(attempts),
                rank: claimed.failure_rank,
                error: None,
            };
            self.settle(&claimed, release).await;
            return;
        }
        let ctx = match JobContext::new(Arc::clone(&inner), claimed.clone()) {
            Ok(ctx) => ctx,
            Err(error) => {
                self.fail(
                    &claimed,
                    JobError::from(error),
                    &RunState {
                        completed: false,
                        rank: 0,
                        progressed: false,
                    },
                )
                .await;
                return;
            }
        };
        let mut task: JoinHandle<_> = tokio::spawn(drive(ctx));
        let finished = tokio::time::timeout(settings.job_deadline(), &mut task).await;
        let (result, state) = match finished {
            Ok(Ok(done)) => done,
            Ok(Err(join)) => {
                let panicked = join.is_panic();
                if panicked {
                    inner.counters.panics.fetch_add(1, Ordering::Relaxed);
                }
                let error = JobError::transient(
                    if panicked { "panic" } else { "task_failed" },
                    if panicked {
                        "the job handler panicked"
                    } else {
                        "the job task ended without a result"
                    },
                );
                (
                    Err(error),
                    RunState {
                        completed: false,
                        rank: 0,
                        progressed: false,
                    },
                )
            }
            Err(_) => {
                task.abort();
                let error = JobError::transient(
                    "deadline_exceeded",
                    format!(
                        "the run did not end within jobs.job_deadline_secs ({} s)",
                        settings.job_deadline().as_secs()
                    ),
                );
                (
                    Err(error),
                    RunState {
                        completed: false,
                        rank: 0,
                        progressed: false,
                    },
                )
            }
        };
        match result {
            Ok(RunOutcome::Done) => {
                // a finishing chunk already completed the job in its commit; otherwise the
                // runner completes it now
                let done = state.completed
                    || self
                        .settle(
                            &claimed,
                            Transition::Done {
                                processed: 0,
                                failed: 0,
                            },
                        )
                        .await;
                if done {
                    inner.counters.completed.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(RunOutcome::Continue) => {
                self.settle(
                    &claimed,
                    Transition::Ready {
                        at: None,
                        delay_secs: 0.0,
                        attempts: Some(0),
                        rank: state.rank,
                        error: None,
                    },
                )
                .await;
            }
            Ok(RunOutcome::Hold) => {
                self.settle(&claimed, Transition::Held { rank: state.rank })
                    .await;
            }
            Ok(RunOutcome::RescheduleAfter(after)) => {
                self.settle(
                    &claimed,
                    Transition::Ready {
                        at: None,
                        delay_secs: after.as_secs_f64(),
                        attempts: Some(0),
                        rank: state.rank,
                        error: None,
                    },
                )
                .await;
            }
            Ok(RunOutcome::Reschedule(at)) => {
                self.settle(
                    &claimed,
                    Transition::Ready {
                        at: Some(at),
                        delay_secs: 0.0,
                        attempts: Some(0),
                        rank: state.rank,
                        error: None,
                    },
                )
                .await;
            }
            Err(error) => self.fail(&claimed, error, &state).await,
        }
    }

    /// Apply a transition; a lost lease is counted, not an error.
    async fn settle(&self, claimed: &ClaimedJob, transition: Transition) -> bool {
        #[cfg(feature = "test-faults")]
        {
            let hooks = self
                .inner
                .hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if hooks.is_some_and(|h| h.fail_settle()) {
                tracing::error!(
                    target: "erp::jobs",
                    job = %claimed.job.uuid(),
                    "test fault: the settling statement was not sent"
                );
                return false;
            }
        }
        match apply(&self.inner.orm, claimed, &transition).await {
            Ok(true) => true,
            Ok(false) => {
                self.inner
                    .counters
                    .lease_lost
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target: "erp::jobs",
                    job = %claimed.job.uuid(),
                    kind = claimed.decl.name,
                    "the lease was lost before the run could be settled; the new owner has the job"
                );
                false
            }
            Err(error) => {
                tracing::error!(
                    target: "erp::jobs",
                    job = %claimed.job.uuid(),
                    kind = claimed.decl.name,
                    error = %error,
                    "a job could not be settled; its lease will expire and the reaper returns it"
                );
                false
            }
        }
    }

    /// Settle a run that ended in `error`: retry, dead-letter, yield, cancel or give up the job.
    async fn fail(&self, claimed: &ClaimedJob, error: JobError, state: &RunState) {
        let rank = state.rank;
        let attempt = if state.progressed { 1 } else { claimed.attempt };
        let inner = &self.inner;
        let job = claimed.job.uuid();
        let kind = claimed.decl.name;
        match &error {
            JobError::LeaseLost { .. } => {
                inner.counters.lease_lost.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(target: "erp::jobs", %job, kind, "the job's lease was lost; the run stopped");
            }
            JobError::Cancelled { .. } => {
                inner.counters.cancelled.fetch_add(1, Ordering::Relaxed);
                self.settle(claimed, Transition::Cancelled).await;
            }
            JobError::Yield { after } => {
                let attempts = i32::try_from(claimed.attempt.saturating_sub(1)).unwrap_or(0);
                self.settle(
                    claimed,
                    Transition::Ready {
                        at: None,
                        delay_secs: after.as_secs_f64(),
                        attempts: Some(attempts),
                        rank: claimed.failure_rank,
                        error: None,
                    },
                )
                .await;
            }
            JobError::Transient { code, message, .. } => {
                let (code, message) = ((*code).to_owned(), message.to_string());
                if attempt >= claimed.max_attempts {
                    self.dead_letter(
                        claimed,
                        code,
                        format!(
                            "{message} (attempt {attempt} of {}: no attempt is left)",
                            claimed.max_attempts
                        ),
                    )
                    .await;
                } else {
                    let delay = match retry_delay(inner, claimed, attempt) {
                        Ok(delay) => delay,
                        Err(entropy) => {
                            tracing::error!(target: "erp::jobs", %job, kind, error = %entropy, "no entropy for the backoff jitter; the lease will expire");
                            return;
                        }
                    };
                    let failure_rank = i16::try_from(attempt.min(32_767)).unwrap_or(i16::MAX);
                    inner.counters.retried.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(target: "erp::jobs", %job, kind, attempt, code = %code, "the job failed and will be retried");
                    self.settle(
                        claimed,
                        Transition::Ready {
                            at: None,
                            delay_secs: delay.as_secs_f64(),
                            attempts: Some(i32::try_from(attempt).unwrap_or(i32::MAX)),
                            rank: failure_rank.max(rank),
                            error: Some((code, message)),
                        },
                    )
                    .await;
                }
            }
            other => {
                // Permanent, Item, Payload, PrincipalRevoked, ItemFailuresExceeded: retrying
                // cannot change the answer
                let (code, message) = other.stored().unwrap_or(("failed", other.to_string()));
                self.dead_letter(claimed, code.to_owned(), message).await;
            }
        }
    }

    async fn dead_letter(&self, claimed: &ClaimedJob, code: String, message: String) {
        self.inner.counters.dead.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            target: "erp::jobs",
            job = %claimed.job.uuid(),
            kind = claimed.decl.name,
            code = %code,
            "the job is a dead letter"
        );
        self.settle(claimed, Transition::Dead { code, message })
            .await;
    }

    /// Jobs this runner may start now: free slots of the database, the process and the share the
    /// live workers leave, bounded by `jobs.claim_batch`.
    fn free_slots(&self) -> u32 {
        let db_free = self.inner.db_slots.available_permits();
        let process_free = self.inner.process_slots.available_permits();
        let in_use = (self.settings().workers_per_db() as usize).saturating_sub(db_free);
        let share_free = (self.slot_limit() as usize).saturating_sub(in_use);
        let free = db_free.min(process_free).min(share_free);
        u32::try_from(free).unwrap_or(u32::MAX)
    }

    /// Whether this node may claim: ready when it has a registry view.
    fn node_may_claim(&self) -> bool {
        match &self.inner.node {
            Some(nodes) => nodes.ready(),
            None => true,
        }
    }

    /// Reap when `jobs.reap_interval_secs` have passed since this runner last did.
    async fn reap_if_due(&self) -> Result<u32> {
        let due = {
            let mut last = self
                .inner
                .last_reap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let due = last.is_none_or(|at| at.elapsed() >= self.settings().reap_interval());
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        if !due {
            return Ok(0);
        }
        let report = self.reap(1_000).await?;
        Ok(report.requeued + report.dead)
    }

    /// One pass: reap if due, claim what the free slots allow and run every claimed job to the
    /// end of its run. Deterministic: tests and one-shot commands drive the queue with it.
    ///
    /// # Errors
    ///
    /// The database's errors from the reaper or the claim (the jobs themselves report through
    /// their own transitions).
    pub async fn tick(&self) -> Result<TickReport> {
        if !self.node_may_claim() {
            return Ok(TickReport {
                held_back: true,
                ..TickReport::default()
            });
        }
        let reaped = self.reap_if_due().await?;
        let claimed = self.claim(self.free_slots()).await?;
        let count = u32::try_from(claimed.len()).unwrap_or(u32::MAX);
        let mut runs = JoinSet::new();
        for job in claimed {
            let runner = self.clone();
            let permits = self.permits()?;
            runs.spawn(async move {
                runner.run_claimed(job).await;
                drop(permits);
            });
        }
        while runs.join_next().await.is_some() {}
        Ok(TickReport {
            claimed: count,
            reaped,
            held_back: false,
        })
    }

    /// Take one database slot and one process slot for a run.
    fn permits(&self) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let db = Arc::clone(&self.inner.db_slots)
            .try_acquire_owned()
            .map_err(|_| Error::internal("a job was claimed with no database slot free"))?;
        let process = Arc::clone(&self.inner.process_slots)
            .try_acquire_owned()
            .map_err(|_| Error::internal("a job was claimed with no process slot free"))?;
        Ok((db, process))
    }

    /// Ask the runner loop to look for work now (an enqueue in this process, a notification).
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Start the runner loop on the current Tokio runtime: claim while there is work and a free
    /// slot, otherwise sleep until the earliest of the next due job, the next wake and the poll
    /// interval (`jobs.poll_interval_secs`, jittered).
    pub fn spawn(self) -> JobRunnerHandle {
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(self.run_loop(stop_rx));
        JobRunnerHandle {
            stop: stop_tx,
            task,
        }
    }

    async fn run_loop(self, mut stop: watch::Receiver<bool>) {
        let mut running: JoinSet<()> = JoinSet::new();
        if let Err(error) = super::retention::ensure_core_schedules(&self.inner.orm).await {
            tracing::error!(target: "erp::jobs", error = %error, "the core job schedules could not be ensured; retention will not run until the next start");
        }
        // one more claim a notify interval after a wake-up covers the hints that were suppressed
        // inside that interval
        let mut trailing: Option<Instant> = None;
        let mut woken = false;
        loop {
            if *stop.borrow() {
                break;
            }
            while running.try_join_next().is_some() {}
            let mut wait = self.settings().poll_interval();
            if woken {
                trailing = Some(Instant::now() + self.settings().notify_interval());
                woken = false;
            }
            if self.node_may_claim() {
                if let Err(error) = self.reap_if_due().await {
                    tracing::warn!(target: "erp::jobs", error = %error, "the job reaper failed; it runs again at its next turn");
                }
                let free = self.free_slots();
                if free > 0 {
                    match self.claim(free).await {
                        Ok(claimed) => {
                            let full = u32::try_from(claimed.len()).unwrap_or(u32::MAX)
                                >= free.min(self.settings().claim_batch());
                            for job in claimed {
                                match self.permits() {
                                    Ok(permits) => {
                                        let runner = self.clone();
                                        running.spawn(async move {
                                            runner.run_claimed(job).await;
                                            drop(permits);
                                        });
                                    }
                                    Err(error) => {
                                        tracing::error!(target: "erp::jobs", error = %error, "no slot for a claimed job; its lease will expire");
                                    }
                                }
                            }
                            if full {
                                // the batch was full: more may be due
                                wait = Duration::ZERO;
                            } else if let Ok(Some(next)) = self.next_due().await {
                                wait = wait.min(next);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(target: "erp::jobs", error = %error, "a job claim failed; trying again after the poll interval");
                        }
                    }
                }
            }
            if let Some(at) = trailing {
                let left = at.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    trailing = None;
                } else {
                    wait = wait.min(left);
                }
            }
            if wait.is_zero() {
                tokio::task::yield_now().await;
                continue;
            }
            let wait = jittered(wait);
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = self.inner.wake.notified() => woken = true,
                () = listener_wake(self.inner.listener_wake.as_deref()) => woken = true,
                changed = stop.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                Some(_) = running.join_next(), if !running.is_empty() => {}
            }
        }
        while running.join_next().await.is_some() {}
    }
}

/// Waits for a notification of the signal listener; never completes without one.
async fn listener_wake(notify: Option<&tokio::sync::Notify>) {
    match notify {
        Some(notify) => notify.notified().await,
        None => std::future::pending().await,
    }
}

/// `wait` with a uniform jitter of ±10 %, so processes that started together do not poll together.
fn jittered(wait: Duration) -> Duration {
    let spread = crate::transaction::jitter(200);
    // 0..=200 maps to 0.9..=1.1
    wait.mul_f64(0.9 + (spread as f64) / 1000.0)
}

/// A running [`JobRunner::spawn`] loop.
#[derive(Debug)]
pub struct JobRunnerHandle {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl JobRunnerHandle {
    /// Stop claiming and wait for the runs in progress to end.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }

    /// Whether the loop has ended (it only ends after [`shutdown`](Self::shutdown) or a panic).
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}
