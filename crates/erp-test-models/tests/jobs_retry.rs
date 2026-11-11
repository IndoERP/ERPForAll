//! stage D: how a run ends (D5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::jobs::{HandlerRef, JobKindDecl, JobRef};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{SCRIPT, cell, enqueue, exec, runner, script};

erp_orm::register_job_kind!(
    SCRIPT_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.script", HandlerRef::Native(&SCRIPT)).max_attempts(3)
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

/// One text row of the job with the columns the retry tests look at.
async fn row(h: &Harness, job: JobRef) -> String {
    cell(
        h,
        &format!(
            "SELECT concat_ws('|', state, attempts, failure_rank, last_error_code, last_error, \
             dead_at IS NOT NULL, lease_owner IS NULL, \
             round(extract(epoch FROM run_after - clock_timestamp()))::int) \
             FROM erp_job WHERE id = {}",
            job.id()
        ),
    )
    .await
}

/// Make a ready job claimable again at once.
async fn due_now(h: &Harness) {
    exec(
        h,
        "UPDATE erp_job SET run_after = clock_timestamp() WHERE state = 'ready'",
    )
    .await;
}

#[tokio::test]
async fn a_transient_failure_goes_back_to_ready_with_a_backoff_until_attempts_run_out() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![script("transient")]).await;
    let runner = runner(&h);
    runner.tick().await.unwrap();
    let first = row(&h, jobs[0]).await;
    let parts: Vec<&str> = first.split('|').collect();
    // ready, attempts 1, rank 1, the code and the message, not dead, lease released
    assert_eq!(
        &parts[..7],
        ["ready", "1", "1", "flaky", "temporary failure", "f", "t"]
    );
    // the first backoff is base 10 s with half jitter: 5..=10 s
    let wait: i32 = parts[7].parse().unwrap();
    assert!((4..=10).contains(&wait), "waits {wait} s");
    // not due: the next tick finds nothing
    assert_eq!(runner.tick().await.unwrap().claimed, 0);
    due_now(&h).await;
    runner.tick().await.unwrap();
    let second = row(&h, jobs[0]).await;
    let parts: Vec<&str> = second.split('|').collect();
    assert_eq!(&parts[..3], ["ready", "2", "2"]);
    // the second backoff doubles: 10..=20 s
    let wait: i32 = parts[7].parse().unwrap();
    assert!((9..=20).contains(&wait), "waits {wait} s");
    due_now(&h).await;
    runner.tick().await.unwrap();
    // the third attempt was the last (max_attempts 3): dead with the reason
    assert_eq!(
        row(&h, jobs[0]).await,
        "dead|3|2|flaky|temporary failure (attempt 3 of 3: no attempt is left)|t|t|0"
    );
    let metrics = runner.metrics();
    assert_eq!((metrics.retried, metrics.dead), (2, 1));
}

#[tokio::test]
async fn a_permanent_failure_dead_letters_the_job_on_the_first_attempt() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![script("permanent")]).await;
    runner(&h).tick().await.unwrap();
    assert_eq!(
        row(&h, jobs[0]).await,
        "dead|1|0|broken|cannot be fixed|t|t|0"
    );
}

#[tokio::test]
async fn a_yield_reschedules_the_job_without_using_an_attempt() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![script("yield")]).await;
    runner(&h).tick().await.unwrap();
    let state = cell(
        &h,
        &format!(
            "SELECT concat_ws('|', state, attempts, coalesce(last_error_code, 'none'), \
             round(extract(epoch FROM run_after - clock_timestamp()))::int BETWEEN 58 AND 60) \
             FROM erp_job WHERE id = {}",
            jobs[0].id()
        ),
    )
    .await;
    // back to ready with the attempt given back, no error recorded, due in about a minute
    assert_eq!(state, "ready|0|none|t");
}

#[tokio::test]
async fn a_chunk_that_made_progress_restarts_the_attempts_so_earlier_failures_do_not_dead_letter() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![script("progress_then_transient")]).await;
    // two earlier failed attempts are on the clock (max_attempts is 3)
    exec(&h, "UPDATE erp_job SET attempts = 2").await;
    let runner = runner(&h);
    runner.tick().await.unwrap();
    // this run's chunk committed progress, so the failure after it is the first since progress:
    // the job retries instead of dying at attempt 3
    let state = row(&h, jobs[0]).await;
    let parts: Vec<&str> = state.split('|').collect();
    assert_eq!(
        &parts[..5],
        [
            "ready",
            "1",
            "1",
            "flaky",
            "temporary failure after progress"
        ]
    );
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT items_done || '|' || progress FROM erp_job WHERE id = {}",
                jobs[0].id()
            )
        )
        .await,
        "2|{\"step\": 1}"
    );
}

#[tokio::test]
async fn a_serialization_failure_inside_a_chunk_runs_the_chunk_again_without_a_failed_attempt() {
    let h = harness().await;
    support::jobs::SERIALIZATION_RUNS.store(0, std::sync::atomic::Ordering::SeqCst);
    enqueue(&h, vec![script("serialization_once")]).await;
    let runner = runner(&h);
    runner.tick().await.unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(
        cell(&h, "SELECT outcome || '|' || attempts FROM erp_job_run").await,
        "done|1"
    );
    let metrics = runner.metrics();
    assert_eq!(
        (metrics.retried, metrics.dead, metrics.completed),
        (0, 0, 1)
    );
}

#[tokio::test]
async fn a_panicking_handler_is_a_transient_failure_and_frees_its_slot() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![script("panic"), script("native_done")]).await;
    let runner = runner(&h);
    runner.tick().await.unwrap();
    assert_eq!(
        row(&h, jobs[0])
            .await
            .split('|')
            .take(5)
            .collect::<Vec<_>>(),
        ["ready", "1", "1", "panic", "the job handler panicked"]
    );
    // the other job of the batch ran, and the runner counted the panic
    assert_eq!(support::jobs::state(&h, jobs[1]).await, "");
    assert_eq!(runner.metrics().panics, 1);
    assert_eq!(runner.metrics().completed, 1);
}

#[tokio::test]
async fn continue_and_a_finished_native_handler_settle_as_the_run_says() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![script("continue"), script("native_done")]).await;
    let runner = runner(&h);
    runner.tick().await.unwrap();
    let state = row(&h, jobs[0]).await;
    assert_eq!(
        &state.split('|').collect::<Vec<_>>()[..3],
        ["ready", "0", "0"]
    );
    // a native handler that returns Done without a finishing chunk: the runner completes it
    assert_eq!(support::jobs::state(&h, jobs[1]).await, "");
    assert_eq!(
        cell(&h, "SELECT outcome || '|' || attempts FROM erp_job_run").await,
        "done|1"
    );
}

#[tokio::test]
async fn a_run_past_the_job_deadline_is_stopped_and_retried_as_transient() {
    use erp_orm::jobs::{JobRunner, JobSettings, Setting};
    let h = harness().await;
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(
            JobSettings::builder()
                .set(Setting::JobDeadlineSecs, 10)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let jobs = enqueue(&h, vec![script("sleep")]).await;
    let runner = JobRunner::builder(orm, support::jobs::PassResolver)
        .build()
        .unwrap();
    runner.tick().await.unwrap();
    let state = row(&h, jobs[0]).await;
    assert_eq!(
        &state.split('|').collect::<Vec<_>>()[..5],
        [
            "ready",
            "1",
            "1",
            "deadline_exceeded",
            "the run did not end within jobs.job_deadline_secs (10 s)"
        ]
    );
}

/// message that fits the column even when the message itself is at the limit)
#[tokio::test]
async fn a_transient_error_of_the_full_message_size_on_the_last_attempt_dead_letters_with_its_code()
{
    let h = harness().await;
    let jobs = enqueue(&h, vec![script("transient_huge").max_attempts(1).unwrap()]).await;
    runner(&h).tick().await.unwrap();
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', state, last_error_code, octet_length(last_error) <= 4096, \
                 last_error_code = 'lease_expired') FROM erp_job WHERE id = {}",
                jobs[0].id()
            )
        )
        .await,
        "dead|flaky|t|f"
    );
}
