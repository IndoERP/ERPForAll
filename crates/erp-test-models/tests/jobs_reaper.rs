//! stage C: the reaper returns expired leases.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::BoxFuture;
use erp_orm::jobs::{
    EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl, JobRef, ReapReport,
    RunOutcome,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{cell, enqueue, exec, runner};

struct Nop;

impl JobHandler for Nop {
    fn run<'a>(
        &'a self,
        _ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async { Ok(RunOutcome::Done) })
    }
}

static NOP: Nop = Nop;

erp_orm::register_job_kind!(
    PLAIN,
    JobKindDecl::new("test_jobs", "test_jobs.plain", HandlerRef::Native(&NOP)).max_attempts(3)
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

/// Enqueue and claim `n` jobs, then let every lease run out.
async fn expired(h: &Harness, n: usize) -> Vec<JobRef> {
    let jobs = enqueue(
        h,
        (0..n)
            .map(|_| EnqueueRequest::new("test_jobs.plain"))
            .collect(),
    )
    .await;
    let claimed = runner(h).claim(8).await.unwrap();
    assert_eq!(claimed.len(), n);
    exec(
        h,
        "UPDATE erp_job SET lease_until = clock_timestamp() - interval '1 second' WHERE state = 'running'",
    )
    .await;
    jobs
}

#[tokio::test]
async fn an_expired_lease_returns_the_job_to_ready_with_a_backoff_and_a_reason() {
    let h = harness().await;
    let jobs = expired(&h, 1).await;
    let report = runner(&h).reap(100).await.unwrap();
    assert_eq!((report.requeued, report.dead), (1, 0));
    let row = cell(
        &h,
        &format!(
            "SELECT concat_ws('|', state, lease_owner IS NULL, lease_until IS NULL, attempts, failure_rank, \
             last_error_code, last_error, dead_at IS NULL, \
             round(extract(epoch FROM run_after - clock_timestamp()))::int BETWEEN 4 AND 10) \
             FROM erp_job WHERE id = {}",
            jobs[0].id()
        ),
    )
    .await;
    assert_eq!(
        row,
        "ready|t|t|1|1|lease_expired|the lease expired during attempt 1 of 3|t|t"
    );
    // not due yet: the backoff keeps it from being claimed at once
    assert!(runner(&h).claim(8).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_job_out_of_attempts_becomes_a_dead_letter_with_the_reason() {
    let h = harness().await;
    let jobs = expired(&h, 1).await;
    exec(&h, "UPDATE erp_job SET attempts = max_attempts").await;
    let report = runner(&h).reap(100).await.unwrap();
    assert_eq!((report.requeued, report.dead), (0, 1));
    let row = cell(
        &h,
        &format!(
            "SELECT concat_ws('|', state, lease_owner IS NULL, dead_at IS NOT NULL, last_error_code, last_error) \
             FROM erp_job WHERE id = {}",
            jobs[0].id()
        ),
    )
    .await;
    assert_eq!(
        row,
        "dead|t|t|lease_expired|the lease expired during attempt 3 of 3; no attempt is left"
    );
    assert!(runner(&h).claim(8).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_row_held_by_a_running_chunk_is_skipped_not_waited_for() {
    let h = harness().await;
    let jobs = expired(&h, 2).await;
    let mut chunk = h.begin().await.unwrap();
    chunk
        .execute(
            "SELECT id FROM erp_job WHERE id = $1 FOR UPDATE",
            &[&jobs[0].id()],
        )
        .await
        .unwrap();
    let report = tokio::time::timeout(std::time::Duration::from_secs(10), runner(&h).reap(100))
        .await
        .expect("the reaper never waits for a held row")
        .unwrap();
    assert_eq!(report.requeued + report.dead, 1);
    assert_eq!(support::jobs::state(&h, jobs[0]).await, "running");
    assert_eq!(support::jobs::state(&h, jobs[1]).await, "ready");
    chunk.rollback().await.unwrap();
    let report = runner(&h).reap(100).await.unwrap();
    assert_eq!(report.requeued + report.dead, 1);
    assert_eq!(support::jobs::state(&h, jobs[0]).await, "ready");
}

#[tokio::test]
async fn a_live_lease_is_left_alone_and_the_batch_is_bounded() {
    let h = harness().await;
    let jobs = expired(&h, 5).await;
    // one of them renewed its lease just now
    exec(
        &h,
        &format!(
            "UPDATE erp_job SET lease_until = clock_timestamp() + interval '1 minute' WHERE id = {}",
            jobs[0].id()
        ),
    )
    .await;
    let first = runner(&h).reap(2).await.unwrap();
    assert_eq!(first.requeued + first.dead, 2);
    let rest = runner(&h).reap(100).await.unwrap();
    assert_eq!(rest.requeued + rest.dead, 2);
    assert_eq!(support::jobs::state(&h, jobs[0]).await, "running");
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE state = 'ready'"
        )
        .await,
        "4"
    );
    assert_eq!(runner(&h).reap(100).await.unwrap(), ReapReport::default());
}

#[tokio::test]
async fn the_runner_counts_what_it_reaped() {
    let h = harness().await;
    expired(&h, 2).await;
    exec(
        &h,
        "UPDATE erp_job SET attempts = max_attempts WHERE id = (SELECT min(id) FROM erp_job)",
    )
    .await;
    let runner = runner(&h);
    runner.reap(100).await.unwrap();
    let metrics = runner.metrics();
    assert_eq!(metrics.reaped, 2);
    assert_eq!(metrics.dead, 1);
}
