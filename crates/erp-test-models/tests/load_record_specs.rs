//! The record rules of the data loader: `noupdate` and update mode, foreign external
//! ids, unresolved references, one2many sub-records, `auto_sequence`, and the end-of-run cleanup
//! (`_process_end`). XML text through `XmlDataReader` into `LoadSession::load_file` on a harness
//! transaction; no module system.
//!
//! The file module is a real addon of the harness registry (`test_convert`, `test_convert_ui`):
//! an external id of another module must name one that is part of the install step.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_data::load::{
    DataNotice, FileContext, LoadError, LoadErrorKind, LoadSession, NoFiles, NoticeKind,
    SessionOptions,
};
use erp_data::model_data::{load_records, lookup, update_xmlids};
use erp_data::{ConvertMode, LoadRecord, XidBinding, XmlDataReader, XmlId};
use erp_sdk::{Cell, Domain, ErrorKind, RawVals, Records, Transaction, Value};
use erp_test_models::{base_users, test_convert, test_convert_ui};
use erp_testkit::Harness;

const M: &str = "test_convert";
const RECORD: &str = "test_convert.record";
const USERED: &str = "test_convert.usered";

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

async fn load_in(
    session: &mut LoadSession,
    tx: &mut Transaction,
    module: &str,
    xml: &str,
    mode: ConvertMode,
) -> Result<(), LoadError> {
    let file = FileContext::new(module, "data/f.xml", mode);
    let mut ops = XmlDataReader::new(xml, "data/f.xml", module, false);
    session.load_file(tx, &file, &mut ops).await
}

async fn init(session: &mut LoadSession, tx: &mut Transaction, xml: &str) -> Result<(), LoadError> {
    load_in(session, tx, M, xml, ConvertMode::Init).await
}

async fn update(
    session: &mut LoadSession,
    tx: &mut Transaction,
    xml: &str,
) -> Result<(), LoadError> {
    load_in(session, tx, M, xml, ConvertMode::Update).await
}

fn xid(text: &str) -> XmlId {
    XmlId::parse(text).unwrap()
}

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
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
    all(tx, model).await.cells(tx, field).await.unwrap()
}

/// The ids of a model with the given name, from the database.
async fn ids_named(tx: &mut Transaction, model: &str, name: &str) -> Vec<i64> {
    let table = model.replace('.', "_");
    tx.query(
        &format!("SELECT id FROM \"{table}\" WHERE name = $1 ORDER BY id"),
        &[&name],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.get(0))
    .collect()
}

/// What `ir.model.data` stores for `text`, straight from the database.
async fn stored(tx: &mut Transaction, text: &str) -> Option<(String, Option<i64>, bool)> {
    let env = tx.env();
    let x = xid(text);
    lookup(tx, &env, std::slice::from_ref(&x))
        .await
        .unwrap()
        .remove(&x)
        .map(|r| (r.model, r.res_id, r.noupdate))
}

fn vals(records: &Records, pairs: &[(&str, Value)]) -> RawVals {
    let owned: Vec<(String, Value)> = pairs
        .iter()
        .map(|(n, v)| ((*n).to_owned(), v.clone()))
        .collect();
    records
        .vals_from_values(owned.iter().map(|(n, v)| (n, v)))
        .unwrap()
}

/// The user edits a record's name.
async fn set_name(tx: &mut Transaction, model: &str, id: i64, name: &str) {
    let records = tx.env().records(model).unwrap();
    let edit = vals(&records, &[("name", Value::str(name))]);
    records.browse([id]).unwrap().write(tx, edit).await.unwrap();
}

/// Make everything the loader wrote so far look like an earlier operation's work: the end-of-run
/// cleanup spares rows written by the current transaction.
async fn age_external_ids(tx: &mut Transaction) {
    tx.execute(
        "UPDATE ir_model_data SET write_date = write_date - interval '1 day'",
        &[],
    )
    .await
    .unwrap();
}

macro_rules! test_env {
    ($h:ident, $tx:ident) => {
        let $h = harness().await;
        let mut $tx = $h.begin().await.unwrap();
    };
}

#[tokio::test]
async fn spec_data_003_noupdate_is_fixed_at_creation() {
    test_env!(h, tx);
    let records = tx.env().records(RECORD).unwrap();
    let flag_of_a = xid("m.cat_a");

    // created with noupdate=False, loaded in update mode with noupdate=True and new values:
    // the values are written, the flag stays False
    load_records(
        &mut tx,
        &records,
        vec![LoadRecord::new(
            Some(flag_of_a.clone()),
            vals(&records, &[("name", Value::str("A"))]),
            false,
        )],
        false,
    )
    .await
    .unwrap();
    load_records(
        &mut tx,
        &records,
        vec![LoadRecord::new(
            Some(flag_of_a.clone()),
            vals(&records, &[("name", Value::str("A2"))]),
            true,
        )],
        true,
    )
    .await
    .unwrap();
    assert_eq!(cells(&mut tx, RECORD, "name").await, [s("A2")]);
    assert!(!stored(&mut tx, "m.cat_a").await.unwrap().2);

    // created with noupdate=True, loaded in update mode with noupdate=False: nothing is written,
    // the flag stays True
    let b = xid("m.cat_b");
    load_records(
        &mut tx,
        &records,
        vec![LoadRecord::new(
            Some(b.clone()),
            vals(&records, &[("name", Value::str("B"))]),
            true,
        )],
        false,
    )
    .await
    .unwrap();
    load_records(
        &mut tx,
        &records,
        vec![LoadRecord::new(
            Some(b),
            vals(&records, &[("name", Value::str("B2"))]),
            false,
        )],
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        cells(&mut tx, RECORD, "name").await,
        [s("A2"), s("B")],
        "the noupdate row keeps its values"
    );
    assert!(stored(&mut tx, "m.cat_b").await.unwrap().2);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

const NOUPDATE_WITH_SUB: &str = r#"<odoo>
  <data noupdate="1">
    <record id="r" model="test_convert.record">
      <field name="name">NAME</field>
      <field name="usered_ids">
        <record id="s" model="test_convert.usered"><field name="name">SUB</field></record>
      </field>
    </record>
  </data>
  <record id="plain" model="test_convert.record"><field name="name">PLAIN</field></record>
</odoo>"#;

fn noupdate_file(name: &str, sub: &str, plain: &str) -> String {
    NOUPDATE_WITH_SUB
        .replace("NAME", name)
        .replace("SUB", sub)
        .replace("PLAIN", plain)
}

#[tokio::test]
async fn spec_data_004_noupdate_records_are_skipped_in_update_mode_but_kept_loaded() {
    test_env!(h, tx);
    let mut first = session();
    init(
        &mut first,
        &mut tx,
        &noupdate_file("orig", "sub-orig", "plain-orig"),
    )
    .await
    .unwrap();
    // the user edits the record and its sub-record
    let r = ids_named(&mut tx, RECORD, "orig").await[0];
    let sub = ids_named(&mut tx, USERED, "sub-orig").await[0];
    set_name(&mut tx, RECORD, r, "user").await;
    set_name(&mut tx, USERED, sub, "user-sub").await;

    // the upgrade: a new session, update mode, new values in the file
    let mut upgrade = session();
    update(
        &mut upgrade,
        &mut tx,
        &noupdate_file("new", "sub-new", "plain-new"),
    )
    .await
    .unwrap();
    assert_eq!(
        cells(&mut tx, RECORD, "name").await,
        [s("user"), s("plain-new")]
    );
    assert_eq!(cells(&mut tx, USERED, "name").await, [s("user-sub")]);
    // both external ids are in the loaded set and in the cache, so the cleanup keeps them
    for text in ["test_convert.r", "test_convert.s", "test_convert.plain"] {
        assert!(upgrade.is_loaded(&xid(text)), "{text} is loaded");
    }
    for text in ["test_convert.r", "test_convert.s"] {
        assert!(
            upgrade.cache().get(&xid(text)).is_some(),
            "{text} is cached"
        );
    }
    assert_eq!(upgrade.loaded_count(), 3);
    age_external_ids(&mut tx).await;
    upgrade.process_end(&mut tx, &[M]).await.unwrap();
    assert_eq!(ids_named(&mut tx, RECORD, "user").await, [r]);
    assert_eq!(ids_named(&mut tx, USERED, "user-sub").await, [sub]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A record without an id under `noupdate` is skipped in update mode and created in init mode
/// .
#[tokio::test]
async fn a_noupdate_record_without_an_id_is_skipped_in_update_mode_only() {
    test_env!(h, tx);
    let xml = r#"<odoo><data noupdate="1">
  <record model="test_convert.record"><field name="name">anonymous</field></record>
</data></odoo>"#;
    let mut session = session();
    update(&mut session, &mut tx, xml).await.unwrap();
    assert!(cells(&mut tx, RECORD, "name").await.is_empty());
    init(&mut session, &mut tx, xml).await.unwrap();
    assert_eq!(cells(&mut tx, RECORD, "name").await, [s("anonymous")]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_005_a_noupdate_record_the_user_deleted_is_recreated_unless_forcecreate_is_0() {
    for drop_the_external_id_too in [false, true] {
        test_env!(h, tx);
        let xml = r#"<odoo><data noupdate="1">
  <record id="again" model="test_convert.record"><field name="name">again</field></record>
  <record id="never" forcecreate="0" model="test_convert.record"><field name="name">never</field></record>
</data></odoo>"#;
        let mut first = session();
        // `never` is created at install time too (forcecreate only matters in update mode)
        init(&mut first, &mut tx, xml).await.unwrap();
        let old_again = ids_named(&mut tx, RECORD, "again").await[0];
        // the user deletes both records; the external-id rows may stay dangling or go with them
        tx.execute("DELETE FROM test_convert_record", &[])
            .await
            .unwrap();
        if drop_the_external_id_too {
            tx.execute(
                "DELETE FROM ir_model_data WHERE module = 'test_convert'",
                &[],
            )
            .await
            .unwrap();
        }
        let mut upgrade = session();
        update(&mut upgrade, &mut tx, xml).await.unwrap();

        let again = ids_named(&mut tx, RECORD, "again").await;
        assert_eq!(again.len(), 1, "the first record is recreated");
        assert_ne!(again[0], old_again, "with a new id");
        assert_eq!(
            stored(&mut tx, "test_convert.again").await.unwrap().1,
            Some(again[0]),
            "and the external id points at it"
        );
        assert!(
            ids_named(&mut tx, RECORD, "never").await.is_empty(),
            "forcecreate=0 stays absent (external ids dropped: {drop_the_external_id_too})"
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn spec_data_006_a_batch_keeps_its_order_and_mixes_updated_and_noupdate_rows() {
    test_env!(h, tx);
    let records = tx.env().records(RECORD).unwrap();
    let ids = ["m.foo", "m.bar", "m.baz"].map(xid);
    let noupdate = [false, true, false];
    let batch = |suffix: &str| -> Vec<LoadRecord> {
        ids.iter()
            .zip(noupdate)
            .zip(["Foo", "Bar", "Baz"])
            .map(|((id, nu), name)| {
                LoadRecord::new(
                    Some(id.clone()),
                    vals(&records, &[("name", Value::str(format!("{name}{suffix}")))]),
                    nu,
                )
            })
            .collect()
    };
    let created = load_records(&mut tx, &records, batch(""), false)
        .await
        .unwrap();
    let updated = load_records(&mut tx, &records, batch("X"), true)
        .await
        .unwrap();
    assert_eq!(updated.ids(), created.ids(), "results are in input order");
    assert_eq!(
        updated.cells(&mut tx, "name").await.unwrap(),
        [s("FooX"), s("Bar"), s("BazX")]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_007_an_inherits_parent_gets_the_child_external_id_with_the_parent_model() {
    for noupdate in [false, true] {
        test_env!(h, tx);
        let children = tx.env().records("test_convert.child").unwrap();
        let created = load_records(
            &mut tx,
            &children,
            vec![LoadRecord::new(
                Some(xid("t.user_foo")),
                vals(
                    &children,
                    &[("name", Value::str("Foo")), ("extra", Value::str("e"))],
                ),
                noupdate,
            )],
            false,
        )
        .await
        .unwrap();
        let parent_id = created
            .cells(&mut tx, "record_id")
            .await
            .unwrap()
            .into_iter()
            .next();
        let Some(Cell::Id(parent_id)) = parent_id else {
            panic!("the child has a parent: {parent_id:?}");
        };
        assert_eq!(
            stored(&mut tx, "t.user_foo_test_convert_record").await,
            Some((RECORD.to_owned(), Some(parent_id), noupdate)),
            "the parent has <xid>_<parent model> with the child's noupdate"
        );
        assert_eq!(
            stored(&mut tx, "t.user_foo").await.map(|r| r.2),
            Some(noupdate)
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

/// The same through a data file, so the parent's external id exists for the cleanup to spare.
#[tokio::test]
async fn an_inherits_child_in_a_data_file_gets_the_parent_external_id() {
    test_env!(h, tx);
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo><record id="c" model="test_convert.child"><field name="name">Foo</field></record></odoo>"#,
    )
    .await
    .unwrap();
    let parent = all(&mut tx, RECORD).await.ids()[0];
    assert_eq!(
        stored(&mut tx, "test_convert.c_test_convert_record").await,
        Some((RECORD.to_owned(), Some(parent), false))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_008_whitespace_in_an_external_id_and_an_id_of_another_model_are_refused() {
    // an id with a space cannot be built, so no typed path can bind it
    let problem = XmlId::parse("t.category_with space").unwrap_err();
    assert_eq!(
        problem.to_string(),
        "external ID \"t.category_with space\" contains whitespace"
    );
    // and a data file with one fails at the reader, before anything is written
    test_env!(h, tx);
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        r#"<odoo><record id="with space" model="test_convert.record"/></odoo>"#,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err.kind(), LoadErrorKind::Parse(_)),
        "{:?}",
        err.kind()
    );
    // the reader wraps the id parser's text with the place; the parser's text is what this checks
    let space = XmlId::parse("with space").unwrap_err().to_string();
    assert!(err.to_string().contains(&space), "{err}");
    assert!(cells(&mut tx, RECORD, "name").await.is_empty());

    // an external id that already names a record of another model
    let records = tx.env().records(RECORD).unwrap();
    let userers = tx.env().records(USERED).unwrap();
    let taken = xid("test_convert_ui.shared");
    let made = load_records(
        &mut tx,
        &userers,
        vec![LoadRecord::new(
            Some(taken.clone()),
            vals(&userers, &[("name", Value::str("U"))]),
            false,
        )],
        false,
    )
    .await
    .unwrap();
    let row_id = {
        let env = tx.env();
        lookup(&mut tx, &env, std::slice::from_ref(&taken))
            .await
            .unwrap()[&taken]
            .id
    };
    let err = load_records(
        &mut tx,
        &records,
        vec![LoadRecord::new(
            Some(taken.clone()),
            vals(&records, &[("name", Value::str("R"))]),
            false,
        )],
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "For external id {taken} when trying to create/update a record of model {RECORD} \
             found record of different model {USERED} ({row_id})"
        )
    );
    assert_eq!(made.len(), 1);
    // through a data file the loader names the place
    let err = init(
        &mut session,
        &mut tx,
        r#"<odoo>
<record id="test_convert_ui.shared" model="test_convert.record"/>
</odoo>"#,
    )
    .await
    .unwrap_err();
    assert_eq!(err.orm_kind(), Some(ErrorKind::Validation));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- / 011 -------------------------------------------------------------------

async fn install_foreign_target(tx: &mut Transaction) {
    let mut first = session();
    init(
        &mut first,
        tx,
        r#"<odoo><record id="cat_a" model="test_convert.usered"><field name="name">A</field></record></odoo>"#,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn spec_data_010_a_foreign_record_must_exist_unless_forcecreate_is_1() {
    for mode in [ConvertMode::Init, ConvertMode::Update] {
        test_env!(h, tx);
        install_foreign_target(&mut tx).await;
        let mut second = session();
        // the existing one is updated
        load_in(
            &mut second,
            &mut tx,
            "test_convert_ui",
            r#"<odoo><record id="test_convert.cat_a" model="test_convert.usered"><field name="name">A2</field></record></odoo>"#,
            mode,
        )
        .await
        .unwrap();
        assert_eq!(cells(&mut tx, USERED, "name").await, [s("A2")]);
        assert!(second.is_loaded(&xid("test_convert.cat_a")));

        // the missing one fails
        let err = load_in(
            &mut second,
            &mut tx,
            "test_convert_ui",
            "<odoo>\n<record id=\"test_convert.cat_z\" model=\"test_convert.usered\"/>\n</odoo>",
            mode,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err.kind(), LoadErrorKind::MissingForeignRecord { .. }),
            "{:?}",
            err.kind()
        );
        assert_eq!(
            err.to_string(),
            "while parsing data/f.xml:2, somewhere inside\n\
             <record id=\"test_convert.cat_z\" model=\"test_convert.usered\">\n\
             Cannot update missing record 'test_convert.cat_z'"
        );
        // with forcecreate="1" it is created, with no notice
        load_in(
            &mut second,
            &mut tx,
            "test_convert_ui",
            r#"<odoo><record id="test_convert.cat_z" forcecreate="1" model="test_convert.usered"><field name="name">Z</field></record></odoo>"#,
            mode,
        )
        .await
        .unwrap();
        assert_eq!(cells(&mut tx, USERED, "name").await, [s("A2"), s("Z")]);
        assert!(stored(&mut tx, "test_convert.cat_z").await.is_some());
        assert_eq!(second.take_notices(), Vec::<DataNotice>::new());
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

/// A foreign id under `noupdate` with an explicit `forcecreate="0"` is skipped, not an error
/// .
#[tokio::test]
async fn a_missing_foreign_record_with_forcecreate_0_under_noupdate_is_skipped() {
    test_env!(h, tx);
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo><data noupdate="1">
  <record id="test_convert_ui.nobody" forcecreate="0" model="test_convert.usered"/>
</data></odoo>"#,
    )
    .await
    .unwrap();
    assert!(cells(&mut tx, USERED, "name").await.is_empty());
    assert!(stored(&mut tx, "test_convert_ui.nobody").await.is_none());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_011_an_id_of_a_module_outside_the_install_step_and_a_second_dot_are_refused() {
    test_env!(h, tx);
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo>\n<record id=\"other.x\" model=\"test_convert.record\"/>\n</odoo>",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err.kind(), LoadErrorKind::UninstalledModule { .. }),
        "{:?}",
        err.kind()
    );
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:2, somewhere inside\n\
         <record id=\"other.x\" model=\"test_convert.record\">\n\
         The ID \"other.x\" refers to an uninstalled module"
    );
    assert!(stored(&mut tx, "other.x").await.is_none());

    // `a.b.c` fails in the reader with the text of the id parser
    let problem = XmlId::parse("a.b.c").unwrap_err().to_string();
    let err = init(
        &mut session,
        &mut tx,
        r#"<odoo><record id="a.b.c" model="test_convert.record"/></odoo>"#,
    )
    .await
    .unwrap_err();
    assert!(matches!(err.kind(), LoadErrorKind::Parse(_)));
    // the reader wraps the id parser's text with the place; the parser's text is what this checks
    assert!(err.to_string().contains(&problem), "{err}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_013_an_unresolved_ref_skips_a_forcecreate_0_record_and_fails_the_others() {
    test_env!(h, tx);
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="skipped" forcecreate="0" model="test_convert.record">
    <field name="name">skipped</field>
    <field name="usered_id" ref="missing"/>
  </record>
  <record id="kept" model="test_convert.record"><field name="name">kept</field></record>
</odoo>"#,
    )
    .await
    .unwrap();
    assert_eq!(cells(&mut tx, RECORD, "name").await, [s("kept")]);
    assert!(stored(&mut tx, "test_convert.skipped").await.is_none());
    assert!(!session.is_loaded(&xid("test_convert.skipped")));
    // the skip is returned, not only logged
    assert_eq!(
        session.take_notices(),
        [DataNotice::new(
            M,
            "data/f.xml",
            NoticeKind::SkippedUnresolvedRef {
                xid: Some("test_convert.skipped".to_owned()),
                field: "usered_id".to_owned(),
                reference: "test_convert.missing".to_owned(),
            }
        )]
    );

    // without forcecreate="0" the same record fails
    let err = init(
        &mut session,
        &mut tx,
        "<odoo>\n<record id=\"strict\" model=\"test_convert.record\">\n\
         <field name=\"usered_id\" ref=\"missing\"/></record>\n</odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err.kind(),
        LoadErrorKind::ExternalIdNotFound { .. }
    ));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:2, somewhere inside\n\
         <record id=\"strict\" model=\"test_convert.record\">\n\
         External ID not found in the system: test_convert.missing"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_014_sub_records_follow_their_parent_with_the_inverse_field_set() {
    test_env!(h, tx);
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="r" model="test_convert.record">
    <field name="name">parent</field>
    <field name="usered_ids">
      <record id="s1" model="test_convert.usered">
        <field name="name">first</field>
        <field name="child_ids">
          <record id="g" model="test_convert.usered"><field name="name">grandchild</field></record>
        </field>
      </record>
      <record id="s2" model="test_convert.usered"><field name="name">second</field></record>
    </field>
  </record>
</odoo>"#,
    )
    .await
    .unwrap();
    let parent = ids_named(&mut tx, RECORD, "parent").await[0];
    let first = ids_named(&mut tx, USERED, "first").await[0];
    // the inverse field of each level points at its parent
    let names = cells(&mut tx, USERED, "name").await;
    let inverse = cells(&mut tx, USERED, "record_id").await;
    let grand_parent = cells(&mut tx, USERED, "parent_id").await;
    let row = |name: &str| names.iter().position(|n| *n == s(name)).unwrap();
    assert_eq!(inverse[row("first")], Cell::Id(parent));
    assert_eq!(inverse[row("second")], Cell::Id(parent));
    assert_eq!(grand_parent[row("grandchild")], Cell::Id(first));
    // every sub-record has its own external id, loaded
    for text in ["test_convert.s1", "test_convert.s2", "test_convert.g"] {
        assert!(session.is_loaded(&xid(text)), "{text}");
        assert!(stored(&mut tx, text).await.is_some(), "{text}");
    }
    // the parent's own values never carried the sub-records
    assert_eq!(
        cells(&mut tx, RECORD, "name").await,
        [s("parent")],
        "one parent row"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A `<field>` with `<record>` elements on a field that is not one2many is refused, naming it.
#[tokio::test]
async fn sub_records_on_a_field_that_is_not_one2many_are_refused() {
    test_env!(h, tx);
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo>\n<record id=\"r\" model=\"test_convert.record\"><field name=\"name\">\
         <record model=\"test_convert.usered\"/></field></record>\n</odoo>",
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:2, somewhere inside\n\
         <record id=\"r\" model=\"test_convert.record\">\n\
         field 'name' of model 'test_convert.record' holds <record> elements but is not a \
         one2many field"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

async fn sequences(tx: &mut Transaction, model: &str) -> Vec<(Cell, Cell)> {
    let names = cells(tx, model, "name").await;
    let seq = cells(tx, model, "sequence").await;
    names.into_iter().zip(seq).collect()
}

#[tokio::test]
async fn spec_data_023_auto_sequence_restarts_for_each_top_level_record_and_numbers_its_tree() {
    test_env!(h, tx);
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo auto_sequence="1">
  <record id="a" model="test_convert.record"><field name="name">a</field></record>
  <record id="b" model="test_convert.record"><field name="name">b</field><field name="sequence">5</field></record>
  <record id="c" model="test_convert.record"><field name="name">c</field></record>
</odoo>"#,
    )
    .await
    .unwrap();
    assert_eq!(
        sequences(&mut tx, RECORD).await,
        [
            (s("a"), Cell::Int(10)),
            (s("b"), Cell::Int(5)),
            (s("c"), Cell::Int(10))
        ]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();

    // one counter numbers a record and its nested sub-records depth first, then restarts; a
    // skipped sub-record does not advance it
    test_env!(h, tx);
    let mut session = self::session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo auto_sequence="1">
  <record id="p1" model="test_convert.record">
    <field name="name">p1</field>
    <field name="usered_ids">
      <record id="skipped" forcecreate="0" model="test_convert.usered">
        <field name="name">skipped</field><field name="user_id" ref="nobody"/>
      </record>
      <record id="u1" model="test_convert.usered">
        <field name="name">u1</field>
        <field name="child_ids">
          <record id="g1" model="test_convert.usered"><field name="name">g1</field></record>
        </field>
      </record>
      <record id="u2" model="test_convert.usered"><field name="name">u2</field></record>
    </field>
  </record>
  <record id="p2" model="test_convert.record"><field name="name">p2</field></record>
</odoo>"#,
    )
    .await
    .unwrap();
    assert_eq!(
        sequences(&mut tx, RECORD).await,
        [(s("p1"), Cell::Int(10)), (s("p2"), Cell::Int(10))]
    );
    let used = sequences(&mut tx, USERED).await;
    let of = |name: &str| used.iter().find(|(n, _)| *n == s(name)).unwrap().1.clone();
    assert_eq!(
        (of("u1"), of("g1"), of("u2")),
        (Cell::Int(20), Cell::Int(30), Cell::Int(40))
    );
    assert!(used.iter().all(|(n, _)| *n != s("skipped")));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();

    // a container without the flag numbers nothing, and the flag is not inherited by nested ones
    test_env!(h, tx);
    let mut session = self::session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo auto_sequence="1"><data>
  <record id="n" model="test_convert.record"><field name="name">n</field></record>
</data></odoo>"#,
    )
    .await
    .unwrap();
    assert_eq!(sequences(&mut tx, RECORD).await, [(s("n"), Cell::Null)]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- / 028 -------------------------------------------------------------------

const V1: &str = r#"<odoo>
  <record id="a" model="test_convert.record"><field name="name">a</field></record>
  <record id="b" model="test_convert.record"><field name="name">b</field></record>
  <data noupdate="1">
    <record id="c" model="test_convert.record"><field name="name">c</field></record>
  </data>
  <record id="d" model="test_convert.record"><field name="name">d</field></record>
</odoo>"#;
const V2: &str = r#"<odoo>
  <record id="a" model="test_convert.record"><field name="name">a</field></record>
</odoo>"#;

#[tokio::test]
async fn spec_data_027_the_end_of_an_update_deletes_the_data_the_new_version_no_longer_ships() {
    test_env!(h, tx);
    let mut v1 = session();
    init(&mut v1, &mut tx, V1).await.unwrap();
    let env = tx.env();
    let b = ids_named(&mut tx, RECORD, "b").await[0];
    // `b` is also named by another module's external id
    update_xmlids(
        &mut tx,
        &env,
        &[XidBinding::new(xid("other.b"), RECORD, b, false)],
        false,
    )
    .await
    .unwrap();
    age_external_ids(&mut tx).await;

    let mut v2 = session();
    update(&mut v2, &mut tx, V2).await.unwrap();
    v2.process_end(&mut tx, &[M]).await.unwrap();

    // `c` is noupdate: kept with its external id
    assert_eq!(ids_named(&mut tx, RECORD, "c").await.len(), 1);
    assert!(stored(&mut tx, "test_convert.c").await.is_some());
    // `b` keeps its record for `other.b`; only `m.b` is gone
    assert_eq!(ids_named(&mut tx, RECORD, "b").await, [b]);
    assert!(stored(&mut tx, "test_convert.b").await.is_none());
    assert_eq!(stored(&mut tx, "other.b").await.unwrap().1, Some(b));
    // `d` had one external id: record and id are gone; `a` is loaded and stays
    assert!(ids_named(&mut tx, RECORD, "d").await.is_empty());
    assert!(stored(&mut tx, "test_convert.d").await.is_none());
    assert_eq!(ids_named(&mut tx, RECORD, "a").await.len(), 1);
    assert!(stored(&mut tx, "test_convert.a").await.is_some());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_028_the_end_of_an_update_keeps_inherits_parents_of_loaded_children() {
    let child = |name: &str| {
        format!(
            r#"<odoo><record id="u" model="test_convert.child"><field name="name">{name}</field></record></odoo>"#
        )
    };
    // the child is in the new version: the parent (external id `test_convert.u_test_convert_record`,
    // which no file names) and its external id stay
    test_env!(h, tx);
    let mut v1 = session();
    init(&mut v1, &mut tx, &child("one")).await.unwrap();
    age_external_ids(&mut tx).await;
    let parent = all(&mut tx, RECORD).await.ids()[0];
    let mut v2 = session();
    update(&mut v2, &mut tx, &child("two")).await.unwrap();
    v2.process_end(&mut tx, &[M]).await.unwrap();
    assert_eq!(all(&mut tx, RECORD).await.ids(), [parent]);
    assert_eq!(
        stored(&mut tx, "test_convert.u_test_convert_record").await,
        Some((RECORD.to_owned(), Some(parent), false))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();

    // the child left the new version: both external ids are obsolete and both records go
    test_env!(h, tx);
    let mut v1 = self::session();
    init(&mut v1, &mut tx, &child("one")).await.unwrap();
    age_external_ids(&mut tx).await;
    let mut v2 = self::session();
    update(&mut v2, &mut tx, "<odoo/>").await.unwrap();
    v2.process_end(&mut tx, &[M]).await.unwrap();
    assert!(all(&mut tx, RECORD).await.ids().is_empty());
    assert!(all(&mut tx, "test_convert.child").await.ids().is_empty());
    assert!(stored(&mut tx, "test_convert.u").await.is_none());
    assert!(
        stored(&mut tx, "test_convert.u_test_convert_record")
            .await
            .is_none()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// An external id without a `write_date` is judged by the loaded set like any other: the cleanup
/// must not filter on `write_date < now`, which skips a NULL (`process_end`).
#[tokio::test]
async fn an_obsolete_external_id_without_write_date_is_still_removed() {
    test_env!(h, tx);
    let mut v1 = session();
    init(&mut v1, &mut tx, V1).await.unwrap();
    age_external_ids(&mut tx).await;
    tx.execute(
        "UPDATE ir_model_data SET write_date = NULL WHERE module = 'test_convert' AND name = 'd'",
        &[],
    )
    .await
    .unwrap();
    let mut v2 = session();
    update(&mut v2, &mut tx, V2).await.unwrap();
    v2.process_end(&mut tx, &[M]).await.unwrap();
    assert!(ids_named(&mut tx, RECORD, "d").await.is_empty());
    assert!(stored(&mut tx, "test_convert.d").await.is_none());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// An external id written by the current transaction (a hook, a data-callable method) is never
/// obsolete, whether or not a file names it.
#[tokio::test]
async fn an_external_id_written_in_this_transaction_is_not_obsolete() {
    test_env!(h, tx);
    let mut v1 = session();
    init(&mut v1, &mut tx, V1).await.unwrap();
    age_external_ids(&mut tx).await;
    let mut v2 = session();
    update(&mut v2, &mut tx, V2).await.unwrap();
    // code outside the loader re-points `d` to a fresh record during the operation
    let records = tx.env().records(RECORD).unwrap();
    let fresh = records
        .create(
            &mut tx,
            vec![vals(&records, &[("name", Value::str("fresh"))])],
        )
        .await
        .unwrap()
        .ids()[0];
    let env = tx.env();
    update_xmlids(
        &mut tx,
        &env,
        &[XidBinding::new(xid("test_convert.d"), RECORD, fresh, false)],
        false,
    )
    .await
    .unwrap();
    v2.process_end(&mut tx, &[M]).await.unwrap();
    assert_eq!(ids_named(&mut tx, RECORD, "fresh").await, [fresh]);
    assert_eq!(
        stored(&mut tx, "test_convert.d").await.unwrap().1,
        Some(fresh)
    );
    // while `b` and its record are removed as before
    assert!(ids_named(&mut tx, RECORD, "b").await.is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// External ids of models the registry does not have are left alone.
#[tokio::test]
async fn external_ids_of_unknown_models_are_left_alone() {
    test_env!(h, tx);
    tx.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         VALUES ('test_convert', 'ghost', 'no.such.model', 7, false)",
        &[],
    )
    .await
    .unwrap();
    age_external_ids(&mut tx).await;
    let mut session = session();
    session.process_end(&mut tx, &[M]).await.unwrap();
    assert_eq!(
        stored(&mut tx, "test_convert.ghost").await,
        Some(("no.such.model".to_owned(), Some(7), false))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The same external id twice in one file: the second record updates the first (the window is
/// written before a record whose own id is pending).
#[tokio::test]
async fn the_same_external_id_twice_in_a_file_updates_the_first_record() {
    test_env!(h, tx);
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="dup" model="test_convert.record"><field name="name">one</field><field name="count">1</field></record>
  <record id="dup" model="test_convert.record"><field name="name">two</field></record>
</odoo>"#,
    )
    .await
    .unwrap();
    assert_eq!(cells(&mut tx, RECORD, "name").await, [s("two")]);
    assert_eq!(cells(&mut tx, RECORD, "count").await, [Cell::Int(1)]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A `ref()` whose name is computed reads the database while the record is evaluated, so the
/// records waiting before it are written first.
#[tokio::test]
async fn a_ref_with_a_computed_name_sees_the_records_loaded_before_it() {
    test_env!(h, tx);
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo>
  <record id="u1" model="test_convert.usered"><field name="name">target</field></record>
  <record id="r" model="test_convert.record">
    <field name="name">r</field>
    <field name="usered_id" eval="ref('u' + '1')"/>
  </record>
</odoo>"#,
    )
    .await
    .unwrap();
    let u1 = all(&mut tx, USERED).await.ids()[0];
    assert_eq!(cells(&mut tx, RECORD, "usered_id").await, [Cell::Id(u1)]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A failure of the write belongs to the whole batch: it is reported at the batch's first record
/// and says how many records were written together.
#[tokio::test]
async fn a_failing_batch_is_reported_at_its_first_record_naming_the_batch() {
    test_env!(h, tx);
    let mut session = session();
    init(
        &mut session,
        &mut tx,
        r#"<odoo><record id="taken" model="test_convert.usered"/></odoo>"#,
    )
    .await
    .unwrap();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo>\n\
         <record id=\"r1\" model=\"test_convert.record\"/>\n\
         <record id=\"taken\" model=\"test_convert.record\"/>\n\
         <record id=\"r3\" model=\"test_convert.record\"/>\n\
         </odoo>",
    )
    .await
    .unwrap_err();
    assert_eq!(err.orm_kind(), Some(ErrorKind::Validation));
    let row_id = {
        let env = tx.env();
        let taken = xid("test_convert.taken");
        lookup(&mut tx, &env, std::slice::from_ref(&taken))
            .await
            .unwrap()[&taken]
            .id
    };
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:2, somewhere inside\n\
         <record id=\"r1\" model=\"test_convert.record\">\n\
         (one of 3 <record> elements of model 'test_convert.record', lines 2 to 4, written \
         as one batch)"
    );
    let source = std::error::Error::source(&err).unwrap().to_string();
    assert_eq!(
        source,
        format!(
            "For external id test_convert.taken when trying to create/update a record of model \
             {RECORD} found record of different model {USERED} ({row_id})"
        )
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
