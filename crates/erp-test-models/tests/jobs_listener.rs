//! stage E: channel subscriptions on the signal listener and the runner's poll fallback
//! (3, D12.4).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use erp_orm::jobs::{HandlerRef, JobKindDecl, JobRunner, JobSettings, Setting};
use erp_orm::{ListenerOptions, NotifyChannel, Orm, Signal, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::NoTls;
use support::jobs::{PassResolver, SCRIPT, cell, enqueue, script};

erp_orm::register_job_kind!(
    SCRIPT_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.script", HandlerRef::Native(&SCRIPT))
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

/// A second ORM (its own lineage, as another process would have) with a signal and the job
/// settings given.
async fn second_orm(h: &Harness, settings: JobSettings) -> Orm {
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let client = h.db().client().await.unwrap();
    let baseline = Signal::read(&client).await.unwrap();
    Orm::builder(Arc::clone(h.registry()), pool(h, 4))
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(baseline))
        .job_settings(settings)
        .build()
        .unwrap()
}

fn settings(poll_secs: u64) -> JobSettings {
    JobSettings::builder()
        .set(Setting::PollIntervalSecs, poll_secs)
        .build()
        .unwrap()
}

async fn script_jobs(h: &Harness) -> String {
    cell(
        h,
        "SELECT count(*)::text FROM erp_job WHERE kind = 'test_jobs.script'",
    )
    .await
}

async fn until_gone(h: &Harness, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if script_jobs(h).await == "0" {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn a_notification_on_a_subscribed_channel_wakes_its_subscriber() {
    let h = harness().await;
    let orm = second_orm(&h, JobSettings::default()).await;
    let signal = orm.signal().unwrap();
    let wake = signal
        .subscribe(&NotifyChannel::new("jobs_probe").unwrap())
        .unwrap();
    let other = signal
        .subscribe(&NotifyChannel::new("jobs_other").unwrap())
        .unwrap();
    let handle = orm
        .spawn_signal_listener(h.db().config(), NoTls, ListenerOptions::default())
        .unwrap();
    // the first wake is the catch-up of the session that just started
    tokio::time::timeout(Duration::from_secs(5), wake.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), other.notified())
        .await
        .unwrap();
    let client = h.db().client().await.unwrap();
    client
        .batch_execute("NOTIFY jobs_probe, 'ignored'")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), wake.notified())
        .await
        .unwrap();
    // the other channel's subscriber was not woken by it
    assert!(
        tokio::time::timeout(Duration::from_millis(300), other.notified())
            .await
            .is_err()
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn the_signal_channel_cannot_be_subscribed() {
    let h = harness().await;
    let orm = second_orm(&h, JobSettings::default()).await;
    let error = orm
        .signal()
        .unwrap()
        .subscribe(&NotifyChannel::signal())
        .unwrap_err();
    assert_eq!(
        error.message(),
        "the channel erp_signal is the signal listener's own; subscribe to another"
    );
}

#[tokio::test]
async fn a_reconnect_wakes_every_subscriber_once() {
    let h = harness().await;
    let orm = second_orm(&h, JobSettings::default()).await;
    let wake = orm
        .signal()
        .unwrap()
        .subscribe(&NotifyChannel::new("jobs_probe").unwrap())
        .unwrap();
    let options =
        ListenerOptions::default().backoff(Duration::from_millis(50), Duration::from_millis(200));
    let handle = orm
        .spawn_signal_listener(h.db().config(), NoTls, options)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), wake.notified())
        .await
        .unwrap();
    // the connection goes away (as on a failover); the listener reconnects and catches up
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid()",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), wake.notified())
        .await
        .unwrap();
    // one wake for the one reconnect
    assert!(
        tokio::time::timeout(Duration::from_millis(500), wake.notified())
            .await
            .is_err()
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn without_a_listener_the_runner_claims_within_its_poll_interval() {
    let h = harness().await;
    // the runner's ORM is another lineage: no in-process poke, and no listener runs
    let orm = Orm::builder(Arc::clone(h.registry()), pool(&h, 4))
        .policy(StaticPolicy::AllowAll)
        .job_settings(settings(1))
        .build()
        .unwrap();
    let runner = JobRunner::builder(orm, PassResolver).build().unwrap();
    let handle = runner.spawn();
    tokio::time::sleep(Duration::from_millis(300)).await;
    enqueue(&h, vec![script("native_done")]).await;
    assert!(
        until_gone(&h, Duration::from_secs(4)).await,
        "polled within 1 s plus the run"
    );
    handle.shutdown().await;
}

/// the poll interval is the 300 s maximum, so only the NOTIFY explains the pick-up)
#[tokio::test]
async fn a_due_enqueue_reaches_another_orms_runner_through_the_listener() {
    let h = harness().await;
    let orm = second_orm(&h, settings(300)).await;
    let runner = JobRunner::builder(orm.clone(), PassResolver)
        .build()
        .unwrap();
    let listener = orm
        .spawn_signal_listener(h.db().config(), NoTls, ListenerOptions::default())
        .unwrap();
    let handle = runner.spawn();
    // let the runner finish its first pass and go to sleep for 300 s
    tokio::time::sleep(Duration::from_millis(800)).await;
    erp_orm::jobs::reset_notify_state_for_tests(h.orm().id());
    enqueue(&h, vec![script("native_done")]).await;
    assert!(
        until_gone(&h, Duration::from_secs(8)).await,
        "the NOTIFY woke the sleeping runner"
    );
    handle.shutdown().await;
    listener.shutdown().await;
}
