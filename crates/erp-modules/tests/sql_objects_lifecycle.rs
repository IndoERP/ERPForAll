// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native SQL-object contributions through real module lifecycle operations.
//! Synthetic addons/models have no default data; each test owns its database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod common;
use common::*;
use erp_modules::{
    InitOptions,
    system::{ModuleSystem, ModuleSystemBuilder},
};
use erp_orm::{Orm, SUPERUSER_ID};
use erp_registry::{
    AddonDescriptor, SqlDep, SqlEvent, SqlFunctionDecl, SqlLanguage, SqlLevel, SqlObjectDecl,
    SqlObjectManifest, SqlResult, SqlScalar, SqlTiming, SqlTriggerDecl,
};
use erp_testkit::TestDb;

static A: AddonDescriptor = AddonDescriptor {
    name: "sq_a",
    depends: &["base"],
    exact_only: false,
};
static B: AddonDescriptor = AddonDescriptor {
    name: "sq_b",
    depends: &["sq_a"],
    exact_only: false,
};
static OBJECTS: SqlObjectManifest = SqlObjectManifest::new("sq_a", &["fn_label", "trg_label"]);
static ONLY: SqlObjectManifest = SqlObjectManifest::new("sq_a", &["fn_only"]);
fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}
fn fixture() -> Addons {
    let addons = Addons::new();
    addons
        .add("sq_a", "name='SQL A'\ndepends=['base']\n")
        .add("sq_b", "name='SQL B'\ndepends=['sq_a']\n");
    addons
}
fn module_builder(db: &TestDb, addons: &Addons) -> ModuleSystemBuilder {
    builder(db, addons).code([&A, &B])
}
fn labels(body: &'static str) -> Vec<&'static SqlObjectDecl> {
    vec![
        leak(SqlObjectDecl::function(
            "sq_a",
            "fn_label",
            SqlFunctionDecl::new("sq_a_label", SqlLanguage::PlPgSql, SqlResult::Trigger, body)
                .deps(&[SqlDep::Model("sq_a.item")]),
        )),
        leak(SqlObjectDecl::trigger(
            "sq_a",
            "trg_label",
            SqlTriggerDecl::new(
                "sq_a_label",
                "sq_a.item",
                ("sq_a", "fn_label"),
                SqlTiming::Before,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            ),
        )),
    ]
}
async fn init(system: &ModuleSystem) -> Orm {
    system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap()
        .orm
}
async fn inserted(orm: &Orm) -> String {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let row = tx
        .query(
            "INSERT INTO sq_a_item (name) VALUES ('input') RETURNING name",
            &[],
        )
        .await
        .unwrap();
    let value = row[0].get(0);
    tx.commit().await.unwrap();
    value
}
async fn identity(orm: &Orm) -> Vec<Vec<Option<String>>> {
    rows(orm, "SELECT p.oid::text, t.oid::text, o.revision::text, encode(o.declaration_sha256, 'hex') FROM pg_proc p JOIN pg_trigger t ON t.tgfoid=p.oid JOIN erp_schema_sql_object o ON o.addon='sq_a' AND o.key='fn_label' WHERE p.proname='sq_a_label'").await
}

#[tokio::test]
async fn module_install_body_upgrade_and_repeat_preserve_identity() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let build = |body| {
        module_builder(&db, &addons)
            .decls([model("sq_a", "sq_a.item")])
            .require_sql_objects([&OBJECTS])
            .require_sql_objects([&OBJECTS])
            .with_sql_objects(labels(body))
            .build()
            .unwrap()
    };
    let first = build("BEGIN NEW.name := 'v1'; RETURN NEW; END");
    let root = init(&first).await;
    assert!(
        su(&root).registry().sql_objects().objects().is_empty(),
        "uninstalled addon leaked into base"
    );
    let installed = first
        .button_immediate_install(&su(&root), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(state(&installed.orm, "sq_a").await, "installed");
    assert_eq!(inserted(&installed.orm).await, "v1");
    let before = identity(&installed.orm).await;
    assert_eq!(before.len(), 1);
    let second = build("BEGIN NEW.name := 'v2'; RETURN NEW; END");
    let upgraded = second
        .button_immediate_upgrade(&second.system_env(erp_orm::Context::new()), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(inserted(&upgraded.orm).await, "v2");
    let after = identity(&upgraded.orm).await;
    assert_eq!(before[0][..2], after[0][..2]);
    assert_ne!(before[0][2..], after[0][2..]);
    let repeated = second
        .button_immediate_upgrade(&su(&upgraded.orm), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(identity(&repeated.orm).await, after);
    assert_eq!(inserted(&repeated.orm).await, "v2");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn objects_only_addon_installs_and_upgrades() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let build = |body| {
        module_builder(&db, &addons)
            .require_sql_objects([&ONLY])
            .with_sql_objects([leak(SqlObjectDecl::function(
                "sq_a",
                "fn_only",
                SqlFunctionDecl::new(
                    "sq_a_only",
                    SqlLanguage::PlPgSql,
                    SqlResult::Scalar(SqlScalar::Text),
                    body,
                ),
            ))])
            .build()
            .unwrap()
    };
    let first = build("BEGIN RETURN 'v1'; END");
    let root = init(&first).await;
    let installed = first
        .button_immediate_install(&su(&root), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(
        rows(&installed.orm, "SELECT sq_a_only()").await[0][0].as_deref(),
        Some("v1")
    );
    let second = build("BEGIN RETURN 'v2'; END");
    let upgraded = second
        .button_immediate_upgrade(&second.system_env(erp_orm::Context::new()), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(
        rows(&upgraded.orm, "SELECT sq_a_only()").await[0][0].as_deref(),
        Some("v2")
    );
    assert!(!table_exists(&upgraded.orm, "sq_a_item").await);
    db.cleanup().await.unwrap();
}

static LATER: SqlObjectManifest = SqlObjectManifest::new("sq_b", &["fn_stamp", "trg_stamp"]);
fn later_objects() -> Vec<&'static SqlObjectDecl> {
    vec![
        leak(SqlObjectDecl::function(
            "sq_b",
            "fn_stamp",
            SqlFunctionDecl::new(
                "sq_b_stamp",
                SqlLanguage::PlPgSql,
                SqlResult::Trigger,
                "BEGIN NEW.name := 'later'; RETURN NEW; END",
            )
            .deps(&[SqlDep::Model("sq_b.later")]),
        )),
        leak(SqlObjectDecl::trigger(
            "sq_b",
            "trg_stamp",
            SqlTriggerDecl::new(
                "sq_b_stamp",
                "sq_b.later",
                ("sq_b", "fn_stamp"),
                SqlTiming::Before,
                &[SqlEvent::Insert],
                SqlLevel::Row,
            ),
        )),
    ]
}

#[tokio::test]
async fn two_module_upgrade_defers_objects_until_later_table_exists() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let first = module_builder(&db, &addons)
        .decls([model("sq_a", "sq_a.item")])
        .build()
        .unwrap();
    let root = init(&first).await;
    let installed = first
        .button_immediate_install(&su(&root), &[n("sq_b")])
        .await
        .unwrap();
    assert!(!table_exists(&installed.orm, "sq_b_later").await);
    let next = module_builder(&db, &addons)
        .decls([model("sq_a", "sq_a.item"), model("sq_b", "sq_b.later")])
        .require_sql_objects([&LATER])
        .with_sql_objects(later_objects())
        .build()
        .unwrap();
    let upgraded = next
        .button_immediate_upgrade(&next.system_env(erp_orm::Context::new()), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(upgraded.upgraded, vec![n("sq_a"), n("sq_b")]);
    let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
    let result = tx
        .query(
            "INSERT INTO sq_b_later (name) VALUES ('input') RETURNING name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(result[0].get::<_, String>(0), "later");
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        rows(
            &upgraded.orm,
            "SELECT key FROM erp_schema_sql_object WHERE addon='sq_b' ORDER BY key"
        )
        .await,
        vec![
            vec![Some("fn_stamp".to_owned())],
            vec![Some("trg_stamp".to_owned())]
        ]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn final_guard_refuses_objects_whose_table_never_gets_a_schema_step() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let first = module_builder(&db, &addons).build().unwrap();
    let root = init(&first).await;
    let installed = first
        .button_immediate_install(&su(&root), &[n("sq_b")])
        .await
        .unwrap();
    let next = module_builder(&db, &addons)
        .decls([model("sq_a", "sq_a.item"), model("sq_b", "sq_b.later")])
        .require_sql_objects([&OBJECTS])
        .with_sql_objects(labels("BEGIN RETURN NEW; END"))
        .build()
        .unwrap();
    // B is upgraded alone; A's new model and objects are visible in the staged registry,
    // but A is already installed and its schema step is not scheduled.
    let error = next
        .button_immediate_upgrade(&next.system_env(erp_orm::Context::new()), &[n("sq_b")])
        .await
        .unwrap_err();
    match error {
        erp_modules::error::Error::Orm(error) => {
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "SQL object 'sq_a/fn_label': deferred installation was not completed by any schema step of this operation"
            );
        }
        other => panic!("expected final completion error, got {other:?}"),
    }
    assert!(
        !table_exists(&installed.orm, "sq_b_later").await,
        "earlier successful step must roll back"
    );
    assert!(!table_exists(&installed.orm, "sq_a_item").await);
    assert_eq!(state(&installed.orm, "sq_b").await, "installed");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn selected_addon_missing_expected_registration_is_refused() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = module_builder(&db, &addons)
        .require_sql_objects([&ONLY])
        .build()
        .unwrap();
    let root = init(&system).await;
    let error = system
        .button_immediate_install(&su(&root), &[n("sq_a")])
        .await
        .unwrap_err();
    match error {
        erp_modules::error::Error::Orm(source) => {
            assert_eq!(source.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                source.message(),
                "SQL object 'sq_a/fn_only' required by addon 'sq_a' was not registered"
            );
        }
        other => panic!("expected registry refusal, got {other:?}"),
    }
    assert_eq!(state(&root, "sq_a").await, "uninstalled");
    db.cleanup().await.unwrap();
}

static BASE_OBJECT: SqlObjectManifest = SqlObjectManifest::new("base", &["fn_bootstrap"]);
#[tokio::test]
async fn bootstrap_can_install_base_sql_objects() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = module_builder(&db, &addons)
        .require_sql_objects([&BASE_OBJECT])
        .with_sql_objects([leak(SqlObjectDecl::function(
            "base",
            "fn_bootstrap",
            SqlFunctionDecl::new(
                "sq_bootstrap",
                SqlLanguage::PlPgSql,
                SqlResult::Scalar(SqlScalar::Text),
                "BEGIN RETURN 'bootstrapped'; END",
            ),
        ))])
        .build()
        .unwrap();
    let root = init(&system).await;
    assert_eq!(
        rows(&root, "SELECT sq_bootstrap()").await[0][0].as_deref(),
        Some("bootstrapped")
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn queued_schema_step_keeps_the_installed_sql_object_body() {
    use erp_modules::system::POST_COMMIT_LOCK;
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let first = module_builder(&db, &addons)
        .decls([model("sq_a", "sq_a.item")])
        .require_sql_objects([&OBJECTS])
        .with_sql_objects(labels("BEGIN NEW.name := 'v1'; RETURN NEW; END"))
        .build()
        .unwrap();
    let root = init(&first).await;
    let installed = first
        .button_immediate_install(&su(&root), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(inserted(&installed.orm).await, "v1");
    let indexed_model =
        || {
            leak(erp_registry::ModelDecl::new("sq_a", "sq_a.item").fields(
                const {
                    &[erp_registry::FieldDecl::char("name")
                        .index(Some(erp_fields::IndexKind::Btree))]
                },
            ))
        };
    let enqueue = module_builder(&db, &addons)
        .decls([indexed_model()])
        .require_sql_objects([&OBJECTS])
        .with_sql_objects(labels("BEGIN NEW.name := 'v1'; RETURN NEW; END"))
        .schema_sync_options(erp_orm::SchemaSyncOptions::new().large_table_rows(0))
        .build()
        .unwrap();
    let holder = db.client().await.unwrap();
    holder
        .query_one(
            "SELECT pg_advisory_lock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    let queued = enqueue
        .button_immediate_upgrade(&enqueue.system_env(erp_orm::Context::new()), &[n("sq_a")])
        .await
        .unwrap();
    assert!(queued.post_commit.done.is_empty());
    assert!(!table_exists(&queued.orm, "sq_a_item__name_index").await);
    holder
        .query_one(
            "SELECT pg_advisory_unlock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    drop(holder);
    let next = module_builder(&db, &addons)
        .decls([indexed_model()])
        .require_sql_objects([&OBJECTS])
        .with_sql_objects(labels("BEGIN NEW.name := 'v2'; RETURN NEW; END"))
        .schema_sync_options(erp_orm::SchemaSyncOptions::new().lifecycle(true))
        .build()
        .unwrap();
    let loaded = next.load().await.unwrap();
    assert_eq!(loaded.post_commit_pending.len(), 1);
    let report = next.run_post_commit(&loaded.orm).await;
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(report.stopped.is_none(), "{report:?}");
    assert_eq!(report.done.len(), 1);
    assert!(table_exists(&loaded.orm, "sq_a_item__name_index").await);
    assert_eq!(inserted(&loaded.orm).await, "v1");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn objects_only_step_before_later_module_table() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let first = module_builder(&db, &addons).build().unwrap();
    let root = init(&first).await;
    let installed = first
        .button_immediate_install(&su(&root), &[n("sq_b")])
        .await
        .unwrap();
    assert_eq!(state(&installed.orm, "sq_b").await, "installed");
    let mut objects = later_objects();
    objects.push(leak(SqlObjectDecl::function(
        "sq_a",
        "fn_only",
        SqlFunctionDecl::new(
            "sq_a_only",
            SqlLanguage::PlPgSql,
            SqlResult::Scalar(SqlScalar::Text),
            "BEGIN RETURN 'ready'; END",
        ),
    )));
    let next = module_builder(&db, &addons)
        .decls([model("sq_b", "sq_b.later")])
        .require_sql_objects([&ONLY, &LATER])
        .with_sql_objects(objects)
        .build()
        .unwrap();
    let upgraded = next
        .button_immediate_upgrade(&next.system_env(erp_orm::Context::new()), &[n("sq_a")])
        .await
        .unwrap();
    assert_eq!(
        rows(&upgraded.orm, "SELECT sq_a_only()").await[0][0].as_deref(),
        Some("ready")
    );
    assert!(table_exists(&upgraded.orm, "sq_b_later").await);
    assert_eq!(
        rows(
            &upgraded.orm,
            "SELECT count(*)::text FROM erp_schema_sql_object WHERE addon='sq_b'"
        )
        .await[0][0]
            .as_deref(),
        Some("2")
    );
    db.cleanup().await.unwrap();
}

struct RemoveBootstrapProvenance;
impl erp_modules::LifecycleHook for RemoveBootstrapProvenance {
    fn before_commit<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        _context: erp_modules::LifecycleChange<'a>,
    ) -> erp_orm::BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            tx.execute(
                "DELETE FROM erp_schema_sql_object WHERE addon='base' AND key='fn_bootstrap'",
                &[],
            )
            .await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn initialization_refuses_incomplete_objects_after_callbacks() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = module_builder(&db, &addons)
        .require_sql_objects([&BASE_OBJECT])
        .with_sql_objects([leak(SqlObjectDecl::function(
            "base",
            "fn_bootstrap",
            SqlFunctionDecl::new(
                "sq_bootstrap",
                SqlLanguage::PlPgSql,
                SqlResult::Scalar(SqlScalar::Text),
                "BEGIN RETURN 'ready'; END",
            ),
        ))])
        .lifecycle_hook(std::sync::Arc::new(RemoveBootstrapProvenance))
        .build()
        .unwrap();
    let error = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap_err();
    match error {
        erp_modules::error::Error::Orm(error) => {
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "SQL object 'base/fn_bootstrap': deferred installation was not completed by any schema step of this operation"
            );
        }
        other => panic!("expected final initialization refusal, got {other:?}"),
    }
    let client = db.client().await.unwrap();
    assert!(
        client
            .query_one("SELECT to_regprocedure('sq_bootstrap()')::text", &[])
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .is_none()
    );
    assert!(
        client
            .query_one("SELECT to_regclass('ir_module_module')::text", &[])
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .is_none()
    );
    drop(client);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn publication_registry_selects_installed_objects_and_refuses_pending() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = module_builder(&db, &addons)
        .require_sql_objects([&ONLY])
        .with_sql_objects([leak(SqlObjectDecl::function(
            "sq_a",
            "fn_only",
            SqlFunctionDecl::new(
                "sq_a_only",
                SqlLanguage::Sql,
                SqlResult::Scalar(SqlScalar::Text),
                "SELECT 'only'",
            ),
        ))])
        .build()
        .unwrap();
    let root = init(&system).await;
    let mut tx = root.begin(SUPERUSER_ID).await.unwrap();
    let registry = system.registry_for_publication(&mut tx).await.unwrap();
    assert!(registry.sql_objects().objects().is_empty());
    // A second node's independent connection cannot publish over the first snapshot.
    let mut other = root.begin(SUPERUSER_ID).await.unwrap();
    let busy = system
        .registry_for_publication(&mut other)
        .await
        .unwrap_err();
    assert!(matches!(busy, erp_modules::Error::OperationInProgress));
    assert_eq!(
        busy.to_string(),
        erp_modules::Error::OperationInProgress.to_string()
    );
    other.rollback().await.unwrap();
    drop(other);
    // A serving node's admission only reads: it is not refused while the lock is held (cycle 13).
    let mut reader = root.begin(SUPERUSER_ID).await.unwrap();
    assert!(
        system
            .installed_registry(&mut reader)
            .await
            .unwrap()
            .sql_objects()
            .objects()
            .is_empty()
    );
    reader.rollback().await.unwrap();
    drop(reader);
    tx.rollback().await.unwrap();
    drop(tx);
    let mut recovered = root.begin(SUPERUSER_ID).await.unwrap();
    assert!(
        system
            .registry_for_publication(&mut recovered)
            .await
            .unwrap()
            .sql_objects()
            .objects()
            .is_empty()
    );
    recovered.rollback().await.unwrap();
    drop(recovered);
    let installed = system
        .button_immediate_install(&su(&root), &[n("sq_a")])
        .await
        .unwrap();
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    let registry = system.registry_for_publication(&mut tx).await.unwrap();
    assert_eq!(registry.sql_objects().objects().len(), 1);
    // While the publisher holds the lock, a serving node's admission reads the same installed
    // composition: the installed object, not an empty registry.
    let mut reader = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    let admitted = system.installed_registry(&mut reader).await.unwrap();
    assert_eq!(admitted.sql_objects().objects().len(), 1);
    assert_eq!(
        admitted.sql_objects().objects()[0].id(),
        registry.sql_objects().objects()[0].id()
    );
    reader.rollback().await.unwrap();
    drop(reader);
    tx.batch("UPDATE ir_module_module SET state='to upgrade' WHERE name='sq_a'")
        .await
        .unwrap();
    let error = system.registry_for_publication(&mut tx).await.unwrap_err();
    assert!(
        matches!(&error, erp_modules::Error::Orm(source) if source.kind() == erp_fields::ErrorKind::Programming),
        "{error}"
    );
    assert!(
        error.to_string().contains(
            "Pending module operations require operator recovery before publishing runtime grants"
        ),
        "{error}"
    );
    // A node's admission gets its own sentence and the Concurrency kind, not the publisher's.
    let refused = system.installed_registry(&mut tx).await.unwrap_err();
    let erp_modules::Error::Orm(source) = &refused else {
        panic!("not an ORM refusal: {refused}");
    };
    assert_eq!(source.kind(), erp_fields::ErrorKind::Concurrency);
    assert_eq!(
        source.message(),
        "Pending module operations require operator recovery"
    );
    let rows = tx
        .query("SELECT state FROM ir_module_module WHERE name='sq_a'", &[])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, String>(0), "to upgrade");
    tx.rollback().await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

/// `base` and `sq_b` are upgraded in one run; `sq_b`'s new version adds a model with SQL objects
/// on it and has a `pre` stage, so `base`'s schema step holds the new table back. `base`'s step
/// must not refuse `sq_b`'s objects for the missing table: they wait for `sq_b`'s step, which
/// runs the `pre` migration, creates the table and installs them.
#[tokio::test]
async fn objects_on_a_table_held_for_a_later_pre_stage_wait_for_its_step() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons
        .add("sq_a", "name='SQL A'\nversion='1.0'\ndepends=['base']\n")
        .add("sq_b", "name='SQL B'\nversion='1.0'\ndepends=['sq_a']\n");
    let previous = std::fs::read_dir(addons.path())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                &name,
                &entry.path().canonicalize().unwrap(),
            )
            .unwrap();
            (n(&name), snapshot.digest().to_owned())
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let first = module_builder(&db, &addons).build().unwrap();
    let root = init(&first).await;
    let installed = first
        .button_immediate_install(&su(&root), &[n("sq_b")])
        .await
        .unwrap();
    assert!(!table_exists(&installed.orm, "sq_b_later").await);
    addons
        .add("sq_b", "name='SQL B'\nversion='1.1'\ndepends=['sq_a']\n")
        .file(
            "sq_b",
            "migrations/1.1/pre-a.sql",
            "CREATE TABLE sq_b_pre_ran (id int);\n",
        );
    let next = module_builder(&db, &addons)
        .identity_previous(previous)
        .decls([model("sq_b", "sq_b.later")])
        .require_sql_objects([&LATER])
        .with_sql_objects(later_objects())
        .build()
        .unwrap();
    let upgraded = next
        .button_immediate_upgrade(
            &next.system_env(erp_orm::Context::new()),
            &[n("base"), n("sq_b")],
        )
        .await
        .unwrap_or_else(|e| panic!("upgrade of base and sq_b: {e:?}"));
    for module in ["base", "sq_b"] {
        assert!(upgraded.upgraded.contains(&n(module)), "{module} upgraded");
    }
    // the pre stage ran, so the table was held back at base's step
    assert!(table_exists(&upgraded.orm, "sq_b_pre_ran").await);
    let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
    let result = tx
        .query(
            "INSERT INTO sq_b_later (name) VALUES ('input') RETURNING name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(result[0].get::<_, String>(0), "later");
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}
