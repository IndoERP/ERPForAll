//! stage H: deferred performance identities.
//!
//! Compiled and discoverable, never run by ordinary CI: every test is `#[ignore]` with the reason
//! `DEFERRED_PERFORMANCE` and asserts nothing about a number. Run them by hand in the
//! milestone-end performance phase on the benchmark host:
//!
//! ```text
//! cargo nextest run -p erp-test-models --test jobs_perf_pickup --run-ignored only --no-capture
//! ```
//!
//! They print raw lines (`PICKUP ...`, `NOTIFY_MIX ...`); the numbers belong in a committed
//! result file with the host and the tool versions, written by the tool, never typed. No value
//! measured here exists yet.
//!
//! The BENCH-007 variant of the design (the BENCH-007 mix plus a job-enqueuing write, NOTIFY on
//! against off, next to SIGNAL's ceiling) needs the `erp-bench` mix driver; `notify_mix_*` below
//! measures the enqueue side alone (committing enqueuers per second with the NOTIFY on and
//! off) and is not BENCH-007: the integration with that driver is open.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    reason = "tests"
)]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use erp_orm::jobs::{
    ChunkResult, EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl,
    JobRunner, JobSettings, RunOutcome, Setting,
};
use erp_orm::{BoxFuture, ListenerOptions, Orm, Signal, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::NoTls;
use support::jobs::{PassResolver, cell, exec};

/// Writes the database time at which its first chunk started, next to the enqueue time the
/// payload's row carries.
struct Stamp;

impl JobHandler for Stamp {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let seq = ctx.payload::<serde_json::Value>()?["seq"]
                .as_i64()
                .unwrap_or(0);
            ctx.chunk(move |tx| {
                Box::pin(async move {
                    tx.execute(
                        "UPDATE perf_pickup SET started = clock_timestamp() WHERE seq = $1",
                        &[&seq],
                    )
                    .await?;
                    Ok(ChunkResult::done(serde_json::json!({}), 1))
                })
            })
            .await?;
            Ok(RunOutcome::Done)
        })
    }
}

static STAMP: Stamp = Stamp;

erp_orm::register_job_kind!(
    STAMP_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.stamp", HandlerRef::Native(&STAMP))
);

async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    exec(
        &h,
        "CREATE TABLE perf_pickup (seq int8 PRIMARY KEY, enqueued timestamptz, started timestamptz)",
    )
    .await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    h
}

fn pool(h: &Harness) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), 8).unwrap()
}

async fn runner_orm(h: &Harness, poll_secs: u64) -> Orm {
    let client = h.db().client().await.unwrap();
    let baseline = Signal::read(&client).await.unwrap();
    Orm::builder(Arc::clone(h.registry()), pool(h))
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(baseline))
        .job_settings(
            JobSettings::builder()
                .set(Setting::PollIntervalSecs, poll_secs)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap()
}

/// Enqueue `count` jobs at `per_second`, wait for all to start, print the percentiles of the
/// commit-to-start latency (database clock).
async fn pickup(h: &Harness, per_second: u64, count: i64, listener: bool) {
    exec(h, "TRUNCATE perf_pickup").await;
    let orm = runner_orm(h, if listener { 300 } else { 1 }).await;
    let runner = JobRunner::builder(orm.clone(), PassResolver)
        .build()
        .unwrap();
    let listening = listener.then(|| {
        orm.spawn_signal_listener(h.db().config(), NoTls, ListenerOptions::default())
            .unwrap()
    });
    let handle = runner.spawn();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let gap = Duration::from_micros(1_000_000 / per_second.max(1));
    let started = Instant::now();
    for seq in 0..count {
        let mut tx = h.begin_as(7).await.unwrap();
        let env = tx.env();
        tx.execute(
            "INSERT INTO perf_pickup (seq, enqueued) VALUES ($1, clock_timestamp())",
            &[&seq],
        )
        .await
        .unwrap();
        tx.enqueue(
            &env,
            EnqueueRequest::new("test_jobs.stamp")
                .payload(&serde_json::json!({ "seq": seq }))
                .unwrap(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let due = started + gap * u32::try_from(seq + 1).unwrap_or(u32::MAX);
        tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline
        && cell(
            h,
            "SELECT count(*) FILTER (WHERE started IS NULL)::text FROM perf_pickup",
        )
        .await
            != "0"
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let line = cell(
        h,
        "SELECT format('p50=%s p95=%s p99=%s ms n=%s', \
         round((percentile_cont(0.5) WITHIN GROUP (ORDER BY ms))::numeric, 2), \
         round((percentile_cont(0.95) WITHIN GROUP (ORDER BY ms))::numeric, 2), \
         round((percentile_cont(0.99) WITHIN GROUP (ORDER BY ms))::numeric, 2), count(*)) \
         FROM (SELECT extract(epoch FROM started - enqueued) * 1000 AS ms FROM perf_pickup \
               WHERE started IS NOT NULL) t",
    )
    .await;
    eprintln!("PICKUP rate={per_second}/s notify={listener} {line}");
    handle.shutdown().await;
    if let Some(listening) = listening {
        listening.shutdown().await;
    }
}

#[tokio::test]
#[ignore = "DEFERRED_PERFORMANCE: milestone-end pick-up latency"]
async fn pickup_latency_with_notify_at_1_100_and_1000_per_second() {
    let h = harness().await;
    for rate in [1, 100, 1_000] {
        pickup(&h, rate, 200, true).await;
    }
}

#[tokio::test]
#[ignore = "DEFERRED_PERFORMANCE: milestone-end pick-up latency by polling only"]
async fn pickup_latency_by_polling_only_at_1_100_and_1000_per_second() {
    let h = harness().await;
    for rate in [1, 100, 1_000] {
        pickup(&h, rate, 200, false).await;
    }
}

/// Committing enqueuers per second for a fixed time, NOTIFY coalescing at the default interval
/// and at the floor (not BENCH-007; see the module note).
async fn notify_mix(h: &Harness, interval_ms: u64) {
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(
            JobSettings::builder()
                .set(Setting::NotifyIntervalMs, interval_ms)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let committed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = Instant::now() + Duration::from_secs(10);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let orm = orm.clone();
        let committed = Arc::clone(&committed);
        tasks.push(tokio::spawn(async move {
            while Instant::now() < stop {
                let mut tx = orm.begin(erp_orm::UserId::new(7).unwrap()).await.unwrap();
                let env = tx.env();
                tx.enqueue(
                    &env,
                    EnqueueRequest::new("test_jobs.stamp")
                        .payload(&serde_json::json!({ "seq": -1 }))
                        .unwrap(),
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
                committed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    eprintln!(
        "NOTIFY_MIX notify_interval_ms={interval_ms} commits_per_second={}",
        committed.load(std::sync::atomic::Ordering::Relaxed) / 10
    );
}

#[tokio::test]
#[ignore = "DEFERRED_PERFORMANCE: milestone-end enqueue throughput with NOTIFY on and off"]
async fn notify_mix_enqueue_throughput_with_notify_coalesced_and_at_the_floor() {
    let h = harness().await;
    notify_mix(&h, 1_000).await;
    notify_mix(&h, 50).await;
}
