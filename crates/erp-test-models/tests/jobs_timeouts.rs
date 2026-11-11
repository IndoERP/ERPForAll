//! stage D: chunk limits and lease sizing.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::BoxFuture;
use erp_orm::jobs::{
    ChunkResult, EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl,
    JobRunner, JobSettings, RunOutcome, Setting, TimeoutClass,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{PassResolver, cell, enqueue};

/// One chunk whose statements sleep for the payload's `sleeps` (seconds, one statement each).
struct Sleepy;

impl JobHandler for Sleepy {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let sleeps: Vec<f64> =
                serde_json::from_value(ctx.payload::<serde_json::Value>()?["sleeps"].clone())
                    .unwrap();
            ctx.chunk(move |tx| {
                let sleeps = sleeps.clone();
                Box::pin(async move {
                    for seconds in sleeps {
                        tx.query("SELECT pg_sleep($1)", &[&seconds]).await?;
                    }
                    Ok(ChunkResult::done(serde_json::json!({"slept": true}), 1))
                })
            })
            .await?;
            Ok(RunOutcome::Done)
        })
    }
}

static SLEEPY: Sleepy = Sleepy;

erp_orm::register_job_kind!(
    SLEEPY_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.sleepy", HandlerRef::Native(&SLEEPY)).max_attempts(3)
);
erp_orm::register_job_kind!(
    SLEEPY_LONG_KIND,
    JobKindDecl::new(
        "test_jobs",
        "test_jobs.sleepy_long",
        HandlerRef::Native(&SLEEPY)
    )
    .max_attempts(3)
    .timeouts(TimeoutClass::Maintenance)
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

fn runner_with(h: &Harness, statement_ms: u64, transaction_ms: u64) -> JobRunner {
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(
            JobSettings::builder()
                .set(Setting::ChunkDefaultLockMs, 100)
                .set(Setting::ChunkDefaultStatementMs, statement_ms)
                .set(Setting::ChunkDefaultTransactionMs, transaction_ms)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    JobRunner::builder(orm, PassResolver).build().unwrap()
}

fn sleepy(kind: &'static str, sleeps: &[f64]) -> EnqueueRequest {
    EnqueueRequest::new(kind)
        .payload(&serde_json::json!({ "sleeps": sleeps }))
        .unwrap()
}

async fn last_error(h: &Harness) -> String {
    cell(
        h,
        "SELECT concat_ws('|', state, last_error_code) FROM erp_job",
    )
    .await
}

/// transient)
#[tokio::test]
async fn a_statement_past_the_chunk_statement_limit_fails_the_run_as_transient() {
    let h = harness().await;
    enqueue(&h, vec![sleepy("test_jobs.sleepy", &[3.0])]).await;
    let runner = runner_with(&h, 1_000, 5_000);
    runner.tick().await.unwrap();
    assert_eq!(last_error(&h).await, "ready|statement_timeout");
    // the connection is usable again: the same runner claims and runs the next job
    enqueue(&h, vec![sleepy("test_jobs.sleepy", &[])]).await;
    assert_eq!(runner.tick().await.unwrap().claimed, 1);
}

/// within the statement limit, and the pool recycles the connection)
#[tokio::test]
async fn a_chunk_past_the_transaction_backstop_is_terminated_and_the_pool_recovers() {
    let h = harness().await;
    enqueue(&h, vec![sleepy("test_jobs.sleepy", &[0.9, 0.9, 0.9])]).await;
    let runner = runner_with(&h, 1_000, 2_000);
    runner.tick().await.unwrap();
    assert_eq!(last_error(&h).await, "ready|transaction_timeout");
    // the backstop terminated the session (25P04); the next chunks run on fresh connections
    for _ in 0..3 {
        enqueue(&h, vec![sleepy("test_jobs.sleepy", &[])]).await;
        assert_eq!(runner.tick().await.unwrap().claimed, 1);
    }
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM erp_job_run").await,
        "3"
    );
}

#[tokio::test]
async fn a_maintenance_kind_runs_under_the_maintenance_limits() {
    let h = harness().await;
    enqueue(&h, vec![sleepy("test_jobs.sleepy_long", &[1.5])]).await;
    // the default class would stop at 1 s; the maintenance class defaults to 300 s
    let runner = runner_with(&h, 1_000, 5_000);
    runner.tick().await.unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(cell(&h, "SELECT outcome FROM erp_job_run").await, "done");
}
