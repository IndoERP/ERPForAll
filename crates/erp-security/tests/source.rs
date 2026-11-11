//! `PolicySource` (
//! §3.2, §3.4, §8.3): shared builds on the source's own connection, published only on success
//! and labelled with the generation read in their own snapshot; SIGNAL's `Refresh` for
//! `policy`; private rebuilds from the writing transaction, never published. The compiler is a
//! recording stand-in, so these tests check the source, not the rules.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::{
    AccessPolicy, Orm, PolicyGeneration, PolicyProvider, SUPERUSER_ID, Signal, SnapshotId,
    SnapshotOrigin, SnapshotRef, Transaction,
};
use erp_registry::Registry;
use erp_security::rows::PolicyRows;
use erp_security::source::{PolicyCompiler, PolicySource};
use erp_testkit::Harness;
use support::{TestSnapshot, connect, harness, pool};

/// What one compile saw.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Compiled {
    id: SnapshotId,
    origin: SnapshotOrigin,
    generation: u64,
    rules: Vec<String>,
}

/// Records every compile; can be told to fail, or to mislabel the origin.
#[derive(Clone)]
struct Recorder {
    registry_generation: u64,
    log: Arc<Mutex<Vec<Compiled>>>,
    fail: Arc<AtomicBool>,
    mislabel: Arc<AtomicBool>,
}

impl Recorder {
    fn new(registry: &Registry) -> Self {
        Self {
            registry_generation: registry.generation(),
            log: Arc::default(),
            fail: Arc::default(),
            mislabel: Arc::default(),
        }
    }

    fn log(&self) -> Vec<Compiled> {
        self.log.lock().unwrap().clone()
    }
}

impl PolicyCompiler for Recorder {
    fn compile(&self, rows: &PolicyRows, origin: SnapshotOrigin) -> Result<Arc<dyn AccessPolicy>> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::new(
                ErrorKind::Programming,
                "the test compiler was told to fail",
            ));
        }
        let label = if self.mislabel.load(Ordering::SeqCst) {
            SnapshotOrigin::Private
        } else {
            origin
        };
        let snapshot = TestSnapshot::new(
            label,
            PolicyGeneration::new(self.registry_generation, rows.generation()),
            BTreeMap::new(),
        )?;
        self.log.lock().unwrap().push(Compiled {
            id: snapshot.snapshot().id(),
            origin,
            generation: rows.generation(),
            rules: rows.rules().iter().map(|r| r.name().to_owned()).collect(),
        });
        Ok(Arc::new(snapshot))
    }
}

/// The id of a committed `ir.model` row to hang rules on.
async fn model(h: &Harness) -> i64 {
    let client = connect(h).await;
    client
        .query_one(
            "INSERT INTO ir_model (name, model, state, transient) \
             VALUES ('Thing', 'x_thing', 'manual', false) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// Another process commits a rule (its trigger bumps `policy`).
async fn foreign_rule(h: &Harness, model: i64, name: &str) {
    let client = connect(h).await;
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, operation, active) VALUES ($1, $2, 'r', true)",
            &[&name, &model],
        )
        .await
        .unwrap();
}

/// The first build publishes a shared snapshot stamped with the registry generation and the
/// `policy` generation its own snapshot read.
#[tokio::test]
async fn build_publishes_the_first_shared_snapshot() {
    let h = harness().await;
    let thing = model(&h).await;
    foreign_rule(&h, thing, "first").await;
    let recorder = Recorder::new(h.registry());
    let source = PolicySource::build_with(Arc::clone(h.registry()), pool(&h, 2), recorder.clone())
        .await
        .unwrap();
    let current = source.current();
    let log = recorder.log();
    assert_eq!(log.len(), 1);
    assert_eq!(
        (log[0].origin, log[0].generation, log[0].rules.clone()),
        (SnapshotOrigin::Shared, 1, vec!["first".to_owned()])
    );
    assert_eq!(
        current.snapshot(),
        SnapshotRef::new(log[0].id, SnapshotOrigin::Shared)
    );
    assert_eq!(
        current.generation(),
        PolicyGeneration::new(h.registry().generation(), 1)
    );
    h.cleanup().await.unwrap();
}

/// DN §8.3 snapshot identity: every build gets a new `SnapshotId`, also when nothing changed
/// (two builds of one generation), and a refresh after a commit carries the new rows and the new
/// generation.
#[tokio::test]
async fn every_build_has_its_own_id() {
    let h = harness().await;
    let thing = model(&h).await;
    let recorder = Recorder::new(h.registry());
    let source = PolicySource::build_with(Arc::clone(h.registry()), pool(&h, 2), recorder.clone())
        .await
        .unwrap();
    let first = source.current().snapshot().id();
    let same = source.refresh_shared().await.unwrap();
    assert_ne!(same.snapshot().id(), first, "same generation, new id");
    assert_eq!(same.generation().policy(), 0);
    foreign_rule(&h, thing, "later").await;
    let later = source.refresh_shared().await.unwrap();
    assert_eq!(later.generation().policy(), 1);
    assert_eq!(source.current().snapshot().id(), later.snapshot().id());
    let ids: Vec<SnapshotId> = recorder.log().iter().map(|c| c.id).collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids grow: {ids:?}");
    assert_eq!(recorder.log()[2].rules, vec!["later".to_owned()]);
    h.cleanup().await.unwrap();
}

/// A failed build returns its error and leaves the published snapshot as it was; the next build
/// publishes again.
#[tokio::test]
async fn a_failed_build_keeps_the_published_snapshot() {
    let h = harness().await;
    let thing = model(&h).await;
    let recorder = Recorder::new(h.registry());
    let source = PolicySource::build_with(Arc::clone(h.registry()), pool(&h, 2), recorder.clone())
        .await
        .unwrap();
    let before = source.current().snapshot();
    foreign_rule(&h, thing, "r").await;
    recorder.fail.store(true, Ordering::SeqCst);
    let err = source
        .refresh_shared()
        .await
        .err()
        .expect("the build fails");
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "the test compiler was told to fail");
    assert_eq!(source.current().snapshot(), before, "nothing published");
    recorder.fail.store(false, Ordering::SeqCst);
    let after = source.refresh_shared().await.unwrap();
    assert_eq!(after.generation().policy(), 1);
    assert_ne!(source.current().snapshot(), before);
    h.cleanup().await.unwrap();
}

/// A compiler that labels a snapshot with another origin than asked is refused.
#[tokio::test]
async fn a_mislabelled_snapshot_is_refused() {
    let h = harness().await;
    let recorder = Recorder::new(h.registry());
    recorder.mislabel.store(true, Ordering::SeqCst);
    let err = PolicySource::build_with(Arc::clone(h.registry()), pool(&h, 2), recorder)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the policy compiler labelled a Shared snapshot of policy generation 0 as Private with \
         generation 0"
    );
    h.cleanup().await.unwrap();
}

/// SEC-018's private side (DN §3.4, §7 "In-transaction private rebuild ≤ 5"): the writing
/// transaction's rebuild reads its own uncommitted rule and bump in at most five statements, is
/// labelled `Private` with an id of its own, and is never published.
#[tokio::test]
async fn a_private_rebuild_sees_the_transaction_and_is_never_published() {
    let h = harness().await;
    let thing = model(&h).await;
    let recorder = Recorder::new(h.registry());
    let source = PolicySource::build_with(Arc::clone(h.registry()), pool(&h, 2), recorder.clone())
        .await
        .unwrap();
    let shared = source.current().snapshot();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO ir_access (name, model_id, operation, active) VALUES ('mine', $1, 'r', true)",
        &[&thing],
    )
    .await
    .unwrap();
    let private = erp_testkit::assert_query_count!(tx, >= 5, <= 5, {
        source.rebuild_private(&mut tx).await.unwrap()
    });
    assert_eq!(private.snapshot().origin(), SnapshotOrigin::Private);
    assert_eq!(private.generation().policy(), 1, "its own bump");
    assert_ne!(private.snapshot().id(), shared.id());
    assert_eq!(source.current().snapshot(), shared, "never published");
    let last = recorder.log().pop().unwrap();
    assert_eq!(
        (last.origin, last.rules),
        (SnapshotOrigin::Private, vec!["mine".to_owned()])
    );
    // through the provider trait the transaction engine calls
    let provider: &dyn PolicyProvider = &source;
    let again = provider.rebuild_private(&mut tx).await.unwrap();
    assert_eq!(again.snapshot().origin(), SnapshotOrigin::Private);
    assert_ne!(again.snapshot().id(), private.snapshot().id());
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(source.current().snapshot(), shared);
    h.cleanup().await.unwrap();
}

/// A source rebuilds the transactions of its own registry only.
#[tokio::test]
async fn a_private_rebuild_refuses_another_registry() {
    let h = harness().await;
    let other = harness().await;
    let source = PolicySource::build_with(
        Arc::clone(h.registry()),
        pool(&h, 2),
        Recorder::new(h.registry()),
    )
    .await
    .unwrap();
    let mut tx = other.begin().await.unwrap();
    let err = source
        .rebuild_private(&mut tx)
        .await
        .err()
        .expect("another registry is refused");
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "a policy source rebuilds the transactions of its own registry only; this transaction \
         belongs to an Orm of another registry"
    );
    drop(tx);
    other.cleanup().await.unwrap();
    h.cleanup().await.unwrap();
}

/// An `Orm` on `orm_pool` serving `source`'s snapshots, with `source` as its refresh hook.
async fn signal_orm(
    h: &Harness,
    source: &PolicySource,
    orm_pool: erp_testkit::deadpool_postgres::Pool,
) -> Orm {
    let baseline = Signal::read(&connect(h).await).await.unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source.clone());
    Orm::builder(Arc::clone(h.registry()), orm_pool)
        .policy(provider)
        .resolver(erp_security::resolver::Resolver::id_order())
        .signal(
            Signal::new(baseline)
                .with_refresh(source.clone())
                .with_apply_timeout(Duration::from_secs(3))
                .unwrap(),
        )
        .build()
        .unwrap()
}

async fn first_statement(orm: &Orm) -> Result<Transaction> {
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    tx.query("SELECT 1", &[]).await?;
    Ok(tx)
}

/// SIGNAL's hook (DN §3.2): a request that reads a larger `policy` generation waits for one shared
/// build, which runs on the source's own pool, so it succeeds while the request holds the only
/// connection of the `Orm`'s pool; the transaction then runs on the new snapshot.
#[tokio::test]
async fn the_refresh_hook_builds_on_its_own_connection() {
    let h = harness().await;
    let thing = model(&h).await;
    let recorder = Recorder::new(h.registry());
    let source = PolicySource::build_with(Arc::clone(h.registry()), pool(&h, 1), recorder.clone())
        .await
        .unwrap();
    let orm = signal_orm(&h, &source, pool(&h, 1)).await;
    foreign_rule(&h, thing, "foreign").await;
    let tx = first_statement(&orm).await.unwrap();
    assert_eq!(source.current().generation().policy(), 1);
    assert_eq!(orm.signal().unwrap().stats().refreshes, 1);
    assert_eq!(
        recorder.log().last().unwrap().rules,
        vec!["foreign".to_owned()]
    );
    drop(tx);
    // nothing new: no second build
    let tx = first_statement(&orm).await.unwrap();
    drop(tx);
    assert_eq!(recorder.log().len(), 2);
    h.cleanup().await.unwrap();
}

/// Why the source needs its own pool: sharing the `Orm`'s single connection, the build waits for
/// the connection the waiting request holds, until SIGNAL's apply timeout cancels it.
#[tokio::test]
async fn a_source_on_the_requests_pool_would_wait_for_itself() {
    let h = harness().await;
    let thing = model(&h).await;
    let shared_pool = pool(&h, 1);
    let source = PolicySource::build_with(
        Arc::clone(h.registry()),
        shared_pool.clone(),
        Recorder::new(h.registry()),
    )
    .await
    .unwrap();
    let orm = signal_orm(&h, &source, shared_pool).await;
    foreign_rule(&h, thing, "foreign").await;
    let err = first_statement(&orm).await.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(
        err.message(),
        "the signal refresh hook did not finish within 3s (Signal::with_apply_timeout): it was \
         cancelled and the registry and policy generations stay unapplied, so the next request \
         tries again"
    );
    assert_eq!(
        source.current().generation().policy(),
        0,
        "nothing published"
    );
    h.cleanup().await.unwrap();
}

/// A moved `registry` generation is not the source's to refresh: the request fails loudly
/// instead of running on a stale registry.
#[tokio::test]
async fn a_registry_change_is_refused() {
    let h = harness().await;
    let source = PolicySource::build_with(
        Arc::clone(h.registry()),
        pool(&h, 1),
        Recorder::new(h.registry()),
    )
    .await
    .unwrap();
    let orm = signal_orm(&h, &source, pool(&h, 1)).await;
    connect(&h)
        .await
        .execute(
            "INSERT INTO erp_signal_generation (key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();
    let err = first_statement(&orm).await.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the registry generation moved, and a policy source refreshes the policy of one \
         registry only: install the server's refresh hook (a later release), which replaces the Orm \
         and builds a policy source for the new registry"
    );
    h.cleanup().await.unwrap();
}

/// With the real compiler: the shared snapshot closes the group graph read from the database,
/// and a transaction that adds an implication decides on a private snapshot that already has it
/// while the shared one does not.
#[tokio::test]
async fn the_compiler_builds_from_the_database_rows() {
    let h = harness().await;
    let client = connect(&h).await;
    let mut ids = Vec::new();
    for name in ["Everyone", "Internal", "Employee"] {
        let id: i64 = client
            .query_one(
                "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
                &[&name],
            )
            .await
            .unwrap()
            .get(0);
        ids.push(id);
    }
    let (everyone, internal, employee) = (ids[0], ids[1], ids[2]);
    client
        .execute(
            "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2)",
            &[&internal, &everyone],
        )
        .await
        .unwrap();
    let gid = |id: i64| erp_orm::GroupId::new(id).unwrap();
    let source = PolicySource::build(Arc::clone(h.registry()), pool(&h, 2))
        .await
        .unwrap();
    let shared = source.current();
    assert_eq!(shared.snapshot().origin(), SnapshotOrigin::Shared);
    assert_eq!(
        shared.groups(&[gid(employee)]).unwrap(),
        erp_orm::GroupSet::new([gid(employee)])
    );
    assert_eq!(
        shared.groups(&[gid(internal)]).unwrap(),
        erp_orm::GroupSet::new([gid(internal), gid(everyone)])
    );

    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2)",
        &[&employee, &internal],
    )
    .await
    .unwrap();
    let private = source.rebuild_private(&mut tx).await.unwrap();
    assert_eq!(
        private.groups(&[gid(employee)]).unwrap(),
        erp_orm::GroupSet::new([gid(employee), gid(internal), gid(everyone)])
    );
    assert_eq!(
        source.current().groups(&[gid(employee)]).unwrap(),
        erp_orm::GroupSet::new([gid(employee)]),
        "the shared snapshot is unchanged"
    );
    let err = shared.groups(&[gid(999_999)]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "The group 999999 does not exist (any more): the user's groups cannot be resolved."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// DN §3.2 through SIGNAL: a build error fails the request that waited for it with that error,
/// the stale snapshot is not served and the `policy` generation stays unapplied, so the next
/// request builds again and runs on the new snapshot.
#[tokio::test]
async fn a_failed_refresh_fails_the_request_and_the_next_one_retries() {
    let h = harness().await;
    let thing = model(&h).await;
    let recorder = Recorder::new(h.registry());
    let source = PolicySource::build_with(Arc::clone(h.registry()), pool(&h, 1), recorder.clone())
        .await
        .unwrap();
    let orm = signal_orm(&h, &source, pool(&h, 1)).await;
    foreign_rule(&h, thing, "foreign").await;
    recorder.fail.store(true, Ordering::SeqCst);
    let err = first_statement(&orm).await.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "the test compiler was told to fail");
    assert_eq!(
        source.current().generation().policy(),
        0,
        "nothing published"
    );
    recorder.fail.store(false, Ordering::SeqCst);
    let tx = first_statement(&orm).await.unwrap();
    assert_eq!(source.current().generation().policy(), 1);
    assert_eq!(
        orm.signal().unwrap().stats().refreshes,
        2,
        "one failed, one built"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// DN §3.2, SIGNAL ruling (c): a `policy` generation that went backwards (rows deleted by hand,
/// a restored database) is a regression: SIGNAL asks for a full refresh, which a policy source
/// cannot do alone (the registry is part of it), so the request fails with the regression
/// condition and nothing is published.
#[tokio::test]
async fn a_lower_policy_generation_fails_loudly() {
    let h = harness().await;
    let thing = model(&h).await;
    foreign_rule(&h, thing, "r").await;
    let source = PolicySource::build_with(
        Arc::clone(h.registry()),
        pool(&h, 1),
        Recorder::new(h.registry()),
    )
    .await
    .unwrap();
    let orm = signal_orm(&h, &source, pool(&h, 1)).await;
    assert_eq!(source.current().generation().policy(), 1);
    connect(&h)
        .await
        .batch_execute("DELETE FROM erp_signal_generation WHERE key = 'policy'")
        .await
        .unwrap();
    let err = first_statement(&orm).await.err().unwrap();
    assert!(err.is_signal_regression(), "{err:?}");
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(
        err.message(),
        "the signal generations went backwards (policy 1 -> 0): the database was restored or \
         signal rows were deleted; the full refresh failed and the next request tries again"
    );
    assert_eq!(
        source.current().generation().policy(),
        1,
        "nothing published"
    );
    h.cleanup().await.unwrap();
}
