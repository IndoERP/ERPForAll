//! Integration tests for translated fields persistence and ORM runtime.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_fields::{Cell, RecId, Value, ValueMap};
use erp_orm::{RawVals, Records, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;

mod test_translated_addon {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_translated_addon", depends = ["base"]);

    #[model(
        name = "test.translated.record",
        table = "test_translated_record",
        order = "name",
        addon = "test_translated_addon"
    )]
    pub struct TestTranslatedRecord {
        #[field(char, translate = true)]
        pub name: Option<Arc<str>>,
        #[field(char, translate = true)]
        pub vat_label: Option<Arc<str>>,
    }

    #[model(
        name = "test.translated.rel",
        table = "test_translated_rel",
        addon = "test_translated_addon"
    )]
    pub struct TestTranslatedRel {
        #[field(many2one, comodel = "test.translated.record")]
        pub record_id: Option<RecId>,
        #[field(char, related = "record_id.name")]
        pub record_name: Option<Arc<str>>,
    }

    #[model(
        name = "test.translated.req",
        table = "test_translated_req",
        addon = "test_translated_addon"
    )]
    pub struct TestTranslatedReq {
        #[field(char, translate = true, required = true)]
        pub title: Option<Arc<str>>,
    }
}

async fn test_harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_translated_addon::ADDON,
    ])
    .build()
    .await
    .expect("harness")
}

fn name_field_ids(
    tx: &Transaction,
    rec: &Records,
) -> (erp_registry::ModelId, erp_registry::FieldId) {
    let model_id = rec.model_id();
    let field_id = tx
        .env()
        .registry()
        .model_by_id(model_id)
        .field("name")
        .unwrap()
        .id;
    (model_id, field_id)
}

async fn create_lang(
    tx: &mut Transaction,
    name: &str,
    code: &str,
    url_code: &str,
    active: bool,
) -> RecId {
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str(name.to_string().into()));
    vals.insert("code".into(), Cell::Str(code.to_string().into()));
    vals.insert("url_code".into(), Cell::Str(url_code.to_string().into()));
    vals.insert("active".into(), Cell::Bool(active));
    vals.insert("direction".into(), Cell::Str("ltr".into()));
    vals.insert("date_format".into(), Cell::Str("%m/%d/%Y".into()));
    vals.insert("time_format".into(), Cell::Str("%H:%M:%S".into()));
    vals.insert("week_start".into(), Cell::Str("7".into()));
    vals.insert("grouping".into(), Cell::Str("[3,0]".into()));
    vals.insert("decimal_point".into(), Cell::Str(".".into()));
    vals.insert("thousands_sep".into(), Cell::Str(",".into()));

    let records = tx.env().records("res.lang").unwrap();
    let created = records.create(tx, vec![vals]).await.unwrap();
    created.rec_ids()[0]
}

async fn setup_langs(tx: &mut Transaction, fr: bool, es: bool, nl: bool) {
    let existing = tx
        .query("SELECT id FROM res_lang WHERE code = 'en_US'", &[])
        .await
        .unwrap();
    if existing.is_empty() {
        create_lang(tx, "English", "en_US", "en", true).await;
    }
    if fr {
        let existing = tx
            .query("SELECT id FROM res_lang WHERE code = 'fr_FR'", &[])
            .await
            .unwrap();
        if existing.is_empty() {
            create_lang(tx, "French", "fr_FR", "fr", true).await;
        }
    }
    if es {
        let existing = tx
            .query("SELECT id FROM res_lang WHERE code = 'es_ES'", &[])
            .await
            .unwrap();
        if existing.is_empty() {
            create_lang(tx, "Spanish", "es_ES", "es", true).await;
        }
    }
    if nl {
        let existing = tx
            .query("SELECT id FROM res_lang WHERE code = 'nl_NL'", &[])
            .await
            .unwrap();
        if existing.is_empty() {
            create_lang(tx, "Dutch", "nl_NL", "nl", true).await;
        }
    }
}

#[tokio::test]
async fn spec_field_061_translated_read_with_fallback() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, true, false).await;

    // Create record with {'en_US': 'Customers', 'fr_FR': 'Clients'}
    let model = tx.env().records("test.translated.record").unwrap();
    let mut dict = ValueMap::new();
    dict.insert("en_US".to_string(), Value::str("Customers"));
    dict.insert("fr_FR".to_string(), Value::str("Clients"));
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::Map(Arc::new(dict)));

    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    // Verify raw column in database is jsonb
    let rows = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val: serde_json::Value = rows[0].get(0);
    assert_eq!(db_val["en_US"], "Customers");
    assert_eq!(db_val["fr_FR"], "Clients");

    // Reading with lang unset: 'Customers'
    let records = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .browse([id.get()])
        .unwrap();
    let read_unset = records.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_unset[0].get("name"), Some(&Value::str("Customers")));

    // Reading with lang='en_US': 'Customers'
    let records_en = records.with_context([("lang", Value::str("en_US"))]);
    let read_en = records_en.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_en[0].get("name"), Some(&Value::str("Customers")));

    // Reading with lang='fr_FR': 'Clients'
    let records_fr = records.with_context([("lang", Value::str("fr_FR"))]);
    let read_fr = records_fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_fr[0].get("name"), Some(&Value::str("Clients")));

    // Reading with lang='es_ES': 'Customers' (fallback)
    let records_es = records.with_context([("lang", Value::str("es_ES"))]);
    let read_es = records_es.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_es[0].get("name"), Some(&Value::str("Customers")));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_062_create_in_non_english() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;

    // Creating with lang='fr_FR' and name='French'
    let model = tx.env().records("test.translated.record").unwrap();
    let model_fr = model.with_context([("lang", Value::str("fr_FR"))]);
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("French"));
    let raw = model_fr.vals_from_values(&values).unwrap();
    let created = model_fr.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    // Stored: {'en_US': 'French', 'fr_FR': 'French'}
    let rows = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val: serde_json::Value = rows[0].get(0);
    assert_eq!(db_val["en_US"], "French");
    assert_eq!(db_val["fr_FR"], "French");

    // Writing 'English' with lang='en_US'
    let records_en = model
        .with_context([("lang", Value::str("en_US"))])
        .browse([id.get()])
        .unwrap();
    let mut write_values = ValueMap::new();
    write_values.insert("name".to_string(), Value::str("English"));
    let raw_write = records_en.vals_from_values(&write_values).unwrap();
    records_en.write(&mut tx, raw_write).await.unwrap();
    tx.flush().await.unwrap();

    // After English write, French still reads 'French' and English reads 'English'
    let records_fr = model
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();
    let read_fr = records_fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_fr[0].get("name"), Some(&Value::str("French")));

    let read_en = records_en.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_en[0].get("name"), Some(&Value::str("English")));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_063_per_language_writes() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;

    // Record created in en_US as 'English'
    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("English"));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    let records_en = model
        .with_context([("lang", Value::str("en_US"))])
        .browse([id.get()])
        .unwrap();
    let records_fr = model
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();

    // Writing 'English 2' in en_US
    let mut w1 = ValueMap::new();
    w1.insert("name".to_string(), Value::str("English 2"));
    let r1 = records_en.vals_from_values(&w1).unwrap();
    records_en.write(&mut tx, r1).await.unwrap();

    // French reads 'English 2' after the first write (fallback)
    let read_fr = records_fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_fr[0].get("name"), Some(&Value::str("English 2")));

    // Writing 'French 2' in fr_FR then 'English 3' in en_US
    let mut w2 = ValueMap::new();
    w2.insert("name".to_string(), Value::str("French 2"));
    let r2 = records_fr.vals_from_values(&w2).unwrap();
    records_fr.write(&mut tx, r2).await.unwrap();

    let mut w3 = ValueMap::new();
    w3.insert("name".to_string(), Value::str("English 3"));
    let r3 = records_en.vals_from_values(&w3).unwrap();
    records_en.write(&mut tx, r3).await.unwrap();

    // After the last writes French reads 'French 2' and English 'English 3'
    let read_fr2 = records_fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_fr2[0].get("name"), Some(&Value::str("French 2")));

    let read_en3 = records_en.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_en3[0].get("name"), Some(&Value::str("English 3")));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_064_writing_false_clears_every_language() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;

    // vat_label translated with en_US='VAT', fr_FR='TVA'
    let model = tx.env().records("test.translated.record").unwrap();
    let mut dict = ValueMap::new();
    dict.insert("en_US".to_string(), Value::str("VAT"));
    dict.insert("fr_FR".to_string(), Value::str("TVA"));
    let mut values = ValueMap::new();
    values.insert("vat_label".to_string(), Value::Map(Arc::new(dict)));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    // Writing False in fr_FR
    let records_fr = model
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();
    let mut w_false = ValueMap::new();
    w_false.insert("vat_label".to_string(), Value::False);
    let r_false = records_fr.vals_from_values(&w_false).unwrap();
    records_fr.write(&mut tx, r_false).await.unwrap();
    tx.flush().await.unwrap();

    // Column is NULL and field reads False in every language
    let rows = tx
        .query(
            "SELECT vat_label FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val: Option<serde_json::Value> = rows[0].get(0);
    assert!(db_val.is_none());

    let records_en = model
        .with_context([("lang", Value::str("en_US"))])
        .browse([id.get()])
        .unwrap();
    let read_en = records_en.read(&mut tx, &["vat_label"]).await.unwrap();
    assert_eq!(read_en[0].get("vat_label"), Some(&Value::False));

    let read_fr = records_fr.read(&mut tx, &["vat_label"]).await.unwrap();
    assert_eq!(read_fr[0].get("vat_label"), Some(&Value::False));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_065_writing_into_null_translated_value() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, true).await; // en_US, fr_FR, nl_NL active

    // vat_label set to NULL by writing False
    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("vat_label".to_string(), Value::False);
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    let records_fr = model
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();
    let records_en = model
        .with_context([("lang", Value::str("en_US"))])
        .browse([id.get()])
        .unwrap();
    let records_nl = model
        .with_context([("lang", Value::str("nl_NL"))])
        .browse([id.get()])
        .unwrap();

    // Writing 'TVA_FR2' in fr_FR
    let mut w1 = ValueMap::new();
    w1.insert("vat_label".to_string(), Value::str("TVA_FR2"));
    let r1 = records_fr.vals_from_values(&w1).unwrap();
    records_fr.write(&mut tx, r1).await.unwrap();

    // After the first write English and Dutch read 'TVA_FR2'
    let read_en1 = records_en.read(&mut tx, &["vat_label"]).await.unwrap();
    assert_eq!(read_en1[0].get("vat_label"), Some(&Value::str("TVA_FR2")));
    let read_nl1 = records_nl.read(&mut tx, &["vat_label"]).await.unwrap();
    assert_eq!(read_nl1[0].get("vat_label"), Some(&Value::str("TVA_FR2")));

    // Writing 'TVA_FR3' in fr_FR and '' in en_US
    let mut w2 = ValueMap::new();
    w2.insert("vat_label".to_string(), Value::str("TVA_FR3"));
    let r2 = records_fr.vals_from_values(&w2).unwrap();
    records_fr.write(&mut tx, r2).await.unwrap();

    let mut w3 = ValueMap::new();
    w3.insert("vat_label".to_string(), Value::str(""));
    let r3 = records_en.vals_from_values(&w3).unwrap();
    records_en.write(&mut tx, r3).await.unwrap();

    // After the next writes English and Dutch read ''
    let read_en2 = records_en.read(&mut tx, &["vat_label"]).await.unwrap();
    assert_eq!(read_en2[0].get("vat_label"), Some(&Value::str("")));
    let read_nl2 = records_nl.read(&mut tx, &["vat_label"]).await.unwrap();
    assert_eq!(read_nl2[0].get("vat_label"), Some(&Value::str("")));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_066_invalid_language_in_context() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, false, false, false).await;
    // nl_NL installed then deactivated
    create_lang(&mut tx, "Dutch", "nl_NL", "nl", false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Test"));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    let records = model.browse([id.get()]).unwrap();

    for bad_lang in ["nl_NL", "Dummy", "_en_US", "'', NOW("] {
        let rec = records.with_context([("lang", Value::str(bad_lang))]);
        let err = rec.read(&mut tx, &["name"]).await.unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::UserError);
        assert_eq!(err.message(), format!("Invalid language code: {bad_lang}"));
    }

    // lang=None and lang='en_US' always work
    let rec_none = records.with_context([("lang", Value::False)]);
    assert!(rec_none.read(&mut tx, &["name"]).await.is_ok());

    let rec_en = records.with_context([("lang", Value::str("en_US"))]);
    assert!(rec_en.read(&mut tx, &["name"]).await.is_ok());

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_067_writing_translation_dict() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;
    create_lang(&mut tx, "Dutch", "nl_NL", "nl", false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Init"));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    let records = model.browse([id.get()]).unwrap();

    // Writing {'en_US': 'A', 'fr_FR': 'B'} -> both set
    let mut d1 = ValueMap::new();
    d1.insert("en_US".to_string(), Value::str("A"));
    d1.insert("fr_FR".to_string(), Value::str("B"));
    let mut v1 = ValueMap::new();
    v1.insert("name".to_string(), Value::Map(Arc::new(d1)));
    let r1 = records.vals_from_values(&v1).unwrap();
    records.write(&mut tx, r1).await.unwrap();

    let rec_en = records.with_context([("lang", Value::str("en_US"))]);
    let rec_fr = records.with_context([("lang", Value::str("fr_FR"))]);
    assert_eq!(
        rec_en.read(&mut tx, &["name"]).await.unwrap()[0].get("name"),
        Some(&Value::str("A"))
    );
    assert_eq!(
        rec_fr.read(&mut tx, &["name"]).await.unwrap()[0].get("name"),
        Some(&Value::str("B"))
    );

    // Writing {'nl_NL': 'C'} -> ValidationError: Some of the languages are not active
    let mut d2 = ValueMap::new();
    d2.insert("nl_NL".to_string(), Value::str("C"));
    let mut v2 = ValueMap::new();
    v2.insert("name".to_string(), Value::Map(Arc::new(d2)));
    let r2 = records.vals_from_values(&v2).unwrap();
    let err2 = records.write(&mut tx, r2).await.unwrap_err();
    assert_eq!(err2.kind(), erp_fields::ErrorKind::Validation);
    assert!(
        err2.message()
            .contains("Some of the languages are not active")
    );

    // Writing {'fr_FR': False} -> ValidationError: Some of the translations are False
    let mut d3 = ValueMap::new();
    d3.insert("fr_FR".to_string(), Value::False);
    let mut v3 = ValueMap::new();
    v3.insert("name".to_string(), Value::Map(Arc::new(d3)));
    let r3 = records.vals_from_values(&v3).unwrap();
    let err3 = records.write(&mut tx, r3).await.unwrap_err();
    assert_eq!(err3.kind(), erp_fields::ErrorKind::Validation);
    assert!(
        err3.message()
            .contains("Some of the translations are False")
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_068_english_inactive() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, true, false).await;
    // Deactivate English in res.lang
    tx.execute(
        "UPDATE res_lang SET active = false WHERE code = 'en_US'",
        &[],
    )
    .await
    .unwrap();

    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Initial"));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    let records_fr = model
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();
    let records_es = model
        .with_context([("lang", Value::str("es_ES"))])
        .browse([id.get()])
        .unwrap();
    let records_none = model.browse([id.get()]).unwrap();

    // Writing 'French Name' in fr_FR
    let mut w1 = ValueMap::new();
    w1.insert("name".to_string(), Value::str("French Name"));
    let r1 = records_fr.vals_from_values(&w1).unwrap();
    records_fr.write(&mut tx, r1).await.unwrap();
    tx.flush().await.unwrap();

    // en_US is kept in sync with the language written
    let rows1 = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db1: serde_json::Value = rows1[0].get(0);
    assert_eq!(db1["en_US"], "French Name");
    assert_eq!(db1["fr_FR"], "French Name");

    // Writing 'Spanish Name' in es_ES
    let mut w2 = ValueMap::new();
    w2.insert("name".to_string(), Value::str("Spanish Name"));
    let r2 = records_es.vals_from_values(&w2).unwrap();
    records_es.write(&mut tx, r2).await.unwrap();
    tx.flush().await.unwrap();

    // en_US is kept in sync with the language written
    let rows2 = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db2: serde_json::Value = rows2[0].get(0);
    assert_eq!(db2["en_US"], "Spanish Name");
    assert_eq!(db2["es_ES"], "Spanish Name");

    // Writing 'None Name' with no lang
    let mut w3 = ValueMap::new();
    w3.insert("name".to_string(), Value::str("None Name"));
    let r3 = records_none.vals_from_values(&w3).unwrap();
    records_none.write(&mut tx, r3).await.unwrap();
    tx.flush().await.unwrap();

    // Each language reads its own value, both from cache and after reload
    assert_eq!(
        records_fr.read(&mut tx, &["name"]).await.unwrap()[0].get("name"),
        Some(&Value::str("French Name"))
    );
    assert_eq!(
        records_es.read(&mut tx, &["name"]).await.unwrap()[0].get("name"),
        Some(&Value::str("Spanish Name"))
    );
    assert_eq!(
        records_none.read(&mut tx, &["name"]).await.unwrap()[0].get("name"),
        Some(&Value::str("None Name"))
    );

    // After reload (clear cache)
    let _ = tx.invalidate_all().await;
    assert_eq!(
        records_fr.read(&mut tx, &["name"]).await.unwrap()[0].get("name"),
        Some(&Value::str("French Name"))
    );
    assert_eq!(
        records_es.read(&mut tx, &["name"]).await.unwrap()[0].get("name"),
        Some(&Value::str("Spanish Name"))
    );
    assert_eq!(
        records_none.read(&mut tx, &["name"]).await.unwrap()[0].get("name"),
        Some(&Value::str("None Name"))
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_two_connection_delta_merge() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, true, false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Initial"));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.commit().await.unwrap();

    // Connection 2 opens first and reads, so its cache holds the stale `{en_US}` map ...
    let mut tx2 = h.begin().await.unwrap();
    let rec2 = tx2
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("es_ES"))])
        .browse([id.get()])
        .unwrap();
    let seen = rec2.read(&mut tx2, &["name"]).await.unwrap();
    assert_eq!(seen[0].get("name"), Some(&Value::str("Initial")));

    // ... connection 1 then writes fr_FR and commits ...
    let mut tx1 = h.begin().await.unwrap();
    let rec1 = tx1
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();
    let mut w1 = ValueMap::new();
    w1.insert("name".to_string(), Value::str("French"));
    let r1 = rec1.vals_from_values(&w1).unwrap();
    rec1.write(&mut tx1, r1).await.unwrap();
    tx1.commit().await.unwrap();

    // ... and connection 2 writes es_ES from its stale cache: only its own key may be sent
    let mut w2 = ValueMap::new();
    w2.insert("name".to_string(), Value::str("Spanish"));
    let r2 = rec2.vals_from_values(&w2).unwrap();
    rec2.write(&mut tx2, r2).await.unwrap();
    // the database refuses the stale writer (contract: merge or serialization error, never a lost
    // update); the commit is where the delta is flushed
    let err = tx2.commit().await.unwrap_err();
    assert_eq!(
        err.kind(),
        erp_fields::ErrorKind::Concurrency,
        "{}",
        err.message()
    );
    tx2.rollback().await.unwrap();
    drop(tx2);
    drop(tx1);

    // connection 1's translation survived
    let mut tx3 = h.begin().await.unwrap();
    let rows = tx3
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val: serde_json::Value = rows[0].get(0);
    assert_eq!(db_val["en_US"], "Initial");
    assert_eq!(db_val["fr_FR"], "French");
    assert!(db_val.get("es_ES").is_none());
    tx3.rollback().await.unwrap();
    drop(tx3);

    // a retry from a fresh snapshot writes only its own key and merges
    let mut tx4 = h.begin().await.unwrap();
    let rec4 = tx4
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("es_ES"))])
        .browse([id.get()])
        .unwrap();
    let mut w4 = ValueMap::new();
    w4.insert("name".to_string(), Value::str("Spanish"));
    let r4 = rec4.vals_from_values(&w4).unwrap();
    rec4.write(&mut tx4, r4).await.unwrap();
    tx4.commit().await.unwrap();
    let mut tx5 = h.begin().await.unwrap();
    let rows = tx5
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val: serde_json::Value = rows[0].get(0);
    assert_eq!(db_val["fr_FR"], "French");
    assert_eq!(db_val["es_ES"], "Spanish");

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_same_tx_language_switch_hits_cache() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;

    // Create record with {'en_US': 'Hello', 'fr_FR': 'Bonjour'}
    let model = tx.env().records("test.translated.record").unwrap();
    let mut dict = ValueMap::new();
    dict.insert("en_US".to_string(), Value::str("Hello"));
    dict.insert("fr_FR".to_string(), Value::str("Bonjour"));
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::Map(Arc::new(dict)));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();
    let _ = tx.invalidate_all().await; // start clean so read must fetch once

    let rec_en = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("en_US"))])
        .browse([id.get()])
        .unwrap();
    let val_en = rec_en.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(val_en[0].get("name"), Some(&Value::str("Hello")));

    tx.capture_statements(true);

    let rec_fr = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();
    let val_fr = rec_fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(val_fr[0].get("name"), Some(&Value::str("Bonjour")));

    let queries = tx.captured().len();
    assert_eq!(
        queries,
        0,
        "switching language in same tx must hit raw JSONB in cache with zero SQL statements, captured: {:?}",
        tx.captured()
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_savepoint_rollback() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Original"));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    let rec_fr = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();

    let mut w = ValueMap::new();
    w.insert("name".to_string(), Value::str("Temp French"));
    let r = rec_fr.vals_from_values(&w).unwrap();

    let _ = tx
        .savepoint(async |tx: &mut Transaction| -> erp_fields::Result<()> {
            rec_fr.write(tx, r.clone()).await?;
            Err(erp_fields::Error::user("abort savepoint"))
        })
        .await;

    // After rollback, value is still original and no baseline survives the discarded write
    let read_back = rec_fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(read_back[0].get("name"), Some(&Value::str("Original")));
    let (model_id, field_id) = name_field_ids(&tx, &rec_fr);
    assert!(tx.translated_baseline(model_id, field_id, id).is_none());

    // A released savepoint flushes its write: the delta reaches the row and the baseline is spent
    tx.savepoint(async |tx: &mut Transaction| -> erp_fields::Result<()> {
        rec_fr.write(tx, r.clone()).await
    })
    .await
    .unwrap();
    assert!(tx.translated_baseline(model_id, field_id, id).is_none());
    let rows = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val: serde_json::Value = rows[0].get(0);
    assert_eq!(db_val["en_US"], "Original");
    assert_eq!(db_val["fr_FR"], "Temp French");

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_copy_preserves_all_languages() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, true, false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut dict = ValueMap::new();
    dict.insert("en_US".to_string(), Value::str("Original English"));
    dict.insert("fr_FR".to_string(), Value::str("Original French"));
    dict.insert("es_ES".to_string(), Value::str("Original Spanish"));
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::Map(Arc::new(dict)));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    let rec = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .browse([id.get()])
        .unwrap();
    let copied = rec.copy(&mut tx, None).await.unwrap();
    tx.flush().await.unwrap();
    let copy_id = copied.rec_ids()[0];

    // Assert database has all 3 languages copied
    let rows = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&copy_id.get()],
        )
        .await
        .unwrap();
    let db_val: serde_json::Value = rows[0].get(0);
    assert_eq!(db_val["en_US"], "Original English");
    assert_eq!(db_val["fr_FR"], "Original French");
    assert_eq!(db_val["es_ES"], "Original Spanish");

    // Reading in fr_FR returns French
    let rec_fr = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([copy_id.get()])
        .unwrap();
    let val_fr = rec_fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(val_fr[0].get("name"), Some(&Value::str("Original French")));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_required_translated_field() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;

    let req_model = tx.env().records("test.translated.req").unwrap();

    // 1. Create without a value fails with Validation error (missing required field)
    let empty_vals = RawVals::new();
    let res = tx
        .savepoint(async |tx: &mut Transaction| -> erp_fields::Result<()> {
            req_model.create(tx, vec![empty_vals]).await?;
            Ok(())
        })
        .await;
    let err = res.expect_err("create without value must be refused");
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert!(
        err.message().contains("Missing required field"),
        "error message: {}",
        err.message()
    );

    // 2. Create with required field in en_US and empty string in fr_FR is allowed
    let mut dict = ValueMap::new();
    dict.insert("en_US".to_string(), Value::str("Required Title"));
    dict.insert("fr_FR".to_string(), Value::str(""));
    let mut values = ValueMap::new();
    values.insert("title".to_string(), Value::Map(Arc::new(dict)));
    let raw = req_model.vals_from_values(&values).unwrap();
    let created = req_model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    let rec_fr = tx
        .env()
        .records("test.translated.req")
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();
    let read_fr = rec_fr.read(&mut tx, &["title"]).await.unwrap();
    assert_eq!(read_fr[0].get("title"), Some(&Value::str("")));

    // 2b. Required means the base (en_US) value is present and non-empty (lead decision 4).
    let mut empty_base = ValueMap::new();
    empty_base.insert("title".to_string(), Value::str(""));
    let raw_empty = req_model.vals_from_values(&empty_base).unwrap();
    let res = tx
        .savepoint(async |tx: &mut Transaction| -> erp_fields::Result<()> {
            req_model.create(tx, vec![raw_empty]).await?;
            Ok(())
        })
        .await;
    let err = res.expect_err("empty base value on a required translated field must be refused");
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert!(
        err.message().contains("Missing required field 'title'"),
        "error message: {}",
        err.message()
    );

    // 3. Writing an empty string in non-en_US language with en_US present is allowed
    let mut w = ValueMap::new();
    w.insert("title".to_string(), Value::str(""));
    let raw_w = rec_fr.vals_from_values(&w).unwrap();
    rec_fr.write(&mut tx, raw_w).await.unwrap();
    tx.flush().await.unwrap();

    let read_fr2 = rec_fr.read(&mut tx, &["title"]).await.unwrap();
    assert_eq!(read_fr2[0].get("title"), Some(&Value::str("")));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_nested_savepoints_and_baselines() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, true, false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Original"));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    // 1. Nested savepoints: outer commit, inner rollback
    let rec_en = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .browse([id.get()])
        .unwrap();
    let rec_fr = rec_en.with_context([("lang", Value::str("fr_FR"))]);
    let rec_es = rec_en.with_context([("lang", Value::str("es_ES"))]);

    let res = tx
        .savepoint(async |tx: &mut Transaction| -> erp_fields::Result<()> {
            // Outer write: English update
            let mut w1 = ValueMap::new();
            w1.insert("name".to_string(), Value::str("Outer English"));
            let r1 = rec_en.vals_from_values(&w1)?;
            rec_en.write(tx, r1).await?;

            // Inner savepoint: French update, rolled back
            let _ = tx
                .savepoint(async |tx: &mut Transaction| -> erp_fields::Result<()> {
                    let mut w2 = ValueMap::new();
                    w2.insert("name".to_string(), Value::str("Inner French"));
                    let r2 = rec_fr.vals_from_values(&w2)?;
                    rec_fr.write(tx, r2).await?;
                    Err(erp_fields::Error::user("rollback inner"))
                })
                .await;

            // Outer write 2: Spanish update
            let mut w3 = ValueMap::new();
            w3.insert("name".to_string(), Value::str("Outer Spanish"));
            let r3 = rec_es.vals_from_values(&w3)?;
            rec_es.write(tx, r3).await?;

            Ok(())
        })
        .await;
    assert!(res.is_ok());
    tx.flush().await.unwrap();

    // Check DB: English and Spanish were committed; French was rolled back
    let rows = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val: serde_json::Value = rows[0].get(0);
    assert_eq!(db_val["en_US"], "Outer English");
    assert_eq!(db_val["es_ES"], "Outer Spanish");
    assert!(db_val.get("fr_FR").is_none());

    // 2. Error inside a savepoint followed by a successful write
    let _ = tx
        .savepoint(async |tx: &mut Transaction| -> erp_fields::Result<()> {
            let mut bad_dict = ValueMap::new();
            bad_dict.insert("invalid_lang".to_string(), Value::str("Bad"));
            let mut bad_vals = ValueMap::new();
            bad_vals.insert("name".to_string(), Value::Map(Arc::new(bad_dict)));
            let r = rec_en.vals_from_values(&bad_vals)?;
            rec_en.write(tx, r).await?;
            Ok(())
        })
        .await;

    // Followed by a successful write outside the rolled-back savepoint
    let mut w_ok = ValueMap::new();
    w_ok.insert("name".to_string(), Value::str("Success French"));
    let r_ok = rec_fr.vals_from_values(&w_ok).unwrap();
    rec_fr.write(&mut tx, r_ok).await.unwrap();
    tx.flush().await.unwrap();

    let rows2 = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val2: serde_json::Value = rows2[0].get(0);
    assert_eq!(db_val2["fr_FR"], "Success French");

    // 3. Second write to the same slot keeps first baseline
    let mut w_first = ValueMap::new();
    w_first.insert("name".to_string(), Value::str("First Write"));
    let r_first = rec_en.vals_from_values(&w_first).unwrap();
    rec_en.write(&mut tx, r_first).await.unwrap();

    // Baseline captured from the initial state
    let model_id = rec_en.model_id();
    let field_id = tx
        .env()
        .registry()
        .model_by_id(model_id)
        .field("name")
        .unwrap()
        .id;
    let baseline_cell = tx.translated_baseline(model_id, field_id, id).cloned();
    assert!(baseline_cell.is_some());

    // Second write before flush
    let mut w_second = ValueMap::new();
    w_second.insert("name".to_string(), Value::str("Second Write"));
    let r_second = rec_en.vals_from_values(&w_second).unwrap();
    rec_en.write(&mut tx, r_second).await.unwrap();

    // Baseline must still be the initial baseline
    let baseline_cell_after = tx.translated_baseline(model_id, field_id, id).cloned();
    assert_eq!(baseline_cell, baseline_cell_after);

    tx.flush().await.unwrap();
    let rows3 = tx
        .query(
            "SELECT name FROM test_translated_record WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let db_val3: serde_json::Value = rows3[0].get(0);
    assert_eq!(db_val3["en_US"], "Second Write");

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_public_read_paths() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut dict = ValueMap::new();
    dict.insert("en_US".to_string(), Value::str("English Apple"));
    dict.insert("fr_FR".to_string(), Value::str("Pomme Francaise"));
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::Map(Arc::new(dict)));
    let raw = model.vals_from_values(&values).unwrap();
    let created = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();

    let rec_fr = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([id.get()])
        .unwrap();

    // 1. read
    let res_read = rec_fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(
        res_read[0].get("name"),
        Some(&Value::str("Pomme Francaise"))
    );

    // 2. search_read (domain on id)
    let domain = erp_sdk::Domain::field("id").unwrap().eq(id.get());
    let res_sr = rec_fr
        .search_read(
            &mut tx,
            &domain,
            &["name"],
            &erp_orm::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(res_sr[0].get("name"), Some(&Value::str("Pomme Francaise")));

    // 3. web_read
    let spec = erp_orm::Spec::new().field("name");
    let res_wr = rec_fr.web_read(&mut tx, spec.clone()).await.unwrap();
    assert_eq!(
        res_wr[0].dict().unwrap().get("name").unwrap(),
        Value::str("Pomme Francaise")
    );

    // 4. related field
    let rel_model = tx.env().records("test.translated.rel").unwrap();
    let mut rel_vals = RawVals::new();
    rel_vals.insert("record_id".into(), Cell::Id(id.get()));
    let rel_created = rel_model.create(&mut tx, vec![rel_vals]).await.unwrap();
    let rel_id = rel_created.rec_ids()[0];
    tx.flush().await.unwrap();

    let rel_rec_fr = tx
        .env()
        .records("test.translated.rel")
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))])
        .browse([rel_id.get()])
        .unwrap();
    let res_rel = rel_rec_fr.read(&mut tx, &["record_name"]).await.unwrap();
    assert_eq!(
        res_rel[0].get("record_name"),
        Some(&Value::str("Pomme Francaise"))
    );

    let _ = tx.invalidate_all().await;
    let rel_rec_en = tx
        .env()
        .records("test.translated.rel")
        .unwrap()
        .with_context([("lang", Value::str("en_US"))])
        .browse([rel_id.get()])
        .unwrap();
    let res_rel_en = rel_rec_en.read(&mut tx, &["record_name"]).await.unwrap();
    assert_eq!(
        res_rel_en[0].get("record_name"),
        Some(&Value::str("English Apple"))
    );

    // 5. onchange (default on new record in fr_FR context)
    let new_rec_fr = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([
            ("lang", Value::str("fr_FR")),
            ("default_name", Value::str("Pomme Par Defaut")),
        ]);
    let onchange_res = new_rec_fr
        .onchange(&mut tx, ValueMap::new(), Vec::new(), spec)
        .await
        .unwrap();
    if let Value::Map(m) = onchange_res {
        let v = m
            .get("value")
            .and_then(|v| v.dict())
            .expect("onchange should have value map");
        assert_eq!(v.get("name").unwrap(), Value::str("Pomme Par Defaut"));
    } else {
        panic!("onchange must return a Map");
    }

    // 6. export_data
    use erp_data::impex::{ImpexExt, SuperuserOnly};
    let exp = rec_fr
        .export_data(&mut tx, &["name"], &SuperuserOnly)
        .await
        .unwrap();
    assert_eq!(exp.datas, vec![vec![Value::str("Pomme Francaise")]]);

    // 7. read_group
    let rg_args = erp_orm::ReadGroup::new(domain).groupby(["name"]);
    let res_rg = rec_fr.read_group(&mut tx, rg_args).await.unwrap();
    assert_eq!(res_rg[0][0], Value::str("Pomme Francaise"));

    h.cleanup().await.unwrap();
}

/// Lead decision 5 (2026-10-27): create, write of `active` and unlink on `res.lang` drop the
/// active-languages memo, so the next translated read sees the new set inside the same transaction.
#[tokio::test]
async fn res_lang_changes_drop_the_active_languages_memo() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, false, false, false).await;
    let nl = create_lang(&mut tx, "Dutch", "nl_NL", "nl", false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Test"));
    let raw = model.vals_from_values(&values).unwrap();
    let id = model.create(&mut tx, vec![raw]).await.unwrap().rec_ids()[0];
    tx.flush().await.unwrap();
    let rec = model.browse([id.get()]).unwrap();

    async fn lang_read_ok(tx: &mut Transaction, rec: &Records, lang: &str) -> bool {
        rec.with_context([("lang", Value::str(lang))])
            .read(tx, &["name"])
            .await
            .is_ok()
    }

    // Memo is loaded by this refused read; nl_NL is inactive.
    assert!(!lang_read_ok(&mut tx, &rec, "nl_NL").await);

    // write of `active` drops the memo.
    let langs = tx.env().records("res.lang").unwrap();
    let nl_rec = langs.browse([nl.get()]).unwrap();
    let mut on = ValueMap::new();
    on.insert("active".to_string(), Value::Bool(true));
    let raw_on = nl_rec.vals_from_values(&on).unwrap();
    nl_rec.write(&mut tx, raw_on).await.unwrap();
    assert!(lang_read_ok(&mut tx, &rec, "nl_NL").await);

    // create drops the memo.
    assert!(!lang_read_ok(&mut tx, &rec, "es_ES").await);
    create_lang(&mut tx, "Spanish", "es_ES", "es", true).await;
    assert!(lang_read_ok(&mut tx, &rec, "es_ES").await);

    // unlink drops the memo.
    nl_rec.unlink(&mut tx).await.unwrap();
    assert!(!lang_read_ok(&mut tx, &rec, "nl_NL").await);

    h.cleanup().await.unwrap();
}

/// Reviewer finding 1: a `res.lang` write followed by a translated write in the same transaction,
/// without a flush in between, must see the new language set.
#[tokio::test]
async fn translated_write_sees_unflushed_res_lang_changes() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;
    let nl = create_lang(&mut tx, "Dutch", "nl_NL", "nl", false).await;
    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Test"));
    let raw = model.vals_from_values(&values).unwrap();
    let id = model.create(&mut tx, vec![raw]).await.unwrap().rec_ids()[0];
    tx.flush().await.unwrap();
    let rec = model.browse([id.get()]).unwrap();

    let dict = |code: &str| {
        let mut d = ValueMap::new();
        d.insert(code.to_string(), Value::str("x"));
        let mut v = ValueMap::new();
        v.insert("name".to_string(), Value::Map(Arc::new(d)));
        rec.vals_from_values(&v).unwrap()
    };
    // Loads the memo while nl_NL is inactive.
    let err = rec.write(&mut tx, dict("nl_NL")).await.unwrap_err();
    assert!(err.message().contains("not active"), "{}", err.message());

    let langs = tx.env().records("res.lang").unwrap();
    let nl_rec = langs.browse([nl.get()]).unwrap();
    let mut on = ValueMap::new();
    on.insert("active".to_string(), Value::Bool(true));
    let raw_on = nl_rec.vals_from_values(&on).unwrap();
    nl_rec.write(&mut tx, raw_on).await.unwrap();
    // No flush here.
    rec.write(&mut tx, dict("nl_NL")).await.unwrap();

    // Deactivating fr_FR without a flush must refuse a fr_FR write.
    let fr = tx
        .query("SELECT id FROM res_lang WHERE code = 'fr_FR'", &[])
        .await
        .unwrap();
    let fr_id: i64 = fr[0].get(0);
    let fr_rec = langs.browse([fr_id]).unwrap();
    let mut off = ValueMap::new();
    off.insert("active".to_string(), Value::Bool(false));
    let raw_off = fr_rec.vals_from_values(&off).unwrap();
    fr_rec.write(&mut tx, raw_off).await.unwrap();
    let err = rec.write(&mut tx, dict("fr_FR")).await.unwrap_err();
    assert!(err.message().contains("not active"), "{}", err.message());

    h.cleanup().await.unwrap();
}

/// Reviewer findings 3 and 5: required translated fields refuse null/False writes with a
/// validation error, and map entries that are neither strings nor False are refused.
#[tokio::test]
async fn translated_write_boundaries_fail_loudly() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;
    let req = tx.env().records("test.translated.req").unwrap();
    let mut values = ValueMap::new();
    values.insert("title".to_string(), Value::str("Needed"));
    let raw = req.vals_from_values(&values).unwrap();
    let id = req.create(&mut tx, vec![raw]).await.unwrap().rec_ids()[0];
    tx.flush().await.unwrap();
    let rec = req.browse([id.get()]).unwrap();

    let mut clear = ValueMap::new();
    clear.insert("title".to_string(), Value::False);
    let raw_clear = rec.vals_from_values(&clear).unwrap();
    let err = rec.write(&mut tx, raw_clear).await.unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert!(
        err.message().contains("Missing required field 'title'"),
        "{}",
        err.message()
    );

    let mut bad = ValueMap::new();
    bad.insert("en_US".to_string(), Value::Int(5));
    let mut v = ValueMap::new();
    v.insert("title".to_string(), Value::Map(Arc::new(bad)));
    let err = rec.vals_from_values(&v).unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert!(
        err.message().contains("translation for 'en_US'"),
        "{}",
        err.message()
    );

    h.cleanup().await.unwrap();
}

/// Reviewer r2 M1: a fresh transaction (no active-languages memo yet) reading `display_name` in an
/// inactive language must raise like any other read, not project English.
#[tokio::test]
async fn display_name_in_inactive_language_fails_loudly() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, false, false, false).await;
    create_lang(&mut tx, "Dutch", "nl_NL", "nl", false).await;
    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Apple"));
    let raw = model.vals_from_values(&values).unwrap();
    let id = model.create(&mut tx, vec![raw]).await.unwrap().rec_ids()[0];
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("nl_NL"))])
        .browse([id.get()])
        .unwrap();
    let err = rec.read(&mut tx, &["display_name"]).await.unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::UserError);
    assert_eq!(err.message(), "Invalid language code: nl_NL");

    h.cleanup().await.unwrap();
}

/// Reviewer r2 M3/M4 (lead decision 1): a stored cell that is not a language map reaches no reader
/// as a string or as NULL; each reader class fails loudly naming the field.
#[tokio::test]
async fn corrupt_translated_cell_fails_loudly_in_every_reader_class() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;
    let model = tx.env().records("test.translated.record").unwrap();
    let mut values = ValueMap::new();
    values.insert("name".to_string(), Value::str("Apple"));
    let raw = model.vals_from_values(&values).unwrap();
    let id = model.create(&mut tx, vec![raw]).await.unwrap().rec_ids()[0];
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE test_translated_record SET name = '[\"x\"]'::jsonb WHERE id = $1",
        &[&id.get()],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();

    let rec = model.browse([id.get()]).unwrap();
    let mentions = |e: erp_fields::Error| {
        assert_eq!(
            e.kind(),
            erp_fields::ErrorKind::Programming,
            "{}",
            e.message()
        );
        assert!(
            e.message().contains("test.translated.record.name"),
            "{}",
            e.message()
        );
    };
    // dynamic scalar reader
    mentions(rec.value(&mut tx, "name").await.unwrap_err());
    tx.invalidate_all().await.unwrap();
    // batch reader
    mentions(rec.values(&mut tx, "name").await.unwrap_err());
    tx.invalidate_all().await.unwrap();
    // dictionary reader
    mentions(rec.read(&mut tx, &["name"]).await.unwrap_err());
    tx.invalidate_all().await.unwrap();
    // web reader
    let spec = erp_orm::Spec::new().field("name");
    mentions(rec.web_read(&mut tx, spec).await.unwrap_err());

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_069_searching_and_ordering_translated_fields() {
    use erp_orm::SearchOptions;
    use erp_sdk::{Domain, Operator};

    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, true, false).await;

    let model = tx.env().records("test.translated.record").unwrap();

    let mut vals1 = ValueMap::new();
    let mut trans1 = ValueMap::new();
    trans1.insert("en_US".to_string(), Value::str("Customers"));
    trans1.insert("fr_FR".to_string(), Value::str("Clients"));
    vals1.insert("name".to_string(), Value::Map(Arc::new(trans1)));
    let raw1 = model.vals_from_values(&vals1).unwrap();

    let mut vals2 = ValueMap::new();
    let mut trans2 = ValueMap::new();
    trans2.insert("en_US".to_string(), Value::str("Padawans"));
    trans2.insert("fr_FR".to_string(), Value::str("Apprentis"));
    vals2.insert("name".to_string(), Value::Map(Arc::new(trans2)));
    let raw2 = model.vals_from_values(&vals2).unwrap();

    let created = model.create(&mut tx, vec![raw1, raw2]).await.unwrap();
    let id_customers = created.rec_ids()[0];
    let id_padawans = created.rec_ids()[1];
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();

    // 1. Searches in en_US
    let rec_en = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("en_US"))]);
    let d_eq = Domain::condition("name", Operator::Eq, "Customers").unwrap();
    let res = rec_en.search(&mut tx, &d_eq).await.unwrap();
    assert_eq!(res.rec_ids(), &[id_customers]);

    let d_ilike = Domain::condition("name", Operator::ILike, "%padawan%").unwrap();
    let res = rec_en.search(&mut tx, &d_ilike).await.unwrap();
    assert_eq!(res.rec_ids(), &[id_padawans]);

    let d_in = Domain::condition("name", Operator::In, vec!["Customers", "Padawans"]).unwrap();
    let res = rec_en.search(&mut tx, &d_in).await.unwrap();
    assert_eq!(res.len(), 2);

    // 2. Searches in fr_FR
    let rec_fr = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))]);
    let d_eq = Domain::condition("name", Operator::Eq, "Clients").unwrap();
    let res = rec_fr.search(&mut tx, &d_eq).await.unwrap();
    assert_eq!(res.rec_ids(), &[id_customers]);

    let d_ilike = Domain::condition("name", Operator::ILike, "%apprenti%").unwrap();
    let res = rec_fr.search(&mut tx, &d_ilike).await.unwrap();
    assert_eq!(res.rec_ids(), &[id_padawans]);

    let d_in = Domain::condition("name", Operator::In, vec!["Clients", "Apprentis"]).unwrap();
    let res = rec_fr.search(&mut tx, &d_in).await.unwrap();
    assert_eq!(res.len(), 2);

    // In fr_FR, searching for English "Customers" matches nothing because French is set
    let d_en_in_fr = Domain::condition("name", Operator::Eq, "Customers").unwrap();
    let res = rec_fr.search(&mut tx, &d_en_in_fr).await.unwrap();
    assert_eq!(res.len(), 0);

    // 3. Searches in es_ES (no Spanish translations set -> falls back to English)
    let rec_es = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("es_ES"))]);
    let d_eq = Domain::condition("name", Operator::Eq, "Customers").unwrap();
    let res = rec_es.search(&mut tx, &d_eq).await.unwrap();
    assert_eq!(res.rec_ids(), &[id_customers]);

    let d_ilike = Domain::condition("name", Operator::ILike, "%padawan%").unwrap();
    let res = rec_es.search(&mut tx, &d_ilike).await.unwrap();
    assert_eq!(res.rec_ids(), &[id_padawans]);

    let d_in = Domain::condition("name", Operator::In, vec!["Customers", "Padawans"]).unwrap();
    let res = rec_es.search(&mut tx, &d_in).await.unwrap();
    assert_eq!(res.len(), 2);

    for (records, text) in [
        (&rec_en, "customers"),
        (&rec_fr, "clients"),
        (&rec_es, "customers"),
    ] {
        let exact = Domain::condition("name", Operator::EqILike, text).unwrap();
        assert_eq!(
            records.search(&mut tx, &exact).await.unwrap().rec_ids(),
            &[id_customers]
        );
    }

    // 4. Ordering by name in fr_FR puts 'Apprentis' before 'Clients'
    let order_fr = SearchOptions::default().order("name asc");
    let ordered_fr = rec_fr
        .search_with(&mut tx, &Domain::TRUE, &order_fr)
        .await
        .unwrap();
    assert_eq!(ordered_fr.rec_ids(), &[id_padawans, id_customers]);

    // Ordering by name in en_US puts 'Customers' before 'Padawans'
    let order_en = SearchOptions::default().order("name asc");
    let ordered_en = rec_en
        .search_with(&mut tx, &Domain::TRUE, &order_en)
        .await
        .unwrap();
    assert_eq!(ordered_en.rec_ids(), &[id_customers, id_padawans]);

    let rel = tx.env().records("test.translated.rel").unwrap();
    rel.create(
        &mut tx,
        vec![
            RawVals::new().set_cell("record_id", Cell::Id(id_customers.get())),
            RawVals::new().set_cell("record_id", Cell::Id(id_padawans.get())),
        ],
    )
    .await
    .unwrap();
    let grouped = rel
        .with_context([("lang", Value::str("fr_FR"))])
        ._read_group(
            &mut tx,
            erp_orm::ReadGroup::new(Domain::TRUE)
                .groupby(["record_id"])
                .order("record_id"),
        )
        .await
        .unwrap();
    let first = grouped.rows()[0][0].to_value(erp_orm::RecordsAs::Id);
    assert_eq!(first, Value::Int(id_padawans.get()));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_070_loading_all_languages_at_once() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, true).await;

    let model = tx.env().records("test.translated.record").unwrap();

    let mut vals1 = ValueMap::new();
    let mut trans1 = ValueMap::new();
    trans1.insert("en_US".to_string(), Value::str("Customers"));
    trans1.insert("fr_FR".to_string(), Value::str("Clients"));
    trans1.insert("nl_NL".to_string(), Value::str("Klanten"));
    vals1.insert("name".to_string(), Value::Map(Arc::new(trans1)));
    let raw1 = model.vals_from_values(&vals1).unwrap();

    let mut vals2 = ValueMap::new();
    vals2.insert("name".to_string(), Value::False);
    let raw2 = model.vals_from_values(&vals2).unwrap();

    let created = model.create(&mut tx, vec![raw1, raw2]).await.unwrap();
    let id1 = created.rec_ids()[0];
    let id2 = created.rec_ids()[1];
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();

    let rec1_fr = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([
            ("prefetch_langs", Value::Bool(true)),
            ("lang", Value::str("fr_FR")),
        ])
        .browse([id1.get()])
        .unwrap();

    let rec1_nl = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("nl_NL"))])
        .browse([id1.get()])
        .unwrap();

    let rec1_en = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("lang", Value::str("en_US"))])
        .browse([id1.get()])
        .unwrap();

    let _ = rec1_fr.value(&mut tx, "name").await.unwrap();
    let _ = rec1_nl.value(&mut tx, "name").await.unwrap();
    let _ = rec1_en.value(&mut tx, "name").await.unwrap();
    tx.invalidate_records(&rec1_fr, Some(&["name"]))
        .await
        .unwrap();

    let before_queries = tx.query_count();
    let val_fr = rec1_fr.value(&mut tx, "name").await.unwrap();
    assert_eq!(val_fr, Value::str("Clients"));
    let first_read_queries = tx.query_count() - before_queries;
    assert_eq!(first_read_queries, 1, "First read should take 1 query");

    let before_nl = tx.query_count();
    let val_nl = rec1_nl.value(&mut tx, "name").await.unwrap();
    assert_eq!(val_nl, Value::str("Klanten"));
    assert_eq!(
        tx.query_count() - before_nl,
        0,
        "Subsequent nl read should take 0 queries"
    );

    let before_en = tx.query_count();
    let val_en = rec1_en.value(&mut tx, "name").await.unwrap();
    assert_eq!(val_en, Value::str("Customers"));
    assert_eq!(
        tx.query_count() - before_en,
        0,
        "Subsequent en read should take 0 queries"
    );

    let rec1_all = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .with_context([("prefetch_langs", Value::Bool(true))])
        .browse([id1.get()])
        .unwrap();
    assert_eq!(
        rec1_all.value(&mut tx, "name").await.unwrap(),
        Value::str("Customers")
    );
    assert_eq!(
        rec1_all.values(&mut tx, "name").await.unwrap(),
        vec![Value::str("Customers")]
    );
    let rows = rec1_all.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(rows[0].get("name"), Some(&Value::str("Customers")));
    let all_map = rec1_all
        .get_field_translations(&mut tx, "name")
        .await
        .unwrap();
    assert_eq!(all_map.get("fr_FR"), Some("Clients"));

    let rec2_fr = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .browse([id2.get()])
        .unwrap();
    assert_eq!(rec2_fr.value(&mut tx, "name").await.unwrap(), Value::False);
    let before_null = tx.query_count();
    let translations2 = rec2_fr
        .get_field_translations(&mut tx, "name")
        .await
        .unwrap();
    assert!(translations2.is_empty());
    assert_eq!(tx.query_count() - before_null, 0);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_071_update_field_translations() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;

    let model = tx.env().records("test.translated.record").unwrap();
    let mut vals = ValueMap::new();
    vals.insert("name".to_string(), Value::str("Initial Name"));
    let raw = model.vals_from_values(&vals).unwrap();
    let rec = model.create(&mut tx, vec![raw]).await.unwrap();
    let id = rec.rec_ids()[0];
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .browse([id.get()])
        .unwrap();

    let updated = rec
        .update_field_translations(
            &mut tx,
            "name",
            [
                ("en_US", Value::str("English Name")),
                ("fr_FR", Value::str("French Name")),
            ],
        )
        .await
        .unwrap();
    assert!(updated);

    let val_en = rec
        .with_context([("lang", Value::str("en_US"))])
        .value(&mut tx, "name")
        .await
        .unwrap();
    assert_eq!(val_en, Value::str("English Name"));

    let val_fr = rec
        .with_context([("lang", Value::str("fr_FR"))])
        .value(&mut tx, "name")
        .await
        .unwrap();
    assert_eq!(val_fr, Value::str("French Name"));

    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .browse([id.get()])
        .unwrap();

    let updated = rec
        .update_field_translations(&mut tx, "name", [("fr_FR", Value::False)])
        .await
        .unwrap();
    assert!(updated);

    let val_fr = rec
        .with_context([("lang", Value::str("fr_FR"))])
        .value(&mut tx, "name")
        .await
        .unwrap();
    assert_eq!(val_fr, Value::str("English Name"));

    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .browse([id.get()])
        .unwrap();

    rec.with_context([("lang", Value::str("en_US"))])
        .set_value(&mut tx, "name", Value::str("English Name 2"))
        .await
        .unwrap();

    let val_en = rec
        .with_context([("lang", Value::str("en_US"))])
        .value(&mut tx, "name")
        .await
        .unwrap();
    assert_eq!(val_en, Value::str("English Name 2"));

    let val_fr = rec
        .with_context([("lang", Value::str("fr_FR"))])
        .value(&mut tx, "name")
        .await
        .unwrap();
    assert_eq!(val_fr, Value::str("English Name 2"));

    let err = rec
        .update_field_translations(&mut tx, "name", [("de_DE", Value::str("German"))])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "The following languages are not activated: de_DE"
    );

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn revision_invalid_search_and_order_languages() {
    use erp_orm::SearchOptions;
    use erp_sdk::{Domain, Operator};
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, false, false, false).await;
    for lang in ["fr_FR".to_owned(), "bad\0lang".to_owned(), "x".repeat(65)] {
        let rec = tx
            .env()
            .records("test.translated.record")
            .unwrap()
            .with_context([("lang", Value::str(&lang))]);
        let domain = Domain::condition("name", Operator::Eq, "test").unwrap();
        for result in [
            rec.search_query(&mut tx, &domain, &SearchOptions::default())
                .await,
            rec.search_query(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::default().order("name"),
            )
            .await,
        ] {
            let err = result.expect_err("invalid language must be refused before SQL");
            assert_eq!(err.kind(), erp_fields::ErrorKind::UserError);
            assert_eq!(err.message(), format!("Invalid language code: {lang}"));
        }
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn revision_corrupt_translation_maps_are_refused() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test.translated.record").unwrap();
    let rec = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Base".into()))],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    for json in [
        r#"{"en_US":"Base","fr_FR":42}"#,
        r#"{"en_US":"Base","fr_FR":null}"#,
        r#"["bad"]"#,
    ] {
        tx.execute(
            "UPDATE test_translated_record SET name = $1::text::jsonb WHERE id = $2",
            &[&json, &rec.rec_ids()[0].get()],
        )
        .await
        .unwrap();
        tx.invalidate_all().await.unwrap();
        let err = rec
            .get_field_translations(&mut tx, "name")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        let expected = if json.starts_with('[') {
            "test.translated.record.name: translated cell must be a language map of strings"
        } else {
            "test.translated.record.name: expected a string translation for language 'fr_FR'"
        };
        assert_eq!(err.message(), expected);
        let err = rec
            .with_context([("prefetch_langs", Value::Bool(true))])
            .value(&mut tx, "name")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        let projected_error = if json.starts_with('[') {
            "test.translated.record.name: translated cell holds a JSON array instead of a language map"
        } else {
            expected
        };
        assert_eq!(err.message(), projected_error);
        let err = rec
            .update_field_translations(&mut tx, "name", [("en_US", Value::str("New"))])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(err.message(), expected);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn revision_falsy_english_base_is_ignored() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx, true, false, false).await;
    for val in [
        Value::False,
        Value::Bool(false),
        Value::Json(Arc::new(serde_json::Value::Null)),
    ] {
        for (model_name, field) in [
            ("test.translated.record", "name"),
            ("test.translated.req", "title"),
        ] {
            let model = tx.env().records(model_name).unwrap();
            let rec = model
                .create(
                    &mut tx,
                    vec![RawVals::new().set_cell(field, Cell::Str("Base".into()))],
                )
                .await
                .unwrap();
            rec.update_field_translations(&mut tx, field, [("fr_FR", Value::str("Français"))])
                .await
                .unwrap();
            assert!(
                !rec.update_field_translations(&mut tx, field, [("en_US", val.clone())])
                    .await
                    .unwrap()
            );
            tx.flush().await.unwrap();
            tx.invalidate_records(&rec, Some(&[field])).await.unwrap();
            let map = rec.get_field_translations(&mut tx, field).await.unwrap();
            assert_eq!(map.get("en_US"), Some("Base"));
            assert_eq!(map.get("fr_FR"), Some("Français"));
            assert_eq!(rec.value(&mut tx, field).await.unwrap(), Value::str("Base"));
            assert!(
                rec.update_field_translations(
                    &mut tx,
                    field,
                    [("en_US", val.clone()), ("fr_FR", val.clone())]
                )
                .await
                .unwrap()
            );
            let map = rec.get_field_translations(&mut tx, field).await.unwrap();
            assert_eq!(map.get("en_US"), Some("Base"));
            assert_eq!(map.get("fr_FR"), None);
        }
    }
    let model = tx.env().records("test.translated.record").unwrap();
    let rec = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Base".into()))],
        )
        .await
        .unwrap();
    assert!(
        rec.update_field_translations(&mut tx, "name", [("en_US", Value::str(""))])
            .await
            .unwrap()
    );
    assert_eq!(rec.value(&mut tx, "name").await.unwrap(), Value::str(""));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn revision_translation_update_invalid_values_and_duplicates() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .records("test.translated.record")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Base".into()))],
        )
        .await
        .unwrap();
    let err = rec
        .update_field_translations(&mut tx, "name", [("en_US", Value::Int(42))])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "Translations for model translated fields only accept falsy values and str, got Int(42) for en_US"
    );
    assert!(
        !rec.update_field_translations(&mut tx, "name", std::iter::empty::<(&str, Value)>())
            .await
            .unwrap()
    );
    rec.update_field_translations(
        &mut tx,
        "name",
        [("en_US", Value::False), ("en_US", Value::str("Final"))],
    )
    .await
    .unwrap();
    assert_eq!(
        rec.value(&mut tx, "name").await.unwrap(),
        Value::str("Final")
    );
    rec.update_field_translations(
        &mut tx,
        "name",
        [("en_US", Value::str("First")), ("en_US", Value::False)],
    )
    .await
    .unwrap();
    assert_eq!(
        rec.value(&mut tx, "name").await.unwrap(),
        Value::str("Final")
    );
    h.cleanup().await.unwrap();
}
