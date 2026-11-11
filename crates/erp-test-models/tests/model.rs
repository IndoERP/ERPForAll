//! MODEL cases that need method dispatch or a database (the pure registry cases are in
//! `crates/erp-registry/tests/compose.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use erp_test_models::test_base_ext::BaseExtMethods;
use erp_test_models::test_inherit_a::{A, AMethods};
use erp_test_models::test_inherit_parent::{Child, Parent, ParentMethods};
use erp_test_models::test_orm::{Mixed, Partner};
use erp_test_models::{
    base_users, test_base_ext, test_inherit_a, test_inherit_b, test_inherit_c, test_inherit_parent,
    test_inherit_parent_ext, test_orm,
};
use erp_testkit::Harness;

#[tokio::test]
async fn spec_model_002_in_place_extension_and_override_order() {
    let h = Harness::builder([
        &test_inherit_a::ADDON,
        &test_inherit_b::ADDON,
        &test_inherit_c::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let a = tx.env().model::<A>().unwrap();
    // A's foo() calls bar(), defined by B (42) and overridden by C (super * 2).
    assert_eq!(a.foo(&mut tx).await.unwrap(), 84);
    let spec = h.registry().model("test.inherit.a").unwrap();
    let mro: Vec<_> = spec.mro.iter().map(|d| d.addon).collect();
    assert_eq!(
        mro,
        [
            "test_inherit_c",
            "test_inherit_b",
            "test_inherit_a",
            "web",
            "base"
        ]
    );
    let chain: Vec<_> = h
        .orm()
        .dispatch()
        .implementations(spec.id, "bar")
        .iter()
        .map(|i| i.addon)
        .collect();
    assert_eq!(chain, ["test_inherit_c", "test_inherit_b"]);
    assert!(spec.field("field_b").is_some() && spec.field("field_c").is_some());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_model_002_chain_follows_the_addon_set() {
    let h = Harness::builder([&test_inherit_a::ADDON, &test_inherit_b::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let a = tx.env().model::<A>().unwrap();
    assert_eq!(a.foo(&mut tx).await.unwrap(), 42);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_model_006_new_model_sees_later_parent_extensions() {
    let h = Harness::builder([&test_inherit_parent::ADDON, &test_inherit_parent_ext::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let parent = env.model::<Parent>().unwrap();
    let child = env.model::<Child>().unwrap();
    assert_eq!(parent.stuff(&mut tx).await.unwrap(), "P1P2");
    assert_eq!(child.stuff(&mut tx).await.unwrap(), "P1P2C1");
    let registry = h.registry();
    let p = registry.model("test_inherit_parent").unwrap();
    let c = registry.model("test_inherit_child").unwrap();
    assert!(p.field("foo").is_some() && p.field("bar").is_none());
    assert!(c.field("foo").is_some() && c.field("bar").is_some());
    assert_eq!(
        (p.table.as_str(), c.table.as_str()),
        ("test_inherit_parent", "test_inherit_child")
    );
    assert_eq!((p.table_objects.len(), c.table_objects.len()), (1, 1));
    assert_eq!(c.inherit, ["test_inherit_parent"]);
    // the child's table got the parent's constraint
    let n = support::one(
        &mut tx,
        "SELECT count(*)::text FROM pg_constraint WHERE conname = 'test_inherit_child_unique_foo'",
    )
    .await;
    assert_eq!(n.as_deref(), Some("1"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_model_007_every_model_inherits_base() {
    let h = Harness::builder([&test_orm::ADDON, &test_base_ext::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let partner = env.model::<Partner>().unwrap();
    let mixed = env.model::<Mixed>().unwrap();
    assert_eq!(
        partner.hello(&mut tx).await.unwrap(),
        "hello from test_orm.partner"
    );
    assert_eq!(
        mixed.hello(&mut tx).await.unwrap(),
        "hello from test_orm.mixed"
    );
    for m in h.registry().models() {
        assert!(m.field("note_like").is_some(), "{} lacks note_like", m.name);
    }
    assert!(h.registry().model("base").unwrap().inherit.is_empty());
    let col = support::one(
        &mut tx,
        "SELECT count(*)::text FROM information_schema.columns \
         WHERE table_name = 'test_orm_partner' AND column_name = 'note_like'",
    )
    .await;
    assert_eq!(col.as_deref(), Some("1"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_model_011_audit_columns_and_foreign_keys() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let fks = support::rows(
        &mut tx,
        "SELECT a.attname::text, t.relname::text, k.confdeltype::text FROM pg_constraint k \
         JOIN pg_class c ON c.oid = k.conrelid JOIN pg_class t ON t.oid = k.confrelid \
         JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = k.conkey[1] \
         WHERE k.contype = 'f' AND c.relname = 'test_orm_partner' ORDER BY 1",
    )
    .await;
    let expected = |c: &str| {
        vec![
            Some(c.to_owned()),
            Some("res_users".to_owned()),
            Some("n".to_owned()),
        ]
    };
    assert_eq!(fks, vec![expected("create_uid"), expected("write_uid")]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_model_030_init_models_step_order() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let statements = tx.captured().join("\n;;\n");
    let pos = |needle: &str| {
        statements
            .find(needle)
            .unwrap_or_else(|| panic!("no {needle:?} in {statements}"))
    };
    let table = pos("CREATE TABLE \"test_orm_category\"");
    let constraint = pos("ADD CONSTRAINT \"test_orm_category_positive_color\"");
    let index = pos("CREATE INDEX \"test_orm_schema__index_btree_index\"");
    let fk = pos("FOREIGN KEY (\"create_uid\")");
    assert!(
        table < constraint && constraint < index && index < fk,
        "{statements}"
    );
    tx.commit().await.unwrap();
    // A second run on an up-to-date schema bounds the transaction (saving the caller's
    // timeouts), takes the schema lock, reads the catalog and restores the timeouts: no DDL.
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    assert_eq!(
        tx.captured().len(),
        5,
        "timeouts + lock + catalog + provenance + restore only: {:?}",
        tx.captured()
    );
    assert!(
        tx.captured()
            .iter()
            .any(|s| s.contains("erp_schema_generated")),
        "must read generated column provenance: {:?}",
        tx.captured()
    );
    assert!(
        !tx.captured().iter().any(|s| {
            let trimmed = s.trim_start();
            trimmed.starts_with("CREATE ")
                || trimmed.starts_with("ALTER ")
                || trimmed.starts_with("DROP ")
        }),
        "no DDL in unchanged sync: {:?}",
        tx.captured()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_model_031_missing_table_is_recreated() {
    let h = Harness::builder([&test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    // Drop table outside the ORM
    tx.execute("DROP TABLE test_orm_country CASCADE", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Recreate via sync_schema
    let report = h.orm().sync_schema().await.unwrap();
    assert!(report.is_empty());

    // Verify the table was recreated and records can be created
    let mut tx = h.begin().await.unwrap();
    let countries = tx.env().records("test_orm.country").unwrap();
    let mut vals = std::collections::BTreeMap::new();
    vals.insert("name".to_string(), erp_fields::Value::str("Atlantis"));
    let created = countries.create_values(&mut tx, &[vals]).await.unwrap();
    assert_eq!(created.len(), 1);
    drop(tx);
    h.cleanup().await.unwrap();
}
