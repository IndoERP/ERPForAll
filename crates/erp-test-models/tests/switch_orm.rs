//! `Transaction::switch_orm` and `Orm::rebuild`: one transaction moving through the
//! stage registries of a module operation.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use erp_registry::{Registry, RegistryBuilder};
use erp_sdk::{Context, ErrorKind, Orm, SUPERUSER_ID, StaticPolicy, Value};
use erp_test_models::test_compute::{self as tc, Constrained, PerfBase};
use erp_test_models::test_orm::Partner;
use erp_test_models::{base_users, test_orm, test_web};
use erp_testkit::{Harness, TestDb};
use support::one;

fn users_only() -> Arc<Registry> {
    Arc::new(RegistryBuilder::new([&base_users::ADDON]).build().unwrap())
}

fn with_test_orm() -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_orm::ADDON])
            .with_decls(crate::currency_fixture::decls())
            .build()
            .unwrap(),
    )
}

fn with_compute() -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_orm::ADDON, &tc::ADDON])
            .with_decls(crate::currency_fixture::decls())
            .build()
            .unwrap(),
    )
}

/// A database with `res.users` (and the superuser row) created through `orm`.
async fn prepare(orm: &Orm) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login) VALUES (1, '__system__', '__system__')")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// One transaction: tables of the first registry, switch, tables and records of the second, one
/// commit. Statement counter, after-commit hooks and the environment's uid survive the switch.
#[tokio::test]
async fn switch_orm_moves_one_transaction_to_another_registry() {
    let db = TestDb::create().await.unwrap();
    let reg_a = users_only();
    let reg_b = with_test_orm();
    let orm_a = Orm::builder(Arc::clone(&reg_a), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let orm_b = orm_a.rebuild(Arc::clone(&reg_b)).build().unwrap();
    assert!(orm_a.same_lineage(&orm_b));

    let mut tx = orm_a.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login) VALUES (1, '__system__', '__system__')")
        .await
        .unwrap();
    let err = tx.env().records("test_orm.partner").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    tx.after_commit(move || flag.store(true, Ordering::SeqCst));

    let before = tx.query_count();
    tx.switch_orm(&orm_b).await.unwrap();
    assert_eq!(
        tx.query_count(),
        before,
        "nothing pending: the switch sends no statement"
    );
    assert!(Arc::ptr_eq(tx.registry(), &reg_b));
    assert!(Arc::ptr_eq(tx.env().registry(), &reg_b));
    assert_eq!(tx.env().uid(), SUPERUSER_ID);
    assert!(tx.env().su());

    // the second registry's tables are created in the same PostgreSQL transaction
    tx.init_models(&[]).await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "one")])
        .await
        .unwrap();
    assert!(!ran.load(Ordering::SeqCst));
    tx.commit().await.unwrap();
    assert!(ran.load(Ordering::SeqCst), "after-commit hooks survive");

    let mut check = orm_b.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        one(&mut check, "SELECT name FROM test_orm_partner")
            .await
            .as_deref(),
        Some("one")
    );
    check.commit().await.unwrap();
    drop(check);
    db.cleanup().await.unwrap();
}

/// A switch that is rolled back leaves nothing, like any statement of the transaction.
#[tokio::test]
async fn rollback_after_switch_undoes_both_stages() {
    let db = TestDb::create().await.unwrap();
    let orm_a = Orm::builder(users_only(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    prepare(&orm_a).await;
    let orm_b = orm_a.rebuild(with_test_orm()).build().unwrap();
    let mut tx = orm_a.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login) VALUES (2, 'stage a', 'a')")
        .await
        .unwrap();
    tx.switch_orm(&orm_b).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM res_users WHERE login = 'a'"
        )
        .await
        .as_deref(),
        Some("0")
    );
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM pg_tables WHERE tablename = 'test_orm_partner'"
        )
        .await
        .as_deref(),
        Some("0")
    );
    tx.commit().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

/// Pending writes are flushed before the switch; the cache is cleared; records of the old
/// registry are refused afterwards, even when the new registry has the same models.
#[tokio::test]
async fn switch_orm_flushes_clears_cache_and_refuses_old_records() {
    let db = TestDb::create().await.unwrap();
    let orm_b = Orm::builder(with_test_orm(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    prepare(&orm_b).await;
    let reg_c = with_test_orm();
    let orm_c = orm_b.rebuild(Arc::clone(&reg_c)).build().unwrap();

    let mut tx = orm_b.begin(SUPERUSER_ID).await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let p = partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "before")])
        .await
        .unwrap();
    p.write(&mut tx, partners.vals().set(Partner::NAME, "after"))
        .await
        .unwrap();
    assert!(tx.is_dirty(p.as_records(), "name").unwrap());
    tx.switch_orm(&orm_c).await.unwrap();
    assert_eq!(
        one(&mut tx, "SELECT name FROM test_orm_partner")
            .await
            .as_deref(),
        Some("after"),
        "the pending write was flushed before the switch"
    );

    // the same record on the new registry: nothing cached
    let fresh = tx.env().model::<Partner>().unwrap();
    let p_new = fresh.browse(p.ids()).unwrap();
    assert!(!tx.is_cached(p_new.as_records(), "name").unwrap());

    // the old recordset is refused, never silently reused
    let err = p
        .write(&mut tx, partners.vals().set(Partner::NAME, "stale"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "{} belongs to another registry than this transaction's",
            p.as_records()
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

/// Every refusal names its reason and switches nothing.
#[tokio::test]
async fn switch_orm_refusals() {
    let db = TestDb::create().await.unwrap();
    let reg_b = with_compute();
    let orm_b = Orm::builder(Arc::clone(&reg_b), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    prepare(&orm_b).await;
    let orm_c = orm_b.rebuild(with_compute()).build().unwrap();

    let refused = |err: &erp_sdk::Error, message: &str| {
        assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
        assert_eq!(err.message(), message);
    };

    // an ORM of another database identity (built separately, even on the same pool)
    let other = Orm::builder(with_compute(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    assert!(!orm_b.same_lineage(&other));
    let mut tx = orm_b.begin(SUPERUSER_ID).await.unwrap();
    refused(
        &tx.switch_orm(&other).await.unwrap_err(),
        "cannot switch the transaction to an ORM of another database: switch_orm only accepts \
         ORMs built with Orm::rebuild from this transaction's ORM",
    );
    // the same registry: nothing to switch to
    let same = orm_b.rebuild(Arc::clone(&reg_b)).build().unwrap();
    refused(
        &tx.switch_orm(&same).await.unwrap_err(),
        "cannot switch the transaction to the registry it already uses",
    );
    assert!(Arc::ptr_eq(tx.registry(), &reg_b));

    // inside a savepoint
    let err = tx
        .savepoint(async |tx| tx.switch_orm(&orm_c).await)
        .await
        .unwrap_err();
    refused(
        &err,
        "cannot switch the transaction's ORM inside a savepoint",
    );
    assert!(Arc::ptr_eq(tx.registry(), &reg_b));

    // poisoned by a validation error (an ORM constraint: nothing failed in the database)
    let constrained = tx.env().model::<Constrained>().unwrap();
    let validation = constrained
        .create(
            &mut tx,
            vec![constrained.vals().set(Constrained::NAME, "bad")],
        )
        .await
        .unwrap_err();
    assert_eq!(validation.kind(), ErrorKind::Validation);
    assert!(tx.is_poisoned());
    refused(
        &tx.switch_orm(&orm_c).await.unwrap_err(),
        &format!(
            "cannot switch the ORM of a transaction that must be rolled back ({})",
            validation.message()
        ),
    );
    tx.rollback().await.unwrap();

    // aborted by a server error
    let failed = tx.query("SELECT 1/0", &[]).await.unwrap_err();
    assert_eq!(failed.sqlstate(), Some("22012"));
    refused(
        &tx.switch_orm(&orm_c).await.unwrap_err(),
        "cannot switch the ORM of an aborted transaction (division by zero); roll back first",
    );
    tx.rollback().await.unwrap();

    // after a rollback the switch works
    tx.switch_orm(&orm_c).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

/// DDL run through a stage ORM invalidates the statements another ORM of the same database
/// prepared on another pooled connection (the schema epoch belongs to the database identity, not
/// to one ORM), also for a transaction object that keeps its connection across commits.
#[tokio::test]
async fn ddl_through_a_stage_orm_invalidates_statements_of_the_root_orm() {
    let db = TestDb::create().await.unwrap();
    let root = Orm::builder(with_test_orm(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    prepare(&root).await;
    let stage = root.rebuild(with_compute()).build().unwrap();

    // the root ORM prepares `SELECT *` on its connection and keeps that connection
    let mut on_root = root.begin(SUPERUSER_ID).await.unwrap();
    let before = on_root
        .query("SELECT * FROM test_orm_partner", &[])
        .await
        .unwrap();
    assert!(before.is_empty());
    let columns_before = on_root
        .query(
            "SELECT count(*) FROM information_schema.columns WHERE table_name = 'test_orm_partner'",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    on_root.commit().await.unwrap();

    // the stage ORM (another connection: the root transaction still holds its own) alters it
    let mut on_stage = stage.begin(SUPERUSER_ID).await.unwrap();
    on_stage
        .batch("ALTER TABLE test_orm_partner ADD COLUMN stage_extra text")
        .await
        .unwrap();
    on_stage
        .execute(
            "INSERT INTO test_orm_partner (name, stage_extra) VALUES ('p', 'x')",
            &[],
        )
        .await
        .unwrap();
    on_stage.commit().await.unwrap();

    // the root's cached plan of `SELECT *` describes the old shape: it must not be reused
    let after = on_root
        .query("SELECT * FROM test_orm_partner", &[])
        .await
        .unwrap();
    assert_eq!(after.len(), 1);
    let width = usize::try_from(columns_before).unwrap() + 1;
    assert_eq!(after[0].len(), width, "the new column is read");
    on_root.commit().await.unwrap();
    drop(on_root);
    drop(on_stage);
    db.cleanup().await.unwrap();
}

/// The uid, superuser flag and context of the environment survive the switch (the System
/// principal, the only one that may switch, with a context key).
#[tokio::test]
async fn switch_orm_keeps_uid_su_and_context() {
    let db = TestDb::create().await.unwrap();
    let orm_a = Orm::builder(users_only(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    prepare(&orm_a).await;
    let orm_b = orm_a.rebuild(with_test_orm()).build().unwrap();
    let context = Context::from_pairs([("lang", Value::str("fr_FR")), ("k", Value::Int(7))]);
    let mut tx = orm_a
        .begin_with(SUPERUSER_ID, context.clone())
        .await
        .unwrap();
    assert!(tx.env().su());
    tx.switch_orm(&orm_b).await.unwrap();
    let env = tx.env();
    assert_eq!(env.uid(), 1);
    assert!(env.su());
    assert_eq!(env.context(), &context);
    assert_eq!(env.context().lang(), Some("fr_FR"));
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

/// A pending write whose flush fails: the flush's error is returned and nothing is switched.
#[tokio::test]
async fn switch_orm_returns_the_flush_error_and_switches_nothing() {
    let db = TestDb::create().await.unwrap();
    let reg_b = with_test_orm();
    let orm_b = Orm::builder(Arc::clone(&reg_b), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    prepare(&orm_b).await;
    let orm_c = orm_b.rebuild(with_test_orm()).build().unwrap();
    let mut tx = orm_b.begin(SUPERUSER_ID).await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let p = partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "one")])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let id = p.ids()[0];
    tx.execute("DELETE FROM test_orm_partner WHERE id = $1", &[&id])
        .await
        .unwrap();
    p.set(&mut tx, Partner::NAME, "pending").await.unwrap();
    let err = tx.switch_orm(&orm_c).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        format!(
            "Record does not exist or has been deleted.\n(Record: test_orm.partner({id},), User: 1)"
        )
    );
    assert!(Arc::ptr_eq(tx.registry(), &reg_b), "nothing switched");
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

/// Poll `fut` exactly once, then drop it (a cancelled operation); true when it was pending.
async fn poll_once<F: std::future::Future>(fut: F) -> bool {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| std::task::Poll::Ready(fut.as_mut().poll(cx).is_pending())).await
}

/// A transaction whose ORM operation was cancelled mid-change is refused.
#[tokio::test]
async fn switch_orm_refuses_a_cancelled_transaction() {
    let db = TestDb::create().await.unwrap();
    let reg_b = with_compute();
    let orm_b = Orm::builder(Arc::clone(&reg_b), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    prepare(&orm_b).await;
    let orm_c = orm_b.rebuild(with_compute()).build().unwrap();
    let mut tx = orm_b.begin(SUPERUSER_ID).await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(PerfBase::VALUE, 1)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    one(&mut tx, "SELECT 'open'::text").await;
    let keyed = rec.with_context([("key", Value::Int(i64::from(tc::YIELD_KEY)))]);
    assert!(
        poll_once(keyed.get(&mut tx, PerfBase::VALUE_CTX)).await,
        "the read must still be pending after one poll"
    );
    let err = tx.switch_orm(&orm_c).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the transaction can no longer be used: an ORM operation was cancelled (its future was \
         dropped) while it was changing the transaction's state; roll back"
    );
    assert!(Arc::ptr_eq(tx.registry(), &reg_b), "nothing switched");
    tx.rollback().await.unwrap();
    tx.switch_orm(&orm_c).await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

async fn note_onchange(
    tx: &mut erp_sdk::Transaction,
    discussion: &erp_sdk::Records,
    note: &str,
) -> erp_sdk::Result<Value> {
    let mut values = erp_sdk::ValueMap::new();
    values.insert("note".to_owned(), Value::str(note));
    let spec_map: erp_sdk::ValueMap = ["name", "note"]
        .into_iter()
        .map(|f| (f.to_owned(), Value::Map(Arc::new(erp_sdk::ValueMap::new()))))
        .collect();
    let spec = erp_orm::Spec::from_value(&Value::Map(Arc::new(spec_map))).unwrap();
    discussion
        .onchange(tx, values, vec!["note".to_owned()], spec)
        .await
}

/// Inside an isolated (onchange) scope the switch is refused; a released transaction is refused.
#[tokio::test]
async fn switch_orm_refuses_isolated_scopes_and_released_transactions() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .build()
        .await
        .unwrap();
    let target = h
        .orm()
        .rebuild(Arc::new(
            RegistryBuilder::new([&base_users::ADDON, &test_web::ADDON])
                .build()
                .unwrap(),
        ))
        .build()
        .unwrap();
    assert!(test_web::SWITCH_TARGET.set(target.clone()).is_ok());
    let mut tx = h.begin().await.unwrap();
    let discussions = tx.env().records("test_web.discussion").unwrap();
    let vals = std::collections::BTreeMap::from([("name".to_owned(), Value::str("General"))]);
    let discussion = discussions.create_values(&mut tx, &[vals]).await.unwrap();
    tx.commit().await.unwrap();

    let err = note_onchange(&mut tx, &discussion, "switch orm")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "cannot switch the transaction's ORM inside an isolated scope (onchange)"
    );
    assert!(Arc::ptr_eq(tx.registry(), h.registry()), "nothing switched");

    // the scope's ROLLBACK TO fails after a raw ROLLBACK: the transaction is released
    let err = note_onchange(&mut tx, &discussion, "raw rollback")
        .await
        .unwrap_err();
    assert!(tx.is_released(), "{err}");
    let err = tx.switch_orm(&target).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the transaction can no longer be used: its connection was released after a failure or a \
         dropped operation"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

fn noop_register_hook<'a>(
    tx: &'a mut erp_sdk::Transaction,
    records: &'a erp_sdk::Records,
    args: (),
    sup: erp_sdk::Super<(), ()>,
) -> erp_sdk::BoxFuture<'a, erp_sdk::Result<()>> {
    Box::pin(async move { sup.call(tx, records, args).await })
}

fn noop_register_hook_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static F: erp_orm::MethodFn<(), ()> = noop_register_hook;
    &F
}

static PARTNER_REGISTER_HOOK: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: "test_orm",
    model: "test_orm.partner",
    method: "_register_hook",
    kind: erp_orm::ImplKind::Override,
    sig_name: std::any::type_name::<erp_orm::MethodFn<(), ()>>,
    entry: noop_register_hook_entry,
    file: file!(),
    line: line!(),
};

/// `Orm::rebuild` carries the method implementations given to the original builder (an override
/// must not silently disappear from a stage registry); giving one again does not duplicate it.
#[tokio::test]
async fn rebuild_keeps_runtime_implementations() {
    let db = TestDb::create().await.unwrap();
    let orm_a = Orm::builder(with_test_orm(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .impls([&PARTNER_REGISTER_HOOK])
        .build()
        .unwrap();
    let has_override = |orm: &Orm| {
        let model = orm.registry().model("test_orm.partner").unwrap().id;
        orm.dispatch()
            .implementations(model, "_register_hook")
            .iter()
            .any(|i| std::ptr::eq(*i, &PARTNER_REGISTER_HOOK))
    };
    assert!(has_override(&orm_a));
    let orm_b = orm_a.rebuild(with_compute()).build().unwrap();
    assert!(has_override(&orm_b), "carried over by rebuild");
    let orm_c = orm_b
        .rebuild(with_test_orm())
        .impls([&PARTNER_REGISTER_HOOK])
        .build()
        .unwrap();
    assert!(
        has_override(&orm_c),
        "given again: still one implementation"
    );
    // a registry without the addon simply has no chain for it
    let orm_d = orm_a.rebuild(users_only()).build().unwrap();
    assert!(orm_d.registry().model("test_orm.partner").is_none());
    db.cleanup().await.unwrap();
}

/// A transaction that switched away from an ORM and commits clears that ORM's shared ormcache:
/// values cached there may describe data the transaction changed.
#[tokio::test]
async fn commit_after_switch_clears_the_old_orm_ormcache() {
    let db = TestDb::create().await.unwrap();
    let orm_a = Orm::builder(with_test_orm(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    prepare(&orm_a).await;
    let orm_b = orm_a.rebuild(with_compute()).build().unwrap();
    let key = erp_sdk::OrmCacheKey::new("test_orm.partner", "count", Vec::new());
    let mut tx = orm_a.begin(SUPERUSER_ID).await.unwrap();
    tx.ormcache_add("partners", key.clone(), Value::Int(0))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut reader = orm_a.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        reader.ormcache_get("partners", &key).await.unwrap(),
        Some(Value::Int(0))
    );
    drop(reader);

    tx.switch_orm(&orm_b).await.unwrap();
    tx.execute("INSERT INTO test_orm_partner (name) VALUES ('new')", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut reader = orm_a.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        reader.ormcache_get("partners", &key).await.unwrap(),
        None,
        "stale value dropped"
    );
    drop(reader);
    drop(tx);
    db.cleanup().await.unwrap();
}

/// The base-install grant belongs to the lineage (lead decision, merge train 2026-10-18):
/// `Orm::rebuild` shares the original ORM's id, so the grant the module-install executor got
/// from the build site stays valid on the stage ORMs of one module operation, on a transaction
/// opened there and on one that `switch_orm`ed there. An ORM from another `Orm::builder` on the
/// same pool is another lineage: the grant is refused for its transactions.
#[tokio::test]
async fn base_install_grant_stays_valid_across_rebuild_and_switch() {
    let db = TestDb::create().await.unwrap();
    let (orm_a, grant) = Orm::builder(users_only(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build_with_base_install_grant()
        .unwrap();
    prepare(&orm_a).await;
    let orm_b = orm_a.rebuild(with_test_orm()).build().unwrap();
    assert_eq!(
        orm_b.id(),
        orm_a.id(),
        "a rebuilt ORM shares the lineage id"
    );
    assert_eq!(grant.orm_id(), orm_a.id());

    // a transaction opened on the stage ORM accepts the original grant
    let mut tx = orm_b.begin(SUPERUSER_ID).await.unwrap();
    tx.begin_base_install(&grant).unwrap();
    assert!(tx.base_installing());
    tx.end_base_install();
    drop(tx);

    // the install window survives the switch, and can be opened again after it
    let mut tx = orm_a.begin(SUPERUSER_ID).await.unwrap();
    tx.begin_base_install(&grant).unwrap();
    tx.switch_orm(&orm_b).await.unwrap();
    assert!(tx.base_installing(), "switch_orm keeps the install window");
    tx.end_base_install();
    tx.begin_base_install(&grant).unwrap();
    assert!(tx.base_installing());
    drop(tx);

    // rebuilding with the grant method mints the same lineage's grant, nothing new
    let (orm_c, again) = orm_a
        .rebuild(with_compute())
        .build_with_base_install_grant()
        .unwrap();
    assert_eq!(again.orm_id(), orm_a.id());
    assert_eq!(orm_c.id(), orm_a.id());

    // another lineage on the same pool refuses it, and the flag stays off
    let other = Orm::builder(users_only(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    assert!(!other.same_lineage(&orm_a));
    assert_ne!(other.id(), orm_a.id());
    let mut tx = other.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.begin_base_install(&grant).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "the base-install grant belongs to another Orm than the one that opened this transaction"
    );
    assert!(!tx.base_installing());
    drop(tx);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_install_scope_raii_guard_lifecycle_and_refusals() {
    let db = TestDb::create().await.unwrap();
    let (orm_a, grant) = Orm::builder(users_only(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build_with_base_install_grant()
        .unwrap();

    let mut tx = orm_a.begin(SUPERUSER_ID).await.unwrap();
    assert!(!tx.base_installing());

    // scoped guard sets base_installing and resets on drop
    {
        let guard = tx.base_install_scope(&grant).unwrap();
        assert!(guard.base_installing());
    }
    assert!(!tx.base_installing(), "drop resets base_installing");

    // non-system principal is refused by base_install_scope
    let user_id = erp_orm::UserId::new(42).unwrap();
    let mut user_tx = orm_a.begin(user_id).await.unwrap();
    let err = user_tx.base_install_scope(&grant).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert!(!user_tx.base_installing());
    drop(user_tx);

    // another lineage on the same pool refuses base_install_scope
    let other = Orm::builder(users_only(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut other_tx = other.begin(SUPERUSER_ID).await.unwrap();
    let err = other_tx.base_install_scope(&grant).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert!(!other_tx.base_installing());
    drop(other_tx);

    drop(tx);
    db.cleanup().await.unwrap();
}
