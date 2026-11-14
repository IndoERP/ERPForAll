// SPDX-License-Identifier: MIT OR Apache-2.0
//! Column collation creation, reconciliation, provenance and index semantics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod support;
use erp_fields::{ErrorKind, GeneratedStorage};
use erp_registry::{
    AddonDescriptor, FieldDecl, IndexDecl, ModelDecl, PartitionDecl, PartitionInterval,
    PartitionRetain, RegistryBuilder, TableObjectDecl,
};
use erp_sdk::{Orm, SUPERUSER_ID, Transaction};
use erp_testkit::Harness;
use std::sync::Arc;
use support::one;
static ADDON: AddonDescriptor = AddonDescriptor {
    name: "coltest",
    depends: &["base"],
    exact_only: false,
};
static CODE_INDEX: [TableObjectDecl; 1] = [TableObjectDecl::index(
    "_code_typed",
    IndexDecl::new(&["code"]),
)];
fn model(collation: &'static str, width: u32, generated: bool) -> ModelDecl {
    let mut fields = vec![
        FieldDecl::char("code")
            .size(width)
            .collation(collation)
            .index(Some(erp_fields::IndexKind::Btree)),
        FieldDecl::text("note").collation(collation),
    ];
    if generated {
        fields.push(
            FieldDecl::text("derived")
                .generated("code")
                .generated_storage(GeneratedStorage::Stored)
                .collation(collation),
        );
    }
    ModelDecl::new("coltest", "coltest.item")
        .table_objects(&CODE_INDEX)
        .fields(Vec::leak(fields))
}
fn orm(h: &Harness, decl: ModelDecl) -> Orm {
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(decl))])
        .build()
        .unwrap();
    h.orm().rebuild(Arc::new(registry)).build().unwrap()
}
async fn harness(decl: ModelDecl) -> Harness {
    Harness::builder([&ADDON])
        .with_decls([decl])
        .build()
        .await
        .unwrap()
}
async fn sync(h: &Harness, decl: ModelDecl) -> erp_fields::Result<String> {
    let orm = orm(h, decl);
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    tx.capture_statements(true);
    tx.init_models(&[]).await?;
    let ddl = tx.captured().join("\n");
    tx.commit().await?;
    Ok(ddl)
}
async fn identity(tx: &mut Transaction, column: &str) -> String {
    let sql = format!(
        "SELECT n.nspname || '.' || c.collname FROM pg_attribute a JOIN pg_collation c ON c.oid=a.attcollation JOIN pg_namespace n ON n.oid=c.collnamespace WHERE a.attrelid='coltest_item'::regclass AND a.attname='{column}'"
    );
    one(tx, &sql).await.unwrap()
}
async fn indexes_match(tx: &mut Transaction) {
    let rows=tx.query("SELECT bool_and(i.indcollation[0]=a.attcollation AND i.indisvalid), count(*) FROM pg_index i JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=i.indkey[0] WHERE i.indrelid='coltest_item'::regclass AND a.attname='code'", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, Option<bool>>(0), Some(true));
    assert_eq!(rows[0].get::<_, i64>(1), 2);
}
#[tokio::test]
async fn fresh_columns_have_explicit_identity_and_stable_resync() {
    let h = harness(model("C", 30, true)).await;
    let mut tx = h.begin().await.unwrap();
    for c in ["code", "note", "derived"] {
        assert_eq!(identity(&mut tx, c).await, "pg_catalog.C");
    }
    indexes_match(&mut tx).await;
    tx.commit().await.unwrap();
    let sql = sync(&h, model("C", 30, true)).await.unwrap();
    assert!(!sql.contains("ALTER TABLE \"coltest_item\""), "{sql}");
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn populated_columns_change_both_ways_without_narrowing() {
    let h = harness(model("default", 30, false)).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO coltest_item (code,note) VALUES ('a long existing value','é'), (NULL,NULL); ALTER TABLE coltest_item ALTER COLUMN code TYPE varchar(30) COLLATE \"POSIX\"").await.unwrap();
    tx.commit().await.unwrap();
    for name in ["C", "default"] {
        let sql = sync(&h, model(name, 5, false)).await.unwrap();
        assert!(sql.contains("COLLATE"), "{sql}");
        let mut tx = h.begin().await.unwrap();
        assert_eq!(
            identity(&mut tx, "code").await,
            format!("pg_catalog.{name}")
        );
        assert_eq!(one(&mut tx,"SELECT format_type(atttypid,atttypmod) FROM pg_attribute WHERE attrelid='coltest_item'::regclass AND attname='code'").await.as_deref(),Some("character varying(30)"));
        assert_eq!(
            one(
                &mut tx,
                "SELECT string_agg(coalesce(code,'<NULL>'),'|' ORDER BY id) FROM coltest_item"
            )
            .await
            .as_deref(),
            Some("a long existing value|<NULL>")
        );
        indexes_match(&mut tx).await;
        tx.commit().await.unwrap();
        let sql = sync(&h, model(name, 5, false)).await.unwrap();
        assert!(!sql.contains("ALTER TABLE \"coltest_item\""), "{sql}");
    }
    // Widening and collation are applied together by the ordinary type-conversion path.
    sync(&h, model("C", 80, false)).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(identity(&mut tx, "code").await, "pg_catalog.C");
    assert_eq!(one(&mut tx,"SELECT format_type(atttypid,atttypmod) FROM pg_attribute WHERE attrelid='coltest_item'::regclass AND attname='code'").await.as_deref(),Some("character varying(80)"));
    indexes_match(&mut tx).await;
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn rollback_and_dependency_failure_preserve_catalog() {
    let h = harness(model("default", 30, false)).await;
    let new = orm(&h, model("C", 30, false));
    let mut tx = new.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    assert_eq!(identity(&mut tx, "code").await, "pg_catalog.C");
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(identity(&mut tx, "code").await, "pg_catalog.default");
    tx.batch("CREATE VIEW coltest_dependency AS SELECT code FROM coltest_item")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = sync(&h, model("C", 30, false)).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Database);
    assert_eq!(
        e.message(),
        "cannot alter type of a column used by a view or rule"
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(identity(&mut tx, "code").await, "pg_catalog.default");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn generated_collation_changes_require_explicit_migration() {
    let h = harness(model("default", 30, true)).await;
    let e = sync(&h, model("C", 30, true)).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "Generated field 'coltest.item.derived': collation change requires an explicit migration"
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(identity(&mut tx, "derived").await, "pg_catalog.default");
    tx.batch("ALTER TABLE coltest_item ALTER COLUMN derived TYPE text COLLATE \"C\"")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = sync(&h, model("default", 30, true)).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "Generated field 'coltest.item.derived': catalog drift; explicit migration required"
    );
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn c_order_is_byte_order_without_normalization() {
    let h = harness(model("C", 30, false)).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO coltest_item(code) VALUES ('é'),('Z'),('a'),('é'),('É')")
        .await
        .unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT string_agg(code,'|' ORDER BY code) FROM coltest_item"
        )
        .await
        .as_deref(),
        Some("Z|a|é|É|é")
    );
    assert_eq!(
        one(&mut tx, "SELECT (max(code) = 'é')::text FROM coltest_item")
            .await
            .as_deref(),
        Some("false")
    );
    let records = tx.env().records("coltest.item").unwrap();
    let found = records
        .search_with(
            &mut tx,
            &support::dom(erp_sdk::serde_json::json!([])),
            &erp_sdk::SearchOptions::new().order("code"),
        )
        .await
        .unwrap();
    assert_eq!(
        found.values(&mut tx, "code").await.unwrap(),
        ["Z", "a", "é", "É", "é"].map(erp_sdk::Value::str)
    );
    let found = records
        .search_with(
            &mut tx,
            &support::dom(erp_sdk::serde_json::json!([["code", "=", "é"]])),
            &erp_sdk::SearchOptions::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        found.values(&mut tx, "code").await.unwrap(),
        [erp_sdk::Value::str("é")]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn view_outputs_are_checked_without_altering_the_view() {
    for (actual, mismatch) in [("default", "C"), ("C", "default")] {
        let h = harness(model(actual, 30, false)).await;
        let mut tx = h.begin().await.unwrap();
        tx.batch("ALTER TABLE coltest_item RENAME TO coltest_backing; CREATE VIEW coltest_item AS SELECT * FROM coltest_backing").await.unwrap();
        tx.commit().await.unwrap();
        let e = sync(&h, model(mismatch, 30, false)).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "view field 'coltest.item.code': collation mismatch; explicit view migration required"
        );
        sync(&h, model(actual, 30, false)).await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn added_column_gets_collation_and_backfill_default() {
    let initial = ModelDecl::new("coltest", "coltest.item")
        .fields(Vec::leak(vec![FieldDecl::integer("number")]));
    let h = harness(initial).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO coltest_item(number) VALUES(1)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let decl = ModelDecl::new("coltest", "coltest.item").fields(Vec::leak(vec![
        FieldDecl::integer("number"),
        FieldDecl::text("code")
            .collation("C")
            .required(true)
            .default(erp_registry::StaticValue::Str("seed")),
    ]));
    sync(&h, decl).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(identity(&mut tx, "code").await, "pg_catalog.C");
    assert_eq!(
        one(&mut tx, "SELECT code FROM coltest_item")
            .await
            .as_deref(),
        Some("seed")
    );
    assert_eq!(one(&mut tx,"SELECT attnotnull::text FROM pg_attribute WHERE attrelid='coltest_item'::regclass AND attname='code'").await.as_deref(),Some("true"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn partition_parent_and_leaves_keep_collation() {
    fn partitioned() -> ModelDecl {
        model("C", 30, false).partition(PartitionDecl::new(
            PartitionInterval::Day,
            2,
            PartitionRetain::All,
        ))
    }
    let h = harness(partitioned()).await;
    let mut tx = h.begin().await.unwrap();
    let rows=tx.query("SELECT count(*), bool_and(c.collname='C' AND n.nspname='pg_catalog') FROM pg_partition_tree('coltest_item') t JOIN pg_attribute a ON a.attrelid=t.relid AND a.attname='code' JOIN pg_collation c ON c.oid=a.attcollation JOIN pg_namespace n ON n.oid=c.collnamespace",&[]).await.unwrap();
    assert!(rows[0].get::<_, i64>(0) > 1);
    assert_eq!(rows[0].get::<_, Option<bool>>(1), Some(true));
    let rows=tx.query("SELECT bool_and(i.indcollation[0]=a.attcollation AND i.indisvalid) FROM pg_partition_tree('coltest_item') t JOIN pg_index i ON i.indrelid=t.relid JOIN pg_attribute a ON a.attrelid=t.relid AND a.attnum=i.indkey[0] WHERE a.attname='code'",&[]).await.unwrap();
    assert_eq!(rows[0].get::<_, Option<bool>>(0), Some(true));
    tx.commit().await.unwrap();
    drop(tx);
    let sql = sync(&h, partitioned()).await.unwrap();
    assert!(!sql.contains("COLLATE"), "{sql}");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn collation_change_on_large_table_requires_maintenance() {
    let h = harness(model("default", 30, false)).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO coltest_item(code) VALUES('a'),('b')")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let new = orm(&h, model("C", 30, false));
    let mut tx = new.begin(SUPERUSER_ID).await.unwrap();
    let e = tx
        .init_models_with(&[], &erp_orm::SchemaSyncOptions::new().large_table_rows(1))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "field 'coltest.item.code': collation change requires a maintenance window"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(identity(&mut tx, "code").await, "pg_catalog.default");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
