//! stage E: wake-ups inside the NOTIFY budget.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use erp_orm::jobs::{
    EnqueueRequest, HandlerRef, JobKindDecl, JobRunner, JobSettings, LiveNodes, Setting, When,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{AsyncMessage, NoTls};
use support::jobs::{PassResolver, SCRIPT, cell, enqueue, script};

erp_orm::register_job_kind!(
    SCRIPT_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.script", HandlerRef::Native(&SCRIPT))
);

async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    erp_orm::jobs::reset_notify_state_for_tests(h.orm().id());
    h
}

/// A client that listens on `erp_jobs` and records every notification's payload.
struct Listener {
    payloads: Arc<Mutex<Vec<String>>>,
    _client: erp_testkit::tokio_postgres::Client,
}

impl Listener {
    async fn start(h: &Harness) -> Self {
        let (client, mut connection) = h.db().config().connect(NoTls).await.unwrap();
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&payloads);
        tokio::spawn(async move {
            while let Some(message) = std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                if let Ok(AsyncMessage::Notification(n)) = message {
                    seen.lock().unwrap().push(n.payload().to_owned());
                }
            }
        });
        client.batch_execute("LISTEN erp_jobs").await.unwrap();
        Self {
            payloads,
            _client: client,
        }
    }

    /// Notifications received so far, after letting the server deliver the last commit's.
    async fn count(&self) -> usize {
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.payloads.lock().unwrap().len()
    }
}

/// A registry view that shows no other node: this process is the only job node.
struct Alone;

impl LiveNodes for Alone {
    fn live_workers(&self) -> u32 {
        1
    }
    fn has_peers(&self) -> bool {
        false
    }
    fn ready(&self) -> bool {
        true
    }
}

/// A registry view with another live node.
struct WithPeer;

impl LiveNodes for WithPeer {
    fn live_workers(&self) -> u32 {
        2
    }
    fn has_peers(&self) -> bool {
        true
    }
    fn ready(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn twenty_enqueues_in_an_interval_send_one_empty_notification() {
    let h = harness().await;
    let listener = Listener::start(&h).await;
    let started = Instant::now();
    for _ in 0..20 {
        enqueue(&h, vec![script("native_done")]).await;
    }
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "the burst must fit in one notify interval of 1 s"
    );
    assert_eq!(listener.count().await, 1);
    assert_eq!(listener.payloads.lock().unwrap().as_slice(), [""]);
    // an interval later the next due job notifies again
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    enqueue(&h, vec![script("native_done")]).await;
    assert_eq!(listener.count().await, 2);
}

#[tokio::test]
async fn a_rolled_back_notify_does_not_start_the_interval() {
    let h = harness().await;
    let listener = Listener::start(&h).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    tx.enqueue(&env, EnqueueRequest::new("test_jobs.script"))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(listener.count().await, 0);
    // the next committed enqueue is the first notification of the interval
    enqueue(&h, vec![script("native_done")]).await;
    assert_eq!(listener.count().await, 1);
}

#[tokio::test]
async fn a_future_dated_job_sends_no_notification() {
    let h = harness().await;
    let listener = Listener::start(&h).await;
    enqueue(
        &h,
        vec![script("native_done").at(When::After(Duration::from_secs(3_600)))],
    )
    .await;
    assert_eq!(listener.count().await, 0);
    // a job due within the slack (1 s) does
    enqueue(
        &h,
        vec![script("native_done").at(When::After(Duration::from_millis(500)))],
    )
    .await;
    assert_eq!(listener.count().await, 1);
}

#[tokio::test]
async fn the_only_job_node_wakes_its_own_runner_and_sends_no_notification() {
    let h = harness().await;
    let listener = Listener::start(&h).await;
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(
            JobSettings::builder()
                .set(Setting::PollIntervalSecs, 300)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    erp_orm::jobs::reset_notify_state_for_tests(orm.id());
    let runner = JobRunner::builder(orm.clone(), PassResolver)
        .live_nodes(Alone)
        .build()
        .unwrap();
    let handle = runner.clone().spawn();
    // let the loop go to sleep for its 300 s poll
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut tx = orm.begin(erp_orm::UserId::new(7).unwrap()).await.unwrap();
    let env = tx.env();
    tx.enqueue(
        &env,
        EnqueueRequest::new("test_jobs.script")
            .payload(&serde_json::json!({"do": "native_done"}))
            .unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // only the in-process poke can explain a run within seconds of a 300 s poll
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline
        && cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE kind = 'test_jobs.script'",
        )
        .await
            != "0"
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE kind = 'test_jobs.script'"
        )
        .await,
        "0"
    );
    assert_eq!(listener.count().await, 0, "an only node needs no NOTIFY");
    handle.shutdown().await;
}

#[tokio::test]
async fn with_another_node_alive_the_enqueue_sends_the_notification() {
    let h = harness().await;
    let listener = Listener::start(&h).await;
    let orm = h.orm().rebuild(h.registry().clone()).build().unwrap();
    erp_orm::jobs::reset_notify_state_for_tests(orm.id());
    let _runner = JobRunner::builder(orm.clone(), PassResolver)
        .live_nodes(WithPeer)
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_orm::UserId::new(7).unwrap()).await.unwrap();
    let env = tx.env();
    tx.enqueue(&env, EnqueueRequest::new("test_jobs.script"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(listener.count().await, 1);
}
