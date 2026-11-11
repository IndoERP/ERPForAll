//! What the demo-data absorption rule and the file readers of the data loader do with failures
//! that are not the data's own (integration review): the install fails with the loader's
//! error, and files named by data count against the operation's budget.
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
use erp_data::load::{FileReadError, LoadErrorKind};
use erp_modules::error::{Error, ModuleStep};
use erp_modules::loader::{DataFile, DataKind, FileLoader, Operations};
use erp_modules::manifest::ModuleName;
use erp_modules::system::InitOptions;
use erp_orm::{BoxFuture, Transaction};
use erp_testkit::TestDb;
use std::sync::Arc;

const DEMO_RECORD: &str = "<odoo><record id=\"r\" model=\"test.cat\">\
    <field name=\"name\" file=\"x/static/a.txt\"/></record></odoo>";

/// Install module `x` with `demo` as its only demo file (and the files `files`), and return the
/// error of the install.
async fn failing_install(demo: &str, files: &[(&str, &str)]) -> Error {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndemo = ['demo/bad.xml']\n")
        .file("x", "demo/bad.xml", demo);
    for (path, content) in files {
        addons.file("x", path, content);
    }
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat")])
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
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    db.cleanup().await.unwrap();
    err
}

/// The `DataLoad` error inside a failed demo step, with the step's module and file checked.
fn demo_load_error(err: &Error) -> &erp_data::load::LoadError {
    let (module, step, file, source) = step_failed(err);
    assert_eq!(
        (module, step, file),
        ("x", ModuleStep::DemoFile, Some("demo/bad.xml"))
    );
    match source {
        Error::DataLoad(e) => e,
        other => panic!("expected DataLoad, got {other:?}"),
    }
}

/// A feature that arrives later (a demo `<menuitem>` before `ir.ui.menu` exists) is a defect of
/// the loader's stage, not of the demo data: the install fails and names where the model comes
/// from (addon `base`).
#[tokio::test]
async fn a_demo_menuitem_before_the_menu_model_fails_the_install() {
    let err = failing_install("<odoo><menuitem id=\"mi\" name=\"M\"/></odoo>", &[]).await;
    let e = demo_load_error(&err);
    assert!(
        matches!(
            e.kind(),
            LoadErrorKind::ModelArrivesLater { what, model, slice }
                if (what.as_str(), model.as_str(), slice.as_str())
                    == (
                        "<menuitem>",
                        "ir.ui.menu",
                        "addon base: add 'base' to the module's depends"
                    )
        ),
        "{e:?}"
    );
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing demo/bad.xml:1, somewhere inside\n{}\n\
             <menuitem> needs model 'ir.ui.menu', which arrives with addon base: add 'base' to \
             the module's depends",
            e.op().unwrap()
        )
    );
}

/// An expression that needs a name the host has not bound yet (`user`) is a missing feature, not a
/// mistake of the data.
#[tokio::test]
async fn a_demo_expression_needing_an_unbound_host_name_fails_the_install() {
    let err = failing_install(
        "<odoo><record id=\"r\" model=\"test.cat\"><field name=\"name\" eval=\"user\"/></record></odoo>",
        &[],
    )
    .await;
    let e = demo_load_error(&err);
    let expected = erp_expr::ExprError::unavailable(
        "user",
        "user and group checks are provided by the host and are not available before M3",
    );
    assert!(
        matches!(e.kind(), LoadErrorKind::Eval { error, .. } if *error == expected),
        "{e:?}"
    );
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing demo/bad.xml:1, somewhere inside\n{}\neval \"user\" failed: {expected}",
            e.op().unwrap()
        )
    );
}

/// A file a demo field names that the host cannot read as a regular file (here a directory) is
/// the host's failure, with module-relative text.
#[tokio::test]
async fn a_demo_file_that_cannot_be_read_fails_the_install() {
    let err = failing_install(
        "<odoo><record id=\"r\" model=\"test.cat\"><field name=\"name\" file=\"x/static\"/></record></odoo>",
        &[("static/a.txt", "a")],
    )
    .await;
    let e = demo_load_error(&err);
    assert!(
        matches!(e.kind(), LoadErrorKind::File(FileReadError::Failed(_))),
        "{e:?}"
    );
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing demo/bad.xml:1, somewhere inside\n{}\nx/static: {}",
            e.op().unwrap(),
            // the reader refuses a path that is not a regular file with this I/O kind; the text
            // is the kind's own (no absolute path)
            std::io::ErrorKind::InvalidInput
        )
    );
}

#[tokio::test]
async fn a_demo_file_the_addon_lacks_is_a_demo_failure() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndemo = ['demo/bad.xml']\n")
        .file("x", "demo/bad.xml", DEMO_RECORD);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat")])
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
    assert_eq!(state(&outcome.orm, "x").await, "installed");
    let [notice] = outcome.data_notices.as_slice() else {
        panic!("expected one notice, got {:?}", outcome.data_notices);
    };
    assert!(matches!(
        notice.kind(),
        erp_data::load::NoticeKind::DemoFailed { error } if error.contains("File not found: x/static/a.txt")
    ));
    db.cleanup().await.unwrap();
}

/// A `..` in a name written in data is the author's mistake, said so: not "internal invariant".
#[tokio::test]
async fn a_file_name_with_dot_dot_is_refused_with_a_clear_message() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/d.xml']\n")
        .file(
            "x",
            "data/d.xml",
            "<odoo><record id=\"r\" model=\"test.cat\">\
             <field name=\"name\" file=\"x/../x/static/a.txt\"/></record></odoo>",
        )
        .file("x", "static/a.txt", "a");
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
        ("x", ModuleStep::DataFile, Some("data/d.xml"))
    );
    let Error::DataLoad(e) = source else {
        panic!("expected DataLoad, got {source:?}");
    };
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing data/d.xml:1, somewhere inside\n{}\n\
             x/../x/static/a.txt: a file named by data must be a relative path inside the module \
             (no '..', no leading '/')",
            e.op().unwrap()
        )
    );
    db.cleanup().await.unwrap();
}

/// The files that data fields name count against the operation's data budget like the data
/// files: the same 1 KiB file named by enough records passes a lowered budget.
#[tokio::test]
async fn files_named_by_data_count_against_the_operation_budget() {
    let record = |i: usize| {
        format!(
            "<record id=\"r{i}\" model=\"test.cat\"><field name=\"name\" file=\"x/static/big.txt\"/></record>"
        )
    };
    let data = format!("<odoo>{}</odoo>", (0..4).map(record).collect::<String>());
    let big = "z".repeat(1024);
    let limit = u64::try_from(data.len()).unwrap() + 2 * 1024 + 100;
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndata = ['data/d.xml']\n")
        .file("x", "data/d.xml", &data)
        .file("x", "static/big.txt", &big);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .max_operation_data_bytes(limit)
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
    let (_, _, _, source) = step_failed(&err);
    let Error::DataLoad(e) = source else {
        panic!("expected DataLoad, got {source:?}");
    };
    assert!(
        matches!(e.kind(), LoadErrorKind::File(FileReadError::Failed(_))),
        "{e:?}"
    );
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing data/d.xml:1, somewhere inside\n{}\n\
             Module \"x\": data file 'static/big.txt' passes the {limit} bytes of data files one \
             operation reads (ModuleSystemBuilder::max_operation_data_bytes)",
            e.op().unwrap()
        )
    );
    db.cleanup().await.unwrap();
}

/// A loader whose demo file ends the backend it runs on and then reports a data failure.
struct DyingDemo;

impl FileLoader for DyingDemo {
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
            if file.kind() == DataKind::Demo {
                // the connection dies; the failure the loader reports is an ordinary data failure
                let _ = tx
                    .query("SELECT pg_terminate_backend(pg_backend_pid())", &[])
                    .await;
                return Err(erp_fields::Error::validation("demo broke").into());
            }
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

/// When the savepoint of the demo files cannot be rolled back (the connection is gone), the
/// install fails with both errors: it is never "installed without demo data".
#[tokio::test]
async fn a_demo_savepoint_that_cannot_roll_back_fails_the_install_with_both_errors() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndemo = ['demo/a.xml']\n")
        .file("x", "demo/a.xml", "<odoo/>");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat")])
        .data_loader(Arc::new(DyingDemo))
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
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    let Error::Orm(e) = &err else {
        panic!("expected the ORM error of the lost connection, got {err:?}");
    };
    assert!(
        e.message().starts_with(
            "the demo files of module x failed and the savepoint could not be rolled back: "
        ) && e.message().ends_with(
            "; the demo failure was: Module \"x\": demo file 'demo/a.xml' failed\ndemo broke"
        ),
        "{}",
        e.message()
    );
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    db.cleanup().await.unwrap();
}

/// What the `create` override of `test.cat` raises while demo data is loaded.
static DEMO_CREATE_ERROR: std::sync::Mutex<Option<fn() -> erp_fields::Error>> =
    std::sync::Mutex::new(None);

fn demo_create<'a>(
    tx: &'a mut Transaction,
    records: &'a erp_orm::Records,
    args: (Vec<erp_orm::RawVals>,),
    sup: erp_orm::Super<(Vec<erp_orm::RawVals>,), erp_orm::Records>,
) -> BoxFuture<'a, erp_fields::Result<erp_orm::Records>> {
    Box::pin(async move {
        let raise = *DEMO_CREATE_ERROR.lock().unwrap();
        if let Some(raise) = raise
            && records.env().context().get("install_demo").is_some()
        {
            return Err(raise());
        }
        sup.call(tx, records, args).await
    })
}

fn demo_create_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static F: erp_orm::MethodFn<(Vec<erp_orm::RawVals>,), erp_orm::Records> = demo_create;
    &F
}

static DEMO_CREATE: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: "x",
    model: "test.cat",
    method: "create",
    kind: erp_orm::ImplKind::Override,
    sig_name: std::any::type_name::<erp_orm::MethodFn<(Vec<erp_orm::RawVals>,), erp_orm::Records>>,
    entry: demo_create_entry,
    file: file!(),
    line: line!(),
};

const DEMO_CAT: &str =
    "<odoo><record id=\"r\" model=\"test.cat\"><field name=\"name\">c</field></record></odoo>";

/// Install `x` with [`DEMO_CAT`] as its demo file while the `create` override raises `raise`.
async fn install_with_demo_create_error(
    raise: fn() -> erp_fields::Error,
) -> (
    TestDb,
    Result<erp_modules::system::OperationOutcome, Error>,
    erp_orm::Orm,
) {
    *DEMO_CREATE_ERROR.lock().unwrap() = Some(raise);
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\ndemo = ['demo/cat.xml']\n")
        .file("x", "demo/cat.xml", DEMO_CAT);
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat")])
        .impls([&DEMO_CREATE])
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
        .await;
    (db, outcome, init.orm)
}

/// A broken contract between layers raised while demo data is written (a load state the loader
/// did not write: `erp_fields::Error::invariant`) fails the install; it is not taken for the demo
/// data's own mistake (review F-1). A plain `Programming` error (an unknown field, a
/// wrong value type) still is: the module installs without demo data. Before the fix both were
/// absorbed.
#[tokio::test]
async fn a_broken_contract_in_demo_data_fails_the_install() {
    const BROKEN: &str = "the context names install_module 'x' without install_mode";
    let (db, outcome, orm) =
        install_with_demo_create_error(|| erp_fields::Error::invariant(BROKEN)).await;
    let err = outcome.unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("x", ModuleStep::DemoFile, Some("demo/cat.xml"))
    );
    let Error::DataLoad(load) = source else {
        panic!("expected DataLoad, got {source:?}")
    };
    let LoadErrorKind::Orm(inner) = load.kind() else {
        panic!("expected an ORM error, got {load:?}")
    };
    assert_eq!(inner.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(inner.message(), BROKEN);
    assert!(inner.is_invariant_violation());
    assert_eq!(state(&orm, "x").await, "uninstalled");
    db.cleanup().await.unwrap();

    let (db, outcome, _) = install_with_demo_create_error(|| {
        erp_fields::Error::programming("Invalid field 'colour' on 'test.cat'")
    })
    .await;
    let outcome = outcome.unwrap();
    assert_eq!(state(&outcome.orm, "x").await, "installed");
    let [notice] = outcome.data_notices.as_slice() else {
        panic!("expected one notice, got {:?}", outcome.data_notices);
    };
    // the notice text is the whole error chain (file, place, record); only its last link, the
    // error raised, is this test's
    assert!(
        matches!(
            notice.kind(),
            erp_data::load::NoticeKind::DemoFailed { error }
                if error.ends_with("Invalid field 'colour' on 'test.cat'")
        ),
        "{notice:?}"
    );
    db.cleanup().await.unwrap();
}

/// Even a wrapper with an ordinary data-error kind cannot erase an invariant marker.
#[tokio::test]
async fn an_invariant_marker_on_a_validation_error_fails_the_demo_install() {
    const BROKEN: &str = "wrapped engine contract failed";
    let (db, outcome, orm) = install_with_demo_create_error(|| {
        erp_fields::Error::validation(BROKEN)
            .with_condition(erp_fields::Condition::InvariantViolation)
    })
    .await;
    let err = outcome.unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("x", ModuleStep::DemoFile, Some("demo/cat.xml"))
    );
    let Error::DataLoad(load) = source else {
        panic!("expected DataLoad, got {source:?}")
    };
    let LoadErrorKind::Orm(inner) = load.kind() else {
        panic!("expected ORM error, got {load:?}")
    };
    assert_eq!(inner.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(inner.message(), BROKEN);
    assert!(inner.is_invariant_violation());
    assert_eq!(state(&orm, "x").await, "uninstalled");
    db.cleanup().await.unwrap();
}
