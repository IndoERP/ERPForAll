//! read routing of `read`-effect transactions to a read pool (H16),
//! the unit of work that keeps read-your-writes, the access predicate on the routed connection and
//! the replica lag guard. The "replica" is a second pool on the
//! same database, as a read-only role (`SET ROLE`), whose connections carry
//! `application_name=erp_read_pool`; the lag tests import a real older snapshot into the routed
//! transaction.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use erp_orm::{
    AccessPolicy, BeginOptions, CompanyId, GroupId, GroupSet, IntoPolicyProvider, ListenerOptions,
    Operation, Orm, Principal, PrincipalBuilder, PrincipalHint, PrincipalResolver, ReadAuditOwner,
    ReadPoolFailure, Resolution, ResolverStatement, Route, SUPERUSER_ID, Signal, SignalKey, UserId,
    spawn_in_unit_of_work, unit_of_work,
};
use erp_sdk::{Domain, ErrorKind, Operator, Records, Transaction, Value};
use erp_sql::param::{Param, Scalar};
use erp_test_models::{base_users, test_access, test_field_access};
use erp_testkit::deadpool_postgres::{
    Hook, HookError, Manager, ManagerConfig, Pool, RecyclingMethod, Runtime,
};
use erp_testkit::tokio_postgres::{Config, NoTls, Row};
use erp_testkit::{DomainPolicy, FieldPolicy, Harness, Otherwise};
use support::{map, rpc};

const OBJ: &str = "test_access.some_obj";
const READ_POOL: &str = "erp_read_pool";
const PRIMARY: &str = "erp_primary";

// ---------------------------------------------------------------------------------------------
// fixtures

fn pool_for(mut config: Config, options: &str) -> Pool {
    config.options(options);
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(4)
        .runtime(Runtime::Tokio1)
        .build()
        .unwrap()
}

/// A refresh hook that applies registry and policy bumps in place.
struct InPlace;

impl erp_orm::Refresh for InPlace {
    fn refresh<'a>(
        &'a self,
        _changes: &'a erp_orm::Changes,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::Refreshed>> {
        Box::pin(async { Ok(erp_orm::Refreshed::InPlace) })
    }
}

async fn install_signal(h: &Harness) -> erp_orm::Generations {
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    let generations = tx.signal_generations().await.unwrap();
    tx.commit().await.unwrap();
    generations
}

/// An ORM over the harness database with signals and a read pool (`erp_read_pool`); its primary
/// pool connections carry `erp_primary`.
async fn routed_orm(h: &Harness, policy: impl IntoPolicyProvider) -> Orm {
    routed_builder(h, policy, None).await.build().unwrap()
}

async fn routed_builder(
    h: &Harness,
    policy: impl IntoPolicyProvider,
    read_options: Option<&str>,
) -> erp_orm::OrmBuilder {
    // the tables first: the role reads the tables that exist when it is made
    install_signal(h).await;
    let options =
        read_options.map_or_else(|| format!("-c application_name={READ_POOL}"), str::to_owned);
    routed_builder_on(h, policy, read_only_pool(h, &options).await)
        .await
        .trust_caller_resolutions()
}

/// The lag bound routed builders set: no signal listener runs in most tests, and the bound
/// stands in for it (a process without a listener routes nothing otherwise).
const TEST_MAX_LAG: Duration = Duration::from_secs(60);

/// A pool that logs in as the database's read-only login (`Harness::read_only_role`).
async fn read_only_pool(h: &Harness, options: &str) -> Pool {
    let mut config = h.read_only_role().await.unwrap();
    config.options(options);
    Pool::builder(Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    ))
    .max_size(4)
    .runtime(Runtime::Tokio1)
    .build()
    .unwrap()
}

/// The builder with signals and `read` as the read pool (no resolver choice made).
async fn routed_builder_on(
    h: &Harness,
    policy: impl IntoPolicyProvider,
    read: Pool,
) -> erp_orm::OrmBuilder {
    routed_builder_with_lag(h, policy, read, Some(TEST_MAX_LAG)).await
}

/// [`routed_builder_on`] with the lag bound of the caller (`None`: unbounded).
async fn routed_builder_with_lag(
    h: &Harness,
    policy: impl IntoPolicyProvider,
    read: Pool,
    lag: Option<Duration>,
) -> erp_orm::OrmBuilder {
    let baseline = install_signal(h).await;
    let primary = pool_for(h.db().config(), &format!("-c application_name={PRIMARY}"));
    let builder = Orm::builder(Arc::clone(h.registry()), primary)
        .policy(policy)
        .signal(Signal::new(baseline).with_refresh(InPlace))
        .read_pool(read);
    match lag {
        Some(lag) => builder.read_pool_max_lag(lag),
        None => builder,
    }
}

/// A read pool on the same database as the owning (writable) role.
fn writable_pool(h: &Harness) -> Pool {
    pool_for(h.db().config(), &format!("-c application_name={READ_POOL}"))
}

fn read_opts(entry: &str) -> BeginOptions {
    BeginOptions::new(SUPERUSER_ID).read_only(entry.to_owned())
}

async fn app_name(tx: &mut Transaction) -> String {
    let rows = tx
        .query("SELECT current_setting('application_name')", &[])
        .await
        .unwrap();
    rows[0].get::<_, String>(0)
}

async fn bump(h: &Harness, key: &SignalKey) {
    let client = h.db().client().await.unwrap();
    client
        .execute(
            "INSERT INTO erp_signal_generation (key) VALUES ($1)",
            &[&key.to_key_string()],
        )
        .await
        .unwrap();
}

async fn access_harness(policy: DomainPolicy) -> Harness {
    Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap()
}

fn open_policy() -> DomainPolicy {
    DomainPolicy::builder(Otherwise::Allow).build().unwrap()
}

// ---------------------------------------------------------------------------------------------
// routing

/// Budget (no spec case): a routed read-only transaction opens in one round trip on the read
/// connection, with the generation read and the replica lag read in the same flight as `BEGIN`
/// and the limits, as a primary start does (start composed with).
#[tokio::test]
async fn a_routed_read_only_start_is_one_round_trip() {
    let h = access_harness(open_policy()).await;
    install_signal(&h).await;
    let mut config = h.read_only_role().await.unwrap();
    config.options(format!("-c application_name={READ_POOL}"));
    let wire = erp_testkit::WireCounter::start(&config).await.unwrap();
    let read = wire.pool(4).unwrap();
    let orm = routed_builder_on(&h, open_policy(), read)
        .await
        .trust_caller_resolutions()
        .build()
        .unwrap();
    // the first routed transaction verifies its connection (outside the budget)
    let mut first = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(app_name(&mut first).await, READ_POOL);
    first.rollback().await.unwrap();

    // `first` still holds its connection, so this transaction verifies a second one; the
    // verification drops a prepared statement, whose `Close` goes out without waiting for its
    // answer. `assert_round_trips!` waits for that answer before the block, else the opening
    // flight could leave behind it and count as part of the close's turn (0 seen,
    // ).
    let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::ReadPool);
    wire.reset();
    erp_testkit::assert_round_trips!(wire, == 1, {
        tx.open().await.unwrap();
    });
    assert_eq!(tx.route(), Route::ReadPool, "the lag guard kept it here");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A read-intent transaction runs on the read pool, a write never does.
#[tokio::test]
async fn read_intent_routes_to_the_read_pool_and_write_never_does() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    assert!(orm.routing_stats().read_pool);

    let mut read = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(read.route(), Route::ReadPool);
    assert_eq!(app_name(&mut read).await, READ_POOL);
    assert_eq!(read.route(), Route::ReadPool);
    read.rollback().await.unwrap();

    let mut write = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(write.route(), Route::Primary);
    assert_eq!(app_name(&mut write).await, PRIMARY);
    write.rollback().await.unwrap();

    let stats = orm.routing_stats();
    assert_eq!(stats.read_pool_served, 1);
    assert_eq!(stats.unit_of_work_wrote, 0);
    assert_eq!(stats.replica_behind, 0);
    h.cleanup().await.unwrap();
}

/// Budget (no spec case): the ORM `Orm::rebuild` makes for a stage registry keeps the read pool,
/// its failure policy and its lag bound, so reads of a module operation's stage ORM still route to
/// the replica (merge with's `rebuild`, which builds the new builder field by
/// field).
#[tokio::test]
async fn a_rebuilt_orm_keeps_the_read_routing() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let rebuilt = orm.rebuild(Arc::clone(h.registry())).build().unwrap();
    assert!(
        rebuilt.routing_stats().read_pool,
        "the rebuilt ORM reports the read pool"
    );
    let mut read = rebuilt.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(read.route(), Route::ReadPool);
    assert_eq!(app_name(&mut read).await, READ_POOL);
    read.rollback().await.unwrap();
    assert_eq!(rebuilt.routing_stats().read_pool_served, 1);
    assert_eq!(
        orm.routing_stats().read_pool_served,
        0,
        "each ORM counts its own routing"
    );
    h.cleanup().await.unwrap();
}

/// A store answer is read on the primary only (store wire assumption A2,
/// store-wire.md` "Read on the primary only"): a read-only transaction routed to the read pool
/// cannot take the snapshot descriptor, one on the primary can.
#[tokio::test]
async fn a_store_snapshot_on_the_read_pool_is_refused() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let mut routed = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(routed.route(), Route::ReadPool);
    let error = routed.store_snapshot().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "store_snapshot: store answers are read on the primary only (assumption A2: snapshots \
         of a standby are not shown to nest with the primary's); this transaction ran on the \
         read pool. Serve the call from the primary (messaging store decision D-4; declare the \
         method in PRIMARY_READS)"
    );
    routed.rollback().await.unwrap();
    let mut primary = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(primary.route(), Route::Primary);
    assert!(primary.store_snapshot().await.unwrap().own().is_none());
    primary.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Without a read pool every transaction uses the primary, and the stats say so.
#[tokio::test]
async fn without_a_read_pool_every_transaction_uses_the_primary() {
    let h = access_harness(open_policy()).await;
    let orm = h.orm();
    let mut read = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(read.route(), Route::Primary);
    read.query("SELECT 1", &[]).await.unwrap();
    read.rollback().await.unwrap();
    let stats = orm.routing_stats();
    assert!(!stats.read_pool);
    assert_eq!(stats.read_pool_served, 0);
    let error = orm.check_read_pool().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "no read pool is configured (OrmBuilder::read_pool)"
    );
    h.cleanup().await.unwrap();
}

/// A read pool needs the signal: the lag guard has nothing to compare otherwise.
#[tokio::test]
async fn a_read_pool_without_a_signal_is_refused_at_build() {
    let h = access_harness(open_policy()).await;
    let pool = pool_for(h.db().config(), "-c application_name=x");
    let error = Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .policy(open_policy())
        .read_pool(pool)
        .build()
        .err()
        .expect("build must fail");
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "OrmBuilder::read_pool needs OrmBuilder::signal: the replica lag guard compares the \
         generations the read pool returns with the ones this process applied"
    );
    h.cleanup().await.unwrap();
}

/// Read-your-writes: once a write transaction began in the unit of work, its reads use the
/// primary, also a second transaction opened while the write is still open.
#[tokio::test]
async fn a_unit_of_work_that_wrote_reads_from_the_primary() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;

    unit_of_work(async {
        // before any write: the read pool
        let mut before = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(before.route(), Route::ReadPool);
        before.open().await.unwrap();
        drop(before);

        // a write is open: a second transaction in the same unit of work reads from the primary
        let write = orm.begin(SUPERUSER_ID).await.unwrap();
        let mut during = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(during.route(), Route::Primary);
        during.open().await.unwrap();
        drop(during);
        drop(write);

        // the write ended (even rolled back, even committed: the replica may not have replayed
        // it): still the primary
        let mut after = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(after.route(), Route::Primary);
        after.open().await.unwrap();
    })
    .await;
    // counted when the BEGIN succeeded; both ran inside the unit of work
    let stats = orm.routing_stats();
    assert_eq!(
        (
            stats.unit_of_work_wrote,
            stats.read_pool_served,
            stats.unscoped
        ),
        (2, 1, 0)
    );

    // the next unit of work starts clean
    unit_of_work(async {
        let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(tx.route(), Route::ReadPool);
        tx.open().await.unwrap();
    })
    .await;
    assert_eq!(orm.routing_stats().unscoped, 0);
    // outside any scope each transaction is its own unit of work, and the read is counted as
    // unscoped: it cannot see the writes of the request it belongs to
    let w = orm.begin(SUPERUSER_ID).await.unwrap();
    drop(w);
    let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::ReadPool);
    tx.open().await.unwrap();
    drop(tx);
    assert_eq!(orm.routing_stats().unscoped, 1);
    h.cleanup().await.unwrap();
}

/// A read opened before a write began, first used after it: the route is re-checked at `BEGIN`.
#[tokio::test]
async fn a_read_opened_before_the_write_moves_to_the_primary_at_begin() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    unit_of_work(async {
        let mut read = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(read.route(), Route::ReadPool);
        let write = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(app_name(&mut read).await, PRIMARY);
        assert_eq!(read.route(), Route::Primary);
        read.rollback().await.unwrap();
        drop(write);
    })
    .await;
    let stats = orm.routing_stats();
    assert_eq!((stats.read_pool_served, stats.unit_of_work_wrote), (0, 1));
    h.cleanup().await.unwrap();
}

/// An owned read-only transaction (a read-audit owner is declared) of the superuser.
fn owned_read_opts() -> BeginOptions {
    BeginOptions::new(SUPERUSER_ID)
        .read_only_audited("test.read", ReadAuditOwner::new("rtt-route-test").unwrap())
}

/// an owned read-only transaction is recorded before the first statement; the first statement
/// reroutes the transaction to the primary (a write began in the unit of work), and the commit
/// still hands the elevation to the owner's attempt row.
#[tokio::test]
async fn unscoped_sudo_evidence_survives_a_reroute_to_the_primary() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    // the store state is cached per ORM: this one has not checked it yet
    orm.check_audit_store().await.unwrap();
    unit_of_work(async {
        let mut read = orm.begin_opts(owned_read_opts()).await.unwrap();
        assert_eq!(read.route(), Route::ReadPool);
        let write = orm.begin(SUPERUSER_ID).await.unwrap();
        let base = read.env();
        read.audited_unscoped_sudo(&base, "rtt_route").unwrap();
        assert_eq!(app_name(&mut read).await, PRIMARY);
        assert_eq!(read.route(), Route::Primary);
        read.commit().await.unwrap();
        let evidence = read.take_audit_attempt();
        assert!(evidence.has_elevation(), "{evidence:?}");
        let shown = format!("{evidence:?}");
        assert!(shown.contains("core.sudo.unscoped"), "{shown}");
        drop(write);
    })
    .await;
    h.cleanup().await.unwrap();
}

/// owned read-only transaction that has sent no statement opens the transaction first, so the
/// reroute to the primary settles before the block exists: the block reads on the primary, and the
/// commit hands the elevation to the owner's attempt row.
#[tokio::test]
async fn a_scoped_block_in_an_owned_read_reroutes_before_the_block_and_keeps_its_evidence() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    // the store state is cached per ORM: this one has not checked it yet
    orm.check_audit_store().await.unwrap();
    unit_of_work(async {
        let mut read = orm.begin_opts(owned_read_opts()).await.unwrap();
        assert_eq!(read.route(), Route::ReadPool);
        let write = orm.begin(SUPERUSER_ID).await.unwrap();
        let model = read.registry().model_id("res.users").unwrap();
        let scope = erp_orm::ElevationScope::new([erp_orm::ScopeItem::new(
            model,
            erp_orm::OperationSet::of([Operation::Read]),
        )
        .unwrap()])
        .unwrap();
        let reason = erp_orm::audit::ElevationReason::new("rtt.route.block").unwrap();
        let base = read.env();
        let app = read
            .elevated(&base, reason, scope, async |tx, env| {
                env.records("res.users")?.search(tx, &Domain::TRUE).await?;
                Ok(app_name(tx).await)
            })
            .await
            .unwrap();
        assert_eq!(app, PRIMARY);
        assert_eq!(read.route(), Route::Primary);
        read.commit().await.unwrap();
        let evidence = read.take_audit_attempt();
        assert!(evidence.has_elevation(), "{evidence:?}");
        let shown = format!("{evidence:?}");
        assert!(shown.contains("rtt.route.block"), "{shown}");
        drop(write);
    })
    .await;
    h.cleanup().await.unwrap();
}

/// `Orm::run` is a unit of work: reads opened by its work, after its write transaction began, use
/// the primary.
#[tokio::test]
async fn orm_run_is_a_unit_of_work() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let inner = orm.clone();
    let route = orm
        .run(SUPERUSER_ID, erp_orm::Context::new(), async move |_tx| {
            let read = inner.begin_opts(read_opts("test.read")).await?;
            Ok(read.route())
        })
        .await
        .unwrap();
    assert_eq!(route, Route::Primary);
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// the access predicate is kept

fn company(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

fn principal(uid: i64) -> Principal {
    PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), [company(1)], NonZeroU64::MIN).unwrap(),
        )
        .companies(vec![company(1)])
        .build()
        .unwrap()
}

fn restricting() -> DomainPolicy {
    // reads see val <= 3 only; everything else on the model is refused
    DomainPolicy::builder(Otherwise::Deny)
        .rule(
            OBJ,
            Operation::Read,
            Domain::condition("val", Operator::Le, 3_i64).unwrap(),
        )
        .build()
        .unwrap()
}

async fn objs(h: &Harness) -> Vec<i64> {
    let rows: Vec<_> = (1..=6)
        .map(|v| {
            map(&[
                ("name", Value::str(format!("o{v}"))),
                ("val", Value::Int(v)),
            ])
        })
        .collect();
    let mut tx = h.begin().await.unwrap();
    let ids = tx
        .env()
        .records(OBJ)
        .unwrap()
        .create_values(&mut tx, &rows)
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    ids
}

/// What a principal reads of the six objects through one transaction.
#[derive(Debug, PartialEq)]
struct Seen {
    found: Vec<i64>,
    readable: Vec<i64>,
    refused: (ErrorKind, String),
}

async fn seen(tx: &mut Transaction, ids: &[i64]) -> Seen {
    let all: Records = tx
        .env()
        .records(OBJ)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap();
    let found = tx
        .env()
        .records(OBJ)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids();
    let readable = all
        .filtered_access(tx, Operation::Read)
        .await
        .unwrap()
        .ids();
    let refused = all.check_access(tx, Operation::Read).await.unwrap_err();
    Seen {
        found,
        readable,
        refused: (refused.kind(), refused.message().to_owned()),
    }
}

/// The same principal, policy snapshot and compiled rules decide on the routed connection: the
/// same ids are found and readable, the same records are refused with the same error.
#[tokio::test]
async fn the_record_rules_apply_to_a_routed_transaction() {
    let h = access_harness(restricting()).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let ids = objs(&h).await;
    let orm = routed_orm(&h, restricting()).await;

    let mut primary = orm
        .begin_opts(BeginOptions::for_principal(principal(2)))
        .await
        .unwrap();
    let on_primary = seen(&mut primary, &ids).await;
    assert_eq!(primary.route(), Route::Primary);
    primary.rollback().await.unwrap();

    let mut routed = orm
        .begin_opts(
            BeginOptions::for_principal(principal(2)).read_only("test_access.some_obj.read"),
        )
        .await
        .unwrap();
    let on_read_pool = seen(&mut routed, &ids).await;
    assert_eq!(routed.route(), Route::ReadPool);
    assert_eq!(app_name(&mut routed).await, READ_POOL);
    routed.rollback().await.unwrap();

    assert_eq!(on_read_pool, on_primary);
    // the restriction really restricts: three of six, and the refusal names the model
    assert_eq!(on_primary.found, ids[..3].to_vec());
    assert_eq!(on_primary.readable, ids[..3].to_vec());
    assert_eq!(
        on_primary.refused.0,
        ErrorKind::Access,
        "{}",
        on_primary.refused.1
    );
    assert_eq!(
        on_primary.refused.1,
        format!(
            "Your access rights do not let you access 'Object For Test Access Right' \
             (test_access.some_obj) records.\n\nRecords: test_access.some_obj({},), User: 2",
            ids[3]
        )
    );
    assert_eq!(orm.routing_stats().read_pool_served, 1);
    h.cleanup().await.unwrap();
}

const ITEM: &str = "test_fa.item";

fn field_policy() -> FieldPolicy {
    FieldPolicy::new().forbid(ITEM, "forbidden3")
}

/// A refused field stays refused on the read pool; an allowed one reads the same value.
#[tokio::test]
async fn the_field_masks_apply_to_a_routed_transaction() {
    let policy = field_policy();
    let h = Harness::builder([&base_users::ADDON, &test_field_access::ADDON])
        .policy(policy.clone())
        .build()
        .await
        .unwrap();
    let (item, uid) = {
        let mut tx = h.begin().await.unwrap();
        let uid = tx
            .env()
            .records("res.users")
            .unwrap()
            .create_values(
                &mut tx,
                &[map(&[
                    ("name", Value::str("demo")),
                    ("login", Value::str("demo")),
                ])],
            )
            .await
            .unwrap()
            .ids()[0];
        let item = tx
            .env()
            .records(ITEM)
            .unwrap()
            .create_values(
                &mut tx,
                &[map(&[
                    ("name", Value::str("item")),
                    ("val", Value::Int(7)),
                    ("forbidden3", Value::Int(58)),
                ])],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        (item, uid)
    };
    let orm = routed_orm(&h, policy).await;

    async fn probe(tx: &mut Transaction, item: i64, uid: i64) -> (Value, (ErrorKind, String)) {
        let records = tx
            .env()
            .records(ITEM)
            .unwrap()
            .with_user(uid)
            .browse([item])
            .unwrap();
        let allowed = records.value(tx, "val").await.unwrap();
        let refused = records.value(tx, "forbidden3").await.unwrap_err();
        let refused_read = records.read(tx, &["forbidden3"]).await.unwrap_err();
        assert_eq!(refused.message(), refused_read.message());
        (allowed, (refused.kind(), refused.message().to_owned()))
    }

    let mut primary = orm.begin(SUPERUSER_ID).await.unwrap();
    let on_primary = probe(&mut primary, item, uid).await;
    primary.rollback().await.unwrap();
    let mut routed = orm
        .begin_opts(read_opts("test_fa.item.read"))
        .await
        .unwrap();
    let on_read_pool = probe(&mut routed, item, uid).await;
    assert_eq!(routed.route(), Route::ReadPool);
    routed.rollback().await.unwrap();

    assert_eq!(on_read_pool, on_primary);
    assert_eq!(on_primary.0, Value::Int(7));
    assert_eq!(on_primary.1.0, ErrorKind::Access);
    assert_eq!(
        on_primary.1.1,
        "You do not have enough rights to access the field \"forbidden3\" on Object For Test \
         Access Right (test_fa.item). Please contact your system administrator.\n\nOperation: \
         read"
    );
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// read-only end to end

/// A write in a routed transaction fails with the typed error naming the entry method, through
/// the ORM and through SQL; the database refuses what the guard cannot classify.
#[tokio::test]
async fn a_write_in_a_routed_transaction_fails_loudly() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let guard = |op: &str| {
        format!(
            "'test_access.some_obj.read' is declared effect = read but tried to {op}; declare \
             effect = write (or wider)."
        )
    };

    let mut tx = orm
        .begin_opts(read_opts("test_access.some_obj.read"))
        .await
        .unwrap();
    assert_eq!(tx.route(), Route::ReadPool);
    let error = tx
        .execute("INSERT INTO test_access_some_obj (name) VALUES ('x')", &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), guard("write through SQL"));

    let error = tx
        .env()
        .records(OBJ)
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", Value::str("x"))])])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), guard("create 'test_access.some_obj'"));

    // transaction-level advisory locks would lock only the replica: refused by name
    for function in [
        "pg_advisory_xact_lock(1)",
        "pg_try_advisory_xact_lock_shared(1)",
    ] {
        let error = tx
            .query(&format!("SELECT {function}"), &[])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert!(
            error.message().contains(&format!(
                "called {}(), which a read-only",
                &function[..function.find('(').unwrap()]
            )),
            "{}",
            error.message()
        );
    }
    tx.rollback().await.unwrap();

    // a statement the guard lets through is refused by the READ ONLY transaction (25006), even for
    // a role that holds the privilege (the read role gets USAGE on sequences here)
    h.read_only_role().await.unwrap();
    let role = format!("ro_{}", h.db().name());
    h.db()
        .client()
        .await
        .unwrap()
        .batch_execute(&format!(
            "GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO \"{role}\""
        ))
        .await
        .unwrap();
    let mut tx = orm
        .begin_opts(read_opts("test_access.some_obj.read"))
        .await
        .unwrap();
    let error = tx
        .query(
            "SELECT nextval(pg_get_serial_sequence('test_access_some_obj', 'id'))",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), guard("write through SQL"));
    assert_eq!(error.sqlstate(), Some("25006"));
    h.cleanup().await.unwrap();
}

/// `check_read_pool` tells a read-only role from a writable one. `default_transaction_read_only`
/// is a session default any session can change: it is reported, but it is not a boundary.
#[tokio::test]
async fn check_read_pool_tells_a_read_only_role_from_a_writable_one() {
    let h = access_harness(open_policy()).await;
    let read_only = routed_orm(&h, open_policy()).await;
    let status = read_only.check_read_pool().await.unwrap();
    assert!(!status.in_recovery);
    assert!(!status.can_write);
    assert!(status.is_read_only());

    let owner_with_default = pool_for(
        h.db().config(),
        "-c application_name=erp_read_pool -c default_transaction_read_only=on",
    );
    let orm = routed_builder_on(&h, open_policy(), owner_with_default)
        .await
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let status = orm.check_read_pool().await.unwrap();
    assert!(status.default_read_only);
    assert!(
        status.can_write,
        "the owning role writes whatever the session default says"
    );
    assert!(!status.is_read_only());
    h.cleanup().await.unwrap();
}

/// A read pool whose role can write is refused before the first route (typed error, counted), or
/// runs the read on the primary under `ReadPoolFailure::Primary`, counted with its cause.
#[tokio::test]
async fn a_writable_read_pool_is_refused_before_the_first_route() {
    let h = access_harness(open_policy()).await;
    let orm = routed_builder_on(&h, open_policy(), writable_pool(&h))
        .await
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let error = orm
        .begin_opts(read_opts("test.read"))
        .await
        .err()
        .expect("begin must fail");
    assert!(error.is_read_pool_unavailable(), "{error}");
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "read replica unavailable");
    let cause = std::error::Error::source(&error)
        .expect("the cause is kept")
        .to_string();
    assert!(
        cause.contains("neither a standby nor a login whose roles lack write privileges"),
        "{cause}"
    );
    let stats = orm.routing_stats();
    assert_eq!(
        (
            stats.read_pool_served,
            stats.read_pool_errors,
            stats.read_pool_unavailable
        ),
        (0, 1, 0)
    );

    let orm = routed_builder_on(&h, open_policy(), writable_pool(&h))
        .await
        .trust_caller_resolutions()
        .read_pool_failure(ReadPoolFailure::Primary)
        .build()
        .unwrap();
    let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::Primary);
    assert_eq!(app_name(&mut tx).await, PRIMARY);
    let stats = orm.routing_stats();
    assert_eq!(
        (
            stats.read_pool_served,
            stats.read_pool_errors,
            stats.read_pool_unavailable
        ),
        (0, 0, 1)
    );
    h.cleanup().await.unwrap();
}

/// A read pool on another database is a configuration error: typed, counted, never a route and
/// never a fallback, whatever the failure setting says. A pool built without the version hook
/// goes through the same check, and an unsupported server is its own error.
#[tokio::test]
async fn a_read_pool_on_another_database_is_refused_and_never_falls_back() {
    let h = access_harness(open_policy()).await;
    let other = access_harness(open_policy()).await;
    let pool = read_only_pool(&other, &format!("-c application_name={READ_POOL}")).await;
    for failure in [ReadPoolFailure::Fail, ReadPoolFailure::Primary] {
        let orm = routed_builder_on(&h, open_policy(), pool.clone())
            .await
            .trust_caller_resolutions()
            .read_pool_failure(failure)
            .build()
            .unwrap();
        let error = orm
            .begin_opts(read_opts("test.read"))
            .await
            .err()
            .expect("begin must fail");
        assert_eq!(error.kind(), ErrorKind::Programming, "{failure:?}");
        let expected = format!(
            "the read pool does not serve the primary's database (database {:?} vs {:?}): \
             refusing to route reads there",
            other.db().name(),
            h.db().name()
        );
        assert_eq!(error.message(), expected, "{failure:?}");
        assert_eq!(orm.check_read_pool().await.unwrap_err().message(), expected);
        let stats = orm.routing_stats();
        assert_eq!(
            (
                stats.read_pool_served,
                stats.read_pool_errors,
                stats.read_pool_unavailable
            ),
            (0, 1, 0),
            "{failure:?}"
        );
    }
    other.cleanup().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_read_pool_on_an_unsupported_server_never_falls_back() {
    use erp_orm::pool::UnsupportedPostgresError;
    let h = access_harness(open_policy()).await;
    let unsupported = UnsupportedPostgresError::new(170_000, "17.0");
    let message = unsupported.to_hook_message();
    let manager = Manager::from_config(
        h.db().config(),
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool = Pool::builder(manager)
        .max_size(2)
        .runtime(Runtime::Tokio1)
        .post_create(Hook::sync_fn(move |_, _| {
            Err(HookError::message(message.clone()))
        }))
        .build()
        .unwrap();
    let orm = routed_builder_on(&h, open_policy(), pool)
        .await
        .trust_caller_resolutions()
        .read_pool_failure(ReadPoolFailure::Primary)
        .build()
        .unwrap();
    let error = orm
        .begin_opts(read_opts("test.read"))
        .await
        .err()
        .expect("begin must fail");
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), unsupported.to_string());
    let stats = orm.routing_stats();
    assert_eq!(
        (stats.read_pool_errors, stats.read_pool_unavailable),
        (1, 0)
    );
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// builder settings

#[tokio::test]
async fn read_pool_settings_without_a_read_pool_are_refused_at_build() {
    let h = access_harness(open_policy()).await;
    let builder =
        || Orm::builder(Arc::clone(h.registry()), h.db().pool().clone()).policy(open_policy());
    let message = "OrmBuilder::read_pool_failure and OrmBuilder::read_pool_max_lag need \
                   OrmBuilder::read_pool: without a read pool they would be silently ignored";
    for orm in [
        builder()
            .read_pool_failure(ReadPoolFailure::Primary)
            .build(),
        builder().read_pool_max_lag(Duration::from_secs(1)).build(),
    ] {
        let error = orm.err().expect("build must fail");
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(error.message(), message);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_zero_max_lag_is_refused_at_build() {
    let h = access_harness(open_policy()).await;
    let error = routed_builder(&h, open_policy(), None)
        .await
        .read_pool_max_lag(Duration::ZERO)
        .build()
        .err()
        .expect("build must fail");
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "OrmBuilder::read_pool_max_lag must be longer than zero"
    );
    h.cleanup().await.unwrap();
}

/// A read-only role of the primary is not in recovery: lag 0, the bound never trips. The lag read
/// rides the `BEGIN` flight (no extra round trip), so a routed read still works with the bound set.
#[tokio::test]
async fn a_max_lag_bound_passes_a_pool_that_is_not_a_standby() {
    let h = access_harness(open_policy()).await;
    let orm = routed_builder(&h, open_policy(), None)
        .await
        .read_pool_max_lag(Duration::from_millis(1))
        .build()
        .unwrap();
    let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(app_name(&mut tx).await, READ_POOL);
    assert_eq!(tx.route(), Route::ReadPool);
    let stats = orm.routing_stats();
    assert_eq!((stats.read_pool_served, stats.replica_lagging), (1, 0));
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// an unreachable read pool

fn dead_pool() -> Pool {
    let mut config = Config::from_str("host=127.0.0.1 port=1 user=nobody dbname=none").unwrap();
    config.connect_timeout(std::time::Duration::from_secs(1));
    pool_for(config, "-c application_name=dead")
}

#[tokio::test]
async fn an_unreachable_read_pool_is_a_typed_error_by_default() {
    let h = access_harness(open_policy()).await;
    let orm = routed_builder_on(&h, open_policy(), dead_pool())
        .await
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let error = orm
        .begin_opts(read_opts("test.read"))
        .await
        .err()
        .expect("begin must fail");
    assert!(error.is_read_pool_unavailable(), "{error}");
    assert_eq!(error.kind(), ErrorKind::Database);
    // no builder API names in a message a client may see; the cause is the source
    assert_eq!(error.message(), "read replica unavailable");
    assert!(std::error::Error::source(&error).is_some());
    let stats = orm.routing_stats();
    assert_eq!(
        (stats.read_pool_errors, stats.read_pool_unavailable),
        (1, 0)
    );
    // a write is unaffected
    orm.begin(SUPERUSER_ID)
        .await
        .unwrap()
        .rollback()
        .await
        .unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_unreachable_read_pool_can_fall_back_to_the_primary_counted() {
    let h = access_harness(open_policy()).await;
    let orm = routed_builder_on(&h, open_policy(), dead_pool())
        .await
        .trust_caller_resolutions()
        .read_pool_failure(ReadPoolFailure::Primary)
        .build()
        .unwrap();
    let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::Primary);
    assert_eq!(app_name(&mut tx).await, PRIMARY);
    let stats = orm.routing_stats();
    assert_eq!(
        (stats.read_pool_errors, stats.read_pool_unavailable),
        (0, 1)
    );
    h.cleanup().await.unwrap();
}

/// Terminate every backend of the read pool and wait until they are gone.
async fn kill_read_backends(h: &Harness) {
    let client = h.db().client().await.unwrap();
    client
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = $1 AND datname = current_database() \
             AND pid <> pg_backend_pid()",
            &[&READ_POOL],
        )
        .await
        .unwrap();
    for _ in 0..250 {
        let alive: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE application_name = $1 AND datname = current_database()",
                &[&READ_POOL],
            )
            .await
            .unwrap()
            .get(0);
        if alive == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the read pool backends did not terminate");
}

/// A read connection that dies between checkout and `BEGIN` (replica restart, failover) follows
/// the failure setting too: a typed error, or the primary, both counted.
#[tokio::test]
async fn a_read_connection_that_dies_after_checkout_follows_the_failure_setting() {
    for failure in [ReadPoolFailure::Fail, ReadPoolFailure::Primary] {
        let h = access_harness(open_policy()).await;
        let orm = routed_builder(&h, open_policy(), None)
            .await
            .read_pool_failure(failure)
            .build()
            .unwrap();
        let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(tx.route(), Route::ReadPool);
        kill_read_backends(&h).await;
        match failure {
            ReadPoolFailure::Fail => {
                let error = tx.query("SELECT 1", &[]).await.unwrap_err();
                assert!(error.is_read_pool_unavailable(), "{error}");
                assert_eq!(error.message(), "read replica unavailable");
                assert!(tx.is_released());
                let stats = orm.routing_stats();
                assert_eq!(
                    (
                        stats.read_pool_errors,
                        stats.read_pool_unavailable,
                        stats.read_pool_served
                    ),
                    (1, 0, 0)
                );
            }
            _ => {
                let rows = tx.query("SELECT 1::int8", &[]).await.unwrap();
                assert_eq!(rows[0].get::<_, i64>(0), 1);
                assert_eq!(tx.route(), Route::Primary);
                assert_eq!(app_name(&mut tx).await, PRIMARY);
                let stats = orm.routing_stats();
                assert_eq!(
                    (
                        stats.read_pool_errors,
                        stats.read_pool_unavailable,
                        stats.read_pool_served
                    ),
                    (0, 1, 0)
                );
            }
        }
        h.cleanup().await.unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// replica lag guard

/// Export a snapshot from a holding transaction: the state of the database now.
async fn export_snapshot(h: &Harness) -> (erp_testkit::deadpool_postgres::Object, String) {
    let client = h.db().client().await.unwrap();
    client
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    let id: String = client
        .query_one("SELECT pg_export_snapshot()", &[])
        .await
        .unwrap()
        .get(0);
    (client, id)
}

/// The process learns of a primary change through a primary transaction (as the listener would).
async fn apply_on_primary(orm: &Orm) {
    let mut write = orm.begin(SUPERUSER_ID).await.unwrap();
    write.query("SELECT 1", &[]).await.unwrap();
    write.rollback().await.unwrap();
}

/// A replica snapshot older than a generation this process applied: the routed transaction is
/// discarded and runs on the primary, counted; no regression error, no full refresh. A snapshot
/// that is current stays on the read pool. Registry, policy, membership and a named cache are all
/// covered by the same comparison.
#[tokio::test]
async fn a_replica_behind_the_applied_generations_runs_on_the_primary() {
    for key in [
        SignalKey::Policy,
        SignalKey::Registry,
        SignalKey::Membership,
        SignalKey::cache("rtt.cache").unwrap(),
    ] {
        let h = access_harness(open_policy()).await;
        let orm = routed_orm(&h, open_policy()).await;

        // the replica's state: taken before the bump
        let (holder, old) = export_snapshot(&h).await;
        bump(&h, &key).await;
        apply_on_primary(&orm).await;
        let applied = orm.signal().unwrap().applied();
        let before = orm.signal().unwrap().stats();
        assert_eq!(before.applies, 1, "{key:?}");

        // the lagging replica
        let mut tx = orm
            .begin_opts(read_opts("test.read").import_snapshot(old.clone()))
            .await
            .unwrap();
        assert_eq!(tx.route(), Route::ReadPool);
        assert_eq!(app_name(&mut tx).await, PRIMARY, "{key:?}");
        assert_eq!(tx.route(), Route::Primary);
        tx.rollback().await.unwrap();
        let stats = orm.routing_stats();
        assert_eq!(
            (stats.read_pool_served, stats.replica_behind),
            (0, 1),
            "{key:?}: {stats:?}"
        );
        let signal = orm.signal().unwrap().stats();
        assert_eq!(signal.regressions, 0, "{key:?}: no false regression");
        assert_eq!(
            signal.applies, before.applies,
            "{key:?}: nothing re-applied"
        );
        assert_eq!(orm.signal().unwrap().applied(), applied);

        // a current replica stays on the read pool
        let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(app_name(&mut tx).await, READ_POOL);
        tx.rollback().await.unwrap();
        assert_eq!(orm.routing_stats().read_pool_served, 1);
        holder.batch_execute("ROLLBACK").await.unwrap();
        h.cleanup().await.unwrap();
    }
}

/// A replica ahead of the process is normal: the generation it reads is applied, the read stays
/// on the read pool, nothing is counted as behind.
#[tokio::test]
async fn a_replica_ahead_of_the_process_is_applied_and_stays_routed() {
    for key in [
        SignalKey::Policy,
        SignalKey::Membership,
        SignalKey::cache("rtt.cache").unwrap(),
    ] {
        let h = access_harness(open_policy()).await;
        let orm = routed_orm(&h, open_policy()).await;
        let known = orm.signal().unwrap().applied().get(&key);
        bump(&h, &key).await;
        let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(app_name(&mut tx).await, READ_POOL, "{key:?}");
        assert_eq!(tx.route(), Route::ReadPool);
        tx.rollback().await.unwrap();
        let stats = orm.routing_stats();
        assert_eq!(
            (stats.read_pool_served, stats.replica_behind),
            (1, 0),
            "{key:?}"
        );
        assert_eq!(
            orm.signal().unwrap().applied().get(&key),
            known + 1,
            "{key:?}"
        );
        assert_eq!(orm.signal().unwrap().stats().applies, 1);
        h.cleanup().await.unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// membership on a lagging replica (the resolver runs on the routed connection)

/// Reads `res_users.access_group_id` as the subject's one group, and remembers on which
/// connection (`application_name`) each resolution ran.
#[derive(Clone, Default)]
struct GroupResolver {
    ran_on: Arc<Mutex<Vec<String>>>,
}

fn group(id: i64) -> GroupId {
    GroupId::new(id).unwrap()
}

fn group_of(row: &Row) -> Vec<GroupId> {
    row.get::<_, Option<i64>>(1)
        .map(|g| vec![group(g)])
        .unwrap_or_default()
}

impl PrincipalResolver for GroupResolver {
    fn statement(&self, hint: &PrincipalHint) -> erp_sdk::Result<ResolverStatement> {
        Ok(ResolverStatement::new(
            "SELECT id, access_group_id, current_setting('application_name') FROM res_users \
             WHERE id = $1::int8"
                .to_owned(),
            vec![Param::Scalar(Scalar::Int8(hint.subject().get()))],
        ))
    }

    fn finish(
        &self,
        hint: &PrincipalHint,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> erp_sdk::Result<Principal> {
        let row = rows.first().ok_or_else(|| {
            erp_sdk::Error::access(format!("User {} does not exist", hint.subject()))
        })?;
        self.ran_on.lock().unwrap().push(row.get::<_, String>(2));
        let groups = policy.groups(&group_of(row))?;
        let resolution = Resolution::in_id_order(groups, [company(1)], NonZeroU64::MIN)?;
        Ok(PrincipalBuilder::user(hint.subject())
            .resolution(resolution)
            .companies(vec![company(1)])
            .build()?)
    }

    fn re_resolve<'a>(
        &'a self,
        tx: &'a mut Transaction,
        subjects: &'a [UserId],
    ) -> erp_orm::BoxFuture<'a, erp_sdk::Result<Vec<Resolution>>> {
        Box::pin(async move {
            let ids: Vec<i64> = subjects.iter().map(|s| s.get()).collect();
            let rows = tx
                .query_typed(
                    "SELECT id, access_group_id FROM res_users WHERE id = ANY($1::int8[])",
                    &[(&ids, erp_testkit::tokio_postgres::types::Type::INT8_ARRAY)],
                )
                .await?;
            subjects
                .iter()
                .map(|s| {
                    let row = rows
                        .iter()
                        .find(|r| r.get::<_, i64>(0) == s.get())
                        .ok_or_else(|| {
                            erp_sdk::Error::access(format!("User {s} does not exist"))
                        })?;
                    Ok(Resolution::in_id_order(
                        GroupSet::new(group_of(row)),
                        [company(1)],
                        NonZeroU64::MIN,
                    )?)
                })
                .collect()
        })
    }
}

/// Members of group 1 read everything; nobody else reads anything.
fn group_policy() -> DomainPolicy {
    DomainPolicy::builder(Otherwise::Deny)
        .group_rule(OBJ, Operation::Read, group(1), Domain::TRUE)
        .needs_resolved_principals()
        .build()
        .unwrap()
}

/// What a user reads of the objects.
#[derive(Debug, PartialEq)]
struct Outcome {
    found: Result<Vec<i64>, (ErrorKind, String)>,
    check: Result<(), (ErrorKind, String)>,
}

async fn outcome(tx: &mut Transaction, ids: &[i64]) -> Outcome {
    let found = tx
        .env()
        .records(OBJ)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .map(|found| found.ids())
        .map_err(|e| (e.kind(), e.message().to_owned()));
    let all = tx
        .env()
        .records(OBJ)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap();
    let check = all
        .check_access(tx, Operation::Read)
        .await
        .map_err(|e| (e.kind(), e.message().to_owned()));
    Outcome { found, check }
}

fn member_opts() -> BeginOptions {
    BeginOptions::new(UserId::new(2).unwrap()).read_only("test_access.some_obj.read")
}

/// The user lost the group on the primary; the replica has not replayed it. The routed
/// transaction resolves the principal on the replica, which still has the membership: without the
/// `membership` generation the guard sees nothing behind (policy, registry and caches are
/// current) and the user keeps reading until the replica catches up. With it the transaction is
/// discarded and decided on the primary: same refusal as the primary gives.
#[tokio::test]
async fn a_removed_membership_is_not_served_from_a_lagging_replica() {
    let resolver = GroupResolver::default();
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(group_policy())
        .resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    {
        let mut tx = h.begin().await.unwrap();
        tx.batch(
            "INSERT INTO test_access_group (id, name) VALUES (1, 'g1'); \
             INSERT INTO res_users (id, name, login, active, access_group_id) \
             VALUES (2, 'demo', 'demo', true, 1); \
             SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let ids = objs(&h).await;
    let read = read_only_pool(&h, &format!("-c application_name={READ_POOL}")).await;
    let orm = routed_builder_on(&h, group_policy(), read)
        .await
        .resolver(resolver.clone())
        .build()
        .unwrap();

    // while the user is a member, the primary and the replica agree; the resolution ran on the
    // connection the transaction ran on (not on a caller-supplied resolution)
    let mut tx = orm
        .begin_opts(BeginOptions::new(UserId::new(2).unwrap()))
        .await
        .unwrap();
    let member_on_primary = outcome(&mut tx, &ids).await;
    tx.rollback().await.unwrap();
    assert_eq!(member_on_primary.found, Ok(ids.clone()));
    assert!(member_on_primary.check.is_ok());
    let mut tx = orm.begin_opts(member_opts()).await.unwrap();
    let member_on_replica = outcome(&mut tx, &ids).await;
    assert_eq!(tx.route(), Route::ReadPool, "{:?}", orm.routing_stats());
    tx.rollback().await.unwrap();
    assert_eq!(member_on_replica, member_on_primary);
    assert_eq!(
        *resolver.ran_on.lock().unwrap(),
        [PRIMARY, READ_POOL],
        "the resolver ran on the routed connection"
    );
    resolver.ran_on.lock().unwrap().clear();

    // the replica's state before the revocation
    let (holder, old) = export_snapshot(&h).await;
    h.db()
        .client()
        .await
        .unwrap()
        .execute(
            "UPDATE res_users SET access_group_id = NULL WHERE id = 2",
            &[],
        )
        .await
        .unwrap();
    // this process learns of it through the primary
    apply_on_primary(&orm).await;
    let mut tx = orm
        .begin_opts(BeginOptions::new(UserId::new(2).unwrap()))
        .await
        .unwrap();
    let revoked_on_primary = outcome(&mut tx, &ids).await;
    tx.rollback().await.unwrap();
    assert_ne!(revoked_on_primary.found, Ok(ids.clone()));
    let refused = revoked_on_primary.check.clone().unwrap_err();
    assert_eq!(refused.0, ErrorKind::Access, "{}", refused.1);
    resolver.ran_on.lock().unwrap().clear();

    // the lagging replica: decided like the primary, and the stale resolution never ran there
    let mut tx = orm
        .begin_opts(member_opts().import_snapshot(old))
        .await
        .unwrap();
    // (the principal is resolved by begin_opts, so the guard has already run)
    let lagging = outcome(&mut tx, &ids).await;
    assert_eq!(tx.route(), Route::Primary);
    tx.rollback().await.unwrap();
    assert_eq!(lagging, revoked_on_primary);
    assert_eq!(*resolver.ran_on.lock().unwrap(), [PRIMARY]);
    let stats = orm.routing_stats();
    assert_eq!((stats.replica_behind, stats.read_pool_served), (1, 1));

    // a replica that has replayed the revocation refuses on its own
    let mut tx = orm.begin_opts(member_opts()).await.unwrap();
    let caught_up = outcome(&mut tx, &ids).await;
    assert_eq!(tx.route(), Route::ReadPool);
    tx.rollback().await.unwrap();
    assert_eq!(caught_up, revoked_on_primary);
    holder.batch_execute("ROLLBACK").await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// failures while leaving the read pool

/// A failed `ROLLBACK` on the replica is logged and counted, the connection is detached from the
/// pool, and the transaction still ends up on the primary.
#[tokio::test]
async fn a_failed_rollback_on_the_replica_is_counted_and_the_connection_detached() {
    let h = access_harness(open_policy()).await;
    let read = read_only_pool(&h, &format!("-c application_name={READ_POOL}")).await;
    let orm = routed_builder_on(&h, open_policy(), read.clone())
        .await
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let (holder, old) = export_snapshot(&h).await;
    bump(&h, &SignalKey::Policy).await;
    apply_on_primary(&orm).await;

    let mut tx = orm
        .begin_opts(
            read_opts("test.read")
                .import_snapshot(old)
                .break_before_reroute(),
        )
        .await
        .unwrap();
    assert_eq!(read.status().size, 1);
    assert_eq!(app_name(&mut tx).await, PRIMARY);
    assert_eq!(tx.route(), Route::Primary);
    let stats = orm.routing_stats();
    assert_eq!(
        (stats.replica_behind, stats.rollback_failures),
        (1, 1),
        "{stats:?}"
    );
    assert_eq!(read.status().size, 0, "the broken connection left the pool");
    tx.rollback().await.unwrap();
    holder.batch_execute("ROLLBACK").await.unwrap();
    h.cleanup().await.unwrap();
}

/// Dropping the future of a reroute at any point leaves a transaction that is released or on the
/// primary, never open on the replica's older snapshot.
#[tokio::test]
async fn a_cancelled_reroute_never_leaves_the_transaction_on_the_replica() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let (holder, old) = export_snapshot(&h).await;
    bump(&h, &SignalKey::Policy).await;
    apply_on_primary(&orm).await;
    let mut cancelled = 0;
    for micros in (20..4000).step_by(60) {
        let mut tx = orm
            .begin_opts(read_opts("test.read").import_snapshot(old.clone()))
            .await
            .unwrap();
        let first = tokio::time::timeout(
            Duration::from_micros(micros),
            tx.query("SELECT 1::int8", &[]),
        )
        .await;
        if first.is_ok() {
            // finished within the budget: it ran on the primary
            assert_eq!(tx.route(), Route::Primary);
            continue;
        }
        cancelled += 1;
        // the transaction was kept after its future was dropped: it either works on the primary
        // or is refused, never serves from the replica's snapshot
        match tx
            .query("SELECT current_setting('application_name')", &[])
            .await
        {
            Ok(rows) => assert_eq!(rows[0].get::<_, String>(0), PRIMARY, "{micros}us"),
            Err(error) => assert!(
                tx.is_released() || tx.route() == Route::Primary,
                "{micros}us: {error}"
            ),
        }
    }
    assert!(cancelled > 0, "no trial was cancelled mid-flight");
    holder.batch_execute("ROLLBACK").await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// the listener

/// While a started signal listener is down (or catching up) this process may not know of
/// changes: routed reads go to the primary, counted. With the lag bounded (these tests set it), or once it is up, they
/// are served by the read pool.
#[tokio::test]
async fn reads_use_the_primary_while_the_signal_listener_is_down() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let mut dead = Config::from_str("host=127.0.0.1 port=1 user=nobody dbname=none").unwrap();
    dead.connect_timeout(Duration::from_secs(1));
    let handle = orm
        .spawn_signal_listener(dead, NoTls, ListenerOptions::default())
        .unwrap();
    let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::Primary);
    tx.open().await.unwrap();
    let stats = orm.routing_stats();
    assert_eq!((stats.listener_down, stats.read_pool_served), (1, 0));
    drop(tx);
    // no listener expected any more: back to the read pool
    drop(handle);
    let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::ReadPool);
    tx.open().await.unwrap();
    drop(tx);

    // a listener that is up does not hold reads back
    let handle = orm
        .spawn_signal_listener(h.db().config(), NoTls, ListenerOptions::default())
        .unwrap();
    for _ in 0..250 {
        if orm.signal().unwrap().stats().listener_up {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(orm.signal().unwrap().stats().listener_up);
    let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::ReadPool);
    tx.open().await.unwrap();
    assert_eq!(orm.routing_stats().listener_down, 1);
    drop(tx);
    handle.shutdown().await;
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// units of work and the test seam

/// A transaction keeps the unit of work it was opened in: used later in a task that has no scope
/// it still goes to the primary after a write began, and `spawn_in_unit_of_work` carries the scope
/// into a spawned task.
#[tokio::test]
async fn a_transaction_keeps_its_unit_of_work_into_another_task() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    unit_of_work(async {
        let read = orm.begin_opts(read_opts("test.read")).await.unwrap();
        assert_eq!(read.route(), Route::ReadPool);
        let write = orm.begin(SUPERUSER_ID).await.unwrap();
        // first used in a task without a scope, after the write began
        let used = tokio::spawn(async move {
            let mut read = read;
            let name = app_name(&mut read).await;
            (name, read.route())
        })
        .await
        .unwrap();
        assert_eq!(used, (PRIMARY.to_owned(), Route::Primary));
        drop(write);

        let carried = orm.clone();
        let kept = spawn_in_unit_of_work(async move {
            carried
                .begin_opts(read_opts("test.read"))
                .await
                .unwrap()
                .route()
        })
        .await
        .unwrap();
        assert_eq!(
            kept,
            Route::Primary,
            "the spawned task joined the unit of work"
        );
        let carried = orm.clone();
        let lost = tokio::spawn(async move {
            carried
                .begin_opts(read_opts("test.read"))
                .await
                .unwrap()
                .route()
        })
        .await
        .unwrap();
        assert_eq!(lost, Route::ReadPool, "a plain spawn does not");
    })
    .await;
    h.cleanup().await.unwrap();
}

/// The snapshot seam only exists for the read route.
#[tokio::test]
async fn the_snapshot_seam_is_refused_off_the_read_route() {
    let h = access_harness(open_policy()).await;
    let message = "BeginOptions::import_snapshot needs a transaction routed to the read pool";
    for orm in [h.orm().clone(), routed_orm(&h, open_policy()).await] {
        let options = read_opts("test.read")
            .import_snapshot("00000000-1")
            .on_primary();
        let error = orm
            .begin_opts(options)
            .await
            .err()
            .expect("begin must fail");
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(error.message(), message);
    }
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// RPC

/// A `read`-effect call routes through the ORM, a `write` call does not; each call is one unit of
/// work (no unscoped read), and the registry-freshness check of a 404 does not use the replica.
#[tokio::test]
async fn rpc_read_calls_route_and_write_calls_do_not() {
    let h = rpc::harness_with_read_pool().await;
    assert!(h.orm().routing_stats().read_pool);
    let before = h.orm().routing_stats().read_pool_served;

    let r = rpc::call(&h, "/json/2/test_rpc.effects/probe", "{}", rpc::alice()).await;
    rpc::ok(&r);
    let after_read = h.orm().routing_stats().read_pool_served;
    assert!(after_read > before, "{before} -> {after_read}");
    assert_eq!(
        h.orm().routing_stats().unscoped,
        0,
        "a call is a unit of work"
    );

    let r = rpc::call(
        &h,
        "/json/2/test_rpc.effects/probe_write",
        "{}",
        rpc::alice(),
    )
    .await;
    rpc::ok(&r);
    assert_eq!(
        h.orm().routing_stats().read_pool_served,
        after_read,
        "a write is not routed"
    );

    let r = rpc::call(&h, "/json/2/test_rpc.nothing/probe", "{}", rpc::alice()).await;
    assert_eq!(r.status(), 404, "{}", String::from_utf8_lossy(r.body()));
    assert_eq!(
        h.orm().routing_stats().read_pool_served,
        after_read,
        "the freshness check of the 404 runs on the primary"
    );
    h.cleanup().await.unwrap();
}

/// The same restricted reads answer the same through a read pool and without one, company
/// context included.
#[tokio::test]
async fn rpc_answers_equal_the_primary_ones() {
    let with = rpc::harness_with_read_pool().await;
    let without = rpc::harness().await;
    let mut mine = (0, 0);
    for (h, id) in [(&with, &mut mine.0), (&without, &mut mine.1)] {
        *id = rpc::effect(h, "mine", 1).await;
        rpc::effect(h, "other company", 2).await;
    }
    assert_eq!(mine.0, mine.1);
    let bodies = [
        r#"{"domain": [], "fields": ["name"]}"#,
        r#"{"domain": [], "fields": ["name"], "context": {"allowed_company_ids": [2]}}"#,
        r#"{"domain": [], "fields": ["name"], "context": {"allowed_company_ids": [2, 1]}}"#,
        r#"{"domain": [["name", "=", "mine"]], "fields": ["name", "company"]}"#,
    ];
    for body in bodies {
        let a = rpc::call(
            &with,
            "/json/2/test_rpc.effects/search_read",
            body,
            rpc::alice(),
        )
        .await;
        let b = rpc::call(
            &without,
            "/json/2/test_rpc.effects/search_read",
            body,
            rpc::alice(),
        )
        .await;
        assert_eq!(a.status(), b.status(), "{body}");
        assert_eq!(rpc::json(&a), rpc::json(&b), "{body}");
    }
    let r = rpc::call(
        &with,
        "/json/2/test_rpc.effects/search_read",
        bodies[0],
        rpc::alice(),
    )
    .await;
    assert_eq!(
        rpc::ok(&r),
        serde_json::json!([{"id": mine.0, "name": "mine"}])
    );
    assert!(
        with.orm().routing_stats().read_pool_served >= bodies.len() as u64,
        "the reads were served by the read pool"
    );
    with.cleanup().await.unwrap();
    without.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// round 2 of the security review (SEC2-1 .. SEC2-8)

/// SEC2-2: with a read pool, a model registered as a membership source without a membership
/// trigger is refused at build, naming the model; res.users declares the trigger and passes.
#[tokio::test]
async fn a_membership_source_without_a_trigger_is_refused_with_a_read_pool() {
    static NO_TRIGGER: erp_orm::PolicySourceWrite =
        erp_orm::PolicySourceWrite::new(OBJ, erp_orm::SourceKind::Membership);
    let h = access_harness(open_policy()).await;
    let error = routed_builder(&h, open_policy(), None)
        .await
        .policy_sources([&NO_TRIGGER])
        .build()
        .err()
        .expect("build must fail");
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the model 'test_access.some_obj' is registered as a membership source \
         (SourceKind::Membership) but declares no membership signal trigger (`signal_triggers = \
         [(\"<name>\", \"membership\")]`): with a read pool configured a replica that has not \
         replayed a change to it could not be detected"
    );
    routed_builder(&h, open_policy(), None)
        .await
        .build()
        .unwrap();
    h.cleanup().await.unwrap();
}

/// SEC2-3: a process whose signal listener is not running reads from the primary, counted,
/// unless the replay lag is bounded.
#[tokio::test]
async fn a_process_without_a_listener_reads_from_the_primary_unless_the_lag_is_bounded() {
    let h = access_harness(open_policy()).await;
    let read = read_only_pool(&h, &format!("-c application_name={READ_POOL}")).await;
    let unbounded = routed_builder_with_lag(&h, open_policy(), read.clone(), None)
        .await
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let mut tx = unbounded.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::Primary);
    tx.open().await.unwrap();
    let stats = unbounded.routing_stats();
    assert_eq!((stats.listener_down, stats.read_pool_served), (1, 0));
    drop(tx);

    let bounded = routed_builder_with_lag(&h, open_policy(), read, Some(Duration::from_secs(30)))
        .await
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let mut tx = bounded.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(tx.route(), Route::ReadPool);
    tx.open().await.unwrap();
    let stats = bounded.routing_stats();
    assert_eq!((stats.listener_down, stats.read_pool_served), (0, 1));
    h.cleanup().await.unwrap();
}

/// SEC2-4: each connection of the read pool is verified on its first routed use, so a connection
/// opened later (after a failover, say) is checked too: here the login gains write privileges
/// after the first connection passed, and the second connection is refused.
#[tokio::test]
async fn every_read_pool_connection_is_verified_not_only_the_first() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let first = orm.begin_opts(read_opts("test.read")).await.unwrap();
    assert_eq!(first.route(), Route::ReadPool);
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "GRANT INSERT ON erp_signal_generation TO \"ro_{}\"",
            h.db().name()
        ))
        .await
        .unwrap();
    // `first` still holds its connection: this one is new
    let error = orm
        .begin_opts(read_opts("test.read"))
        .await
        .err()
        .expect("the second connection must fail the check");
    assert!(error.is_read_pool_unavailable(), "{error}");
    let cause = std::error::Error::source(&error).unwrap().to_string();
    assert_eq!(
        cause,
        "the read pool is neither a standby nor a login whose roles lack write privileges on the \
         ORM's tables (default_transaction_read_only is not a boundary, and neither is a SET \
         ROLE the session can undo)"
    );
    assert_eq!(orm.routing_stats().read_pool_errors, 1);
    drop(first);
    h.cleanup().await.unwrap();
}

/// SEC2-5: a pool that logs in as the owner and switches with `-c role=<read-only>` is refused:
/// the session can switch back. Only a real read-only login passes.
#[tokio::test]
async fn an_owner_login_with_a_role_switch_is_not_read_only() {
    let h = access_harness(open_policy()).await;
    install_signal(&h).await;
    h.read_only_role().await.unwrap();
    let switched = pool_for(
        h.db().config(),
        &format!(
            "-c application_name={READ_POOL} -c role=ro_{}",
            h.db().name()
        ),
    );
    let orm = routed_builder_on(&h, open_policy(), switched)
        .await
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let status = orm.check_read_pool().await.unwrap();
    assert!(status.can_write, "the login can switch back: {status:?}");
    let error = orm
        .begin_opts(read_opts("test.read"))
        .await
        .err()
        .expect("begin must fail");
    assert!(error.is_read_pool_unavailable(), "{error}");
    assert_eq!(orm.routing_stats().read_pool_served, 0);
    h.cleanup().await.unwrap();
}

/// SEC2-6: routed reads that begin at the same time do not collide with each other.
#[tokio::test]
async fn concurrent_routed_reads_do_not_collide() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let orm = orm.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..20 {
                let mut tx = orm.begin_opts(read_opts("test.read")).await.unwrap();
                tx.open().await.unwrap();
                assert_eq!(tx.route(), Route::ReadPool);
                tokio::task::yield_now().await;
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let stats = orm.routing_stats();
    assert_eq!(stats.read_pool_served, 60, "{stats:?}");
    assert_eq!((stats.refresh_running, stats.replica_behind), (0, 0));
    h.cleanup().await.unwrap();
}

/// SEC2-8: the primary failing while the read pool is verified is the primary's error, not "read
/// replica unavailable", and not counted as a read pool failure.
#[tokio::test]
async fn a_primary_failure_during_verification_is_not_a_read_pool_failure() {
    let h = access_harness(open_policy()).await;
    let baseline = install_signal(&h).await;
    let read = read_only_pool(&h, &format!("-c application_name={READ_POOL}")).await;
    let orm = Orm::builder(Arc::clone(h.registry()), dead_pool())
        .policy(open_policy())
        .signal(Signal::new(baseline).with_refresh(InPlace))
        .read_pool(read)
        .read_pool_max_lag(TEST_MAX_LAG)
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let error = orm
        .begin_opts(read_opts("test.read"))
        .await
        .err()
        .expect("begin must fail");
    assert!(!error.is_read_pool_unavailable(), "{error}");
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "could not get a database connection");
    let stats = orm.routing_stats();
    assert_eq!(
        (stats.read_pool_errors, stats.read_pool_unavailable),
        (0, 0)
    );
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// integration review fixes (fix round 1)

/// F1: a replay lag read that cannot be understood ends the transaction as released, not Open on
/// the replica without a guard. The unreadable and the missing row both give the lag read's own
/// error.
#[tokio::test]
async fn an_unreadable_lag_read_releases_the_routed_transaction() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    for (sql, message) in [
        (
            "SELECT 1::int8, 1::float8",
            "could not read the replica's replay lag",
        ),
        (
            "SELECT true, 1::float8 WHERE false",
            "the replica's replay lag read returned no row",
        ),
    ] {
        let mut tx = orm
            .begin_opts(read_opts("test.read").lag_read_sql(sql))
            .await
            .unwrap();
        assert_eq!(tx.route(), Route::ReadPool);
        let error = tx.open().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database, "{sql}");
        assert_eq!(error.message(), message, "{sql}");
        assert!(tx.is_released(), "{sql}: left open on the replica");
    }
    h.cleanup().await.unwrap();
}

/// F5: a lag read the server refuses is reported as a failed lag read, not as a failed signal
/// read.
#[tokio::test]
async fn a_failed_lag_read_is_not_reported_as_a_signal_read() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let mut tx = orm
        .begin_opts(read_opts("test.read").lag_read_sql("SELECT 1/0, 1"))
        .await
        .unwrap();
    let error = tx.open().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert!(
        error
            .message()
            .starts_with("could not read the replica's replay lag: "),
        "{}",
        error.message()
    );
    assert!(!error.message().contains("signal"), "{}", error.message());
    assert!(tx.is_released());
    h.cleanup().await.unwrap();
}

/// F8: when the lag is over the bound, the rows the resolver read on the replica are dropped and
/// the primary resolves again: the resolver finishes once, on the primary.
#[tokio::test]
async fn a_lag_over_the_bound_discards_the_resolver_rows_of_the_replica() {
    let resolver = GroupResolver::default();
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(group_policy())
        .resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    {
        let mut tx = h.begin().await.unwrap();
        tx.batch(
            "INSERT INTO test_access_group (id, name) VALUES (1, 'g1'); \
             INSERT INTO res_users (id, name, login, active, access_group_id) \
             VALUES (2, 'demo', 'demo', true, 1); \
             SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let read = read_only_pool(&h, &format!("-c application_name={READ_POOL}")).await;
    let orm = routed_builder_on(&h, group_policy(), read)
        .await
        .resolver(resolver.clone())
        .build()
        .unwrap();
    let mut tx = orm
        .begin_opts(member_opts().lag_read_sql("SELECT true, 1000::float8"))
        .await
        .unwrap();
    tx.open().await.unwrap();
    assert_eq!(tx.route(), Route::Primary);
    assert_eq!(app_name(&mut tx).await, PRIMARY);
    tx.rollback().await.unwrap();
    assert_eq!(
        *resolver.ran_on.lock().unwrap(),
        [PRIMARY],
        "the replica's resolver rows were used"
    );
    let stats = orm.routing_stats();
    assert_eq!((stats.replica_lagging, stats.read_pool_served), (1, 0));
    h.cleanup().await.unwrap();
}

/// A resolver whose statement ends the connection it runs on (through a function that may
/// terminate the backend), to show a connection lost in the resolver's answer.
struct KillResolver;

impl PrincipalResolver for KillResolver {
    fn statement(&self, _hint: &PrincipalHint) -> erp_sdk::Result<ResolverStatement> {
        Ok(ResolverStatement::new(
            "SELECT rtt_kill()".to_owned(),
            vec![],
        ))
    }

    fn finish(
        &self,
        _hint: &PrincipalHint,
        _rows: &[Row],
        _policy: &dyn AccessPolicy,
    ) -> erp_sdk::Result<Principal> {
        Err(erp_sdk::Error::programming(
            "the resolver's connection was ended; finish must not run",
        ))
    }

    fn re_resolve<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _subjects: &'a [UserId],
    ) -> erp_orm::BoxFuture<'a, erp_sdk::Result<Vec<Resolution>>> {
        Box::pin(async { Err(erp_sdk::Error::programming("not used")) })
    }
}

/// A routed read for a user the resolver has to resolve, opened.
async fn member_read(orm: &Orm) -> erp_sdk::Result<()> {
    let mut tx = orm.begin_opts(member_opts()).await?;
    tx.open().await
}

/// F3 (and F4, which picks the error by the same position): a connection lost in the resolver's
/// answer is a dead read connection and follows the failure setting, as when the loss shows in
/// `BEGIN`.
#[tokio::test]
async fn a_read_connection_lost_in_the_resolvers_answer_follows_the_failure_setting() {
    for failure in [ReadPoolFailure::Fail, ReadPoolFailure::Primary] {
        let h = access_harness(open_policy()).await;
        let owner = h.db().client().await.unwrap();
        owner
            .batch_execute(
                "CREATE FUNCTION rtt_kill() RETURNS bool LANGUAGE sql SECURITY DEFINER AS \
                 $$ SELECT pg_terminate_backend(pg_backend_pid()) $$",
            )
            .await
            .unwrap();
        let read_resolver = routed_builder(&h, open_policy(), None)
            .await
            .read_pool_failure(failure)
            .resolver(KillResolver);
        let orm = read_resolver.build().unwrap();
        match failure {
            ReadPoolFailure::Fail => {
                let error = member_read(&orm).await.expect_err("the read must fail");
                assert!(error.is_read_pool_unavailable(), "{error}");
                assert_eq!(error.message(), "read replica unavailable");
                let stats = orm.routing_stats();
                assert_eq!((stats.read_pool_errors, stats.read_pool_served), (1, 0));
            }
            _ => {
                // the primary resolves again (and the function ends nothing there: it does end
                // the primary backend too, so only the counted decision is checked)
                let _ = member_read(&orm).await;
                assert_eq!(orm.routing_stats().read_pool_unavailable, 1);
            }
        }
        h.cleanup().await.unwrap();
    }
}

/// F6: a replica that has not replayed the signal table yet (`UNDEFINED_TABLE`) is a replica that
/// is behind: the transaction runs on the primary, counted, instead of telling the operator to run
/// `install_signal`. The read here names a table the replica does not have.
#[tokio::test]
async fn a_replica_without_the_signal_table_runs_on_the_primary() {
    let h = access_harness(open_policy()).await;
    let orm = routed_orm(&h, open_policy()).await;
    let mut tx = orm
        .begin_opts(read_opts("test.read").signal_read_sql("SELECT key FROM rtt_not_replayed_yet"))
        .await
        .unwrap();
    assert_eq!(tx.route(), Route::ReadPool);
    tx.open().await.unwrap();
    assert_eq!(tx.route(), Route::Primary);
    assert_eq!(app_name(&mut tx).await, PRIMARY);
    tx.rollback().await.unwrap();
    let stats = orm.routing_stats();
    assert_eq!((stats.replica_behind, stats.read_pool_served), (1, 0));
    h.cleanup().await.unwrap();
}

/// F7: the round trips of a reroute at the socket. A lag over the bound found at `BEGIN`: the
/// replica's start (1) and its `ROLLBACK` (1), then the primary's start (1) = 3. A route changed
/// before anything was sent (the unit of work wrote) costs the primary's start alone.
#[tokio::test]
async fn a_reroute_costs_three_round_trips_or_one_from_idle() {
    let h = access_harness(open_policy()).await;
    install_signal(&h).await;
    let mut read_config = h.read_only_role().await.unwrap();
    read_config.options(format!("-c application_name={READ_POOL}"));
    let read_wire = erp_testkit::WireCounter::start(&read_config).await.unwrap();
    let mut primary_config = h.db().config();
    primary_config.options(format!("-c application_name={PRIMARY}"));
    let primary_wire = erp_testkit::WireCounter::start(&primary_config)
        .await
        .unwrap();
    let baseline = install_signal(&h).await;
    let orm = Orm::builder(Arc::clone(h.registry()), primary_wire.pool(4).unwrap())
        .policy(open_policy())
        .signal(Signal::new(baseline).with_refresh(InPlace))
        .read_pool(read_wire.pool(4).unwrap())
        .read_pool_max_lag(TEST_MAX_LAG)
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let lagging = || read_opts("test.read").lag_read_sql("SELECT true, 1000::float8");
    // warm both connections (the first routed use verifies; statements get prepared)
    for _ in 0..2 {
        let mut warm = orm.begin_opts(lagging()).await.unwrap();
        warm.open().await.unwrap();
        assert_eq!(warm.route(), Route::Primary);
        warm.rollback().await.unwrap();
    }
    let mut tx = orm.begin_opts(lagging()).await.unwrap();
    assert_eq!(tx.route(), Route::ReadPool);
    erp_testkit::assert_round_trips!(read_wire, == 2, {
        erp_testkit::assert_round_trips!(primary_wire, == 1, {
            tx.open().await.unwrap();
        });
    });
    assert_eq!(tx.route(), Route::Primary);
    tx.rollback().await.unwrap();

    // from Idle: the unit of work wrote after this read was opened (the first round warms the
    // second primary connection the write leaves free)
    for round in 0..2 {
        unit_of_work(async {
            let mut read = orm.begin_opts(read_opts("test.read")).await.unwrap();
            assert_eq!(read.route(), Route::ReadPool);
            let mut write = orm.begin(SUPERUSER_ID).await.unwrap();
            write.open().await.unwrap();
            if round == 0 {
                read.open().await.unwrap();
            } else {
                erp_testkit::assert_round_trips!(read_wire, == 0, {
                    erp_testkit::assert_round_trips!(primary_wire, == 1, {
                        read.open().await.unwrap();
                    });
                });
            }
            assert_eq!(read.route(), Route::Primary);
            read.rollback().await.unwrap();
            write.rollback().await.unwrap();
        })
        .await;
    }
    h.cleanup().await.unwrap();
}

/// F2: the early schema lock of a signal sync waits under the schema limits, not the ORM's own
/// (here 500 ms): the wait is the 10 s the message names.
#[tokio::test]
async fn the_early_schema_lock_waits_for_the_schema_lock_timeout() {
    let h = access_harness(open_policy()).await;
    let baseline = install_signal(&h).await;
    let config = erp_orm::OrmConfig {
        timeouts: erp_orm::Timeouts::new(
            Duration::from_millis(500),
            Duration::from_secs(5),
            Duration::from_secs(60),
        )
        .unwrap(),
        ..erp_orm::OrmConfig::default()
    };
    let orm = Orm::builder(
        Arc::clone(h.registry()),
        pool_for(h.db().config(), &format!("-c application_name={PRIMARY}")),
    )
    .policy(open_policy())
    .signal(Signal::new(baseline).with_refresh(InPlace))
    .config(config)
    .build()
    .unwrap();
    // another synchronization holds the schema lock
    let holder = h.db().client().await.unwrap();
    holder.batch_execute("BEGIN").await.unwrap();
    holder
        .query("SELECT pg_advisory_xact_lock(4207351906)", &[])
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let error = orm.sync_schema().await.unwrap_err();
    let waited = started.elapsed();
    holder.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(error.kind(), ErrorKind::Lock);
    assert_eq!(
        error.message(),
        format!(
            "another schema synchronization of this database is running; gave up waiting for it \
             after {} (lock_timeout), try again when it has finished",
            erp_orm::SCHEMA_LOCK_TIMEOUT
        )
    );
    assert!(
        waited >= Duration::from_secs(8),
        "gave up after {waited:?}, not the schema lock timeout"
    );
    h.cleanup().await.unwrap();
}
