// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema sync of `db_default` fields: the static default of a stored required boolean
//! stays the column's database default on a new table, a new column and an existing column; a
//! changed literal is set again, a dropped flag drops it, an unchanged column costs no DDL; a raw
//! `INSERT` without the column takes it; and the ORM still writes the column itself.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder, StaticValue};
use erp_sdk::{Cell, ErrorKind, Orm, RawVals, SUPERUSER_ID, StaticPolicy, Transaction};
use erp_testkit::Harness;
use support::one;

static DBD: AddonDescriptor = AddonDescriptor {
    name: "test_dbd",
    depends: &["base"],
    exact_only: false,
};

const NAME: FieldDecl = FieldDecl::char("name");

/// `x.flagged` with `name` and, when `escape` is given, the boolean `escape` (required, default
/// `default`, `db_default` as `flag`).
fn flagged(escape: Option<(bool, bool)>) -> ModelDecl {
    let fields: Vec<FieldDecl> = match escape {
        None => vec![NAME],
        Some((default, flag)) => vec![
            NAME,
            FieldDecl::boolean("escape")
                .required(true)
                .default(StaticValue::Bool(default))
                .db_default(flag),
        ],
    };
    ModelDecl::new("test_dbd", "x.flagged").fields(Vec::leak(fields))
}

async fn harness(decl: ModelDecl) -> Harness {
    Harness::builder([&DBD])
        .with_decls([decl])
        .build()
        .await
        .unwrap()
}

/// Schema sync of the same database with another version of the model; the DDL it sent.
async fn resync(h: &Harness, decl: ModelDecl) -> String {
    let orm = orm(h, decl);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let ddl = tx.captured().join("\n");
    tx.commit().await.unwrap();
    ddl
}

fn orm(h: &Harness, decl: ModelDecl) -> Orm {
    let decl: &'static ModelDecl = Box::leak(Box::new(decl));
    let registry = Arc::new(
        RegistryBuilder::new([&DBD])
            .with_decls([decl])
            .build()
            .unwrap(),
    );
    Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

/// The catalog's default expression of `x_flagged.escape`, `None` without a default.
async fn column_default(tx: &mut Transaction) -> Option<String> {
    one(
        tx,
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
           JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
          WHERE d.adrelid = 'x_flagged'::regclass AND a.attname = 'escape'",
    )
    .await
}

async fn not_null(tx: &mut Transaction) -> Option<String> {
    one(
        tx,
        "SELECT attnotnull::text FROM pg_attribute \
          WHERE attrelid = 'x_flagged'::regclass AND attname = 'escape'",
    )
    .await
}

/// A raw `INSERT` that leaves `escape` out; the value it stored.
async fn raw_insert(tx: &mut Transaction, name: &str) -> Option<String> {
    one(
        tx,
        &format!("INSERT INTO x_flagged (name) VALUES ('{name}') RETURNING escape::text"),
    )
    .await
}

/// A new table: the column is NOT NULL with its default, a raw `INSERT` without it takes the
/// default, and the next sync of the unchanged model sends no DDL for it.
#[tokio::test]
async fn a_new_table_keeps_the_default() {
    let h = harness(flagged(Some((true, true)))).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(column_default(&mut tx).await.as_deref(), Some("true"));
    assert_eq!(not_null(&mut tx).await.as_deref(), Some("true"));
    assert_eq!(raw_insert(&mut tx, "raw").await.as_deref(), Some("true"));
    tx.commit().await.unwrap();
    let ddl = resync(&h, flagged(Some((true, true)))).await;
    assert!(!ddl.contains("DEFAULT"), "{ddl}");
    h.cleanup().await.unwrap();
}

/// A new column on a table with rows: existing rows get the default (fast default) and the
/// column keeps it.
#[tokio::test]
async fn a_new_column_keeps_the_default() {
    let h = harness(flagged(None)).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO x_flagged (name) VALUES ('before')")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let ddl = resync(&h, flagged(Some((true, true)))).await;
    assert!(ddl.contains("ADD COLUMN \"escape\""), "{ddl}");
    assert!(!ddl.contains("DROP DEFAULT"), "{ddl}");
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT escape::text FROM x_flagged WHERE name = 'before'"
        )
        .await
        .as_deref(),
        Some("true")
    );
    assert_eq!(column_default(&mut tx).await.as_deref(), Some("true"));
    assert_eq!(not_null(&mut tx).await.as_deref(), Some("true"));
    assert_eq!(raw_insert(&mut tx, "raw").await.as_deref(), Some("true"));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// An existing column without a default gets it when the flag is added; a changed literal is set
/// again; dropping the flag drops the default (and only then).
#[tokio::test]
async fn an_existing_column_follows_the_flag_and_the_literal() {
    let h = harness(flagged(Some((true, false)))).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(column_default(&mut tx).await, None);
    tx.rollback().await.unwrap();

    let ddl = resync(&h, flagged(Some((true, true)))).await;
    assert!(ddl.contains("SET DEFAULT TRUE"), "{ddl}");
    let mut tx = h.begin().await.unwrap();
    assert_eq!(column_default(&mut tx).await.as_deref(), Some("true"));
    tx.rollback().await.unwrap();

    let ddl = resync(&h, flagged(Some((false, true)))).await;
    assert!(ddl.contains("SET DEFAULT FALSE"), "{ddl}");
    let mut tx = h.begin().await.unwrap();
    assert_eq!(column_default(&mut tx).await.as_deref(), Some("false"));
    assert_eq!(raw_insert(&mut tx, "raw").await.as_deref(), Some("false"));
    tx.rollback().await.unwrap();

    let ddl = resync(&h, flagged(Some((false, false)))).await;
    assert!(ddl.contains("DROP DEFAULT"), "{ddl}");
    let mut tx = h.begin().await.unwrap();
    assert_eq!(column_default(&mut tx).await, None);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A default set outside the ORM on a column without the flag stays refused when it differs from
/// the field's static default; the flag's own default is the only one dropped.
#[tokio::test]
async fn a_foreign_default_on_an_unflagged_column_stays_refused() {
    let h = harness(flagged(Some((true, false)))).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE x_flagged ALTER COLUMN escape SET DEFAULT FALSE")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let orm = orm(&h, flagged(Some((true, false))));
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "ORM columns must not have a database default (set outside the ORM): \
         \"x_flagged\".\"escape\"; declare the default on the field and drop the column default \
         (ALTER TABLE … ALTER COLUMN … DROP DEFAULT)"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A default equal to the field's own on an optional boolean was never the flag's (the registry
/// refuses `db_default` on an optional field): it stays refused, it is not dropped.
#[tokio::test]
async fn a_default_on_an_optional_boolean_stays_refused() {
    fn optional() -> ModelDecl {
        let fields = vec![
            NAME,
            FieldDecl::boolean("escape").default(StaticValue::Bool(true)),
        ];
        ModelDecl::new("test_dbd", "x.flagged").fields(Vec::leak(fields))
    }
    let h = harness(optional()).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE x_flagged ALTER COLUMN escape SET DEFAULT TRUE")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let orm = orm(&h, optional());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "ORM columns must not have a database default (set outside the ORM): \
         \"x_flagged\".\"escape\"; declare the default on the field and drop the column default \
         (ALTER TABLE … ALTER COLUMN … DROP DEFAULT)"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The ORM writes a flagged column in every row of a create (still holds): with the
/// column default changed by hand, a create that leaves the field out stores the field's default,
/// and a batch whose rows give and omit it stores each row's own value.
#[tokio::test]
async fn the_orm_create_writes_the_column_itself() {
    let h = harness(flagged(Some((true, true)))).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE x_flagged ALTER COLUMN escape SET DEFAULT FALSE")
        .await
        .unwrap();
    let created = tx
        .env()
        .records("x.flagged")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new().set_cell("name", Cell::Str("omits".into())),
                RawVals::new()
                    .set_cell("name", Cell::Str("gives".into()))
                    .set_cell("escape", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let ids = created.ids();
    assert_eq!(ids.len(), 2);
    let stored = one(
        &mut tx,
        &format!(
            "SELECT string_agg(escape::text, ',' ORDER BY id) FROM x_flagged WHERE id IN ({}, {})",
            ids[0], ids[1]
        ),
    )
    .await;
    assert_eq!(stored.as_deref(), Some("true,false"));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
