//! stage D: chunks commit their effects with the job's progress (1; design
//! §5.4, §5.6).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Mutex;

use erp_orm::BoxFuture;
use erp_orm::jobs::{
    ChunkReport, ChunkResult, EnqueueRequest, HandlerRef, HistoryPolicy, JobContext, JobError,
    JobHandler, JobKindDecl, RunOutcome,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{cell, enqueue, exec, runner};

static REPORTS: Mutex<Vec<ChunkReport>> = Mutex::new(Vec::new());

/// Chunk one writes an effect row and reports progress `{"n":1}`; the payload's `mode` picks what
/// the second step does.
struct Staged;

impl JobHandler for Staged {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let mode: String = ctx
                .payload::<serde_json::Value>()?
                .get("mode")
                .and_then(|m| m.as_str())
                .unwrap_or("two_chunks")
                .to_owned();
            let first = ctx
                .chunk(|tx| {
                    Box::pin(async move {
                        for note in ["a", "b", "c"] {
                            tx.execute("INSERT INTO jobs_effect (note) VALUES ($1)", &[&note])
                                .await?;
                        }
                        Ok(ChunkResult::more(serde_json::json!({"n": 1}), 3, Some(5)))
                    })
                })
                .await?;
            REPORTS.lock().unwrap().push(first);
            match mode.as_str() {
                "one_chunk" => Ok(RunOutcome::Continue),
                "fail_second" => {
                    ctx.chunk(|tx| {
                        Box::pin(async move {
                            tx.execute("INSERT INTO jobs_effect (note) VALUES ('doomed')", &[])
                                .await?;
                            Err::<ChunkResult<serde_json::Value>, _>(JobError::permanent(
                                "boom",
                                "the second chunk failed",
                            ))
                        })
                    })
                    .await?;
                    Ok(RunOutcome::Done)
                }
                _ => {
                    let progress: Option<serde_json::Value> = ctx.progress()?;
                    assert_eq!(progress, Some(serde_json::json!({"n": 1})));
                    let last = ctx
                        .chunk(|tx| {
                            Box::pin(async move {
                                tx.execute("INSERT INTO jobs_effect (note) VALUES ('d')", &[])
                                    .await?;
                                Ok(ChunkResult::done(serde_json::json!({"n": 2}), 2))
                            })
                        })
                        .await?;
                    REPORTS.lock().unwrap().push(last);
                    Ok(RunOutcome::Done)
                }
            }
        })
    }
}

static STAGED: Staged = Staged;

erp_orm::register_job_kind!(
    STAGED_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.staged", HandlerRef::Native(&STAGED))
);
erp_orm::register_job_kind!(
    STAGED_NO_HISTORY,
    JobKindDecl::new(
        "test_jobs",
        "test_jobs.staged_quiet",
        HandlerRef::Native(&STAGED)
    )
    .history(HistoryPolicy::None)
);

async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    exec(
        &h,
        "CREATE TABLE jobs_effect (id bigserial PRIMARY KEY, note text NOT NULL)",
    )
    .await;
    h
}

fn staged(mode: &str) -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.staged")
        .payload(&serde_json::json!({ "mode": mode }))
        .unwrap()
}

#[tokio::test]
async fn a_chunk_commits_its_effects_and_the_jobs_progress_together() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![staged("one_chunk")]).await;
    let runner = runner(&h);
    assert_eq!(runner.tick().await.unwrap().claimed, 1);
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM jobs_effect").await,
        "3"
    );
    let row = cell(
        &h,
        &format!(
            "SELECT concat_ws('|', state, progress, items_done, items_remaining, attempts, \
             lease_owner IS NULL) FROM erp_job WHERE id = {}",
            jobs[0].id()
        ),
    )
    .await;
    // Continue: back to ready at once, attempts reset, progress and counters of the chunk kept
    assert_eq!(row, "ready|{\"n\": 1}|3|5|0|t");
}

#[tokio::test]
async fn a_failing_chunk_rolls_back_its_effects_and_leaves_the_progress_of_the_chunk_before() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![staged("fail_second")]).await;
    runner(&h).tick().await.unwrap();
    // the first chunk's three rows are committed, the doomed row of the second is not
    assert_eq!(
        cell(
            &h,
            "SELECT string_agg(note, ',' ORDER BY id) FROM jobs_effect"
        )
        .await,
        "a,b,c"
    );
    let row = cell(
        &h,
        &format!(
            "SELECT concat_ws('|', state, progress, items_done, last_error_code, last_error) \
             FROM erp_job WHERE id = {}",
            jobs[0].id()
        ),
    )
    .await;
    assert_eq!(row, "dead|{\"n\": 1}|3|boom|the second chunk failed");
}

/// checkpoint or completion)
#[tokio::test]
async fn a_chunk_costs_two_job_statements_besides_its_own_and_the_last_chunk_completes_the_job() {
    let h = harness().await;
    REPORTS.lock().unwrap().clear();
    enqueue(&h, vec![staged("two_chunks")]).await;
    let runner = runner(&h);
    runner.tick().await.unwrap();
    let reports = REPORTS.lock().unwrap().clone();
    assert_eq!(reports.len(), 2);
    // the first chunk: fence + 3 inserts + checkpoint; the last: fence + 1 insert + completion
    assert_eq!(reports[0].statements, 5);
    assert_eq!(reports[0].processed, 3);
    assert!(!reports[0].finished);
    assert_eq!(reports[1].statements, 3);
    assert!(reports[1].finished);
    // the completion was in the last chunk's commit: the job is gone, one history row, no
    // other transition was needed
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(
        cell(
            &h,
            "SELECT concat_ws('|', outcome, items_done, attempts, kind) FROM erp_job_run"
        )
        .await,
        "done|5|1|test_jobs.staged"
    );
    assert_eq!(runner.metrics().completed, 1);
}

#[tokio::test]
async fn a_kind_without_history_leaves_no_history_row() {
    let h = harness().await;
    enqueue(
        &h,
        vec![
            EnqueueRequest::new("test_jobs.staged_quiet")
                .payload(&serde_json::json!({"mode": "two_chunks"}))
                .unwrap(),
        ],
    )
    .await;
    runner(&h).tick().await.unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM erp_job_run").await,
        "0"
    );
}
