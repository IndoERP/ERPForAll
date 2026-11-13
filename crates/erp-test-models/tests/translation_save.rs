//! `TranslationImporter::save` against PostgreSQL (task, design
//! and §4.2).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_data::ast::XmlId;
use erp_data::load::BatchLimits;
use erp_data::model_data::{self, XidBinding};
use erp_data::{OverwriteMode, TranslationImporter};
use erp_fields::{Cell, ErrorKind, RecId, Value, ValueMap};
use erp_orm::{RawVals, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;

mod test_translated_addon {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_translated_save_addon", depends = ["base"]);

    #[model(
        name = "test.translated.record",
        table = "test_translated_record",
        addon = "test_translated_save_addon"
    )]
    pub struct TestTranslatedRecord {
        #[field(char, translate = true)]
        pub name: Option<Arc<str>>,
        #[field(char)]
        pub plain: Option<Arc<str>>,
    }

    #[model(
        name = "test.translated.req",
        table = "test_translated_req",
        addon = "test_translated_save_addon"
    )]
    pub struct TestTranslatedReq {
        #[field(char, translate = true, required = true)]
        pub title: Option<Arc<str>>,
    }

    #[model(
        name = "test.translated.html",
        table = "test_translated_html",
        addon = "test_translated_save_addon"
    )]
    pub struct TestTranslatedHtml {
        #[field(html, translate = true)]
        pub body: Option<Arc<str>>,
        #[field(html, translate = true, sanitize = false)]
        pub raw: Option<Arc<str>>,
    }

    #[model(
        name = "test.translated.html.req",
        table = "test_translated_html_req",
        addon = "test_translated_save_addon"
    )]
    pub struct TestTranslatedHtmlReq {
        #[field(html, translate = true, required = true)]
        pub body: Option<Arc<str>>,
    }
}

const MODEL: &str = "test.translated.record";

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &test_translated_addon::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

async fn setup_langs(tx: &mut Transaction) {
    for (name, code, url) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        let mut vals = RawVals::new();
        vals.insert("name".into(), Cell::Str(name.into()));
        vals.insert("code".into(), Cell::Str(code.into()));
        vals.insert("url_code".into(), Cell::Str(url.into()));
        vals.insert("active".into(), Cell::Bool(true));
        vals.insert("direction".into(), Cell::Str("ltr".into()));
        vals.insert("date_format".into(), Cell::Str("%m/%d/%Y".into()));
        vals.insert("time_format".into(), Cell::Str("%H:%M:%S".into()));
        vals.insert("week_start".into(), Cell::Str("7".into()));
        vals.insert("grouping".into(), Cell::Str("[3,0]".into()));
        vals.insert("decimal_point".into(), Cell::Str(".".into()));
        vals.insert("thousands_sep".into(), Cell::Str(",".into()));
        let exists = !tx
            .query("SELECT 1 FROM res_lang WHERE code = $1", &[&code])
            .await
            .unwrap()
            .is_empty();
        if !exists {
            let records = tx.env().records("res.lang").unwrap();
            records.create(tx, vec![vals]).await.unwrap();
        }
    }
}

/// A record of `model` whose translated `field` holds `langs` (none: SQL NULL), bound to `xid`.
async fn record(
    tx: &mut Transaction,
    model: &str,
    field: &str,
    langs: &[(&str, &str)],
    xid: &str,
    noupdate: bool,
) -> RecId {
    let records = tx.env().records(model).unwrap();
    let mut values = ValueMap::new();
    if !langs.is_empty() {
        let mut dict = ValueMap::new();
        for (lang, text) in langs {
            dict.insert((*lang).to_owned(), Value::str(text));
        }
        values.insert(field.to_owned(), Value::Map(Arc::new(dict)));
    }
    let raw = records.vals_from_values(&values).unwrap();
    let created = records.create(tx, vec![raw]).await.unwrap();
    let id = created.rec_ids()[0];
    tx.flush().await.unwrap();
    let env = tx.env();
    model_data::update_xmlids(
        tx,
        &env,
        &[XidBinding::new(
            XmlId::parse(xid).unwrap(),
            model,
            id.get(),
            noupdate,
        )],
        false,
    )
    .await
    .unwrap();
    id
}

/// A PO catalog with one `fr_FR` translation per `(xid, source, text)`.
fn importer(model: &str, field: &str, rows: &[(&str, &str, &str)]) -> TranslationImporter {
    let mut po = String::from("msgid \"\"\nmsgstr \"\"\n\n");
    for (xid, src, text) in rows {
        po.push_str(&format!(
            "#: model:{model},{field}:{xid}\nmsgid \"{src}\"\nmsgstr \"{text}\"\n\n"
        ));
    }
    let mut imp = TranslationImporter::new();
    imp.load_po(&po, "t.po", "fr_FR").unwrap();
    imp
}

async fn raw(tx: &mut Transaction, table: &str, col: &str, id: RecId) -> Option<serde_json::Value> {
    let rows = tx
        .query(
            &format!("SELECT {col} FROM {table} WHERE id = $1"),
            &[&id.get()],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

async fn name_of(tx: &mut Transaction, id: RecId) -> serde_json::Value {
    raw(tx, "test_translated_record", "name", id).await.unwrap()
}

#[tokio::test]
async fn overwrite_modes_respect_noupdate() {
    // (mode, expected fr_FR of the noupdate record, of the updatable record)
    for (mode, kept, updated) in [
        (OverwriteMode::NoOverwrite, "old", "old"),
        (OverwriteMode::Overwrite, "old", "new"),
        (OverwriteMode::ForceOverwrite, "new", "new"),
    ] {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        setup_langs(&mut tx).await;
        let both = [("en_US", "Src"), ("fr_FR", "old")];
        let a = record(&mut tx, MODEL, "name", &both, "m.a", true).await;
        let b = record(&mut tx, MODEL, "name", &both, "m.b", false).await;
        let imp = importer(
            MODEL,
            "name",
            &[("m.a", "Src", "new"), ("m.b", "Src", "new")],
        );
        let summary = imp.save(&mut tx, mode).await.unwrap();
        assert_eq!(summary.records_updated, 2, "{mode:?}");
        assert_eq!(
            name_of(&mut tx, a).await["fr_FR"],
            kept,
            "{mode:?} noupdate"
        );
        assert_eq!(
            name_of(&mut tx, b).await["fr_FR"],
            updated,
            "{mode:?} updatable"
        );
        assert_eq!(
            name_of(&mut tx, a).await["en_US"],
            "Src",
            "{mode:?} keeps en_US"
        );
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn no_overwrite_adds_a_missing_language_and_keeps_the_others() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;
    importer(MODEL, "name", &[("m.a", "Src", "Nom")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap();
    let stored = name_of(&mut tx, id).await;
    assert_eq!(stored["en_US"], "Src");
    assert_eq!(stored["fr_FR"], "Nom");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn duplicate_entries_of_one_record_keep_the_last_with_one_record_per_statement() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let a = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;
    let b = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.b", false).await;
    // the same key twice; one record per statement forces two statements
    let imp = importer(
        MODEL,
        "name",
        &[
            ("m.a", "Src", "first"),
            ("m.b", "Src", "other"),
            ("m.a", "Src", "last"),
        ],
    );
    let limits = BatchLimits::default().with_max_records(1).unwrap();
    let summary = imp
        .save_with_limits(&mut tx, OverwriteMode::NoOverwrite, limits)
        .await
        .unwrap();
    assert_eq!(summary.entries_processed, 3);
    assert_eq!(summary.records_updated, 2);
    assert_eq!(name_of(&mut tx, a).await["fr_FR"], "last");
    assert_eq!(name_of(&mut tx, b).await["fr_FR"], "other");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn save_refuses_a_transaction_that_is_not_system_with_full_elevation() {
    let h = harness().await;
    let mut tx = h.begin_as(2).await.unwrap();
    let err = importer(MODEL, "name", &[("m.a", "Src", "x")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert!(
        err.message().starts_with(
            "TranslationImporter::save: only a transaction of the System principal with full \
             elevation may do this"
        ),
        "{}",
        err.message()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_missing_external_id_fails_before_any_record_changes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;
    let imp = importer(
        MODEL,
        "name",
        &[("m.a", "Src", "Nom"), ("m.gone", "Src", "Perdu")],
    );
    let err = imp
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        "translation target 'm.gone' names no record of 'test.translated.record'"
    );
    assert!(name_of(&mut tx, id).await.get("fr_FR").is_none());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn unknown_model_untranslated_field_and_inactive_language_are_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;

    let err = importer("no.such", "name", &[("m.a", "S", "x")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(err.message(), "translation of an unknown model 'no.such'");

    let err = importer(MODEL, "plain", &[("m.a", "S", "x")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "field 'plain' of 'test.translated.record' is not translated"
    );

    let mut imp = TranslationImporter::new();
    let po = "msgid \"\"\nmsgstr \"\"\n\n#: model:test.translated.record,name:m.a\nmsgid \"S\"\nmsgstr \"x\"\n";
    imp.load_po(po, "t.po", "nl_NL").unwrap();
    let err = imp
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(err.message(), "Invalid language code: nl_NL");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_empty_column_is_seeded_with_the_source_text_never_french_only() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(&mut tx, MODEL, "name", &[], "m.a", false).await;
    assert!(
        raw(&mut tx, "test_translated_record", "name", id)
            .await
            .is_none()
    );
    importer(MODEL, "name", &[("m.a", "Source", "Nom")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap();
    let stored = name_of(&mut tx, id).await;
    assert_eq!(stored["en_US"], "Source");
    assert_eq!(stored["fr_FR"], "Nom");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_empty_column_without_a_source_is_refused_and_stays_null() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(&mut tx, MODEL, "name", &[], "m.a", false).await;
    // a record that sets no source for the field: the entry carries no `src`
    let ops = erp_data::xml::parse_xml_str(
        r#"<odoo><record id="a" model="test.translated.record"><field name="name" lang="fr_FR">Nom</field></record></odoo>"#,
        "t.xml",
        "m",
        false,
    )
    .unwrap();
    let erp_data::ast::DataOperation::Record(op) = &ops[0] else {
        panic!("a record")
    };
    let mut imp = TranslationImporter::new();
    imp.load_record_op("m", op).unwrap();
    let err = imp
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message()
            .contains("a translation map without en_US is refused"),
        "{}",
        err.message()
    );
    assert!(
        raw(&mut tx, "test_translated_record", "name", id)
            .await
            .is_none()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_required_field_keeps_its_source_text_when_a_translation_is_saved() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(
        &mut tx,
        "test.translated.req",
        "title",
        &[("en_US", "Kept")],
        "m.r",
        false,
    )
    .await;
    let mut imp = TranslationImporter::new();
    let po = "msgid \"\"\nmsgstr \"\"\n\n#: model:test.translated.req,title:m.r\nmsgid \"Kept\"\nmsgstr \"Garde\"\n";
    imp.load_po(po, "t.po", "fr_FR").unwrap();
    imp.save(&mut tx, OverwriteMode::ForceOverwrite)
        .await
        .unwrap();
    let stored = raw(&mut tx, "test_translated_req", "title", id)
        .await
        .unwrap();
    assert_eq!(stored["en_US"], "Kept");
    assert_eq!(stored["fr_FR"], "Garde");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn save_drops_cached_values_so_the_same_transaction_reads_the_translation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;
    let fr = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse([id.get()])
        .unwrap()
        .with_context([("lang", Value::str("fr_FR"))]);
    let before = fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(before[0].get("name"), Some(&Value::str("Src")));
    importer(MODEL, "name", &[("m.a", "Src", "Nom")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap();
    let after = fr.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(after[0].get("name"), Some(&Value::str("Nom")));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_null_en_us_in_the_column_is_refused_not_left_in_place() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;
    tx.execute(
        "UPDATE test_translated_record SET name = '{\"en_US\": null, \"fr_FR\": \"x\"}'::jsonb WHERE id = $1",
        &[&id.get()],
    )
    .await
    .unwrap();
    let err = importer(MODEL, "name", &[("m.a", "Src", "Nom")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("holds a null or non-text value"),
        "{}",
        err.message()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_numeric_en_us_in_the_column_is_refused_not_left_in_place() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;
    tx.execute(
        "UPDATE test_translated_record SET name = '{\"en_US\": 5}'::jsonb WHERE id = $1",
        &[&id.get()],
    )
    .await
    .unwrap();
    let err = importer(MODEL, "name", &[("m.a", "Src", "Nom")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("holds a null or non-text value"),
        "{}",
        err.message()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_empty_language_code_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;
    let po = "msgid \"\"\nmsgstr \"\"\n\n#: model:test.translated.record,name:m.a\nmsgid \"S\"\nmsgstr \"x\"\n";
    let mut imp = TranslationImporter::new();
    imp.load_po(po, "t.po", "").unwrap();
    let err = imp
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "a translation entry has an empty language code"
    );
    h.cleanup().await.unwrap();
}

/// Bind one more external id to an existing record.
async fn alias(tx: &mut Transaction, model: &str, id: RecId, xid: &str) {
    let env = tx.env();
    model_data::update_xmlids(
        tx,
        &env,
        &[XidBinding::new(
            XmlId::parse(xid).unwrap(),
            model,
            id.get(),
            false,
        )],
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn two_external_ids_of_one_record_are_one_record_updated_across_chunks() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let a = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.a", false).await;
    alias(&mut tx, MODEL, a, "m.z_alias").await;
    let b = record(&mut tx, MODEL, "name", &[("en_US", "Src")], "m.b", false).await;
    // sorted ids are m.a, m.b, m.z_alias: with one entry per statement the aliases of `a`
    // would land in different chunks
    let imp = importer(
        MODEL,
        "name",
        &[
            ("m.a", "Src", "from a"),
            ("m.b", "Src", "from b"),
            ("m.z_alias", "Src", "from alias"),
        ],
    );
    let limits = BatchLimits::default().with_max_records(1).unwrap();
    let summary = imp
        .save_with_limits(&mut tx, OverwriteMode::NoOverwrite, limits)
        .await
        .unwrap();
    assert_eq!(summary.records_updated, 2);
    assert!(name_of(&mut tx, a).await["fr_FR"].is_string());
    assert_eq!(name_of(&mut tx, b).await["fr_FR"], "from b");
    h.cleanup().await.unwrap();
}

const HTML_MODEL: &str = "test.translated.html";
const HOSTILE: &str = "<p onclick='x'>Bonjour</p><script>alert(1)</script>";

#[tokio::test]
async fn html_translations_are_admitted_before_the_importer_stores_them() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let both = [("en_US", "<p>Hello</p>")];
    let target = record(&mut tx, HTML_MODEL, "body", &both, "m.a", false).await;
    // the reference: the same text written through the ORM translated write
    let reference = record(
        &mut tx,
        HTML_MODEL,
        "body",
        &[("en_US", "<p>Hello</p>"), ("fr_FR", HOSTILE)],
        "m.ref",
        false,
    )
    .await;
    let before = tx.html_reports().len();
    importer(HTML_MODEL, "body", &[("m.a", "<p>Hello</p>", HOSTILE)])
        .save(&mut tx, OverwriteMode::Overwrite)
        .await
        .unwrap();
    let stored = raw(&mut tx, "test_translated_html", "body", target)
        .await
        .unwrap();
    let expected = raw(&mut tx, "test_translated_html", "body", reference)
        .await
        .unwrap();
    assert_eq!(stored["fr_FR"], expected["fr_FR"]);
    let text = stored["fr_FR"].as_str().unwrap();
    assert_ne!(text, HOSTILE);
    assert!(!text.contains("onclick"), "{text}");
    assert!(!text.contains("<script"), "{text}");
    assert_eq!(tx.html_reports().len(), before + 1);
    let report = &tx.html_reports()[before];
    assert_eq!(report.model(), HTML_MODEL);
    assert_eq!(report.field(), "body");
    assert_eq!(report.language(), Some("fr_FR"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_seeded_html_source_is_admitted() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let target = record(&mut tx, HTML_MODEL, "body", &[], "m.a", false).await;
    let reference = record(
        &mut tx,
        HTML_MODEL,
        "body",
        &[("en_US", HOSTILE)],
        "m.ref",
        false,
    )
    .await;
    importer(HTML_MODEL, "body", &[("m.a", HOSTILE, "<p>Bonjour</p>")])
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap();
    let stored = raw(&mut tx, "test_translated_html", "body", target)
        .await
        .unwrap();
    let expected = raw(&mut tx, "test_translated_html", "body", reference)
        .await
        .unwrap();
    assert_eq!(stored["en_US"], expected["en_US"]);
    assert!(!stored["en_US"].as_str().unwrap().contains("onclick"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn strict_mode_refuses_an_html_translation_with_removals_and_stores_nothing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let target = record(
        &mut tx,
        HTML_MODEL,
        "body",
        &[("en_US", "<p>Hello</p>")],
        "m.a",
        false,
    )
    .await;
    tx.require_strict_html();
    let expected = tx
        .admit_html_translation(HTML_MODEL, "body", "fr_FR", HOSTILE)
        .unwrap_err();
    let err = importer(HTML_MODEL, "body", &[("m.a", "<p>Hello</p>", HOSTILE)])
        .save(&mut tx, OverwriteMode::Overwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), expected.message());
    assert!(
        err.message().starts_with("test.translated.html.body:"),
        "{}",
        err.message()
    );
    assert!(!err.message().contains("alert(1)"), "{}", err.message());
    let stored = raw(&mut tx, "test_translated_html", "body", target)
        .await
        .unwrap();
    assert!(stored.get("fr_FR").is_none());
    assert_eq!(stored["en_US"], "<p>Hello</p>");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_nul_byte_in_an_html_translation_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let target = record(
        &mut tx,
        HTML_MODEL,
        "body",
        &[("en_US", "<p>Hello</p>")],
        "m.a",
        false,
    )
    .await;
    let text = "<p>a\u{0}b</p>";
    let expected = tx
        .admit_html_translation(HTML_MODEL, "body", "fr_FR", text)
        .unwrap_err();
    let err = importer(HTML_MODEL, "body", &[("m.a", "<p>Hello</p>", text)])
        .save(&mut tx, OverwriteMode::Overwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), expected.message());
    let stored = raw(&mut tx, "test_translated_html", "body", target)
        .await
        .unwrap();
    assert!(stored.get("fr_FR").is_none());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_required_html_source_that_cleans_to_nothing_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(
        &mut tx,
        "test.translated.html.req",
        "body",
        &[("en_US", "<p>Kept</p>")],
        "m.r",
        false,
    )
    .await;
    // an empty column on a required field is a damaged row: lift the constraint to build one
    tx.execute(
        "ALTER TABLE test_translated_html_req ALTER COLUMN body DROP NOT NULL",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE test_translated_html_req SET body = NULL WHERE id = $1",
        &[&id.get()],
    )
    .await
    .unwrap();
    // the premise, from our own sanitizer: this source cleans to nothing
    let cleaned = tx
        .admit_html_translation(
            "test.translated.html.req",
            "body",
            "en_US",
            "<script>x</script>",
        )
        .unwrap();
    assert_eq!(cleaned, "");
    let mut imp = TranslationImporter::new();
    let po = "msgid \"\"\nmsgstr \"\"\n\n#: model:test.translated.html.req,body:m.r\nmsgid \"<script>x</script>\"\nmsgstr \"<p>Corps</p>\"\n";
    imp.load_po(po, "t.po", "fr_FR").unwrap();
    let err = imp
        .save(&mut tx, OverwriteMode::NoOverwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "'m.r' of 'test.translated.html.req': the source text of required field 'body' is empty \
         after HTML sanitation; en_US cannot be seeded from it"
    );
    assert!(
        raw(&mut tx, "test_translated_html_req", "body", id)
            .await
            .is_none()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn raw_text_html_translations_are_stored_unchanged_without_a_report() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    let id = record(
        &mut tx,
        HTML_MODEL,
        "raw",
        &[("en_US", "<p>Hello</p>")],
        "m.a",
        false,
    )
    .await;
    let before = tx.html_reports().len();
    importer(HTML_MODEL, "raw", &[("m.a", "<p>Hello</p>", HOSTILE)])
        .save(&mut tx, OverwriteMode::Overwrite)
        .await
        .unwrap();
    let stored = raw(&mut tx, "test_translated_html", "raw", id)
        .await
        .unwrap();
    assert_eq!(stored["fr_FR"], HOSTILE);
    assert_eq!(tx.html_reports().len(), before);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn selection_label_translation_fail_loudly_on_malformed_entries() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    setup_langs(&mut tx).await;
    erp_orm::reflection::reflect_models(&mut tx).await.unwrap();

    // 1. Unknown selection value fails loudly naming field and value
    let mut imp = TranslationImporter::new();
    let po_unknown = r#"
msgid ""
msgstr ""

#: model:ir.model.fields.selection,name:base.selection__res_partner__type__nonexistent
msgid "Nonexistent"
msgstr "Inexistant"
"#;
    imp.load_po(po_unknown, "t.po", "fr_FR").unwrap();
    let err = imp
        .save(&mut tx, OverwriteMode::Overwrite)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert!(
        err.message()
            .contains("unknown selection value 'nonexistent' for field 'type'")
    );

    // 2. Empty translation label fails loudly naming field and value
    let mut imp2 = TranslationImporter::new();
    let po_empty = r#"
msgid ""
msgstr ""

#: model:ir.model.fields.selection,name:base.selection__res_partner__type__other
msgid "Other"
msgstr "   "
"#;
    imp2.load_po(po_empty, "t.po", "fr_FR").unwrap();
    let err2 = imp2
        .save(&mut tx, OverwriteMode::Overwrite)
        .await
        .unwrap_err();
    assert_eq!(err2.kind(), ErrorKind::Validation);
    assert!(
        err2.message()
            .contains("selection field 'type' value 'other'")
    );
    assert!(err2.message().contains("empty label"));

    // 3. Valid translation succeeds
    let mut imp3 = TranslationImporter::new();
    let po_valid = r#"
msgid ""
msgstr ""

#: model:ir.model.fields.selection,name:base.selection__res_partner__type__other
msgid "Other"
msgstr "Autre"
"#;
    imp3.load_po(po_valid, "t.po", "fr_FR").unwrap();
    let summary = imp3.save(&mut tx, OverwriteMode::Overwrite).await.unwrap();
    assert_eq!(summary.records_updated, 1);

    drop(tx);
    h.cleanup().await.unwrap();
}
