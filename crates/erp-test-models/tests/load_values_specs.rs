//! The value forms of the data loader: `search=`, files, lists, `type="xml"|"html"` with
//! `%(xid)d` markers and the language of the loader (partial).
//! Same shape as `load_env_specs.rs`: XML text through `XmlDataReader` into `LoadSession`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::HashMap;
use std::sync::Arc;

use erp_data::load::{
    AddonFiles, DataNotice, FileContext, FileReadError, LoadError, LoadErrorKind, LoadSession,
    NoticeKind, SessionOptions,
};
use erp_data::{ConvertMode, XmlDataReader};
use erp_orm::BoxFuture;
use erp_registry::{FieldDecl, ModelDecl};
use erp_sdk::{Cell, Context, Domain, Records, Transaction, Value};
use erp_test_models::{base_users, test_convert, test_convert_ui};
use erp_testkit::Harness;

// Membership-only language model for these minimal registries (the full base addon is absent).
const LANGUAGE_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("code").required(true),
    FieldDecl::boolean("active"),
];

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_convert::ADDON,
        &test_convert_ui::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("base_users", "res.lang").fields(LANGUAGE_FIELDS),
    ])
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

/// Files by `(module, path)`.
struct Files(HashMap<(String, String), Vec<u8>>);

impl Files {
    fn of(files: &[(&str, &str, &[u8])]) -> Arc<Self> {
        Arc::new(Self(
            files
                .iter()
                .map(|(m, p, c)| (((*m).to_owned(), (*p).to_owned()), c.to_vec()))
                .collect(),
        ))
    }
}

impl AddonFiles for Files {
    fn read<'a>(
        &'a self,
        module: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileReadError>> {
        Box::pin(async move {
            self.0
                .get(&(module.to_owned(), path.to_owned()))
                .cloned()
                .ok_or_else(|| FileReadError::NotFound {
                    module: module.to_owned(),
                    path: path.to_owned(),
                })
        })
    }
}

fn session_with(files: Arc<dyn AddonFiles>) -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", files))
}

fn session() -> LoadSession {
    session_with(Files::of(&[]))
}

async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    xml: &str,
    mode: ConvertMode,
) -> Result<(), LoadError> {
    let file = FileContext::new("m", "data/f.xml", mode);
    let mut ops = XmlDataReader::new(xml, "data/f.xml", "m", false);
    session.load_file(tx, &file, &mut ops).await
}

async fn init(session: &mut LoadSession, tx: &mut Transaction, xml: &str) -> Result<(), LoadError> {
    load(session, tx, xml, ConvertMode::Init).await
}

async fn all(tx: &mut Transaction, model: &str) -> Records {
    tx.env()
        .records(model)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
}

async fn cells(tx: &mut Transaction, model: &str, field: &str) -> Vec<Cell> {
    let records = all(tx, model).await;
    records.cells(tx, field).await.unwrap()
}

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

fn text_of(cell: &Cell) -> String {
    match cell {
        Cell::Str(text) => text.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

/// Two `usered` records `first` and `second`, in this order.
const USERED: &str = r#"
  <record id="u1" model="test_convert.usered"><field name="name">first</field></record>
  <record id="u2" model="test_convert.usered"><field name="name">second</field></record>"#;

#[tokio::test]
async fn spec_data_012_field_value_forms() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        &format!(
            r#"<odoo>{USERED}
  <record id="rec" model="test_convert.record">
    <field name="name">plain text</field>
    <field name="count" type="int">5</field>
    <field name="usered_m2m_ids" eval="[Command.set([ref('u1'), ref('u2')])]"/>
    <field name="usered_id" ref="u1"/>
  </record>
  <record id="found" model="test_convert.record">
    <field name="usered_id" search="[('name', '=', 'second')]"/>
    <field name="usered_m2m_ids" search="[]"/>
    <field name="name" model="test_convert.usered" search="[('name', '=', 'first')]" use="name"/>
  </record>
  <record id="none_found" model="test_convert.record">
    <field name="usered_id" search="[('name', '=', 'nobody')]"/>
    <field name="name" type="int">None</field>
  </record>
  <record id="listed" model="test_convert_ui.listed">
    <field name="data" type="list">
      <value>foo</value>
      <value type="int">5</value>
      <value type="float">4.76</value>
      <value type="int">None</value>
    </field>
  </record>
</odoo>"#
        ),
    )
    .await
    .unwrap();

    let useds = all(&mut tx, "test_convert.usered").await.ids();
    let (first, second) = (useds[0], useds[1]);
    let recs = all(&mut tx, "test_convert.record").await;
    let names = recs.cells(&mut tx, "name").await.unwrap();
    let counts = recs.cells(&mut tx, "count").await.unwrap();
    let ids = recs.cells(&mut tx, "usered_id").await.unwrap();
    // char text, type="int", ref=
    assert_eq!(names[0], s("plain text"));
    assert_eq!(counts[0], Cell::Int(5));
    assert_eq!(ids[0], Cell::Id(first));
    // search= on a many2one: the first match; use= reads another field of the match
    assert_eq!(ids[1], Cell::Id(second));
    assert_eq!(names[1], s("first"));
    // search= with no match: not set; the literal None of type="int" on a char field: not set
    assert_eq!(ids[2], Cell::Null);
    assert_eq!(names[2], Cell::Null);
    // eval of a command list, and search= on a many2many (every match, Command.set)
    for (index, expected) in [(0, vec![first, second]), (1, vec![first, second])] {
        let one = recs.browse([recs.ids()[index]]).unwrap();
        let Value::List(items) = one.value(&mut tx, "usered_m2m_ids").await.unwrap() else {
            panic!("a many2many reads as a list");
        };
        let mut linked: Vec<i64> = items
            .iter()
            .map(|v| match v {
                Value::Int(id) => *id,
                other => panic!("expected an id, got {other:?}"),
            })
            .collect();
        linked.sort_unstable();
        assert_eq!(linked, expected);
    }
    // a list of values: text, int, float and a null
    let data = cells(&mut tx, "test_convert_ui.listed", "data").await;
    let expected = erp_sdk::serde_json::json!(["foo", 5, 4.76, null]);
    assert_eq!(data, [Cell::Json(Arc::new(expected))]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_search_without_a_model_on_a_plain_field_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"name\" search=\"[]\"/></record></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(err.kind(), LoadErrorKind::SearchNeedsModel));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">\n\
         Define an attribute model=\"...\" in your .XML file!"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_search_on_a_model_that_is_not_installed_names_the_model() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"name\" model=\"no.such\" search=\"[]\"/></record></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(err.kind(), LoadErrorKind::ModelMissing { .. }));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">\n\
         search= model 'no.such' is not in the registry of this install step"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_search_domain_that_is_not_a_domain_is_the_domain_parsers_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"usered_id\" search=\"[('name', '=')]\"/></record></odoo>",
    )
    .await
    .unwrap_err();
    assert_eq!(err.orm_kind(), Some(erp_sdk::ErrorKind::Validation));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn file_forms_read_the_addon_and_type_file_checks_it_exists() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let files = Files::of(&[
        ("m", "data/hello.txt", "héllo\n".as_bytes()),
        ("m", "static/logo.png", &[0xff, 0x00, 0x7f]),
        ("other", "data/o.txt", b"from another addon"),
    ]);
    let mut session = session_with(files);
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="a" model="test_convert.record"><field name="name" file="m/data/hello.txt"/></record>
  <record id="b" model="test_convert.record"><field name="name" file="other/data/o.txt"/></record>
  <record id="c" model="test_convert.record"><field name="name" type="file">static/logo.png</field></record>
  <record id="d" model="test_convert.record"><field name="name" type="base64" file="m/data/hello.txt"/></record>
</odoo>"#,
    )
    .await
    .unwrap();
    let names = cells(&mut tx, "test_convert.record", "name").await;
    assert_eq!(names[0], s("héllo\n"));
    assert_eq!(names[1], s("from another addon"));
    assert_eq!(names[2], s("m,static/logo.png"));
    // base64 of the file's bytes
    assert_eq!(names[3], s("aMOpbGxvCg=="));
    let notices = session.take_notices();
    assert_eq!(
        notices,
        [DataNotice::new(
            "m",
            "data/f.xml",
            NoticeKind::DeprecatedBase64 {
                model: "test_convert.record".to_owned(),
                field: "name".to_owned()
            }
        )]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn bytes_that_are_text_load_into_a_char_field_and_others_are_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let files = Files::of(&[("m", "a.txt", b"plain"), ("m", "b.bin", &[0xff, 0xfe])]);
    let mut session = session_with(files);
    init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"a\" model=\"test_convert.record\">\
         <field name=\"name\" type=\"bytes\" file=\"m/a.txt\"/></record></odoo>",
    )
    .await
    .unwrap();
    assert_eq!(
        cells(&mut tx, "test_convert.record", "name").await,
        [s("plain")]
    );
    // a text file that is not UTF-8
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"b\" model=\"test_convert.record\">\
         <field name=\"name\" file=\"m/b.bin\"/></record></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(err.kind(), LoadErrorKind::FileNotText { .. }));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"b\" model=\"test_convert.record\">\n\
         m/b.bin is not valid UTF-8 text"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_missing_file_and_a_path_without_a_module_are_errors() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"a\" model=\"test_convert.record\">\
         <field name=\"name\" type=\"file\">static/x.png</field></record></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err.kind(),
        LoadErrorKind::File(FileReadError::NotFound { .. })
    ));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"a\" model=\"test_convert.record\">\n\
         File not found: m/static/x.png"
    );
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"b\" model=\"test_convert.record\">\
         <field name=\"name\" file=\"hello.txt\"/></record></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err.kind(),
        LoadErrorKind::File(FileReadError::Failed(_))
    ));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"b\" model=\"test_convert.record\">\n\
         file=\"hello.txt\" must be written <module>/<path inside the module>"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_015_xml_and_html_fields() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="other" model="test_convert_ui.listed"><field name="name">x</field></record>
  <record id="view" model="test_convert_ui.carrier">
    <field name="name">v</field>
    <field name="arch" type="xml"><a href="/r/%(other)d">100%% sure %(m.other)s</a><b/></field>
  </record>
  <record id="html" model="test_convert.record">
    <field name="body" type="html"><t/></field>
  </record>
</odoo>"#,
    )
    .await
    .unwrap();
    let other = all(&mut tx, "test_convert_ui.listed").await.ids()[0];
    let arch = text_of(&cells(&mut tx, "test_convert_ui.carrier", "arch").await[0]);
    assert!(
        arch.starts_with("<?xml version=\"1.0\"?>\n<data>"),
        "{arch}"
    );
    assert!(arch.ends_with("</data>"), "{arch}");
    // both spellings of the marker are replaced by the id; `%%` is a literal `%`
    assert!(arch.contains(&format!("href=\"/r/{other}\"")), "{arch}");
    assert!(arch.contains(&format!("100% sure {other}</a>")), "{arch}");
    assert!(!arch.contains("%("), "{arch}");
    // HTML values pass through the field policy; unknown tags are removed.
    let body = text_of(&cells(&mut tx, "test_convert.record", "body").await[0]);
    assert_eq!(body, "");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_marker_naming_an_unknown_record_is_an_external_id_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"v\" model=\"test_convert_ui.carrier\"><field name=\"name\">v</field>\
         <field name=\"arch\" type=\"xml\"><a x=\"%(nobody)d\"/></field></record></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err.kind(),
        LoadErrorKind::ExternalIdNotFound { .. }
    ));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"v\" model=\"test_convert_ui.carrier\">\n\
         External ID not found in the system: m.nobody"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_marker_with_an_invalid_external_id_is_refused_and_non_ascii_text_survives() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"v\" model=\"test_convert_ui.carrier\"><field name=\"name\">v</field>\
         <field name=\"arch\" type=\"xml\"><a x=\"%(no body)d\"/></field></record></odoo>",
    )
    .await
    .unwrap_err();
    assert_eq!(err.orm_kind(), Some(erp_sdk::ErrorKind::Validation));
    init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"w\" model=\"test_convert_ui.carrier\"><field name=\"name\">w</field>\
         <field name=\"arch\" type=\"xml\"><a>é 日本 100%%</a></field></record></odoo>",
    )
    .await
    .unwrap();
    let arch = text_of(&cells(&mut tx, "test_convert_ui.carrier", "arch").await[0]);
    assert!(arch.contains("<a>é 日本 100%</a>"), "{arch}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn type_xml_on_an_html_field_is_loaded_and_reported() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"body\" type=\"xml\"><p>hi</p></field></record></odoo>",
    )
    .await
    .unwrap();
    let body = text_of(&cells(&mut tx, "test_convert.record", "body").await[0]);
    // the loader writes the declaration on its own line; the policy turns it into a comment
    assert_eq!(body, "<!--?xml version=\"1.0\"?-->\n<p>hi</p>");
    assert_eq!(
        session.take_notices(),
        [DataNotice::new(
            "m",
            "data/f.xml",
            NoticeKind::HtmlDeclaredXml {
                model: "test_convert.record".to_owned(),
                field: "body".to_owned()
            }
        )]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// observed in `translation_load.rs`)
#[tokio::test]
async fn spec_data_016_the_loader_context_carries_no_lang() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    setup
        .batch("INSERT INTO res_lang (code, active) VALUES ('fr_FR', true)")
        .await
        .unwrap();
    setup.commit().await.unwrap();
    // the control: a transaction whose environment has French active creates a probe that sees it
    let mut french = Context::new();
    french.set("lang", Value::str("fr_FR"));
    let mut tx = h.orm().begin_with(h.principal(), french).await.unwrap();
    let probes = tx.env().records("test_convert_ui.lang_probe").unwrap();
    probes
        .create_values(
            &mut tx,
            &[[("name".to_owned(), Value::str("direct"))]
                .into_iter()
                .collect()],
        )
        .await
        .unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="loaded" model="test_convert_ui.lang_probe"><field name="name">foo</field></record>
</odoo>"#,
    )
    .await
    .unwrap();
    let seen = cells(&mut tx, "test_convert_ui.lang_probe", "lang_seen").await;
    let names = cells(&mut tx, "test_convert_ui.lang_probe", "name").await;
    let by_name: HashMap<String, Cell> = names
        .iter()
        .map(text_of)
        .zip(seen.iter().cloned())
        .collect();
    assert_eq!(by_name["direct"], s("fr_FR"));
    assert_eq!(by_name["foo"], s("-"));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The content of the files a window's records read counts toward the batch's byte limit: with
/// 1 KiB files and a 2.5 KiB limit the records are written in more than one batch, with the
/// default limit in one; and `type="file"` only checks that the file exists.
#[tokio::test]
async fn file_content_counts_toward_the_batch_byte_limit() {
    let h = harness().await;
    let mut inserts = Vec::new();
    for limit in [None, Some(2_500usize)] {
        let mut tx = h.begin().await.unwrap();
        let files = Files::of(&[("m", "static/big.txt", "z".repeat(1024).as_bytes())]);
        let mut session = session_with(files);
        if let Some(bytes) = limit {
            session.set_batch_limits(
                erp_data::load::BatchLimits::default()
                    .with_max_bytes(bytes)
                    .unwrap(),
            );
        }
        let mut xml = String::from("<odoo>");
        for i in 0..6 {
            xml.push_str(&format!(
                "<record id=\"r{i}\" model=\"test_convert.record\">\
                 <field name=\"name\" file=\"m/static/big.txt\"/></record>"
            ));
        }
        xml.push_str("</odoo>");
        let table = tx
            .registry()
            .model("test_convert.record")
            .unwrap()
            .table
            .clone();
        tx.capture_statements(true);
        init(&mut session, &mut tx, &xml).await.unwrap();
        // the row insert may sit inside a `WITH` (ordered insert with `RETURNING`)
        let wanted = format!("INSERT INTO \"{table}\"");
        let count = tx
            .captured()
            .iter()
            .filter(|sql| sql.contains(&wanted))
            .count();
        assert!(count > 0, "no insert into {table}: {:#?}", tx.captured());
        inserts.push(count);
        assert_eq!(cells(&mut tx, "test_convert.record", "name").await.len(), 6);
        tx.rollback().await.unwrap();
    }
    assert_eq!(inserts, [1, 2]);
    h.cleanup().await.unwrap();
}
