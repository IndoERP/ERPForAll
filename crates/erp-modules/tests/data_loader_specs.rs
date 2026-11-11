//! The data loader on a database: records through the default `ErpDataLoader`, demo
//! data, file kinds, modes and the stubs the later sub-slices replace.
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

use std::sync::Arc;

use common::*;
use erp_data::XmlId;
use erp_data::load::{DataNotice, LoadErrorKind, NoticeKind};
use erp_fields::ErrorKind;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::loader::{DataFile, FileLoader, Operations};
use erp_modules::manifest::ModuleName;
use erp_modules::system::InitOptions;
use erp_orm::{BoxFuture, SUPERUSER_ID, Transaction};
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;

fn record_xml(id: &str, model: &str, name: &str) -> String {
    format!(
        "<odoo><record id=\"{id}\" model=\"{model}\"><field name=\"name\">{name}</field></record></odoo>"
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

async fn names(orm: &erp_orm::Orm, table: &str) -> Vec<String> {
    rows(orm, &format!("SELECT name FROM {table} ORDER BY name"))
        .await
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect()
}

#[tokio::test]
async fn spec_data_002_record_creates_the_row_the_xid_and_env_ref_finds_it() {
    let addons = Addons::new();
    addons
        .add("m", "name = 'M'\ndata = ['data/cat.xml']\n")
        .file("m", "data/cat.xml", &record_xml("cat_a", "test.cat", "A"));
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("m", "test.cat")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("m")])
        .await
        .unwrap();
    assert!(
        outcome.data_notices.is_empty(),
        "{:?}",
        outcome.data_notices
    );
    assert!(outcome.post_commit.failed.is_empty());
    let orm = outcome.orm;

    assert_eq!(names(&orm, "test_cat").await, ["A"]);
    let xid_rows = rows(
        &orm,
        "SELECT module, name, model, noupdate::text, res_id::text FROM ir_model_data \
         WHERE module = 'm'",
    )
    .await;
    let record_id = rows(&orm, "SELECT id::text FROM test_cat WHERE name = 'A'").await[0][0]
        .clone()
        .unwrap();
    assert_eq!(
        xid_rows,
        [[
            Some("m".to_owned()),
            Some("cat_a".to_owned()),
            Some("test.cat".to_owned()),
            Some("false".to_owned()),
            Some(record_id.clone()),
        ]]
    );

    // `env.ref('m.cat_a')` finds the record
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let found =
        erp_data::model_data::env_ref(&mut tx, &env, &XmlId::parse("m.cat_a").unwrap(), true)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(found.model_name(), "test.cat");
    assert_eq!(found.ids(), [record_id.parse::<i64>().unwrap()]);
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

// ---- / 034: demo data -------------------------------------------------------

fn demo_addons() -> Addons {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add(
            "x",
            "name = 'X'\ndepends = ['a']\ndata = ['data/d.xml']\ndemo = ['demo/dm.xml']\n",
        )
        .file(
            "x",
            "data/d.xml",
            &record_xml("x_data", "test.cat", "x data"),
        )
        .file(
            "x",
            "demo/dm.xml",
            &record_xml("x_demo", "test.cat", "x demo"),
        )
        .add(
            "bad",
            "name = 'Bad'\ndepends = ['a']\ndemo = ['demo/broken.xml']\n",
        )
        .file(
            "bad",
            "demo/broken.xml",
            &record_xml("bad_demo", "no.such.model", "never"),
        )
        .add(
            "y",
            "name = 'Y'\ndepends = ['a', 'bad']\ndemo = ['demo/dm.xml']\n",
        )
        .file(
            "y",
            "demo/dm.xml",
            &record_xml("y_demo", "test.cat", "y demo"),
        );
    addons
}

async fn demo_flags(orm: &erp_orm::Orm) -> Vec<(String, bool)> {
    rows(
        orm,
        "SELECT name, demo::text FROM ir_module_module WHERE name IN ('a', 'bad', 'x', 'y') \
         ORDER BY name",
    )
    .await
    .into_iter()
    .map(|r| (r[0].clone().unwrap(), r[1].as_deref() == Some("true")))
    .collect()
}

#[tokio::test]
async fn spec_mod_033_demo_records_exist_only_when_every_dependency_has_demo() {
    // a database created without demo data: nothing gets demo
    let addons = demo_addons();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.cat")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("x"), n("y")])
        .await
        .unwrap();
    assert!(outcome.data_notices.is_empty());
    assert_eq!(
        demo_flags(&outcome.orm).await,
        [("a", false), ("bad", false), ("x", false), ("y", false)].map(|(n, d)| (n.to_owned(), d))
    );
    assert_eq!(names(&outcome.orm, "test_cat").await, ["x data"]);
    db.cleanup().await.unwrap();

    // a database created with demo data: a module gets demo only when all its dependencies have
    // it; `bad`'s demo data fails (MOD-034), so `y`, which depends on it, gets none
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.cat")])
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
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("x"), n("y")])
        .await
        .unwrap();
    assert_eq!(
        demo_flags(&outcome.orm).await,
        [("a", true), ("bad", false), ("x", true), ("y", false)].map(|(n, d)| (n.to_owned(), d))
    );
    assert_eq!(
        names(&outcome.orm, "test_cat").await,
        ["x data", "x demo"],
        "y has no demo record because its dependency `bad` has no demo data"
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_mod_034_demo_failure_rolls_back_the_demo_files_and_installs_without_demo() {
    let addons = Addons::new();
    addons
        .add(
            "x",
            "name = 'X'\ndata = ['data/d.xml']\ndemo = ['demo/ok.xml', 'demo/bad.xml']\n",
        )
        .file(
            "x",
            "data/d.xml",
            &record_xml("d1", "test.cat", "data record"),
        )
        .file(
            "x",
            "demo/ok.xml",
            &record_xml("dm1", "test.cat", "demo one"),
        )
        .file(
            "x",
            "demo/bad.xml",
            "<odoo><record id=\"dm2\" model=\"test.cat\"><field name=\"no_such_field\">1</field>\
             </record></odoo>",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat"), demo_failure_model()])
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
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap();

    // installed, without demo data; the data record exists, no record of any demo file does
    assert_eq!(outcome.installed, vec![n("x")]);
    assert_eq!(state(&outcome.orm, "x").await, "installed");
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT demo::text FROM ir_module_module WHERE name = 'x'"
        )
        .await,
        [[Some("false".to_owned())]]
    );
    assert_eq!(names(&outcome.orm, "test_cat").await, ["data record"]);
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT name FROM ir_model_data WHERE module = 'x' ORDER BY name"
        )
        .await,
        [[Some("d1".to_owned())]],
        "the first demo file's external id was rolled back with it"
    );

    // the failure is returned, not logged away
    let [notice] = outcome.data_notices.as_slice() else {
        panic!("expected one notice, got {:?}", outcome.data_notices);
    };
    assert_eq!((notice.module(), notice.file()), ("x", "demo/bad.xml"));
    let NoticeKind::DemoFailed { error } = notice.kind() else {
        panic!("expected DemoFailed, got {:?}", notice.kind());
    };
    let lines: Vec<&str> = error.lines().collect();
    assert_eq!(
        lines[0], "Module \"x\": demo file 'demo/bad.xml' failed",
        "{error}"
    );
    assert_eq!(
        lines[1], "while parsing demo/bad.xml:1, somewhere inside",
        "{error}"
    );
    assert_eq!(
        lines[2], "<record id=\"dm2\" model=\"test.cat\">",
        "{error}"
    );

    // and `ir.demo_failure` references the module (the model exists in this registry)
    let failures = rows(
        &outcome.orm,
        "SELECT m.name, f.error FROM ir_demo_failure f JOIN ir_module_module m ON m.id = f.module_id",
    )
    .await;
    assert_eq!(
        failures,
        [[Some("x".to_owned()), Some(error.clone())]],
        "the row carries the same text as the notice"
    );
    db.cleanup().await.unwrap();
}

/// a `.sql` demo file — still fails the operation; the data-failure list is the runner's)
#[tokio::test]
async fn demo_failure_of_the_infrastructure_kind_fails_the_install() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndemo = ['demo/bad.sql']\n")
        .file("x", "demo/bad.sql", "INSERT INTO no_such_table VALUES (1);");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(
            InitOptions::new()
                .with_skip_auto_install(true)
                .with_demo(true),
        )
        .await
        .unwrap();
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("x", ModuleStep::DemoFile, Some("demo/bad.sql"))
    );
    // an undefined table is SQLSTATE 42P01 (class 42), not data (class 22, 23)
    assert!(
        matches!(source, Error::Orm(e) if e.kind() == ErrorKind::Database && e.sqlstate() == Some("42P01")),
        "{source:?}"
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    db.cleanup().await.unwrap();
}

// ----: file kinds and repeats ------------------------------------------------

#[tokio::test]
async fn spec_mod_035_xml_csv_sql_load_js_is_ignored_repeats_load_twice() {
    let addons = Addons::new();
    // `d.js` does not exist: an ignored file is not even read
    addons
        .add(
            "x",
            "name = 'X'\ndata = ['data/a.xml', 'data/test.cat.csv', 'data/c.sql', 'data/d.js', \
             'data/a.xml']\n",
        )
        .file("x", "data/a.xml", &record_xml("r1", "test.cat", "from xml"))
        .file("x", "data/test.cat.csv", "id,name\ncsv_row,from csv\n")
        .file(
            "x",
            "data/c.sql",
            "INSERT INTO test_cat (name) VALUES ('from sql');",
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
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap();
    assert_eq!(
        names(&outcome.orm, "test_cat").await,
        ["from csv", "from sql", "from xml"],
        "the same file listed twice loads twice, onto the same record"
    );
    assert_eq!(
        outcome.data_notices,
        [
            DataNotice::new("x", "data/d.js", NoticeKind::IgnoredFile),
            DataNotice::new("x", "data/a.xml", NoticeKind::RepeatedFile),
        ]
    );
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT name FROM ir_model_data WHERE module = 'x' ORDER BY name"
        )
        .await,
        [[Some("csv_row".to_owned())], [Some("r1".to_owned())]]
    );
    db.cleanup().await.unwrap();

    // any other extension fails the install, and nothing is left behind
    let addons = Addons::new();
    addons
        .add("t", "name = 'T'\ndata = ['data/a.xml', 'data/e.txt']\n")
        .file("t", "data/a.xml", &record_xml("r1", "test.cat", "t"))
        .file("t", "data/e.txt", "text");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("t", "test.cat")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let before = snapshot(&init.orm).await;
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("t")])
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::DataFileUnsupported { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "Module \"t\": can't load unknown file type data/e.txt (expected .xml, .csv, .sql or .js)"
    );
    assert_eq!(snapshot(&init.orm).await, before);
    db.cleanup().await.unwrap();
}

/// A `.sql` file that commits the operation's transaction is caught by the checkpoint.
#[tokio::test]
async fn sql_data_file_that_commits_is_detected() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/c.sql']\n")
        .file("x", "data/c.sql", "COMMIT;");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("x", ModuleStep::DataFile, Some("data/c.sql"))
    );
    assert!(
        matches!(source, Error::OperationTransactionEnded { .. }),
        "{source:?}"
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    db.cleanup().await.unwrap();
}

// ---- (runner half) ---------------------------------------------------------

/// and goes on; the whole case is `data_csv_specs.rs`)
#[tokio::test]
async fn csv_without_id_in_update_mode_is_a_notice_and_the_operation_goes_on() {
    let addons = Addons::new();
    addons
        .add(
            "x",
            "name = 'X'\ndata = ['data/test.cat.csv', 'data/a.xml']\n",
        )
        .file("x", "data/test.cat.csv", "name\nno id column\n")
        .file("x", "data/a.xml", &record_xml("r1", "test.cat", "from xml"));
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    // the CSV file loads in init mode on install (no `id` needed); change nothing and upgrade
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap();
    assert!(
        installed.data_notices.is_empty(),
        "{:?}",
        installed.data_notices
    );
    let outcome = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("x")])
        .await
        .unwrap();
    let [notice] = outcome.data_notices.as_slice() else {
        panic!("expected one notice, got {:?}", outcome.data_notices);
    };
    assert_eq!((notice.module(), notice.file()), ("x", "data/test.cat.csv"));
    assert!(
        matches!(notice.kind(), NoticeKind::SkippedFile { reason } if reason.contains("does not contain 'id'")),
        "{notice:?}"
    );
    assert_eq!(outcome.upgraded, vec![n("x")], "the operation went on");
    db.cleanup().await.unwrap();
}

// ---- / 031 ------------------------------------------------------------------

/// A loader that logs the `summary` the module row has while the module's file loads.
struct SummaryProbe;

impl FileLoader for SummaryProbe {
    fn load<'a, 'o: 'a>(
        &'a self,
        tx: &'a mut Transaction,
        file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            for op in operations {
                op?;
            }
            let rows = tx
                .query(
                    "SELECT summary FROM ir_module_module WHERE name = $1",
                    &[&file.module().as_str()],
                )
                .await?;
            let summary: Option<String> = rows[0].get(0);
            log(format!("{}:{}", file.module(), summary.unwrap_or_default()));
            Ok(())
        })
    }

    fn purge<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _modules: &'a [ModuleName],
    ) -> BoxFuture<'a, erp_modules::Result<Vec<erp_data::load::DataNotice>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// loads; re-validating the loaded noupdate views is)
#[tokio::test]
async fn spec_mod_032_module_row_is_refreshed_before_the_data_loads() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\nsummary = 'old'\ndata = ['data/a.xml']\n")
        .file("x", "data/a.xml", ONE_RECORD_XML);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.x")])
        .data_loader(Arc::new(SummaryProbe))
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    committed(
        system
            .button_immediate_install(&su(&init.orm), &[n("x")])
            .await
            .unwrap(),
    );
    assert_eq!(take_log(), ["x:old"]);
    addons.add("x", "name = 'X'\nsummary = 'new'\ndata = ['data/a.xml']\n");
    enroll_fixture_transition(&system, &["x"]).await;
    committed(
        system
            .button_immediate_upgrade(&su(&init.orm), &[n("x")])
            .await
            .unwrap(),
    );
    assert_eq!(take_log(), ["x:new"]);
    db.cleanup().await.unwrap();
}

/// even under `noupdate`; the whole case, with the upgrade half, is in `data_update_specs.rs`)
#[tokio::test]
async fn spec_mod_031_reinit_overwrites_what_the_user_changed() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/a.xml']\n")
        .add("y", "name = 'Y'\ndepends = ['x']\n")
        .file(
            "x",
            "data/a.xml",
            "<odoo><data noupdate=\"1\"><record id=\"r1\" model=\"test.cat\">\
             <field name=\"name\">original</field></record></data></odoo>",
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
        .button_immediate_install(&su(&init.orm), &[n("y")])
        .await
        .unwrap();
    assert_eq!(names(&installed.orm, "test_cat").await, ["original"]);
    exec(
        &installed.orm,
        "UPDATE test_cat SET name = 'changed by the user'",
    )
    .await;

    // reinit of x also reinitializes its installed dependent y, and keeps both installed
    let outcome = system.reinit(&su(&installed.orm), &[n("x")]).await.unwrap();
    assert_eq!(outcome.upgraded, vec![n("x"), n("y")]);
    assert_eq!(names(&outcome.orm, "test_cat").await, ["original"]);
    assert_eq!(state(&outcome.orm, "x").await, "installed");
    assert_eq!(state(&outcome.orm, "y").await, "installed");
    db.cleanup().await.unwrap();
}

// ---- stubs the later sub-slices replace --------------------------------------------------

/// A record the loader cannot load yet fails the install with the file, line and operation in the
/// message, and the module is left uninstalled (the stubs' `Unsupported` kinds are tested at the
/// loader level in `erp-test-models`).
#[tokio::test]
async fn an_unsupported_operation_fails_the_install_naming_file_line_and_operation() {
    let addons = Addons::new();
    addons.add("x", "name = 'X'\ndata = ['data/a.xml']\n").file(
        "x",
        "data/a.xml",
        "<odoo>\n<asset id=\"as\" name=\"A\"><bundle>web.x</bundle><path>a/b.js</path></asset>\n</odoo>",
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
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("x", ModuleStep::DataFile, Some("data/a.xml"))
    );
    assert!(matches!(source, Error::DataLoad(_)), "{source:?}");
    assert_eq!(
        chain(&err),
        [
            "Module \"x\": data file 'data/a.xml' failed",
            "while parsing data/a.xml:2, somewhere inside\n\
             <asset id=\"as\">\n\
             <asset> is not supported until M4 (web client asset bundles)",
        ]
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    db.cleanup().await.unwrap();
}

// ---- train round 2: base-install grant -----------------------------------------------------

/// `base`'s data files load through the module system with its own base-install grant (minted
/// by the module system's `build_with_base_install_grant`, lent to the load session): no harness
/// grant is involved, and the default loader marks the transaction without refusing.
#[tokio::test]
async fn base_data_files_load_with_the_module_systems_own_grant() {
    let addons = Addons::new();
    addons
        .add(
            "base",
            "name = 'Base'\nauto_install = true\ncategory = 'Hidden'\ndata = ['data/cat.xml']\n",
        )
        .file(
            "base",
            "data/cat.xml",
            &record_xml("cat_base", "test.cat", "B"),
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("base", "test.cat")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert_eq!(names(&init.orm, "test_cat").await, ["B"]);
    let xid = rows(
        &init.orm,
        "SELECT module, name FROM ir_model_data WHERE model = 'test.cat'",
    )
    .await;
    assert_eq!(
        xid,
        [[Some("base".to_owned()), Some("cat_base".to_owned())]]
    );
}

/// `test.cat` with a parent, scalar fields of every kind, tags and a JSON payload, for the
/// object-valued expressions.
fn test_cat_model_with_parent(addon: &'static str) -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(
        vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("parent_id", "test.cat"),
            FieldDecl::integer("sequence"),
            FieldDecl::float("weight"),
            FieldDecl::boolean("flag"),
            FieldDecl::many2many("tag_ids", "test.tag"),
            FieldDecl::json("payload"),
        ]
        .into_boxed_slice(),
    );
    Box::leak(Box::new(
        ModelDecl::new(addon, "test.cat")
            .description("Test cat with parent")
            .fields(fields)
            .at(file!(), line!()),
    ))
}

/// Installs addon `m` with `xml` as its data file over `test.cat` and `test.tag`.
async fn install_cats(xml: &str) -> (TestDb, Result<erp_orm::Orm, Error>) {
    let addons = Addons::new();
    addons
        .add("m", "name = 'M'\ndata = ['data/cats.xml']\n")
        .file("m", "data/cats.xml", xml);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([test_cat_model_with_parent("m"), model("m", "test.tag")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let result = system
        .button_immediate_install(&su(&init.orm), &[n("m")])
        .await
        .map(|outcome| outcome.orm);
    (db, result)
}

/// The error of an install that must fail (`Orm` has no `Debug`, so no `unwrap_err`).
fn failed<T>(result: Result<T, Error>) -> Error {
    match result {
        Ok(_) => panic!("the install should fail"),
        Err(e) => e,
    }
}

/// The `DataLoad` error of a failed install of `data/cats.xml`.
fn cats_load_error(err: &Error) -> &erp_data::load::LoadError {
    let (module, step, file, source) = step_failed(err);
    assert_eq!(
        (module, step, file),
        ("m", ModuleStep::DataFile, Some("data/cats.xml"))
    );
    match source {
        Error::DataLoad(e) => e,
        other => panic!("expected DataLoad, got {other:?}"),
    }
}

/// Asserts that the install failed evaluating `expr` with `expected` (kind and full message).
fn assert_eval_failed(err: &Error, expr: &str, expected: &erp_expr::ExprError) {
    let e = cats_load_error(err);
    assert!(
        matches!(e.kind(), LoadErrorKind::Eval { expr: x, error } if x == expr && error == expected),
        "{e:?}"
    );
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing data/cats.xml:{}, somewhere inside\n{}\neval \"{expr}\" failed: \
             {expected}",
            e.line().unwrap(),
            e.op().unwrap()
        )
    );
}

/// The id of the `test_cat` or `test_tag` row named `name`.
async fn id_of(orm: &erp_orm::Orm, table: &str, name: &str) -> String {
    let found = rows(
        orm,
        &format!("SELECT id::text FROM {table} WHERE name = '{name}'"),
    )
    .await;
    assert_eq!(found.len(), 1, "one {table} row named {name}");
    found[0][0].clone().unwrap()
}

/// `parent_id` of the cat named `name` (`None` when empty).
async fn parent_of(orm: &erp_orm::Orm, name: &str) -> Option<String> {
    rows(
        orm,
        &format!("SELECT parent_id::text FROM test_cat WHERE name = '{name}'"),
    )
    .await[0][0]
        .clone()
}

/// The tag ids of the cat named `name`, ascending.
async fn tags_of(orm: &erp_orm::Orm, name: &str) -> Vec<String> {
    let cat = id_of(orm, "test_cat", name).await;
    let table = rows(
        orm,
        "SELECT table_name::text FROM information_schema.columns \
         WHERE column_name = 'test_tag_id' AND table_name LIKE '%test_cat%'",
    )
    .await[0][0]
        .clone()
        .unwrap();
    rows(
        orm,
        &format!("SELECT test_tag_id::text FROM {table} WHERE test_cat_id = {cat} ORDER BY 1"),
    )
    .await
    .into_iter()
    .map(|r| r[0].clone().unwrap())
    .collect()
}

#[tokio::test]
async fn spec_data_090_model_proxy_and_browse_in_data_expressions() {
    let xml = r#"<odoo>
        <record id="tag_red" model="test.tag"><field name="name">Red</field></record>
        <record id="cat_1" model="test.cat">
            <field name="name">Alpha</field>
        </record>
        <record id="cat_2" model="test.cat">
            <field name="name" model="test.cat" eval="obj(ref('cat_1')).name + '-browse'"/>
        </record>
        <record id="cat_3" model="test.cat">
            <field name="name" model="test.cat" eval="obj().env.ref('cat_1').name + '-ref'"/>
        </record>
        <record id="cat_4" model="test.cat">
            <field name="name" model="test.cat" eval="obj().sudo().browse(ref('cat_1')).name + '-sudo'"/>
        </record>
        <record id="cat_5" model="test.cat">
            <field name="name" model="test.cat" eval="obj().with_context().browse(ref('cat_1')).name + '-ctx'"/>
        </record>
        <record id="cat_6" model="test.cat">
            <field name="name" model="test.cat" eval="obj().env['test.tag'].browse(ref('tag_red')).name + '-other'"/>
            <field name="parent_id" model="test.cat" eval="obj(obj(ref('cat_1')))"/>
        </record>
        <record id="cat_7" model="test.cat">
            <field name="name" model="test.cat" eval="'%s|%s|%s' % (bool(obj()), obj().id, obj().ids)"/>
            <field name="sequence" model="test.cat" eval="obj(ref('cat_1')).id"/>
        </record>
        <record id="cat_8" model="test.cat">
            <field name="name" model="test.cat" eval="obj().with_context(active_test=False).browse(ref('cat_1')).name + '-act'"/>
        </record>
    </odoo>"#;
    let (db, result) = install_cats(xml).await;
    let orm = result.unwrap();
    let mut all = names(&orm, "test_cat").await;
    all.sort();
    assert_eq!(
        all,
        [
            "Alpha",
            "Alpha-act",
            "Alpha-browse",
            "Alpha-ctx",
            "Alpha-ref",
            "Alpha-sudo",
            "False|False|[]",
            "Red-other",
        ]
    );
    let alpha = id_of(&orm, "test_cat", "Alpha").await;
    // obj(record) browses the record's id; obj(id).id is the id itself
    assert_eq!(parent_of(&orm, "Red-other").await, Some(alpha.clone()));
    let seq = rows(
        &orm,
        "SELECT sequence::text FROM test_cat WHERE name = 'False|False|[]'",
    )
    .await;
    assert_eq!(seq, [[Some(alpha)]]);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_090_with_context_arguments_fail_loudly() {
    let expr = "obj().with_context('not-a-dict').search([]).id";
    let xml = format!(
        r#"<odoo><record id="c" model="test.cat"><field name="parent_id" model="test.cat" eval="{expr}"/></record></odoo>"#
    );
    let (db, result) = install_cats(&xml).await;
    let expected =
        erp_expr::ExprError::type_error("with_context() argument must be a dict, not 'str'");
    assert_eval_failed(&failed(result), expr, &expected);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_091_record_attribute_access_and_relational_traversal() {
    let xml = r#"<odoo>
        <record id="tag_red" model="test.tag"><field name="name">Red</field></record>
        <record id="tag_blue" model="test.tag"><field name="name">Blue</field></record>
        <record id="cat_root" model="test.cat">
            <field name="name">Hello World</field>
            <field name="sequence">7</field>
            <field name="weight">2.5</field>
            <field name="flag" eval="True"/>
            <field name="tag_ids" eval="[(6, 0, [ref('tag_red'), ref('tag_blue')])]"/>
        </record>
        <record id="cat_child" model="test.cat">
            <field name="name">Child</field>
            <field name="parent_id" model="test.cat" eval="obj().env.ref('cat_root').id"/>
        </record>
        <record id="cat_grandchild" model="test.cat">
            <field name="name">Grandchild</field>
            <field name="parent_id" model="test.cat" eval="obj().env.ref('cat_child').ids[0]"/>
        </record>
        <record id="cat_scalars" model="test.cat">
            <field name="name" model="test.cat" eval="'%s|%s|%s|%s' % (obj().env.ref('cat_root').name.split(' ')[0], obj().env.ref('cat_root').sequence, obj().env.ref('cat_root').weight, obj().env.ref('cat_root').flag)"/>
            <field name="sequence" model="test.cat" eval="len(obj().env.ref('cat_child').ids)"/>
        </record>
        <record id="cat_traversal" model="test.cat">
            <field name="name" model="test.cat" eval="obj().env.ref('cat_grandchild').parent_id.parent_id.name.lower()"/>
            <field name="parent_id" model="test.cat" eval="obj().env.ref('cat_grandchild').parent_id"/>
            <field name="tag_ids" model="test.cat" eval="[(6, 0, obj().env.ref('cat_root').tag_ids.ids)]"/>
        </record>
        <record id="cat_empty" model="test.cat">
            <field name="name" model="test.cat" eval="'%s|%s|%s' % (bool(obj().env.ref('cat_root').parent_id), obj().env.ref('cat_root').parent_id.id, obj().env.ref('cat_root').parent_id.ids)"/>
            <field name="parent_id" model="test.cat" eval="obj().env.ref('cat_root').parent_id"/>
        </record>
    </odoo>"#;
    let (db, result) = install_cats(xml).await;
    let orm = result.unwrap();
    let root = id_of(&orm, "test_cat", "Hello World").await;
    let child = id_of(&orm, "test_cat", "Child").await;

    // .id is the id, .ids is [id]; a many2one reads as a record of the comodel
    assert_eq!(parent_of(&orm, "Child").await, Some(root.clone()));
    assert_eq!(parent_of(&orm, "Grandchild").await, Some(child.clone()));
    assert_eq!(parent_of(&orm, "hello world").await, Some(child));
    // str, int, float and bool fields read as their stored values; len(rec.ids) == 1
    let scalars = rows(
        &orm,
        "SELECT name, sequence::text FROM test_cat WHERE name LIKE 'Hello|%'",
    )
    .await;
    assert_eq!(
        scalars,
        [[Some("Hello|7|2.5|True".to_owned()), Some("1".to_owned())]]
    );
    // an x2many reads as a recordset whose .ids are the related ids
    let root_tags = tags_of(&orm, "Hello World").await;
    assert_eq!(root_tags.len(), 2);
    assert_eq!(tags_of(&orm, "hello world").await, root_tags);
    // an empty many2one is falsy, its .id is False and it stores as no value
    assert!(
        names(&orm, "test_cat")
            .await
            .contains(&"False|False|[]".to_owned())
    );
    assert_eq!(parent_of(&orm, "False|False|[]").await, None);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_092_model_proxy_search_in_data_expressions() {
    let xml = r#"<odoo>
        <record id="tag_red" model="test.tag"><field name="name">Red</field></record>
        <record id="tag_blue" model="test.tag"><field name="name">Blue</field></record>
        <record id="tag_green" model="test.tag"><field name="name">Green</field></record>
        <record id="cat_target_a" model="test.cat">
            <field name="name">Target A</field>
        </record>
        <record id="cat_target_b" model="test.cat">
            <field name="name">Target B</field>
        </record>
        <record id="cat_searcher_a" model="test.cat">
            <field name="name">Searcher A</field>
            <field name="parent_id" model="test.cat" eval="obj().search([('name', '=', 'Target A')]).id"/>
            <field name="tag_ids" model="test.cat" eval="[(6, 0, obj().env['test.tag'].search([('name', 'in', ['Red', 'Blue'])]).ids)]"/>
        </record>
        <record id="cat_searcher_b" model="test.cat">
            <field name="name">Searcher B</field>
            <field name="parent_id" model="test.cat" eval="obj().sudo().search([('name', '=', 'Target B')], limit=1).id"/>
        </record>
        <record id="cat_searcher_c" model="test.cat">
            <field name="name">Searcher C</field>
            <field name="parent_id" model="test.cat" eval="obj().with_context().search([('name', 'like', 'Target')], limit=1).id"/>
            <field name="sequence" model="test.cat" eval="len(obj().search([('name', 'like', 'Target')], limit=1).ids)"/>
        </record>
        <record id="cat_searcher_d" model="test.cat">
            <field name="name">Searcher D</field>
            <field name="parent_id" model="test.cat" eval="obj().search([('name', '=', 'Nobody')]).id"/>
        </record>
        <record id="cat_searcher_e" model="test.cat">
            <field name="name">Searcher E</field>
            <field name="parent_id" model="test.cat" eval="obj().with_context(active_test=False).sudo(False).search([('name', 'like', 'Target')], offset=1, order='name asc', limit=1).id"/>
        </record>
    </odoo>"#;
    let (db, result) = install_cats(xml).await;
    let orm = result.unwrap();
    let target_a = id_of(&orm, "test_cat", "Target A").await;
    let target_b = id_of(&orm, "test_cat", "Target B").await;
    assert_eq!(parent_of(&orm, "Searcher A").await, Some(target_a.clone()));
    assert_eq!(parent_of(&orm, "Searcher B").await, Some(target_b.clone()));
    // limit=1 keeps the first match in the model's order (id)
    assert_eq!(parent_of(&orm, "Searcher C").await, Some(target_a));
    let seq = rows(
        &orm,
        "SELECT sequence::text FROM test_cat WHERE name = 'Searcher C'",
    )
    .await;
    assert_eq!(seq, [[Some("1".to_owned())]]);
    // .id of an empty result is False
    assert_eq!(parent_of(&orm, "Searcher D").await, None);
    // offset=1 and order='name asc' picks Target B over Target A
    assert_eq!(parent_of(&orm, "Searcher E").await, Some(target_b));
    // .ids are the ids of every match
    let mut expected = vec![
        id_of(&orm, "test_tag", "Red").await,
        id_of(&orm, "test_tag", "Blue").await,
    ];
    expected.sort();
    assert_eq!(tags_of(&orm, "Searcher A").await, expected);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_093_object_valued_expression_results_in_field_evaluation() {
    let xml = r#"<odoo>
        <record id="tag_red" model="test.tag"><field name="name">Red</field></record>
        <record id="tag_blue" model="test.tag"><field name="name">Blue</field></record>
        <record id="cat_root" model="test.cat">
            <field name="name">Root</field>
        </record>
        <record id="cat_child1" model="test.cat">
            <field name="name">Child 1</field>
            <field name="parent_id" model="test.cat" eval="obj().env.ref('cat_root')"/>
            <field name="tag_ids" model="test.cat" eval="[(6, 0, [obj().env.ref('tag_red'), obj().env.ref('tag_blue')])]"/>
        </record>
        <record id="cat_child2" model="test.cat">
            <field name="name">Child 2</field>
            <field name="parent_id" model="test.cat" eval="obj().search([('name', '=', 'Root')])"/>
            <field name="tag_ids" model="test.cat" eval="[(4, obj().env.ref('tag_blue'), 0)]"/>
        </record>
        <record id="cat_child3" model="test.cat">
            <field name="name">Child 3</field>
            <field name="parent_id" model="test.cat" eval="obj().search([('name', '=', 'Nonexistent')])"/>
            <field name="payload" model="test.cat" eval="{'parent': obj().env.ref('cat_root'), 'tags': (obj().env.ref('tag_red'),)}"/>
        </record>
    </odoo>"#;
    let (db, result) = install_cats(xml).await;
    let orm = result.unwrap();
    let root = id_of(&orm, "test_cat", "Root").await;
    let red = id_of(&orm, "test_tag", "Red").await;
    let blue = id_of(&orm, "test_tag", "Blue").await;

    // a record converts to its id; a one-record recordset in a many2one to its id
    assert_eq!(parent_of(&orm, "Child 1").await, Some(root.clone()));
    assert_eq!(parent_of(&orm, "Child 2").await, Some(root.clone()));
    // an empty recordset converts to False
    assert_eq!(parent_of(&orm, "Child 3").await, None);
    // records inside a list inside a tuple, and inside a tuple, convert to ids
    let mut both = vec![red.clone(), blue.clone()];
    both.sort();
    assert_eq!(tags_of(&orm, "Child 1").await, both);
    assert_eq!(tags_of(&orm, "Child 2").await, [blue]);
    // records inside a dict (and a tuple inside it) convert to ids
    let payload = rows(
        &orm,
        "SELECT payload->>'parent', payload->'tags'->>0 FROM test_cat WHERE name = 'Child 3'",
    )
    .await;
    assert_eq!(payload, [[Some(root), Some(red)]]);
    db.cleanup().await.unwrap();
}

/// `obj` without a `model` on the `<field>` is unbound, also inside a
/// `<record>` of a model (review D1: no fallback to the record's model).
#[tokio::test]
async fn spec_data_094_obj_without_model_is_unbound_in_a_record_field() {
    let expr = "obj().search([]).id";
    let xml = format!(
        r#"<odoo><record id="c" model="test.cat"><field name="parent_id" eval="{expr}"/></record></odoo>"#
    );
    let (db, result) = install_cats(&xml).await;
    let expected = erp_expr::ExprError::unavailable(
        "obj",
        "record access is provided by the data loader (EvalContext::with_model)",
    );
    assert_eval_failed(&failed(result), expr, &expected);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_094_fail_loudly_on_invalid_object_and_model_access() {
    // 1. obj without a model (a <value> of a <function> with no model=) is a NameError
    let expr = "obj.something";
    let xml = format!(
        r#"<odoo><function model="test.cat" name="write"><value eval="{expr}"/></function></odoo>"#
    );
    let (db, result) = install_cats(&xml).await;
    let expected = erp_expr::ExprError::unavailable(
        "obj",
        "record access is provided by the data loader (EvalContext::with_model)",
    );
    assert_eval_failed(&failed(result), expr, &expected);
    db.cleanup().await.unwrap();

    // 2. an unknown attribute of a record is an AttributeError naming model and attribute,
    //    raised before any SQL (the registry does not have the field)
    let expr = "obj().env.ref('c1').nonexistent_field";
    let xml = format!(
        r#"<odoo><record id="c1" model="test.cat"><field name="name">C1</field></record><record id="c2" model="test.cat"><field name="name" model="test.cat" eval="{expr}"/></record></odoo>"#
    );
    let (db, result) = install_cats(&xml).await;
    let expected = erp_expr::ExprError::attribute_error("test.cat", "nonexistent_field");
    assert_eval_failed(&failed(result), expr, &expected);
    db.cleanup().await.unwrap();

    // 3. calling a record is a TypeError
    let expr = "obj().env.ref('c1')()";
    let xml = format!(
        r#"<odoo><record id="c1" model="test.cat"><field name="name">C1</field></record><record id="c3" model="test.cat"><field name="name" model="test.cat" eval="{expr}"/></record></odoo>"#
    );
    let (db, result) = install_cats(&xml).await;
    let expected = erp_expr::ExprError::type_error("'test.cat' object is not callable");
    assert_eval_failed(&failed(result), expr, &expected);
    db.cleanup().await.unwrap();

    // 4. a model proxy (raw obj()) and an environment are not field values
    for (expr, got) in [("obj()", "a test.cat"), ("obj().env", "a Environment")] {
        let xml = format!(
            r#"<odoo><record id="c4" model="test.cat"><field name="name" model="test.cat" eval="{expr}"/></record></odoo>"#
        );
        let (db, result) = install_cats(&xml).await;
        let err = failed(result);
        let e = cats_load_error(&err);
        assert!(
            matches!(
                e.kind(),
                LoadErrorKind::NotAFieldValue { model, field, got: g }
                    if (model.as_str(), field.as_str(), g.as_str()) == ("test.cat", "name", got)
            ),
            "{e:?}"
        );
        assert_eq!(
            e.to_string(),
            format!(
                "while parsing data/cats.xml:1, somewhere inside\n{}\nfield 'name' of model \
                 'test.cat': an expression produced {got}, which is not a field value",
                e.op().unwrap()
            )
        );
        db.cleanup().await.unwrap();
    }
}

/// A record id that does not exist is a `MissingError` naming model, id and field, never `False`
/// (review D2).
#[tokio::test]
async fn reading_a_field_of_a_missing_record_fails_loudly() {
    let xml = r#"<odoo><record id="c" model="test.cat"><field name="name" model="test.cat" eval="obj(999999).name"/></record></odoo>"#;
    let (db, result) = install_cats(xml).await;
    let err = failed(result);
    let e = cats_load_error(&err);
    let LoadErrorKind::Orm(inner) = e.kind() else {
        panic!("{e:?}")
    };
    assert_eq!(inner.kind(), ErrorKind::Missing);
    assert_eq!(
        inner.message(),
        "Record does not exist or has been deleted: test.cat(999999) (reading field 'name' in a \
         data expression)"
    );
    db.cleanup().await.unwrap();
}

/// The bounded x2many read of a record id that does not exist is a `MissingError` too, never an
/// empty recordset (review D2, kept by the EXPR-CLOSE-1 fix).
#[tokio::test]
async fn reading_an_x2many_of_a_missing_record_fails_loudly() {
    let xml = r#"<odoo><record id="c" model="test.cat"><field name="name">C</field><field name="sequence" model="test.cat" eval="len(obj(999999).tag_ids.ids)"/></record></odoo>"#;
    let (db, result) = install_cats(xml).await;
    let err = failed(result);
    let e = cats_load_error(&err);
    let LoadErrorKind::Orm(inner) = e.kind() else {
        panic!("{e:?}")
    };
    assert_eq!(inner.kind(), ErrorKind::Missing);
    assert_eq!(
        inner.message(),
        "Record does not exist or has been deleted: test.cat(999999) (reading field 'tag_ids' \
         in a data expression)"
    );
    db.cleanup().await.unwrap();
}

/// Each lookup round fetches what one evaluation named; a chain of record attributes needs one
/// round per hop. `MAX_EVAL_ROUNDS` lookups load; one more fails with a limit error naming the
/// limit, not with "not resolved" (review S2).
#[tokio::test]
async fn eval_lookup_rounds_are_bounded_and_named() {
    let limit = erp_data::load::MAX_EVAL_ROUNDS;
    // addon `a`: cats c0..c<limit>, each the parent of the next
    let mut chain = String::from("<odoo>");
    chain.push_str(r#"<record id="c0" model="test.cat"><field name="name">C0</field></record>"#);
    for i in 1..=limit {
        chain.push_str(&format!(
            r#"<record id="c{i}" model="test.cat"><field name="name">C{i}</field><field name="parent_id" ref="c{}"/></record>"#,
            i - 1
        ));
    }
    chain.push_str("</odoo>");
    // `.parent_id` `limit - 1` times then `.name`: `limit` lookups of record fields
    let within = format!(
        "obj().env.ref('a.c{}'){}.name + '-top'",
        limit - 1,
        ".parent_id".repeat(limit - 1)
    );
    // one hop more: `limit + 1` lookups
    let beyond = format!(
        "obj().env.ref('a.c{limit}'){}.name",
        ".parent_id".repeat(limit)
    );
    let record = |expr: &str| {
        format!(
            r#"<odoo><record id="top" model="test.cat"><field name="name" model="test.cat" eval="{expr}"/></record></odoo>"#
        )
    };
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\ndata = ['data/chain.xml']\n")
        .file("a", "data/chain.xml", &chain);
    addons
        .add(
            "m",
            "name = 'M'\ndepends = ['a']\ndata = ['data/cats.xml']\n",
        )
        .file("m", "data/cats.xml", &record(&within));
    addons
        .add(
            "m2",
            "name = 'M2'\ndepends = ['a']\ndata = ['data/cats.xml']\n",
        )
        .file("m2", "data/cats.xml", &record(&beyond));
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([test_cat_model_with_parent("a"), model("a", "test.tag")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let orm = system
        .button_immediate_install(&su(&init.orm), &[n("a"), n("m")])
        .await
        .unwrap()
        .orm;
    assert_eq!(
        names(&orm, "test_cat")
            .await
            .iter()
            .filter(|n| *n == "C0-top")
            .count(),
        1
    );
    let root = id_of(&orm, "test_cat", "C0").await;

    let err = failed(system.button_immediate_install(&su(&orm), &[n("m2")]).await);
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("m2", ModuleStep::DataFile, Some("data/cats.xml"))
    );
    let Error::DataLoad(e) = source else {
        panic!("expected DataLoad, got {source:?}")
    };
    // the last evaluation stopped at the root's name, the one lookup past the limit
    let expected = erp_expr::ExprError::limit(format!(
        "data expression lookup rounds exceeded (limit {limit}); the last evaluation still \
         needed data: ValueError: Field 'name' of test.cat({root}) was not resolved before \
         evaluation"
    ));
    assert!(
        matches!(e.kind(), LoadErrorKind::Eval { expr, error } if *expr == beyond && *error == expected),
        "{e:?}"
    );
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing data/cats.xml:1, somewhere inside\n{}\neval \"{beyond}\" failed: \
             {expected}",
            e.op().unwrap()
        )
    );
    db.cleanup().await.unwrap();
}

/// in one lookup round: the expression evaluates, it does not fail with the rounds limit.
#[tokio::test]
async fn eval_mapped_over_more_than_the_round_limit() {
    let count = erp_data::load::MAX_EVAL_ROUNDS + 8;
    let mut xml = String::from("<odoo>");
    for i in 0..count {
        xml.push_str(&format!(
            r#"<record id="n{i}" model="test.cat"><field name="name">n-{i}</field></record>"#
        ));
    }
    xml.push_str(
        r#"<record id="probe" model="test.cat"><field name="name">Probe</field><field name="sequence" model="test.cat" eval="len(obj().search([('name', 'like', 'n-')]).mapped('name'))"/></record></odoo>"#,
    );
    let (db, result) = install_cats(&xml).await;
    let orm = result.unwrap_or_else(|e| panic!("install failed: {e:?}"));
    assert_eq!(
        rows(
            &orm,
            "SELECT sequence::text FROM test_cat WHERE name = 'Probe'"
        )
        .await[0][0],
        Some(count.to_string())
    );
    db.cleanup().await.unwrap();
}

/// A data expression reading an x2many value over [`erp_expr::MAX_SEARCH_RESULTS`] fails with a
/// limit error naming the model, field, record and limit (review EXPR-CLOSE-1:
/// the read is bounded before the ids are collected); a value of exactly the limit loads.
#[tokio::test]
async fn an_x2many_value_over_the_recordset_limit_fails_with_a_named_limit_error() {
    let cap = erp_expr::MAX_SEARCH_RESULTS;
    let probe = |cat: &str| {
        format!(
            r#"<odoo><record id="probe" model="test.cat"><field name="name">Probe</field><field name="sequence" model="test.cat" eval="len(obj().env.ref('a.{cat}').tag_ids.ids)"/></record></odoo>"#
        )
    };
    let over_expr_source = probe("cat_over");
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\ndata = ['data/cats.xml']\n")
        .file(
            "a",
            "data/cats.xml",
            r#"<odoo>
            <record id="cat_at" model="test.cat"><field name="name">At cap</field></record>
            <record id="cat_over" model="test.cat"><field name="name">Over cap</field></record>
        </odoo>"#,
        );
    addons
        .add(
            "m",
            "name = 'M'\ndepends = ['a']\ndata = ['data/probe.xml']\n",
        )
        .file("m", "data/probe.xml", &probe("cat_at"));
    addons
        .add(
            "m2",
            "name = 'M2'\ndepends = ['a']\ndata = ['data/probe.xml']\n",
        )
        .file("m2", "data/probe.xml", &over_expr_source);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([test_cat_model_with_parent("a"), model("a", "test.tag")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let orm = system
        .button_immediate_install(&su(&init.orm), &[n("a")])
        .await
        .unwrap()
        .orm;
    // `cap + 1` tags in one statement; `cat_at` holds the first `cap`, `cat_over` all of them
    let table = rows(
        &orm,
        "SELECT table_name::text FROM information_schema.columns \
         WHERE column_name = 'test_tag_id' AND table_name LIKE '%test_cat%'",
    )
    .await[0][0]
        .clone()
        .unwrap();
    exec(
        &orm,
        &format!(
            "INSERT INTO test_tag (name) SELECT 'bulk ' || g FROM generate_series(1, {over}) g;
             INSERT INTO {table} (test_cat_id, test_tag_id)
             SELECT c.id, t.id FROM test_cat c,
                    (SELECT id, row_number() OVER (ORDER BY id) AS n FROM test_tag) t
             WHERE (c.name = 'At cap' AND t.n <= {cap}) OR c.name = 'Over cap'",
            over = cap + 1
        ),
    )
    .await;
    let counts = rows(
        &orm,
        &format!(
            "SELECT c.name, count(*)::text FROM {table} r JOIN test_cat c ON c.id = r.test_cat_id \
             GROUP BY c.name ORDER BY c.name"
        ),
    )
    .await;
    assert_eq!(
        counts,
        [
            [Some("At cap".to_owned()), Some(cap.to_string())],
            [Some("Over cap".to_owned()), Some((cap + 1).to_string())],
        ]
    );

    // exactly the limit loads
    let orm = system
        .button_immediate_install(&su(&orm), &[n("m")])
        .await
        .unwrap()
        .orm;
    let seq = rows(
        &orm,
        "SELECT sequence::text FROM test_cat WHERE name = 'Probe'",
    )
    .await;
    assert_eq!(seq, [[Some(cap.to_string())]]);

    // one more fails, naming the model, field, record and limit
    let over = id_of(&orm, "test_cat", "Over cap").await;
    let err = failed(system.button_immediate_install(&su(&orm), &[n("m2")]).await);
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("m2", ModuleStep::DataFile, Some("data/probe.xml"))
    );
    let Error::DataLoad(e) = source else {
        panic!("expected DataLoad, got {source:?}")
    };
    let expr = format!("test.cat({over}).tag_ids");
    let expected = erp_expr::ExprError::limit(format!(
        "field 'tag_ids' of test.cat({over}) holds more than {cap} records, the limit of a \
         recordset in a data expression"
    ));
    assert!(
        matches!(e.kind(), LoadErrorKind::Eval { expr: x, error } if *x == expr && *error == expected),
        "{e:?}"
    );
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing data/probe.xml:1, somewhere inside\n{}\neval \"{expr}\" failed: \
             {expected}",
            e.op().unwrap()
        )
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_095_environment_host_context_in_data_expressions() {
    let xml = r#"<odoo>
        <record id="cat_env" model="test.cat">
            <field name="name" model="test.cat" eval="obj().env.context.get('lang', 'none')"/>
            <field name="sequence" model="test.cat" eval="obj().env.uid"/>
            <field name="weight" model="test.cat" eval="float(obj().env.company.id)"/>
            <field name="flag" model="test.cat" eval="obj().env.su"/>
            <field name="payload" model="test.cat" eval="{'user': obj().env.user.id, 'companies': obj().env.companies.ids}"/>
        </record>
    </odoo>"#;
    let (db, result) = install_cats(xml).await;
    let orm = result.unwrap();
    let fields = rows(
        &orm,
        "SELECT name, sequence::text, weight::text, flag::text, payload->>'user', (payload->'companies')::text FROM test_cat WHERE name = 'none'",
    )
    .await;
    assert_eq!(
        fields,
        [[
            Some("none".to_owned()),
            Some("1".to_owned()),
            Some("0".to_owned()),
            Some("true".to_owned()),
            Some("1".to_owned()),
            Some("[]".to_owned())
        ]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_096_user_group_checks_and_context_overrides_in_data_expressions() {
    let xml = r#"<odoo>
        <record id="cat_tgt_1" model="test.cat"><field name="name">Target 1</field></record>
        <record id="cat_tgt_2" model="test.cat"><field name="name">Target 2</field></record>
        <record id="cat_tgt_3" model="test.cat"><field name="name">Target 3</field></record>
        <record id="cat_override" model="test.cat">
            <field name="name">Override Cat</field>
            <field name="parent_id" model="test.cat" eval="obj().with_context(active_test=False).sudo(False).search([('name', 'like', 'Target')], offset=1, order='name desc', limit=1).id"/>
            <field name="flag" model="test.cat" eval="obj().sudo(False).env.su == False"/>
            <field name="payload" model="test.cat" eval="{'active_test': obj().with_context(active_test=False).env.context.get('active_test') == False}"/>
        </record>
    </odoo>"#;
    let (db, result) = install_cats(xml).await;
    let orm = result.unwrap();
    let target_2 = id_of(&orm, "test_cat", "Target 2").await;
    assert_eq!(parent_of(&orm, "Override Cat").await, Some(target_2));
    let fields = rows(
        &orm,
        "SELECT flag::text, payload->>'active_test' FROM test_cat WHERE name = 'Override Cat'",
    )
    .await;
    assert_eq!(fields, [[Some("true".to_owned()), Some("true".to_owned())]]);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_096_unknown_security_group_fails_loudly() {
    let expr = "obj().env.user.has_group('base.group_nonexistent')";
    let xml = format!(
        r#"<odoo><record id="c" model="test.cat"><field name="flag" model="test.cat" eval="{expr}"/></record></odoo>"#
    );
    let (db, result) = install_cats(&xml).await;
    assert!(
        result.is_err(),
        "unknown group must fail during data loading"
    );
    db.cleanup().await.unwrap();
}

/// A group check on a `res.users` record that does not exist fails loudly and names the user; it
/// is never `false`.
#[tokio::test]
async fn has_group_on_a_missing_user_fails_loudly() {
    let expr = "obj().env['res.users'].browse(987654).has_group('base.group_user')";
    let xml = format!(
        r#"<odoo><record id="c" model="test.cat"><field name="flag" model="test.cat" eval="{expr}"/></record></odoo>"#
    );
    let (db, result) = install_cats(&xml).await;
    let err = failed(result);
    let e = cats_load_error(&err);
    let LoadErrorKind::Orm(inner) = e.kind() else {
        panic!("{e:?}")
    };
    assert_eq!(inner.kind(), ErrorKind::Missing);
    assert_eq!(
        inner.message(),
        "Record does not exist or has been deleted: res.users(987654) \
         (has_group('base.group_user') in a data expression)"
    );
    db.cleanup().await.unwrap();
}
