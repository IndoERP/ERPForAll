//! SCALE.NODES: the node registry on a real database (design
//! ). Memberships run with a 250 ms interval and a lease of four
//! intervals unless a test sets others, each on its own pool of one connection; faults are real
//! database faults (a table lock, a refusing `CHECK`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use erp_fields::ErrorKind;
use erp_orm::node::{
    AddonContent, AddonsDigest, BuildIdentity, Digest, FailureClass, NODE_DDL, NodeError,
    NodeIdentity, NodeMembership, NodeOptions, NodeRole, NodeRow, NodeState, Readiness,
    RegisterFault, Rollout, SectionName, SettingsWriter, SharedSettings, UnreadyReason, list_nodes,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::StaticPolicy;
use erp_test_models::base_users;
use erp_testkit::deadpool_postgres::{Hook, HookError, Pool, Runtime};
use erp_testkit::{Harness, WireCounter, assert_round_trips};

const TICK: Duration = Duration::from_millis(250);
/// The interval of tests that assert a timing window: wide enough for a busy CI runner.
const SLOW: Duration = Duration::from_secs(1);
const LEASE_INTERVALS: u32 = 4;
/// How long a test waits for something that takes a few intervals.
const PATIENCE: Duration = Duration::from_secs(10);

/// A settings section whose one value makes digests differ.
struct Limits(u64);

impl SharedSettings for Limits {
    fn section(&self) -> SectionName {
        SectionName::new("limits")
    }

    fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
        let Self(value) = self;
        out.shared_u64("limits.value", *value)
    }
}

/// What a test node is.
#[derive(Clone, Copy)]
struct Spec {
    name: &'static str,
    host: &'static str,
    role: NodeRole,
    limits: u64,
    interval: Duration,
    lease: u32,
    retention: Duration,
    dev: bool,
}

impl Spec {
    fn app(name: &'static str) -> Self {
        Self {
            name,
            host: "host-1",
            role: NodeRole::App,
            limits: 1,
            interval: TICK,
            lease: LEASE_INTERVALS,
            retention: Duration::from_secs(30),
            dev: false,
        }
    }

    fn lease(mut self, intervals: u32) -> Self {
        self.lease = intervals;
        self
    }

    fn retention(mut self, retention: Duration) -> Self {
        self.retention = retention;
        self
    }

    fn limits(mut self, limits: u64) -> Self {
        self.limits = limits;
        self
    }

    fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    fn role(mut self, role: NodeRole) -> Self {
        self.role = role;
        self
    }

    fn host(mut self, host: &'static str) -> Self {
        self.host = host;
        self
    }

    fn dev(mut self) -> Self {
        self.dev = true;
        self
    }
}

fn options(spec: &Spec) -> NodeOptions {
    NodeOptions::builder()
        .heartbeat_interval_for_tests(spec.interval)
        .lease_intervals(spec.lease)
        .retention_for_tests(spec.retention)
        .build()
        .unwrap()
}

/// A database after `Orm::sync_schema` (the node tables are core DDL).
async fn setup() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    let report = h.orm().sync_schema().await.expect("schema sync");
    assert!(report.is_empty(), "{report:?}");
    h
}

/// A one-slot pool with its connection already open: the registration bound (and the bound on
/// taking a node id) is one interval (250 ms here) and includes the connection setup, and a cold connect (TCP and SCRAM) on a
/// loaded CI host can take longer than that by itself. The production interval floor (5 s; 10 s
/// by default) leaves room for it.
async fn pool(h: &Harness) -> Pool {
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &NodeOptions::default()).unwrap();
    drop(pool.get().await.unwrap());
    pool
}

fn build() -> BuildIdentity {
    BuildIdentity::__from_build(
        "erp-test-node",
        "1.0.0",
        false,
        Some("0123456789abcdef0123456789abcdef01234567"),
        Some("0"),
    )
}

fn addons() -> AddonsDigest {
    AddonsDigest::builder()
        .addon("base", "1.0.0", AddonContent::Embedded)
        .unwrap()
        .build()
        .unwrap()
}

async fn identity(h: &Harness, spec: Spec) -> Arc<NodeIdentity> {
    // taking the node id is bounded by one interval too: a warm pool, as for the registration
    let warm = pool(h).await;
    let identity = NodeIdentity::builder(spec.role, spec.name, spec.host, build(), addons())
        .settings(&Limits(spec.limits))
        .unwrap()
        .options(options(&spec))
        .dev(spec.dev)
        .create(&warm)
        .await
        .unwrap();
    Arc::new(identity)
}

async fn register(h: &Harness, spec: Spec) -> NodeMembership {
    NodeMembership::register(identity(h, spec).await, pool(h).await)
        .await
        .unwrap()
}

/// Register and request readiness; wait until ready.
async fn start(h: &Harness, spec: Spec) -> NodeMembership {
    let m = register(h, spec).await;
    m.request_ready().await.unwrap();
    wait_ready(&m).await;
    m
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until `done` (an async check) holds.
async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut done: impl FnMut() -> F) {
    let until = Instant::now() + PATIENCE;
    while !done().await {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn count(h: &Harness, statement: &str) -> i64 {
    h.db()
        .client()
        .await
        .unwrap()
        .query_one(statement, &[])
        .await
        .unwrap()
        .get(0)
}

async fn wait_ready(m: &NodeMembership) {
    wait_until("readiness", || m.readiness() == Readiness::Ready).await;
    assert!(m.is_serving());
}

async fn rows(h: &Harness) -> Vec<NodeRow> {
    list_nodes(h.db().pool()).await.unwrap()
}

async fn row(h: &Harness, m: &NodeMembership) -> Option<NodeRow> {
    let id = m.identity().id();
    rows(h).await.into_iter().find(|r| r.id() == id)
}

async fn sql(h: &Harness, statement: &str) {
    h.db()
        .client()
        .await
        .unwrap()
        .batch_execute(statement)
        .await
        .unwrap();
}

async fn detail_count(h: &Harness, m: &NodeMembership) -> i64 {
    let id = m.identity().id().to_string();
    h.db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*) FROM erp_node_detail WHERE node_id = $1::text::uuid",
            &[&id],
        )
        .await
        .unwrap()
        .get(0)
}

/// Whether the database still counts the node's lease as running (its clock, now).
async fn lease_running(client: &tokio_postgres::Client, m: &NodeMembership) -> bool {
    client
        .query_one(
            "SELECT expires_at > clock_timestamp() FROM erp_node WHERE id = $1::text::uuid",
            &[&m.identity().id().to_string()],
        )
        .await
        .unwrap()
        .get(0)
}

/// The send instant of the last successful renewal and the self-fence deadline, on this test's
/// clock.
fn fence_times(m: &NodeMembership) -> (Instant, Instant) {
    let t = m.fence_timing_for_tests();
    let now = Instant::now();
    let until = t.until_fence.expect("the deadline has not passed yet");
    (now - t.since_sent, now + until)
}

fn digest_mismatch(r: &Readiness) -> &erp_orm::node::Mismatch {
    match r {
        Readiness::Unready(UnreadyReason::DigestMismatch(m)) => m,
        other => panic!("expected a digest mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn registration_writes_one_starting_row() {
    let h = setup().await;
    // a long interval: no renewal runs while the row is inspected
    let spec = Spec::app("app-1").interval(Duration::from_secs(60));
    let identity = identity(&h, spec).await;
    assert_eq!(identity.id().version(), 7);
    let m = NodeMembership::register(Arc::clone(&identity), pool(&h).await)
        .await
        .unwrap();
    assert_eq!(m.readiness(), Readiness::Starting);
    assert!(!m.is_serving());
    let all = rows(&h).await;
    assert_eq!(all.len(), 1);
    let r = &all[0];
    assert_eq!(r.id(), identity.id());
    assert_eq!(r.state(), NodeState::Starting);
    assert_eq!(r.unready_reason(), None);
    assert_eq!(r.beat(), 1);
    assert_eq!(r.heartbeat_at(), r.registered_at());
    let lease_us = i64::try_from(identity.options().lease().as_micros()).unwrap();
    assert_eq!(
        r.expires_at().as_microsecond() - r.heartbeat_at().as_microsecond(),
        lease_us
    );
    assert_eq!(detail_count(&h, &m).await, 3);

    let again = NodeMembership::register(Arc::clone(&identity), pool(&h).await)
        .await
        .unwrap_err();
    assert!(matches!(again, NodeError::AlreadyRegistered { .. }));
    assert_eq!(again.kind(), ErrorKind::Programming);
    assert_eq!(
        again.to_string(),
        format!(
            "node {} is already registered in database {}: one membership per process and \
             database",
            identity.id(),
            h.db().name()
        )
    );
    assert_eq!(rows(&h).await.len(), 1);
    m.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_renewal_is_one_statement_in_one_round_trip_and_a_hot_update() {
    let h = setup().await;
    let wire = WireCounter::start(&h.db().config()).await.unwrap();
    let identity = identity(&h, Spec::app("app-1").interval(Duration::from_secs(60))).await;
    let m = NodeMembership::register(identity, wire.pool(1).unwrap())
        .await
        .unwrap();
    let before = row(&h, &m).await.unwrap();
    wire.reset();
    let view = assert_round_trips!(wire, == 1, { m.renew_now_for_tests().await.unwrap() });
    let messages = wire.messages();
    // BEGIN … SET LOCAL and COMMIT are simple queries in the same flight as the one statement
    assert_eq!(
        (messages.parse, messages.execute, messages.query),
        (1, 1, 2)
    );
    let after = row(&h, &m).await.unwrap();
    assert!(after.expires_at() > before.expires_at());
    assert_eq!(after.registered_at(), before.registered_at());
    assert_eq!(after.beat(), before.beat() + 1);
    assert_eq!(view.beat(), after.beat());

    // HOT: every update of the row stays on its page (no index on a changed column)
    let client = h.db().client().await.unwrap();
    let stats = || async {
        let row = client
            .query_one(
                "SELECT n_tup_upd, n_tup_hot_upd FROM pg_stat_user_tables WHERE relname = \
                 'erp_node'",
                &[],
            )
            .await
            .unwrap();
        (row.get::<_, i64>(0), row.get::<_, i64>(1))
    };
    // the cumulative statistics arrive after the writer flushes them (about a second)
    let until = Instant::now() + Duration::from_secs(30);
    let base = loop {
        let s = stats().await;
        if s.0 >= 1 {
            break s;
        }
        assert!(
            Instant::now() < until,
            "statistics never showed the first renewal"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    for _ in 0..10 {
        m.renew_now_for_tests().await.unwrap();
    }
    let until = Instant::now() + Duration::from_secs(30);
    let now = loop {
        let s = stats().await;
        if s.0 >= base.0 + 10 {
            break s;
        }
        assert!(
            Instant::now() < until,
            "statistics never showed ten renewals"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(now.0 - base.0, 10, "updates");
    assert_eq!(now.1 - base.1, now.0 - base.0, "HOT updates");
    m.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_crashed_node_drops_out_after_its_lease_and_retention_removes_it() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a")).await;
    let b = start(&h, Spec::app("app-b")).await;
    let view = a.renew_now_for_tests().await.unwrap();
    assert_eq!(view.divisor(NodeRole::App).get(), 2);
    let b_id = b.identity().id();
    // the process dies: no shutdown
    drop(b);
    wait_until("B to drop out of A's count", || {
        a.view().divisor(NodeRole::App).get() == 1
    })
    .await;
    let dead = rows(&h).await.into_iter().find(|r| r.id() == b_id).unwrap();
    assert!(!dead.is_fresh());
    assert_ne!(dead.state(), NodeState::Stopped);

    // past the retention, the first round of the next node to register deletes it (A paused:
    // its rounds purge too)
    a.pause_heartbeat_for_tests(true);
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '2 hours', \
             expires_at = now() - interval '2 hours' WHERE id = '{b_id}'"
        ),
    )
    .await;
    let c = register(&h, Spec::app("app-c")).await;
    wait_for("C's first purge", || async {
        rows(&h).await.iter().all(|r| r.id() != b_id)
    })
    .await;
    let ids: Vec<_> = rows(&h).await.iter().map(NodeRow::id).collect();
    assert!(ids.contains(&a.identity().id()));
    let b_details: i64 = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*) FROM erp_node_detail WHERE node_id = $1::text::uuid",
            &[&b_id.to_string()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(b_details, 0);

    // and a renewal purges too
    let c_id = c.identity().id();
    drop(c);
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '2 hours', \
             expires_at = now() - interval '2 hours' WHERE id = '{c_id}'"
        ),
    )
    .await;
    a.pause_heartbeat_for_tests(false);
    let until = Instant::now() + PATIENCE;
    loop {
        let ids: Vec<_> = rows(&h).await.iter().map(NodeRow::id).collect();
        if !ids.contains(&c_id) {
            assert!(ids.contains(&a.identity().id()));
            break;
        }
        assert!(Instant::now() < until, "A's renewals never purged C's row");
        tokio::time::sleep(TICK).await;
    }
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_newcomer_with_another_configuration_stays_unready_and_says_why() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a").limits(10)).await;
    let b = register(&h, Spec::app("app-b").limits(20)).await;
    let readiness = b.request_ready().await.unwrap();
    let mismatch = digest_mismatch(&readiness);
    let (da, db) = (a.identity().digest(), b.identity().digest());
    let name = h.db().name();
    assert_eq!(
        mismatch.message(),
        format!(
            "database {name}: this node (digest {db}) differs from the 1 ready node with digest \
             {da}: configuration differs (sections: limits); build matches; addons match. Ship \
             the same configuration, build and addons, or open a rollout window: erp cluster \
             rollout open -d {name} --to {db}"
        )
    );
    assert_eq!(mismatch.sections(), ["limits"]);
    assert!(mismatch.addons().is_empty());
    assert!(!mismatch.build_differs());
    assert_eq!(mismatch.reference(), Some(da));
    assert!(!b.is_serving());
    let r = row(&h, &b).await.unwrap();
    assert_eq!(r.state(), NodeState::Unready);
    assert_eq!(r.unready_reason(), Some("digest_mismatch"));
    // the ready node keeps serving and reports the peer
    let view = a.renew_now_for_tests().await.unwrap();
    assert_eq!(a.readiness(), Readiness::Ready);
    assert!(a.is_serving());
    assert_eq!(view.digest_mismatch().unwrap().groups(), [(db, 1)]);
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_rollout_window_allows_exactly_two_versions_for_a_bounded_time() {
    async fn wait(what: &str, patience: Duration, mut done: impl FnMut() -> bool) {
        let until = Instant::now().checked_add(patience).unwrap();
        while !done() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_rollout_ready(m: &NodeMembership, patience: Duration) {
        wait("rollout readiness", patience, || {
            m.readiness() == Readiness::Ready
        })
        .await;
        assert!(m.is_serving());
    }

    let h = setup().await;
    // This case checks rollout decisions, not registration latency. Measure warm registrations
    // on this run; one shared interval keeps digests comparable and covers scheduling variation.
    let mut slowest = Duration::ZERO;
    for name in ["calibration-a", "calibration-b", "calibration-c"] {
        let identity = identity(&h, Spec::app(name).interval(Duration::from_secs(60))).await;
        let warm = pool(&h).await;
        let sent = Instant::now();
        let sample = NodeMembership::register(identity, warm).await.unwrap();
        let elapsed = sent.elapsed();
        slowest = slowest.max(elapsed);
        eprintln!("rollout calibration {name}: warm registration {elapsed:?}");
        sample.shutdown().await.unwrap();
    }
    let interval = slowest.checked_mul(8).unwrap().max(Duration::from_secs(1));
    assert!(
        interval <= Duration::from_secs(60),
        "rollout calibration needs interval {interval:?}, beyond the permitted 60 s"
    );
    // A readiness transition can settle for an interval and retry on the next renewal.
    let patience = interval.checked_mul(8).unwrap();
    eprintln!("rollout fixture: interval {interval:?}, watchdog {patience:?}");
    let spec = |name, limits| Spec::app(name).limits(limits).interval(interval);
    let admin = h.db().pool();
    let name = h.db().name().to_owned();
    let a = register(&h, spec("app-a", 1)).await;
    a.request_ready().await.unwrap();
    wait_rollout_ready(&a, patience).await;
    let d1 = a.identity().digest();
    let b = register(&h, spec("app-b", 2)).await;
    let d2 = b.identity().digest();
    let window = Rollout::open(admin, d2, None, Duration::from_secs(1800), Some("test"))
        .await
        .unwrap();
    assert_eq!((window.from(), window.to()), (d1, d2));
    assert!(window.is_active());
    assert_eq!(
        window.until().as_microsecond() - window.opened_at().as_microsecond(),
        1_800_000_000
    );
    b.request_ready().await.unwrap();
    wait_rollout_ready(&b, patience).await;
    let c = register(&h, spec("app-c", 3)).await;
    let d3 = c.identity().digest();
    let readiness = c.request_ready().await.unwrap();
    assert_eq!(
        digest_mismatch(&readiness).message(),
        format!(
            "database {name}: this node's digest {d3} is outside the open rollout window from \
             {d1} to {d2}"
        )
    );
    // `from` cannot be defaulted while two digests are ready
    let mut ready = [d1, d2];
    ready.sort();
    let refused = Rollout::open(admin, d3, None, Duration::from_secs(1800), Some("test"))
        .await
        .unwrap_err();
    assert!(matches!(refused, NodeError::RolloutRefused { .. }));
    assert_eq!(refused.kind(), ErrorKind::UserError);
    assert_eq!(
        refused.to_string(),
        format!(
            "database {name}: rollout window refused: the ready nodes run 2 digests ({}, {}); \
             name the version being replaced with --from",
            ready[0], ready[1]
        )
    );
    for (secs, shown) in [(59, "59"), (86_401, "86401")] {
        let refused = Rollout::open(admin, d3, Some(d1), Duration::from_secs(secs), Some("test"))
            .await
            .unwrap_err();
        assert!(matches!(refused, NodeError::RolloutRefused { .. }));
        assert_eq!(refused.kind(), ErrorKind::UserError);
        // refused before any connection: no database in the message
        assert_eq!(
            refused.to_string(),
            format!(
                "rollout window refused: the window must last from 1 minute to 24 hours, got \
                 {shown} s"
            )
        );
    }
    let closed = Rollout::close(admin).await.unwrap().unwrap();
    assert_eq!(closed.id(), window.id());
    assert!(Rollout::current(admin).await.unwrap().is_none());
    // after the window, a newcomer of either version waits while both are live
    let d = register(&h, spec("app-d", 1)).await;
    let e = register(&h, spec("app-e", 2)).await;
    let two = format!(
        "database {name}: a rollout window is needed or ended while two versions are live: {} on \
         1 node, {} on 1 node",
        ready[0], ready[1]
    );
    for m in [&d, &e] {
        match m.request_ready().await.unwrap() {
            Readiness::Unready(UnreadyReason::TwoVersionsLive(t)) => {
                assert_eq!(t.to_string(), two);
            }
            other => panic!("expected two versions live, got {other:?}"),
        }
    }
    // once the old version is gone, the new one is the reference
    a.shutdown().await.unwrap();
    wait_rollout_ready(&e, patience).await;
    wait("D to report a digest mismatch", patience, || {
        matches!(
            d.readiness(),
            Readiness::Unready(UnreadyReason::DigestMismatch(_))
        )
    })
    .await;
    assert_eq!(digest_mismatch(&d.readiness()).reference(), Some(d2));
    for m in [b, c, d, e] {
        m.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn the_live_count_counts_fresh_starting_and_ready_rows_of_a_role() {
    let h = setup().await;
    // G crashed: its lease passed
    let g = register(&h, Spec::app("app-g")).await;
    let g_id = g.identity().id();
    drop(g);
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '1 minute', \
             expires_at = now() - interval '1 minute' WHERE id = '{g_id}'"
        ),
    )
    .await;
    let a = register(&h, Spec::app("app-a")).await;
    // alone: 1 for both roles
    assert_eq!(a.view().divisor(NodeRole::App).get(), 1);
    assert_eq!(a.view().divisor(NodeRole::Worker).get(), 1);
    assert_eq!(a.request_ready().await.unwrap(), Readiness::Ready);
    let c = start(&h, Spec::app("worker-c").role(NodeRole::Worker)).await;
    let b = register(&h, Spec::app("app-b")).await;
    let d = register(&h, Spec::app("app-d").limits(2)).await;
    digest_mismatch(&d.request_ready().await.unwrap());
    let e = register(&h, Spec::app("app-e")).await;
    e.drain().await.unwrap();
    let f = register(&h, Spec::app("app-f")).await;
    let f_id = f.identity().id();
    f.shutdown().await.unwrap();

    let view = a.renew_now_for_tests().await.unwrap();
    assert_eq!(view.divisor(NodeRole::App).get(), 2);
    assert_eq!(view.divisor(NodeRole::Worker).get(), 1);
    let mut peers: Vec<_> = view.fresh_peers().iter().map(|p| p.id()).collect();
    peers.sort();
    let mut expected = vec![
        b.identity().id(),
        c.identity().id(),
        d.identity().id(),
        e.identity().id(),
    ];
    expected.sort();
    assert_eq!(peers, expected);
    assert!(!peers.contains(&f_id) && !peers.contains(&g_id));
    for m in [a, b, c, d, e] {
        m.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn concurrent_readiness_of_different_digests_exactly_one_wins() {
    let h = setup().await;
    let a = register(&h, Spec::app("app-a").limits(1).interval(SLOW)).await;
    let b = register(&h, Spec::app("app-b").limits(2).interval(SLOW)).await;
    // past the settle delay (one interval, counted since registration)
    tokio::time::sleep(SLOW + SLOW / 2).await;
    let (ra, rb) = tokio::join!(a.request_ready(), b.request_ready());
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    let (winner, loser) = match (&ra, &rb) {
        (Readiness::Ready, _) => (&a, &rb),
        (_, Readiness::Ready) => (&b, &ra),
        _ => panic!("nobody won: {ra:?} / {rb:?}"),
    };
    let mismatch = digest_mismatch(loser);
    assert_eq!(mismatch.reference(), Some(winner.identity().digest()));
    assert!(
        mismatch
            .message()
            .contains(&winner.identity().digest().to_string())
    );
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn with_live_peers_a_newcomer_settles_one_interval() {
    let h = setup().await;
    let a = register(&h, Spec::app("app-a").interval(SLOW)).await;
    // no fresh peer: ready from the first request
    assert_eq!(a.request_ready().await.unwrap(), Readiness::Ready);
    let b = register(&h, Spec::app("app-b").interval(SLOW)).await;
    match b.request_ready().await.unwrap() {
        Readiness::Settling { remaining, .. } => {
            assert!(
                remaining > Duration::ZERO && remaining <= SLOW,
                "{remaining:?}"
            );
        }
        other => panic!("expected settling, got {other:?}"),
    }
    assert!(!b.is_serving());
    wait_ready(&b).await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_processes_with_one_node_id_the_second_to_renew_is_fenced() {
    let h = setup().await;
    // one-second interval: the clone's first scheduled renewal is far after the test's
    let a = start(&h, Spec::app("app-a").interval(Duration::from_secs(1))).await;
    let clone = a.clone_process_for_tests(pool(&h).await).await.unwrap();
    a.renew_now_for_tests().await.unwrap();
    let expected = format!(
        "database {}: another process renews node {} (its row is ahead of this process's beat): \
         this process was probably cloned; restart it so it gets a new node id",
        h.db().name(),
        a.identity().id()
    );
    let err = clone.renew_now_for_tests().await.unwrap_err();
    assert!(matches!(err, NodeError::IdentityConflict { .. }));
    assert_eq!(err.kind(), ErrorKind::Conflict);
    assert_eq!(err.to_string(), expected);
    assert_eq!(
        clone.readiness(),
        Readiness::Unready(UnreadyReason::IdentityConflict)
    );
    assert!(!clone.is_serving());
    assert_eq!(
        clone.renew_now_for_tests().await.unwrap_err().to_string(),
        expected
    );
    assert_eq!(
        clone.request_ready().await.unwrap_err().to_string(),
        expected
    );
    // its heartbeats stop
    let beat = clone.view().beat();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(clone.view().beat(), beat);
    assert!(!clone.is_serving());
    a.renew_now_for_tests().await.unwrap();
    assert_eq!(a.readiness(), Readiness::Ready);
    assert!(a.is_serving());
    // a copy that never renewed does not know; its shutdown finds the row ahead and says so
    let unaware = a.clone_process_for_tests(pool(&h).await).await.unwrap();
    a.renew_now_for_tests().await.unwrap();
    let err = unaware.shutdown().await.unwrap_err();
    assert!(matches!(err, NodeError::IdentityConflict { .. }));
    assert_eq!(err.to_string(), expected);
    assert_eq!(row(&h, &a).await.unwrap().state(), NodeState::Ready);
    a.shutdown().await.unwrap();
    drop(clone);
}

#[tokio::test]
async fn a_lost_tail_is_repaired_without_leaving_readiness() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a").interval(Duration::from_secs(60))).await;
    let before = row(&h, &a).await.unwrap();
    let id = a.identity().id();
    sql(&h, &format!("DELETE FROM erp_node WHERE id = '{id}'")).await;
    a.renew_now_for_tests().await.unwrap();
    let after = row(&h, &a).await.unwrap();
    assert_eq!(after.registered_at(), before.registered_at());
    assert_eq!(after.state(), NodeState::Ready);
    assert_eq!(after.beat(), before.beat() + 1);
    // the details come back with the row
    assert_eq!(detail_count(&h, &a).await, 3);
    assert_eq!(a.readiness(), Readiness::Ready);
    sql(
        &h,
        &format!("UPDATE erp_node SET beat = 1 WHERE id = '{id}'"),
    )
    .await;
    a.renew_now_for_tests().await.unwrap();
    let again = row(&h, &a).await.unwrap();
    assert_eq!(again.beat(), before.beat() + 2);
    assert_eq!(a.readiness(), Readiness::Ready);
    assert!(a.is_serving());
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_transient_fault_fences_the_node_and_recovery_goes_through_the_transition() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a").interval(SLOW)).await;
    a.renew_now_for_tests().await.unwrap();
    let fence_after = a.identity().options().fence_after();
    let (sent, deadline) = fence_times(&a);
    assert!(deadline <= sent + fence_after + Duration::from_millis(5));
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute("BEGIN; LOCK TABLE erp_node IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    // the renewals fail transiently; admission goes on until the deadline
    tokio::time::sleep_until((deadline - Duration::from_millis(300)).into()).await;
    let checked_at = Instant::now();
    let serving = a.is_serving();
    assert!(
        serving || checked_at >= deadline,
        "a transient fault must not stop admission before the deadline"
    );
    // from the deadline on, admission is off while the lease still runs on the database clock
    tokio::time::sleep_until((deadline + Duration::from_millis(20)).into()).await;
    assert!(!a.is_serving());
    assert!(lease_running(&locker, &a).await);
    match a.readiness() {
        Readiness::Unready(UnreadyReason::HeartbeatLost { .. }) => {}
        other => panic!("expected heartbeat lost, got {other:?}"),
    }
    wait_until("the failed renewal", || a.view().last_error().is_some()).await;
    let error = a.view().last_error().cloned().unwrap();
    assert_eq!(error.class(), FailureClass::Transient);
    assert!(
        error.sqlstate() == Some("57014")
            || (error.sqlstate().is_none() && error.message().contains("did not finish within")),
        "{error}"
    );
    // hold it past the lease, then release
    tokio::time::sleep_until((sent + a.identity().options().lease() + SLOW).into()).await;
    locker.batch_execute("COMMIT").await.unwrap();
    wait_ready(&a).await;
    assert!(a.view().last_error().is_none());
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_permanent_fault_makes_the_node_unready_at_once() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a")).await;
    sql(
        &h,
        "ALTER TABLE erp_node ADD CONSTRAINT erp_node_refuse CHECK (false) NOT VALID",
    )
    .await;
    wait_until("the refused renewal", || {
        matches!(
            a.readiness(),
            Readiness::Unready(UnreadyReason::HeartbeatRefused { .. })
        )
    })
    .await;
    // refused, not lost: the deadline did not have to pass
    match a.readiness() {
        Readiness::Unready(UnreadyReason::HeartbeatRefused { error }) => {
            assert_eq!(error.class(), FailureClass::Permanent);
            assert_eq!(error.sqlstate(), Some("23514"));
        }
        other => panic!("expected heartbeat refused, got {other:?}"),
    }
    assert!(!a.is_serving());
    let last = a.view().last_error().cloned().unwrap();
    assert_eq!(
        (last.class(), last.sqlstate()),
        (FailureClass::Permanent, Some("23514"))
    );
    sql(&h, "ALTER TABLE erp_node DROP CONSTRAINT erp_node_refuse").await;
    wait_ready(&a).await;
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_starved_heartbeat_task_still_stops_admission_on_time() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a").interval(SLOW)).await;
    a.renew_now_for_tests().await.unwrap();
    a.pause_heartbeat_for_tests(true);
    let fence_after = a.identity().options().fence_after();
    let (sent, deadline) = fence_times(&a);
    // the deadline is never later than the renewal's send plus lease - interval
    assert!(deadline <= sent + fence_after + Duration::from_millis(5));
    tokio::time::sleep_until((deadline - Duration::from_millis(300)).into()).await;
    let checked_at = Instant::now();
    let serving = a.is_serving();
    assert!(
        serving || checked_at >= deadline,
        "a starved task must not stop admission before the deadline"
    );
    // from the deadline on, admission is off while the lease still runs on the database clock
    tokio::time::sleep_until((deadline + Duration::from_millis(20)).into()).await;
    assert!(!a.is_serving());
    let client = h.db().client().await.unwrap();
    assert!(lease_running(&client, &a).await);
    assert_eq!(
        a.readiness(),
        Readiness::Unready(UnreadyReason::HeartbeatLost { last_error: None })
    );
    let first = row(&h, &a).await.unwrap();
    tokio::time::sleep(SLOW).await;
    let second = row(&h, &a).await.unwrap();
    // nothing was written: the row holds the last beat the node sent, still `ready`
    assert_eq!(first.beat(), a.view().beat());
    assert_eq!(second.beat(), first.beat());
    assert_eq!(second.state(), NodeState::Ready);
    a.pause_heartbeat_for_tests(false);
    wait_ready(&a).await;
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_recently_expired_ready_peer_still_defines_the_expected_digest() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a").limits(1).interval(SLOW)).await;
    let d1 = a.identity().digest();
    // identities and pools first: the window below must not include their setup
    let b_identity = identity(&h, Spec::app("app-b").limits(2).interval(SLOW)).await;
    let c_identity = identity(&h, Spec::app("app-c").limits(2).interval(SLOW)).await;
    let (b_pool, c_pool) = (pool(&h).await, pool(&h).await);
    a.pause_heartbeat_for_tests(true);
    let a_id = a.identity().id();
    // expired within the last lease (4 s; a forward jump of the database clock)
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '2 seconds', \
             expires_at = now() - interval '1 second' WHERE id = '{a_id}'"
        ),
    )
    .await;
    let b = NodeMembership::register(b_identity, b_pool).await.unwrap();
    let readiness = b.request_ready().await.unwrap();
    assert_eq!(digest_mismatch(&readiness).reference(), Some(d1));
    // the default `from` of a window follows the same rule: A's digest is still the ready one
    let admin = h.db().pool();
    let window = Rollout::open(
        admin,
        b.identity().digest(),
        None,
        Duration::from_secs(600),
        Some("test"),
    )
    .await
    .unwrap();
    assert_eq!(window.from(), d1);
    Rollout::close(admin).await.unwrap();
    // more than one lease ago: no longer the reference
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '6 seconds', \
             expires_at = now() - interval '5 seconds' WHERE id = '{a_id}'"
        ),
    )
    .await;
    let c = NodeMembership::register(c_identity, c_pool).await.unwrap();
    c.request_ready().await.unwrap();
    wait_ready(&c).await;
    drop(a);
    b.shutdown().await.unwrap();
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_missing_table_fails_registration_with_the_fix() {
    // the harness creates the core tables as the schema sync does (the node tables among
    // them): a database without them is one where they were dropped
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    sql(
        &h,
        "DROP TABLE erp_node_rollout, erp_node_detail, erp_node CASCADE",
    )
    .await;
    let identity = identity(&h, Spec::app("app-a")).await;
    let err = NodeMembership::register(identity, pool(&h).await)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        NodeError::TableMissing {
            table: "erp_node",
            ..
        }
    ));
    assert_eq!(err.kind(), ErrorKind::Programming);
    let name = h.db().name();
    assert_eq!(
        err.to_string(),
        format!(
            "erp_node is missing in database {name}: run the schema sync, `erp module upgrade \
             base -d {name}`"
        )
    );
}

#[tokio::test]
async fn drain_leaves_every_count_at_once_and_shutdown_ends_the_lease() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a")).await;
    let b = start(&h, Spec::app("app-b")).await;
    a.drain().await.unwrap();
    assert!(!a.is_serving());
    assert_eq!(a.readiness(), Readiness::Unready(UnreadyReason::Draining));
    assert_eq!(row(&h, &a).await.unwrap().state(), NodeState::Stopping);
    let view = b.renew_now_for_tests().await.unwrap();
    assert_eq!(view.divisor(NodeRole::App).get(), 1);
    assert!(
        view.fresh_peers()
            .iter()
            .any(|p| p.id() == a.identity().id())
    );
    let a_id = a.identity().id();
    a.shutdown().await.unwrap();
    let gone = rows(&h).await.into_iter().find(|r| r.id() == a_id).unwrap();
    assert_eq!(gone.state(), NodeState::Stopped);
    assert_eq!(gone.expires_at(), gone.heartbeat_at());
    let view = b.renew_now_for_tests().await.unwrap();
    assert!(view.fresh_peers().is_empty());
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_request_role_has_no_privilege_on_the_node_tables() {
    let h = setup().await;
    let role: String = format!("hb_{}", h.db().name()).chars().take(63).collect();
    sql(&h, &format!("CREATE ROLE \"{role}\" NOLOGIN")).await;
    let client = h.db().client().await.unwrap();
    let privileges = || async {
        client
            .query(
                "SELECT c.relname::text, string_agg(a.privilege_type, ',' ORDER BY \
                 a.privilege_type) FROM pg_class c CROSS JOIN LATERAL aclexplode(c.relacl) a \
                 WHERE c.relname IN ('erp_node', 'erp_node_detail', 'erp_node_rollout') \
                 AND c.relkind = 'r' AND a.grantee = (SELECT oid FROM pg_roles WHERE rolname = $1) \
                 GROUP BY c.relname ORDER BY c.relname",
                &[&role],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
            .collect::<Vec<_>>()
    };
    assert!(privileges().await.is_empty());
    let expected = vec![
        (
            "erp_node".to_owned(),
            "DELETE,INSERT,SELECT,UPDATE".to_owned(),
        ),
        (
            "erp_node_detail".to_owned(),
            "DELETE,INSERT,SELECT,UPDATE".to_owned(),
        ),
        ("erp_node_rollout".to_owned(), "DELETE,SELECT".to_owned()),
    ];
    for _ in 0..2 {
        let mut tx = h.begin().await.unwrap();
        tx.grant_node_role(&role).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(privileges().await, expected);
    }
    let owner: String = client
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    let mut tx = h.begin().await.unwrap();
    let err = tx.grant_node_role(&owner).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "role \"{owner}\" owns erp_node: the heartbeat role must be another role (revoking \
             from the owner would remove what the schema sync needs)"
        )
    );
    tx.rollback().await.unwrap();
    sql(
        &h,
        &format!(
            "REVOKE ALL ON erp_node, erp_node_detail, erp_node_rollout FROM \"{role}\"; \
             DROP ROLE \"{role}\""
        ),
    )
    .await;
}

#[tokio::test]
async fn erp_dev_never_joins_a_database_served_by_live_non_dev_nodes() {
    let h = setup().await;
    let app = start(&h, Spec::app("app-1").host("h1")).await;
    let dev = identity(&h, Spec::app("dev-1").host("h2").dev()).await;
    let err = NodeMembership::register(Arc::clone(&dev), pool(&h).await)
        .await
        .unwrap_err();
    assert!(matches!(err, NodeError::DevOnLiveCluster { .. }));
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.to_string(),
        format!(
            "database {}: erp dev refuses to join a database served by live non-dev nodes: app-1 \
             (host h1, pid {}, app); use a development database",
            h.db().name(),
            std::process::id()
        )
    );
    assert!(rows(&h).await.iter().all(|r| r.id() != dev.id()));
    app.shutdown().await.unwrap();
    let joined = NodeMembership::register(Arc::clone(&dev), pool(&h).await)
        .await
        .unwrap();
    let r = row(&h, &joined).await.unwrap();
    assert!(r.is_dev());
    joined.shutdown().await.unwrap();
}

/// Not a spec case: the digest check of `Rollout::open` names both versions in a rollback.
#[tokio::test]
async fn a_rollback_is_a_window_with_from_and_to_swapped() {
    let h = setup().await;
    let admin = h.db().pool();
    let a = start(&h, Spec::app("app-a").limits(1)).await;
    let other: Digest = identity(&h, Spec::app("app-x").limits(9)).await.digest();
    let forward = Rollout::open(admin, other, None, Duration::from_secs(600), Some("test"))
        .await
        .unwrap();
    let back = Rollout::open(
        admin,
        forward.from(),
        Some(forward.to()),
        Duration::from_secs(600),
        Some("test"),
    )
    .await
    .unwrap();
    assert_eq!((back.from(), back.to()), (forward.to(), forward.from()));
    // opening the second closed the first
    let current = Rollout::current(admin).await.unwrap().unwrap();
    assert_eq!(current.id(), back.id());
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stalled_registration_releases_the_readiness_lock_within_its_bound() {
    let h = setup().await;
    let (resume, gate) = tokio::sync::oneshot::channel();
    let stalled_identity = identity(&h, Spec::app("app-stalled")).await;
    let stalled_pool = pool(&h).await;
    let stalled = tokio::spawn(NodeMembership::register_with_fault_for_tests(
        stalled_identity,
        stalled_pool,
        RegisterFault::PauseBetweenFlights(gate),
    ));
    // its transaction holds the readiness lock (two-key advisory lock: class, key 3)
    let (class, key) = erp_orm::node::READINESS_LOCK;
    let held = format!(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND classid = {class} \
         AND objid = {key} AND objsubid = 2 AND granted AND database = (SELECT oid FROM \
         pg_database WHERE datname = current_database())"
    );
    let held = held.as_str();
    wait_for("the stalled registration to take the lock", || async {
        count(&h, held).await == 1
    })
    .await;
    let stalled_at = Instant::now();
    // another registration gets through once the server ends the stalled transaction
    let other = identity(&h, Spec::app("app-b")).await;
    let other_pool = pool(&h).await;
    let b = loop {
        match NodeMembership::register(Arc::clone(&other), other_pool.clone()).await {
            Ok(m) => break m,
            // still waiting for the lock: the client bound or the lock wait's statement timeout
            Err(e) if matches!(e, NodeError::Timeout { .. }) || e.sqlstate() == Some("57014") => {
                assert!(
                    stalled_at.elapsed() < Duration::from_secs(5),
                    "the stalled transaction kept the readiness lock"
                );
            }
            Err(e) => panic!("{e}"),
        }
    };
    // within the stalled transaction's bound and grace, and the time to notice
    assert!(
        stalled_at.elapsed() < Duration::from_secs(3),
        "{:?}",
        stalled_at.elapsed()
    );
    assert_eq!(count(&h, held).await, 0);
    // the stalled process resumes into a terminated session: its registration fails, no row
    resume.send(()).unwrap();
    let err = stalled.await.unwrap().unwrap_err();
    assert!(
        matches!(err, NodeError::Database { .. } | NodeError::Timeout { .. }),
        "{err}"
    );
    assert_eq!(rows(&h).await.len(), 1);
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_registry_transaction_ends_at_its_transaction_bound() {
    let h = setup().await;
    let p = pool(&h).await;
    let conn = p.get().await.unwrap();
    // a longer backstop set for the session (or the role, by an operator): PostgreSQL keeps the
    // timer it armed at the transaction's start unless the registry's own value re-arms it
    conn.batch_execute("SET transaction_timeout = 60000")
        .await
        .unwrap();
    let begin = erp_orm::node::registry_begin_for_tests(
        Duration::from_secs(10),
        Duration::from_millis(400),
    );
    conn.batch_execute(&begin).await.unwrap();
    // short statements back to back: no statement and no idle gap comes near its own bound, so
    // only the transaction bound can end the transaction
    let started = Instant::now();
    let mut ended = None;
    for _ in 0..20 {
        if let Err(e) = conn.batch_execute("SELECT pg_sleep(0.1)").await {
            ended = Some(e);
            break;
        }
    }
    let elapsed = started.elapsed();
    let err = ended.expect("the transaction outlived its 400 ms bound by 2 s");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    // the server ended the session (transaction_timeout terminates it)
    assert!(
        err.code().map(|c| c.code()) == Some("25P04") || conn.is_closed(),
        "{err}"
    );
}

#[tokio::test]
async fn dev_nodes_never_define_a_reference_and_a_live_one_blocks_readiness_by_name() {
    let h = setup().await;
    let name = h.db().name().to_owned();
    // a dev node that crashed a moment ago: its ready row is within the lease grace
    let dev = start(
        &h,
        Spec::app("dev-1").host("h2").dev().limits(7).interval(SLOW),
    )
    .await;
    let dev_id = dev.identity().id();
    drop(dev);
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '2 seconds', \
             expires_at = now() - interval '1 second' WHERE id = '{dev_id}'"
        ),
    )
    .await;
    let app = register(&h, Spec::app("app-1").limits(1).interval(SLOW)).await;
    assert_eq!(app.request_ready().await.unwrap(), Readiness::Ready);
    app.shutdown().await.unwrap();
    sql(&h, &format!("DELETE FROM erp_node WHERE id = '{dev_id}'")).await;
    // a live dev node (same digest): a non-dev newcomer stays unready and names it
    let dev = start(&h, Spec::app("dev-2").host("h2").dev().interval(SLOW)).await;
    let app = register(&h, Spec::app("app-2").interval(SLOW)).await;
    let expected = format!(
        "database {name}: dev node dev-2 (host h2, pid {}) is live in this database: a non-dev \
         node never serves next to an erp dev process; stop it first",
        std::process::id()
    );
    match app.request_ready().await.unwrap() {
        Readiness::Unready(reason) => assert_eq!(reason.to_string(), expected),
        other => panic!("expected unready, got {other:?}"),
    }
    assert!(!app.is_serving());
    assert_eq!(
        row(&h, &app).await.unwrap().unready_reason(),
        Some("dev_node_live")
    );
    // once it leaves, the newcomer becomes ready by itself
    dev.shutdown().await.unwrap();
    wait_ready(&app).await;
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_settle_delay_is_the_largest_interval_of_the_live_peers() {
    let h = setup().await;
    let admin = h.db().pool();
    let a = start(&h, Spec::app("app-a").interval(Duration::from_secs(2))).await;
    // 250 ms: another `node` section, so another digest; a window lets both be ready
    let b_identity = identity(&h, Spec::app("app-b")).await;
    Rollout::open(
        admin,
        b_identity.digest(),
        None,
        Duration::from_secs(600),
        Some("test"),
    )
    .await
    .unwrap();
    let b = NodeMembership::register(b_identity, pool(&h).await)
        .await
        .unwrap();
    match b.request_ready().await.unwrap() {
        Readiness::Settling { remaining, .. } => assert!(
            remaining > SLOW && remaining <= Duration::from_secs(2),
            "{remaining:?}"
        ),
        other => panic!("expected settling, got {other:?}"),
    }
    wait_ready(&b).await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

static COLLIDE: AddonDescriptor = AddonDescriptor {
    name: "test_node_collide",
    depends: &["base"],
    exact_only: false,
};

#[tokio::test]
async fn a_model_table_never_collides_with_a_core_table() {
    let h = setup().await;
    for table in ["erp_node", "erp_node_detail", "erp_node_rollout"] {
        let registry = Arc::new(
            RegistryBuilder::new([&base_users::ADDON, &COLLIDE])
                .with_decls([&*Box::leak(Box::new(
                    ModelDecl::new("test_node_collide", "x.node")
                        .table(table)
                        .fields(const { &[FieldDecl::char("name")] }),
                ))])
                .build()
                .unwrap(),
        );
        let Err(err) = erp_sdk::Orm::builder(registry, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
        else {
            panic!("an ORM whose model uses the core table {table} was built");
        };
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "model x.node uses table {table}, a core table of erp-orm (the node registry): \
                 give the model another table"
            )
        );
    }
}

#[tokio::test]
async fn the_schema_sync_takes_no_lock_on_existing_node_tables() {
    let h = setup().await;
    let client = h.db().client().await.unwrap();
    client.batch_execute("BEGIN").await.unwrap();
    client.batch_execute(NODE_DDL).await.unwrap();
    let locks: Vec<String> = client
        .query(
            "SELECT c.relname::text || ' ' || l.mode FROM pg_locks l JOIN pg_class c ON c.oid = \
             l.relation WHERE l.pid = pg_backend_pid() AND c.relname IN ('erp_node', \
             'erp_node_detail', 'erp_node_rollout', 'erp_node_feature', \
             'erp_node_feature_digest') ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(locks.is_empty(), "{locks:?}");
    client.batch_execute("ROLLBACK").await.unwrap();
}

#[tokio::test]
async fn a_registration_retry_replaces_its_expired_row_and_adopts_its_committed_one() {
    let h = setup().await;
    // a crashed membership of this identity, its lease over
    let identity_a = identity(&h, Spec::app("app-a")).await;
    let first = NodeMembership::register(Arc::clone(&identity_a), pool(&h).await)
        .await
        .unwrap();
    let old = row(&h, &first).await.unwrap();
    drop(first);
    let id = identity_a.id();
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '3 seconds', \
             expires_at = now() - interval '2 seconds' WHERE id = '{id}'"
        ),
    )
    .await;
    let again = NodeMembership::register(Arc::clone(&identity_a), pool(&h).await)
        .await
        .unwrap();
    let new = row(&h, &again).await.unwrap();
    assert!(new.registered_at() > old.registered_at());
    assert_eq!((new.state(), new.is_fresh()), (NodeState::Starting, true));
    assert_eq!(detail_count(&h, &again).await, 3);
    again.shutdown().await.unwrap();
    // a registration that committed but whose answer was lost: the retry adopts its row
    let identity_b = identity(&h, Spec::app("app-b")).await;
    let err = NodeMembership::register_with_fault_for_tests(
        Arc::clone(&identity_b),
        pool(&h).await,
        RegisterFault::LoseCommitReply,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, NodeError::Timeout { .. }), "{err}");
    let written = rows(&h)
        .await
        .into_iter()
        .find(|r| r.id() == identity_b.id())
        .unwrap();
    assert_eq!((written.state(), written.beat()), (NodeState::Starting, 1));
    let adopted = NodeMembership::register(Arc::clone(&identity_b), pool(&h).await)
        .await
        .unwrap();
    let r = row(&h, &adopted).await.unwrap();
    assert_eq!(r.registered_at(), written.registered_at());
    assert_eq!(
        rows(&h)
            .await
            .iter()
            .filter(|r| r.id() == identity_b.id())
            .count(),
        1
    );
    assert_eq!(detail_count(&h, &adopted).await, 3);
    assert_eq!(adopted.request_ready().await.unwrap(), Readiness::Ready);
    // once adopted, the identity is refused again: one membership per process and database
    let refused = NodeMembership::register(Arc::clone(&identity_b), pool(&h).await)
        .await
        .unwrap_err();
    assert!(matches!(refused, NodeError::AlreadyRegistered { .. }));
    adopted.shutdown().await.unwrap();
}

#[tokio::test]
async fn housekeeping_never_fences_a_healthy_node() {
    let h = setup().await;
    // lease 3 intervals, the default (one interval of slack after the next renewal is due);
    // retention 5 s: every renewal is due to purge (at most once per interval)
    let spec = Spec::app("app-a")
        .interval(Duration::from_millis(500))
        .lease(3)
        .retention(Duration::from_secs(5));
    let a = start(&h, spec).await;
    let dead = register(&h, Spec::app("app-dead")).await;
    let dead_id = dead.identity().id();
    drop(dead);
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '2 hours', \
             expires_at = now() - interval '2 hours' WHERE id = '{dead_id}'"
        ),
    )
    .await;
    // the purge's cascade waits on these row locks, at every turn, until its lock timeout
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute(&format!(
            "BEGIN; SELECT * FROM erp_node_detail WHERE node_id = '{dead_id}' FOR UPDATE"
        ))
        .await
        .unwrap();
    wait_for("a purge to wait on the locked detail rows", || async {
        count(
            &h,
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND \
             wait_event_type = 'Lock' AND query LIKE '%purged AS (%'",
        )
        .await
            >= 1
    })
    .await;
    let beat = a.view().beat();
    // admission is read on its own thread, without pause, so a gap of a millisecond shows
    let admission = a.admission();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let (mut checks, mut refused) = (0u64, 0u64);
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                checks += 1;
                if !admission.is_serving() {
                    refused += 1;
                }
                std::hint::spin_loop();
            }
            (checks, refused)
        })
    };
    tokio::time::sleep(a.identity().options().lease() * 3).await;
    stop.store(true, std::sync::atomic::Ordering::Release);
    let (checks, refused) = watcher.join().unwrap();
    assert_eq!(
        refused,
        0,
        "a blocked purge stopped admission in {refused} of {checks} checks: {:?}",
        a.readiness()
    );
    assert_eq!(a.readiness(), Readiness::Ready);
    assert!(
        a.view().beat() >= beat + 4,
        "{} -> {}",
        beat,
        a.view().beat()
    );
    // the blocked purge is reported, not hidden: it gave up on the lock (55P03, lock_not_available)
    // and says how long it has been failing
    let purge = a.view().last_purge_error().cloned();
    let purge = purge.expect("the blocked purge is reported in the view");
    assert_eq!(purge.sqlstate(), Some("55P03"), "{purge}");
    let first = purge.failing_since();
    wait_until("another failed purge", || {
        a.view()
            .last_purge_error()
            .is_some_and(|e| e.failures() > purge.failures())
    })
    .await;
    let again = a.view().last_purge_error().cloned().unwrap();
    assert_eq!(
        again.failing_since(),
        first,
        "the first failure is kept: {again:?}"
    );
    locker.batch_execute("COMMIT").await.unwrap();
    // the purge runs again later and removes the row
    wait_for("the purge", || async {
        rows(&h).await.iter().all(|r| r.id() != dead_id)
    })
    .await;
    wait_until("the purge error to clear", || {
        a.view().last_purge_error().is_none()
    })
    .await;
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_heartbeat_task_that_ends_stops_admission_at_once_and_says_so() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a").interval(SLOW)).await;
    a.panic_heartbeat_for_tests().await.unwrap();
    let asked = Instant::now();
    wait_until("admission to stop", || !a.is_serving()).await;
    // at once, not at the self-fence deadline (up to 3 s after the last renewal)
    assert!(asked.elapsed() < SLOW, "{:?}", asked.elapsed());
    match a.readiness() {
        Readiness::Unready(reason) => assert_eq!(
            reason.to_string(),
            "the heartbeat task panicked; new work is refused: restart the process"
        ),
        other => panic!("expected unready, got {other:?}"),
    }
    let err = a.request_ready().await.unwrap_err();
    assert!(matches!(err, NodeError::MembershipEnded { .. }), "{err}");
    drop(a);
}

#[tokio::test]
async fn an_aborted_membership_never_returns_a_connection_mid_transaction() {
    let h = setup().await;
    let p = pool(&h).await;
    let m = NodeMembership::register(
        identity(&h, Spec::app("app-a").interval(Duration::from_secs(60))).await,
        p.clone(),
    )
    .await
    .unwrap();
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute("BEGIN; LOCK TABLE erp_node IN EXCLUSIVE MODE")
        .await
        .unwrap();
    let renewing = tokio::spawn(async move { m.renew_now_for_tests().await });
    wait_for("the renewal to wait on the lock", || async {
        count(
            &h,
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND \
             wait_event_type = 'Lock' AND query LIKE 'WITH me AS%'",
        )
        .await
            == 1
    })
    .await;
    // dropping the membership mid-flight aborts its heartbeat task
    renewing.abort();
    assert!(renewing.await.unwrap_err().is_cancelled());
    wait_until("the connection to be discarded", || p.status().size == 0).await;
    locker.batch_execute("COMMIT").await.unwrap();
}

#[tokio::test]
async fn the_role_grants_refuse_reserved_missing_member_and_superuser_roles() {
    let h = setup().await;
    let client = h.db().client().await.unwrap();
    let owner: String = client
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    let server = |what: &str| {
        format!(
            "it can {what} as the database server's operating system user, which reaches every \
             table"
        )
    };
    let name = |prefix: &str| -> String {
        format!("{prefix}_{}", h.db().name())
            .chars()
            .take(63)
            .collect()
    };
    let member = name("m");
    // PostgreSQL 16+: a member that inherits nothing can still `SET ROLE` to the owner
    let set_only = name("so");
    // ADMIN OPTION only: it can grant the owner to itself
    let admin = name("a");
    // ADMIN OPTION on a role that is a member of the owner: it can join that role
    let via_member = name("vm");
    let member_of_owner = name("zm");
    // ADMIN OPTION on a role that holds the ADMIN OPTION on the owner
    let via_admin = name("va");
    let admin_of_owner = name("za");
    let create_role = name("cr");
    let replication = name("rp");
    // for each predefined role that reaches every table: a role it can join (ADMIN OPTION only)
    // that is a member of it
    let data_roles = [
        ("pg_write_all_data", "it can write every table".to_owned()),
        ("pg_read_all_data", "it can read every table".to_owned()),
        ("pg_execute_server_program", server("run programs")),
        ("pg_write_server_files", server("write files")),
        ("pg_read_server_files", server("read files")),
    ];
    let via_data: Vec<(String, String)> = (0..data_roles.len())
        .map(|i| (name(&format!("vd{i}")), name(&format!("zd{i}"))))
        .collect();
    // a role it can join (ADMIN OPTION only) that has REPLICATION, and one with CREATEROLE
    let via_attribute = name("vr");
    let attribute_holder = name("zr");
    let via_create = name("vc");
    let create_holder = name("zc");
    // a superuser role it is a member of with every option off (a membership row all the same),
    // and one it holds the ADMIN OPTION on: a superuser is a member of every role, so the
    // ADMIN OPTION on it leads to the owner and is refused as such first
    let via_super = name("vs");
    let via_super_admin = name("vsa");
    let super_holder = name("zs");
    // the path to a refused role (review round 5, finding 1): two routes to pg_read_all_data, a
    // chain of three roles named first and a direct one named last but created and granted first
    // (lowest oid, first in the catalog's order), so the path is chosen by name, not by scan order
    let via_routes = name("vh");
    let route_late = name("hz");
    let route_chain = [name("ha"), name("hb"), name("hc")];
    // the predefined role granted directly next to a route through a role named before it: the
    // direct grant is the path
    let via_direct = name("vb");
    let route_named_first = name("aa");
    // a superuser granted directly (named before the real route) next to a CREATEROLE role
    // reached through another role: the path to the CREATEROLE role does not name the superuser,
    // which `pg_has_role` would (a superuser is a member of every role)
    let via_beside_super = name("vx");
    let super_beside = name("sa");
    let route_to_create = name("sc");
    let create_beyond = name("cb");
    let writer = name("w");
    let reader = name("r");
    let program = name("xp");
    let file_writer = name("fw");
    let file_reader = name("fr");
    let superuser = name("s");
    let mut data_setup = String::new();
    for ((role, _), (via, holder)) in data_roles.iter().zip(&via_data) {
        data_setup.push_str(&format!(
            "CREATE ROLE \"{holder}\" NOLOGIN IN ROLE {role}; CREATE ROLE \"{via}\" NOLOGIN; \
             GRANT \"{holder}\" TO \"{via}\" WITH ADMIN TRUE, INHERIT FALSE, SET FALSE; "
        ));
    }
    sql(&h, &data_setup).await;
    let [chain_a, chain_b, chain_c] = &route_chain;
    sql(
        &h,
        &format!(
            "CREATE ROLE \"{route_late}\" NOLOGIN IN ROLE pg_read_all_data; \
             CREATE ROLE \"{via_routes}\" NOLOGIN IN ROLE \"{route_late}\"; \
             CREATE ROLE \"{chain_c}\" NOLOGIN IN ROLE pg_read_all_data; \
             CREATE ROLE \"{chain_b}\" NOLOGIN IN ROLE \"{chain_c}\"; \
             CREATE ROLE \"{chain_a}\" NOLOGIN IN ROLE \"{chain_b}\"; \
             GRANT \"{chain_a}\" TO \"{via_routes}\"; \
             CREATE ROLE \"{route_named_first}\" NOLOGIN IN ROLE pg_read_all_data; \
             CREATE ROLE \"{via_direct}\" NOLOGIN IN ROLE pg_read_all_data, \"{route_named_first}\"; \
             CREATE ROLE \"{super_beside}\" NOLOGIN SUPERUSER; \
             CREATE ROLE \"{create_beyond}\" NOLOGIN CREATEROLE; \
             CREATE ROLE \"{route_to_create}\" NOLOGIN IN ROLE \"{create_beyond}\"; \
             CREATE ROLE \"{via_beside_super}\" NOLOGIN IN ROLE \"{route_to_create}\"; \
             GRANT \"{super_beside}\" TO \"{via_beside_super}\" WITH ADMIN FALSE, INHERIT FALSE, \
               SET FALSE"
        ),
    )
    .await;
    sql(
        &h,
        &format!(
            "CREATE ROLE \"{member}\" NOLOGIN IN ROLE \"{owner}\"; \
             CREATE ROLE \"{set_only}\" NOLOGIN; \
             GRANT \"{owner}\" TO \"{set_only}\" WITH INHERIT FALSE, SET TRUE; \
             CREATE ROLE \"{admin}\" NOLOGIN; \
             GRANT \"{owner}\" TO \"{admin}\" WITH ADMIN TRUE, INHERIT FALSE, SET FALSE; \
             CREATE ROLE \"{member_of_owner}\" NOLOGIN IN ROLE \"{owner}\"; \
             CREATE ROLE \"{via_member}\" NOLOGIN; \
             GRANT \"{member_of_owner}\" TO \"{via_member}\" WITH ADMIN TRUE, INHERIT FALSE, SET FALSE; \
             CREATE ROLE \"{admin_of_owner}\" NOLOGIN; \
             GRANT \"{owner}\" TO \"{admin_of_owner}\" WITH ADMIN TRUE, INHERIT FALSE, SET FALSE; \
             CREATE ROLE \"{via_admin}\" NOLOGIN; \
             GRANT \"{admin_of_owner}\" TO \"{via_admin}\" WITH ADMIN TRUE, INHERIT FALSE, SET FALSE; \
             CREATE ROLE \"{create_role}\" NOLOGIN CREATEROLE; \
             CREATE ROLE \"{replication}\" NOLOGIN REPLICATION; \
             CREATE ROLE \"{create_holder}\" NOLOGIN CREATEROLE; \
             CREATE ROLE \"{via_create}\" NOLOGIN; \
             GRANT \"{create_holder}\" TO \"{via_create}\" WITH ADMIN TRUE, INHERIT FALSE, \
               SET FALSE; \
             CREATE ROLE \"{super_holder}\" NOLOGIN SUPERUSER; \
             CREATE ROLE \"{via_super}\" NOLOGIN; \
             GRANT \"{super_holder}\" TO \"{via_super}\" WITH ADMIN FALSE, INHERIT FALSE, \
               SET FALSE; \
             CREATE ROLE \"{via_super_admin}\" NOLOGIN; \
             GRANT \"{super_holder}\" TO \"{via_super_admin}\" WITH ADMIN TRUE, INHERIT FALSE, \
               SET FALSE; \
             CREATE ROLE \"{attribute_holder}\" NOLOGIN REPLICATION; \
             CREATE ROLE \"{via_attribute}\" NOLOGIN; \
             GRANT \"{attribute_holder}\" TO \"{via_attribute}\" WITH ADMIN TRUE, INHERIT FALSE, \
               SET FALSE; \
             CREATE ROLE \"{writer}\" NOLOGIN IN ROLE pg_write_all_data; \
             CREATE ROLE \"{reader}\" NOLOGIN IN ROLE pg_read_all_data; \
             CREATE ROLE \"{program}\" NOLOGIN IN ROLE pg_execute_server_program; \
             CREATE ROLE \"{file_writer}\" NOLOGIN IN ROLE pg_write_server_files; \
             CREATE ROLE \"{file_reader}\" NOLOGIN IN ROLE pg_read_server_files; \
             CREATE ROLE \"{superuser}\" NOLOGIN SUPERUSER"
        ),
    )
    .await;
    let reserved = |role: &str| {
        format!(
            "role {role:?} is reserved: PUBLIC, NONE and the predefined pg_* roles are refused \
             (a grant to them would reach every role or a whole class of roles)"
        )
    };
    let owner_member = |role: &str| {
        format!(
            "role {role:?} is a member of {owner:?}, which owns erp_node: it can use or take on \
             the owner's rights whatever this grants, so the heartbeat role must not be a member \
             of the owner"
        )
    };
    let predefined = |role: &str, predefined: &str, reach: &str| {
        format!(
            "role {role:?} is a member of {predefined:?}: {reach} whatever this grants, so the \
             heartbeat role must not be a member of it"
        )
    };
    let admin_option = |role: &str| {
        format!(
            "role {role:?} holds the ADMIN OPTION on {owner:?}, which owns erp_node, or on a role \
             that leads to it: it can make itself a member of the owner whatever this grants, so \
             the heartbeat role must not hold it"
        )
    };
    let mut cases = vec![
        (set_only.clone(), owner_member(&set_only)),
        (admin.clone(), admin_option(&admin)),
        (via_member.clone(), admin_option(&via_member)),
        (via_admin.clone(), admin_option(&via_admin)),
        (
            create_role.clone(),
            format!(
                "role {create_role:?} has CREATEROLE: it can give itself the membership of other \
                 roles whatever this grants, so the heartbeat role must not have it"
            ),
        ),
        (
            replication.clone(),
            format!(
                "role {replication:?} has REPLICATION: it can copy every table through a base \
                 backup whatever this grants, so the heartbeat role must not have it"
            ),
        ),
        (
            via_attribute.clone(),
            format!(
                "role {via_attribute:?} is a member of {attribute_holder:?}, a role with \
                 REPLICATION: it can copy every table through a base backup whatever this grants, \
                 so the heartbeat role must not reach it"
            ),
        ),
        (
            via_create.clone(),
            format!(
                "role {via_create:?} is a member of {create_holder:?}, a role with CREATEROLE: it \
                 can give itself the membership of other roles whatever this grants, so the \
                 heartbeat role must not reach it"
            ),
        ),
        (
            via_super.clone(),
            format!(
                "role {via_super:?} is a member of {super_holder:?}, a superuser role: it can act \
                 as a superuser whatever this grants, so the heartbeat role must not reach it"
            ),
        ),
        (via_super_admin.clone(), admin_option(&via_super_admin)),
        (
            writer.clone(),
            predefined(&writer, "pg_write_all_data", "it can write every table"),
        ),
        (
            reader.clone(),
            predefined(&reader, "pg_read_all_data", "it can read every table"),
        ),
        (
            program.clone(),
            predefined(
                &program,
                "pg_execute_server_program",
                &server("run programs"),
            ),
        ),
        (
            file_writer.clone(),
            predefined(
                &file_writer,
                "pg_write_server_files",
                &server("write files"),
            ),
        ),
        (
            file_reader.clone(),
            predefined(&file_reader, "pg_read_server_files", &server("read files")),
        ),
        ("public".to_owned(), reserved("public")),
        ("none".to_owned(), reserved("none")),
        ("pg_monitor".to_owned(), reserved("pg_monitor")),
        (
            "no_such_role_for_this_test".to_owned(),
            "role \"no_such_role_for_this_test\" does not exist".to_owned(),
        ),
        (member.clone(), owner_member(&member)),
        (
            superuser.clone(),
            format!(
                "role {superuser:?} is a superuser: privileges do not restrict it, so the \
                 heartbeat role must not be one"
            ),
        ),
    ];
    cases.push((
        via_routes.clone(),
        format!(
            "role {via_routes:?} is a member of {chain_a:?}, which leads to \"pg_read_all_data\": \
             it can read every table whatever this grants, so the heartbeat role must not reach it"
        ),
    ));
    cases.push((
        via_direct.clone(),
        predefined(&via_direct, "pg_read_all_data", "it can read every table"),
    ));
    cases.push((
        via_beside_super.clone(),
        format!(
            "role {via_beside_super:?} is a member of {route_to_create:?}, which leads to \
             {create_beyond:?}, a role with CREATEROLE: it can give itself the membership of other \
             roles whatever this grants, so the heartbeat role must not reach it"
        ),
    ));
    // each predefined role, named with the role on the path
    for ((role, reach), (via, holder)) in data_roles.iter().zip(&via_data) {
        cases.push((
            via.clone(),
            format!(
                "role {via:?} is a member of {holder:?}, which leads to {role:?}: {reach} \
                 whatever this grants, so the heartbeat role must not reach it"
            ),
        ));
    }
    let mut tx = h.begin().await.unwrap();
    for (role, message) in &cases {
        let err = tx.grant_node_role(role).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{role}");
        assert_eq!(err.message(), message.as_str());
    }
    tx.rollback().await.unwrap();
    let mut created: Vec<String> = [
        &member,
        &set_only,
        &admin,
        &via_member,
        &member_of_owner,
        &via_admin,
        &admin_of_owner,
        &create_role,
        &replication,
        &via_attribute,
        &attribute_holder,
        &via_create,
        &create_holder,
        &via_super,
        &via_super_admin,
        &super_holder,
        &writer,
        &reader,
        &program,
        &file_writer,
        &file_reader,
        &superuser,
    ]
    .into_iter()
    .cloned()
    .collect();
    for (via, holder) in &via_data {
        created.push(via.clone());
        created.push(holder.clone());
    }
    created.extend([
        via_routes.clone(),
        route_late.clone(),
        via_direct.clone(),
        route_named_first.clone(),
        via_beside_super.clone(),
        super_beside.clone(),
        route_to_create.clone(),
        create_beyond.clone(),
    ]);
    created.extend(route_chain.iter().cloned());
    let listed = created
        .iter()
        .map(|r| format!("'{r}'"))
        .collect::<Vec<_>>()
        .join(", ");
    // nothing reached PUBLIC or any of these roles
    let granted = count(
        &h,
        &format!(
            "SELECT count(*) FROM pg_class c CROSS JOIN LATERAL aclexplode(c.relacl) a WHERE \
             c.relname IN ('erp_node', 'erp_node_detail', 'erp_node_rollout') AND (a.grantee = 0 \
             OR a.grantee IN (SELECT oid FROM pg_roles WHERE rolname IN ({listed})))"
        ),
    )
    .await;
    assert_eq!(granted, 0);
    for role in &created {
        sql(&h, &format!("DROP ROLE \"{role}\"")).await;
    }
}

#[tokio::test]
async fn rollout_windows_past_the_retention_are_purged() {
    let h = setup().await;
    sql(
        &h,
        "INSERT INTO erp_node_rollout (from_digest, to_digest, opened_by, opened_at, until, \
         closed_at) VALUES \
         (decode(repeat('01', 32), 'hex'), decode(repeat('02', 32), 'hex'), 'old closed', \
          now() - interval '3 hours', now() - interval '2 hours', now() - interval '150 minutes'), \
         (decode(repeat('03', 32), 'hex'), decode(repeat('04', 32), 'hex'), 'recent closed', \
          now() - interval '10 minutes', now() + interval '10 minutes', now() - interval '1 second'), \
         (decode(repeat('05', 32), 'hex'), decode(repeat('06', 32), 'hex'), 'old ended', \
          now() - interval '3 hours', now() - interval '2 hours', NULL)",
    )
    .await;
    // the first round after the registration purges what ended more than the retention (30 s
    // here) ago
    let a = register(&h, Spec::app("app-a")).await;
    let left = || async {
        h.db()
            .client()
            .await
            .unwrap()
            .query("SELECT opened_by FROM erp_node_rollout ORDER BY id", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect::<Vec<String>>()
    };
    wait_for("the first purge", || async { left().await.len() == 1 }).await;
    assert_eq!(left().await, ["recent closed"]);
    assert!(a.view().last_purge_error().is_none());
    a.shutdown().await.unwrap();
}

/// Not a spec case: taking a node id is bounded like every registry operation.
#[tokio::test]
async fn taking_a_node_id_is_bounded() {
    let h = setup().await;
    let one = erp_orm::pool::create_pool(h.db().config(), 1).unwrap();
    let held = one.get().await.unwrap();
    let builder = NodeIdentity::builder(NodeRole::App, "app-a", "host-1", build(), addons())
        .options(options(&Spec::app("app-a")));
    let err = tokio::time::timeout(Duration::from_secs(5), builder.create(&one))
        .await
        .expect("create() must be bounded")
        .unwrap_err();
    assert!(
        matches!(
            err,
            NodeError::Timeout {
                what: "taking a node id",
                ..
            }
        ),
        "{err}"
    );
    drop(held);
}

/// Not a spec case: a host name with a control character is refused (it is shown in messages).
#[tokio::test]
async fn a_host_name_with_a_control_character_is_refused() {
    let h = setup().await;
    for host in ["h\u{7}x", "h\nx", "h\u{9b}x"] {
        let err = NodeIdentity::builder(NodeRole::App, "app-a", host, build(), addons())
            .create(h.db().pool())
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::InvalidSetting { .. }));
        assert_eq!(
            err.to_string(),
            format!("invalid host {host:?}: 1 to 255 bytes without control characters")
        );
    }
}

/// Not a spec case: `Rollout::open` refuses a malformed window before it connects.
#[tokio::test]
async fn a_malformed_window_is_refused_before_connecting() {
    let h = setup().await;
    let mut config = h.db().config();
    // nothing listens on port 1
    config.port(1);
    let unreachable = erp_orm::pool::create_pool(config, 1).unwrap();
    let d = Digest::from_bytes([1; 32]);
    let err = Rollout::open(&unreachable, d, None, Duration::from_secs(59), Some("test"))
        .await
        .unwrap_err();
    assert!(matches!(err, NodeError::RolloutRefused { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        "rollout window refused: the window must last from 1 minute to 24 hours, got 59 s"
    );
    let err = Rollout::open(
        &unreachable,
        d,
        Some(d),
        Duration::from_secs(600),
        Some("test"),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "rollout window refused: the window must replace a version: from and to are both {d}"
        )
    );
}

#[tokio::test]
async fn request_tasks_share_an_admission_handle_and_the_owner_shuts_down() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a")).await;
    let id = a.identity().id();
    let admission = a.admission();
    // cheap to clone and `Send`: every request task holds one
    let shared = admission.clone();
    let checked = tokio::spawn(async move { (shared.is_serving(), shared.readiness()) });
    assert_eq!(checked.await.unwrap(), (true, Readiness::Ready));
    assert_eq!(admission.view().node(), id);
    assert_eq!(admission.database(), h.db().name());
    let mut changes = admission.subscribe();
    // the owner handle stays unique, so the `stopped` row is written
    a.shutdown().await.unwrap();
    assert!(!admission.is_serving());
    assert_eq!(admission.readiness(), Readiness::Stopped);
    assert_eq!(changes.borrow_and_update().readiness(), &Readiness::Stopped);
    let gone = rows(&h).await.into_iter().find(|r| r.id() == id).unwrap();
    assert_eq!(gone.state(), NodeState::Stopped);
}

#[tokio::test]
async fn a_failed_transition_is_reported_in_the_view_until_one_succeeds() {
    let h = setup().await;
    let a = register(&h, Spec::app("app-a")).await;
    // another session holds the readiness lock: the transition's lock wait times out
    let (class, key) = erp_orm::node::READINESS_LOCK;
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute(&format!("SELECT pg_advisory_lock({class}, {key})"))
        .await
        .unwrap();
    let err = a.request_ready().await.unwrap_err();
    assert_eq!(err.sqlstate(), Some("57014"), "{err}");
    let reported = a.view().last_transition_error().cloned().unwrap();
    assert_eq!(reported.sqlstate(), Some("57014"));
    assert_eq!(reported.message(), err.to_string());
    // the heartbeat retries after every renewal; the view keeps the reason meanwhile
    let view = a.renew_now_for_tests().await.unwrap();
    assert_eq!(
        view.last_transition_error().and_then(|e| e.sqlstate()),
        Some("57014")
    );
    assert_eq!(a.readiness(), Readiness::Starting);
    locker
        .batch_execute(&format!("SELECT pg_advisory_unlock({class}, {key})"))
        .await
        .unwrap();
    wait_ready(&a).await;
    assert!(a.view().last_transition_error().is_none());
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_window_records_the_session_role_that_opened_it() {
    let h = setup().await;
    let admin = h.db().pool();
    let role: String = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one("SELECT session_user::text", &[])
        .await
        .unwrap()
        .get(0);
    let (d1, d2) = (Digest::from_bytes([1; 32]), Digest::from_bytes([2; 32]));
    // no label: the server records the session role
    let w = Rollout::open(admin, d2, Some(d1), Duration::from_secs(600), None)
        .await
        .unwrap();
    assert_eq!(
        (w.opened_by(), w.opened_role()),
        (role.as_str(), role.as_str())
    );
    // a label from the client is kept next to the role the server saw
    let w = Rollout::open(
        admin,
        d1,
        Some(d2),
        Duration::from_secs(600),
        Some("erp cluster (ops)"),
    )
    .await
    .unwrap();
    assert_eq!(
        (w.opened_by(), w.opened_role()),
        ("erp cluster (ops)", role.as_str())
    );
    Rollout::close(admin).await.unwrap();
}

#[tokio::test]
async fn a_lost_asynchronous_renewal_never_extends_admission() {
    let h = setup().await;
    // a 1 s interval and a lease of four: the fence is three intervals after a renewal's send
    // when the write before it was sent one interval earlier
    let a = register(&h, Spec::app("app-a").interval(SLOW)).await;
    let options = *a.identity().options();
    a.renew_now_for_tests().await.unwrap();
    let (first_sent, _) = fence_times(&a);
    // the next renewal is sent late: the task is held for one and a half intervals, and the
    // renewal waits in its queue (no scheduled round can run first)
    a.pause_heartbeat_for_tests(true);
    tokio::time::sleep_until((first_sent + SLOW + SLOW / 2).into()).await;
    let (renewed, ()) = tokio::join!(a.renew_now_for_tests(), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        a.pause_heartbeat_for_tests(false);
    });
    renewed.unwrap();
    let (late_sent, deadline) = fence_times(&a);
    assert!(
        late_sent >= first_sent + SLOW + SLOW / 2,
        "the renewal was not late: {:?} after the first",
        late_sent - first_sent
    );
    // if that asynchronous renewal is lost, peers see the first one's expiry: the deadline is
    // never past it, and earlier than the late renewal's own fence
    let tolerance = Duration::from_millis(5);
    assert!(
        deadline <= first_sent + options.lease() + tolerance,
        "deadline {:?} after the first renewal's send, its expiry is {:?}",
        deadline - first_sent,
        options.lease()
    );
    assert!(deadline + SLOW / 4 < late_sent + options.fence_after());
    // a synchronous commit (the readiness transition) makes the late renewal durable: the
    // deadline moves to its own fence
    assert_eq!(a.request_ready().await.unwrap(), Readiness::Ready);
    let (sent, deadline) = fence_times(&a);
    assert!(sent.max(late_sent) - sent.min(late_sent) <= tolerance);
    let fence = late_sent + options.fence_after();
    assert!(
        deadline.max(fence) - deadline.min(fence) <= tolerance,
        "deadline {:?} after the late renewal's send, its fence is {:?}",
        deadline - late_sent,
        options.fence_after()
    );
    a.shutdown().await.unwrap();
}

/// A warm one-slot heartbeat pool whose connections run as `role` (`SET ROLE` when each opens).
async fn pool_as(h: &Harness, role: &str) -> Pool {
    let set_role = format!("SET ROLE \"{role}\"");
    let config = erp_orm::node::heartbeat_config(h.db().config(), &NodeOptions::default());
    let pool = erp_orm::pool::pool_builder(config)
        .max_size(1)
        .runtime(Runtime::Tokio1)
        .post_create(Hook::async_fn(move |client, _| {
            let set_role = set_role.clone();
            Box::pin(async move {
                client
                    .batch_execute(&set_role)
                    .await
                    .map_err(HookError::Backend)
            })
        }))
        .build()
        .unwrap();
    let conn = pool.get().await.unwrap();
    let current: String = conn
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(current, role);
    drop(conn);
    pool
}

/// A rollout window that ended two hours ago.
const OLD_WINDOW: &str = "INSERT INTO erp_node_rollout (from_digest, to_digest, opened_by, \
     opened_at, until, closed_at) VALUES (decode(repeat('01', 32), 'hex'), \
     decode(repeat('02', 32), 'hex'), 'old', now() - interval '3 hours', \
     now() - interval '2 hours', now() - interval '2 hours')";

#[tokio::test]
async fn the_heartbeat_role_runs_a_whole_membership_with_its_grant() {
    let h = setup().await;
    let role: String = format!("hb_{}", h.db().name()).chars().take(63).collect();
    sql(&h, &format!("CREATE ROLE \"{role}\" NOLOGIN")).await;
    let mut tx = h.begin().await.unwrap();
    tx.grant_node_role(&role).await.unwrap();
    tx.commit().await.unwrap();
    sql(&h, OLD_WINDOW).await;
    // retention 1 s: every renewal is due to purge (at most once per interval)
    let spec = Spec::app("app-hb").retention(Duration::from_secs(1));
    let identity = identity(&h, spec).await;
    let m = NodeMembership::register(identity, pool_as(&h, &role).await)
        .await
        .unwrap();
    let windows = "SELECT count(*) FROM erp_node_rollout";
    // the first purge after the registration, as the heartbeat role
    wait_for("the first purge", || async {
        count(&h, windows).await == 0
    })
    .await;
    // and a later one, for a window written later
    sql(&h, OLD_WINDOW).await;
    wait_for("a later purge", || async { count(&h, windows).await == 0 }).await;
    assert!(
        m.view().last_error().is_none(),
        "{:?}",
        m.view().last_error()
    );
    assert!(
        m.view().last_purge_error().is_none(),
        "{:?}",
        m.view().last_purge_error()
    );
    assert_eq!(m.request_ready().await.unwrap(), Readiness::Ready);
    m.drain().await.unwrap();
    let id = m.identity().id();
    m.shutdown().await.unwrap();
    let gone = rows(&h).await.into_iter().find(|r| r.id() == id).unwrap();
    assert_eq!(gone.state(), NodeState::Stopped);
    sql(
        &h,
        &format!(
            "REVOKE ALL ON erp_node, erp_node_detail, erp_node_rollout FROM \"{role}\"; \
             DROP ROLE \"{role}\""
        ),
    )
    .await;
}

#[tokio::test]
async fn registry_commits_are_synchronous_whatever_the_database_default() {
    let h = setup().await;
    let db = h.db().name();
    // every write to a registry table records the commit setting of its transaction
    sql(
        &h,
        &format!(
            "ALTER DATABASE \"{db}\" SET synchronous_commit = off; \
             CREATE TABLE commit_log (n bigserial PRIMARY KEY, tbl text, state text, setting text); \
             CREATE FUNCTION log_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
               INSERT INTO commit_log (tbl, state, setting) \
               VALUES (TG_TABLE_NAME, coalesce(to_jsonb(NEW) ->> 'state', '-'), \
                       current_setting('synchronous_commit')); RETURN NEW; END $$; \
             CREATE TRIGGER log_commit AFTER INSERT OR UPDATE ON erp_node FOR EACH ROW \
               EXECUTE FUNCTION log_commit(); \
             CREATE TRIGGER log_commit AFTER INSERT OR UPDATE ON erp_node_detail FOR EACH ROW \
               EXECUTE FUNCTION log_commit(); \
             CREATE TRIGGER log_commit AFTER INSERT OR UPDATE ON erp_node_rollout FOR EACH ROW \
               EXECUTE FUNCTION log_commit()"
        ),
    )
    .await;
    // the membership's connection and the operator's open after the change: they start with `off`
    let a = register(&h, Spec::app("app-a").interval(Duration::from_secs(60))).await;
    let operator = pool(&h).await;
    a.renew_now_for_tests().await.unwrap();
    assert_eq!(a.request_ready().await.unwrap(), Readiness::Ready);
    // a lost row: the renewal re-inserts it, then the details are written in a transaction of
    // their own
    let id = a.identity().id();
    sql(&h, &format!("DELETE FROM erp_node WHERE id = '{id}'")).await;
    a.renew_now_for_tests().await.unwrap();
    let details = detail_count(&h, &a).await;
    assert!(details > 0);
    // an operator opens and closes a rollout window
    let to = identity(&h, Spec::app("app-b").limits(2)).await.digest();
    Rollout::open(&operator, to, None, Duration::from_secs(1800), Some("test"))
        .await
        .unwrap();
    Rollout::close(&operator).await.unwrap().unwrap();
    a.drain().await.unwrap();
    a.shutdown().await.unwrap();
    let log: Vec<(String, String, String)> = h
        .db()
        .client()
        .await
        .unwrap()
        .query("SELECT tbl, state, setting FROM commit_log ORDER BY n", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    let entry = |tbl: &str, state: &str, setting: &str| {
        (tbl.to_owned(), state.to_owned(), setting.to_owned())
    };
    // registration and its details, synchronous
    let mut expected = vec![entry("erp_node", "starting", "on")];
    expected.extend((0..details).map(|_| entry("erp_node_detail", "-", "on")));
    expected.extend([
        // a renewal, asynchronous
        entry("erp_node", "starting", "off"),
        // the readiness transition: synchronous
        entry("erp_node", "ready", "on"),
        // the renewal that re-inserts the lost row: asynchronous
        entry("erp_node", "ready", "off"),
    ]);
    // its details, written again: synchronous
    expected.extend((0..details).map(|_| entry("erp_node_detail", "-", "on")));
    expected.extend([
        // the window's open and close: synchronous
        entry("erp_node_rollout", "-", "on"),
        entry("erp_node_rollout", "-", "on"),
        // the drain, the shutdown: synchronous
        entry("erp_node", "stopping", "on"),
        entry("erp_node", "stopped", "on"),
    ]);
    assert_eq!(log, expected);
}

/// A count of sessions waiting for the readiness lock.
fn readiness_lock_waiters() -> String {
    let (class, key) = erp_orm::node::READINESS_LOCK;
    format!(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND classid = {class} \
         AND objid = {key} AND objsubid = 2 AND NOT granted AND database = (SELECT oid FROM \
         pg_database WHERE datname = current_database())"
    )
}

#[tokio::test]
async fn a_transition_in_flight_never_undoes_a_drain() {
    let h = setup().await;
    let (class, key) = erp_orm::node::READINESS_LOCK;
    let waiters = readiness_lock_waiters();
    let lock = format!("SELECT pg_advisory_lock({class}, {key})");
    let unlock = format!("SELECT pg_advisory_unlock({class}, {key})");
    let locker = h.db().client().await.unwrap();
    // a drain asked while a readiness request waits for the lock
    let a = register(&h, Spec::app("app-a").interval(Duration::from_secs(60))).await;
    let mut changes = a.subscribe();
    locker.batch_execute(&lock).await.unwrap();
    let driver = async {
        wait_for("the transition to wait for the readiness lock", || async {
            count(&h, &waiters).await == 1
        })
        .await;
        // the task stops before its next command, as a busy runtime would: the drain waits
        // behind the transition
        a.pause_heartbeat_for_tests(true);
        // the drain asked to leave before this point (`join!` polls it first)
        changes.borrow_and_update();
        locker.batch_execute(&unlock).await.unwrap();
        wait_until("the transition to publish its decision", || {
            changes.has_changed().unwrap()
        })
        .await;
        let seen = (
            a.is_serving(),
            a.readiness(),
            a.view().readiness().clone(),
            changes.borrow_and_update().readiness().clone(),
        );
        a.pause_heartbeat_for_tests(false);
        seen
    };
    let (ready, drained, (serving, readiness, viewed, subscribed)) =
        tokio::join!(a.request_ready(), a.drain(), driver);
    assert_eq!(ready.unwrap(), Readiness::Ready);
    drained.unwrap();
    assert!(
        !serving,
        "the transition turned admission back on during the drain"
    );
    // every reader sees the drain: the readiness, the view and its subscribers
    let draining = Readiness::Unready(UnreadyReason::Draining);
    assert_eq!(readiness, draining);
    assert_eq!(
        viewed, draining,
        "the view still says the transition's decision"
    );
    assert_eq!(
        subscribed, draining,
        "a subscriber still sees the transition's decision"
    );
    assert!(!a.is_serving());
    assert_eq!(a.readiness(), Readiness::Unready(UnreadyReason::Draining));
    // a later readiness request does not undo it either
    assert_eq!(
        a.request_ready().await.unwrap(),
        Readiness::Unready(UnreadyReason::Draining)
    );
    assert!(!a.is_serving());
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_transition_in_flight_never_undoes_a_shutdown() {
    let h = setup().await;
    let (class, key) = erp_orm::node::READINESS_LOCK;
    let waiters = readiness_lock_waiters();
    let lock = format!("SELECT pg_advisory_lock({class}, {key})");
    let unlock = format!("SELECT pg_advisory_unlock({class}, {key})");
    let locker = h.db().client().await.unwrap();
    // a shutdown asked while the heartbeat's own transition waits for the lock
    let b = register(&h, Spec::app("app-b").interval(SLOW * 2)).await;
    locker.batch_execute(&lock).await.unwrap();
    // the request times out on the lock; the heartbeat retries the transition after each renewal
    let err = b.request_ready().await.unwrap_err();
    assert_eq!(err.sqlstate(), Some("57014"), "{err}");
    wait_for(
        "the retried transition to wait for the readiness lock",
        || async { count(&h, &waiters).await == 1 },
    )
    .await;
    // the task stops before its next command: the shutdown waits behind the transition
    b.pause_heartbeat_for_tests(true);
    let admission = b.admission();
    let mut changes = admission.subscribe();
    let driver = async {
        // the shutdown asked to leave before this point (`join!` polls it first)
        changes.borrow_and_update();
        locker.batch_execute(&unlock).await.unwrap();
        wait_until("the transition to publish its decision", || {
            changes.has_changed().unwrap()
        })
        .await;
        let seen = (
            admission.is_serving(),
            admission.readiness(),
            admission.view().readiness().clone(),
            changes.borrow_and_update().readiness().clone(),
        );
        admission.pause_heartbeat_for_tests(false);
        seen
    };
    let (stopped, (serving, readiness, viewed, subscribed)) = tokio::join!(b.shutdown(), driver);
    stopped.unwrap();
    assert!(
        !serving,
        "the transition turned admission back on during the shutdown"
    );
    let draining = Readiness::Unready(UnreadyReason::Draining);
    assert_eq!(readiness, draining);
    assert_eq!(
        viewed, draining,
        "the view still says the transition's decision"
    );
    assert_eq!(
        subscribed, draining,
        "a subscriber still sees the transition's decision"
    );
    assert!(!admission.is_serving());
    assert_eq!(admission.readiness(), Readiness::Stopped);
}

#[tokio::test]
async fn a_node_past_its_deadline_never_counts_itself_in_any_view() {
    let h = setup().await;
    let a = start(&h, Spec::app("app-a").interval(SLOW)).await;
    let b = start(&h, Spec::app("app-b").interval(SLOW)).await;
    let view = b.renew_now_for_tests().await.unwrap();
    assert_eq!(view.divisor(NodeRole::App).get(), 2);
    let changes = b.subscribe();
    // B's task is starved: nothing is published after its deadline
    b.pause_heartbeat_for_tests(true);
    let (_, deadline) = fence_times(&b);
    tokio::time::sleep_until((deadline + Duration::from_millis(20)).into()).await;
    assert!(!b.is_serving());
    // every view, whenever it was taken, applies the deadline when it is read
    assert_eq!(view.divisor(NodeRole::App).get(), 1);
    assert_eq!(b.view().divisor(NodeRole::App).get(), 1);
    assert_eq!(b.admission().view().divisor(NodeRole::App).get(), 1);
    assert_eq!(changes.borrow().divisor(NodeRole::App).get(), 1);
    b.pause_heartbeat_for_tests(false);
    wait_ready(&b).await;
    assert_eq!(b.view().divisor(NodeRole::App).get(), 2);
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_membership_of_one_identity_in_one_process_is_refused() {
    let h = setup().await;
    let identity = identity(&h, Spec::app("app-a")).await;
    let first = NodeMembership::register(Arc::clone(&identity), pool(&h).await)
        .await
        .unwrap();
    let before = row(&h, &first).await.unwrap();
    // the first membership's task is starved and its lease runs out on the database clock
    first.pause_heartbeat_for_tests(true);
    let id = identity.id();
    sql(
        &h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '3 seconds', \
             expires_at = now() - interval '2 seconds' WHERE id = '{id}'"
        ),
    )
    .await;
    let err = NodeMembership::register(Arc::clone(&identity), pool(&h).await)
        .await
        .unwrap_err();
    assert!(matches!(err, NodeError::AlreadyRegistered { .. }), "{err}");
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.to_string(),
        format!(
            "node {id} is already registered in database {}: one membership per process and \
             database",
            h.db().name()
        )
    );
    // the first membership's row was left alone
    let after = row(&h, &first).await.unwrap();
    assert_eq!(after.registered_at(), before.registered_at());
    assert_eq!(after.beat(), before.beat());
    first.pause_heartbeat_for_tests(false);
    first.shutdown().await.unwrap();
    // once the first membership has ended, the identity registers again
    let again = NodeMembership::register(Arc::clone(&identity), pool(&h).await)
        .await
        .unwrap();
    again.shutdown().await.unwrap();
}

/// A node row that expired two hours ago (its process is gone), past every retention.
async fn dead_node(h: &Harness, name: &'static str) -> erp_orm::node::NodeId {
    let dead = register(h, Spec::app(name)).await;
    let id = dead.identity().id();
    drop(dead);
    sql(
        h,
        &format!(
            "UPDATE erp_node SET heartbeat_at = now() - interval '2 hours', \
             expires_at = now() - interval '2 hours' WHERE id = '{id}'"
        ),
    )
    .await;
    id
}

/// Sessions waiting on a lock in a statement that contains `text`.
fn lock_waiters(text: &str) -> String {
    format!(
        "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND \
         wait_event_type = 'Lock' AND query LIKE '%{text}%'"
    )
}

#[tokio::test]
async fn a_locked_window_never_holds_back_the_purge_of_dead_nodes() {
    let h = setup().await;
    sql(&h, OLD_WINDOW).await;
    let dead_id = dead_node(&h, "app-dead").await;
    // an operator session holds the old window (an open transaction that closes or removes it)
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute("BEGIN; SELECT * FROM erp_node_rollout FOR UPDATE")
        .await
        .unwrap();
    // retention 1 s: every round purges
    let a = register(&h, Spec::app("app-a").retention(Duration::from_secs(1))).await;
    wait_for("the dead node's row to be purged", || async {
        rows(&h).await.iter().all(|r| r.id() != dead_id)
    })
    .await;
    let windows = "SELECT count(*) FROM erp_node_rollout";
    // the held window is skipped turn after turn, and says so: the count of skipped turns grows
    // from the first, which keeps its time
    wait_until("a skipped window turn", || {
        a.view().last_window_purge_error().is_some()
    })
    .await;
    let first = a.view().last_window_purge_error().cloned().unwrap();
    assert_eq!(first.sqlstate(), Some("55P03"), "{first}");
    wait_until("three skipped window turns in a row", || {
        a.view()
            .last_window_purge_error()
            .is_some_and(|e| e.failures() >= 3)
    })
    .await;
    let third = a.view().last_window_purge_error().cloned().unwrap();
    assert_eq!(third.sqlstate(), Some("55P03"), "{third}");
    assert_eq!(third.failing_since(), first.failing_since());
    assert_eq!(count(&h, windows).await, 1);
    // the node rows are not held back, and are not failing
    assert!(
        a.view().last_purge_error().is_none(),
        "{:?}",
        a.view().last_purge_error()
    );
    locker.batch_execute("ROLLBACK").await.unwrap();
    wait_for("the window to be purged at a later turn", || async {
        count(&h, windows).await == 0
    })
    .await;
    wait_until("the skipped turns to clear", || {
        a.view().last_window_purge_error().is_none()
    })
    .await;
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_window_extended_while_the_purge_waits_for_it_is_kept() {
    let h = setup().await;
    // a window that ended two hours ago without being closed
    sql(
        &h,
        "INSERT INTO erp_node_rollout (from_digest, to_digest, opened_by, opened_at, until) \
         VALUES (decode(repeat('01', 32), 'hex'), decode(repeat('02', 32), 'hex'), 'ended', \
         now() - interval '3 hours', now() - interval '2 hours')",
    )
    .await;
    // an operator extends it; the transaction is still open when the purge reads the window
    let operator = h.db().client().await.unwrap();
    operator
        .batch_execute("BEGIN; UPDATE erp_node_rollout SET until = now() + interval '1 hour'")
        .await
        .unwrap();
    // the first round purges (one interval after the registration); the purge waits on the
    // operator's row lock long enough for the test to commit
    let a = register(
        &h,
        Spec::app("app-a")
            .interval(Duration::from_secs(4))
            .retention(Duration::from_secs(1)),
    )
    .await;
    a.purge_lock_timeout_for_tests(Duration::from_secs(60));
    wait_for(
        "the window purge to wait on the operator's row lock",
        || async { count(&h, &lock_waiters("FROM erp_node_rollout")).await == 1 },
    )
    .await;
    operator.batch_execute("COMMIT").await.unwrap();
    wait_for("the window purge to finish", || async {
        count(
            &h,
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND \
             state = 'active' AND query LIKE '%FROM erp_node_rollout%' AND pid <> pg_backend_pid()",
        )
        .await
            == 0
    })
    .await;
    // the extended window is kept: the purge checks its age again on the row it removes
    assert_eq!(
        count(
            &h,
            "SELECT count(*) FROM erp_node_rollout WHERE until > now() AND closed_at IS NULL"
        )
        .await,
        1
    );
    assert!(
        a.view().last_purge_error().is_none(),
        "{:?}",
        a.view().last_purge_error()
    );
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_view_and_commands_never_wait_for_the_purge() {
    let h = setup().await;
    // the first round purges six seconds after the registration, bounded by half an interval
    let interval = Duration::from_secs(6);
    let a = start(
        &h,
        Spec::app("app-a")
            .interval(interval)
            .retention(Duration::from_secs(1)),
    )
    .await;
    a.purge_lock_timeout_for_tests(Duration::from_secs(60));
    // the dead node's details are held by an operator: the purge waits on them for its bound
    let dead_id = dead_node(&h, "app-dead").await;
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute(&format!(
            "BEGIN; SELECT * FROM erp_node_detail WHERE node_id = '{dead_id}' FOR UPDATE"
        ))
        .await
        .unwrap();
    let mut changes = a.subscribe();
    changes.borrow_and_update();
    let beat = a.view().beat();
    wait_for("the purge to wait on the locked detail rows", || async {
        count(&h, &lock_waiters("purged AS (")).await == 1
    })
    .await;
    let waiting: i32 = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT pid FROM pg_stat_activity WHERE datname = current_database() AND \
             wait_event_type = 'Lock' AND query LIKE '%purged AS (%'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    // the round's view was published before its purge started
    assert!(
        changes.has_changed().unwrap(),
        "the round's view waits for its purge"
    );
    assert!(a.view().beat() > beat);
    // a drain asked while the purge waits is answered without waiting for it
    let asked = Instant::now();
    a.drain().await.unwrap();
    let took = asked.elapsed();
    assert!(
        took < interval / 6,
        "the drain waited {took:?} for the purge (its bound is {:?})",
        interval / 2
    );
    assert_eq!(
        a.view().readiness(),
        &Readiness::Unready(UnreadyReason::Draining)
    );
    // the cut-short purge's backend ends by its statement timeout while the lock is still held:
    // nothing is left behind
    wait_for("the cut-short purge's backend to end", || async {
        count(
            &h,
            &format!("SELECT count(*) FROM pg_stat_activity WHERE pid = {waiting}"),
        )
        .await
            == 0
    })
    .await;
    locker.batch_execute("COMMIT").await.unwrap();
    // the cut-short purge runs again at a later turn
    wait_for("the dead node's row to be purged", || async {
        rows(&h).await.iter().all(|r| r.id() != dead_id)
    })
    .await;
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_window_purge_that_times_out_is_a_failure_not_a_skipped_turn() {
    let h = setup().await;
    sql(&h, OLD_WINDOW).await;
    let dead_id = dead_node(&h, "app-dead").await;
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute("BEGIN; SELECT * FROM erp_node_rollout FOR UPDATE")
        .await
        .unwrap();
    let a = register(
        &h,
        Spec::app("app-a")
            .interval(SLOW)
            .retention(Duration::from_secs(1)),
    )
    .await;
    // the window statement waits past its statement timeout, not its lock timeout
    a.purge_lock_timeout_for_tests(Duration::from_secs(60));
    wait_until("the window purge to be reported", || {
        a.view().last_window_purge_error().is_some()
    })
    .await;
    let failed = a.view().last_window_purge_error().cloned().unwrap();
    assert!(
        failed.sqlstate() == Some("57014")
            || (failed.sqlstate().is_none()
                && failed.message().starts_with(&format!(
                    "database {}: the purge of ended rollout windows did not finish within ",
                    h.db().name()
                ))),
        "{failed}"
    );
    // the dead node rows were purged in the same turn
    assert!(rows(&h).await.iter().all(|r| r.id() != dead_id));
    assert!(a.view().last_purge_error().is_none());
    locker.batch_execute("ROLLBACK").await.unwrap();
    wait_for("the window to be purged", || async {
        count(&h, "SELECT count(*) FROM erp_node_rollout").await == 0
    })
    .await;
    wait_until("the window error to clear", || {
        a.view().last_window_purge_error().is_none()
    })
    .await;
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failing_node_purge_never_holds_back_the_window_purge() {
    let h = setup().await;
    sql(&h, OLD_WINDOW).await;
    let dead_id = dead_node(&h, "app-dead").await;
    // the dead node's details are held: the node-row purge fails at its lock timeout
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute(&format!(
            "BEGIN; SELECT * FROM erp_node_detail WHERE node_id = '{dead_id}' FOR UPDATE"
        ))
        .await
        .unwrap();
    let a = register(&h, Spec::app("app-a").retention(Duration::from_secs(1))).await;
    wait_for("the window to be purged", || async {
        count(&h, "SELECT count(*) FROM erp_node_rollout").await == 0
    })
    .await;
    wait_until("the node-row purge failure to be reported", || {
        a.view().last_purge_error().is_some()
    })
    .await;
    let failed = a.view().last_purge_error().cloned().unwrap();
    assert_eq!(failed.sqlstate(), Some("55P03"), "{failed}");
    assert!(
        a.view().last_window_purge_error().is_none(),
        "{:?}",
        a.view().last_window_purge_error()
    );
    assert!(rows(&h).await.iter().any(|r| r.id() == dead_id));
    locker.batch_execute("COMMIT").await.unwrap();
    wait_for("the dead node's row to be purged", || async {
        rows(&h).await.iter().all(|r| r.id() != dead_id)
    })
    .await;
    wait_until("the node-row error to clear", || {
        a.view().last_purge_error().is_none()
    })
    .await;
    a.shutdown().await.unwrap();
}

/// The node registry's log events, captured by a process-wide subscriber (one per test process;
/// each test reads only its own database's events).
mod captured {
    use std::collections::BTreeMap;
    use std::fmt;
    use std::sync::{Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

    /// One event: its level and its fields as text (`message` included).
    #[derive(Debug, Clone)]
    pub(super) struct Logged {
        pub(super) level: Level,
        pub(super) fields: BTreeMap<&'static str, String>,
    }

    impl Logged {
        pub(super) fn failures(&self) -> Option<u32> {
            self.fields.get("failures").map(|f| f.parse().unwrap())
        }

        pub(super) fn message(&self) -> &str {
            self.fields.get("message").map_or("", String::as_str)
        }
    }

    static LOGGED: Mutex<Vec<Logged>> = Mutex::new(Vec::new());
    static INSTALLED: OnceLock<()> = OnceLock::new();

    struct Capture;

    struct Fields(BTreeMap<&'static str, String>);

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0.insert(field.name(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name(), value.to_owned());
        }
    }

    impl Subscriber for Capture {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target() == "erp::node"
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _: &Id, _: &Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            LOGGED.lock().unwrap().push(Logged {
                level: *event.metadata().level(),
                fields: fields.0,
            });
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }

    /// Install the capture once per process.
    pub(super) fn install() {
        INSTALLED.get_or_init(|| {
            tracing::subscriber::set_global_default(Capture)
                .expect("no other subscriber in the node tests");
        });
    }

    /// The events of one piece of background work on one database, in order.
    pub(super) fn of(database: &str, work: &str) -> Vec<Logged> {
        LOGGED
            .lock()
            .unwrap()
            .iter()
            .filter(|e| {
                e.fields.get("database").map(String::as_str) == Some(database)
                    && e.fields.get("work").map(String::as_str) == Some(work)
            })
            .cloned()
            .collect()
    }
}

/// Every logged failure of `work` counts one more than the one before it, from one, at the level
/// its count says (a warning, an error from the third in a row); returns the last count.
fn assert_failures_logged_in_steps(logged: &[captured::Logged], work: &str) -> u32 {
    let failures: Vec<&captured::Logged> =
        logged.iter().filter(|e| e.failures().is_some()).collect();
    assert!(!failures.is_empty(), "no failure of {work} logged");
    let mut expected = 1;
    for event in failures
        .iter()
        .filter(|e| e.message() != "background work succeeded again")
    {
        assert_eq!(event.failures(), Some(expected), "{work}: {logged:#?}");
        let level = if expected >= 3 {
            tracing::Level::ERROR
        } else {
            tracing::Level::WARN
        };
        assert_eq!(event.level, level, "{work}: {logged:#?}");
        expected += 1;
    }
    expected - 1
}

#[tokio::test]
async fn background_failures_are_logged_from_every_path_and_so_is_their_recovery() {
    captured::install();
    let h = setup().await;
    let db = h.db().name().to_owned();
    const TRANSITION: &str = "the readiness transition";
    const DETAILS: &str = "the details write of a re-inserted row";
    // This case verifies logged SQL failures and recovery, not a sub-second timing bound.
    // Leave enough scheduling grace for PostgreSQL's statement timeout to reach the client
    // before the outer readiness deadline on a loaded CI host; keep every error assertion.
    let a = register(&h, Spec::app("app-a").interval(SLOW)).await;
    // the readiness transition fails first on the command path: another session holds the
    // readiness lock, so a ready request times out
    let (class, key) = erp_orm::node::READINESS_LOCK;
    let locker = h.db().client().await.unwrap();
    locker
        .batch_execute(&format!("SELECT pg_advisory_lock({class}, {key})"))
        .await
        .unwrap();
    let err = a.request_ready().await.unwrap_err();
    assert_eq!(err.sqlstate(), Some("57014"), "{err}");
    let logged = captured::of(&db, TRANSITION);
    assert_eq!(logged.len(), 1, "{logged:#?}");
    assert_eq!(logged[0].failures(), Some(1), "{logged:#?}");
    assert_eq!(logged[0].level, tracing::Level::WARN);
    assert_eq!(logged[0].fields.get("error"), Some(&err.to_string()));
    // the heartbeat retries it after every renewal: the same count goes on, into errors
    wait_until("the transition's third failure in a row", || {
        a.view()
            .last_transition_error()
            .is_some_and(|e| e.failures() >= 3)
    })
    .await;
    locker
        .batch_execute(&format!("SELECT pg_advisory_unlock({class}, {key})"))
        .await
        .unwrap();
    wait_ready(&a).await;
    let logged = captured::of(&db, TRANSITION);
    let last = assert_failures_logged_in_steps(&logged, TRANSITION);
    assert!(last >= 3, "{logged:#?}");
    // the recovery is logged once, with the count it ends
    let recovered: Vec<_> = logged
        .iter()
        .filter(|e| e.message() == "background work succeeded again")
        .collect();
    assert_eq!(recovered.len(), 1, "{logged:#?}");
    assert_eq!(recovered[0].failures(), Some(last));
    assert_eq!(recovered[0].level, tracing::Level::INFO);
    // the details write after a re-inserted row fails while a constraint refuses every detail
    // row, and is logged the same way, then its recovery
    sql(
        &h,
        "ALTER TABLE erp_node_detail ADD CONSTRAINT refuse_details CHECK (false) NOT VALID",
    )
    .await;
    let id = a.identity().id();
    sql(&h, &format!("DELETE FROM erp_node WHERE id = '{id}'")).await;
    wait_until("the details write to fail", || {
        captured::of(&db, DETAILS)
            .iter()
            .any(|e| e.failures().is_some())
    })
    .await;
    sql(
        &h,
        "ALTER TABLE erp_node_detail DROP CONSTRAINT refuse_details",
    )
    .await;
    wait_until("the details write to succeed again", || {
        captured::of(&db, DETAILS)
            .iter()
            .any(|e| e.message() == "background work succeeded again")
    })
    .await;
    let logged = captured::of(&db, DETAILS);
    let last = assert_failures_logged_in_steps(&logged, DETAILS);
    let recovered = logged.last().unwrap();
    assert_eq!(recovered.message(), "background work succeeded again");
    assert_eq!(recovered.failures(), Some(last));
    assert_eq!(recovered.level, tracing::Level::INFO);
    assert_eq!(detail_count(&h, &a).await, 3);
    a.shutdown().await.unwrap();
}

/// Not a spec case: the heartbeat pool's connections notice a dead peer (DB review 2).
#[test]
fn the_heartbeat_pool_turns_tcp_keepalives_on() {
    let options = NodeOptions::default();
    let config = erp_orm::node::heartbeat_config(tokio_postgres::Config::new(), &options);
    assert!(config.get_keepalives());
    assert_eq!(config.get_keepalives_idle(), options.interval());
    assert_eq!(
        config.get_keepalives_interval(),
        Some(options.interval() / 4)
    );
    assert_eq!(config.get_keepalives_retries(), Some(4));
    assert_eq!(config.get_tcp_user_timeout(), Some(&options.interval()));
    erp_orm::node::heartbeat_pool(tokio_postgres::Config::new(), &options).unwrap();
}

/// Regression S-5: TCP keepalive options have whole-second kernel granularity.
#[test]
fn short_heartbeat_seams_keep_socket_options_nonzero() {
    for millis in [100, 999, 1000, 3999, 4000] {
        let interval = Duration::from_millis(millis);
        let options = NodeOptions::builder()
            .heartbeat_interval_for_tests(interval)
            .build()
            .unwrap();
        let config = erp_orm::node::heartbeat_config(tokio_postgres::Config::new(), &options);
        assert_eq!(
            config.get_keepalives_idle(),
            interval.max(Duration::from_secs(1))
        );
        assert_eq!(
            config.get_keepalives_interval(),
            Some((interval / 4).max(Duration::from_secs(1)))
        );
        assert_eq!(config.get_tcp_user_timeout(), Some(&interval));
        assert_eq!(options.interval(), interval);
    }
}

#[tokio::test]
async fn short_heartbeat_seam_connects_to_the_real_tcp_cluster() {
    let h = setup().await;
    let options = NodeOptions::builder()
        .heartbeat_interval_for_tests(Duration::from_millis(100))
        .build()
        .unwrap();
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &options).unwrap();
    let client = pool.get().await.unwrap();
    let value: i32 = client
        .query_one("SELECT 1::int4", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(value, 1);
}

#[tokio::test]
async fn read_audit_policy_mismatch_refuses_node_admission() {
    use erp_rpc::{ReadAuditPolicy, RpcConfig};
    let h = setup().await;
    async fn node(h: &Harness, name: &'static str, policy: ReadAuditPolicy) -> NodeMembership {
        let spec = Spec::app(name);
        let config = RpcConfig::builder()
            .read_audit_policy(policy)
            .build()
            .unwrap();
        let warm = pool(h).await;
        let identity = NodeIdentity::builder(spec.role, name, spec.host, build(), addons())
            .settings(&config)
            .unwrap()
            .options(options(&spec))
            .create(&warm)
            .await
            .unwrap();
        NodeMembership::register(Arc::new(identity), warm)
            .await
            .unwrap()
    }
    let a = node(
        &h,
        "audit-policy-a",
        ReadAuditPolicy::user_groups(&["base.group_user", "base.group_system"]).unwrap(),
    )
    .await;
    a.request_ready().await.unwrap();
    wait_ready(&a).await;
    let b = node(
        &h,
        "audit-policy-b",
        ReadAuditPolicy::user_groups(&["base.group_system", "base.group_user", "base.group_user"])
            .unwrap(),
    )
    .await;
    b.request_ready().await.unwrap();
    wait_ready(&b).await;
    let c = node(&h, "audit-policy-c", ReadAuditPolicy::all_users()).await;
    let readiness = c.request_ready().await.unwrap();
    assert_eq!(digest_mismatch(&readiness).sections(), ["rpc"]);
    assert!(!c.is_serving());
    c.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
    h.cleanup().await.unwrap();
}
