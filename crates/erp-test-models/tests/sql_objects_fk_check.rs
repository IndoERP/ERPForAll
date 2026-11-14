// SPDX-License-Identifier: MIT OR Apache-2.0
//! B6 real PostgreSQL acceptance. Native fixture models intentionally ship no default dataset.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod support;
use erp_fields::{ErrorKind, Result};
use erp_orm::{Orm, SUPERUSER_ID, SchemaSyncOptions, Signal, StaticPolicy};
use erp_registry::*;
use erp_test_models::base_users;
use erp_testkit::Harness;
use std::sync::Arc;
use support::{one, rows};
static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_fk",
    depends: &["base"],
    exact_only: false,
};
const FK_MESSAGE: &str = "The database refused the change: Other records still point to the record you want to delete.\n\nReferring model: 'fk.child' (fk.child)\nConstraint: 'fk_pair'\nArchive the record instead of deleting it.";
fn leak<T>(v: T) -> &'static T {
    Box::leak(Box::new(v))
}
fn fk(action: SqlForeignKeyAction, deferral: SqlDeferral) -> SqlObjectDecl {
    SqlObjectDecl::foreign_key(
        ADDON.name,
        "fk",
        SqlForeignKeyDecl::new(
            "fk_pair",
            "fk.child",
            &["company", "reference_id"],
            "fk.parent",
            &["company", "id"],
            SqlForeignKeyTarget::Key("_company_id"),
        )
        .on_delete(action)
        .deferral(deferral),
    )
}
fn build(decls: Vec<SqlObjectDecl>) -> Arc<Registry> {
    build_with_check(decls, "CHECK (company > 0)")
}

fn build_with_check(decls: Vec<SqlObjectDecl>, check: &'static str) -> Arc<Registry> {
    build_fixture(decls, check, false)
}

fn build_fixture(decls: Vec<SqlObjectDecl>, check: &'static str, wide: bool) -> Arc<Registry> {
    let company = || {
        if wide {
            FieldDecl::many2one("company", "fk.parent").required(true)
        } else {
            FieldDecl::integer("company").required(true)
        }
    };
    let models = [
        ModelDecl::new(ADDON.name, "fk.parent")
            .log_access(false)
            .fields(leak([company()]))
            .table_objects(
                const {
                    &[TableObjectDecl {
                        name: "_company_id",
                        kind: TableObjectKind::Key(KeyDecl::new(
                            &["company", "id"],
                            "Unique pair.",
                        )),
                    }]
                },
            ),
        ModelDecl::new(ADDON.name, "fk.child")
            .log_access(false)
            .fields(leak([
                company(),
                FieldDecl::many2one("reference_id", "fk.parent"),
                FieldDecl::char("note"),
            ]))
            .table_objects(leak([TableObjectDecl {
                name: "_positive",
                kind: TableObjectKind::Constraint {
                    definition: check,
                    message: "Company must be positive.",
                },
            }])),
    ];
    let keys = Box::leak(
        decls
            .iter()
            .map(SqlObjectDecl::key)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls(models.into_iter().map(leak))
            .require_sql_objects([leak(SqlObjectManifest::new(ADDON.name, keys))])
            .with_sql_objects(decls.into_iter().map(leak))
            .linked(false)
            .build()
            .unwrap(),
    )
}
fn registry() -> Arc<Registry> {
    build(vec![fk(
        SqlForeignKeyAction::SetNull(&["reference_id"]),
        SqlDeferral::NotDeferrable,
    )])
}
async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    h
}
async fn tx(h: &Harness, registry: Arc<Registry>) -> erp_orm::Transaction {
    let c = h.db().client().await.unwrap();
    let signal = Signal::read(&c).await.unwrap();
    drop(c);
    Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(signal))
        .build()
        .unwrap()
        .begin(SUPERUSER_ID)
        .await
        .unwrap()
}
async fn sync(
    h: &Harness,
    registry: Arc<Registry>,
    options: SchemaSyncOptions,
) -> Result<Vec<String>> {
    let mut tx = tx(h, registry).await;
    tx.capture_statements(true);
    let result = tx.init_models_with(&[], &options).await;
    match result {
        Ok(done) => {
            assert!(done.is_complete());
            let sent = tx.captured().to_vec();
            tx.commit().await?;
            Ok(sent)
        }
        Err(e) => {
            tx.rollback().await.unwrap();
            Err(e)
        }
    }
}
fn lifecycle() -> SchemaSyncOptions {
    SchemaSyncOptions::inline().lifecycle(true)
}
async fn cell(h: &Harness, sql: &str) -> Option<String> {
    let mut tx = h.begin().await.unwrap();
    let result = one(&mut tx, sql).await;
    tx.rollback().await.unwrap();
    result
}
async fn snapshot(h: &Harness) -> Vec<Vec<Option<String>>> {
    let mut tx = h.begin().await.unwrap();
    let result = rows(
        &mut tx,
        "SELECT row_to_json(o)::text FROM erp_schema_sql_object o ORDER BY addon,key",
    )
    .await;
    tx.rollback().await.unwrap();
    result
}
async fn validation(h: &Harness, r: Arc<Registry>, sql: &str, state: &str, constraint: &str) {
    let mut t = tx(h, r).await;
    let e = t.execute(sql, &[]).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation, "{e:?}");
    assert_eq!(e.sqlstate(), Some(state));
    assert_eq!(e.constraint(), Some(constraint));
    assert_eq!(e.message(), FK_MESSAGE);
    assert!(!e.message().contains("SENSITIVE_ROW_VALUE"));
    t.rollback().await.unwrap();
}
#[tokio::test]
async fn selective_null_preserves_company_and_standard_error() {
    let h = harness().await;
    let r = registry();
    sync(&h, r.clone(), lifecycle()).await.unwrap();
    let mut t = tx(&h, r.clone()).await;
    t.batch("INSERT INTO fk_parent(id,company) VALUES (1,1); INSERT INTO fk_child(company,reference_id,note) VALUES (1,1,'SENSITIVE_ROW_VALUE')").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    validation(
        &h,
        r.clone(),
        "INSERT INTO fk_child(company,reference_id,note) VALUES (2,1,'SENSITIVE_ROW_VALUE')",
        "23503",
        "fk_pair",
    )
    .await;
    let mut t = tx(&h, r.clone()).await;
    t.execute("DELETE FROM fk_parent WHERE id=1", &[])
        .await
        .unwrap();
    t.commit().await.unwrap();
    drop(t);
    assert_eq!(cell(&h, "SELECT company::text || '/' || coalesce(reference_id::text,'NULL') || '/' || note FROM fk_child").await.as_deref(), Some("1/NULL/SENSITIVE_ROW_VALUE"));
    assert_eq!(cell(&h, "SELECT convalidated::text || '/' || cardinality(confdelsetcols)::text FROM pg_constraint WHERE conrelid='fk_child'::regclass AND conname='fk_pair'").await.as_deref(), Some("true/1"));
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn atomic_order_repeat_and_late_failure() {
    let h = harness().await;
    let r = registry();
    let sent = sync(&h, r.clone(), lifecycle()).await.unwrap();
    let key = sent
        .iter()
        .position(|s| s.contains("ADD CONSTRAINT \"fk_parent_company_id\""))
        .unwrap();
    let fk = sent
        .iter()
        .position(|s| s.contains("ADD CONSTRAINT \"fk_pair\""))
        .unwrap();
    assert!(key < fk);
    let before = snapshot(&h).await;
    let oid = cell(&h, "SELECT oid::text FROM pg_constraint WHERE conrelid='fk_child'::regclass AND conname='fk_pair'").await;
    let sent = sync(&h, r.clone(), lifecycle()).await.unwrap();
    assert!(
        sent.iter().all(|s| !s.starts_with("CREATE ")
            && !s.starts_with("ALTER ")
            && !s.starts_with("DROP ")),
        "{sent:#?}"
    );
    assert_eq!(snapshot(&h).await, before);
    assert_eq!(cell(&h, "SELECT oid::text FROM pg_constraint WHERE conrelid='fk_child'::regclass AND conname='fk_pair'").await, oid);
    h.cleanup().await.unwrap();
    let h = harness().await;
    // An installed base schema has rows that violate the newly declared composite FK.
    sync(&h, build(vec![]), lifecycle()).await.unwrap();
    let mut t = tx(&h, build(vec![])).await;
    t.batch("INSERT INTO fk_parent(id,company) VALUES (1,1); INSERT INTO fk_child(company,reference_id) VALUES (2,1)").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let e = sync(&h, r, lifecycle()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.sqlstate(), Some("23503"));
    assert_eq!(
        e.message(),
        format!("SQL object 'test_fk/fk': installing failed: {FK_MESSAGE}")
    );
    assert_eq!(cell(&h, "SELECT count(*)::text FROM pg_constraint WHERE conname='fk_pair' AND conrelid='fk_child'::regclass").await.as_deref(), Some("0"));
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM fk_child")
            .await
            .as_deref(),
        Some("1")
    );
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn deferred_fk_checks_at_commit_and_when_forced() {
    let h = harness().await;
    let r = build(vec![fk(
        SqlForeignKeyAction::NoAction,
        SqlDeferral::InitiallyDeferred,
    )]);
    sync(&h, r.clone(), lifecycle()).await.unwrap();
    let mut t = tx(&h, r.clone()).await;
    t.batch("INSERT INTO fk_parent(id,company) VALUES(1,1)")
        .await
        .unwrap();
    t.commit().await.unwrap();
    drop(t);
    let mut t = tx(&h, r.clone()).await;
    t.batch("INSERT INTO fk_child(company,reference_id) VALUES(2,1)")
        .await
        .unwrap();
    let e = t.commit().await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.sqlstate(), Some("23503"));
    assert_eq!(e.message(), FK_MESSAGE);
    drop(t);
    let mut t = tx(&h, r).await;
    t.batch("INSERT INTO fk_child(company,reference_id) VALUES(2,1)")
        .await
        .unwrap();
    let e = t
        .execute("SET CONSTRAINTS fk_pair IMMEDIATE", &[])
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.sqlstate(), Some("23503"));
    t.rollback().await.unwrap();
    drop(t);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn changed_fk_shape_and_unowned_constraint_refuse() {
    let h = harness().await;
    let r = registry();
    sync(&h, r.clone(), lifecycle()).await.unwrap();
    let before = snapshot(&h).await;
    let e = sync(
        &h,
        build(vec![fk(
            SqlForeignKeyAction::Restrict,
            SqlDeferral::NotDeferrable,
        )]),
        lifecycle(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object 'test_fk/fk': delete action changed; explicit migration required"
    );
    assert_eq!(snapshot(&h).await, before);
    let mut t = h.begin().await.unwrap();
    t.batch("DELETE FROM erp_schema_sql_object_dependency WHERE addon='test_fk'; DELETE FROM erp_schema_sql_object WHERE addon='test_fk'").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let e = sync(&h, r, lifecycle()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object 'test_fk/fk': constraint \"fk_pair\" on \"fk_child\" exists but is not owned by SQL object provenance; adopting it needs an explicit migration"
    );
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn native_check_keeps_declared_safe_text() {
    let h = harness().await;
    let r = registry();
    sync(&h, r.clone(), lifecycle()).await.unwrap();
    let mut t = tx(&h, r).await;
    let e = t
        .execute(
            "INSERT INTO fk_child(company,note) VALUES(-1,'SENSITIVE_ROW_VALUE')",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.sqlstate(), Some("23514"));
    assert_eq!(
        e.message(),
        "The database refused the change: Company must be positive."
    );
    t.rollback().await.unwrap();
    drop(t);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn legacy_catalog_upgrade_preserves_condition() {
    let h = harness().await;
    let cond = SqlObjectDecl::condition(
        ADDON.name,
        "condition",
        SqlConditionDecl::new("ERP09", "fk_condition", "Safe.").kind(SqlConditionKind::Validation),
    );
    sync(&h, build(vec![cond]), lifecycle()).await.unwrap();
    let before = snapshot(&h).await;
    let mut t = h.begin().await.unwrap();
    t.batch("ALTER TABLE erp_schema_sql_object DROP CONSTRAINT erp_schema_sql_object_kind_check; ALTER TABLE erp_schema_sql_object ADD CONSTRAINT erp_schema_sql_object_kind_check CHECK(kind IN ('function','trigger','condition'))").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let legacy = cell(&h, "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='erp_schema_sql_object'::regclass AND conname='erp_schema_sql_object_kind_check'").await;
    let upgraded = || {
        build(vec![
            cond,
            fk(SqlForeignKeyAction::NoAction, SqlDeferral::NotDeferrable),
        ])
    };
    let mut t = tx(&h, build(vec![cond])).await;
    t.batch("INSERT INTO fk_parent(id,company) VALUES(1,1); INSERT INTO fk_child(company,reference_id) VALUES(2,1)").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let e = sync(&h, upgraded(), lifecycle()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.sqlstate(), Some("23503"));
    assert_eq!(
        e.message(),
        format!("SQL object 'test_fk/fk': installing failed: {FK_MESSAGE}")
    );
    assert_eq!(cell(&h, "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='erp_schema_sql_object'::regclass AND conname='erp_schema_sql_object_kind_check'").await, legacy);
    assert_eq!(snapshot(&h).await, before);
    let mut t = tx(&h, build(vec![cond])).await;
    t.execute("DELETE FROM fk_child", &[]).await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    sync(
        &h,
        build(vec![
            cond,
            fk(SqlForeignKeyAction::NoAction, SqlDeferral::NotDeferrable),
        ]),
        lifecycle(),
    )
    .await
    .unwrap();
    let after = snapshot(&h).await;
    assert_eq!(after.first(), before.first());
    assert_eq!(after.len(), before.len() + 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_integrity_is_refused_before_mutation() {
    let h = harness().await;
    sync(&h, build(vec![]), lifecycle()).await.unwrap();
    let mut t = tx(&h, build(vec![])).await;
    t.batch("INSERT INTO fk_parent(id,company) VALUES(1,1); INSERT INTO fk_child(company,reference_id) VALUES(1,1)").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let e = sync(
        &h,
        registry(),
        SchemaSyncOptions::new().lifecycle(true).large_table_rows(0),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object 'test_fk/fk': deferred integrity installation requires in a later release"
    );
    assert_eq!(cell(&h,"SELECT count(*)::text FROM pg_constraint WHERE conrelid='fk_child'::regclass AND conname='fk_pair'").await.as_deref(),Some("0"));
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn tampered_integrity_is_not_adopted() {
    let h = harness().await;
    let r = registry();
    sync(&h, r.clone(), lifecycle()).await.unwrap();
    let mut t = h.begin().await.unwrap();
    t.batch("ALTER TABLE fk_child DROP CONSTRAINT fk_pair; ALTER TABLE fk_child ADD CONSTRAINT fk_pair FOREIGN KEY(company,reference_id) REFERENCES fk_parent(company,id) ON DELETE CASCADE").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let e = sync(&h, r.clone(), lifecycle()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object 'test_fk/fk': installed integrity differs; reconcile through addon upgrade"
    );
    let mut t = h.begin().await.unwrap();
    t.batch("ALTER TABLE erp_schema_sql_object DROP CONSTRAINT erp_schema_sql_object_kind_check; ALTER TABLE erp_schema_sql_object ADD CONSTRAINT erp_schema_sql_object_kind_check CHECK(kind <> '')").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let e = sync(&h, r, lifecycle()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object catalogue: unknown kind CHECK shape; explicit migration required"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn sql_objects_only_path_respects_large_table_admission() {
    let h = harness().await;
    sync(&h, build(vec![]), lifecycle()).await.unwrap();
    let mut t = tx(&h, build(vec![])).await;
    t.batch("INSERT INTO fk_parent(id,company) VALUES(1,1); INSERT INTO fk_child(company,reference_id) VALUES(1,1)").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let mut t = tx(&h, registry()).await;
    let e = t
        .init_sql_objects(&SchemaSyncOptions::new().lifecycle(true).large_table_rows(0))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object 'test_fk/fk': deferred integrity installation requires in a later release"
    );
    t.rollback().await.unwrap();
    drop(t);
    assert_eq!(cell(&h,"SELECT count(*)::text FROM pg_constraint WHERE conrelid='fk_child'::regclass AND conname='fk_pair'").await.as_deref(),Some("0"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fresh_integrity_install_rolls_back_on_late_check_error() {
    let h = harness().await;
    let before = cell(&h, "SELECT to_regclass('erp_schema_sql_object')::text").await;
    let r = build_with_check(
        vec![fk(
            SqlForeignKeyAction::NoAction,
            SqlDeferral::NotDeferrable,
        )],
        "CHECK (b6_missing(company))",
    );
    let e = sync(&h, r, lifecycle()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Database);
    assert_eq!(e.sqlstate(), Some("42883"));
    assert_eq!(e.message(), "function b6_missing(integer) does not exist");
    assert_eq!(
        cell(&h, "SELECT to_regclass('fk_parent')::text").await,
        None
    );
    assert_eq!(cell(&h, "SELECT to_regclass('fk_child')::text").await, None);
    assert_eq!(
        cell(&h, "SELECT to_regclass('erp_schema_sql_object')::text").await,
        before
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_key_column_types_require_explicit_migration() {
    let h = harness().await;
    let original = registry();
    sync(&h, original.clone(), lifecycle()).await.unwrap();
    let mut t = tx(&h, original.clone()).await;
    t.batch("INSERT INTO fk_parent(id,company) VALUES(1,1); INSERT INTO fk_child(company,reference_id) VALUES(1,1)").await.unwrap();
    t.commit().await.unwrap();
    drop(t);
    let wide = build_fixture(
        vec![fk(
            SqlForeignKeyAction::SetNull(&["reference_id"]),
            SqlDeferral::NotDeferrable,
        )],
        "CHECK (company > 0)",
        true,
    );
    let e = sync(&h, wide, lifecycle()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "SQL object 'test_fk/fk': column types changed; explicit migration required"
    );
    assert_eq!(cell(&h,"SELECT format_type(atttypid,atttypmod) FROM pg_attribute WHERE attrelid='fk_parent'::regclass AND attname='company'").await.as_deref(),Some("integer"));
    sync(&h, original, lifecycle()).await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn primary_key_target_uses_actual_index_identity() {
    let h = harness().await;
    let parent = ModelDecl::new(ADDON.name, "fk.long_parent")
        .log_access(false)
        .table("b6_parent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let child = ModelDecl::new(ADDON.name, "fk.long_child")
        .log_access(false)
        .fields(const { &[FieldDecl::many2one("reference_id", "fk.long_parent")] });
    let reference = SqlObjectDecl::foreign_key(
        ADDON.name,
        "primary",
        SqlForeignKeyDecl::new(
            "b6_primary",
            "fk.long_child",
            &["reference_id"],
            "fk.long_parent",
            &["id"],
            SqlForeignKeyTarget::PrimaryKey,
        ),
    );
    let r = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls([leak(parent), leak(child)])
            .require_sql_objects([leak(SqlObjectManifest::new(ADDON.name, &["primary"]))])
            .with_sql_objects([leak(reference)])
            .linked(false)
            .build()
            .unwrap(),
    );
    sync(&h, r.clone(), lifecycle()).await.unwrap();
    sync(&h, r, lifecycle()).await.unwrap();
    assert_eq!(cell(&h,"SELECT convalidated::text FROM pg_constraint WHERE conrelid='fk_long_child'::regclass AND conname='b6_primary'").await.as_deref(),Some("true"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_key_lifecycle_subset_installs_at_the_later_table_step() {
    let h = harness().await;
    let registry = registry();
    let parent = registry.model("fk.parent").unwrap().id;
    let child = registry.model("fk.child").unwrap().id;
    let mut tx = tx(&h, registry).await;
    let options = lifecycle();
    assert!(
        tx.init_models_with(&[parent], &options)
            .await
            .unwrap()
            .is_complete()
    );
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM erp_schema_sql_object WHERE addon='test_fk'"
        )
        .await
        .as_deref(),
        Some("0")
    );
    let error = tx.refuse_deferred_sql_objects(&options).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_fk/fk': deferred installation was not completed by any schema step of this operation"
    );
    assert!(
        tx.init_models_with(&[child], &options)
            .await
            .unwrap()
            .is_complete()
    );
    tx.refuse_deferred_sql_objects(&options).await.unwrap();
    assert_eq!(one(&mut tx, "SELECT convalidated::text FROM pg_constraint WHERE conrelid='fk_child'::regclass AND conname='fk_pair'").await.as_deref(), Some("true"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn table_options_refuse_partition_recreation_before_ddl() {
    let h = harness().await;
    let registry = |partitioned| {
        let mut model =
            ModelDecl::new(ADDON.name, "fk.cache").fields(const { &[FieldDecl::char("name")] });
        if partitioned {
            model = model.partition(PartitionDecl::new(
                PartitionInterval::Day,
                2,
                PartitionRetain::Intervals(3),
            ));
        }
        Arc::new(
            RegistryBuilder::new([&base_users::ADDON, &ADDON])
                .with_decls([leak(model)])
                .require_sql_objects([leak(SqlObjectManifest::new(ADDON.name, &["cache"]))])
                .with_sql_objects([leak(SqlObjectDecl::table_options(
                    ADDON.name,
                    "cache",
                    SqlTableOptionsDecl::new("fk.cache", SqlTablePersistence::Logged),
                ))])
                .linked(false)
                .build()
                .unwrap(),
        )
    };
    let original = registry(false);
    sync(&h, original.clone(), lifecycle()).await.unwrap();
    let before = snapshot(&h).await;
    let oid = cell(&h, "SELECT 'fk_cache'::regclass::oid::text").await;
    let error = sync(&h, registry(true), lifecycle()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "SQL object 'test_fk/cache': referenced table 'fk_cache' is recreated by partition synchronization; explicit migration required"
    );
    assert_eq!(
        cell(&h, "SELECT 'fk_cache'::regclass::oid::text").await,
        oid
    );
    assert_eq!(snapshot(&h).await, before);
    sync(&h, original, lifecycle()).await.unwrap();
    h.cleanup().await.unwrap();
}
