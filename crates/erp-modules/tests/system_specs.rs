//! Module operations on a database (MOD §4.2–4.11): initialization, immediate install,
//! upgrade and uninstall in one transaction, hooks, data loading order, plain loads.
//!
//! Each test runs in its own process (nextest), so the recording statics are per test (see
//! `common/mod.rs`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use std::any::Any;
use std::fs;
use std::sync::{Arc, Mutex};

use common::*;
use erp_fields::ErrorKind;
use erp_modules::LimitKind;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::hooks::{HookImpl, HookKind};
use erp_modules::lifecycle::InstallOptions;
use erp_modules::loader::{DataFile, FileLoader, Operations};
use erp_modules::manifest::ModuleName;
use erp_modules::system::{
    ClientAction, DatabaseName, InitOptions, ModuleSystem, OPERATION_LOCK, POST_COMMIT_LOCK,
    PostCommitStep, PostCommitStop, create_database, schedule_post_commit, validate_database_name,
};
use erp_modules::{AddonIndex, base_models};
use erp_orm::{
    BoxFuture, Context, Env, ImplKind, MethodFn, MethodImpl, Orm, Records, SUPERUSER_ID, Super,
    Transaction, UserId,
};
use erp_testkit::TestDb;

// ---- / 016: initialization ------------------------------------------------

#[tokio::test]
async fn spec_mod_015_initialize_lists_modules_and_seeds_auto_install() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndepends = ['base']\nauto_install = true\n")
        .add("y", "name = 'Y'\ninstallable = false\n")
        .add("z", "name = 'Z'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let outcome = system.initialize(InitOptions::new()).await.unwrap();
    assert_eq!(outcome.installed, vec![n("base"), n("x")]);
    assert_eq!(
        states(&outcome.orm).await,
        [
            ("base", "installed"),
            ("x", "installed"),
            ("y", "uninstallable"),
            ("z", "uninstalled")
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );
    let xmlids = rows(
        &outcome.orm,
        "SELECT d.name, d.noupdate::text FROM ir_model_data d JOIN ir_module_module m \
         ON m.id = d.res_id WHERE d.module = 'base' AND d.model = 'ir.module.module' \
         ORDER BY d.name",
    )
    .await;
    let expected: Vec<Vec<Option<String>>> = ["base", "x", "y", "z"]
        .iter()
        .map(|m| vec![Some(format!("module_{m}")), Some("true".to_owned())])
        .collect();
    assert_eq!(xmlids, expected);
    // the installed version is the manifest's semver
    let version = addons.manifest("x").version().semver().copied().unwrap();
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT latest_version FROM ir_module_module WHERE name = 'x'"
        )
        .await[0][0],
        Some(version.to_string())
    );
    db.cleanup().await.unwrap();

    // --skip-auto-install: only base
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("base")]);
    assert_eq!(state(&outcome.orm, "x").await, "uninstalled");
    // a second initialization is refused
    let err = system.initialize(InitOptions::new()).await.unwrap_err();
    assert!(matches!(err, Error::DatabaseAlreadyInitialized { .. }));
    assert_eq!(
        err.to_string(),
        format!("Database {} is already initialized", db.name())
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_016_module_category_chain() {
    let addons = Addons::new();
    addons
        .add(
            "crm_tools",
            "name = 'CRM Tools'\ncategory = 'Sales & CRM/Tools'\n",
        )
        .add(
            "crm_more",
            "name = 'CRM More'\ncategory = 'Sales & CRM/Tools'\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let cats = rows(
        &outcome.orm,
        "SELECT d.name, c.name, p.name FROM ir_model_data d \
         JOIN ir_module_category c ON c.id = d.res_id \
         LEFT JOIN ir_module_category p ON p.id = c.parent_id \
         WHERE d.module = 'base' AND d.model = 'ir.module.category' \
         AND d.name LIKE 'module_category_sales%' ORDER BY d.name",
    )
    .await;
    assert_eq!(
        cats,
        vec![
            vec![
                Some("module_category_sales_and_crm".to_owned()),
                Some("Sales & CRM".to_owned()),
                None
            ],
            vec![
                Some("module_category_sales_and_crm_tools".to_owned()),
                Some("Tools".to_owned()),
                Some("Sales & CRM".to_owned())
            ],
        ]
    );
    let module_cats = rows(
        &outcome.orm,
        "SELECT m.name, c.name FROM ir_module_module m JOIN ir_module_category c \
         ON c.id = m.category_id WHERE m.name LIKE 'crm%' ORDER BY m.name",
    )
    .await;
    assert_eq!(
        module_cats,
        vec![
            vec![Some("crm_more".to_owned()), Some("Tools".to_owned())],
            vec![Some("crm_tools".to_owned()), Some("Tools".to_owned())],
        ],
        "both modules point to the one shared child category"
    );
    db.cleanup().await.unwrap();
}

// ----: access ------------------------------------------------------------------

/// S5: a superuser environment made by another ORM lineage (here on the same pool and even the
/// same registry) is refused by every entry point, and nothing is scheduled.
#[tokio::test]
async fn an_environment_of_another_orm_lineage_is_refused() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let foreign_orm = erp_orm::Orm::builder(
        std::sync::Arc::clone(outcome.orm.registry()),
        db.pool().clone(),
    )
    .policy(erp_orm::StaticPolicy::AllowAll)
    .build()
    .unwrap();
    assert!(!foreign_orm.same_lineage(&outcome.orm));
    let foreign = foreign_orm.env(SUPERUSER_ID, Context::new());
    assert!(foreign.same_lineage(&foreign_orm));
    assert!(!foreign.same_lineage(&outcome.orm));
    let mine = su(&outcome.orm);
    assert!(mine.same_lineage(&outcome.orm));
    let names = [n("a")];
    let refused = |err: Error, method: &str| {
        assert!(
            matches!(&err, Error::ForeignEnvironment { method: m } if *m == method),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "Access denied to module.{method}: the environment does not belong to this \
                 module system's ORM"
            )
        );
    };
    refused(
        system
            .button_install(&foreign, &names, &InstallOptions::new())
            .await
            .unwrap_err(),
        "button_install",
    );
    refused(
        system.update_list(&foreign).await.unwrap_err(),
        "update_list",
    );
    refused(
        system
            .button_immediate_install(&foreign, &names)
            .await
            .unwrap_err(),
        "button_immediate_install",
    );
    assert_eq!(state(&outcome.orm, "a").await, "uninstalled");
    // the module system's own environments still work
    system
        .button_install(&mine, &names, &InstallOptions::new())
        .await
        .unwrap();
    db.cleanup().await.unwrap();
}

/// `module_uninstall` is no public entry point before M3, so its denial is not covered)
#[tokio::test]
async fn spec_mod_022_only_administrators_schedule_operations() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let user = outcome.orm.env(UserId::new(2).unwrap(), Context::new());
    let names = [n("a")];
    let opts = InstallOptions::new();
    let denied = |err: Error, method: &str| {
        assert!(
            matches!(&err, Error::AccessDenied { uid: 2, .. }),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "Access denied to module.{method} for user 2: only the superuser may run module operations until M3 adds administrator groups"
            )
        );
    };
    denied(
        system
            .button_install(&user, &names, &opts)
            .await
            .unwrap_err(),
        "button_install",
    );
    denied(
        system
            .button_upgrade(&user, &names, &opts)
            .await
            .unwrap_err(),
        "button_upgrade",
    );
    denied(
        system.button_uninstall(&user, &names).await.unwrap_err(),
        "button_uninstall",
    );
    denied(
        system
            .button_immediate_install(&user, &names)
            .await
            .unwrap_err(),
        "button_immediate_install",
    );
    denied(
        system
            .button_immediate_upgrade(&user, &names)
            .await
            .unwrap_err(),
        "button_immediate_upgrade",
    );
    denied(
        system
            .button_immediate_uninstall(&user, &names)
            .await
            .unwrap_err(),
        "button_immediate_uninstall",
    );
    denied(system.update_list(&user).await.unwrap_err(), "update_list");
    denied(
        system.button_reset_state(&user).await.unwrap_err(),
        "button_reset_state",
    );
    // sudo of another user is not the superuser: until M3 checks groups, only uid 1 passes
    // (lead review J)
    denied(
        system
            .button_install(&user.sudo(true), &names, &opts)
            .await
            .unwrap_err(),
        "button_install",
    );
    // `with_user(1)` from a user's environment is refused by the engine rule (A4): the
    // environment stays the user's own principal without elevation, and the denial names it
    denied(
        system
            .button_install(&user.with_user(1), &names, &opts)
            .await
            .unwrap_err(),
        "button_install",
    );
    assert_eq!(
        state(&outcome.orm, "a").await,
        "uninstalled",
        "nothing scheduled"
    );
    system
        .button_install(&su(&outcome.orm), &names, &opts)
        .await
        .unwrap();
    assert_eq!(state(&outcome.orm, "a").await, "to install");
    db.cleanup().await.unwrap();
}

// ---- / 028 / 030: immediate operations ----------------------------------------

#[tokio::test]
async fn spec_mod_027_immediate_install_end_to_end() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\nversion = '20.0.1.2'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.a")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(init.orm.registry().model("test.a").is_none());
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("a")])
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("a")]);
    assert_eq!(outcome.action, ClientAction::reload_home());
    assert_eq!(
        outcome.action,
        ClientAction::ActUrl {
            url: "/erp",
            target: "self"
        }
    );
    assert!(outcome.orm.registry().model("test.a").is_some());
    assert!(table_exists(&outcome.orm, "test_a").await);
    let version = addons.manifest("a").version().semver().copied().unwrap();
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT state, latest_version FROM ir_module_module WHERE name = 'a'"
        )
        .await[0],
        vec![Some("installed".to_owned()), Some(version.to_string())]
    );
    assert_eq!(version.to_string(), "1.2.0", " mapping");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_028_immediate_operation_refused_during_another() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    // another operation in progress: its transaction holds the lock
    let mut other = init.orm.begin(SUPERUSER_ID).await.unwrap();
    other
        .query(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await
        .unwrap();
    // Refused without waiting: `other` holds the lock until after this call, so a call that
    // waited for it would never return.
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("a")])
        .await
        .unwrap_err();
    assert!(
        !advisory_waiting(&init.orm, OPERATION_LOCK).await,
        "no session waits for the operation lock"
    );
    assert!(matches!(err, Error::OperationInProgress));
    assert_eq!(
        err.to_string(),
        "ERPForAll is currently processing another module operation.\nPlease try again later or contact your system administrator."
    );
    other.rollback().await.unwrap();
    drop(other);
    assert_eq!(
        state(&init.orm, "a").await,
        "uninstalled",
        "nothing scheduled"
    );
    // once the other operation ended, the install runs
    committed(
        system
            .button_immediate_install(&su(&init.orm), &[n("a")])
            .await
            .unwrap(),
    );
    assert_eq!(state(&init.orm, "a").await, "installed");
    db.cleanup().await.unwrap();
}

/// The parse error of the `broken.xml` files below (an unclosed `<record>`), as `erp-data`'s XML
/// reader reports it.
const BROKEN_XML_ERROR: &str = "while parsing data/broken.xml:1: malformed XML: ill-formed document: expected `</record>`, but `</odoo>` was found";

fn auto_ran(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let rows = tx
            .query("SELECT (to_regclass('test_install_base') IS NOT NULL)", &[])
            .await?;
        let exists: bool = rows[0].get(0);
        log(format!("auto post_init base_table={exists}"));
        Ok(())
    })
}

static AUTO_RAN: HookImpl = HookImpl::new(
    "test_install_auto",
    HookKind::PostInit,
    "auto_ran",
    auto_ran,
)
.at(file!(), line!());

#[tokio::test]
async fn spec_mod_030_multi_module_install_is_all_or_nothing() {
    let addons = Addons::new();
    addons
        .add("test_install_base", "name = 'Install base'\n")
        .add(
            "test_install_auto",
            "name = 'Install auto'\ndepends = ['test_install_base']\nauto_install = true\n\
             post_init_hook = 'auto_ran'\n",
        )
        .add(
            "test_install_fail",
            "name = 'Install fail'\ndepends = ['test_install_base']\ndata = ['data/broken.xml']\n",
        )
        .file(
            "test_install_fail",
            "data/broken.xml",
            "<odoo><record id=\"x\" model=\"test.x\"><field name=\"name\">x</field></odoo>",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model("test_install_base", "test.install.base"),
            model("test_install_auto", "test.install.auto"),
        ])
        .data_loader(Arc::new(RecordingLoader))
        .hooks([&AUTO_RAN])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let before = snapshot(&init.orm).await;
    let registry_before = registry_shape(&system.load().await.unwrap().orm);
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("test_install_fail")])
        .await
        .unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        (
            "test_install_fail",
            ModuleStep::DataFile,
            Some("data/broken.xml")
        )
    );
    assert!(matches!(source, Error::DataParse(_)), "{source:?}");
    assert_eq!(
        chain(&err)[..2],
        [
            "Module \"test_install_fail\": data file 'data/broken.xml' failed",
            BROKEN_XML_ERROR
        ]
    );
    // the modules before it in the batch did run (their tables existed in the transaction)
    assert_eq!(take_log(), ["auto post_init base_table=true"]);
    // nothing of the operation persists: the whole database as before, and the registry a plain
    // load builds from it
    assert_eq!(snapshot(&init.orm).await, before);
    let loaded = system.load().await.unwrap();
    assert!(loaded.forced_update.is_none());
    assert_eq!(registry_shape(&loaded.orm), registry_before);
    for module in [
        "test_install_base",
        "test_install_auto",
        "test_install_fail",
    ] {
        assert_eq!(state(&init.orm, module).await, "uninstalled", "{module}");
    }
    assert!(!table_exists(&init.orm, "test_install_base").await);
    assert!(!table_exists(&init.orm, "test_install_auto").await);
    db.cleanup().await.unwrap();
}

// ---- / 031 / 044: per-module pipeline ---------------------------------------

fn record_pre_init(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let rows = tx
            .query("SELECT (to_regclass('test_x') IS NOT NULL)", &[])
            .await?;
        let exists: bool = rows[0].get(0);
        log(format!("pre_init table_exists={exists}"));
        Ok(())
    })
}

fn record_post_init(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let rows = tx.query("SELECT count(*) FROM test_x", &[]).await?;
        let count: i64 = rows[0].get(0);
        log(format!("post_init rows={count}"));
        Ok(())
    })
}

static PRE_INIT_X: HookImpl =
    HookImpl::new("x", HookKind::PreInit, "pre_x", record_pre_init).at(file!(), line!());

static POST_INIT_X: HookImpl =
    HookImpl::new("x", HookKind::PostInit, "post_x", record_post_init).at(file!(), line!());

fn init_x<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        let rows = tx
            .query("SELECT (to_regclass('test_x') IS NOT NULL)", &[])
            .await?;
        let exists: bool = rows[0].get(0);
        log(format!(
            "init {} table_exists={exists}",
            records.model_name()
        ));
        sup.call(tx, records, args).await
    })
}

fn init_x_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = init_x;
    &F
}

static INIT_X: MethodImpl = MethodImpl {
    addon: "x",
    model: "test.x",
    method: "init",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: init_x_entry,
    file: file!(),
    line: line!(),
};

/// A loader that records like [`RecordingLoader`] and inserts one row into `test.x` per data
/// file of `x` (so `post_init` can see data).
struct InsertingLoader;

impl FileLoader for InsertingLoader {
    /// Installs only: these tests uninstall nothing.
    fn purge<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _modules: &'a [ModuleName],
    ) -> BoxFuture<'a, erp_modules::Result<Vec<erp_data::load::DataNotice>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn load<'a, 'o: 'a>(
        &'a self,
        tx: &'a mut Transaction,
        file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            let mut count = 0;
            for op in operations {
                op?;
                count += 1;
            }
            log(format!(
                "{}:{}:{}:{}:{count}",
                file.kind().as_str(),
                file.module(),
                file.path(),
                file.mode().as_str()
            ));
            if file.module().as_str() == "x" {
                tx.execute("INSERT INTO test_x (name) VALUES ('loaded')", &[])
                    .await?;
            }
            Ok(())
        })
    }
}

/// demo → post_init (data present) → installed, in one transaction; field-groups reflection and
/// translations do not exist before M3)
#[tokio::test]
async fn spec_mod_029_per_module_pipeline_order() {
    let addons = Addons::new();
    addons
        .add(
            "x",
            "name = 'X'\npre_init_hook = 'pre_x'\npost_init_hook = 'post_x'\n\
             data = ['data/b.xml', 'data/a.xml']\ndemo = ['demo/d.xml']\n",
        )
        .file("x", "data/a.xml", ONE_RECORD_XML)
        .file("x", "data/b.xml", ONE_RECORD_XML)
        .file("x", "demo/d.xml", ONE_RECORD_XML);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.x")])
        .impls([&INIT_X])
        .hooks([&PRE_INIT_X, &POST_INIT_X])
        .data_loader(Arc::new(InsertingLoader))
        .build()
        .unwrap();
    let init = system
        .initialize(
            InitOptions::new()
                .with_skip_auto_install(true)
                .with_demo(true),
        )
        .await
        .unwrap();
    take_log();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap();
    assert_eq!(
        take_log(),
        [
            "pre_init table_exists=false",
            "init test.x table_exists=true",
            "data:x:data/b.xml:init:1",
            "data:x:data/a.xml:init:1",
            "demo:x:demo/d.xml:init:1",
            "post_init rows=3",
        ]
    );
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT state, demo::text FROM ir_module_module WHERE name = 'x'"
        )
        .await[0],
        vec![Some("installed".to_owned()), Some("true".to_owned())]
    );
    db.cleanup().await.unwrap();
}

/// update on upgrade; the record-level noupdate rules are's)
#[tokio::test]
async fn spec_mod_031_data_mode_by_operation() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/a.xml']\n")
        .file("x", "data/a.xml", ONE_RECORD_XML);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.x")])
        .data_loader(Arc::new(RecordingLoader))
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    take_log();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("x")])
            .await
            .unwrap(),
    );
    assert_eq!(take_log(), ["data:x:data/a.xml:init:1"]);
    let outcome = system
        .button_immediate_upgrade(&env, &[n("x")])
        .await
        .unwrap();
    assert_eq!(outcome.upgraded, vec![n("x")]);
    assert_eq!(take_log(), ["data:x:data/a.xml:update:1"]);
    let outcome = system.reinit(&env, &[n("x")]).await.unwrap();
    assert_eq!(
        outcome.upgraded,
        vec![n("x")],
        "reinit runs the upgrade step"
    );
    assert_eq!(take_log(), ["data:x:data/a.xml:init:1"]);
    assert_eq!(state(&outcome.orm, "x").await, "installed");
    db.cleanup().await.unwrap();
}

/// With the data loader disabled, a module with data files cannot be installed (fail loudly).
#[tokio::test]
async fn data_files_without_a_loader_are_an_error() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/a.xml']\n")
        .file("x", "data/a.xml", ONE_RECORD_XML)
        .add("t", "name = 'T'\ndata = ['data/a.txt']\n")
        .file("t", "data/a.txt", "text");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).no_data_loader().build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::DataLoaderMissing { module } if module.as_str() == "x"));
    assert_eq!(
        err.to_string(),
        "Module \"x\" lists data or demo files, but the data loader was disabled (ModuleSystemBuilder::no_data_loader)"
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");

    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
        .build()
        .unwrap();
    let env = su_of(&system);
    let err = system
        .button_immediate_install(&env, &[n("t")])
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Module \"t\": can't load unknown file type data/a.txt (expected .xml, .csv, .sql or .js)"
    );
    db.cleanup().await.unwrap();
}

fn init_m_a<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        log(format!("init {} by a", records.model_name()));
        sup.call(tx, records, args).await
    })
}

fn init_m_a_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = init_m_a;
    &F
}

static INIT_M_A: MethodImpl = MethodImpl {
    addon: "a",
    model: "test.m",
    method: "init",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: init_m_a_entry,
    file: file!(),
    line: line!(),
};

static INIT_OTHER_A: MethodImpl = MethodImpl {
    addon: "a",
    model: "test.other",
    method: "init",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: init_m_a_entry,
    file: file!(),
    line: line!(),
};

#[tokio::test]
async fn spec_mod_044_init_runs_for_models_extended_by_an_installed_module() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model("a", "test.m"),
            model("a", "test.other"),
            extension("b", "test.m", "b_note"),
        ])
        .impls([&INIT_M_A, &INIT_OTHER_A])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("a")])
            .await
            .unwrap(),
    );
    let mut first = take_log();
    first.sort();
    assert_eq!(first, ["init test.m by a", "init test.other by a"]);
    // b is installed while a is only loaded: M is among b's affected models; the control model
    // test.other, which b does not extend, gets no init()
    let outcome = system
        .button_immediate_install(&env, &[n("b")])
        .await
        .unwrap();
    assert_eq!(take_log(), ["init test.m by a"]);
    let column = rows(
        &outcome.orm,
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_name = 'test_m' AND column_name = 'b_note'",
    )
    .await;
    assert_eq!(column, vec![vec![Some("b_note".to_owned())]]);
    db.cleanup().await.unwrap();
}

// ---- / 041 / 042 / 043: uninstall, reset, loads ------------------------------

fn uninstall_b(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        log("uninstall b");
        Ok(())
    })
}

fn uninstall_d(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        // data of d still present when its hook runs
        let rows = tx.query("SELECT count(*) FROM test_d", &[]).await?;
        let count: i64 = rows[0].get(0);
        log(format!("uninstall d rows={count}"));
        Ok(())
    })
}

static UNINSTALL_B: HookImpl =
    HookImpl::new("b", HookKind::Uninstall, "uninstall_b", uninstall_b).at(file!(), line!());

static UNINSTALL_D: HookImpl =
    HookImpl::new("d", HookKind::Uninstall, "uninstall_d", uninstall_d).at(file!(), line!());

/// the module's external ids; the records the modules created in other models' tables are
/// purged in `data_uninstall_specs.rs`, which carries the whole case)
#[tokio::test]
async fn spec_mod_040_uninstall_order_and_hooks() {
    let addons = Addons::new();
    spec_fixture(&addons);
    addons
        .add(
            "b",
            "name = 'B'\ndepends = ['a']\nuninstall_hook = 'uninstall_b'\n",
        )
        .add(
            "d",
            "name = 'D'\ndepends = ['b', 'c']\nuninstall_hook = 'uninstall_d'\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model("a", "test.a"),
            model("b", "test.b"),
            extension("b", "test.a", "b_field"),
            model("d", "test.d"),
            model("e", "test.e"),
        ])
        .hooks([&UNINSTALL_B, &UNINSTALL_D])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("e")])
            .await
            .unwrap(),
    );
    for module in ["a", "b", "c", "d", "e"] {
        assert_eq!(state(&init.orm, module).await, "installed", "{module}");
    }
    let installed = system.load().await.unwrap().orm;
    let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("INSERT INTO test_d (name) VALUES ('d1')", &[])
        .await
        .unwrap();
    tx.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         VALUES ('d', 'rec_d1', 'test.d', 1, false)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    take_log();

    let outcome = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap();
    assert_eq!(take_log(), ["uninstall d rows=1", "uninstall b"]);
    assert_eq!(outcome.uninstalled, vec![n("b"), n("d"), n("e")]);
    for module in ["b", "d", "e"] {
        assert_eq!(
            rows(
                &outcome.orm,
                &format!(
                    "SELECT state, latest_version FROM ir_module_module WHERE name = '{module}'"
                )
            )
            .await[0],
            vec![Some("uninstalled".to_owned()), None],
            "{module}"
        );
    }
    for module in ["a", "c"] {
        assert_eq!(state(&outcome.orm, module).await, "installed");
    }
    for table in ["test_b", "test_d", "test_e"] {
        assert!(!table_exists(&outcome.orm, table).await, "{table}");
        assert!(
            outcome
                .orm
                .registry()
                .model(&table.replace('_', "."))
                .is_none()
        );
    }
    assert!(table_exists(&outcome.orm, "test_a").await);
    assert!(
        rows(
            &outcome.orm,
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_name = 'test_a' AND column_name = 'b_field'"
        )
        .await
        .is_empty(),
        "b's column on a's model is dropped"
    );
    assert!(
        rows(
            &outcome.orm,
            "SELECT name FROM ir_model_data WHERE module IN ('b', 'd', 'e')"
        )
        .await
        .is_empty()
    );
    db.cleanup().await.unwrap();
}

fn fail_post_init(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { Err(erp_fields::Error::user("post_init of f failed")) })
}

static FAIL_POST_INIT: HookImpl =
    HookImpl::new("f", HookKind::PostInit, "fail", fail_post_init).at(file!(), line!());

/// `b` to install, `c` to upgrade, `r` to remove ('s three transient states).
async fn schedule_three_transient_states(system: &ModuleSystem, env: &Env, orm: &Orm) {
    system
        .button_install(env, &[n("b")], &InstallOptions::new())
        .await
        .unwrap();
    system
        .button_upgrade(env, &[n("c")], &InstallOptions::new())
        .await
        .unwrap();
    system.button_uninstall(env, &[n("r")]).await.unwrap();
    for (module, scheduled) in [("b", "to install"), ("c", "to upgrade"), ("r", "to remove")] {
        assert_eq!(state(orm, module).await, scheduled, "{module}");
    }
}

#[tokio::test]
async fn spec_mod_041_transient_states_reset_after_failure_and_on_request() {
    let addons = Addons::new();
    spec_fixture(&addons);
    addons
        .add("f", "name = 'F'\npost_init_hook = 'fail'\n")
        .add("r", "name = 'R'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&FAIL_POST_INIT])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("c"), n("r")])
            .await
            .unwrap(),
    );
    // schedule without running (scheduling persists): b to install, c to upgrade, r to remove
    schedule_three_transient_states(&system, &env, &init.orm).await;
    // a failing operation resets every transient state
    let err = system
        .button_immediate_install(&env, &[n("f")])
        .await
        .unwrap_err();
    // the error names the module and the step, and keeps the hook's own error (lead review P)
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        (
            "f",
            ModuleStep::Hook {
                kind: HookKind::PostInit,
                name: "fail"
            },
            None
        )
    );
    assert!(matches!(source, Error::Orm(e) if e.kind() == ErrorKind::UserError));
    assert_eq!(
        chain(&err),
        [
            "Module \"f\": post_init_hook 'fail' failed",
            "post_init of f failed"
        ]
    );
    let reset_states = [
        ("b", "uninstalled"),
        ("c", "installed"),
        ("f", "uninstalled"),
        ("r", "installed"),
    ];
    for (module, expected) in reset_states {
        assert_eq!(state(&init.orm, module).await, expected, "{module}");
    }
    // button_reset_state, on the same three transient states
    schedule_three_transient_states(&system, &env, &init.orm).await;
    let mut reset = system.button_reset_state(&env).await.unwrap();
    reset.sort();
    assert_eq!(reset, vec![n("b"), n("c"), n("r")]);
    for (module, expected) in reset_states {
        assert_eq!(state(&init.orm, module).await, expected, "{module}");
    }
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_042_plain_load_forces_pending_operations() {
    let addons = Addons::new();
    spec_fixture(&addons);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.a")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("a")])
            .await
            .unwrap(),
    );
    system
        .button_upgrade(&env, &[n("a")], &InstallOptions::new())
        .await
        .unwrap();
    let loaded = system.load().await.unwrap();
    let forced = loaded.forced_update.expect("a forced update");
    assert_eq!(forced.upgraded, vec![n("a")]);
    assert_eq!(state(&loaded.orm, "a").await, "installed");
    assert!(loaded.orm.registry().model("test.a").is_some());

    // a module `to install` whose manifest is missing forces nothing
    let mut tx = loaded.orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE ir_module_module SET state = 'to install' WHERE name = 'e'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    fs::remove_dir_all(addons.path().join("e")).unwrap();
    system.with_index(AddonIndex::invalidate_cache);
    let loaded = system.load().await.unwrap();
    assert!(loaded.forced_update.is_none());
    assert_eq!(state(&loaded.orm, "e").await, "to install");
    db.cleanup().await.unwrap();
}

static REGISTER_A: Mutex<u32> = Mutex::new(0);
static REGISTER_B: Mutex<u32> = Mutex::new(0);

fn register_a<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        *REGISTER_A.lock().unwrap() += 1;
        log("register a");
        sup.call(tx, records, args).await
    })
}

fn register_b<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        *REGISTER_B.lock().unwrap() += 1;
        log("register b");
        sup.call(tx, records, args).await
    })
}

fn register_a_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = register_a;
    &F
}

fn register_b_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = register_b;
    &F
}

static REGISTER_HOOK_A: MethodImpl = MethodImpl {
    addon: "a",
    model: "test.m",
    method: "_register_hook",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: register_a_entry,
    file: file!(),
    line: line!(),
};

static REGISTER_HOOK_B: MethodImpl = MethodImpl {
    addon: "b",
    model: "test.m",
    method: "_register_hook",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: register_b_entry,
    file: file!(),
    line: line!(),
};

/// on stage registries; the runtime re-setup with `_unregister_hook` comes with per-database
/// overlays)
#[tokio::test]
async fn spec_mod_043_register_hook_once_after_full_load() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.m"), extension("b", "test.m", "b_note")])
        .impls([&REGISTER_HOOK_A, &REGISTER_HOOK_B])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    take_log();
    // one operation installs a and b: two stage registries, one final registry
    committed(
        system
            .button_immediate_install(&su(&init.orm), &[n("b")])
            .await
            .unwrap(),
    );
    assert_eq!(take_log(), ["register b", "register a"]);
    assert_eq!(*REGISTER_A.lock().unwrap(), 1);
    assert_eq!(*REGISTER_B.lock().unwrap(), 1);
    // a plain load builds a registry: once more each
    let loaded = system.load().await.unwrap();
    assert!(loaded.forced_update.is_none());
    assert_eq!(take_log(), ["register b", "register a"]);
    assert_eq!(*REGISTER_A.lock().unwrap(), 2);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_048_database_not_initialized() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let err = system.load().await.unwrap_err();
    assert!(matches!(&err, Error::DatabaseNotInitialized { database } if database == db.name()));
    assert_eq!(
        err.to_string(),
        format!(
            "Database {} not initialized: initialize it with ModuleSystem::initialize (create_database makes the empty database); `erp module` never initializes one",
            db.name()
        )
    );
    db.cleanup().await.unwrap();
}

// ---- / 047 / 049: metadata models ---------------------------------------------

#[tokio::test]
async fn spec_mod_046_update_list_counts_and_syncs_rows() {
    let addons = Addons::new();
    spec_fixture(&addons);
    addons.add("x", "name = 'X'\nversion = '1.0'\ndepends = ['a', 'c']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("x")])
            .await
            .unwrap(),
    );
    addons
        .add("n", "name = 'N'\n")
        .add("z", "name = 'Z'\n")
        .add(
            "x",
            "name = 'X'\nsummary = 'new'\nversion = '1.1'\ndepends = ['a', 'z']\n",
        );
    system.with_index(AddonIndex::invalidate_cache);
    let result = system.update_list(&env).await.unwrap();
    assert_eq!((result.upgradable, result.added), (1, 2));
    assert_eq!(state(&init.orm, "n").await, "uninstalled");
    assert_eq!(state(&init.orm, "x").await, "installed");
    assert_eq!(
        rows(
            &init.orm,
            "SELECT d.name FROM ir_module_module_dependency d JOIN ir_module_module m \
             ON m.id = d.module_id WHERE m.name = 'x' ORDER BY d.name"
        )
        .await,
        vec![vec![Some("a".to_owned())], vec![Some("z".to_owned())]]
    );
    assert_eq!(
        rows(
            &init.orm,
            "SELECT summary FROM ir_module_module WHERE name = 'x'"
        )
        .await[0][0]
            .as_deref(),
        Some("new")
    );
    // repeated: nothing more to add
    let again = system.update_list(&env).await.unwrap();
    assert_eq!((again.upgradable, again.added), (1, 0));
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_047_installed_module_row_cannot_be_removed() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n").add("u", "name = 'U'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    committed(
        system
            .button_immediate_install(&su(&init.orm), &[n("a")])
            .await
            .unwrap(),
    );
    let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
    let modules = tx.env().records(base_models::MODULE).unwrap();
    let ids: Vec<i64> = rows(
        &init.orm,
        "SELECT id::text FROM ir_module_module WHERE name IN ('a', 'u') ORDER BY name",
    )
    .await
    .into_iter()
    .map(|r| r[0].as_deref().unwrap().parse().unwrap())
    .collect();
    let err = modules
        .browse(ids.clone())
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(err.message(), base_models::UNLINK_INSTALLED_MESSAGE);
    assert_eq!(
        err.message(),
        "You are trying to remove a module that is installed or will be installed."
    );
    tx.rollback().await.unwrap();
    // an uninstalled row can be removed
    modules
        .browse([ids[1]])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        rows(
            &init.orm,
            "SELECT name FROM ir_module_module WHERE name = 'u'"
        )
        .await
        .is_empty()
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_049_module_category_parent_cycles() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
    let mut ids = Vec::new();
    for (name, parent) in [("A", None), ("B", Some(0usize)), ("C", Some(1))] {
        let parent_id: Option<i64> = parent.map(|i| ids[i]);
        let rows = tx
            .query(
                "INSERT INTO ir_module_category (name, parent_id) VALUES ($1, $2) RETURNING id",
                &[&name, &parent_id],
            )
            .await
            .unwrap();
        ids.push(rows[0].get::<_, i64>(0));
    }
    tx.commit().await.unwrap();
    let categories = tx.env().records(base_models::CATEGORY).unwrap();
    for (record, parent) in [(ids[0], ids[2]), (ids[1], ids[1])] {
        let rec = categories.browse([record]).unwrap();
        let err = rec
            .write(
                &mut tx,
                erp_orm::RawVals::new().set_cell("parent_id", erp_fields::Cell::Id(parent)),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "Error ! You cannot create recursive categories."
        );
        tx.rollback().await.unwrap();
    }
    // a valid reparenting passes
    let c = categories.browse([ids[2]]).unwrap();
    c.write(
        &mut tx,
        erp_orm::RawVals::new().set_cell("parent_id", erp_fields::Cell::Id(ids[0])),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    db.cleanup().await.unwrap();
}

// ---- hooks pairing and post-commit -------------------------------------------------------

fn noop_hook(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async { Ok(()) })
}

static ORPHAN: HookImpl =
    HookImpl::new("a", HookKind::PostInit, "not_named", noop_hook).at("orphan.rs", 7);

/// Without `.at(file!(), line!())`: errors say the location is unknown.
static ORPHAN_NO_AT: HookImpl = HookImpl::new("a", HookKind::PostInit, "not_named", noop_hook);

static DUP_1: HookImpl =
    HookImpl::new("a", HookKind::PreInit, "prepare", noop_hook).at("one.rs", 1);

static DUP_2: HookImpl =
    HookImpl::new("a", HookKind::PreInit, "prepare", noop_hook).at("two.rs", 2);

/// Hooks named in manifests pair with exactly one implementation.
#[tokio::test]
async fn hooks_pair_with_manifests() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("a", "name = 'A'\npre_init_hook = 'prepare'\n");
    let err = builder(&db, &addons).build().err().unwrap();
    assert_eq!(
        err.to_string(),
        "Module 'a' names pre_init_hook 'prepare' in manifest.toml, but no #[hook] implementation of it is linked"
    );
    let err = builder(&db, &addons)
        .hooks([&DUP_1, &DUP_2])
        .build()
        .err()
        .unwrap();
    assert_eq!(
        err.to_string(),
        "Module 'a': pre_init_hook 'prepare' is implemented twice: one.rs:1 and two.rs:2"
    );
    let err = builder(&db, &addons)
        .hooks([&DUP_1, &ORPHAN])
        .build()
        .err()
        .unwrap();
    assert_eq!(
        err.to_string(),
        "Module 'a': the post_init_hook implementation 'not_named' at orphan.rs:7 is not named in manifest.toml"
    );
    let err = builder(&db, &addons)
        .hooks([&DUP_1, &ORPHAN_NO_AT])
        .build()
        .err()
        .unwrap();
    assert!(matches!(&err, Error::HookOrphan { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "Module 'a': the post_init_hook implementation 'not_named' at unknown location \
         (HookImpl::at was not called for a's not_named) is not named in manifest.toml"
    );
    assert!(builder(&db, &addons).hooks([&DUP_1]).build().is_ok());
    db.cleanup().await.unwrap();
}

fn schedule_steps(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let p = ModuleName::new("p").unwrap();
        let index = erp_modules::PostCommitAction::create_index(
            erp_modules::CreateIndex::new(
                erp_modules::TableName::parse("test_p").unwrap(),
                erp_modules::IndexName::parse("test_p_name_idx").unwrap(),
                vec![erp_modules::IndexColumn::new("name").unwrap()],
            )
            .unwrap(),
        );
        schedule_post_commit(tx, &p, &pk("name_index"), &index).await?;
        schedule_post_commit(tx, &p, &pk("broken"), &analyze("no_such_table")).await
    })
}

static SCHEDULE_STEPS: HookImpl =
    HookImpl::new("p", HookKind::PostInit, "steps", schedule_steps).at(file!(), line!());

/// The post-commit phase runs scheduled steps after commit, each on its own; failures are
/// reported and stay `failed`.
#[tokio::test]
async fn post_commit_steps_run_after_commit() {
    let addons = Addons::new();
    addons.add("p", "name = 'P'\npost_init_hook = 'steps'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("p", "test.p")])
        .hooks([&SCHEDULE_STEPS])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("p")])
        .await
        .unwrap();
    let done: Vec<(&str, &str)> = outcome
        .post_commit
        .done
        .iter()
        .map(|s| (s.module.as_str(), s.key.as_str()))
        .collect();
    assert_eq!(done, [("p", "name_index")]);
    assert_eq!(outcome.post_commit.failed.len(), 1);
    let failure = &outcome.post_commit.failed[0];
    assert_eq!(
        (failure.step.module.as_str(), failure.step.key.as_str()),
        ("p", "broken")
    );
    assert_eq!(
        failure.error,
        "relation \"public.no_such_table\" does not exist"
    );
    assert_eq!(failure.sqlstate.as_deref(), Some("42P01"));
    assert!(outcome.post_commit.stopped.is_none());
    assert!(outcome.post_commit.superseded.is_empty());
    assert!(table_exists(&outcome.orm, "test_p_name_idx").await);
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT key, state FROM ir_module_post_commit ORDER BY id"
        )
        .await,
        vec![
            vec![Some("name_index".to_owned()), Some("done".to_owned())],
            vec![Some("broken".to_owned()), Some("failed".to_owned())],
        ]
    );
    db.cleanup().await.unwrap();
}

/// Upgrading and reinstalling go through the same runner; the scheduling errors surface with
/// their kind and message and change nothing (through the database).
#[tokio::test]
async fn scheduling_errors_through_the_database() {
    let addons = Addons::new();
    addons.add("x", "name = 'X'\ndepends = ['ghost']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    let err = system
        .button_immediate_install(&env, &[n("x")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::UnknownDependency { module, dependency }
            if module.as_str() == "x" && dependency.as_str() == "ghost"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "You try to install module \"x\" that depends on module \"ghost\".\nBut the latter module is not available in your system."
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    let err = system
        .button_immediate_upgrade(&env, &[n("x")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::UpgradeNotInstalled { module } if module.as_str() == "x"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Cannot upgrade module \u{201c}x\u{201d}. It is not installed."
    );
    db.cleanup().await.unwrap();
}

/// `button_upgrade` with a new dependency, through the database: an installed module whose manifest gains a dependency on an
/// uninstalled module is upgraded with its dependents and the new dependency installed, in one
/// transaction (the dependency joins the registry before the module that needs it,'s
/// corner case).
#[tokio::test]
async fn upgrade_with_a_new_dependency_installs_it_first() {
    let addons = Addons::new();
    spec_fixture(&addons);
    addons.add("n", "name = 'N'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("n", "test.n"), model("b", "test.b")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("e")])
            .await
            .unwrap(),
    );
    // A new version of b depends on n and extends n's model: code and manifest change together
    // (a new binary, so a new module system). b's code is not composable without n.
    addons.add("b", "name = 'B'\ndepends = ['a', 'n']\n");
    let system = builder(&db, &addons)
        .decls([
            model("n", "test.n"),
            model("b", "test.b"),
            extension("b", "test.n", "b_on_n"),
        ])
        .build()
        .unwrap();
    enroll_fixture_transition(&system, &["b"]).await;
    let env = su_of(&system);
    let outcome = system
        .button_immediate_upgrade(&env, &[n("b")])
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("n")]);
    let mut upgraded = outcome.upgraded.clone();
    upgraded.sort();
    assert_eq!(upgraded, vec![n("b"), n("d"), n("e")]);
    for module in ["a", "b", "c", "d", "e", "n"] {
        assert_eq!(state(&outcome.orm, module).await, "installed", "{module}");
    }
    assert!(
        outcome
            .orm
            .registry()
            .model("test.n")
            .unwrap()
            .field("b_on_n")
            .is_some()
    );
    assert!(outcome.dropped.is_empty());
    let column = rows(
        &outcome.orm,
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_name = 'test_n' AND column_name = 'b_on_n'",
    )
    .await;
    assert_eq!(column, vec![vec![Some("b_on_n".to_owned())]]);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn refused_request_keeps_earlier_scheduling() {
    let addons = Addons::new();
    spec_fixture(&addons);
    addons.add("x", "name = 'X'\ndepends = ['ghost']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    system
        .button_install(&env, &[n("a")], &InstallOptions::new())
        .await
        .unwrap();
    let err = system
        .button_immediate_install(&env, &[n("x")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::UnknownDependency { module, dependency }
            if module.as_str() == "x" && dependency.as_str() == "ghost"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "You try to install module \"x\" that depends on module \"ghost\".\nBut the latter module is not available in your system."
    );
    assert_eq!(state(&init.orm, "a").await, "to install");
    db.cleanup().await.unwrap();
}

fn pool_for(config: tokio_postgres::Config) -> deadpool_postgres::Pool {
    let manager = deadpool_postgres::Manager::from_config(
        config,
        tokio_postgres::NoTls,
        deadpool_postgres::ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Fast,
        },
    );
    deadpool_postgres::Pool::builder(manager)
        .max_size(4)
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .build()
        .unwrap()
}

async fn drop_database(maintenance: &deadpool_postgres::Pool, name: &str) {
    let client = maintenance.get().await.unwrap();
    client
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
        .await
        .unwrap();
}

/// installed, an existing name refused; the admin password and language wait for passwords on
/// res.users and res.lang in M3)
#[tokio::test]
async fn spec_mod_050_database_creation_helper() {
    let url = erp_testkit::database_url_from_env().unwrap();
    let admin: tokio_postgres::Config = url.parse().unwrap();
    let maintenance = pool_for(admin.clone());

    let too_long = "a".repeat(64);
    let bad_names: [(&str, String); 5] = [
        (
            "Bad Name!",
            "' ' is not allowed: use a-z, A-Z, 0-9, '_', '.', '-'".to_owned(),
        ),
        ("x", "the name needs at least two characters".to_owned()),
        (
            "_x",
            "the first character must be a letter or a digit (a-z, A-Z, 0-9)".to_owned(),
        ),
        (
            "a/b",
            "'/' is not allowed: use a-z, A-Z, 0-9, '_', '.', '-'".to_owned(),
        ),
        (
            too_long.as_str(),
            "the name is 64 bytes; PostgreSQL keeps 63 and would truncate the rest".to_owned(),
        ),
    ];
    for (bad, reason) in &bad_names {
        let err = DatabaseName::parse(bad).unwrap_err();
        assert!(
            matches!(&err, Error::InvalidDatabaseName { name, reason: r } if name == bad && r == reason),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("Invalid db_name='{bad}': {reason}")
        );
        assert!(matches!(
            validate_database_name(bad),
            Err(Error::InvalidDatabaseName { .. })
        ));
    }
    let empty = DatabaseName::parse("").unwrap_err();
    assert_eq!(empty.to_string(), "Invalid db_name='': the name is empty");
    validate_database_name(&"a".repeat(63)).unwrap();

    let name = format!(
        "m2s1_newdb_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let db_name = DatabaseName::parse(&name).unwrap();
    create_database(&maintenance, &db_name).await.unwrap();
    let err = create_database(&maintenance, &db_name).await.unwrap_err();
    assert!(matches!(&err, Error::DatabaseExists { name: n } if *n == db_name));
    assert_eq!(err.to_string(), format!("Database {name} already exists"));

    let mut config = admin.clone();
    config.dbname(&name);
    let pool = pool_for(config);
    let addons = Addons::new();
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .build()
        .unwrap();
    let outcome = system.initialize(InitOptions::new()).await.unwrap();
    assert_eq!(state(&outcome.orm, "base").await, "installed");
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT extname::text FROM pg_extension WHERE extname = 'pg_trgm'"
        )
        .await,
        vec![vec![Some("pg_trgm".to_owned())]]
    );
    // S12: created in the schema of the ORM's tables
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT (extnamespace::regnamespace::text = current_schema())::text \
             FROM pg_extension WHERE extname = 'pg_trgm'"
        )
        .await,
        vec![vec![Some("true".to_owned())]]
    );
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT datcollate::text FROM pg_database WHERE datname = current_database()"
        )
        .await,
        vec![vec![Some("C".to_owned())]]
    );
    drop(outcome);
    drop(system);
    pool.close();
    drop_database(&maintenance, &name).await;
}

fn fill_shelves(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.execute("UPDATE library_book SET name = 'A' WHERE name IS NULL", &[])
            .await?;
        log("fill_shelves");
        Ok(())
    })
}

/// The form: a hook linked into `HOOKS`.
#[erp_modules::hooks::linkme::distributed_slice(erp_modules::hooks::HOOKS)]
#[linkme(crate = erp_modules::hooks::linkme)]
static FILL_SHELVES: HookImpl =
    HookImpl::new("library", HookKind::PostInit, "fill_shelves", fill_shelves).at(file!(), line!());

/// Hooks linked into `HOOKS` are used unless the builder turns them off.
#[tokio::test]
async fn linked_hooks_run() {
    let addons = Addons::new();
    addons.add(
        "library",
        "name = 'Library'\npost_init_hook = 'fill_shelves'\n",
    );
    let db = TestDb::create().await.unwrap();
    let err = builder(&db, &addons).build().err().unwrap();
    assert!(
        matches!(&err, Error::HookMissing { module, kind: HookKind::PostInit, name }
            if module.as_str() == "library" && name == "fill_shelves"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module 'library' names post_init_hook 'fill_shelves' in manifest.toml, but no #[hook] implementation of it is linked"
    );
    let system = ModuleSystem::builder(db.pool().clone(), addons.index())
        .decls([model("library", "library.book")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    take_log();
    committed(
        system
            .button_immediate_install(&su(&init.orm), &[n("library")])
            .await
            .unwrap(),
    );
    assert_eq!(take_log(), ["fill_shelves"]);
    db.cleanup().await.unwrap();
}

// ---- review fixes -------------------------------------------------------------------

/// Hold the module-operation lock in another session (another operation in progress).
async fn hold_operation_lock(orm: &Orm) -> Transaction {
    let mut other = orm.begin(SUPERUSER_ID).await.unwrap();
    other
        .query(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await
        .unwrap();
    other
}

fn assert_in_progress(err: &Error) {
    assert!(matches!(err, Error::OperationInProgress), "{err:?}");
    assert_eq!(
        err.to_string(),
        "ERPForAll is currently processing another module operation.\nPlease try again later or contact your system administrator."
    );
}

/// Review H2: scheduling methods take the operation lock, so they cannot overwrite the states of
/// a running operation.
#[tokio::test]
async fn scheduling_is_refused_while_an_operation_runs() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n").add("z", "name = 'Z'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    let mut other = hold_operation_lock(&init.orm).await;
    assert_in_progress(
        &system
            .button_install(&env, &[n("a")], &InstallOptions::new())
            .await
            .unwrap_err(),
    );
    assert_in_progress(&system.button_reset_state(&env).await.unwrap_err());
    // a newly discovered addon is not inserted behind the operation's back either
    addons.add("y", "name = 'Y'\n");
    system.with_index(AddonIndex::invalidate_cache);
    assert_in_progress(&system.update_list(&env).await.unwrap_err());
    other.rollback().await.unwrap();
    drop(other);
    assert_eq!(state(&init.orm, "a").await, "uninstalled");
    system
        .button_install(&env, &[n("a")], &InstallOptions::new())
        .await
        .unwrap();
    assert_eq!(state(&init.orm, "a").await, "to install");
    db.cleanup().await.unwrap();
}

static HOLDER_POOL: std::sync::OnceLock<deadpool_postgres::Pool> = std::sync::OnceLock::new();
static HELD: Mutex<Option<deadpool_postgres::Object>> = Mutex::new(None);

/// Queue another session on the operation lock, wait until it waits, then fail: the lock passes
/// to that session when this operation rolls back, before the reset runs.
fn queue_operation_then_fail(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let pool = HOLDER_POOL.get().unwrap().clone();
        tokio::spawn(async move {
            let client = pool.get().await.unwrap();
            client.batch_execute("BEGIN").await.unwrap();
            client
                .query(
                    "SELECT pg_advisory_xact_lock($1, $2)",
                    &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
                )
                .await
                .unwrap();
            *HELD.lock().unwrap() = Some(client);
        });
        loop {
            let rows = tx
                .query(
                    "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
                     AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
                    &[],
                )
                .await?;
            if rows[0].get::<_, i64>(0) > 0 {
                break;
            }
            tx.query("SELECT pg_sleep(0.02)", &[]).await?;
        }
        Err(erp_fields::Error::user("post_init of g failed"))
    })
}

static QUEUE_THEN_FAIL: HookImpl = HookImpl::new(
    "g",
    HookKind::PostInit,
    "queue_then_fail",
    queue_operation_then_fail,
)
.at(file!(), line!());

/// Review H2: the reset after a failed operation takes the operation lock too; when another
/// operation got it first, the reset is refused (both errors returned) instead of wiping the
/// other operation's states.
#[tokio::test]
async fn reset_after_failure_does_not_run_under_another_operation() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("g", "name = 'G'\npost_init_hook = 'queue_then_fail'\n");
    let db = TestDb::create().await.unwrap();
    HOLDER_POOL.set(db.pool().clone()).unwrap();
    let system = builder(&db, &addons)
        .hooks([&QUEUE_THEN_FAIL])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    system
        .button_install(&env, &[n("a")], &InstallOptions::new())
        .await
        .unwrap();
    let err = system
        .button_immediate_install(&env, &[n("g")])
        .await
        .unwrap_err();
    let Error::ResetFailed { error, reset } = &err else {
        panic!("expected ResetFailed, got {err:?}");
    };
    let (module, step, _, source) = step_failed(error);
    assert_eq!(
        (module, step),
        (
            "g",
            ModuleStep::Hook {
                kind: HookKind::PostInit,
                name: "queue_then_fail"
            }
        )
    );
    assert!(matches!(source, Error::Orm(e) if e.kind() == ErrorKind::UserError));
    assert_in_progress(reset);
    assert_eq!(
        chain(&err),
        [
            "A module operation failed, and resetting transient module states afterwards failed \
             too: ERPForAll is currently processing another module operation.\nPlease try again \
             later or contact your system administrator.",
            "Module \"g\": post_init_hook 'queue_then_fail' failed",
            "post_init of g failed",
        ]
    );
    // the other operation's view is untouched: `a` is still scheduled
    assert_eq!(state(&init.orm, "a").await, "to install");
    let held = HELD.lock().unwrap().take().unwrap();
    held.batch_execute("ROLLBACK").await.unwrap();
    drop(held);
    assert_eq!(system.button_reset_state(&env).await.unwrap(), vec![n("a")]);
    db.cleanup().await.unwrap();
}

/// Review H4: in an update run, a scheduled module whose source vanished is an error (it would
/// otherwise stay scheduled while the operation reports success).
#[tokio::test]
async fn scheduled_module_without_source_fails_the_update() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n").add("z", "name = 'Z'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("a")])
            .await
            .unwrap(),
    );
    system
        .button_upgrade(&env, &[n("a")], &InstallOptions::new())
        .await
        .unwrap();
    fs::remove_dir_all(addons.path().join("a")).unwrap();
    system.with_index(AddonIndex::invalidate_cache);
    let err = system
        .button_immediate_install(&env, &[n("z")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::DroppedUpdateModules { modules } if modules.len() == 1),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Modules marked to install, upgrade or remove cannot be loaded:\n  - a: unavailable (in no addon source)"
    );
    assert_eq!(state(&init.orm, "a").await, "installed", "reset");
    assert_eq!(state(&init.orm, "z").await, "uninstalled");
    db.cleanup().await.unwrap();
}

/// Review H5: uninstalling a module that owns records in a model that stays installed is refused
/// when no data loader purges them (deleting only their external ids would orphan them). With the
/// default loader the purge deletes them (`data_uninstall_specs.rs`).
#[tokio::test]
async fn uninstall_refused_while_records_in_surviving_models_are_owned() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.a"), model("b", "test.b")])
        .no_data_loader()
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("b")])
            .await
            .unwrap(),
    );
    let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch(
        "INSERT INTO test_a (id, name) VALUES (1, 'made by b'); \
         INSERT INTO test_b (id, name) VALUES (1, 'own'); \
         INSERT INTO ir_model_data (module, name, model, res_id, noupdate) VALUES \
         ('b', 'rec_a', 'test.a', 1, false), ('b', 'rec_b', 'test.b', 1, false)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let err = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::UninstallOwnedRecords { module, .. } if module.as_str() == "b"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module \"b\" cannot be uninstalled yet: it owns records of models that stay installed \
         (test.a: 1); no data loader is installed to remove them"
    );
    assert_eq!(state(&init.orm, "b").await, "installed");
    assert!(table_exists(&init.orm, "test_b").await);
    assert_eq!(
        rows(
            &init.orm,
            "SELECT name FROM ir_model_data WHERE module = 'b' ORDER BY name"
        )
        .await,
        vec![
            vec![Some("rec_a".to_owned())],
            vec![Some("rec_b".to_owned())]
        ]
    );
    // once the record and its external id are gone, the uninstall runs
    let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("DELETE FROM test_a; DELETE FROM ir_model_data WHERE name = 'rec_a'")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let outcome = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap();
    assert_eq!(outcome.uninstalled, vec![n("b")]);
    assert!(!table_exists(&outcome.orm, "test_b").await);
    db.cleanup().await.unwrap();
}

fn failing_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let p = ModuleName::new("p").unwrap();
        schedule_post_commit(tx, &p, &pk("broken"), &analyze("no_such_table")).await
    })
}

static FAILING_STEP: HookImpl =
    HookImpl::new("p", HookKind::PostInit, "steps", failing_step).at(file!(), line!());

/// Review M10: uninstalling a module removes its post-commit steps (a failed step of a removed
/// module would otherwise run after every operation forever).
#[tokio::test]
async fn uninstall_removes_the_module_post_commit_steps() {
    let addons = Addons::new();
    addons.add("p", "name = 'P'\npost_init_hook = 'steps'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&FAILING_STEP])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    let outcome = system
        .button_immediate_install(&env, &[n("p")])
        .await
        .unwrap();
    assert_eq!(outcome.post_commit.failed.len(), 1);
    let outcome = system
        .button_immediate_uninstall(&env, &[n("p")])
        .await
        .unwrap();
    assert!(outcome.post_commit.failed.is_empty());
    assert!(
        rows(&outcome.orm, "SELECT key FROM ir_module_post_commit")
            .await
            .is_empty()
    );
    db.cleanup().await.unwrap();
}

/// Review M11: `ir.module.post_commit` holds executable SQL; the ORM refuses to write it for
/// everyone (the superuser included): only the module system adds steps.
#[tokio::test]
async fn post_commit_rows_are_not_writable_through_the_orm() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let message = "ir.module.post_commit is written only by the module system \
                   (erp_modules::schedule_post_commit)";
    let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
    let steps = tx.env().records("ir.module.post_commit").unwrap();
    let vals = std::collections::BTreeMap::from([
        ("module".to_owned(), erp_fields::Value::str("x")),
        ("key".to_owned(), erp_fields::Value::str("k")),
        ("kind".to_owned(), erp_fields::Value::str("analyze")),
        (
            "table_name".to_owned(),
            erp_fields::Value::str("ir_module_module"),
        ),
    ]);
    let err = steps.create_values(&mut tx, &[vals]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), message);
    tx.rollback().await.unwrap();
    // rows written by the module system cannot be changed or removed through the ORM either
    let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
    let control = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    schedule_post_commit(&mut tx, &n("base"), &pk("k"), &analyze("ir_module_module"))
        .await
        .unwrap();
    let rows = tx
        .query("SELECT id FROM ir_module_post_commit", &[])
        .await
        .unwrap();
    let id: i64 = rows[0].get(0);
    let step = steps.browse([id]).unwrap();
    let vals = std::collections::BTreeMap::from([(
        "table_name".to_owned(),
        erp_fields::Value::str("res_users"),
    )]);
    let err = step.write_values(&mut tx, &vals).await.unwrap_err();
    assert_eq!((err.kind(), err.message()), (ErrorKind::Access, message));
    let err = step.unlink(&mut tx).await.unwrap_err();
    assert_eq!((err.kind(), err.message()), (ErrorKind::Access, message));
    tx.operation_rollback(&control).await.unwrap();
    drop(tx);
    db.cleanup().await.unwrap();
}

/// A loader that drops parse errors instead of returning them (a broken adapter).
struct SwallowingLoader;

impl FileLoader for SwallowingLoader {
    /// Installs only: these tests uninstall nothing.
    fn purge<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _modules: &'a [ModuleName],
    ) -> BoxFuture<'a, erp_modules::Result<Vec<erp_data::load::DataNotice>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn load<'a, 'o: 'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            for op in operations {
                if op.is_err() {
                    continue;
                }
            }
            Ok(())
        })
    }
}

/// A loader that reads the first operation only and reports success.
struct EarlyStopLoader;

impl FileLoader for EarlyStopLoader {
    /// Installs only: these tests uninstall nothing.
    fn purge<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _modules: &'a [ModuleName],
    ) -> BoxFuture<'a, erp_modules::Result<Vec<erp_data::load::DataNotice>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn load<'a, 'o: 'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            if let Some(op) = operations.next() {
                op?;
            }
            Ok(())
        })
    }
}

/// Review M12: the runner checks the loader's work: a parse error the loader did not return still
/// fails the operation, and so does a loader that stopped before the end of the file.
#[tokio::test]
async fn data_loader_cannot_swallow_errors_or_stop_early() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/broken.xml']\n")
        .file(
            "x",
            "data/broken.xml",
            "<odoo><record id=\"x\" model=\"test.x\"><field name=\"name\">x</field></odoo>",
        )
        .add("y", "name = 'Y'\ndata = ['data/two.xml']\n")
        .file(
            "y",
            "data/two.xml",
            "<odoo><record id=\"r1\" model=\"test.x\"><field name=\"name\">one</field></record>\
             <record id=\"r2\" model=\"test.x\"><field name=\"name\">two</field></record></odoo>",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .data_loader(Arc::new(SwallowingLoader))
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    let err = system
        .button_immediate_install(&env, &[n("x")])
        .await
        .unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("x", ModuleStep::DataFile, Some("data/broken.xml"))
    );
    let Error::DataParse(parse) = source else {
        panic!("expected the parse error, got {source:?}");
    };
    assert_eq!(parse.file(), "data/broken.xml");
    assert_eq!(
        chain(&err)[..2],
        [
            "Module \"x\": data file 'data/broken.xml' failed",
            BROKEN_XML_ERROR
        ]
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");

    let system = builder(&db, &addons)
        .data_loader(Arc::new(EarlyStopLoader))
        .build()
        .unwrap();
    let env = su_of(&system);
    let err = system
        .button_immediate_install(&env, &[n("y")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::DataLoaderStoppedEarly { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module \"y\": the data loader returned before the end of data file 'data/two.xml'"
    );
    assert_eq!(state(&init.orm, "y").await, "uninstalled");
    db.cleanup().await.unwrap();
}

/// Review M13: stored rows the catalog cannot read exactly are errors naming the row (no
/// `false` for a NULL whose default is `true`, no bare PostgreSQL error for a malformed column).
#[tokio::test]
async fn malformed_module_rows_are_errors_naming_the_row() {
    let addons = Addons::new();
    addons.add("x", "name = 'X'\ndepends = ['base']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    exec(
        &init.orm,
        "UPDATE ir_module_module_dependency SET auto_install_required = NULL \
         WHERE module_id = (SELECT id FROM ir_module_module WHERE name = 'x')",
    )
    .await;
    let err = system
        .button_install(&env, &[n("x")], &InstallOptions::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Orm(e) if e.kind() == ErrorKind::Programming),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "inconsistent ir_module_module row 'x': dependency 'base' has no auto_install_required"
    );
    exec(
        &init.orm,
        "UPDATE ir_module_module_dependency SET auto_install_required = true; \
         UPDATE ir_module_module SET country_codes = '\"BE\"'::jsonb WHERE name = 'x'",
    )
    .await;
    let err = system
        .button_install(&env, &[n("x")], &InstallOptions::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Orm(e) if e.kind() == ErrorKind::Programming),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "inconsistent ir_module_module row 'x': country_codes is not a JSON array of strings: \"BE\""
    );
    // a code that is not two letters
    exec(
        &init.orm,
        "UPDATE ir_module_module SET country_codes = '[\"BEL\"]'::jsonb WHERE name = 'x'",
    )
    .await;
    let err = system
        .button_install(&env, &[n("x")], &InstallOptions::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Orm(e) if e.kind() == ErrorKind::Programming),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "inconsistent ir_module_module row 'x': country_codes: Invalid country code 'BEL': expected two ASCII letters (ISO 3166-1 alpha-2)"
    );
    exec(
        &init.orm,
        "UPDATE ir_module_module SET country_codes = '[]'::jsonb, latest_version = '1.0.0' \
         WHERE name = 'x'",
    )
    .await;
    let err = system
        .button_install(&env, &[n("x")], &InstallOptions::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Orm(e) if e.kind() == ErrorKind::Programming),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "inconsistent ir_module_module row 'x': Module \"x\": inconsistent stored row: state is 'uninstalled' but an installed version is stored"
    );
    db.cleanup().await.unwrap();
}

/// Review M13: a category external id whose record was deleted is an error, not a dangling
/// reference.
#[tokio::test]
async fn category_external_id_of_a_deleted_category_is_an_error() {
    let addons = Addons::new();
    addons.add("z", "name = 'Z'\ncategory = 'Zeta'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    addons.add("z", "name = 'Z'\ncategory = 'Other'\n");
    system.with_index(AddonIndex::invalidate_cache);
    system.update_list(&env).await.unwrap();
    exec(
        &init.orm,
        "DELETE FROM ir_module_category WHERE name = 'Zeta'",
    )
    .await;
    addons.add("w", "name = 'W'\ncategory = 'Zeta'\n");
    system.with_index(AddonIndex::invalidate_cache);
    let err = system.update_list(&env).await.unwrap_err();
    assert!(
        matches!(&err, Error::Orm(e) if e.kind() == ErrorKind::Programming),
        "{err:?}"
    );
    let id = rows(
        &init.orm,
        "SELECT res_id::text FROM ir_model_data WHERE name = 'module_category_zeta'",
    )
    .await[0][0]
        .clone()
        .unwrap();
    assert_eq!(
        err.to_string(),
        format!(
            "external id base.module_category_zeta points at ir.module.category {id}, which does not exist"
        )
    );
    db.cleanup().await.unwrap();
}

fn remove_own_row(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.execute("DELETE FROM ir_module_module WHERE name = 'h'", &[])
            .await?;
        Ok(())
    })
}

static REMOVE_OWN_ROW: HookImpl =
    HookImpl::new("h", HookKind::PostInit, "remove_own_row", remove_own_row).at(file!(), line!());

/// Review M14: writing the catalog checks the rows written; a row removed behind the module
/// system's back is an error, not a silent no-op.
#[tokio::test]
async fn catalog_save_checks_row_counts() {
    let addons = Addons::new();
    addons.add("h", "name = 'H'\npost_init_hook = 'remove_own_row'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&REMOVE_OWN_ROW])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("h")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Orm(e) if e.kind() == ErrorKind::Programming),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "ir_module_module: 1 row(s) to update, 0 updated (a row was changed or removed during the operation)"
    );
    assert_eq!(state(&init.orm, "h").await, "uninstalled");
    db.cleanup().await.unwrap();
}

/// Review M15: a data file that is a symbolic link (or under one) is refused: data files are
/// regular files inside the addon.
#[tokio::test]
async fn symlinked_data_files_are_refused() {
    let addons = Addons::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("a.xml"), ONE_RECORD_XML).unwrap();
    addons
        .add("x", "name = 'X'\ndata = ['data/a.xml']\n")
        .file("x", "data/a.xml", ONE_RECORD_XML);
    addons
        .add("y", "name = 'Y'\ndata = ['linked/a.xml']\n")
        .file("y", "linked/a.xml", ONE_RECORD_XML);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    take_log();
    // Inject each link after valid initialization and restore it before the next case,
    // so each install tests its own linked source rather than an earlier invalid addon.
    for (module, directory) in [("x", false), ("y", true)] {
        let link = addons
            .path()
            .join(module)
            .join(if directory { "linked" } else { "data/a.xml" });
        if directory {
            fs::remove_dir_all(&link).unwrap();
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        } else {
            fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(outside.path().join("a.xml"), &link).unwrap();
        }
        let err = system
            .button_immediate_install(&su(&init.orm), &[n(module)])
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::NodeDigest(erp_orm::node::NodeError::AddonSource { addon, message })
                if addon == module && message == "symbolic link in artifact path"),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("addon {module}: symbolic link in artifact path")
        );
        assert_eq!(state(&init.orm, module).await, "uninstalled");
        fs::remove_file(&link).unwrap();
        if directory {
            fs::create_dir_all(&link).unwrap();
            fs::write(link.join("a.xml"), ONE_RECORD_XML).unwrap();
        } else {
            fs::write(&link, ONE_RECORD_XML).unwrap();
        }
    }
    assert!(take_log().is_empty(), "nothing reached the loader");
    db.cleanup().await.unwrap();
}

/// S9: a data file that is a hard link (a second name for a file that may live outside the
/// addon) is refused; a file with one link is read.
#[tokio::test]
async fn hard_linked_data_files_are_refused() {
    let addons = Addons::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("a.xml"), ONE_RECORD_XML).unwrap();
    addons.add("x", "name = 'X'\ndata = ['data/a.xml']\n");
    fs::create_dir_all(addons.path().join("x/data")).unwrap();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    fs::hard_link(
        outside.path().join("a.xml"),
        addons.path().join("x/data/a.xml"),
    )
    .unwrap();
    take_log();
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::NodeDigest(erp_orm::node::NodeError::AddonSource { addon, message })
            if addon == "x" && message == "artifact file has multiple hard links"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "addon x: artifact file has multiple hard links"
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    assert!(take_log().is_empty(), "nothing reached the loader");
    db.cleanup().await.unwrap();
}

/// S11: the number of data files per module and the bytes of data files per operation are
/// bounded, can only be lowered, and a module over them fails its operation naming the limit.
#[tokio::test]
async fn data_file_counts_and_operation_bytes_are_bounded() {
    let addons = Addons::new();
    addons
        .add(
            "x",
            "name = 'X'\ndata = ['data/a.xml', 'data/b.xml', 'data/c.xml']\n",
        )
        .file("x", "data/a.xml", ONE_RECORD_XML)
        .file("x", "data/b.xml", ONE_RECORD_XML)
        .file("x", "data/c.xml", ONE_RECORD_XML);
    let db = TestDb::create().await.unwrap();
    for value in [0_usize, 513] {
        let err = builder(&db, &addons)
            .max_data_files(value)
            .build()
            .err()
            .unwrap();
        assert!(
            matches!(&err, Error::InvalidLimit { name, .. } if name.name() == "max_data_files"),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("max_data_files must be between 1 and 512, got {value}")
        );
    }
    for value in [0, erp_modules::system::MAX_OPERATION_DATA_BYTES + 1] {
        let err = builder(&db, &addons)
            .max_operation_data_bytes(value)
            .build()
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            format!(
                "max_operation_data_bytes must be between 1 and {}, got {value}",
                erp_modules::system::MAX_OPERATION_DATA_BYTES
            )
        );
    }
    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
        .max_data_files(2)
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    take_log();
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            Error::TooManyDataFiles {
                count: 3,
                limit: 2,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module \"x\" lists 3 data and demo files; an operation accepts at most 2 per module \
         (ModuleSystemBuilder::max_data_files)"
    );
    assert!(take_log().is_empty(), "nothing reached the loader");
    assert_eq!(state(&init.orm, "x").await, "uninstalled");

    // bytes: one file fits, the second passes the total of the operation
    let one = u64::try_from(ONE_RECORD_XML.len()).unwrap();
    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
        .max_operation_data_bytes(one + one / 2)
        .build()
        .unwrap();
    let env = su_of(&system);
    take_log();
    let err = system
        .button_immediate_install(&env, &[n("x")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::OperationDataTooLarge { path, .. } if path == "data/b.xml"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "Module \"x\": data file 'data/b.xml' passes the {} bytes of data files one \
             operation reads (ModuleSystemBuilder::max_operation_data_bytes)",
            one + one / 2
        )
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    db.cleanup().await.unwrap();
}

fn step_names(steps: &[PostCommitStep]) -> Vec<(&str, &str)> {
    steps
        .iter()
        .map(|s| (s.module.as_str(), s.key.as_str()))
        .collect()
}

/// Review M8: the post-commit phase runs under its own lock; while another phase holds it, the
/// steps stay pending and the report says why.
#[tokio::test]
async fn post_commit_phase_waits_for_another_phase() {
    let addons = Addons::new();
    addons.add("p", "name = 'P'\npost_init_hook = 'steps'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("p", "test.p")])
        .hooks([&SCHEDULE_STEPS])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let holder = db.pool().get().await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_lock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("p")])
        .await
        .unwrap();
    assert!(
        matches!(
            outcome.post_commit.stopped,
            Some(PostCommitStop::Busy { .. })
        ),
        "{:?}",
        outcome.post_commit.stopped
    );
    assert!(outcome.post_commit.done.is_empty());
    assert!(outcome.post_commit.failed.is_empty());
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT key, state FROM ir_module_post_commit ORDER BY id"
        )
        .await,
        vec![
            vec![Some("name_index".to_owned()), Some("pending".to_owned())],
            vec![Some("broken".to_owned()), Some("pending".to_owned())],
        ]
    );
    holder
        .query(
            "SELECT pg_advisory_unlock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    drop(holder);
    // the next operation's phase runs them
    let outcome = system
        .button_immediate_upgrade(&su(&init.orm), &[n("p")])
        .await
        .unwrap();
    assert_eq!(step_names(&outcome.post_commit.done), [("p", "name_index")]);
    db.cleanup().await.unwrap();
}

fn unique_index_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.execute("INSERT INTO test_u (name) VALUES ('dup'), ('dup')", &[])
            .await?;
        let u = ModuleName::new("u").unwrap();
        let index = erp_modules::PostCommitAction::create_index(
            erp_modules::CreateIndex::new(
                erp_modules::TableName::parse("test_u").unwrap(),
                erp_modules::IndexName::parse("test_u_name_uniq").unwrap(),
                vec![erp_modules::IndexColumn::new("name").unwrap()],
            )
            .unwrap()
            .unique(),
        );
        schedule_post_commit(tx, &u, &pk("name_unique"), &index).await
    })
}

static UNIQUE_INDEX_STEP: HookImpl =
    HookImpl::new("u", HookKind::PostInit, "unique_index", unique_index_step).at(file!(), line!());

/// Review M8: a failed `CREATE INDEX CONCURRENTLY` leaves an invalid index that `IF NOT EXISTS`
/// would accept; the index step drops it before the retry, which builds a valid index.
#[tokio::test]
async fn post_commit_index_step_rebuilds_an_invalid_index() {
    let addons = Addons::new();
    addons
        .add("u", "name = 'U'\npost_init_hook = 'unique_index'\n")
        .add("z", "name = 'Z'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("u", "test.u")])
        .hooks([&UNIQUE_INDEX_STEP])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    let outcome = system
        .button_immediate_install(&env, &[n("u")])
        .await
        .unwrap();
    assert_eq!(outcome.post_commit.failed.len(), 1);
    assert_eq!(
        outcome.post_commit.failed[0].sqlstate.as_deref(),
        Some("23505")
    );
    let validity = "SELECT i.indisvalid::text FROM pg_index i JOIN pg_class c \
                    ON c.oid = i.indexrelid WHERE c.relname = 'test_u_name_uniq'";
    assert_eq!(
        rows(&init.orm, validity).await,
        vec![vec![Some("false".to_owned())]],
        "the failed build left an invalid index"
    );
    exec(
        &init.orm,
        "DELETE FROM test_u WHERE id = (SELECT max(id) FROM test_u)",
    )
    .await;
    let outcome = system
        .button_immediate_install(&env, &[n("z")])
        .await
        .unwrap();
    assert_eq!(
        step_names(&outcome.post_commit.done),
        [("u", "name_unique")]
    );
    assert_eq!(
        rows(&init.orm, validity).await,
        vec![vec![Some("true".to_owned())]]
    );
    db.cleanup().await.unwrap();
}

/// Review M11, S3, S4: keys of post-commit steps are bounded and checked, names of tables, columns
/// and indexes too; steps are scheduled only from the managed transaction of an operation, as the
/// System principal.
#[tokio::test]
async fn post_commit_steps_are_validated() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let base = n("base");
    let action = analyze("ir_module_module");
    let long_key = "k".repeat(65);

    // S4: a plain transaction (even the System principal's) cannot schedule
    let mut plain = init.orm.begin(SUPERUSER_ID).await.unwrap();
    let err = schedule_post_commit(&mut plain, &base, &pk("k"), &action)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "schedule_post_commit: post-commit steps are scheduled from the transaction of a module \
         operation (a hook or data loader run by the module system); this transaction is not one"
    );
    plain.rollback().await.unwrap();
    drop(plain);
    // S4: a managed transaction of a user principal cannot either
    let mut user = init
        .orm
        .begin(erp_orm::UserId::new(2).unwrap())
        .await
        .unwrap();
    let user_control = user
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    let err = schedule_post_commit(&mut user, &base, &pk("k"), &action)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "schedule_post_commit: only the System principal with full elevation may do this (this \
         one acts as user 2)"
    );
    user.operation_rollback(&user_control).await.unwrap();
    drop(user);
    assert_eq!(
        rows(
            &init.orm,
            "SELECT count(*)::text FROM ir_module_post_commit"
        )
        .await,
        vec![vec![Some("0".to_owned())]],
        "nothing was scheduled"
    );

    let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
    let control = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    for (key, message) in [
        (
            "",
            "invalid post-commit key \"\": 1 to 64 bytes of a-z, 0-9, '_', '.', '-'".to_owned(),
        ),
        (
            "Bad Key",
            "invalid post-commit key \"Bad Key\": 1 to 64 bytes of a-z, 0-9, '_', '.', '-'"
                .to_owned(),
        ),
        (
            long_key.as_str(),
            format!(
                "invalid post-commit key \"{long_key}\": 1 to 64 bytes of a-z, 0-9, '_', '.', '-'"
            ),
        ),
    ] {
        let err = erp_modules::PostCommitKey::parse(key).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{key:?}");
        assert_eq!(err.message(), message);
    }
    schedule_post_commit(&mut tx, &base, &pk("k"), &action)
        .await
        .unwrap();
    tx.operation_rollback(&control).await.unwrap();
    drop(tx);

    // names: refused when the value is made, before anything is scheduled
    let rule = "1 to 63 bytes of a-z, 0-9, '_', not starting with a digit";
    for (bad, what) in [
        ("Bad-Table", "table"),
        ("t; DROP TABLE x", "table"),
        ("1abc", "table"),
        ("", "table"),
    ] {
        let err = erp_modules::TableName::parse(bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!("invalid {what} name {bad:?}: {rule}")
        );
    }
    let err = erp_modules::IndexColumn::new("A").unwrap_err();
    assert_eq!(err.message(), format!("invalid column name \"A\": {rule}"));
    let err = erp_modules::IndexColumn::new("a")
        .unwrap()
        .with_opclass("gin trgm")
        .unwrap_err();
    assert_eq!(
        err.message(),
        format!("invalid operator class name \"gin trgm\": {rule}")
    );
    let err = erp_modules::IndexName::parse("Bad-Index").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("invalid index name \"Bad-Index\": {rule}")
    );
    db.cleanup().await.unwrap();
}

/// Review M15: the data file limit is enforced on the bytes read and can only be lowered.
#[tokio::test]
async fn data_file_size_limit() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/a.xml']\n")
        .file("x", "data/a.xml", ONE_RECORD_XML);
    let db = TestDb::create().await.unwrap();
    for bad in [0, erp_modules::system::MAX_DATA_FILE_BYTES + 1] {
        let err = builder(&db, &addons)
            .max_data_file_bytes(bad)
            .build()
            .err()
            .unwrap();
        assert!(
            matches!(
                &err,
                Error::InvalidLimit {
                    name: LimitKind::MaxDataFileBytes,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("max_data_file_bytes must be between 1 and 67108864, got {bad}")
        );
    }
    let limit = u64::try_from(ONE_RECORD_XML.len()).unwrap() - 1;
    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
        .max_data_file_bytes(limit)
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::DataFileTooLarge { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        format!("Module \"x\": data file 'data/a.xml' is larger than {limit} bytes")
    );
    // exactly at the limit is accepted
    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
        .max_data_file_bytes(limit + 1)
        .build()
        .unwrap();
    let env = su_of(&system);
    committed(
        system
            .button_immediate_install(&env, &[n("x")])
            .await
            .unwrap(),
    );
    db.cleanup().await.unwrap();
}

/// The operation key is the module system's alone (review D2): once a module system exists, no
/// other code can claim it, and the module system's own operations still manage their
/// transactions with it.
#[tokio::test]
async fn the_operation_key_is_held_by_the_module_system() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let err = erp_orm::OperationKey::claim().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "OperationKey::claim: the operation key was claimed already; the module system takes it \
         once per process, before any other code"
    );
    let _initialized = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    db.cleanup().await.unwrap();
}
