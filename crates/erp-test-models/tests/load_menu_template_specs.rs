//! `<menuitem>` and `<template>` through the data loader: on the
//! the real `ir.ui.menu`, `ir.actions.*`, `ir.ui.view` and `res.groups` of addon `base`.
//! XML text through `XmlDataReader` into `LoadSession`, no module system.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::error::Error as _;
use std::sync::Arc;

use erp_data::load::{FileContext, LoadError, LoadErrorKind, LoadSession, NoFiles, SessionOptions};
use erp_data::model_data::lookup;
use erp_data::{ConvertMode, XmlDataReader, XmlId};
use erp_sdk::{Cell, Domain, Records, Transaction, Value};
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .await
        .expect("harness")
}

/// A transaction on a database that has none of base's data: the group rules need the
/// regular-user marker group unless base is installing, which is what the loader sets for its
/// own install.
async fn begin(h: &Harness) -> Transaction {
    let mut tx = h.begin().await.unwrap();
    erp_testkit::base_install::set_base_installing(&mut tx, true).unwrap();
    tx
}

fn session() -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)))
}

async fn load_as(
    module: &str,
    session: &mut LoadSession,
    tx: &mut Transaction,
    xml: &str,
    mode: ConvertMode,
) -> Result<(), LoadError> {
    let file = FileContext::new(module, "data/f.xml", mode);
    let mut ops = XmlDataReader::new(xml, "data/f.xml", module, false);
    session.load_file(tx, &file, &mut ops).await
}

async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    xml: &str,
    mode: ConvertMode,
) -> Result<(), LoadError> {
    load_as("m", session, tx, xml, mode).await
}

async fn init(session: &mut LoadSession, tx: &mut Transaction, xml: &str) -> Result<(), LoadError> {
    load(session, tx, xml, ConvertMode::Init).await
}

async fn xid_id(tx: &mut Transaction, xid: &str) -> i64 {
    let env = tx.env();
    let key = XmlId::parse(xid).unwrap();
    lookup(tx, &env, std::slice::from_ref(&key)).await.unwrap()[&key]
        .res_id
        .unwrap()
}

/// The cell of `field` on the record named by `xid`.
async fn cell(tx: &mut Transaction, model: &str, xid: &str, field: &str) -> Cell {
    let id = xid_id(tx, xid).await;
    let records: Records = tx.env().records(model).unwrap().browse([id]).unwrap();
    records.cells(tx, field).await.unwrap().remove(0)
}

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

/// Groups and actions the menus point at.
const PARTS: &str = r#"<odoo>
  <record id="g_user" model="res.groups"><field name="name">User</field></record>
  <record id="g_portal" model="res.groups"><field name="name">Portal</field></record>
  <record id="act" model="ir.actions.act_window"><field name="name">Partners</field><field name="res_model">res.partner</field></record>
  <record id="site" model="ir.actions.act_url"><field name="name">Website</field><field name="url">/web</field></record>
</odoo>"#;

async fn group_ids(tx: &mut Transaction, menu_xid: &str) -> Vec<i64> {
    let id = xid_id(tx, menu_xid).await;
    let menus = tx
        .env()
        .records("ir.ui.menu")
        .unwrap()
        .browse([id])
        .unwrap();
    let Value::List(items) = menus.value(tx, "group_ids").await.unwrap() else {
        panic!("a many2many reads as a list");
    };
    let mut ids: Vec<i64> = items
        .iter()
        .map(|v| match v {
            Value::Int(id) => *id,
            other => panic!("expected an id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn spec_data_021_menuitem_rules() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let mut session = session();
    init(&mut session, &mut tx, PARTS).await.unwrap();
    // the child starts in the portal group; the upgrade links user and unlinks portal
    let tree = |groups: &str| {
        format!(
            r#"<odoo>
  <menuitem id="root" name="Root" web_icon="m,static/i.png" sequence="4">
    <menuitem id="child" action="act" groups="{groups}"/>
    <menuitem id="plain"/>
    <menuitem id="hidden" name="Hidden" active="0" action="site"/>
  </menuitem>
  <menuitem id="elsewhere" parent="root" name="Elsewhere"/>
</odoo>"#
        )
    };
    init(&mut session, &mut tx, &tree("g_portal"))
        .await
        .unwrap();
    let g_user = xid_id(&mut tx, "m.g_user").await;
    let g_portal = xid_id(&mut tx, "m.g_portal").await;
    assert_eq!(group_ids(&mut tx, "m.child").await, [g_portal]);

    load(
        &mut session,
        &mut tx,
        &tree("g_user,-g_portal"),
        ConvertMode::Update,
    )
    .await
    .unwrap();
    let root = xid_id(&mut tx, "m.root").await;
    let act = xid_id(&mut tx, "m.act").await;
    let site = xid_id(&mut tx, "m.site").await;
    let m = "ir.ui.menu";
    // nesting and the parent attribute both set parent_id; a top-level menu has none
    assert_eq!(
        cell(&mut tx, m, "m.child", "parent_id").await,
        Cell::Id(root)
    );
    assert_eq!(
        cell(&mut tx, m, "m.elsewhere", "parent_id").await,
        Cell::Id(root)
    );
    assert_eq!(cell(&mut tx, m, "m.root", "parent_id").await, Cell::Null);
    // the child takes its name from the action, and the action as `model,id`
    let action_name = cell(&mut tx, "ir.actions.act_window", "m.act", "name").await;
    assert_eq!(cell(&mut tx, m, "m.child", "name").await, action_name);
    assert_eq!(
        cell(&mut tx, m, "m.child", "action").await,
        s(&format!("ir.actions.act_window,{act}"))
    );
    assert_eq!(
        cell(&mut tx, m, "m.hidden", "action").await,
        s(&format!("ir.actions.act_url,{site}"))
    );
    // an explicit name wins over the action's
    assert_eq!(cell(&mut tx, m, "m.hidden", "name").await, s("Hidden"));
    // user linked, portal unlinked
    assert_eq!(group_ids(&mut tx, "m.child").await, [g_user]);
    // a root menu keeps its icon and sequence; a menu without name and action is named by its id
    assert_eq!(
        cell(&mut tx, m, "m.root", "web_icon").await,
        s("m,static/i.png")
    );
    assert_eq!(cell(&mut tx, m, "m.root", "sequence").await, Cell::Int(4));
    assert_eq!(cell(&mut tx, m, "m.plain", "name").await, s("plain"));
    assert_eq!(cell(&mut tx, m, "m.plain", "web_icon").await, Cell::Null);
    // active defaults to true and `active="0"` is false
    assert_eq!(
        cell(&mut tx, m, "m.plain", "active").await,
        Cell::Bool(true)
    );
    assert_eq!(
        cell(&mut tx, m, "m.hidden", "active").await,
        Cell::Bool(false)
    );
    // every menu counts as loaded for the end-of-run cleanup
    for name in ["root", "child", "plain", "hidden", "elsewhere"] {
        assert!(
            session.is_loaded(&XmlId::parse(&format!("m.{name}")).unwrap()),
            "{name}"
        );
    }
    // no duplicates came from the second load
    let all = tx
        .env()
        .records(m)
        .unwrap()
        .with_context([("active_test", Value::Bool(false))])
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The statements a tree of `width` children with `width` grandchildren each costs.
async fn statements_for_tree(width: usize, tag: &str) -> usize {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let mut session = session();
    init(&mut session, &mut tx, PARTS).await.unwrap();
    let mut xml = format!("<odoo><menuitem id=\"{tag}_root\" name=\"Root\">");
    for c in 0..width {
        xml.push_str(&format!("<menuitem id=\"{tag}_c{c}\" name=\"C\">"));
        for g in 0..width {
            xml.push_str(&format!(
                "<menuitem id=\"{tag}_c{c}_g{g}\" action=\"act\"/>"
            ));
        }
        xml.push_str("</menuitem>");
    }
    xml.push_str("</menuitem></odoo>");
    tx.capture_statements(true);
    init(&mut session, &mut tx, &xml).await.unwrap();
    let count = tx.captured().len();
    let inserts = tx
        .captured()
        .iter()
        .filter(|sql| sql.contains("INSERT INTO \"ir_ui_menu\""))
        .count();
    // one insert per level of the tree: the menus, then their children, then theirs
    assert_eq!(inserts, 3, "{:?}", tx.captured());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
    count
}

/// Part's cost: a menu tree takes a number of statements per level, not per menu.
#[tokio::test]
async fn a_menu_tree_costs_statements_per_level_not_per_menu() {
    let small = statements_for_tree(2, "s").await;
    let large = statements_for_tree(12, "l").await;
    assert_eq!(small, large);
}

#[tokio::test]
async fn a_menu_with_an_unknown_action_group_or_parent_is_an_external_id_error() {
    for (xml, missing) in [
        ("<menuitem id=\"x\" action=\"nobody\"/>", "m.nobody"),
        (
            "<menuitem id=\"x\" name=\"X\" groups=\"nobody\"/>",
            "m.nobody",
        ),
        (
            "<menuitem id=\"x\" name=\"X\" parent=\"nobody\"/>",
            "m.nobody",
        ),
        (
            "<menuitem id=\"x\" name=\"X\"><menuitem id=\"y\" action=\"nobody\"/></menuitem>",
            "m.nobody",
        ),
    ] {
        let h = harness().await;
        let mut tx = begin(&h).await;
        let mut session = session();
        let err = init(&mut session, &mut tx, &format!("<odoo>{xml}</odoo>"))
            .await
            .unwrap_err();
        assert!(
            matches!(err.kind(), LoadErrorKind::ExternalIdNotFound { .. }),
            "{xml}: {err:?}"
        );
        let tag = if xml.contains("id=\"y\"") { "y" } else { "x" };
        assert_eq!(
            err.to_string(),
            format!(
                "while parsing data/f.xml:1, somewhere inside\n<menuitem id=\"{tag}\">\n\
                 External ID not found in the system: {missing}"
            )
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn an_action_whose_record_was_deleted_is_refused() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let mut session = session();
    init(&mut session, &mut tx, PARTS).await.unwrap();
    let act = xid_id(&mut tx, "m.act").await;
    let table = tx
        .registry()
        .model("ir.actions.act_window")
        .unwrap()
        .table
        .clone();
    tx.flush().await.unwrap();
    tx.query(&format!("DELETE FROM \"{table}\" WHERE id = $1"), &[&act])
        .await
        .unwrap();
    tx.invalidate_model("ir.actions.act_window", None)
        .await
        .unwrap();
    let mut fresh = self::session();
    let err = init(
        &mut fresh,
        &mut tx,
        "<odoo><menuitem id=\"x\" action=\"act\"/></odoo>",
    )
    .await
    .unwrap_err();
    assert_eq!(err.orm_kind(), Some(erp_sdk::ErrorKind::Missing));
    // the ORM error is the source and prints its own message once
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n<menuitem id=\"x\">"
    );
    assert_eq!(
        err.source().unwrap().to_string(),
        "No record found for unique ID m.act. It may have been deleted."
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// t3 is a primary view over t1: the real `ir.ui.view` checks that its root element can be located in the
// parent arch, so it repeats t1's `<div>` root.
const TEMPLATES: &str = r#"<odoo>
  <template id="t1" name="T"><div>Hello</div></template>
  <template id="t2" inherit_id="m.t1" priority="20"><xpath expr="." position="inside"><span>Ext</span></xpath></template>
  <template id="t3" inherit_id="m.t1" primary="True"><div>Primary</div></template>
  <template id="t4" active="False"><b>Inactive</b></template>
</odoo>"#;

#[tokio::test]
async fn spec_data_022_template_transformation() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let mut session = session();
    init(&mut session, &mut tx, TEMPLATES).await.unwrap();
    let v = "ir.ui.view";
    let views = tx
        .env()
        .records(v)
        .unwrap()
        .with_context([("active_test", Value::Bool(false))])
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(views.len(), 4);
    for name in ["t1", "t2", "t3", "t4"] {
        let xid = format!("m.{name}");
        assert_eq!(cell(&mut tx, v, &xid, "type").await, s("qweb"), "{name}");
        assert_eq!(cell(&mut tx, v, &xid, "key").await, s(&xid), "{name}");
    }
    // t1: the arch root is `<t t-name="m.t1">`
    let Cell::Str(arch) = cell(&mut tx, v, "m.t1", "arch").await else {
        panic!("arch is text");
    };
    let root = arch
        .strip_prefix("<?xml version=\"1.0\"?>\n")
        .unwrap_or(&arch);
    assert!(root.starts_with("<t "), "{arch}");
    assert!(arch.contains("t-name=\"m.t1\""), "{arch}");
    assert_eq!(cell(&mut tx, v, "m.t1", "name").await, s("T"));
    // t2: an extension of t1 with priority 20; t3: a primary view
    let t1 = xid_id(&mut tx, "m.t1").await;
    assert_eq!(cell(&mut tx, v, "m.t2", "inherit_id").await, Cell::Id(t1));
    assert_eq!(cell(&mut tx, v, "m.t2", "priority").await, Cell::Int(20));
    // the real model derives `mode` from `inherit_id`: an extension unless declared primary
    assert_eq!(cell(&mut tx, v, "m.t1", "mode").await, s("primary"));
    assert_eq!(cell(&mut tx, v, "m.t2", "mode").await, s("extension"));
    assert_eq!(cell(&mut tx, v, "m.t3", "mode").await, s("primary"));
    // t4 is created inactive
    assert_eq!(cell(&mut tx, v, "m.t4", "active").await, Cell::Bool(false));
    assert_eq!(cell(&mut tx, v, "m.t1", "active").await, Cell::Bool(true));

    // the user activates t4; the module is upgraded: it stays active
    let t4 = xid_id(&mut tx, "m.t4").await;
    let t4_records = tx.env().records(v).unwrap().browse([t4]).unwrap();
    t4_records
        .set_value(&mut tx, "active", Value::Bool(true))
        .await
        .unwrap();
    load(&mut session, &mut tx, TEMPLATES, ConvertMode::Update)
        .await
        .unwrap();
    assert_eq!(cell(&mut tx, v, "m.t4", "active").await, Cell::Bool(true));
    // a template that is new in the upgrade is created with the active state it declares
    load(
        &mut session,
        &mut tx,
        "<odoo><template id=\"t5\" active=\"False\"><i/></template></odoo>",
        ConvertMode::Update,
    )
    .await
    .unwrap();
    assert_eq!(cell(&mut tx, v, "m.t5", "active").await, Cell::Bool(false));
    // init mode (a reinstall) applies it again
    init(&mut session, &mut tx, TEMPLATES).await.unwrap();
    assert_eq!(cell(&mut tx, v, "m.t4", "active").await, Cell::Bool(false));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn template_groups_and_a_failing_template_name_the_template() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let mut session = session();
    init(&mut session, &mut tx, PARTS).await.unwrap();
    init(
        &mut session,
        &mut tx,
        "<odoo><template id=\"tg\" groups=\"g_user,g_portal\"><p/></template></odoo>",
    )
    .await
    .unwrap();
    let id = xid_id(&mut tx, "m.tg").await;
    let g_user = xid_id(&mut tx, "m.g_user").await;
    let g_portal = xid_id(&mut tx, "m.g_portal").await;
    let view = tx
        .env()
        .records("ir.ui.view")
        .unwrap()
        .browse([id])
        .unwrap();
    let Value::List(items) = view.value(&mut tx, "group_ids").await.unwrap() else {
        panic!("a many2many reads as a list");
    };
    let mut linked: Vec<i64> = items
        .iter()
        .map(|v| match v {
            Value::Int(i) => *i,
            other => panic!("expected an id, got {other:?}"),
        })
        .collect();
    linked.sort_unstable();
    assert_eq!(linked, [g_user, g_portal]);

    let err = init(
        &mut session,
        &mut tx,
        "<odoo><template id=\"bad\" inherit_id=\"nothing\"><p/></template></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err.kind(),
        LoadErrorKind::ExternalIdNotFound { .. }
    ));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n<template id=\"bad\">\n\
         External ID not found in the system: m.nothing"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_theme_module_template_targets_the_theme_view_model() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let mut session = session();
    let err = load_as(
        "theme_x",
        &mut session,
        &mut tx,
        "<odoo><template id=\"t\"><p/></template></odoo>",
        ConvertMode::Init,
    )
    .await
    .unwrap_err();
    assert!(matches!(err.kind(), LoadErrorKind::ModelMissing { .. }));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n<template id=\"t\">\n\
         <record> model 'theme.ir.ui.view' is not in the registry of this install step"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
