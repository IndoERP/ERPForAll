//! Native scalar schema conversion is an explicit migration boundary (A2).
//! These fixture-only SQL inserts/DDL do not enable ORM defaults or generators.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::{ErrorKind, Orm, SUPERUSER_ID, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_native_migration",
    depends: &["base"],
    exact_only: false,
};

fn declaration(field: FieldDecl) -> ModelDecl {
    ModelDecl::new("test_native_migration", "test.native.migration")
        .table("test_native_migration")
        .fields(Box::leak(vec![field].into_boxed_slice()))
}

async fn snapshot(tx: &mut erp_sdk::Transaction) -> (String, Vec<Option<String>>) {
    let kind: String = tx
        .query(
            "SELECT t.typname::text FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
         WHERE a.attrelid = 'test_native_migration'::regclass AND a.attname = 'value'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let values = tx
        .query(
            "SELECT value::text FROM test_native_migration ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    (kind, values)
}

async fn conversion_is_explicit(source: FieldDecl, target: FieldDecl, fixture_value: &str) {
    // Both nonempty and empty tables require an explicit migration. A transaction
    // timezone must never silently reinterpret a naive timestamp as an instant.
    for seed in [true, false] {
        let h = Harness::builder([&base_users::ADDON, &ADDON])
            .with_decls([declaration(source)])
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        tx.execute("SET LOCAL TIME ZONE 'Asia/Jakarta'", &[])
            .await
            .unwrap();
        if seed {
            tx.execute(&format!("INSERT INTO test_native_migration (id, value) VALUES (1, {fixture_value}), (2, NULL)"), &[])
                .await.unwrap();
        }
        let before = snapshot(&mut tx).await;
        tx.commit().await.unwrap();
        let registry = Arc::new(
            RegistryBuilder::new([&base_users::ADDON, &ADDON])
                .with_decls([&*Box::leak(Box::new(declaration(target)))])
                .build()
                .unwrap(),
        );
        let orm = Orm::builder(registry, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.execute("SET LOCAL TIME ZONE 'Asia/Jakarta'", &[])
            .await
            .unwrap();
        tx.capture_statements(true);
        let error = tx
            .init_models(&[])
            .await
            .expect_err("native conversion must require an explicit migration");
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert!(error.message().contains("explicit migration"), "{error}");
        assert!(
            error.message().contains("test.native.migration.value"),
            "{error}"
        );
        assert!(
            !tx.captured()
                .iter()
                .any(|sql| sql.contains("ALTER COLUMN \"value\" TYPE")),
            "native cast DDL must never execute: {:?}",
            tx.captured()
        );
        tx.rollback().await.unwrap();
        let mut tx = h.begin().await.unwrap();
        tx.execute("SET LOCAL TIME ZONE 'Asia/Jakarta'", &[])
            .await
            .unwrap();
        assert_eq!(
            snapshot(&mut tx).await,
            before,
            "catalog and actual values must survive refusal"
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn native_uuid_schema_conversion_is_explicit() {
    conversion_is_explicit(
        FieldDecl::char("value"),
        FieldDecl::uuid("value"),
        "'00000000-0000-0000-0000-000000000001'",
    )
    .await;
    conversion_is_explicit(
        FieldDecl::uuid("value"),
        FieldDecl::char("value"),
        "'00000000-0000-0000-0000-000000000001'::uuid",
    )
    .await;
}

#[tokio::test]
async fn native_xid8_schema_conversion_is_explicit() {
    conversion_is_explicit(
        FieldDecl::char("value"),
        FieldDecl::xid8("value"),
        "'18446744073709551615'",
    )
    .await;
    conversion_is_explicit(
        FieldDecl::xid8("value"),
        FieldDecl::char("value"),
        "'18446744073709551615'::xid8",
    )
    .await;
}

#[tokio::test]
async fn native_timestamp_schema_conversion_is_explicit() {
    conversion_is_explicit(
        FieldDecl::datetime("value"),
        FieldDecl::timestamptz("value"),
        "'2026-11-01 12:34:56.123456'::timestamp",
    )
    .await;
    conversion_is_explicit(
        FieldDecl::timestamptz("value"),
        FieldDecl::datetime("value"),
        "'2026-11-01 12:34:56.123456+00'::timestamptz",
    )
    .await;
}

#[tokio::test]
async fn legacy_datetime_schema_conversion_policy_is_unchanged() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([declaration(FieldDecl::datetime("value"))])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute("INSERT INTO test_native_migration (id, value) VALUES (1, '2026-11-01 12:34:56.123456'::timestamp), (2, NULL)", &[]).await.unwrap();
    let before = snapshot(&mut tx).await.1;
    tx.commit().await.unwrap();
    let model = Box::leak(Box::new(declaration(FieldDecl::char("value"))));
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls([&*model])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("SET LOCAL TIME ZONE 'Asia/Jakarta'", &[])
        .await
        .unwrap();
    tx.init_models(&[]).await.unwrap();
    let after = snapshot(&mut tx).await;
    assert_eq!(after.0, "varchar");
    assert_eq!(after.1, before);
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}
