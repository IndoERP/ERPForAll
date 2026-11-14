// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder, StaticValue};
use erp_sdk::{ErrorKind, Orm, SUPERUSER_ID, StaticPolicy};
use erp_testkit::Harness;
use std::sync::Arc;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "bigint_upgrade",
    depends: &["base"],
    exact_only: false,
};
fn declaration(big: bool) -> ModelDecl {
    let field = if big {
        FieldDecl::bigint("counter")
    } else {
        FieldDecl::integer("counter")
    };
    ModelDecl::new("bigint_upgrade", "bigint.upgrade").fields(Vec::leak(vec![
        field
            .default(StaticValue::Int(7))
            .index(Some(erp_fields::IndexKind::Brin)),
    ]))
}
fn orm(h: &Harness, big: bool) -> Orm {
    let registry = Arc::new(
        RegistryBuilder::new([&ADDON])
            .with_decls([&*Box::leak(Box::new(declaration(big)))])
            .build()
            .unwrap(),
    );
    Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}
async fn snapshot(tx: &mut erp_sdk::Transaction) -> (String, Vec<Option<String>>) {
    let ty: String = tx.query("SELECT atttypid::regtype::text FROM pg_attribute WHERE attrelid='bigint_upgrade'::regclass AND attname='counter'", &[]).await.unwrap()[0].get(0);
    let values = tx
        .query("SELECT counter::text FROM bigint_upgrade ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    (ty, values)
}
#[tokio::test]
async fn bigint_schema_widens_without_losing_values_and_narrowing_refuses() {
    let h = Harness::builder([&ADDON])
        .with_decls([declaration(false)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO bigint_upgrade (counter) VALUES (NULL),(0),(-2147483648),(2147483647); ALTER TABLE bigint_upgrade ADD CONSTRAINT keep_bounds CHECK (counter IS NULL OR counter >= -2147483648)").await.unwrap();
    tx.batch("CREATE TABLE bigint_upgrade_parent (value bigint PRIMARY KEY); INSERT INTO bigint_upgrade_parent VALUES (0),(7),(-2147483648),(2147483647),(9223372036854775807); ALTER TABLE bigint_upgrade ADD CONSTRAINT keep_reference FOREIGN KEY (counter) REFERENCES bigint_upgrade_parent(value)").await.unwrap();
    let before = snapshot(&mut tx).await;
    assert_eq!(before.0, "integer");
    tx.commit().await.unwrap();
    drop(tx);
    let upgraded = orm(&h, true);
    let mut tx = upgraded.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    assert_eq!(snapshot(&mut tx).await.0, "bigint");
    tx.rollback().await.unwrap();
    drop(tx);
    let mut original = h.begin().await.unwrap();
    assert_eq!(snapshot(&mut original).await, before);
    drop(original);
    let mut tx = upgraded.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    let after = snapshot(&mut tx).await;
    assert_eq!(after.0, "bigint");
    assert_eq!(after.1, before.1);
    let created = tx
        .env()
        .records("bigint.upgrade")
        .unwrap()
        .create(&mut tx, vec![erp_sdk::RawVals::new()])
        .await
        .unwrap();
    assert_eq!(
        created.value(&mut tx, "counter").await.unwrap(),
        erp_sdk::Value::BigInt(erp_fields::BigInt::from_i64(7))
    );
    tx.flush().await.unwrap();
    let default = tx
        .query(
            "SELECT counter::text FROM bigint_upgrade WHERE id=$1",
            &[&created.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(default[0].get::<_, String>(0), "7");
    tx.execute(
        "INSERT INTO bigint_upgrade (counter) VALUES ($1)",
        &[&i64::MAX],
    )
    .await
    .unwrap();
    tx.init_models(&[]).await.unwrap();
    let index: bool = tx.query("SELECT EXISTS(SELECT FROM pg_indexes WHERE tablename='bigint_upgrade' AND indexdef LIKE '% USING brin %' AND indexdef LIKE '%(counter)%')", &[]).await.unwrap()[0].get(0);
    assert!(index);
    let check: bool = tx.query("SELECT EXISTS(SELECT FROM pg_constraint WHERE conrelid='bigint_upgrade'::regclass AND conname='keep_bounds')", &[]).await.unwrap()[0].get(0);
    assert!(check);
    let foreign_key: bool = tx.query("SELECT EXISTS(SELECT FROM pg_constraint WHERE conrelid='bigint_upgrade'::regclass AND conname='keep_reference' AND contype='f' AND convalidated)", &[]).await.unwrap()[0].get(0);
    assert!(foreign_key);
    tx.commit().await.unwrap();
    drop(tx);
    let down = orm(&h, false);
    let mut tx = down.begin(SUPERUSER_ID).await.unwrap();
    let e = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "bigint.upgrade.counter: narrowing int8 to int4 requires an explicit migration; existing column is unchanged"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(down);
    drop(upgraded);
    h.cleanup().await.unwrap();
}

fn generated_declaration(big: bool) -> ModelDecl {
    let output = if big {
        FieldDecl::bigint("computed")
    } else {
        FieldDecl::integer("computed")
    };
    ModelDecl::new("bigint_upgrade", "bigint.generated_upgrade").fields(Vec::leak(vec![
        FieldDecl::integer("source"),
        output
            .generated("source")
            .generated_storage(erp_fields::GeneratedStorage::Stored),
    ]))
}

#[tokio::test]
async fn bigint_generated_output_widening_requires_explicit_migration() {
    let h = Harness::builder([&ADDON])
        .with_decls([generated_declaration(false)])
        .build()
        .await
        .unwrap();
    let mut initial = h.begin().await.unwrap();
    initial
        .batch("INSERT INTO bigint_generated_upgrade (source) VALUES (8)")
        .await
        .unwrap();
    initial.commit().await.unwrap();
    drop(initial);
    let registry = Arc::new(
        RegistryBuilder::new([&ADDON])
            .with_decls([&*Box::leak(Box::new(generated_declaration(true)))])
            .build()
            .unwrap(),
    );
    let changed = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut attempt = changed.begin(SUPERUSER_ID).await.unwrap();
    let error = attempt.init_models(&[]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Generated field 'bigint.generated_upgrade.computed': automatic conversion is refused; explicit migration required"
    );
    attempt.rollback().await.unwrap();
    drop(attempt);
    let mut original = h.begin().await.unwrap();
    let row = original
        .query(
            "SELECT computed::text, pg_typeof(computed)::text FROM bigint_generated_upgrade",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row[0].get::<_, String>(0), "8");
    assert_eq!(row[0].get::<_, String>(1), "integer");
    drop(original);
    drop(changed);
    h.cleanup().await.unwrap();
}

fn conversion_declaration(big: bool) -> ModelDecl {
    let field = if big {
        FieldDecl::bigint("value")
    } else {
        FieldDecl::float("value")
    };
    ModelDecl::new("bigint_upgrade", "bigint.conversion").fields(Vec::leak(vec![field]))
}

#[tokio::test]
async fn bigint_schema_refuses_implicit_float_conversion() {
    let h = Harness::builder([&ADDON])
        .with_decls([conversion_declaration(false)])
        .build()
        .await
        .unwrap();
    let mut initial = h.begin().await.unwrap();
    initial
        .batch("INSERT INTO bigint_conversion (value) VALUES (1.5)")
        .await
        .unwrap();
    initial.commit().await.unwrap();
    drop(initial);
    let registry = Arc::new(
        RegistryBuilder::new([&ADDON])
            .with_decls([&*Box::leak(Box::new(conversion_declaration(true)))])
            .build()
            .unwrap(),
    );
    let changed = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut attempt = changed.begin(SUPERUSER_ID).await.unwrap();
    let outcome = attempt.init_models(&[]).await;
    if outcome.is_ok() {
        let rows = attempt
            .query("SELECT value::text FROM bigint_conversion", &[])
            .await
            .unwrap();
        eprintln!(
            "unexpected implicit conversion stored {}",
            rows[0].get::<_, String>(0)
        );
    }
    let error = outcome.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "bigint.conversion.value: automatic conversion from 'float8' to bigint requires an explicit migration; existing column is unchanged"
    );
    attempt.rollback().await.unwrap();
    drop(attempt);
    let mut original = h.begin().await.unwrap();
    let rows = original
        .query("SELECT value::text FROM bigint_conversion", &[])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, String>(0), "1.5");
    drop(original);
    drop(changed);
    h.cleanup().await.unwrap();
}
