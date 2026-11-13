//! Tests for static selection label translation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_data::OverwriteMode;
use erp_data::TranslationImporter;
use erp_fields::Value;
use erp_test_models::test_defaults;
use erp_testkit::Harness;

async fn setup_langs(tx: &mut erp_orm::Transaction) {
    for (name, code, url) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        tx.env()
            .records("res.lang")
            .unwrap()
            .create(
                tx,
                vec![
                    erp_orm::RawVals::new()
                        .set_cell("name", erp_fields::Cell::Str(name.into()))
                        .set_cell("code", erp_fields::Cell::Str(code.into()))
                        .set_cell("url_code", erp_fields::Cell::Str(url.into()))
                        .set_cell("active", erp_fields::Cell::Bool(true))
                        .set_cell("direction", erp_fields::Cell::Str("ltr".into())),
                ],
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn spec_field_105_static_selection_labels_follow_context_language() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &test_defaults::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .expect("harness");

    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    erp_orm::reflection::reflect_models(&mut tx).await.unwrap();

    // 1. Initial describe in English / default language returns source labels
    let records = tx.env().records("test_defaults.item").unwrap();
    let fields_en = records
        .fields_get(&mut tx, Some(&["state"]), None)
        .await
        .unwrap();
    let state_field = &fields_en["state"];
    let Value::List(selection_en) = &state_field["selection"] else {
        panic!("expected selection list");
    };
    assert_eq!(
        &**selection_en,
        &[
            Value::list([Value::str("opt_a"), Value::str("Option A")]),
            Value::list([Value::str("opt_b"), Value::str("Option B")]),
        ][..]
    );

    // 2. Load French translation for opt_a only; opt_b has no translation
    let mut imp = TranslationImporter::new();
    let po = r#"
msgid ""
msgstr ""

#: model:ir.model.fields.selection,name:test_defaults.selection__test_defaults_item__state__opt_a
msgid "Option A"
msgstr "Option A (FR)"
"#;
    imp.load_po(po, "t.po", "fr_FR").unwrap();
    imp.save(&mut tx, OverwriteMode::Overwrite).await.unwrap();

    // 3. Describe with lang='fr_FR': opt_a is translated, opt_b retains source label
    let french_env = tx.env().with_context([("lang", Value::str("fr_FR"))]);
    let french_records = french_env.records("test_defaults.item").unwrap();
    let fields_fr = french_records
        .fields_get(&mut tx, Some(&["state"]), None)
        .await
        .unwrap();
    let Value::List(selection_fr) = &fields_fr["state"]["selection"] else {
        panic!("expected selection list");
    };
    assert_eq!(
        &**selection_fr,
        &[
            Value::list([Value::str("opt_a"), Value::str("Option A (FR)")]),
            Value::list([Value::str("opt_b"), Value::str("Option B")]),
        ][..]
    );

    // 4. Describe with lang='en_US' returns source labels
    let english_env = tx.env().with_context([("lang", Value::str("en_US"))]);
    let english_records = english_env.records("test_defaults.item").unwrap();
    let fields_en2 = english_records
        .fields_get(&mut tx, Some(&["state"]), None)
        .await
        .unwrap();
    let Value::List(selection_en2) = &fields_en2["state"]["selection"] else {
        panic!("expected selection list");
    };
    assert_eq!(
        &**selection_en2,
        &[
            Value::list([Value::str("opt_a"), Value::str("Option A")]),
            Value::list([Value::str("opt_b"), Value::str("Option B")]),
        ][..]
    );

    // 5. Test Transaction::selection_labels_for_lang helper in fr_FR
    let en_tx_labels = tx
        .selection_labels("test_defaults.item", "state")
        .await
        .unwrap();
    // Default tx lang is None (en_US)
    assert_eq!(
        en_tx_labels,
        vec![
            ("opt_a".to_string(), "Option A".to_string()),
            ("opt_b".to_string(), "Option B".to_string()),
        ]
    );

    let fr_labels = tx
        .selection_labels_for_lang("test_defaults.item", "state", "fr_FR")
        .await
        .unwrap();
    assert_eq!(
        fr_labels,
        vec![
            ("opt_a".to_string(), "Option A (FR)".to_string()),
            ("opt_b".to_string(), "Option B".to_string()),
        ]
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
