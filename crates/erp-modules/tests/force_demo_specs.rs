//! "Load demo data" on a database created without demo.
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

use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use erp_data::load::NoticeKind;
use erp_modules::error::Error;
use erp_modules::install::{InstallAuthorization, InstallAuthorizer};
use erp_modules::system::InitOptions;
use erp_orm::{BoxFuture, GroupSet, PrincipalBuilder, Resolution, Transaction, UserId};
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;

fn record_xml(id: &str, name: &str) -> String {
    format!(
        "<odoo><record id=\"{id}\" model=\"test.cat\"><field name=\"name\">{name}</field></record></odoo>"
    )
}

fn demo_failure_model() -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(
        vec![
            FieldDecl::many2one("module_id", "ir.module.module"),
            FieldDecl::text("error"),
        ]
        .into_boxed_slice(),
    );
    Box::leak(Box::new(
        ModelDecl::new("base", "ir.demo_failure")
            .description("Demo failure")
            .fields(fields)
            .at(file!(), line!()),
    ))
}

/// `a`; `x` (depends `a`, data and demo); `y` (depends `x`, demo); `bad` (depends `a`, demo that
/// fails on its data); `w` (depends `bad`, demo); `z` (depends `a`, demo, installed later).
fn addons() -> Addons {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add(
            "x",
            "name = 'X'\ndepends = ['a']\ndata = ['data/d.xml']\ndemo = ['demo/dm.xml']\n",
        )
        .file("x", "data/d.xml", &record_xml("x_data", "x data"))
        .file("x", "demo/dm.xml", &record_xml("x_demo", "x demo"))
        .add(
            "y",
            "name = 'Y'\ndepends = ['x']\ndemo = ['demo/dm.xml']\n",
        )
        .file("y", "demo/dm.xml", &record_xml("y_demo", "y demo"))
        .add(
            "bad",
            "name = 'Bad'\ndepends = ['a']\ndemo = ['demo/broken.xml']\n",
        )
        .file(
            "bad",
            "demo/broken.xml",
            "<odoo><record id=\"bad_demo\" model=\"no.such.model\"><field name=\"name\">never</field></record></odoo>",
        )
        .add(
            "w",
            "name = 'W'\ndepends = ['bad']\ndemo = ['demo/dm.xml']\n",
        )
        .file("w", "demo/dm.xml", &record_xml("w_demo", "w demo"))
        .add(
            "z",
            "name = 'Z'\ndepends = ['a']\ndemo = ['demo/dm.xml']\n",
        )
        .file("z", "demo/dm.xml", &record_xml("z_demo", "z demo"));
    addons
}

/// A database created without demo data, with `a`, `x`, `y`, `bad` and `w` installed.
async fn demo_less_database(
    db: &TestDb,
    addons: &Addons,
) -> (erp_modules::ModuleSystem, erp_orm::Orm) {
    let system = builder(db, addons)
        .decls([model("a", "test.cat"), demo_failure_model()])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("y"), n("w")])
        .await
        .unwrap();
    assert!(outcome.data_notices.is_empty());
    (system, outcome.orm)
}

async fn cat_names(orm: &erp_orm::Orm) -> Vec<String> {
    rows(orm, "SELECT name FROM test_cat ORDER BY id")
        .await
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect()
}

async fn demo_flags(orm: &erp_orm::Orm) -> Vec<(String, bool)> {
    rows(
        orm,
        "SELECT name, demo::text FROM ir_module_module \
         WHERE name IN ('a', 'bad', 'w', 'x', 'y', 'z') ORDER BY name",
    )
    .await
    .into_iter()
    .map(|r| (r[0].clone().unwrap(), r[1].as_deref() == Some("true")))
    .collect()
}

async fn versions(orm: &erp_orm::Orm) -> Vec<Vec<Option<String>>> {
    rows(
        orm,
        "SELECT name, latest_version FROM ir_module_module ORDER BY name",
    )
    .await
}

fn flags(list: [(&str, bool); 6]) -> Vec<(String, bool)> {
    list.map(|(n, d)| (n.to_owned(), d)).to_vec()
}

#[tokio::test]
async fn spec_mod_053_force_demo_loads_the_demo_of_every_installed_module_in_order() {
    let addons = addons();
    let db = TestDb::create().await.unwrap();
    let (system, orm) = demo_less_database(&db, &addons).await;
    assert_eq!(cat_names(&orm).await, ["x data"]);
    assert_eq!(
        demo_flags(&orm).await,
        flags([
            ("a", false),
            ("bad", false),
            ("w", false),
            ("x", false),
            ("y", false),
            ("z", false)
        ])
    );
    // an administrator edited the data record: the data files must not run again
    exec(&orm, "UPDATE test_cat SET name = 'x edited'").await;
    let versions_before = versions(&orm).await;
    let tables_before = rows(
        &orm,
        "SELECT count(*)::text FROM information_schema.columns WHERE table_name = 'test_cat'",
    )
    .await;

    let outcome = system.force_demo(&su(&orm)).await.unwrap();
    assert!(outcome.post_commit.failed.is_empty());
    let orm = outcome.orm;

    // `z` is not installed and still has the flag: it will get demo data when installed
    assert_eq!(
        demo_flags(&orm).await,
        flags([
            ("a", true),
            ("bad", false),
            ("w", true),
            ("x", true),
            ("y", true),
            ("z", true)
        ])
    );
    // dependencies load before their dependents, by depth then name (insertion order
    // is load order); the data record keeps the administrator's edit
    assert_eq!(
        cat_names(&orm).await,
        ["x edited", "x demo", "w demo", "y demo"]
    );
    let pos = |name: &str| {
        outcome
            .upgraded
            .iter()
            .position(|m| m.as_str() == name)
            .unwrap_or_else(|| panic!("{name} not in {:?}", outcome.upgraded))
    };
    assert!(pos("a") < pos("x") && pos("x") < pos("y") && pos("a") < pos("bad"));
    assert_eq!(
        rows(
            &orm,
            "SELECT count(*)::text FROM information_schema.columns WHERE table_name = 'test_cat'"
        )
        .await,
        tables_before,
        "the schema is not touched"
    );
    // the demo records are `noupdate`, like any demo file's
    assert_eq!(
        rows(
            &orm,
            "SELECT noupdate::text FROM ir_model_data WHERE module = 'x' AND name = 'x_demo'"
        )
        .await,
        [[Some("true".to_owned())]]
    );
    // no installed version changed
    assert_eq!(versions(&orm).await, versions_before);
    // every module is `installed` again, none is left scheduled
    for module in ["a", "bad", "w", "x", "y"] {
        assert_eq!(state(&orm, module).await, "installed", "{module}");
    }
    assert_eq!(state(&orm, "z").await, "uninstalled");
    db.cleanup().await.unwrap();
}

/// whole operation is `data_loader_specs.rs` for the shared loader, and the HTTP reply naming a
/// failed module is not covered)
#[tokio::test]
async fn spec_mod_054_a_failing_demo_is_rolled_back_alone_and_reported() {
    let addons = addons();
    let db = TestDb::create().await.unwrap();
    let (system, orm) = demo_less_database(&db, &addons).await;

    let outcome = system.force_demo(&su(&orm)).await.unwrap();

    // reported, with the module and the loader's text
    let [notice] = outcome.data_notices.as_slice() else {
        panic!("expected one notice, got {:?}", outcome.data_notices);
    };
    assert_eq!((notice.module(), notice.file()), ("bad", "demo/broken.xml"));
    let NoticeKind::DemoFailed { error } = notice.kind() else {
        panic!("expected DemoFailed, got {:?}", notice.kind());
    };
    assert!(
        error.contains("demo file 'demo/broken.xml' failed"),
        "{error}"
    );

    let orm = outcome.orm;
    // rolled back alone: nothing of `bad`, the others loaded (also `w`, which depends on `bad`)
    assert_eq!(
        cat_names(&orm).await,
        ["x data", "x demo", "w demo", "y demo"]
    );
    assert_eq!(
        rows(&orm, "SELECT name FROM ir_model_data WHERE module = 'bad'").await,
        Vec::<Vec<Option<String>>>::new()
    );
    assert_eq!(
        demo_flags(&orm).await[1],
        ("bad".to_owned(), false),
        "the failed module keeps demo=false"
    );
    assert_eq!(state(&orm, "bad").await, "installed");
    // and the same text is stored when the model exists
    assert_eq!(
        rows(
            &orm,
            "SELECT m.name, f.error FROM ir_demo_failure f \
             JOIN ir_module_module m ON m.id = f.module_id",
        )
        .await,
        [[Some("bad".to_owned()), Some(error.clone())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_055_force_demo_twice_does_not_duplicate_records() {
    let addons = addons();
    let db = TestDb::create().await.unwrap();
    let (system, orm) = demo_less_database(&db, &addons).await;

    let first = system.force_demo(&su(&orm)).await.unwrap();
    let counts = |orm: erp_orm::Orm| async move {
        (
            cat_names(&orm).await,
            rows(&orm, "SELECT count(*)::text FROM ir_model_data").await,
        )
    };
    let after_first = counts(first.orm.clone()).await;

    let second = system.force_demo(&su(&first.orm)).await.unwrap();
    assert_eq!(counts(second.orm.clone()).await, after_first);
    assert_eq!(
        second.data_notices.len(),
        first.data_notices.len(),
        "the same module fails again, no other"
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_056_a_module_installed_after_force_demo_gets_its_demo() {
    let addons = addons();
    let db = TestDb::create().await.unwrap();
    let (system, orm) = demo_less_database(&db, &addons).await;
    let forced = system.force_demo(&su(&orm)).await.unwrap();

    let installed = system
        .button_immediate_install(&su(&forced.orm), &[n("z")])
        .await
        .unwrap();
    assert!(installed.data_notices.is_empty());
    let orm = installed.orm;
    assert_eq!(demo_flags(&orm).await[5], ("z".to_owned(), true));
    assert!(cat_names(&orm).await.contains(&"z demo".to_owned()));
    db.cleanup().await.unwrap();
}

struct Authorizer {
    deny: bool,
    calls: AtomicUsize,
}
impl InstallAuthorizer for Authorizer {
    fn authorize<'a>(
        &'a self,
        _tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.deny {
                return Err(erp_fields::Error::access(
                    "Module installation requires system administration authority",
                ));
            }
            let principal = PrincipalBuilder::user(UserId::new(2).unwrap())
                .resolution(
                    Resolution::in_id_order(GroupSet::default(), [], std::num::NonZeroU64::MIN)
                        .unwrap(),
                )
                .build()
                .unwrap();
            InstallAuthorization::new(principal, "019a0000-0000-7000-8000-000000000001".into())
        })
    }
}

/// HTTP and `web.rs` tests)
#[tokio::test]
async fn spec_mod_057_only_administrators_load_demo_data() {
    let addons = addons();
    let db = TestDb::create().await.unwrap();
    let (system, orm) = demo_less_database(&db, &addons).await;
    let before = (demo_flags(&orm).await, cat_names(&orm).await);

    // a user who is not the superuser
    let user = orm.env(UserId::new(2).unwrap(), erp_orm::Context::new());
    let error = system.force_demo(&user).await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::AccessDenied {
                method: "force_demo",
                uid: 2
            }
        ),
        "{error:?}"
    );

    // a session whose administrator authority was revoked
    let denied = Authorizer {
        deny: true,
        calls: AtomicUsize::new(0),
    };
    let error = system.force_demo_authorized(&denied).await.unwrap_err();
    assert_eq!(denied.calls.load(Ordering::SeqCst), 1);
    assert!(
        error
            .to_string()
            .contains("system administration authority"),
        "{error}"
    );
    assert_eq!((demo_flags(&orm).await, cat_names(&orm).await), before);
    assert_eq!(
        rows(
            &orm,
            "SELECT count(*)::text FROM erp_module_lifecycle_audit"
        )
        .await,
        [[Some("0".to_owned())]]
    );

    // the administrator session: succeeds and is audited once
    exec(
        &orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator')",
    )
    .await;
    let allowed = Authorizer {
        deny: false,
        calls: AtomicUsize::new(0),
    };
    let outcome = system.force_demo_authorized(&allowed).await.unwrap();
    assert_eq!(allowed.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT operation, actor_uid::text, requested::text FROM erp_module_lifecycle_audit"
        )
        .await,
        [[
            Some("force_demo".to_owned()),
            Some("2".to_owned()),
            Some("{base}".to_owned())
        ]]
    );
    assert!(cat_names(&outcome.orm).await.contains(&"x demo".to_owned()));
    db.cleanup().await.unwrap();
}

/// dependency refusals are the catalog test in `lifecycle_specs.rs`)
#[tokio::test]
async fn spec_mod_058_force_demo_is_refused_while_operations_are_pending() {
    let addons = addons();
    let db = TestDb::create().await.unwrap();
    let (system, orm) = demo_less_database(&db, &addons).await;
    let before = (
        states(&orm).await,
        demo_flags(&orm).await,
        cat_names(&orm).await,
    );

    // (a) a scheduled install: the operator entry must not run it demo-only
    system
        .button_install(
            &su(&orm),
            &[n("z")],
            &erp_modules::lifecycle::InstallOptions::new(),
        )
        .await
        .unwrap();
    let scheduled = states(&orm).await;
    let error = system.force_demo(&su(&orm)).await.unwrap_err();
    assert!(
        matches!(&error, Error::ForceDemoPending { modules } if modules == &[n("z")]),
        "{error:?}"
    );
    assert_eq!(
        states(&orm).await,
        scheduled,
        "nothing changed, z is still scheduled"
    );
    assert_eq!(state(&orm, "z").await, "to install");

    // (b) a scheduled uninstall is not executed by it either
    system.button_reset_state(&su(&orm)).await.unwrap();
    system.button_uninstall(&su(&orm), &[n("y")]).await.unwrap();
    let error = system.force_demo(&su(&orm)).await.unwrap_err();
    assert!(matches!(error, Error::ForceDemoPending { .. }), "{error:?}");
    assert_eq!(state(&orm, "y").await, "to remove");
    assert_eq!(state(&orm, "x").await, "installed");

    system.button_reset_state(&su(&orm)).await.unwrap();
    assert_eq!(
        (
            states(&orm).await,
            demo_flags(&orm).await,
            cat_names(&orm).await
        ),
        before
    );
    db.cleanup().await.unwrap();
}
