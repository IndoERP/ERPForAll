//! The record rules across module operations: install loads in `init` mode, upgrade in
//! `update` mode (`noupdate` records are left to the user), reinit in `init` mode again, and the
//! end-of-run cleanup removes what a new version no longer ships.
//!
//! Dev-folder addons, a throwaway database on the dev cluster, leaked model declarations
//! (`common::model`). Each test runs in its own process (nextest).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use common::*;
use erp_modules::system::InitOptions;
use erp_testkit::TestDb;

const V1: &str = "<odoo>\
    <data noupdate=\"1\"><record id=\"kept\" model=\"test.cat\"><field name=\"name\">kept v1</field></record></data>\
    <record id=\"synced\" model=\"test.cat\"><field name=\"name\">synced v1</field></record>\
    </odoo>";
const V2: &str = "<odoo>\
    <data noupdate=\"1\"><record id=\"kept\" model=\"test.cat\"><field name=\"name\">kept v2</field></record></data>\
    <record id=\"synced\" model=\"test.cat\"><field name=\"name\">synced v2</field></record>\
    </odoo>";

/// The name of the record behind each external id of module `x`.
async fn by_xid(orm: &erp_orm::Orm) -> Vec<(String, String)> {
    rows(
        orm,
        "SELECT d.name, c.name FROM ir_model_data d JOIN test_cat c ON c.id = d.res_id \
         WHERE d.module = 'x' AND d.model = 'test.cat' ORDER BY d.name",
    )
    .await
    .into_iter()
    .map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap()))
    .collect()
}

fn pair(xid: &str, name: &str) -> (String, String) {
    (xid.to_owned(), name.to_owned())
}

#[tokio::test]
async fn spec_mod_031_install_loads_in_init_mode_upgrade_in_update_mode_and_reinit_in_init_mode() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/a.xml']\n")
        .add("y", "name = 'Y'\ndepends = ['x']\n")
        .file("x", "data/a.xml", V1);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();

    // install: init mode creates both records
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("y")])
        .await
        .unwrap();
    assert_eq!(
        by_xid(&installed.orm).await,
        [pair("kept", "kept v1"), pair("synced", "synced v1")]
    );
    // the `noupdate` flag of each external id is the container's
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT name, noupdate::text FROM ir_model_data WHERE module = 'x' AND model = 'test.cat' \
             ORDER BY name"
        )
        .await,
        [
            [Some("kept".to_owned()), Some("true".to_owned())],
            [Some("synced".to_owned()), Some("false".to_owned())],
        ]
    );
    exec(
        &installed.orm,
        "UPDATE test_cat SET name = 'changed by the user'",
    )
    .await;

    // upgrade: update mode rewrites the plain record and leaves the noupdate one to the user
    addons.file("x", "data/a.xml", V2);
    enroll_fixture_transition(&system, &["x"]).await;
    let upgraded = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("x")])
        .await
        .unwrap();
    assert!(
        upgraded.data_notices.is_empty(),
        "{:?}",
        upgraded.data_notices
    );
    assert_eq!(
        by_xid(&upgraded.orm).await,
        [
            pair("kept", "changed by the user"),
            pair("synced", "synced v2")
        ]
    );

    // reinit: init mode writes the noupdate record too
    exec(&upgraded.orm, "UPDATE test_cat SET name = 'changed again'").await;
    let reinit = system.reinit(&su(&upgraded.orm), &[n("x")]).await.unwrap();
    assert_eq!(reinit.upgraded, vec![n("x"), n("y")]);
    assert_eq!(
        by_xid(&reinit.orm).await,
        [pair("kept", "kept v2"), pair("synced", "synced v2")]
    );
    assert_eq!(state(&reinit.orm, "x").await, "installed");
    db.cleanup().await.unwrap();
}

/// ships, keeps the `noupdate` ones and the ones another external id still names. An upgrade
/// whose file did not change removes nothing.
#[tokio::test]
async fn an_upgrade_removes_the_data_the_new_version_no_longer_ships() {
    let addons = Addons::new();
    addons.add("x", "name = 'X'\ndata = ['data/a.xml']\n").file(
        "x",
        "data/a.xml",
        "<odoo>\
         <record id=\"r1\" model=\"test.cat\"><field name=\"name\">one</field></record>\
         <record id=\"r2\" model=\"test.cat\"><field name=\"name\">two</field></record>\
         <record id=\"r3\" model=\"test.cat\"><field name=\"name\">three</field></record>\
         <data noupdate=\"1\"><record id=\"r4\" model=\"test.cat\"><field name=\"name\">four</field></record></data>\
         </odoo>",
    );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap();
    // an unchanged file leaves nothing obsolete
    let same = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("x")])
        .await
        .unwrap();
    assert_eq!(by_xid(&same.orm).await.len(), 4);

    // `r3` is also named by a module that stays
    exec(
        &same.orm,
        "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         SELECT 'other', 'shared', 'test.cat', res_id, false FROM ir_model_data \
         WHERE module = 'x' AND name = 'r3'",
    )
    .await;
    addons.file(
        "x",
        "data/a.xml",
        "<odoo><record id=\"r1\" model=\"test.cat\"><field name=\"name\">one</field></record></odoo>",
    );
    enroll_fixture_transition(&system, &["x"]).await;
    let upgraded = system
        .button_immediate_upgrade(&su(&same.orm), &[n("x")])
        .await
        .unwrap();
    assert_eq!(
        by_xid(&upgraded.orm).await,
        [pair("r1", "one"), pair("r4", "four")],
        "r2 is gone with its external id, r3 lost only its external id, r4 is noupdate"
    );
    let remaining: Vec<String> = rows(&upgraded.orm, "SELECT name FROM test_cat ORDER BY name")
        .await
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(remaining, ["four", "one", "three"]);
    assert_eq!(
        rows(
            &upgraded.orm,
            "SELECT module, name FROM ir_model_data WHERE module = 'other'"
        )
        .await,
        [[Some("other".to_owned()), Some("shared".to_owned())]]
    );
    db.cleanup().await.unwrap();
}

/// An upgrade with an `ir.ui.view` model that has no `_validate_module_views` is refused: the
/// `noupdate` views of the upgraded module could not be checked, and skipping the check would be
/// silent (review A-S4). The `base` addon's view model has the method.
#[tokio::test]
async fn an_upgrade_with_a_view_model_without_the_noupdate_check_is_refused() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/a.xml']\n")
        .file("x", "data/a.xml", V1);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat"), model("x", "ir.ui.view")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap();
    let err = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("x")])
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Internal invariant broken: the registry's ir.ui.view has no _validate_module_views: \
         the noupdate views of module x cannot be checked after its upgrade",
        "{err:?}"
    );
    assert_eq!(state(&installed.orm, "x").await, "installed");
    db.cleanup().await.unwrap();
}

/// A run without data loader loads nothing, so an upgrade checks no views: it is not refused for
/// a view model without `_validate_module_views` (review A2-R7: the loader is looked at before
/// the method). The same model with a loader is refused (the test above). Before the fix this
/// upgrade failed with the missing-method error.
#[tokio::test]
async fn an_upgrade_without_data_loader_needs_no_noupdate_check() {
    let addons = Addons::new();
    // no data files: a run without loader refuses a module that has some
    addons.add("x", "name = 'X'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .no_data_loader()
        .decls([model("x", "test.cat"), model("x", "ir.ui.view")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap();
    let upgraded = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("x")])
        .await
        .unwrap();
    assert_eq!(state(&upgraded.orm, "x").await, "installed");
    db.cleanup().await.unwrap();
}

// A method-dispatch probe: names and pending deliberately differ, so swapping the two
// same-typed arguments cannot pass. The module system computes both during a real upgrade.
type ViewCheckArgs = (String, Vec<String>, Vec<String>);
static VIEW_CHECK_CALLS: std::sync::Mutex<Vec<ViewCheckArgs>> = std::sync::Mutex::new(Vec::new());

fn capture_view_check<'a>(
    _tx: &'a mut erp_orm::Transaction,
    _records: &'a erp_orm::Records,
    args: ViewCheckArgs,
    _sup: erp_orm::Super<ViewCheckArgs, ()>,
) -> erp_orm::BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        VIEW_CHECK_CALLS.lock().unwrap().push(args);
        Ok(())
    })
}

fn capture_view_check_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static F: erp_orm::MethodFn<ViewCheckArgs, ()> = capture_view_check;
    &F
}

static CAPTURE_VIEW_CHECK: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: "x",
    model: "ir.ui.view",
    method: "_validate_module_views",
    kind: erp_orm::ImplKind::Define,
    sig_name: std::any::type_name::<erp_orm::MethodFn<ViewCheckArgs, ()>>,
    entry: capture_view_check_entry,
    file: file!(),
    line: line!(),
};

#[tokio::test]
async fn two_module_upgrade_passes_only_later_modules_as_pending() {
    VIEW_CHECK_CALLS.lock().unwrap().clear();
    let addons = Addons::new();
    addons.add("x", "name = 'X'\ndata = ['data/a.xml']\n")
        .file("x", "data/a.xml", V1)
        .add("y", "name = 'Y'\ndepends = ['x']\ndata = ['data/a.xml']\n")
        .file("y", "data/a.xml", "<odoo><record id=\"y_record\" model=\"test.cat\"><field name=\"name\">Y</field></record></odoo>");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat"), model("x", "ir.ui.view")])
        .impls([&CAPTURE_VIEW_CHECK])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("y")])
        .await
        .unwrap();
    assert!(VIEW_CHECK_CALLS.lock().unwrap().is_empty());
    let upgraded = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("x"), n("y")])
        .await
        .unwrap();
    assert_eq!(state(&upgraded.orm, "x").await, "installed");
    assert_eq!(state(&upgraded.orm, "y").await, "installed");
    assert_eq!(
        *VIEW_CHECK_CALLS.lock().unwrap(),
        vec![
            (
                "x".into(),
                vec!["kept".into(), "synced".into()],
                vec!["y".into()]
            ),
            ("y".into(), vec!["y_record".into()], vec![]),
        ]
    );
    db.cleanup().await.unwrap();
}
