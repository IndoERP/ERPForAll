// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon-owned SQL objects installed by schema sync, slice B3 (plan
//! ): phase order, provenance, body-only
//! upgrades, shape refusals, catalog-derived bodies, the core-rendered search path (A1) and the
//! lifecycle-only rule (A6). Real PostgreSQL, one database per test.
//!
//! Fixture addon `test_sqlobj` and its models are test fixtures: they ship no default data.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_fields::{Error, ErrorKind, GeneratedStorage};
use erp_orm::{Orm, SUPERUSER_ID, SchemaSyncOptions, Signal, SignalKey, StaticPolicy};
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder, SqlColumnSet,
    SqlCompareColumn, SqlConditionDecl, SqlDeferral, SqlDep, SqlEvent, SqlFunctionDecl,
    SqlLanguage, SqlLevel, SqlObjectDecl, SqlObjectKind, SqlObjectManifest, SqlResult, SqlScalar,
    SqlSecurity, SqlTiming, SqlTriggerDecl, SqlType,
};
use erp_sdk::Transaction;
use erp_sql::sqlobj;
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::{one, rows};

static SQLOBJ: AddonDescriptor = AddonDescriptor {
    name: "test_sqlobj",
    depends: &["base"],
    exact_only: false,
};

const ADDON: &str = "test_sqlobj";
const MOVE: &str = "test_sqlobj.move";
const MOVE_TABLE: &str = "test_sqlobj_move";
const OTHER: &str = "test_sqlobj.other";
const TRUNCATE_MESSAGE: &str = "Moves cannot be truncated.";
const NEGATIVE_MESSAGE: &str = "Amounts cannot be negative.";

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn leak_slice<T>(value: Vec<T>) -> &'static [T] {
    Box::leak(value.into_boxed_slice())
}

const MOVE_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::integer("amount"),
    FieldDecl::char("note"),
    FieldDecl::char("xact"),
];

fn move_model(extra: &[FieldDecl]) -> ModelDecl {
    let mut fields = MOVE_FIELDS.to_vec();
    fields.extend_from_slice(extra);
    ModelDecl::new(ADDON, MOVE).fields(leak_slice(fields))
}

fn other_model() -> ModelDecl {
    ModelDecl::new(ADDON, OTHER)
        .fields(const { &[FieldDecl::char("name"), FieldDecl::char("xact")] })
}

fn function(key: &'static str, decl: SqlFunctionDecl) -> SqlObjectDecl {
    SqlObjectDecl::function(ADDON, key, decl)
}

fn trigger_fn(name: &'static str, body: &'static str) -> SqlFunctionDecl {
    SqlFunctionDecl::new(name, SqlLanguage::PlPgSql, SqlResult::Trigger, body)
        .deps(&[SqlDep::Model(MOVE)])
}

fn trigger(
    name: &'static str,
    function: &'static str,
    shape: (SqlTiming, SqlEvent, SqlLevel),
) -> SqlTriggerDecl {
    SqlTriggerDecl::new(
        name,
        MOVE,
        (ADDON, function),
        shape.0,
        leak_slice(vec![shape.1]),
        shape.2,
    )
}

const BEFORE_INSERT_ROW: (SqlTiming, SqlEvent, SqlLevel) =
    (SqlTiming::Before, SqlEvent::Insert, SqlLevel::Row);
const BEFORE_TRUNCATE: (SqlTiming, SqlEvent, SqlLevel) =
    (SqlTiming::Before, SqlEvent::Truncate, SqlLevel::Statement);
const AFTER_INSERT_ROW: (SqlTiming, SqlEvent, SqlLevel) =
    (SqlTiming::After, SqlEvent::Insert, SqlLevel::Row);

/// The guards: a BEFORE INSERT row stamp, a BEFORE TRUNCATE guard and a
/// deferred AFTER INSERT constraint check, with their conditions.
fn guards() -> Vec<SqlObjectDecl> {
    vec![
        function(
            "fn_stamp",
            trigger_fn(
                "sqlobj_stamp",
                "BEGIN NEW.xact := pg_current_xact_id()::text; RETURN NEW; END",
            ),
        ),
        function(
            "fn_no_truncate",
            trigger_fn(
                "sqlobj_no_truncate",
                "BEGIN RAISE EXCEPTION USING ERRCODE = 'ERP01', CONSTRAINT = 'sqlobj_no_truncate', \
                 MESSAGE = 'Moves cannot be truncated.'; END",
            ),
        ),
        function(
            "fn_positive",
            trigger_fn(
                "sqlobj_positive",
                "BEGIN IF NEW.amount < 0 THEN RAISE EXCEPTION USING ERRCODE = 'ERP02', \
                 CONSTRAINT = 'sqlobj_negative', MESSAGE = 'Amounts cannot be negative.'; \
                 END IF; RETURN NULL; END",
            ),
        ),
        SqlObjectDecl::trigger(
            ADDON,
            "trg_stamp",
            trigger("sqlobj_stamp", "fn_stamp", BEFORE_INSERT_ROW),
        ),
        SqlObjectDecl::trigger(
            ADDON,
            "trg_no_truncate",
            trigger("sqlobj_no_truncate", "fn_no_truncate", BEFORE_TRUNCATE),
        ),
        SqlObjectDecl::trigger(
            ADDON,
            "trg_positive",
            trigger("sqlobj_positive", "fn_positive", AFTER_INSERT_ROW)
                .constraint(SqlDeferral::InitiallyDeferred),
        ),
        SqlObjectDecl::condition(
            ADDON,
            "cond_truncate",
            SqlConditionDecl::new("ERP01", "sqlobj_no_truncate", TRUNCATE_MESSAGE),
        ),
        SqlObjectDecl::condition(
            ADDON,
            "cond_negative",
            SqlConditionDecl::new("ERP02", "sqlobj_negative", NEGATIVE_MESSAGE),
        ),
    ]
}

fn label(version: &'static str) -> Vec<SqlObjectDecl> {
    let body = match version {
        "v1" => "BEGIN NEW.name := 'v1'; RETURN NEW; END",
        _ => "BEGIN NEW.name := 'v2'; RETURN NEW; END",
    };
    vec![
        function("fn_label", trigger_fn("sqlobj_label", body)),
        SqlObjectDecl::trigger(
            ADDON,
            "trg_label",
            trigger("sqlobj_label", "fn_label", BEFORE_INSERT_ROW),
        ),
    ]
}

fn compare_body(columns: &[SqlCompareColumn]) -> String {
    let list: Vec<String> = columns
        .iter()
        .map(|c| format!("{} {}", c.quoted(), c.sql_type()))
        .collect();
    format!("BEGIN RETURN {}; END", erp_sql::literal(&list.join(", ")))
}

/// A body PL/pgSQL refuses when the function is created (`check_function_bodies` stays on).
fn broken_body(_columns: &[SqlCompareColumn]) -> String {
    "BEGIN RETURN 'x' END".to_owned()
}

fn compare(allow: &'static [&'static str]) -> SqlObjectDecl {
    function(
        "fn_compare",
        SqlFunctionDecl::catalog_derived(
            "sqlobj_compare",
            SqlLanguage::PlPgSql,
            SqlResult::Scalar(SqlScalar::Text),
            SqlColumnSet::new(MOVE, allow),
            compare_body,
        ),
    )
}

fn broken() -> SqlObjectDecl {
    function(
        "fn_zbroken",
        SqlFunctionDecl::catalog_derived(
            "sqlobj_zbroken",
            SqlLanguage::PlPgSql,
            SqlResult::Scalar(SqlScalar::Text),
            SqlColumnSet::new(MOVE, &[]),
            broken_body,
        ),
    )
}

/// The registry of `base_users` and `test_sqlobj` with `models` and `decls`, the manifest listing
/// exactly the keys of `decls`.
fn build(models: Vec<ModelDecl>, decls: Vec<SqlObjectDecl>) -> erp_fields::Result<Arc<Registry>> {
    let decls: Vec<&'static SqlObjectDecl> = decls.into_iter().map(leak).collect();
    let mut manifests: Vec<&'static SqlObjectManifest> = Vec::new();
    if !decls.is_empty() {
        let keys: Vec<&'static str> = decls.iter().map(|d| d.key()).collect();
        manifests.push(leak(SqlObjectManifest::new(ADDON, leak_slice(keys))));
    }
    let models: Vec<&'static ModelDecl> = models.into_iter().map(leak).collect();
    RegistryBuilder::new([&base_users::ADDON, &SQLOBJ])
        .with_decls(models)
        .require_sql_objects(manifests)
        .with_sql_objects(decls)
        .linked(false)
        .build()
        .map(Arc::new)
}

/// A database with `base_users` and the signal table.
async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    h
}

/// An ORM with signals on `registry`, on the harness's database.
async fn orm(h: &Harness, registry: Arc<Registry>) -> Orm {
    let client = h.db().client().await.unwrap();
    let applied = Signal::read(&client).await.unwrap();
    drop(client);
    Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(applied))
        .build()
        .unwrap()
}

/// One schema sync of all models with `registry` (lifecycle or not), committed; the statements
/// it sent. On error the transaction is rolled back.
async fn sync_as(
    h: &Harness,
    registry: Arc<Registry>,
    lifecycle: bool,
) -> erp_fields::Result<Vec<String>> {
    let orm = orm(h, registry).await;
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    tx.capture_statements(true);
    let options = SchemaSyncOptions::inline().lifecycle(lifecycle);
    let sync = tx.init_models_with(&[], &options).await?;
    assert!(sync.is_complete());
    let captured = tx.captured().to_vec();
    tx.commit().await?;
    Ok(captured)
}

async fn sync(h: &Harness, registry: Arc<Registry>) -> erp_fields::Result<Vec<String>> {
    sync_as(h, registry, true).await
}

/// The statements that change the schema or data.
fn writes(captured: &[String]) -> Vec<String> {
    captured
        .iter()
        .filter(|s| {
            let head = s.trim_start().to_ascii_uppercase();
            [
                "CREATE", "ALTER", "DROP", "INSERT", "UPDATE", "DELETE", "COMMENT", "TRUNCATE",
            ]
            .iter()
            .any(|w| head.starts_with(w))
        })
        .cloned()
        .collect()
}

async fn generation(h: &Harness) -> u64 {
    let client = h.db().client().await.unwrap();
    Signal::read(&client)
        .await
        .unwrap()
        .get(&SignalKey::Registry)
}

async fn begin(h: &Harness) -> Transaction {
    h.begin().await.unwrap()
}

async fn cell(h: &Harness, sql: &str) -> Option<String> {
    let mut tx = begin(h).await;
    let value = one(&mut tx, sql).await;
    tx.rollback().await.unwrap();
    value
}

/// Every provenance row as JSON text, by identity.
async fn provenance(h: &Harness) -> Vec<Vec<Option<String>>> {
    let mut tx = begin(h).await;
    let all = rows(
        &mut tx,
        "SELECT o.key, row_to_json(o)::text FROM erp_schema_sql_object o ORDER BY o.addon, o.key",
    )
    .await;
    tx.rollback().await.unwrap();
    all
}

async fn provenance_of(h: &Harness, key: &str, column: &str) -> Option<String> {
    cell(
        h,
        &format!(
            "SELECT {column}::text FROM erp_schema_sql_object WHERE addon = 'test_sqlobj' AND \
             key = '{key}'"
        ),
    )
    .await
}

/// The OID of the function object `key` of `registry`, read from `pg_proc`.
async fn function_oid(h: &Harness, registry: &Registry, key: &str) -> Option<String> {
    let SqlObjectKind::Function(f) = registry.sql_objects().get(ADDON, key).unwrap().kind() else {
        panic!("{key} is not a function");
    };
    let sql = format!(
        "SELECT p.oid::text FROM pg_proc p WHERE p.oid = to_regprocedure({})",
        erp_sql::literal(&sqlobj::signature(f))
    );
    cell(h, &sql).await
}

async fn trigger_oid(h: &Harness, table: &str, name: &str) -> Option<String> {
    cell(
        h,
        &format!(
            "SELECT t.oid::text FROM pg_trigger t WHERE t.tgrelid = to_regclass('{table}') AND \
             t.tgname = '{name}'"
        ),
    )
    .await
}

#[track_caller]
fn refused(result: erp_fields::Result<Vec<String>>, message: &str) {
    let error = result.expect_err("the sync must refuse");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), message);
}

/// The server's error under `error`: SQLSTATE, constraint (condition identifier) and message.
fn server_error(error: &Error) -> (String, Option<String>, String) {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = source {
        if let Some(db) = e
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::as_db_error)
        {
            return (
                db.code().code().to_owned(),
                db.constraint().map(str::to_owned),
                db.message().to_owned(),
            );
        }
        source = e.source();
    }
    panic!("no server error under {error:?}");
}

#[tokio::test]
async fn spec_ormx_041_truncate_stamp_and_deferred_check() {
    let h = harness().await;
    let registry = build(vec![move_model(&[])], guards()).unwrap();
    sync(&h, Arc::clone(&registry)).await.unwrap();
    let conditions: BTreeMap<&str, (&str, &str)> = registry
        .sql_objects()
        .objects()
        .iter()
        .filter_map(|o| match o.kind() {
            SqlObjectKind::Condition(c) => Some((o.id().key(), (c.sqlstate(), c.message()))),
            _ => None,
        })
        .collect();

    // the row trigger stamps the inserting transaction's identity
    let mut tx = begin(&h).await;
    tx.batch("INSERT INTO test_sqlobj_move (name, amount) VALUES ('a', 1), ('b', 2)")
        .await
        .unwrap();
    let stamped = rows(
        &mut tx,
        "SELECT count(*)::text, count(*) FILTER (WHERE xact = pg_current_xact_id()::text)::text \
         FROM test_sqlobj_move",
    )
    .await;
    assert_eq!(stamped, vec![vec![Some("2".into()), Some("2".into())]]);
    tx.commit().await.unwrap();

    // TRUNCATE raises the declared condition and deletes nothing
    let mut tx = begin(&h).await;
    let error = tx.batch("TRUNCATE test_sqlobj_move").await.unwrap_err();
    let (state, message) = conditions["cond_truncate"];
    assert_eq!(
        server_error(&error),
        (
            state.to_owned(),
            Some("sqlobj_no_truncate".to_owned()),
            message.to_owned()
        )
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM test_sqlobj_move")
            .await
            .as_deref(),
        Some("2")
    );

    // the deferred check: not at the INSERT, at COMMIT
    let (state, message) = conditions["cond_negative"];
    let expected = (
        state.to_owned(),
        Some("sqlobj_negative".to_owned()),
        message.to_owned(),
    );
    let mut tx = begin(&h).await;
    tx.batch("INSERT INTO test_sqlobj_move (name, amount) VALUES ('neg', -1)")
        .await
        .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(server_error(&error), expected);
    // and when forced immediate
    let mut tx = begin(&h).await;
    tx.batch("INSERT INTO test_sqlobj_move (name, amount) VALUES ('neg', -1)")
        .await
        .unwrap();
    let error = tx
        .batch("SET CONSTRAINTS \"sqlobj_positive\" IMMEDIATE")
        .await
        .unwrap_err();
    assert_eq!(server_error(&error), expected);
    tx.rollback().await.unwrap();
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM test_sqlobj_move")
            .await
            .as_deref(),
        Some("2")
    );

    // always enabled, no transition relations, the constraint trigger deferred
    let mut tx = begin(&h).await;
    let catalog = rows(
        &mut tx,
        "SELECT t.tgname::text, t.tgenabled::text, (t.tgoldtable IS NULL AND t.tgnewtable IS \
         NULL)::text, t.tgdeferrable::text, t.tginitdeferred::text FROM pg_trigger t WHERE \
         t.tgrelid = 'test_sqlobj_move'::regclass AND t.tgname LIKE 'sqlobj%' ORDER BY 1",
    )
    .await;
    tx.rollback().await.unwrap();
    let s = |v: &str| Some(v.to_owned());
    assert_eq!(
        catalog,
        vec![
            vec![
                s("sqlobj_no_truncate"),
                s("A"),
                s("true"),
                s("false"),
                s("false")
            ],
            vec![
                s("sqlobj_positive"),
                s("A"),
                s("true"),
                s("true"),
                s("true")
            ],
            vec![s("sqlobj_stamp"), s("A"), s("true"), s("false"), s("false")],
        ]
    );
}

#[tokio::test]
async fn spec_ormx_042_fresh_install_order_and_atomic_failure() {
    let h = harness().await;
    let mut decls = guards();
    decls.push(compare(&["note"]));
    let registry = build(vec![move_model(&[])], decls.clone()).unwrap();
    let captured = sync(&h, Arc::clone(&registry)).await.unwrap();
    let graph = registry.sql_objects();
    let created = |key: &str| {
        let SqlObjectKind::Function(f) = graph.get(ADDON, key).unwrap().kind() else {
            panic!("{key}");
        };
        let head = format!("CREATE FUNCTION {}", sqlobj::signature(f));
        captured.iter().position(|s| s.starts_with(&head)).unwrap()
    };
    let table = captured
        .iter()
        .position(|s| s.contains(&format!("CREATE TABLE {}", erp_sql::quote(MOVE_TABLE))))
        .unwrap();
    let trigger = captured
        .iter()
        .position(|s| s.starts_with("CREATE TRIGGER \"sqlobj_stamp\""))
        .unwrap();
    let enable = captured
        .iter()
        .position(|s| s.contains("ENABLE ALWAYS TRIGGER \"sqlobj_stamp\""))
        .unwrap();
    // a PL/pgSQL function before its table, the catalog-derived body after it, triggers last
    assert!(created("fn_stamp") < table, "{captured:#?}");
    assert!(table < created("fn_compare"), "{captured:#?}");
    assert!(
        created("fn_compare") < trigger && trigger <= enable,
        "{captured:#?}"
    );
    let installed = provenance(&h).await;
    assert_eq!(installed.len(), graph.objects().len());

    // a later phase fails: the new table, the new function and their provenance roll back
    let mut tx = begin(&h).await;
    tx.batch("INSERT INTO test_sqlobj_move (name, amount) VALUES ('kept', 1)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = generation(&h).await;
    let mut failing = decls.clone();
    failing.push(function(
        "fn_new",
        SqlFunctionDecl::new(
            "sqlobj_new",
            SqlLanguage::PlPgSql,
            SqlResult::Scalar(SqlScalar::Integer),
            "BEGIN RETURN 1; END",
        ),
    ));
    failing.push(broken());
    let failing = build(vec![move_model(&[]), other_model()], failing).unwrap();
    let error = sync(&h, Arc::clone(&failing)).await.unwrap_err();
    // the server's syntax error text follows the fixed prefix
    let prefix = "SQL object 'test_sqlobj/fn_zbroken': installing failed: ";
    assert!(error.message().starts_with(prefix), "{error:?}");
    assert_eq!(error.sqlstate(), Some("42601"));
    assert_eq!(function_oid(&h, &failing, "fn_new").await, None);
    assert_eq!(
        cell(&h, "SELECT to_regclass('test_sqlobj_other')::text").await,
        None
    );
    assert_eq!(provenance(&h).await, installed);
    assert_eq!(generation(&h).await, before);
    assert_eq!(
        cell(&h, "SELECT string_agg(name, ',') FROM test_sqlobj_move")
            .await
            .as_deref(),
        Some("kept")
    );

    // an SQL-language function cannot depend on a table this sync creates (D6)
    let h = harness().await;
    let count = || {
        function(
            "fn_count",
            SqlFunctionDecl::new(
                "sqlobj_count",
                SqlLanguage::Sql,
                SqlResult::Scalar(SqlScalar::BigInt),
                "SELECT count(*) FROM test_sqlobj_move",
            )
            .deps(&[SqlDep::Model(MOVE)]),
        )
    };
    refused(
        sync(&h, build(vec![move_model(&[])], vec![count()]).unwrap()).await,
        "SQL object 'test_sqlobj/fn_count': SQL-language function depends on table \
         'test_sqlobj_move' created by this sync",
    );
    assert_eq!(
        cell(&h, "SELECT to_regclass('test_sqlobj_move')::text").await,
        None
    );
    // once the table exists it installs
    sync(&h, build(vec![move_model(&[])], vec![]).unwrap())
        .await
        .unwrap();
    let with_count = build(vec![move_model(&[])], vec![count()]).unwrap();
    sync(&h, Arc::clone(&with_count)).await.unwrap();
    assert!(function_oid(&h, &with_count, "fn_count").await.is_some());
}

#[tokio::test]
async fn spec_ormx_043_body_update_keeps_identities() {
    let h = harness().await;
    let v1 = build(vec![move_model(&[])], label("v1")).unwrap();
    sync(&h, Arc::clone(&v1)).await.unwrap();
    let function = function_oid(&h, &v1, "fn_label").await.unwrap();
    let trigger = trigger_oid(&h, MOVE_TABLE, "sqlobj_label").await.unwrap();
    let hash1 = provenance_of(&h, "fn_label", "declaration_sha256")
        .await
        .unwrap();
    let installed1 = provenance_of(&h, "fn_label", "installed_sha256")
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    tx.batch("INSERT INTO test_sqlobj_move (name) VALUES ('x')")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let v2 = build(vec![move_model(&[])], label("v2")).unwrap();
    let before = generation(&h).await;
    let captured = sync(&h, Arc::clone(&v2)).await.unwrap();
    assert_eq!(
        writes(&captured)
            .iter()
            .filter(|s| s.starts_with("CREATE OR REPLACE FUNCTION"))
            .count(),
        1,
        "{captured:#?}"
    );
    assert!(generation(&h).await > before);
    assert_eq!(function_oid(&h, &v2, "fn_label").await.unwrap(), function);
    assert_eq!(
        trigger_oid(&h, MOVE_TABLE, "sqlobj_label").await.unwrap(),
        trigger
    );
    let hash2 = provenance_of(&h, "fn_label", "declaration_sha256")
        .await
        .unwrap();
    assert_ne!(hash2, hash1);
    assert_ne!(
        provenance_of(&h, "fn_label", "installed_sha256")
            .await
            .unwrap(),
        installed1
    );
    assert_eq!(
        provenance_of(&h, "fn_label", "revision").await.as_deref(),
        Some("2")
    );
    let mut tx = begin(&h).await;
    tx.batch("INSERT INTO test_sqlobj_move (name) VALUES ('y')")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // old then new behaviour
    assert_eq!(
        cell(
            &h,
            "SELECT string_agg(name, ',' ORDER BY id) FROM test_sqlobj_move"
        )
        .await
        .as_deref(),
        Some("v1,v2")
    );

    // repeat: no DDL, no write, no generation bump, provenance unchanged
    let recorded = provenance(&h).await;
    let before = generation(&h).await;
    let captured = sync(&h, Arc::clone(&v2)).await.unwrap();
    assert_eq!(writes(&captured), Vec::<String>::new());
    assert_eq!(generation(&h).await, before);
    assert_eq!(provenance(&h).await, recorded);
    assert_eq!(function_oid(&h, &v2, "fn_label").await.unwrap(), function);

    // a dependency-only change is a changed declaration: the dependency rows are rewritten
    let edges = "SELECT count(*)::text FROM erp_schema_sql_object_dependency WHERE addon = \
                 'test_sqlobj' AND key = 'fn_label'";
    assert_eq!(cell(&h, edges).await.as_deref(), Some("1"));
    let mut no_deps = label("v2");
    no_deps[0] = SqlObjectDecl::function(
        ADDON,
        "fn_label",
        SqlFunctionDecl::new(
            "sqlobj_label",
            SqlLanguage::PlPgSql,
            SqlResult::Trigger,
            "BEGIN NEW.name := 'v2'; RETURN NEW; END",
        ),
    );
    sync(&h, build(vec![move_model(&[])], no_deps).unwrap())
        .await
        .unwrap();
    assert_eq!(cell(&h, edges).await.as_deref(), Some("0"));
    assert_eq!(
        provenance_of(&h, "fn_label", "revision").await.as_deref(),
        Some("3")
    );
}

#[tokio::test]
async fn spec_ormx_044_shape_changes_need_a_migration() {
    let h = harness().await;
    let calc = |args: &'static [SqlType], result: SqlScalar, security: SqlSecurity| {
        function(
            "fn_calc",
            SqlFunctionDecl::new(
                "sqlobj_calc",
                SqlLanguage::PlPgSql,
                SqlResult::Scalar(result),
                "BEGIN RETURN $1 + 1; END",
            )
            .args(args)
            .security(security),
        )
    };
    const INT: &[SqlType] = &[SqlType::Scalar(SqlScalar::Integer)];
    const BIG: &[SqlType] = &[SqlType::Scalar(SqlScalar::BigInt)];
    let base = || {
        let mut decls = guards();
        decls.push(calc(INT, SqlScalar::Integer, SqlSecurity::Invoker));
        decls
    };
    let models = || vec![move_model(&[]), other_model()];
    let installed = build(models(), base()).unwrap();
    sync(&h, Arc::clone(&installed)).await.unwrap();
    let calc_oid = function_oid(&h, &installed, "fn_calc").await.unwrap();
    let stamp_oid = trigger_oid(&h, MOVE_TABLE, "sqlobj_stamp").await.unwrap();
    let recorded = provenance(&h).await;

    let replace = |key: &str, with: SqlObjectDecl| {
        let mut decls = base();
        let at = decls.iter().position(|d| d.key() == key).unwrap();
        decls[at] = with;
        decls
    };
    let stamp = |shape: (SqlTiming, SqlEvent, SqlLevel), model: &'static str| {
        SqlObjectDecl::trigger(
            ADDON,
            "trg_stamp",
            SqlTriggerDecl::new(
                "sqlobj_stamp",
                model,
                (ADDON, "fn_stamp"),
                shape.0,
                leak_slice(vec![shape.1]),
                shape.2,
            ),
        )
    };
    let variants: Vec<(Vec<SqlObjectDecl>, &str)> = vec![
        (
            replace(
                "fn_calc",
                calc(BIG, SqlScalar::Integer, SqlSecurity::Invoker),
            ),
            "SQL object 'test_sqlobj/fn_calc': argument types changed; explicit migration required",
        ),
        (
            replace(
                "fn_calc",
                calc(INT, SqlScalar::BigInt, SqlSecurity::Invoker),
            ),
            "SQL object 'test_sqlobj/fn_calc': result type changed; explicit migration required",
        ),
        (
            replace(
                "fn_calc",
                calc(INT, SqlScalar::Integer, SqlSecurity::Definer),
            ),
            "SQL object 'test_sqlobj/fn_calc': security mode changed; explicit migration required",
        ),
        (
            replace("trg_stamp", stamp(BEFORE_TRUNCATE, MOVE)),
            "SQL object 'test_sqlobj/trg_stamp': trigger event changed; explicit migration required",
        ),
        (
            replace(
                "trg_stamp",
                SqlObjectDecl::trigger(
                    ADDON,
                    "trg_stamp",
                    trigger("sqlobj_stamp", "fn_stamp", AFTER_INSERT_ROW)
                        .constraint(SqlDeferral::InitiallyDeferred),
                ),
            ),
            "SQL object 'test_sqlobj/trg_stamp': trigger timing changed; explicit migration required",
        ),
        (
            replace("trg_stamp", stamp(BEFORE_INSERT_ROW, OTHER)),
            "SQL object 'test_sqlobj/trg_stamp': trigger table changed; explicit migration required",
        ),
        (
            replace(
                "trg_positive",
                SqlObjectDecl::trigger(
                    ADDON,
                    "trg_positive",
                    trigger("sqlobj_positive", "fn_positive", AFTER_INSERT_ROW)
                        .constraint(SqlDeferral::InitiallyImmediate),
                ),
            ),
            "SQL object 'test_sqlobj/trg_positive': deferral changed; explicit migration required",
        ),
    ];
    for (decls, message) in variants {
        let changed = build(models(), decls).unwrap();
        let before = generation(&h).await;
        refused(sync(&h, changed).await, message);
        assert_eq!(generation(&h).await, before, "{message}");
        assert_eq!(provenance(&h).await, recorded, "{message}");
        assert_eq!(
            function_oid(&h, &installed, "fn_calc").await.as_deref(),
            Some(calc_oid.as_str())
        );
        assert_eq!(
            trigger_oid(&h, MOVE_TABLE, "sqlobj_stamp").await.as_deref(),
            Some(stamp_oid.as_str())
        );
    }
    // the prior installation still works
    assert_eq!(
        cell(&h, "SELECT public.sqlobj_calc(1)::text")
            .await
            .as_deref(),
        Some("2")
    );
    let mut tx = begin(&h).await;
    tx.batch("INSERT INTO test_sqlobj_move (name, amount) VALUES ('a', 1)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        cell(&h, "SELECT (xact IS NOT NULL)::text FROM test_sqlobj_move")
            .await
            .as_deref(),
        Some("true")
    );
}

/// The compared columns the catalog shows for the move table: ordinary user columns in `attnum`
/// order, without `excluded` and without generated or dropped ones.
async fn catalog_columns(h: &Harness, excluded: &str) -> Option<String> {
    cell(
        h,
        &format!(
            "SELECT string_agg('\"' || replace(a.attname, '\"', '\"\"') || '\" ' || \
             format_type(a.atttypid, a.atttypmod), ', ' ORDER BY a.attnum) FROM pg_attribute a \
             WHERE a.attrelid = 'test_sqlobj_move'::regclass AND a.attnum > 0 AND NOT \
             a.attisdropped AND a.attgenerated = '' AND a.attname <> '{excluded}'"
        ),
    )
    .await
}

#[tokio::test]
async fn spec_ormx_050_column_changes_regenerate_the_body() {
    let h = harness().await;
    let v1 = build(vec![move_model(&[])], vec![compare(&["note"])]).unwrap();
    sync(&h, Arc::clone(&v1)).await.unwrap();
    let body1 = cell(&h, "SELECT public.sqlobj_compare()").await.unwrap();
    assert_eq!(Some(body1.clone()), catalog_columns(&h, "note").await);
    assert!(!body1.contains("\"note\""), "{body1}");
    let input1 = provenance_of(&h, "fn_compare", "catalog_input_sha256")
        .await
        .unwrap();
    let body_hash1 = provenance_of(&h, "fn_compare", "body_sha256")
        .await
        .unwrap();
    let oid = function_oid(&h, &v1, "fn_compare").await.unwrap();

    // an ordinary column and a generated one are added: the body follows in the same sync
    const V2: &[FieldDecl] = &[
        FieldDecl::integer("extra"),
        FieldDecl::integer("twice")
            .generated("amount * 2")
            .generated_storage(GeneratedStorage::Stored),
        FieldDecl::integer("vtwice")
            .generated("amount * 2")
            .generated_storage(GeneratedStorage::Virtual),
    ];
    let v2 = build(vec![move_model(V2)], vec![compare(&["note"])]).unwrap();
    let captured = sync(&h, Arc::clone(&v2)).await.unwrap();
    let column = captured
        .iter()
        .position(|s| s.contains("ADD COLUMN \"extra\""))
        .unwrap();
    let body = captured
        .iter()
        .position(|s| s.starts_with("CREATE OR REPLACE FUNCTION \"public\".\"sqlobj_compare\""))
        .unwrap();
    assert!(column < body, "{captured:#?}");
    let body2 = cell(&h, "SELECT public.sqlobj_compare()").await.unwrap();
    assert_eq!(Some(body2.clone()), catalog_columns(&h, "note").await);
    assert!(body2.contains("\"extra\" integer"), "{body2}");
    assert!(!body2.contains("twice"), "{body2}");
    assert!(!body2.contains("vtwice"), "{body2}");
    let input2 = provenance_of(&h, "fn_compare", "catalog_input_sha256")
        .await
        .unwrap();
    assert_ne!(input2, input1);
    assert_ne!(
        provenance_of(&h, "fn_compare", "body_sha256")
            .await
            .unwrap(),
        body_hash1
    );
    assert_eq!(function_oid(&h, &v2, "fn_compare").await.unwrap(), oid);

    // a later failure rolls the column and the body back together
    const V3: &[FieldDecl] = &[
        FieldDecl::integer("extra"),
        FieldDecl::integer("twice")
            .generated("amount * 2")
            .generated_storage(GeneratedStorage::Stored),
        FieldDecl::integer("vtwice")
            .generated("amount * 2")
            .generated_storage(GeneratedStorage::Virtual),
        FieldDecl::integer("extra3"),
    ];
    let v3 = build(vec![move_model(V3)], vec![compare(&["note"]), broken()]).unwrap();
    let error = sync(&h, v3).await.unwrap_err();
    assert!(
        error
            .message()
            .starts_with("SQL object 'test_sqlobj/fn_zbroken': installing failed: "),
        "{error:?}"
    );
    assert_eq!(
        cell(&h, "SELECT public.sqlobj_compare()").await,
        Some(body2)
    );
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM pg_attribute WHERE attrelid = \
             'test_sqlobj_move'::regclass AND attname = 'extra3'"
        )
        .await
        .as_deref(),
        Some("0")
    );
    assert_eq!(
        provenance_of(&h, "fn_compare", "catalog_input_sha256").await,
        Some(input2)
    );

    // an allow-list entry that is not a stored column fails at registry build
    for (field, allow) in [("nope", &["nope"][..]), ("twice", &["twice"][..])] {
        let error = build(
            vec![move_model(V2)],
            vec![compare(leak_slice(allow.to_vec()))],
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!(
                "SQL object 'test_sqlobj/fn_compare': allow-list field 'test_sqlobj.move.{field}' \
                 is not a stored column"
            )
        );
    }
}

/// A1: every installed function carries the core-rendered search path, `pg_temp` last.
#[tokio::test]
async fn a1_search_path_of_every_function() {
    let h = harness().await;
    let mut decls = guards();
    decls.push(compare(&["note"]));
    let registry = build(vec![move_model(&[])], decls).unwrap();
    sync(&h, Arc::clone(&registry)).await.unwrap();
    let current = cell(&h, "SELECT current_schema()::text").await.unwrap();
    let graph = registry.sql_objects();
    let mut checked = 0;
    for object in graph.objects() {
        let SqlObjectKind::Function(f) = object.kind() else {
            continue;
        };
        let schemas = sqlobj::search_path_schemas(graph, object, &current);
        let quoted: Vec<String> = schemas.iter().map(|s| erp_sql::literal(s)).collect();
        let sql = format!(
            "SELECT (p.proconfig = ARRAY['search_path=pg_catalog, ' || (SELECT \
             string_agg(quote_ident(s), ', ' ORDER BY i) FROM unnest(ARRAY[{}]::text[]) WITH \
             ORDINALITY AS u(s, i)) || ', pg_temp'])::text, p.proconfig::text FROM pg_proc p \
             WHERE p.oid = to_regprocedure({})",
            quoted.join(", "),
            erp_sql::literal(&sqlobj::signature(f))
        );
        let mut tx = begin(&h).await;
        let found = rows(&mut tx, &sql).await;
        tx.rollback().await.unwrap();
        assert_eq!(found.len(), 1, "{sql}");
        assert_eq!(found[0][0].as_deref(), Some("true"), "{:?}", found[0][1]);
        assert!(
            found[0][1].as_deref().unwrap().ends_with(", pg_temp\"}"),
            "{found:?}"
        );
        checked += 1;
    }
    assert_eq!(checked, 4);
}

/// A6: only an addon install or upgrade changes SQL objects; with nothing to change any sync
/// passes.
#[tokio::test]
async fn a6_only_lifecycle_syncs_change_objects() {
    let h = harness().await;
    let v1 = build(vec![move_model(&[])], label("v1")).unwrap();
    refused(
        sync_as(&h, Arc::clone(&v1), false).await,
        "SQL object 'test_sqlobj/fn_label': SQL object DDL runs only inside an addon install or \
         upgrade",
    );
    sync(&h, Arc::clone(&v1)).await.unwrap();
    let captured = sync_as(&h, Arc::clone(&v1), false).await.unwrap();
    assert_eq!(writes(&captured), Vec::<String>::new());
    let v2 = build(vec![move_model(&[])], label("v2")).unwrap();
    refused(
        sync_as(&h, v2, false).await,
        "SQL object 'test_sqlobj/fn_label': SQL object DDL runs only inside an addon install or \
         upgrade",
    );

    // a column change that would re-render a catalog-derived body is refused in S2, and the
    // column rolls back with it
    // the installed label objects stay declared (an undeclared one is refused, D11)
    let with_compare = || {
        let mut decls = label("v1");
        decls.push(compare(&["note"]));
        decls
    };
    let derived = build(vec![move_model(&[])], with_compare()).unwrap();
    sync(&h, derived).await.unwrap();
    const EXTRA: &[FieldDecl] = &[FieldDecl::integer("extra")];
    refused(
        sync_as(
            &h,
            build(vec![move_model(EXTRA)], with_compare()).unwrap(),
            false,
        )
        .await,
        "SQL object 'test_sqlobj/fn_compare': SQL object DDL runs only inside an addon install or \
         upgrade",
    );
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM pg_attribute WHERE attrelid = \
             'test_sqlobj_move'::regclass AND attname = 'extra'"
        )
        .await
        .as_deref(),
        Some("0")
    );

    // the entry point for an addon with SQL objects and no table model (plan D9)
    let h = harness().await;
    sync(&h, build(vec![move_model(&[])], vec![]).unwrap())
        .await
        .unwrap();
    let v1 = build(vec![move_model(&[])], label("v1")).unwrap();
    let orm = orm(&h, Arc::clone(&v1)).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let error = tx
        .init_sql_objects(&SchemaSyncOptions::inline())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_sqlobj/fn_label': SQL object DDL runs only inside an addon install or \
         upgrade"
    );
    tx.rollback().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_sql_objects(&SchemaSyncOptions::inline().lifecycle(true))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(function_oid(&h, &v1, "fn_label").await.is_some());
    assert!(trigger_oid(&h, MOVE_TABLE, "sqlobj_label").await.is_some());
    assert_eq!(provenance(&h).await.len(), 2);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_sql_objects(&SchemaSyncOptions::inline())
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// D2, D4, D11 and a missing catalog object: refused before any DDL.
#[tokio::test]
async fn unowned_definer_undeclared_and_missing_objects_are_refused() {
    let h = harness().await;
    sync(&h, build(vec![move_model(&[])], vec![]).unwrap())
        .await
        .unwrap();
    // an object of the same identity that provenance does not own (D2)
    let mut tx = begin(&h).await;
    tx.batch(
        "CREATE FUNCTION public.sqlobj_label() RETURNS trigger LANGUAGE plpgsql AS \
         'BEGIN RETURN NEW; END'",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let v1 = build(vec![move_model(&[])], label("v1")).unwrap();
    let SqlObjectKind::Function(f) = v1.sql_objects().get(ADDON, "fn_label").unwrap().kind() else {
        panic!("fn_label");
    };
    refused(
        sync(&h, Arc::clone(&v1)).await,
        &format!(
            "SQL object 'test_sqlobj/fn_label': function {} exists but is not owned by SQL object \
             provenance; adopting it needs an explicit migration",
            sqlobj::signature(f)
        ),
    );
    let mut tx = begin(&h).await;
    tx.batch("DROP FUNCTION public.sqlobj_label()")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // SECURITY DEFINER needs a configured definer role binding; none is configured by default
    let definer = function(
        "fn_definer",
        SqlFunctionDecl::new(
            "sqlobj_definer",
            SqlLanguage::Sql,
            SqlResult::Scalar(SqlScalar::Integer),
            "SELECT 1",
        )
        .security(SqlSecurity::Definer),
    );
    refused(
        sync(&h, build(vec![move_model(&[])], vec![definer]).unwrap()).await,
        "SQL object 'test_sqlobj/fn_definer': role binding 'definer' is not configured; the \
         operator names its database role in the schema sync role bindings",
    );

    // an installed object no longer declared while its addon is installed (D11)
    sync(&h, Arc::clone(&v1)).await.unwrap();
    refused(
        sync(&h, build(vec![move_model(&[])], vec![]).unwrap()).await,
        "SQL object 'test_sqlobj/fn_label': recorded in SQL object provenance but no longer \
         declared while addon 'test_sqlobj' is installed; removing it needs \
         a later release",
    );

    // a recorded object missing from the catalog
    let mut tx = begin(&h).await;
    tx.batch("DROP TRIGGER sqlobj_label ON test_sqlobj_move")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    refused(
        sync(&h, v1).await,
        "SQL object 'test_sqlobj/trg_label': installed trigger differs; reconcile through addon \
         upgrade",
    );
}
