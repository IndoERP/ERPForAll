//! Parity tests for `ir.model` and `ir.model.fields` reflection into PostgreSQL.
//! Specs.
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

    // Query ir_model_fields_selection for test_defaults.item.state
    let sel_rows = tx
        .query(
            r#"
            SELECT s.value, s.name->>'en_US', s.sequence
            FROM ir_model_fields_selection s
            JOIN ir_model_fields f ON f.id = s.field_id
            WHERE f.model = 'test_defaults.item' AND f.name = 'state'
            ORDER BY s.sequence
            "#,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(sel_rows.len(), 2);
    let val0: String = sel_rows[0].try_get(0).unwrap();
    let name0: String = sel_rows[0].try_get(1).unwrap();
    let seq0: i32 = sel_rows[0].try_get(2).unwrap();
    assert_eq!(val0, "opt_a");
    assert_eq!(name0, "Option A");
    assert_eq!(seq0, 0);

    let val1: String = sel_rows[1].try_get(0).unwrap();
    let name1: String = sel_rows[1].try_get(1).unwrap();
    let seq1: i32 = sel_rows[1].try_get(2).unwrap();
    assert_eq!(val1, "opt_b");
    assert_eq!(name1, "Option B");
    assert_eq!(seq1, 1);

    // Verify XML ID for selection in ir_model_data: <module>.selection__<table>__<field>__<value>
    let sel_xml_rows = tx
        .query(
            "SELECT module, name, model FROM ir_model_data WHERE module = 'test_defaults' AND name = 'selection__test_defaults_item__state__opt_a'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(sel_xml_rows.len(), 1);
    let sel_target_model: String = sel_xml_rows[0].try_get(2).unwrap();
    assert_eq!(sel_target_model, "ir.model.fields.selection");

    // Upgrade test: re-running reflect_models preserves existing rows and updates without duplicating
    reflect_models(&mut tx).await.unwrap();
    let sel_rows_after = tx
        .query(
            "SELECT count(*) FROM ir_model_fields_selection s JOIN ir_model_fields f ON f.id = s.field_id WHERE f.model = 'test_defaults.item' AND f.name = 'state'",
            &[],
        )
        .await
        .unwrap();
    let count: i64 = sel_rows_after[0].try_get(0).unwrap();
    assert_eq!(
        count, 2,
        "re-reflection must update in place without duplicating selection rows"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
