//! stage C/D: the fence (fencing epochs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Mutex;
use std::time::Duration;

use erp_orm::BoxFuture;
use erp_orm::jobs::{
    ChunkResult, EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKey, JobKindDecl,
    JobRunner, RunOutcome,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{PassResolver, cell, enqueue, exec};

/// What the handler saw when its chunk was refused.
static REFUSALS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// What a `probe` job saw inside its chunk: seconds of lease left, advisory key locks held.
static PROBES: Mutex<Vec<(i64, i64)>> = Mutex::new(Vec::new());

/// One chunk that writes an effect row; the refusal, if any, is recorded.
struct Writer;

impl JobHandler for Writer {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let probe = ctx.payload::<serde_json::Value>()?.get("probe").is_some();
            let result = ctx
                .chunk(move |tx| {
                    Box::pin(async move {
                        if probe {
                            let rows = tx
                                .query(
                                    "SELECT round(extract(epoch FROM lease_until - clock_timestamp()))::int8, \
                                     (SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' \
                                      AND classid = 1703 AND pid = pg_backend_pid())::int8 \
                                     FROM erp_job WHERE state = 'running'",
                                    &[],
                                )
                                .await?;
                            PROBES.lock().unwrap().push((rows[0].get(0), rows[0].get(1)));
                        }
                        tx.execute("INSERT INTO jobs_effect (note) VALUES ('written')", &[])
                            .await?;
                        Ok(ChunkResult::done(serde_json::json!({"done": true}), 1))
                    })
                })
                .await;
            match result {
                Ok(_) => Ok(RunOutcome::Done),
                Err(error) => {
                    REFUSALS.lock().unwrap().push(error.to_string());
                    Err(error)
                }
            }
        })
    }
}

static WRITER: Writer = Writer;

erp_orm::register_job_kind!(
    WRITER_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.writer", HandlerRef::Native(&WRITER))
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

fn writer() -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.writer")
}

fn runner(h: &Harness) -> JobRunner {
    JobRunner::builder(h.orm().clone(), PassResolver)
        .build()
        .unwrap()
}

async fn effects(h: &Harness) -> String {
    cell(h, "SELECT count(*)::text FROM jobs_effect").await
}

#[tokio::test]
async fn a_chunk_with_a_stale_epoch_writes_nothing_and_the_run_stops() {
    let h = harness().await;
    REFUSALS.lock().unwrap().clear();
    let jobs = enqueue(&h, vec![writer()]).await;
    let runner = runner(&h);
    let claimed = runner.claim(1).await.unwrap();
    // another worker took the job over since: the epoch moved on
    exec(&h, "UPDATE erp_job SET lease_epoch = lease_epoch + 1").await;
    runner
        .run_claimed(claimed.into_iter().next().unwrap())
        .await
        .unwrap();
    assert_eq!(effects(&h).await, "0");
    assert_eq!(
        REFUSALS.lock().unwrap().as_slice(),
        [format!("the lease of job {} was lost", jobs[0].uuid())]
    );
    // the job is untouched by the deposed run: still running under the new epoch
    assert_eq!(support::jobs::state(&h, jobs[0]).await, "running");
    assert_eq!(runner.metrics().lease_lost, 1);
}

#[tokio::test]
async fn a_chunk_after_the_lease_expired_is_refused() {
    let h = harness().await;
    REFUSALS.lock().unwrap().clear();
    enqueue(&h, vec![writer()]).await;
    let runner = runner(&h);
    let claimed = runner.claim(1).await.unwrap();
    exec(
        &h,
        "UPDATE erp_job SET lease_until = clock_timestamp() - interval '1 second'",
    )
    .await;
    runner
        .run_claimed(claimed.into_iter().next().unwrap())
        .await
        .unwrap();
    assert_eq!(effects(&h).await, "0");
    assert_eq!(REFUSALS.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_chunk_moves_the_lease_forward() {
    let h = harness().await;
    PROBES.lock().unwrap().clear();
    enqueue(
        &h,
        vec![
            writer()
                .payload(&serde_json::json!({"probe": true}))
                .unwrap(),
        ],
    )
    .await;
    let runner = runner(&h);
    let claimed = runner.claim(1).await.unwrap();
    // the lease is about to run out; the fence of the chunk gives it a fresh 90 s
    exec(
        &h,
        "UPDATE erp_job SET lease_until = clock_timestamp() + interval '5 seconds'",
    )
    .await;
    runner
        .run_claimed(claimed.into_iter().next().unwrap())
        .await
        .unwrap();
    let probes = PROBES.lock().unwrap().clone();
    assert_eq!(probes.len(), 1);
    assert!((88..=90).contains(&probes[0].0), "{probes:?}");
    // a job without a singleton key takes no key lock
    assert_eq!(probes[0].1, 0);
    assert_eq!(effects(&h).await, "1");
}

#[tokio::test]
async fn a_chunk_of_a_singleton_job_holds_the_keys_advisory_lock() {
    let h = harness().await;
    PROBES.lock().unwrap().clear();
    enqueue(
        &h,
        vec![
            writer()
                .payload(&serde_json::json!({"probe": true}))
                .unwrap()
                .singleton(JobKey::new("k").unwrap()),
        ],
    )
    .await;
    runner(&h).tick().await.unwrap();
    let probes = PROBES.lock().unwrap().clone();
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].1, 1);
}

#[tokio::test]
async fn a_deposed_owner_cannot_commit_after_the_job_was_reaped_and_claimed_again() {
    let h = harness().await;
    REFUSALS.lock().unwrap().clear();
    enqueue(&h, vec![writer()]).await;
    let (first, second) = (runner(&h), runner(&h));
    let deposed = first.claim(1).await.unwrap().into_iter().next().unwrap();
    exec(
        &h,
        "UPDATE erp_job SET lease_until = clock_timestamp() - interval '1 second'",
    )
    .await;
    assert_eq!(second.reap(10).await.unwrap().requeued, 1);
    exec(&h, "UPDATE erp_job SET run_after = clock_timestamp()").await;
    let successor = second.claim(1).await.unwrap().into_iter().next().unwrap();
    assert_eq!((deposed.epoch(), successor.epoch()), (1, 2));
    // the deposed owner's chunk is refused; the successor's commits and completes the job
    first.run_claimed(deposed).await.unwrap();
    assert_eq!(effects(&h).await, "0");
    second.run_claimed(successor).await.unwrap();
    assert_eq!(effects(&h).await, "1");
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(
        cell(&h, "SELECT outcome || '|' || attempts FROM erp_job_run").await,
        "done|2"
    );
}

#[tokio::test]
async fn a_deposed_singleton_owner_never_overlaps_the_successor_that_holds_the_key() {
    let h = harness().await;
    REFUSALS.lock().unwrap().clear();
    enqueue(
        &h,
        vec![writer().singleton(JobKey::new("only-one").unwrap())],
    )
    .await;
    let (first, second) = (runner(&h), runner(&h));
    let deposed = first.claim(1).await.unwrap().into_iter().next().unwrap();
    exec(
        &h,
        "UPDATE erp_job SET lease_until = clock_timestamp() - interval '1 second'",
    )
    .await;
    second.reap(10).await.unwrap();
    exec(&h, "UPDATE erp_job SET run_after = clock_timestamp()").await;
    let successor = second.claim(1).await.unwrap().into_iter().next().unwrap();
    // the successor holds the key lock inside an open transaction (its chunk in progress)
    let mut holder = h.begin().await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_xact_lock(1703, hashtext('only-one'))",
            &[],
        )
        .await
        .unwrap();
    // the deposed owner's chunk fences first: its epoch is stale, so it fails there without
    // reaching the key: no wait, no write, no overlap with the successor's chunk
    let started = std::time::Instant::now();
    first.run_claimed(deposed).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(effects(&h).await, "0");
    holder.rollback().await.unwrap();
    second.run_claimed(successor).await.unwrap();
    assert_eq!(effects(&h).await, "1");
}
