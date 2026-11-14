// SPDX-License-Identifier: MIT OR Apache-2.0
//! B8 lifecycle safety on isolated synthetic addons (no business/default records).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod common;
use common::*;
use erp_fields::{ErrorKind, Result};
use erp_modules::{
    InitOptions,
    sql_uninstall::{SqlGuardRead, SqlUninstallGuard, SqlUninstallGuardManifest},
    system::{ModuleSystem, ModuleSystemBuilder},
};
use erp_orm::{BoxFuture, Context, Generations, Orm, SUPERUSER_ID, Signal};
use erp_registry::{
    AddonDescriptor, SqlDep, SqlEvent, SqlFunctionDecl, SqlLanguage, SqlLevel, SqlObjectDecl,
    SqlObjectManifest, SqlResult, SqlTiming, SqlTriggerDecl,
};
use erp_testkit::TestDb;
use std::time::Duration;

static A: AddonDescriptor = AddonDescriptor {
    name: "sq_a",
    depends: &["base"],
    exact_only: false,
};
static EXPECTED: SqlUninstallGuardManifest = SqlUninstallGuardManifest::new("sq_a", &["protected"]);
static GUARD: SqlUninstallGuard = SqlUninstallGuard::new(
    "sq_a",
    "protected",
    &["sq_a.item"],
    "protected_rows",
    "Protected rows prevent removal",
    protected,
);
fn protected<'a>(read: &'a mut SqlGuardRead<'_>) -> BoxFuture<'a, Result<u64>> {
    Box::pin(async move { read.count_eq("sq_a.item", "name", "posted").await })
}
fn fixture() -> Addons {
    let addons = Addons::new();
    addons.add("sq_a", "name='SQL A'\ndepends=['base']\n");
    addons
}
fn plain(db: &TestDb, addons: &Addons) -> ModuleSystemBuilder {
    builder(db, addons)
        .code([&A])
        .decls([model("sq_a", "sq_a.item")])
        .no_data_loader()
}
fn guarded(db: &TestDb, addons: &Addons) -> ModuleSystemBuilder {
    plain(db, addons)
        .require_sql_uninstall_guards([&EXPECTED])
        .with_sql_uninstall_guards([&GUARD])
}
async fn install(system: &ModuleSystem) -> Orm {
    let root = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap()
        .orm;
    system
        .button_immediate_install(&su(&root), &[n("sq_a")])
        .await
        .unwrap()
        .orm
}
fn assert_guard(error: erp_modules::Error) {
    let erp_modules::Error::Orm(error) = error else {
        panic!("wrong error: {error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), "Protected rows prevent removal (1)");
}
async fn snapshot(orm: &Orm) -> Vec<Vec<Option<String>>> {
    rows(orm, "SELECT state::text, (SELECT count(*)::text FROM sq_a_item) FROM ir_module_module WHERE name='sq_a'").await
}

#[tokio::test]
async fn protected_rows_refuse_before_effects_then_zero_allows_uninstall() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    addons.add(
        "sq_a",
        "name='SQL A'\ndepends=['base']\nuninstall_hook='erase'\n",
    );
    let system = guarded(&db, &addons).hooks([&ERASE]).build().unwrap();
    let orm = install(&system).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("INSERT INTO sq_a_item (name) VALUES ('posted')", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let before = snapshot(&orm).await;
    assert_guard(
        system
            .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
            .await
            .unwrap_err(),
    );
    assert_eq!(snapshot(&orm).await, before);
    assert_eq!(UNINSTALL_HOOKS.load(std::sync::atomic::Ordering::SeqCst), 0);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("DELETE FROM sq_a_item", &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let removed = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(state(&removed.orm, "sq_a").await, "uninstalled");
    assert!(!table_exists(&removed.orm, "sq_a_item").await);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn committed_while_guard_waits_is_visible_with_signals() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = guarded(&db, &addons)
        .signal(Signal::new(Generations::new()).with_refresh(KeepServing))
        .build()
        .unwrap();
    let orm = install(&system).await;
    let writer = db.pool().get().await.unwrap();
    writer
        .batch_execute("BEGIN; INSERT INTO sq_a_item (name) VALUES ('posted')")
        .await
        .unwrap();
    let observer = db.pool().get().await.unwrap();
    let caller = system.system_env(Context::new());
    let operation = tokio::spawn(async move {
        system
            .button_immediate_uninstall(&caller, &[n("sq_a")])
            .await
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let waiting: bool = observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=pg_catalog.current_database()) AND relation='sq_a_item'::regclass AND NOT granted)", &[]).await.unwrap().get(0);
            if waiting { break; }
            assert!(!operation.is_finished(), "uninstall ended before reaching its table lock");
            tokio::task::yield_now().await;
        }
    }).await.expect("uninstall must reach the guard table lock");
    writer.batch_execute("COMMIT").await.unwrap();
    assert_guard(operation.await.unwrap().unwrap_err());
    assert_eq!(snapshot(&orm).await[0][1].as_deref(), Some("1"));
    drop(writer);
    drop(observer);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn expected_guard_missing_is_not_silently_skipped() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let error = match plain(&db, &addons)
        .require_sql_uninstall_guards([&EXPECTED])
        .build()
    {
        Ok(_) => panic!("missing guard accepted"),
        Err(error) => error,
    };
    let erp_modules::Error::Orm(error) = error else {
        panic!("wrong error: {error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL uninstall guard 'sq_a/protected' required by addon 'sq_a' was not registered"
    );
    db.cleanup().await.unwrap();
}

static OBJECTS: SqlObjectManifest = SqlObjectManifest::new("sq_a", &["function", "trigger"]);
fn objects() -> Vec<&'static SqlObjectDecl> {
    vec![
        Box::leak(Box::new(SqlObjectDecl::function(
            "sq_a",
            "function",
            SqlFunctionDecl::new(
                "sq_a_protect",
                SqlLanguage::PlPgSql,
                SqlResult::Trigger,
                "BEGIN RETURN NEW; END",
            )
            .deps(&[SqlDep::Model("sq_a.item")]),
        ))),
        Box::leak(Box::new(SqlObjectDecl::trigger(
            "sq_a",
            "trigger",
            SqlTriggerDecl::new(
                "sq_a_protect",
                "sq_a.item",
                ("sq_a", "function"),
                SqlTiming::Before,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            ),
        ))),
    ]
}

#[tokio::test]
async fn reverse_object_removal_cleans_provenance_and_allows_reinstall() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = guarded(&db, &addons)
        .require_sql_objects([&OBJECTS])
        .with_sql_objects(objects())
        .build()
        .unwrap();
    let orm = install(&system).await;
    let removed = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(rows(&removed.orm, "SELECT (pg_catalog.to_regprocedure('sq_a_protect()') IS NULL)::text, (SELECT count(*)::text FROM erp_schema_sql_object WHERE addon='sq_a')").await[0], vec![Some("true".into()), Some("0".into())]);
    let reinstalled = system
        .button_immediate_install(&su(&removed.orm), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(
        rows(
            &reinstalled.orm,
            "SELECT count(*)::text FROM erp_schema_sql_object WHERE addon='sq_a'"
        )
        .await[0][0]
            .as_deref(),
        Some("2")
    );
    db.cleanup().await.unwrap();
}

fn undeclared<'a>(read: &'a mut SqlGuardRead<'_>) -> BoxFuture<'a, Result<u64>> {
    Box::pin(async move { read.count_all("ir.module.module").await })
}
static UNDECLARED: SqlUninstallGuard = SqlUninstallGuard::new(
    "sq_a",
    "protected",
    &["sq_a.item"],
    "protected_rows",
    "Protected rows prevent removal",
    undeclared,
);
#[tokio::test]
async fn guard_cannot_read_an_undeclared_model() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = plain(&db, &addons)
        .require_sql_uninstall_guards([&EXPECTED])
        .with_sql_uninstall_guards([&UNDECLARED])
        .build()
        .unwrap();
    let orm = install(&system).await;
    let error = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap_err();
    let erp_modules::Error::Orm(error) = error else {
        panic!("wrong error: {error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL uninstall guard 'sq_a/protected': model 'ir.module.module' was not declared for reading"
    );
    assert!(table_exists(&orm, "sq_a_item").await);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_uninstall_cannot_bypass_the_guard() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = guarded(&db, &addons).build().unwrap();
    let orm = install(&system).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("INSERT INTO sq_a_item (name) VALUES ('posted')", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    system
        .button_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap();
    assert_guard(system.load().await.unwrap_err());
    assert_eq!(
        rows(&orm, "SELECT count(*)::text FROM sq_a_item").await[0][0].as_deref(),
        Some("1")
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn late_restrict_failure_rolls_back_object_drops() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    addons.add(
        "sq_a",
        "name='SQL A'\ndepends=['base']\nuninstall_hook='erase'\n",
    );
    let system = guarded(&db, &addons)
        .hooks([&ERASE])
        .require_sql_objects([&OBJECTS])
        .with_sql_objects(objects())
        .build()
        .unwrap();
    let orm = install(&system).await;
    exec(&orm, "INSERT INTO sq_a_item(id,name) VALUES (7,'draft')").await;
    let client = db.pool().get().await.unwrap();
    client
        .batch_execute("CREATE VIEW sq_survives AS SELECT * FROM sq_a_item")
        .await
        .unwrap();
    drop(client);
    let before = rows(&orm, "SELECT addon,key,encode(installed_sha256,'hex') FROM erp_schema_sql_object ORDER BY addon,key").await;
    let error = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap_err();
    let erp_modules::Error::SchemaDropBlocked {
        modules,
        message,
        detail,
    } = error
    else {
        panic!("{error:?}")
    };
    assert_eq!(modules, vec![n("sq_a")]);
    assert_eq!(
        message,
        "cannot drop table sq_a_item because other objects depend on it"
    );
    assert_eq!(detail, "view sq_survives depends on table sq_a_item");
    assert_eq!(state(&orm, "sq_a").await, "installed");
    assert_eq!(
        rows(&orm, "SELECT name FROM sq_a_item WHERE id=7").await[0][0].as_deref(),
        Some("draft")
    );
    assert_eq!(rows(&orm, "SELECT addon,key,encode(installed_sha256,'hex') FROM erp_schema_sql_object ORDER BY addon,key").await, before);
    assert_eq!(
        rows(
            &orm,
            "SELECT count(*)::text FROM pg_catalog.pg_trigger WHERE tgname='sq_a_protect'"
        )
        .await[0][0]
            .as_deref(),
        Some("1")
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn cancelling_lock_wait_releases_session_bridge_and_preserves_data() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = std::sync::Arc::new(guarded(&db, &addons).build().unwrap());
    let orm = install(&system).await;
    let writer = db.pool().get().await.unwrap();
    writer
        .batch_execute("BEGIN; INSERT INTO sq_a_item (name) VALUES ('posted')")
        .await
        .unwrap();
    let observer = db.pool().get().await.unwrap();
    let caller = system.system_env(Context::new());
    let running = system.clone();
    let operation = tokio::spawn(async move {
        running
            .button_immediate_uninstall(&caller, &[n("sq_a")])
            .await
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let waiting: bool = observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=pg_catalog.current_database()) AND relation='sq_a_item'::regclass AND NOT granted)", &[]).await.unwrap().get(0);
            if waiting { break; }
            assert!(!operation.is_finished());
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    writer.batch_execute("ROLLBACK").await.unwrap();
    drop(writer);
    drop(observer);
    assert_eq!(state(&orm, "sq_a").await, "installed");
    // Acquiring the operation lock proves no session-level bridge survived cancellation.
    let removed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match system
                .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
                .await
            {
                Err(erp_modules::Error::OperationInProgress) => tokio::task::yield_now().await,
                other => break other.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    assert!(!table_exists(&removed.orm, "sq_a_item").await);
    db.cleanup().await.unwrap();
}

fn swallowed<'a>(read: &'a mut SqlGuardRead<'_>) -> BoxFuture<'a, Result<u64>> {
    Box::pin(async move {
        let _ = read.count_all("ir.module.module").await;
        Ok(0)
    })
}
static SWALLOWED: SqlUninstallGuard = SqlUninstallGuard::new(
    "sq_a",
    "protected",
    &["sq_a.item"],
    "protected_rows",
    "Protected rows prevent removal",
    swallowed,
);
#[tokio::test]
async fn callback_cannot_swallow_a_reader_refusal() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = plain(&db, &addons)
        .with_sql_uninstall_guards([&SWALLOWED])
        .build()
        .unwrap();
    let orm = install(&system).await;
    let error = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap_err();
    let erp_modules::Error::Orm(error) = error else {
        panic!("wrong error: {error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL uninstall guard 'sq_a/protected': model 'ir.module.module' was not declared for reading"
    );
    assert!(table_exists(&orm, "sq_a_item").await);
    db.cleanup().await.unwrap();
}

// Synthetic schema fixtures swap registries explicitly; the hook only acknowledges generations.
struct KeepServing;
impl erp_orm::Refresh for KeepServing {
    fn refresh<'a>(&'a self, _: &'a erp_orm::Changes) -> BoxFuture<'a, Result<erp_orm::Refreshed>> {
        Box::pin(async { Ok(erp_orm::Refreshed::InPlace) })
    }
}

static UNINSTALL_HOOKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn erase_drafts(tx: &mut erp_orm::Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        UNINSTALL_HOOKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The record API is deliberately used: a ReadCommitted shortcut must fail here.
        let records = tx.env().records("sq_a.item")?.browse([7])?;
        records.read(tx, &["name"]).await?;
        tx.execute("DELETE FROM sq_a_item WHERE name='draft'", &[])
            .await?;
        Ok(())
    })
}
static ERASE: erp_modules::hooks::HookImpl = erp_modules::hooks::HookImpl::new(
    "sq_a",
    erp_modules::hooks::HookKind::Uninstall,
    "erase",
    erase_drafts,
);

#[tokio::test]
async fn row_security_never_turns_a_protected_count_into_zero() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = guarded(&db, &addons).build().unwrap();
    let orm = install(&system).await;
    exec(&orm,"INSERT INTO sq_a_item(name) VALUES ('posted'); ALTER TABLE sq_a_item ENABLE ROW LEVEL SECURITY; ALTER TABLE sq_a_item FORCE ROW LEVEL SECURITY").await;
    let error = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap_err();
    let erp_modules::Error::Orm(error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL uninstall guard 'sq_a/protected': table 'sq_a_item' is not a native ordinary count source"
    );
    assert!(table_exists(&orm, "sq_a_item").await);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn zero_query_limit_is_a_refusal_and_not_unlimited() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let mut limits = erp_modules::sql_uninstall::SqlGuardLimits::DEFAULT;
    limits.queries = 0;
    let system = guarded(&db, &addons)
        .sql_guard_limits(limits)
        .build()
        .unwrap();
    let orm = install(&system).await;
    let error = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap_err();
    let erp_modules::Error::Orm(error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL uninstall guard 'sq_a/protected': count query limit exceeded"
    );
    assert!(table_exists(&orm, "sq_a_item").await);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn duplicate_guard_registration_refuses_the_build() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let error = match guarded(&db, &addons)
        .with_sql_uninstall_guards([&GUARD])
        .build()
    {
        Ok(_) => panic!("duplicate accepted"),
        Err(e) => e,
    };
    let erp_modules::Error::Orm(error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL uninstall guard 'sq_a/protected': duplicate registration"
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn changed_function_body_is_drift_not_a_successful_drop() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = guarded(&db, &addons)
        .require_sql_objects([&OBJECTS])
        .with_sql_objects(objects())
        .build()
        .unwrap();
    let orm = install(&system).await;
    exec(&orm,"CREATE OR REPLACE FUNCTION sq_a_protect() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN NEW.name := ''changed''; RETURN NEW; END'").await;
    let error = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap_err();
    let erp_modules::Error::Orm(error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'sq_a/function': installed definition differs; reconcile through addon upgrade"
    );
    assert_eq!(state(&orm, "sq_a").await, "installed");
    assert_eq!(
        rows(
            &orm,
            "SELECT count(*)::text FROM erp_schema_sql_object WHERE addon='sq_a'"
        )
        .await[0][0]
            .as_deref(),
        Some("2")
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn lock_first_opening_refuses_foreign_control_and_prior_mutations() {
    let db = TestDb::create().await.unwrap();
    let registry = std::sync::Arc::new(
        erp_registry::RegistryBuilder::new([&A])
            .linked(false)
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, db.pool().clone()).build().unwrap();
    let key = erp_orm::OperationKey::claim().unwrap();
    let mut one = orm.begin(SUPERUSER_ID).await.unwrap();
    let own = one.manage_operation(&key).unwrap();
    let mut two = orm.begin(SUPERUSER_ID).await.unwrap();
    let foreign = two.manage_operation(&key).unwrap();
    let tables = vec![("public".into(), "not_reached".into())];
    let error = one
        .operation_reopen_locked(
            &foreign,
            &tables,
            erp_modules::OPERATION_LOCK,
            erp_sql::ddl::SCHEMA_LOCK_KEY,
            (Duration::from_secs(1), Duration::from_secs(1)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Transaction::operation_reopen_locked: the operation control belongs to another transaction"
    );
    one.execute("CREATE TABLE already_mutated(id bigint)", &[])
        .await
        .unwrap();
    let error = one
        .operation_reopen_locked(
            &own,
            &tables,
            erp_modules::OPERATION_LOCK,
            erp_sql::ddl::SCHEMA_LOCK_KEY,
            (Duration::from_secs(1), Duration::from_secs(1)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Lock-first lifecycle opening requires unmutated preparation, ordinary primary isolation, tables and bounded waits"
    );
    one.operation_rollback(&own).await.unwrap();
    two.operation_rollback(&foreign).await.unwrap();
    drop(one);
    drop(two);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn removal_plan_of_another_transaction_is_refused() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = guarded(&db, &addons)
        .require_sql_objects([&OBJECTS])
        .with_sql_objects(objects())
        .build()
        .unwrap();
    let orm = install(&system).await;
    let registry = std::sync::Arc::clone(orm.registry());
    // the module system holds the process's key; a test key manages transactions outside it
    let key = erp_orm::OperationKey::for_tests();
    let mut planner = orm.begin(SUPERUSER_ID).await.unwrap();
    let planned = planner.manage_operation(&key).unwrap();
    // nothing is removed (old and new are the same registry), but the installed objects make the
    // plan record its transaction
    let plan = planner
        .plan_sql_object_removal(
            &planned,
            &registry,
            &registry,
            &erp_orm::SchemaSyncOptions::new(),
        )
        .await
        .unwrap();
    let mut other = orm.begin(SUPERUSER_ID).await.unwrap();
    let control = other.manage_operation(&key).unwrap();
    let error = other
        .apply_sql_object_removal(&control, plan)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object removal plan belongs to another transaction"
    );
    other.operation_rollback(&control).await.unwrap();
    planner.operation_rollback(&planned).await.unwrap();
    drop(other);
    drop(planner);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn check_on_a_surviving_model_is_removed_without_native_function_objects() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let extension = Box::leak(Box::new(
        erp_registry::ModelDecl::new("sq_a", "ir.module.module")
            .inherit(&["ir.module.module"])
            .table_objects(&[erp_registry::TableObjectDecl {
                name: "_b8_guard",
                kind: erp_registry::TableObjectKind::Constraint {
                    definition: "CHECK(name IS NOT NULL)",
                    message: "Name required.",
                },
            }]),
    ));
    let system = plain(&db, &addons).decls([&*extension]).build().unwrap();
    let orm = install(&system).await;
    assert_eq!(rows(&orm,"SELECT count(*)::text FROM pg_catalog.pg_constraint WHERE conname='ir_module_module_b8_guard'").await[0][0].as_deref(),Some("1"));
    let removed = system
        .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(rows(&removed.orm,"SELECT count(*)::text FROM pg_catalog.pg_constraint WHERE conname='ir_module_module_b8_guard'").await[0][0].as_deref(),Some("0"));
    assert!(table_exists(&removed.orm, "ir_module_module").await);
    db.cleanup().await.unwrap();
}

struct DeniedAuthority;
impl erp_modules::install::InstallAuthorizer for DeniedAuthority {
    fn authorize<'a>(
        &'a self,
        _: &'a mut erp_orm::Transaction,
    ) -> BoxFuture<'a, Result<erp_modules::install::InstallAuthorization>> {
        Box::pin(async { Err(erp_fields::Error::access("Denied test authority")) })
    }
}
#[tokio::test]
async fn denied_authority_cannot_observe_a_pending_guards_count() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = guarded(&db, &addons).build().unwrap();
    let orm = install(&system).await;
    exec(&orm, "INSERT INTO sq_a_item(name) VALUES ('posted')").await;
    system
        .button_uninstall(&su(&orm), &[n("sq_a")])
        .await
        .unwrap();
    let error = system
        .install_authorized(&[n("sq_a")], &DeniedAuthority)
        .await
        .unwrap_err();
    let erp_modules::Error::Orm(error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), "Denied test authority");
    assert_eq!(state(&orm, "sq_a").await, "to remove");
    db.cleanup().await.unwrap();
}

static B: AddonDescriptor = AddonDescriptor {
    name: "sq_b",
    depends: &["base"],
    exact_only: false,
};
#[tokio::test]
async fn shared_check_survives_until_its_last_owner_is_removed() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    addons.add("sq_b", "name='SQL B'\ndepends=['base']\n");
    let contribution = |addon| {
        Box::leak(Box::new(
            erp_registry::ModelDecl::new(addon, "ir.module.module")
                .inherit(&["ir.module.module"])
                .table_objects(&[erp_registry::TableObjectDecl {
                    name: "_b8_shared",
                    kind: erp_registry::TableObjectKind::Constraint {
                        definition: "CHECK(name IS NOT NULL)",
                        message: "Name required.",
                    },
                }]),
        ))
    };
    let a = contribution("sq_a");
    let b = contribution("sq_b");
    let system = plain(&db, &addons)
        .code([&B])
        .decls([&*a, &*b])
        .build()
        .unwrap();
    let orm = install(&system).await;
    let both = system
        .button_immediate_install(&su(&orm), &[n("sq_b")])
        .await
        .unwrap();
    let kept = system
        .button_immediate_uninstall(&su(&both.orm), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(state(&kept.orm, "sq_b").await, "installed");
    assert_eq!(rows(&kept.orm,"SELECT count(*)::text FROM pg_catalog.pg_constraint WHERE conname='ir_module_module_b8_shared'").await[0][0].as_deref(),Some("1"));
    let removed = system
        .button_immediate_uninstall(&su(&kept.orm), &[n("sq_b")])
        .await
        .unwrap();
    assert_eq!(rows(&removed.orm,"SELECT count(*)::text FROM pg_catalog.pg_constraint WHERE conname='ir_module_module_b8_shared'").await[0][0].as_deref(),Some("0"));
    db.cleanup().await.unwrap();
}

fn post_during_uninstall(tx: &mut erp_orm::Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        tx.execute("INSERT INTO sq_a_item(name) VALUES ('posted')", &[])
            .await?;
        Ok(())
    })
}
static POST: erp_modules::hooks::HookImpl = erp_modules::hooks::HookImpl::new(
    "sq_a",
    erp_modules::hooks::HookKind::Uninstall,
    "post",
    post_during_uninstall,
);
#[tokio::test]
async fn protected_rows_created_by_uninstall_hooks_are_rechecked_before_purge() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    addons.add(
        "sq_a",
        "name='SQL A'\ndepends=['base']\nuninstall_hook='post'\n",
    );
    let system = guarded(&db, &addons).hooks([&POST]).build().unwrap();
    let orm = install(&system).await;
    assert_guard(
        system
            .button_immediate_uninstall(&su(&orm), &[n("sq_a")])
            .await
            .unwrap_err(),
    );
    assert_eq!(state(&orm, "sq_a").await, "installed");
    assert_eq!(
        rows(&orm, "SELECT count(*)::text FROM sq_a_item").await[0][0].as_deref(),
        Some("0")
    );
    db.cleanup().await.unwrap();
}

static FENCE_OWNER: AddonDescriptor = AddonDescriptor {
    name: "sq_a",
    depends: &["base", "sq_b"],
    exact_only: false,
};
static FENCE_MODELS: AddonDescriptor = AddonDescriptor {
    name: "sq_b",
    depends: &["base"],
    exact_only: false,
};
static FENCE_GUARD: SqlUninstallGuard = SqlUninstallGuard::new(
    "sq_a",
    "protected",
    &["sq_b.zebra", "sq_b.alpha"], // Intentionally reverse the physical lock order.
    "protected_rows",
    "Protected rows prevent removal",
    empty_fenced_tables,
);
fn empty_fenced_tables<'a>(read: &'a mut SqlGuardRead<'_>) -> BoxFuture<'a, Result<u64>> {
    Box::pin(async move {
        let first = read.count_all("sq_b.zebra").await?;
        let second = read.count_all("sq_b.alpha").await?;
        first
            .checked_add(second)
            .ok_or_else(|| erp_fields::Error::programming("Synthetic protected row count overflow"))
    })
}
fn fence_barrier(tx: &mut erp_orm::Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        tx.query(
            "SELECT pg_catalog.pg_advisory_xact_lock(889102::bigint)",
            &[],
        )
        .await?;
        Ok(())
    })
}
static FENCE_BARRIER: erp_modules::hooks::HookImpl = erp_modules::hooks::HookImpl::new(
    "sq_a",
    erp_modules::hooks::HookKind::Uninstall,
    "fence_barrier",
    fence_barrier,
);

#[tokio::test]
async fn zero_guard_keeps_every_table_locked_through_the_destructive_transaction() {
    let db = TestDb::builder().pool_size(6).create().await.unwrap();
    let addons = fixture();
    addons.add("sq_b", "name='SQL B'\ndepends=['base']\n");
    addons.add(
        "sq_a",
        "name='SQL A'\ndepends=['base','sq_b']\nuninstall_hook='fence_barrier'\n",
    );
    let system = builder(&db, &addons)
        .code([&FENCE_OWNER, &FENCE_MODELS])
        .decls([model("sq_b", "sq_b.alpha"), model("sq_b", "sq_b.zebra")])
        .hooks([&FENCE_BARRIER])
        .require_sql_uninstall_guards([&EXPECTED])
        .with_sql_uninstall_guards([&FENCE_GUARD])
        .no_data_loader()
        .build()
        .unwrap();
    let orm = install(&system).await;
    let observer = db.pool().get().await.unwrap();
    observer
        .query_one("SELECT pg_catalog.pg_advisory_lock(889102::bigint)", &[])
        .await
        .unwrap();
    let caller = system.system_env(Context::new());
    let operation = tokio::spawn(async move {
        system
            .button_immediate_uninstall(&caller, &[n("sq_a")])
            .await
    });
    let pid: i32 = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let waiting = observer.query("SELECT pid FROM pg_catalog.pg_locks WHERE database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=pg_catalog.current_database()) AND locktype='advisory' AND classid=0 AND objid=889102 AND NOT granted", &[]).await.unwrap();
            if let Some(row) = waiting.first() { break row.get(0); }
            assert!(!operation.is_finished(), "uninstall did not reach the native hook barrier");
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let locked: i64 = observer.query_one("SELECT count(*) FROM pg_catalog.pg_locks WHERE pid=$1 AND mode='AccessExclusiveLock' AND granted AND relation IN ('sq_b_alpha'::regclass,'sq_b_zebra'::regclass)", &[&pid]).await.unwrap().get(0);
    assert_eq!(locked, 2);
    let writer = db.pool().get().await.unwrap();
    let writer_pid: i32 = writer
        .query_one("SELECT pg_catalog.pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let writing = tokio::spawn(async move {
        writer
            .batch_execute("INSERT INTO sq_b_alpha(name) VALUES ('posted')")
            .await
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let blocked: bool = observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE pid=$1 AND relation='sq_b_alpha'::regclass AND NOT granted)", &[&writer_pid]).await.unwrap().get(0);
            if blocked { break; }
            assert!(!writing.is_finished(), "writer escaped the retained guard lock");
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(!operation.is_finished());
    let released: bool = observer
        .query_one("SELECT pg_catalog.pg_advisory_unlock(889102::bigint)", &[])
        .await
        .unwrap()
        .get(0);
    assert!(released);
    let removed = operation.await.unwrap().unwrap();
    writing.await.unwrap();
    assert_eq!(state(&removed.orm, "sq_a").await, "uninstalled");
    assert_eq!(state(&removed.orm, "sq_b").await, "installed");
    assert_eq!(
        rows(&orm, "SELECT name FROM sq_b_alpha").await,
        vec![vec![Some("posted".into())]]
    );
    drop(observer);
    db.cleanup().await.unwrap();
}

fn capture_guard_timeouts(tx: &mut erp_orm::Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        tx.execute("INSERT INTO b8_timeout_capture SELECT pg_catalog.current_setting('lock_timeout'), pg_catalog.current_setting('statement_timeout')", &[]).await?;
        Ok(())
    })
}
static CAPTURE_GUARD_TIMEOUTS: erp_modules::hooks::HookImpl = erp_modules::hooks::HookImpl::new(
    "sq_a",
    erp_modules::hooks::HookKind::Uninstall,
    "capture_timeouts",
    capture_guard_timeouts,
);

#[tokio::test]
async fn lock_first_opening_preserves_lower_caller_deadlines_and_timeout_classification() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    addons.add(
        "sq_a",
        "name='SQL A'\ndepends=['base']\nuninstall_hook='capture_timeouts'\n",
    );
    let setup = guarded(&db, &addons)
        .hooks([&CAPTURE_GUARD_TIMEOUTS])
        .build()
        .unwrap();
    let orm = install(&setup).await;
    exec(
        &orm,
        "CREATE TABLE b8_timeout_capture (lock_wait text, statement_wait text)",
    )
    .await;
    let mut config = db.config();
    config.options("-c lock_timeout=250ms -c statement_timeout=10s");
    let pool = erp_orm::pool::create_pool(config, 3).unwrap();
    let identities = ["base", "sq_a"]
        .into_iter()
        .map(|name| {
            let identity = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name,
                &addons.path().join(name),
            )
            .unwrap();
            (n(name), identity.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(pool, addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&A])
        .decls([model("sq_a", "sq_a.item")])
        .hooks([&CAPTURE_GUARD_TIMEOUTS])
        .require_sql_uninstall_guards([&EXPECTED])
        .with_sql_uninstall_guards([&GUARD])
        .no_data_loader()
        .build()
        .unwrap();
    let loaded = system.load().await.unwrap().orm;
    let writer = db.pool().get().await.unwrap();
    writer
        .batch_execute("BEGIN; INSERT INTO sq_a_item(name) VALUES ('draft')")
        .await
        .unwrap();
    let error = system
        .button_immediate_uninstall(&su(&loaded), &[n("sq_a")])
        .await
        .unwrap_err();
    let erp_modules::Error::LockNotAvailable {
        lock_timeout,
        source,
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(*lock_timeout, Duration::from_millis(250));
    assert_eq!(source.sqlstate(), Some("55P03"));
    assert_eq!(source.message(), "canceling statement due to lock timeout");
    assert_eq!(
        error.to_string(),
        "A statement of a module operation could not get a database lock (SQLSTATE 55P03; lock_timeout is 250 ms)"
    );
    assert_eq!(state(&orm, "sq_a").await, "installed");
    assert!(
        rows(&orm, "SELECT * FROM b8_timeout_capture")
            .await
            .is_empty()
    );
    writer.batch_execute("ROLLBACK").await.unwrap();
    drop(writer);
    committed(
        system
            .button_immediate_uninstall(&su(&loaded), &[n("sq_a")])
            .await
            .unwrap(),
    );
    assert_eq!(
        rows(&orm, "SELECT * FROM b8_timeout_capture").await,
        vec![vec![Some("250ms".into()), Some("10s".into())]]
    );
    db.cleanup().await.unwrap();
}

fn caught_database_error<'a>(read: &'a mut SqlGuardRead<'_>) -> BoxFuture<'a, Result<u64>> {
    Box::pin(async move {
        // Deliberately hostile callback: neither catching the error nor trying again may
        // replace the first database failure with zero or an aborted-transaction error.
        if read.count_all("sq_a.item").await.is_err() {
            match read.count_all("sq_a.item").await {
                Ok(count) => Ok(count),
                Err(_) => Ok(0),
            }
        } else {
            Ok(0)
        }
    })
}
static CAUGHT_DATABASE_ERROR: SqlUninstallGuard = SqlUninstallGuard::new(
    "sq_a",
    "protected",
    &["sq_a.item"],
    "protected_rows",
    "Protected rows prevent removal",
    caught_database_error,
);

#[tokio::test]
async fn caught_reader_database_failure_retains_the_first_sqlstate_and_message() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let setup = plain(&db, &addons)
        .require_sql_uninstall_guards([&EXPECTED])
        .with_sql_uninstall_guards([&CAUGHT_DATABASE_ERROR])
        .build()
        .unwrap();
    let orm = install(&setup).await;
    // Use an explicitly restricted role: the cluster's administrative test user may
    // bypass table ACLs. UPDATE permits the lifecycle table lock, but SELECT is absent.
    let role = format!("b8_guard_{}", db.name());
    let quoted = erp_sql::quote(&role);
    exec(&orm, &format!("CREATE ROLE {quoted} NOLOGIN; GRANT USAGE ON SCHEMA public TO {quoted}; GRANT SELECT ON ALL TABLES IN SCHEMA public TO {quoted}; REVOKE SELECT ON sq_a_item FROM {quoted}; GRANT UPDATE ON sq_a_item TO {quoted}")).await;
    let mut config = db.config();
    config.options(format!("-c role={role}"));
    let pool = erp_orm::pool::create_pool(config, 3).unwrap();
    let identities = ["base", "sq_a"]
        .into_iter()
        .map(|name| {
            let identity = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name,
                &addons.path().join(name),
            )
            .unwrap();
            (n(name), identity.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(pool, addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&A])
        .decls([model("sq_a", "sq_a.item")])
        .require_sql_uninstall_guards([&EXPECTED])
        .with_sql_uninstall_guards([&CAUGHT_DATABASE_ERROR])
        .no_data_loader()
        .build()
        .unwrap();
    let result = system
        .button_immediate_uninstall(&system.system_env(Context::new()), &[n("sq_a")])
        .await;
    let after = state(&orm, "sq_a").await;
    let table_kept = table_exists(&orm, "sq_a_item").await;
    drop(system);
    exec(&orm, &format!("DROP OWNED BY {quoted}; DROP ROLE {quoted}")).await;
    db.cleanup().await.unwrap();
    let error = result.unwrap_err();
    let erp_modules::Error::Orm(error) = error else {
        panic!("{error:?}");
    };
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("42501"));
    assert_eq!(error.message(), "permission denied for table sq_a_item");
    assert_eq!(after, "installed");
    assert!(table_kept);
}

#[tokio::test]
async fn lock_first_opening_refuses_waits_that_round_to_unlimited_or_exceed_postgres_range() {
    let db = TestDb::create().await.unwrap();
    let registry = std::sync::Arc::new(
        erp_registry::RegistryBuilder::new([&A])
            .linked(false)
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, db.pool().clone()).build().unwrap();
    exec(&orm, "CREATE TABLE b8_wait_bound (id bigint)").await;
    let key = erp_orm::OperationKey::claim().unwrap();
    let mut errors = Vec::new();
    for (limits, name) in [
        ((Duration::ZERO, Duration::from_secs(1)), "lock_timeout"),
        (
            (Duration::from_secs(1), Duration::ZERO),
            "statement_timeout",
        ),
        (
            (Duration::from_nanos(1), Duration::from_secs(1)),
            "lock_timeout",
        ),
        (
            (Duration::from_secs(1), Duration::from_nanos(1)),
            "statement_timeout",
        ),
        (
            (
                Duration::from_millis(u64::from(u32::MAX)),
                Duration::from_secs(1),
            ),
            "lock_timeout",
        ),
        (
            (
                Duration::from_secs(1),
                Duration::from_millis(u64::from(u32::MAX)),
            ),
            "statement_timeout",
        ),
    ] {
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let control = tx.manage_operation(&key).unwrap();
        tx.query(
            "SELECT pg_catalog.pg_advisory_xact_lock($1,$2)",
            &[
                &erp_modules::OPERATION_LOCK.0,
                &erp_modules::OPERATION_LOCK.1,
            ],
        )
        .await
        .unwrap();
        tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await.unwrap();
        let result = tx
            .operation_reopen_locked(
                &control,
                &[("public".into(), "b8_wait_bound".into())],
                erp_modules::OPERATION_LOCK,
                erp_sql::ddl::SCHEMA_LOCK_KEY,
                limits,
            )
            .await;
        tx.operation_rollback(&control).await.unwrap();
        errors.push((name, result));
    }
    db.cleanup().await.unwrap();
    for (name, result) in errors {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!("Lock-first lifecycle {name} must fit PostgreSQL's positive millisecond range")
        );
    }
}
