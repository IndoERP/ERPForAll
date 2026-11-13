//! stage G: two nodes in one process (Scale-out,
//! processes; the OS-process version belongs to SCALE.TEST.
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

use erp_orm::jobs::{
    EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl, JobRunner,
    JobSettings, LiveNodes, RunOutcome, Setting,
};
use erp_orm::{BoxFuture, Orm, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{PassResolver, cell, enqueue, exec, script};
use tokio::sync::Notify;

static ITEMS: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());
static STALL: AtomicBool = AtomicBool::new(false);
static REACHED: Notify = Notify::const_new();

/// Works through `items` items, checkpointing after each. When `STALL` is set the run stops dead
/// after its fourth item, as a process killed there would.
struct Resumable;

impl JobHandler for Resumable {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let items = ctx.payload::<serde_json::Value>()?["items"]
                .as_u64()
                .unwrap() as u32;
            let owner = ctx.payload::<serde_json::Value>()?["tag"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let start = ctx
                .progress::<serde_json::Value>()?
                .and_then(|p| p["next"].as_u64())
                .unwrap_or(0) as u32;
            for item in start..items {
                ITEMS.lock().unwrap().push((owner.clone(), item));
                ctx.checkpoint(&serde_json::json!({ "next": item + 1 }), 1, None)
                    .await?;
                if STALL.load(Ordering::SeqCst) && item == 3 {
                    REACHED.notify_one();
                    std::future::pending::<()>().await;
                }
            }
            Ok(RunOutcome::Done)
        })
    }
}

static RESUMABLE: Resumable = Resumable;

erp_orm::register_job_kind!(
    RESUMABLE_KIND,
    JobKindDecl::new(
        "test_jobs",
        "test_jobs.resumable",
        HandlerRef::Native(&RESUMABLE)
    )
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
        .policy(StaticPolicy::AllowAll)
        .build()
        .await
        .expect("harness")
}

/// Another ORM lineage on the same database: another process's view of it.
fn another_orm(h: &Harness, settings: JobSettings) -> Orm {
    Orm::builder(
        std::sync::Arc::clone(h.registry()),
        erp_orm::pool::create_pool(h.db().config(), 4).unwrap(),
    )
    .policy(StaticPolicy::AllowAll)
    .job_settings(settings)
    .build()
    .unwrap()
}

fn resumable(tag: &str) -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.resumable")
        .payload(&serde_json::json!({ "items": 8, "tag": tag }))
        .unwrap()
}

/// The owner dies after its checkpoint 4; once its lease expires the other node resumes from the
/// durable progress, the deposed owner's late commit is refused, and no item is lost.
#[tokio::test]
async fn the_survivor_resumes_from_the_durable_progress_and_the_deposed_owner_is_refused() {
    let h = harness().await;
    ITEMS.lock().unwrap().clear();
    STALL.store(true, Ordering::SeqCst);
    let short = JobSettings::builder()
        .lease_for_tests(Duration::from_millis(700))
        .build()
        .unwrap();
    let node_a = JobRunner::builder(another_orm(&h, short.clone()), PassResolver)
        .build()
        .unwrap();
    let node_b = JobRunner::builder(another_orm(&h, short), PassResolver)
        .build()
        .unwrap();
    let jobs = enqueue(&h, vec![resumable("a")]).await;
    let claimed = node_a.claim(1).await.unwrap().into_iter().next().unwrap();
    let late = claimed.clone();
    let run_a = tokio::spawn({
        let node = node_a.clone();
        async move { node.run_claimed(claimed).await.unwrap() }
    });
    tokio::time::timeout(Duration::from_secs(10), REACHED.notified())
        .await
        .expect("node A reached its fourth item");
    // node A is killed: no release, its lease is all that is left
    run_a.abort();
    let _ = run_a.await;
    STALL.store(false, Ordering::SeqCst);
    assert_eq!(support::jobs::state(&h, jobs[0]).await, "running");
    assert_eq!(
        cell(&h, "SELECT progress::text FROM erp_job").await,
        "{\"next\": 4}"
    );
    // after the lease expires node B takes the job over and finishes it from item 4
    tokio::time::sleep(Duration::from_millis(900)).await;
    let reaped = node_b.reap(10).await.unwrap();
    assert_eq!(reaped.requeued, 1);
    exec(&h, "UPDATE erp_job SET run_after = clock_timestamp()").await;
    let taken = node_b.claim(1).await.unwrap().into_iter().next().unwrap();
    assert_eq!(taken.epoch(), 2);
    node_b.run_claimed(taken).await.unwrap();
    let seen = ITEMS.lock().unwrap().clone();
    let by_a: Vec<u32> = seen
        .iter()
        .filter(|(t, _)| t == "a")
        .map(|(_, i)| *i)
        .collect();
    assert_eq!(
        by_a,
        [0, 1, 2, 3, 4, 5, 6, 7],
        "one run, the survivor continued it"
    );
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(
        cell(&h, "SELECT outcome || '|' || items_done FROM erp_job_run").await,
        "done|8"
    );
    // the deposed owner wakes up late and tries to go on: its epoch is stale, nothing is written
    node_a.run_claimed(late).await.unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM erp_job_run").await,
        "1"
    );
    assert_eq!(node_a.metrics().lease_lost, 1);
}

/// A registry view with `n` live workers.
struct Workers(u32);

impl LiveNodes for Workers {
    fn live_workers(&self) -> u32 {
        self.0
    }
    fn has_peers(&self) -> bool {
        self.0 > 1
    }
    fn ready(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn the_database_cap_is_divided_by_the_live_workers() {
    let h = harness().await;
    let settings = JobSettings::builder()
        .set(Setting::MaxRunningPerDb, 4)
        .set(Setting::WorkersPerDb, 4)
        .set(Setting::Workers, 4)
        .build()
        .unwrap();
    let make = |live: u32| {
        JobRunner::builder(another_orm(&h, settings.clone()), PassResolver)
            .live_nodes(Workers(live))
            .build()
            .unwrap()
    };
    let alone = make(1);
    let two = make(2);
    assert_eq!(alone.slot_limit(), 4);
    assert_eq!(two.slot_limit(), 2);
    // a tick of the node with a half share claims at most two of ten waiting jobs
    enqueue(&h, (0..10).map(|_| script("native_done")).collect()).await;
    assert_eq!(two.tick().await.unwrap().claimed, 2);
    assert_eq!(alone.tick().await.unwrap().claimed, 4);
}
