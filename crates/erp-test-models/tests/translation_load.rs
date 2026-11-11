//! Translated values in data files load and save (task).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_data::load::{
    AddonFiles, FileContext, FileReadError, LoadError, LoadErrorKind, LoadSession, SessionOptions,
};
use erp_data::{ConvertMode, XmlDataReader};
use erp_fields::{Cell, ErrorKind, Value};
use erp_orm::{BoxFuture, RawVals, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;

mod test_tl_addon {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_tl_addon", depends = ["base"]);

    #[model(
        name = "test.tl.record",
        table = "test_tl_record",
        addon = "test_tl_addon"
    )]
    pub struct TlRecord {
        #[field(char, translate = true)]
        pub name: Option<Arc<str>>,
        #[field(char)]
        pub plain: Option<Arc<str>>,
        #[field(one2many, comodel = "test.tl.line", inverse_name = "record_id")]
        pub line_ids: Vec<RecId>,
    }

    #[model(name = "test.tl.line", table = "test_tl_line", addon = "test_tl_addon")]
    pub struct TlLine {
        #[field(char, translate = true)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "test.tl.record")]
        pub record_id: Option<RecId>,
    }
}

struct NoFiles;

impl AddonFiles for NoFiles {
    fn read<'a>(
        &'a self,
        module: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileReadError>> {
        Box::pin(async move {
            Err(FileReadError::NotFound {
                module: module.to_owned(),
                path: path.to_owned(),
            })
        })
    }
}

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &test_tl_addon::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

async fn french(tx: &mut Transaction) {
    for (name, code, url) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        let exists = !tx
            .query("SELECT 1 FROM res_lang WHERE code = $1", &[&code])
            .await
            .unwrap()
            .is_empty();
        if exists {
            continue;
        }
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
        let records = tx.env().records("res.lang").unwrap();
        records.create(tx, vec![vals]).await.unwrap();
    }
}

fn session() -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)))
}

async fn load_xml(
    session: &mut LoadSession,
    tx: &mut Transaction,
    xml: &str,
) -> Result<(), LoadError> {
    let file = FileContext::new("m", "data/f.xml", ConvertMode::Init);
    let mut ops = XmlDataReader::new(xml, "data/f.xml", "m", false);
    session.load_file(tx, &file, &mut ops).await
}

async fn stored(tx: &mut Transaction, table: &str) -> Vec<serde_json::Value> {
    tx.query(&format!("SELECT name FROM {table} ORDER BY id"), &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test]
async fn spec_data_016_xml_translations_are_saved_beside_the_source_value() {
    for field in [
        "<field name=\"name@fr_FR\">Nom</field>",
        "<field name=\"name\" lang=\"fr_FR\">Nom</field>",
    ] {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        french(&mut tx).await;
        let mut session = session();
        load_xml(
            &mut session,
            &mut tx,
            &format!(
                "<odoo><record id=\"r\" model=\"test.tl.record\">\
                 <field name=\"name\">Source</field>{field}</record></odoo>"
            ),
        )
        .await
        .unwrap();
        let rows = stored(&mut tx, "test_tl_record").await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["en_US"], "Source");
        assert_eq!(rows[0]["fr_FR"], "Nom");
        h.cleanup().await.unwrap();
    }
}

/// French; the French key exists only when the file gives one)
#[tokio::test]
async fn spec_data_016_a_french_transaction_still_loads_the_source_value() {
    let h = harness().await;
    let mut ctx = erp_orm::Context::new();
    ctx.set("lang", Value::str("fr_FR"));
    let mut tx = h.orm().begin_with(h.principal(), ctx).await.unwrap();
    french(&mut tx).await;
    let mut session = session();
    load_xml(
        &mut session,
        &mut tx,
        "<odoo><record id=\"r\" model=\"test.tl.record\"><field name=\"name\">foo</field></record></odoo>",
    )
    .await
    .unwrap();
    let rows = stored(&mut tx, "test_tl_record").await;
    assert_eq!(rows[0]["en_US"], "foo");
    assert!(rows[0].get("fr_FR").is_none(), "{}", rows[0]);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_016_csv_translation_columns_are_saved() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    french(&mut tx).await;
    let mut session = session();
    let path = "data/test.tl.record.csv";
    let erp_data::CsvImportResult::Loaded(import) = erp_data::parse_csv(
        "id,name,name@fr_FR\ncsv_rec,csv_foo,csv_fr\n",
        path,
        "m",
        ConvertMode::Init,
    )
    .unwrap() else {
        panic!("expected loaded csv");
    };
    let file = FileContext::new("m", path, ConvertMode::Init);
    let mut ops = vec![Ok(import.into_operation(false))].into_iter();
    session.load_file(&mut tx, &file, &mut ops).await.unwrap();
    let rows = stored(&mut tx, "test_tl_record").await;
    assert_eq!(rows[0]["en_US"], "csv_foo");
    assert_eq!(rows[0]["fr_FR"], "csv_fr");
    h.cleanup().await.unwrap();
}

/// validates the language and seeds `en_US`)
#[tokio::test]
async fn spec_data_016_a_file_context_with_lang_writes_that_language() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    french(&mut tx).await;
    let mut session = session();
    load_xml(
        &mut session,
        &mut tx,
        "<odoo context=\"{'lang': 'fr_FR'}\"><record id=\"r\" model=\"test.tl.record\">\
         <field name=\"name\">Bonjour</field></record></odoo>",
    )
    .await
    .unwrap();
    let rows = stored(&mut tx, "test_tl_record").await;
    assert_eq!(rows[0]["en_US"], "Bonjour");
    assert_eq!(rows[0]["fr_FR"], "Bonjour");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_016_an_inactive_language_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    french(&mut tx).await;
    let mut session = session();
    let err = load_xml(
        &mut session,
        &mut tx,
        "<odoo><record id=\"r\" model=\"test.tl.record\"><field name=\"name\">S</field>\
         <field name=\"name@nl_NL\">X</field></record></odoo>",
    )
    .await
    .unwrap_err();
    let LoadErrorKind::Orm(source) = err.kind() else {
        panic!("expected Orm, got {:?}", err.kind());
    };
    assert_eq!(source.kind(), ErrorKind::UserError);
    assert_eq!(source.message(), "Invalid language code: nl_NL");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_016_a_translated_sub_record_value_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    french(&mut tx).await;
    let mut session = session();
    let err = load_xml(
        &mut session,
        &mut tx,
        "<odoo><record id=\"p\" model=\"test.tl.record\"><field name=\"name\">P</field>\
         <field name=\"line_ids\"><record id=\"c\" model=\"test.tl.line\">\
         <field name=\"name\">C</field><field name=\"name@fr_FR\">Fils</field></record></field>\
         </record></odoo>",
    )
    .await
    .unwrap_err();
    let LoadErrorKind::Unsupported { what, slice } = err.kind() else {
        panic!("expected Unsupported, got {:?}", err.kind());
    };
    assert_eq!(
        what,
        "translated field values ('name@fr_FR' on record 'c'; 1 in all)"
    );
    assert_eq!(
        slice,
        "a later release (translations of one2many sub-records)"
    );
    assert!(stored(&mut tx, "test_tl_record").await.is_empty());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_016_a_failed_file_does_not_leak_translations_into_the_next_file() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    french(&mut tx).await;
    let mut session = session();
    let failed = load_xml(
        &mut session,
        &mut tx,
        "<odoo><record id=\"r\" model=\"test.tl.record\"><field name=\"name\">S</field>\
         <field name=\"name@fr_FR\">Nom</field></record>\
         <record id=\"bad\" model=\"no.such.model\"/></odoo>",
    )
    .await;
    assert!(failed.is_err());
    // the failed file's record was never written; were its translation still collected, the next
    // file's save would name a record that does not exist and fail
    load_xml(
        &mut session,
        &mut tx,
        "<odoo><record id=\"ok\" model=\"test.tl.record\"><field name=\"name\">T</field></record></odoo>",
    )
    .await
    .unwrap();
    let rows = stored(&mut tx, "test_tl_record").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["en_US"], "T");
    h.cleanup().await.unwrap();
}

async fn load_update(
    session: &mut LoadSession,
    tx: &mut Transaction,
    xml: &str,
) -> Result<(), LoadError> {
    let file = FileContext::new("m", "data/u.xml", ConvertMode::Update);
    let mut ops = XmlDataReader::new(xml, "data/u.xml", "m", false);
    session.load_file(tx, &file, &mut ops).await
}

#[tokio::test]
async fn spec_data_016_update_mode_noupdate_skip_keeps_the_stored_translation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    french(&mut tx).await;
    let mut session = session();
    load_xml(
        &mut session,
        &mut tx,
        "<odoo noupdate=\"1\"><record id=\"r\" model=\"test.tl.record\"><field name=\"name\">S</field>\
         <field name=\"name@fr_FR\">Nom</field></record></odoo>",
    )
    .await
    .unwrap();
    load_update(
        &mut session,
        &mut tx,
        "<odoo noupdate=\"1\"><record id=\"r\" model=\"test.tl.record\"><field name=\"name\">S2</field>\
         <field name=\"name@fr_FR\">Nouveau</field></record></odoo>",
    )
    .await
    .unwrap();
    let rows = stored(&mut tx, "test_tl_record").await;
    assert_eq!(rows.len(), 1);
    // the record was skipped: neither the source nor the default (no-overwrite) translation changed
    assert_eq!(rows[0]["en_US"], "S");
    assert_eq!(rows[0]["fr_FR"], "Nom");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_016_a_record_skipped_for_an_unresolved_ref_leaves_no_orphan_translation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    french(&mut tx).await;
    let mut session = session();
    load_update(
        &mut session,
        &mut tx,
        "<odoo><record id=\"l\" model=\"test.tl.line\" forcecreate=\"0\">\
         <field name=\"name\">S</field><field name=\"name@fr_FR\">Nom</field>\
         <field name=\"record_id\" ref=\"no_such_record\"/></record></odoo>",
    )
    .await
    .unwrap();
    assert!(stored(&mut tx, "test_tl_line").await.is_empty());
    h.cleanup().await.unwrap();
}
