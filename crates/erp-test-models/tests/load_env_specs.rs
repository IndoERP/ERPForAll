//! The load session on a harness transaction: records, `uid` and `context`,
//! the errors of the minimal record path, and the stubs the later sub-slices
//! replace. No module system: XML text through `XmlDataReader` into `LoadSession::load_file`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_data::load::{
    FileContext, LoadError, LoadErrorKind, LoadSession, NoFiles, NoticeKind, SessionOptions,
};
use erp_data::model_data::{lookup, update_xmlids};
use erp_data::{ConvertMode, XidBinding, XmlDataReader, XmlId};
use erp_sdk::{Cell, Domain, ErrorKind, Records, Transaction, Value};
use erp_test_models::{base_users, test_convert, test_convert_ui};
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_convert::ADDON,
        &test_convert_ui::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

fn session() -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)))
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

/// The record of `x` and the error text of a failed load, for the error tests.
#[track_caller]
fn kind_and_text(err: &LoadError) -> (&LoadErrorKind, String) {
    (err.kind(), err.to_string())
}

// ---- records -----------------------------------------------------------------------------

#[tokio::test]
async fn record_values_are_coerced_by_field_type_and_the_xid_is_bound() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="u1" model="test_convert.usered"><field name="name">U</field></record>
  <record id="rec" model="test_convert.record">
    <field name="name">A &amp; B</field>
    <field name="count">3</field>
    <field name="ratio" eval="1.5"/>
    <field name="flag">off</field>
    <field name="usered_id" ref="u1"/>
    <field name="target" ref="u1"/>
  </record>
</odoo>"#,
    )
    .await
    .unwrap();
    let env = tx.env();
    let ids = all(&mut tx, "test_convert.usered").await.ids();
    let xids = lookup(
        &mut tx,
        &env,
        &[
            XmlId::parse("m.rec").unwrap(),
            XmlId::parse("m.u1").unwrap(),
        ],
    )
    .await
    .unwrap();
    assert_eq!(xids.len(), 2);
    assert_eq!(xids[&XmlId::parse("m.u1").unwrap()].res_id, Some(ids[0]));
    assert!(!xids[&XmlId::parse("m.rec").unwrap()].noupdate);
    assert_eq!(
        cells(&mut tx, "test_convert.record", "name").await,
        [s("A & B")]
    );
    assert_eq!(
        cells(&mut tx, "test_convert.record", "count").await,
        [Cell::Int(3)]
    );
    assert_eq!(
        cells(&mut tx, "test_convert.record", "ratio").await,
        [Cell::Float(1.5)]
    );
    assert_eq!(
        cells(&mut tx, "test_convert.record", "flag").await,
        [Cell::Bool(false)]
    );
    assert_eq!(
        cells(&mut tx, "test_convert.record", "usered_id").await,
        [Cell::Id(ids[0])]
    );
    // a reference field takes `model,id`
    assert_eq!(
        cells(&mut tx, "test_convert.record", "target").await,
        [s(&format!("test_convert.usered,{}", ids[0]))]
    );
    assert!(session.is_loaded(&XmlId::parse("m.rec").unwrap()));
    assert!(session.is_loaded(&XmlId::parse("m.u1").unwrap()));
    assert_eq!(session.loaded_count(), 2);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_reference_to_a_record_of_the_same_session_costs_no_external_id_lookup() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    tx.capture_statements(true);
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="u1" model="test_convert.usered"><field name="name">U</field></record>
  <record id="rec" model="test_convert.record"><field name="usered_id" ref="u1"/></record>
</odoo>"#,
    )
    .await
    .unwrap();
    let plain_lookups = tx
        .captured()
        .iter()
        .filter(|sql| sql.contains("FROM \"ir_model_data\" WHERE (module, name) IN"))
        .count();
    assert_eq!(plain_lookups, 0, "{:?}", tx.captured());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_xmlid_field_gets_the_external_id_unless_the_data_gives_one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="a" model="test_convert.record"><field name="name">a</field></record>
  <record id="b" model="test_convert.record"><field name="name">b</field><field name="xmlid">mine</field></record>
</odoo>"#,
    )
    .await
    .unwrap();
    assert_eq!(
        cells(&mut tx, "test_convert.record", "xmlid").await,
        [s("m.a"), s("mine")]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn mark_and_rollback_forget_the_loaded_ids_and_a_foreign_mark_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let rec = |id: &str| format!(r#"<odoo><record id="{id}" model="test_convert.record"/></odoo>"#);
    init(&mut session, &mut tx, &rec("kept")).await.unwrap();
    let mark = session.mark();
    let result = tx
        .savepoint(async |tx| {
            init(&mut session, tx, &rec("lost")).await.unwrap();
            Err::<(), _>(erp_sdk::Error::programming("roll back"))
        })
        .await;
    assert_eq!(
        result.unwrap_err().message(),
        "roll back",
        "the savepoint failed with the closure's own error"
    );
    session.rollback_to(mark).unwrap();
    assert!(session.is_loaded(&XmlId::parse("m.kept").unwrap()));
    assert!(!session.is_loaded(&XmlId::parse("m.lost").unwrap()));
    assert!(session.cache().is_empty());

    let mut other = self::session();
    let err = session.rollback_to(other.mark()).unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "a session mark from another load session was given to rollback_to"
        )
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A mark is refused when it is stale (the session already went back to it or to an earlier
/// mark) or when `take_notices` handed out notices it covers; nothing is changed by the refusal.
#[tokio::test]
async fn a_stale_mark_and_a_mark_after_take_notices_are_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let rec = |id: &str| format!(r#"<odoo><record id="{id}" model="test_convert.record"/></odoo>"#);
    init(&mut session, &mut tx, &rec("kept")).await.unwrap();

    // a mark taken after the point the session went back to is stale
    let first = session.mark();
    init(&mut session, &mut tx, &rec("a")).await.unwrap();
    let second = session.mark();
    session.rollback_to(first).unwrap();
    let err = session.rollback_to(second).unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "a stale session mark was given to rollback_to: the session already went back to it \
             or to an earlier mark"
        )
    );
    // so is the mark that was used
    assert_eq!(
        session.rollback_to(first).unwrap_err().kind(),
        ErrorKind::Programming
    );
    assert!(session.is_loaded(&XmlId::parse("m.kept").unwrap()));

    // notices: one before the mark, one after; handing the first out before the rollback is refused
    let delete =
        |id: &str| format!(r#"<odoo><delete model="test_convert.record" id="{id}"/></odoo>"#);
    init(&mut session, &mut tx, &delete("before"))
        .await
        .unwrap();
    let mark = session.mark();
    init(&mut session, &mut tx, &delete("after")).await.unwrap();
    session.rollback_to(mark).unwrap();
    let notices = session.take_notices();
    assert_eq!(
        notices.len(),
        1,
        "only the notice from before the mark survives: {notices:?}"
    );
    assert_eq!(
        notices[0].kind(),
        &NoticeKind::DeleteTargetMissing {
            xid: "m.before".to_owned()
        }
    );
    let covered = session.mark();
    init(&mut session, &mut tx, &delete("late")).await.unwrap();
    // the notice after the mark is handed out before the rollback
    assert_eq!(session.take_notices().len(), 1);
    let err = session.rollback_to(covered).unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "a session mark was given to rollback_to after take_notices handed out notices it \
             covers; take the notices after the savepoint is settled"
        )
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_017_uid_and_context_chain_outer_to_inner() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    // base.user_root is the superuser (id 1); base.user_admin a second user
    let admin = env
        .records("res.users")
        .unwrap()
        .create_values(
            &mut tx,
            &[[
                ("name".to_owned(), Value::str("Admin")),
                ("login".to_owned(), Value::str("admin")),
            ]
            .into_iter()
            .collect()],
        )
        .await
        .unwrap()
        .ids()[0];
    update_xmlids(
        &mut tx,
        &env,
        &[
            XidBinding::new(
                XmlId::parse("base.user_root").unwrap(),
                "res.users",
                1,
                true,
            ),
            XidBinding::new(
                XmlId::parse("base.user_admin").unwrap(),
                "res.users",
                admin,
                true,
            ),
        ],
        false,
    )
    .await
    .unwrap();
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo uid="base.user_root" context="{'default_tz': 'A', 'default_note': 'outer'}">
  <record id="rec" model="test_convert.record"><field name="name">target</field></record>
  <record id="outer_only" model="test_convert.usered"><field name="name">outer</field></record>
  <record id="inner" model="test_convert.usered" uid="base.user_admin" context="{'default_tz': 'B', 'default_record_id': ref('rec')}">
    <field name="name">inner</field>
  </record>
</odoo>"#,
    )
    .await
    .unwrap();
    let outer = 0;
    let inner = 1;
    let ids = all(&mut tx, "test_convert.usered").await.ids();
    // without inner attributes the outer ones apply
    let users = cells(&mut tx, "test_convert.usered", "create_uid").await;
    assert_eq!(users, [Cell::Id(1), Cell::Id(admin)]);
    assert_eq!(
        cells(&mut tx, "test_convert.usered", "user_id").await,
        [Cell::Id(1), Cell::Id(admin)]
    );
    let tz = cells(&mut tx, "test_convert.usered", "tz").await;
    assert_eq!((tz[outer].clone(), tz[inner].clone()), (s("A"), s("B")));
    // contexts merge key by key: the inner record keeps the outer `default_note`
    let note = cells(&mut tx, "test_convert.usered", "note").await;
    assert_eq!(
        (note[outer].clone(), note[inner].clone()),
        (s("outer"), s("outer"))
    );
    // `ref()` works inside a context expression
    let record_id = cells(&mut tx, "test_convert.usered", "record_id").await;
    let rec_id = all(&mut tx, "test_convert.record").await.ids()[0];
    assert_eq!(
        (record_id[outer].clone(), record_id[inner].clone()),
        (Cell::Null, Cell::Id(rec_id))
    );
    assert_eq!(ids.len(), 2);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- errors ------------------------------------------------------------------------------

async fn failing(xml: &str) -> (LoadErrorKindText, bool) {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(&mut session, &mut tx, xml).await.unwrap_err();
    let out = (
        LoadErrorKindText {
            text: err.to_string(),
            kind_debug: format!("{:?}", kind_and_text(&err).0),
            orm: err.orm_kind(),
        },
        true,
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
    out
}

struct LoadErrorKindText {
    text: String,
    kind_debug: String,
    orm: Option<ErrorKind>,
}

#[tokio::test]
async fn an_unknown_model_is_a_model_missing_error_with_file_line_and_tag() {
    let (e, _) = failing("<odoo>\n<record id=\"r\" model=\"no.such\"/>\n</odoo>").await;
    assert!(e.kind_debug.starts_with("ModelMissing"), "{}", e.kind_debug);
    assert_eq!(
        e.text,
        "while parsing data/f.xml:2, somewhere inside\n<record id=\"r\" model=\"no.such\">\n\
         <record> model 'no.such' is not in the registry of this install step"
    );
}

#[tokio::test]
async fn an_unresolved_ref_is_an_external_id_error() {
    let (e, _) = failing(
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"usered_id\" ref=\"nobody\"/></record></odoo>",
    )
    .await;
    assert!(
        e.kind_debug.starts_with("ExternalIdNotFound"),
        "{}",
        e.kind_debug
    );
    assert_eq!(
        e.text,
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">\n\
         External ID not found in the system: m.nobody"
    );
}

#[tokio::test]
async fn a_text_that_is_not_a_number_is_a_type_mismatch() {
    let (e, _) = failing(
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"count\">abc</field></record></odoo>",
    )
    .await;
    assert!(e.kind_debug.starts_with("TypeMismatch"), "{}", e.kind_debug);
    assert_eq!(
        e.text,
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">\n\
         field 'count' of model 'test_convert.record' (integer) cannot take the text \"abc\""
    );
}

#[tokio::test]
async fn an_expression_error_names_the_expression_and_the_python_class() {
    let (e, _) = failing(
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"count\" eval=\"1 / 0\"/></record></odoo>",
    )
    .await;
    assert!(e.kind_debug.starts_with("Eval"), "{}", e.kind_debug);
    assert_eq!(
        e.text,
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">\n\
         eval \"1 / 0\" failed: ZeroDivisionError: division by zero"
    );
}

#[tokio::test]
async fn an_expression_value_that_is_not_a_field_value_is_refused() {
    let (e, _) = failing(
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"name\" eval=\"timedelta(days=1)\"/></record></odoo>",
    )
    .await;
    assert!(
        e.kind_debug.starts_with("NotAFieldValue"),
        "{}",
        e.kind_debug
    );
    assert_eq!(
        e.text,
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">\n\
         field 'name' of model 'test_convert.record': an expression produced a datetime.timedelta, \
         which is not a field value"
    );
}

#[tokio::test]
async fn a_uid_that_is_not_a_user_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let recs = env
        .records("test_convert.record")
        .unwrap()
        .create_values(&mut tx, &[Default::default()])
        .await
        .unwrap();
    update_xmlids(
        &mut tx,
        &env,
        &[XidBinding::new(
            XmlId::parse("m.not_a_user").unwrap(),
            "test_convert.record",
            recs.ids()[0],
            false,
        )],
        false,
    )
    .await
    .unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo uid=\"not_a_user\"><record id=\"r\" model=\"test_convert.record\"/></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(err.kind(), LoadErrorKind::NotAUser { .. }));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">\n\
         uid \"m.not_a_user\" is a record of model 'test_convert.record', not of 'res.users'"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_unknown_field_is_the_orms_error_and_keeps_its_kind() {
    let (e, _) = failing(
        "<odoo><record id=\"r\" model=\"test_convert.record\">\
         <field name=\"nope\">1</field></record></odoo>",
    )
    .await;
    assert!(e.kind_debug.starts_with("Orm"), "{}", e.kind_debug);
    assert_eq!(e.orm, Some(ErrorKind::Programming));
    assert_eq!(
        e.text,
        "while parsing data/f.xml:1, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">"
    );
}

#[tokio::test]
async fn a_parse_error_in_the_stream_is_a_parse_kind_error() {
    let (e, _) = failing("<odoo><record id=\"r\" model=\"test_convert.record\"></odoo>").await;
    assert!(e.kind_debug.starts_with("Parse"), "{}", e.kind_debug);
}

// ---- stubs the later sub-slices replace ---------------------------------------------------

async fn unsupported(xml: &str, mode: ConvertMode) -> String {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = load(&mut session, &mut tx, xml, mode).await.unwrap_err();
    assert!(
        matches!(err.kind(), LoadErrorKind::Unsupported { .. }),
        "{err:?}"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
    err.to_string()
}

fn at1(tag: &str, what: &str, slice: &str) -> String {
    format!(
        "while parsing data/f.xml:1, somewhere inside\n{tag}\n{what} is not supported until {slice}"
    )
}

#[tokio::test]
async fn asset_names_the_slice_that_adds_it() {
    // no slice names `ir.asset` before the web client's asset bundles
    assert_eq!(
        unsupported(
            "<odoo><asset id=\"as\" name=\"A\"><bundle>web.x</bundle><path>a/b.js</path></asset></odoo>",
            ConvertMode::Init
        )
        .await,
        at1(
            "<asset id=\"as\">",
            "<asset>",
            "M4 (web client asset bundles)"
        )
    );
}
