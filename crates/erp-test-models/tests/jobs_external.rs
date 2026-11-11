//! stage D: external steps (2).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use erp_orm::BoxFuture;
use erp_orm::jobs::{
    EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl, JobRunner,
    JobSettings, RunOutcome, RunnerHooks, Setting,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{PassResolver, cell, enqueue, exec};

/// Items the external step went through, in order, across runs.
static ITEMS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static CRASHED: AtomicBool = AtomicBool::new(false);

/// An external step over `items` items of `ms` milliseconds each, checkpointing every two items.
/// With `crash_at` the first run fails (transiently) before that item.
struct Mailer;

impl JobHandler for Mailer {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let payload: serde_json::Value = ctx.payload()?;
            let items = payload["items"].as_u64().unwrap() as u32;
            let ms = payload["ms"].as_u64().unwrap_or(0);
            let crash_at = payload["crash_at"].as_u64().map(|n| n as u32);
            let start = ctx
                .progress::<serde_json::Value>()?
                .and_then(|p| p["next"].as_u64())
                .unwrap_or(0) as u32;
            let mut next = start;
            while next < items {
                if crash_at == Some(next) && !CRASHED.swap(true, Ordering::SeqCst) {
                    return Err(JobError::transient("crash", "the process went away"));
                }
                let token = ctx.cancel_token();
                let item = next;
                ctx.external(async move |token| {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    if token.is_cancelled() {
                        return Err(JobError::transient("stopped", "the token was set"));
                    }
                    Ok(())
                })
                .await
                .inspect_err(|_| {
                    ITEMS.lock().unwrap().push(1_000 + item);
                    drop(token.clone());
                })?;
                ITEMS.lock().unwrap().push(item);
                next += 1;
                if next.is_multiple_of(2) {
                    ctx.checkpoint(
                        &serde_json::json!({ "next": next }),
                        2,
                        Some(u64::from(items - next)),
                    )
                    .await?;
                }
            }
            Ok(RunOutcome::Done)
        })
    }
}

static MAILER: Mailer = Mailer;

erp_orm::register_job_kind!(
    MAILER_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.mailer", HandlerRef::Native(&MAILER)).max_attempts(3)
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

fn runner_with_lease(h: &Harness, lease: Duration) -> JobRunner {
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(
            JobSettings::builder()
                .lease_for_tests(lease)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    JobRunner::builder(orm, PassResolver).build().unwrap()
}

fn mailer(items: u32, ms: u64, crash_at: Option<u32>) -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.mailer")
        .payload(&serde_json::json!({ "items": items, "ms": ms, "crash_at": crash_at }))
        .unwrap()
}

#[tokio::test]
async fn the_lease_renews_while_an_external_step_runs() {
    let h = harness().await;
    ITEMS.lock().unwrap().clear();
    enqueue(&h, vec![mailer(1, 2_000, None)]).await;
    let runner = runner_with_lease(&h, Duration::from_millis(600));
    let reaper = runner_with_lease(&h, Duration::from_millis(600));
    let run = runner.tick();
    let reap = async {
        // the step outlives its first lease three times over; the reaper looks every 150 ms
        let mut taken = 0;
        for _ in 0..16 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let report = reaper.reap(10).await.unwrap();
            taken += report.requeued + report.dead;
        }
        taken
    };
    let (ticked, taken) = tokio::join!(run, reap);
    assert_eq!(ticked.unwrap().claimed, 1);
    assert_eq!(taken, 0, "a renewed lease is never reaped");
    assert_eq!(ITEMS.lock().unwrap().as_slice(), [0]);
    assert_eq!(cell(&h, "SELECT outcome FROM erp_job_run").await, "done");
}

#[tokio::test]
async fn a_lost_lease_cancels_the_token_and_the_run_stops_before_the_next_item() {
    let h = harness().await;
    ITEMS.lock().unwrap().clear();
    let jobs = enqueue(&h, vec![mailer(6, 400, None)]).await;
    let runner = runner_with_lease(&h, Duration::from_millis(600));
    let takeover = async {
        // after the first item another worker takes the job over: the epoch moves on
        tokio::time::sleep(Duration::from_millis(550)).await;
        exec(&h, "UPDATE erp_job SET lease_epoch = lease_epoch + 1").await;
    };
    let (ticked, ()) = tokio::join!(runner.tick(), takeover);
    ticked.unwrap();
    let seen = ITEMS.lock().unwrap().clone();
    // the step noticed the loss (1_000 + item marks the refused call) well before item 5
    assert!(seen.iter().any(|i| *i >= 1_000), "{seen:?}");
    assert!(!seen.contains(&5), "{seen:?}");
    // nothing was written by the deposed run: the new owner's row is still running
    assert_eq!(support::jobs::state(&h, jobs[0]).await, "running");
    assert_eq!(runner.metrics().lease_lost, 1);
}

/// items after it)
#[tokio::test]
async fn a_crash_after_a_checkpoint_resumes_from_it_and_repeats_only_the_items_after_it() {
    let h = harness().await;
    ITEMS.lock().unwrap().clear();
    CRASHED.store(false, Ordering::SeqCst);
    enqueue(&h, vec![mailer(8, 0, Some(5))]).await;
    let runner = runner_with_lease(&h, Duration::from_secs(30));
    runner.tick().await.unwrap();
    // the first run did items 0..=4 and checkpointed after 0..=3, then the process "crashed"
    assert_eq!(ITEMS.lock().unwrap().as_slice(), [0, 1, 2, 3, 4]);
    exec(&h, "UPDATE erp_job SET run_after = clock_timestamp()").await;
    runner.tick().await.unwrap();
    // the second run resumed at the checkpoint: only item 4, which came after it, was repeated
    assert_eq!(
        ITEMS.lock().unwrap().as_slice(),
        [0, 1, 2, 3, 4, 4, 5, 6, 7]
    );
    assert_eq!(cell(&h, "SELECT outcome FROM erp_job_run").await, "done");
}

#[tokio::test]
async fn a_checkpoint_of_a_deposed_run_is_refused_and_writes_nothing() {
    let h = harness().await;
    ITEMS.lock().unwrap().clear();
    let jobs = enqueue(&h, vec![mailer(2, 0, None)]).await;
    let runner = runner_with_lease(&h, Duration::from_secs(30));
    let claimed = runner.claim(1).await.unwrap();
    // another worker owns the job now: the checkpoint of the deposed run is refused
    exec(&h, "UPDATE erp_job SET lease_epoch = lease_epoch + 1").await;
    runner
        .run_claimed(claimed.into_iter().next().unwrap())
        .await;
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', state, items_done, progress IS NULL) FROM erp_job WHERE id = {}",
                jobs[0].id()
            )
        )
        .await,
        "running|0|t"
    );
}

/// A step that panics or never returns while it holds an external step open.
struct Leaky;

impl JobHandler for Leaky {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let payload: serde_json::Value = ctx.payload()?;
            let panics = payload["do"] == "panic";
            ctx.external(async move |_token| -> Result<(), JobError> {
                // longer than a lease, so the renewer has run at least once
                tokio::time::sleep(Duration::from_millis(900)).await;
                if panics {
                    panic!("the external step panicked");
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(())
            })
            .await?;
            Ok(RunOutcome::Done)
        })
    }
}

static LEAKY: Leaky = Leaky;

erp_orm::register_job_kind!(
    LEAKY_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.leaky", HandlerRef::Native(&LEAKY)).max_attempts(3)
);

/// The settling statement of every run is not sent: whatever the run leaves behind stays.
struct SettleFails;

impl RunnerHooks for SettleFails {
    fn fail_settle(&self) -> bool {
        true
    }
}

async fn assert_the_lease_stops_and_the_reaper_requeues(h: &Harness, runner: &JobRunner) {
    // the run ended and nothing settled it: the row is still `running` on its last lease
    assert_eq!(cell(h, "SELECT state FROM erp_job").await, "running");
    let last = cell(h, "SELECT lease_until::text FROM erp_job").await;
    tokio::time::sleep(Duration::from_millis(1_800)).await;
    assert_eq!(
        cell(h, "SELECT lease_until::text FROM erp_job").await,
        last,
        "a run that is over keeps renewing its lease"
    );
    let report = runner.reap(10).await.unwrap();
    assert_eq!(report.requeued, 1, "the expired lease is requeued");
    assert_eq!(cell(h, "SELECT state FROM erp_job").await, "ready");
}

#[tokio::test]
async fn a_panicking_external_step_stops_renewing_the_lease_when_the_settle_fails() {
    let h = harness().await;
    enqueue(
        &h,
        vec![
            EnqueueRequest::new("test_jobs.leaky")
                .payload(&serde_json::json!({ "do": "panic" }))
                .unwrap(),
        ],
    )
    .await;
    let runner = runner_with_lease(&h, Duration::from_millis(600));
    runner.set_hooks(std::sync::Arc::new(SettleFails));
    runner.tick().await.unwrap();
    let reaper = runner_with_lease(&h, Duration::from_millis(600));
    assert_the_lease_stops_and_the_reaper_requeues(&h, &reaper).await;
}

#[tokio::test]
async fn a_run_aborted_at_the_job_deadline_stops_renewing_the_lease_when_the_settle_fails() {
    let h = harness().await;
    enqueue(
        &h,
        vec![
            EnqueueRequest::new("test_jobs.leaky")
                .payload(&serde_json::json!({ "do": "hang" }))
                .unwrap(),
        ],
    )
    .await;
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(
            JobSettings::builder()
                .lease_for_tests(Duration::from_millis(600))
                .set(Setting::JobDeadlineSecs, 10u64)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let runner = JobRunner::builder(orm, PassResolver).build().unwrap();
    runner.set_hooks(std::sync::Arc::new(SettleFails));
    runner.tick().await.unwrap();
    let reaper = runner_with_lease(&h, Duration::from_millis(600));
    assert_the_lease_stops_and_the_reaper_requeues(&h, &reaper).await;
}
