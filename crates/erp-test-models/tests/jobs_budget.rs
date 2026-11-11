//! stage D: the run budget (2).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use erp_orm::BoxFuture;
use erp_orm::jobs::{
    ChunkResult, EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl,
    JobRunner, JobSettings, RunOutcome, Setting,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{PassResolver, cell, enqueue, script};

/// How a run of the `budget` kind went: chunks run, time taken, whether the budget said stop.
static RUNS: Mutex<Vec<(u32, Duration, bool)>> = Mutex::new(Vec::new());

/// Runs one-item chunks (each taking `chunk_ms`) until the budget says stop, then continues.
struct Slow;

impl JobHandler for Slow {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let started = Instant::now();
            let chunk_ms = ctx.payload::<serde_json::Value>()?["chunk_ms"]
                .as_u64()
                .unwrap_or(0);
            while !ctx.budget_exhausted() {
                ctx.chunk(move |_tx| {
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(chunk_ms)).await;
                        Ok(ChunkResult::more(
                            serde_json::json!({"alive": true}),
                            1,
                            Some(100),
                        ))
                    })
                })
                .await?;
            }
            RUNS.lock().unwrap().push((
                ctx.chunks_run(),
                started.elapsed(),
                ctx.budget_exhausted(),
            ));
            Ok(RunOutcome::Continue)
        })
    }
}

static SLOW: Slow = Slow;

erp_orm::register_job_kind!(
    BUDGET_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.budget", HandlerRef::Native(&SLOW))
);
erp_orm::register_job_kind!(
    SCRIPT_KIND,
    JobKindDecl::new(
        "test_jobs",
        "test_jobs.script",
        HandlerRef::Native(&support::jobs::SCRIPT)
    )
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

fn runner_with(h: &Harness, chunks: u64, secs: u64) -> JobRunner {
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(
            JobSettings::builder()
                .set(Setting::MaxChunksPerRun, chunks)
                .set(Setting::RunBudgetSecs, secs)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    JobRunner::builder(orm, PassResolver).build().unwrap()
}

fn budget(chunk_ms: u64) -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.budget")
        .payload(&serde_json::json!({ "chunk_ms": chunk_ms }))
        .unwrap()
}

#[tokio::test]
async fn fast_chunks_run_on_until_the_time_budget_is_used() {
    let h = harness().await;
    RUNS.lock().unwrap().clear();
    let jobs = enqueue(&h, vec![budget(0)]).await;
    let runner = runner_with(&h, 2, 1);
    runner.tick().await.unwrap();
    let runs = RUNS.lock().unwrap().clone();
    assert_eq!(runs.len(), 1);
    // two chunks would have met the chunk limit at once; the run goes on until a second passed
    assert!(runs[0].0 > 2, "{runs:?}");
    assert!(runs[0].1 >= Duration::from_secs(1), "{runs:?}");
    assert!(runs[0].2);
    // Continue: ready again at once, attempts reset, the chunks' counters and progress kept
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', state, attempts, progress, items_done >= 3, \
                 run_after <= clock_timestamp()) FROM erp_job WHERE id = {}",
                jobs[0].id()
            )
        )
        .await,
        "ready|0|{\"alive\": true}|t|t"
    );
}

#[tokio::test]
async fn slow_chunks_run_on_until_the_chunk_limit_is_reached() {
    let h = harness().await;
    RUNS.lock().unwrap().clear();
    enqueue(&h, vec![budget(400)]).await;
    let runner = runner_with(&h, 5, 1);
    runner.tick().await.unwrap();
    let runs = RUNS.lock().unwrap().clone();
    assert_eq!(runs.len(), 1);
    // the second of budget was gone after three chunks; the run still made five
    assert_eq!(runs[0].0, 5, "{runs:?}");
    assert!(runs[0].1 >= Duration::from_millis(2_000), "{runs:?}");
}

#[tokio::test]
async fn a_continued_job_goes_behind_the_jobs_already_due() {
    let h = harness().await;
    RUNS.lock().unwrap().clear();
    let jobs = enqueue(&h, vec![budget(0), script("native_done")]).await;
    let runner = runner_with(&h, 1, 1);
    // the budget job (lower id) runs first and continues; its new due time is later than the
    // other job's, so the other job is claimed before it
    let first = runner.claim(1).await.unwrap();
    assert_eq!(first[0].job(), jobs[0]);
    runner.run_claimed(first.into_iter().next().unwrap()).await;
    let next = runner.claim(1).await.unwrap();
    assert_eq!(next[0].job(), jobs[1]);
    runner.run_claimed(next.into_iter().next().unwrap()).await;
    let again = runner.claim(1).await.unwrap();
    assert_eq!(again[0].job(), jobs[0]);
}
