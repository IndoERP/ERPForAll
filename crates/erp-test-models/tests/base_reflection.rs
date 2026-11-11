//! Parity tests for `ir.model` and `ir.model.fields` reflection into PostgreSQL.
//! Specs:.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_orm::reflection::reflect_models;
use erp_test_models::{base_users, test_defaults};
use erp_testkit::Harness;

async fn reflection_harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &test_defaults::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .expect("harness")
}

#[tokio::test]
async fn spec_model_039_ir_model_reflection() {
    let h = reflection_harness().await;
    let mut tx = h.begin().await.unwrap();

    reflect_models(&mut tx).await.unwrap();

    // Query ir_model for test_defaults.partner
    let rows = tx
        .query(
            "SELECT name, model, state, transient FROM ir_model WHERE model = 'test_defaults.partner'",
            &[],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let name: String = rows[0].try_get(0).unwrap();
    let model: String = rows[0].try_get(1).unwrap();
    let state: String = rows[0].try_get(2).unwrap();
    let transient: bool = rows[0].try_get(3).unwrap();

    assert_eq!(name, "Test Defaults Partner");
    assert_eq!(model, "test_defaults.partner");
    assert_eq!(state, "base");
    assert!(!transient);

    // Verify XML ID in ir_model_data
    let xml_rows = tx
        .query(
            "SELECT module, name, model FROM ir_model_data WHERE module = 'test_defaults' AND name = 'model_test_defaults_partner'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(xml_rows.len(), 1);
    let target_model: String = xml_rows[0].try_get(2).unwrap();
    assert_eq!(target_model, "ir.model");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_model_040_ir_model_fields_reflection() {
    let h = reflection_harness().await;
    let mut tx = h.begin().await.unwrap();

    reflect_models(&mut tx).await.unwrap();

    // Query ir_model_fields for test_defaults.item.pref_partner
    let rows = tx
        .query(
            r#"
            SELECT field_description, ttype, relation, state, store, readonly, required, copied
            FROM ir_model_fields
            WHERE model = 'test_defaults.item' AND name = 'pref_partner'
            "#,
            &[],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let ttype: String = rows[0].try_get(1).unwrap();
    let relation: Option<String> = rows[0].try_get(2).unwrap();
    let state: String = rows[0].try_get(3).unwrap();
    let store: bool = rows[0].try_get(4).unwrap();
    let copied: bool = rows[0].try_get(7).unwrap();

    assert_eq!(ttype, "many2one");
    assert_eq!(relation.as_deref(), Some("test_defaults.partner"));
    assert_eq!(state, "base");
    assert!(store);
    assert!(copied);

    // Verify XML ID for field in ir_model_data: <module>.field_<table>__<field>
    let xml_rows = tx
        .query(
            "SELECT module, name, model FROM ir_model_data WHERE module = 'test_defaults' AND name = 'field_test_defaults_item__pref_partner'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(xml_rows.len(), 1);
    let target_model: String = xml_rows[0].try_get(2).unwrap();
    assert_eq!(target_model, "ir.model.fields");

    drop(tx);
    h.cleanup().await.unwrap();
}
