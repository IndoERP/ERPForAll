//! Module operations, second review round (lead items A–V on): consistency checks against
//! the whole catalog, uninstall schema removal, forced loads, rollback snapshots, drop reports.
//!
//! The recording statics (`LOG`, `FAIL`, `PURGE_RECORDS`) are per test process and several tests
//! assert the exact log: this relies on nextest running every test in its own process
//! (`scripts/ci.sh`). Under plain `cargo test` the tests of this binary would share them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use erp_fields::FieldType;
use erp_modules::LimitKind;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::graph::{DropReason, ModuleState};
use erp_modules::hooks::{HookImpl, HookKind};
use erp_modules::lifecycle::InstallOptions;
use erp_modules::loader::{DataFile, FileLoader, Operations};
use erp_modules::manifest::ModuleName;
use erp_modules::system::{
    InitOptions, ModuleSystem, ModuleSystemBuilder, OPERATION_LOCK, POST_COMMIT_LOCK,
    PendingPostCommit, PendingState, PostCommitReport, PostCommitStop, schedule_post_commit,
};
use erp_orm::{
    BoxFuture, ImplKind, MethodFn, MethodImpl, Orm, Records, SUPERUSER_ID, Super, Transaction,
};
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;

/// The relation table of a stored many2many field, as the registry names it.
fn relation_table(orm: &Orm, model: &str, field: &str) -> String {
    let spec = orm.registry().model(model).unwrap().field(field).unwrap();
    match &spec.ty {
        FieldType::Many2many {
            relation: Some(relation),
            ..
        } => relation.table.clone(),
        other => panic!("{model}.{field} is not a stored many2many: {other:?}"),
    }
}

/// A model of `addon` with a `name` field and a many2many `field` to `comodel`.
fn model_with_m2m(
    addon: &'static str,
    name: &'static str,
    field: &'static str,
    comodel: &'static str,
) -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(
        vec![
            FieldDecl::char("name"),
            FieldDecl::many2many(field, comodel),
        ]
        .into_boxed_slice(),
    );
    Box::leak(Box::new(
        ModelDecl::new(addon, name)
            .description("Test model")
            .fields(fields)
            .at(file!(), line!()),
    ))
}

/// An extension of `name` by `addon` adding a many2many `field` to `comodel`.
fn m2m_extension(
    addon: &'static str,
    name: &'static str,
    field: &'static str,
    comodel: &'static str,
) -> &'static ModelDecl {
    let fields: &'static [FieldDecl] =
        Box::leak(vec![FieldDecl::many2many(field, comodel)].into_boxed_slice());
    let inherit: &'static [&'static str] = Box::leak(vec![name].into_boxed_slice());
    Box::leak(Box::new(
        ModelDecl::extension(addon, name, inherit)
            .fields(fields)
            .at(file!(), line!()),
    ))
}

// ---- A: inconsistent states are checked against the whole catalog -------------------------

/// Lead A: an installed module whose dependency is `uninstalled` is an inconsistent database, in a
/// plain load and in an update run: an error listing the pair, nothing loaded or changed (not a
/// silent drop of the dependent, which the graph would report as a missing dependency because the
/// uninstalled row never reached it).
#[tokio::test]
async fn installed_module_with_uninstalled_dependency_is_inconsistent() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n")
        .add("z", "name = 'Z'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.a"), model("b", "test.b")])
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
    exec(
        &init.orm,
        "UPDATE ir_module_module SET state = 'uninstalled', latest_version = NULL WHERE name = 'a'",
    )
    .await;
    let before = states(&init.orm).await;
    let expected = "Inconsistent module states in the database:\n  - b is installed but its dependency a is 'uninstalled'";
    let check = |err: Error| {
        let Error::InconsistentStates { modules } = &err else {
            panic!("expected InconsistentStates, got {err:?}");
        };
        assert_eq!(modules.len(), 1);
        assert_eq!(
            (
                modules[0].module.as_str(),
                modules[0].dependency.as_str(),
                modules[0].dependency_state
            ),
            ("b", "a", ModuleState::Uninstalled)
        );
        assert_eq!(err.to_string(), expected);
    };
    check(system.load().await.err().unwrap());
    check(
        system
            .button_immediate_install(&env, &[n("z")])
            .await
            .unwrap_err(),
    );
    assert_eq!(states(&init.orm).await, before, "nothing changed");
    db.cleanup().await.unwrap();
}

// ---- C: uninstall removes relation tables and never cascades into other modules -------------

/// Lead C: the many2many relation tables of removed models and of removed fields are dropped
/// with them.
#[tokio::test]
async fn uninstall_drops_many2many_relation_tables() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model("a", "test.a"),
            model("a", "test.tag"),
            m2m_extension("b", "test.a", "tag_ids", "test.tag"),
            model_with_m2m("b", "test.b", "a_ids", "test.a"),
        ])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    let installed = system
        .button_immediate_install(&env, &[n("b")])
        .await
        .unwrap();
    let tables = [
        relation_table(&installed.orm, "test.a", "tag_ids"),
        relation_table(&installed.orm, "test.b", "a_ids"),
    ];
    for table in &tables {
        assert!(table_exists(&installed.orm, table).await, "{table}");
    }
    let outcome = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap();
    assert_eq!(outcome.uninstalled, vec![n("b")]);
    for table in &tables {
        assert!(!table_exists(&outcome.orm, table).await, "{table} is left");
    }
    assert!(table_exists(&outcome.orm, "test_a").await);
    assert!(table_exists(&outcome.orm, "test_tag").await);
    db.cleanup().await.unwrap();
}

fn view_on_b(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.batch(
            "CREATE VIEW c_names AS SELECT name FROM test_b; \
             CREATE VIEW c_notes AS SELECT b_note FROM test_a",
        )
        .await
    })
}

static VIEW_ON_B: HookImpl =
    HookImpl::new("c", HookKind::PostInit, "view_on_b", view_on_b).at(file!(), line!());

/// Lead C: uninstall never drops objects of other modules through `CASCADE`: a view of another
/// module on the removed table or column blocks the uninstall, and nothing changes.
#[tokio::test]
async fn uninstall_does_not_cascade_into_other_modules() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n")
        .add(
            "c",
            "name = 'C'\ndepends = ['b']\npost_init_hook = 'view_on_b'\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model("a", "test.a"),
            model("b", "test.b"),
            extension("b", "test.a", "b_note"),
        ])
        .hooks([&VIEW_ON_B])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = su(&init.orm);
    committed(
        system
            .button_immediate_install(&env, &[n("c")])
            .await
            .unwrap(),
    );
    // c's row goes away behind the module system's back (its views stay): the uninstall of b
    // would otherwise remove c first
    exec(
        &init.orm,
        "UPDATE ir_module_module SET state = 'uninstalled', latest_version = NULL WHERE name = 'c'",
    )
    .await;
    let err = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::SchemaDropBlocked { modules, .. } if *modules == vec![n("b")]),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Cannot remove the schema of modules b: cannot drop column b_note of table test_a because \
         other objects depend on it (view c_notes depends on column b_note of table test_a); \
         remove the objects that use it first"
    );
    assert_eq!(state(&init.orm, "b").await, "installed");
    assert!(table_exists(&init.orm, "c_names").await);
    assert!(table_exists(&init.orm, "c_notes").await);
    exec(&init.orm, "DROP VIEW c_notes").await;
    let err = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot remove the schema of modules b: cannot drop table test_b because other objects \
         depend on it (view c_names depends on table test_b); remove the objects that use it first"
    );
    assert!(table_exists(&init.orm, "c_names").await);
    exec(&init.orm, "DROP VIEW c_names").await;
    committed(
        system
            .button_immediate_uninstall(&env, &[n("b")])
            .await
            .unwrap(),
    );
    assert!(!table_exists(&init.orm, "test_b").await);
    db.cleanup().await.unwrap();
}

// ---- G: a forced load waits for a running operation and builds one registry ---------------

static REGISTERED: Mutex<u32> = Mutex::new(0);

fn count_register<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        *REGISTERED.lock().unwrap() += 1;
        sup.call(tx, records, args).await
    })
}

fn count_register_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = count_register;
    &F
}

static COUNT_REGISTER: MethodImpl = MethodImpl {
    addon: "a",
    model: "test.a",
    method: "_register_hook",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: count_register_entry,
    file: file!(),
    line: line!(),
};

/// Lead G: a plain load that finds pending operations while another operation holds the lock
/// waits for it (it does not fail with `OperationInProgress`), and builds the registry and runs
/// `_register_hook` once, reusing the forced update's ORM.
#[tokio::test]
async fn forced_load_waits_for_a_running_operation_and_builds_once() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.a")])
        .impls([&COUNT_REGISTER])
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
    let mut other = init.orm.begin(SUPERUSER_ID).await.unwrap();
    other
        .query(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await
        .unwrap();
    *REGISTERED.lock().unwrap() = 0;
    // release the lock once the load is queued on it (observed in pg_locks), not after a delay
    let release = async {
        assert!(
            eventually(Duration::from_secs(20), || advisory_waiting(
                &init.orm,
                OPERATION_LOCK
            ))
            .await,
            "the load never waited for the operation lock"
        );
        other.rollback().await.unwrap();
    };
    let (loaded, ()) = tokio::join!(system.load(), release);
    let loaded = loaded.unwrap();
    let forced = loaded.forced_update.as_ref().expect("a forced update");
    assert_eq!(forced.upgraded, vec![n("a")]);
    assert_eq!(state(&loaded.orm, "a").await, "installed");
    assert_eq!(
        *REGISTERED.lock().unwrap(),
        1,
        "one registry, one _register_hook"
    );
    assert!(loaded.orm.registry().model("test.a").is_some());
    drop(other);
    db.cleanup().await.unwrap();
}

// ---- K / N: failures leave the whole database and the registry as they were --------------

/// Where the scenario below fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    Nowhere,
    /// `f`'s `post_init` hook.
    PostInit,
    /// The data loader on `f`'s data file (an ORM error).
    Loader,
    /// `init()` of `f`'s model, after its table was created.
    Init,
    /// `f`'s `pre_init` hook never returns (the caller drops the operation future).
    Hang,
    /// `f`'s uninstall hook.
    UninstallHook,
    /// The data loader's purge, after it deleted records and dropped a column.
    Purge,
}

/// The scenario's failure point (per test process: nextest runs each test in its own process).
static FAIL: Mutex<Fail> = Mutex::new(Fail::Nowhere);

fn fail_at() -> Fail {
    *FAIL.lock().unwrap()
}

fn set_fail(fail: Fail) {
    *FAIL.lock().unwrap() = fail;
}

fn a_post(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.batch(
            "INSERT INTO test_a (name) VALUES ('made by a'); \
             INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
             SELECT 'a', 'rec_a', 'test.a', max(id), false FROM test_a",
        )
        .await?;
        schedule_post_commit(tx, &n("a"), &pk("analyze_a"), &analyze("test_a")).await?;
        log("a post_init");
        Ok(())
    })
}

fn f_pre(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        if fail_at() == Fail::Hang {
            log("f pre_init hangs");
            std::future::pending::<()>().await;
        }
        Ok(())
    })
}

fn f_post(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        if fail_at() == Fail::PostInit {
            return Err(erp_fields::Error::user("post_init of f failed"));
        }
        log("f post_init");
        Ok(())
    })
}

fn g_post(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        log("g post_init");
        Ok(())
    })
}

fn f_uninstall(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        if fail_at() == Fail::UninstallHook {
            return Err(erp_fields::Error::user("uninstall of f failed"));
        }
        log("f uninstall");
        Ok(())
    })
}

fn g_uninstall(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        // a change the rollback must undo
        tx.execute("UPDATE test_p SET name = 'touched by g'", &[])
            .await?;
        log("g uninstall");
        Ok(())
    })
}

static A_POST: HookImpl =
    HookImpl::new("a", HookKind::PostInit, "a_post", a_post).at(file!(), line!());
static F_PRE: HookImpl = HookImpl::new("f", HookKind::PreInit, "f_pre", f_pre).at(file!(), line!());
static F_POST: HookImpl =
    HookImpl::new("f", HookKind::PostInit, "f_post", f_post).at(file!(), line!());
static G_POST: HookImpl =
    HookImpl::new("g", HookKind::PostInit, "g_post", g_post).at(file!(), line!());
static F_UNINSTALL: HookImpl =
    HookImpl::new("f", HookKind::Uninstall, "f_uninstall", f_uninstall).at(file!(), line!());
static G_UNINSTALL: HookImpl =
    HookImpl::new("g", HookKind::Uninstall, "g_uninstall", g_uninstall).at(file!(), line!());

fn init_f<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        if fail_at() == Fail::Init {
            let rows = tx
                .query("SELECT (to_regclass('test_f') IS NOT NULL)", &[])
                .await?;
            let exists: bool = rows[0].get(0);
            log(format!("f init table_exists={exists}"));
            return Err(erp_fields::Error::user("init of test.f failed"));
        }
        sup.call(tx, records, args).await
    })
}

fn init_f_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = init_f;
    &F
}

static INIT_F: MethodImpl = MethodImpl {
    addon: "f",
    model: "test.f",
    method: "init",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: init_f_entry,
    file: file!(),
    line: line!(),
};

/// Whether [`ScenarioLoader::purge`] removes the records (else it leaves them: a broken purge).
static PURGE_RECORDS: AtomicBool = AtomicBool::new(true);

/// Records each file; fails on `f`'s files when the scenario says so; purges by removing the
/// records the removed modules' external ids name in `test.p`, with those external ids.
struct ScenarioLoader;

impl FileLoader for ScenarioLoader {
    fn load<'a, 'o: 'a>(
        &'a self,
        _tx: &'a mut Transaction,
        file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            for op in operations {
                op?;
            }
            if file.module().as_str() == "f" && fail_at() == Fail::Loader {
                return Err(erp_fields::Error::new(
                    erp_fields::ErrorKind::Validation,
                    "the loader refused f's record",
                )
                .into());
            }
            log(format!(
                "{}:{}:{}",
                file.module(),
                file.path(),
                file.mode().as_str()
            ));
            Ok(())
        })
    }

    fn purge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        modules: &'a [ModuleName],
    ) -> BoxFuture<'a, erp_modules::Result<Vec<erp_data::load::DataNotice>>> {
        Box::pin(async move {
            let names: Vec<String> = modules.iter().map(ModuleName::to_string).collect();
            let rows = tx
                .query(
                    "SELECT (to_regclass('test_g') IS NOT NULL), \
                     (SELECT count(*) FROM ir_model_data WHERE module = ANY($1))",
                    &[&names],
                )
                .await?;
            let tables: bool = rows[0].get(0);
            let xmlids: i64 = rows[0].get(1);
            log(format!(
                "purge {} tables={tables} xmlids={xmlids}",
                names.join(",")
            ));
            if fail_at() == Fail::Purge {
                // changes the rollback must undo, data and schema
                tx.batch(
                    "DELETE FROM test_p; ALTER TABLE test_p DROP COLUMN a_col; \
                     DELETE FROM ir_model_data WHERE module = 'g'",
                )
                .await?;
                return Err(erp_fields::Error::user(format!(
                    "purge of {} failed",
                    names.join(", ")
                ))
                .into());
            }
            if PURGE_RECORDS.load(Ordering::SeqCst) {
                tx.batch(&format!(
                    "DELETE FROM test_p WHERE id IN (SELECT res_id FROM ir_model_data \
                     WHERE module IN ({0}) AND model = 'test.p'); \
                     DELETE FROM ir_model_data WHERE module IN ({0}) AND model = 'test.p'",
                    names
                        .iter()
                        .map(|m| format!("'{m}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .await?;
            }
            Ok(Vec::new())
        })
    }
}

/// `p` (installed first, model `test.p`) ← `a` (adds `a_col` to `test.p`, model `test.a`, data,
/// post_init) ← `f` (model `test.f` with `init()`, pre/post_init, data, uninstall hook) ← `g`
/// (model `test.g`, post_init, uninstall hook).
async fn scenario() -> (Addons, TestDb, Arc<ModuleSystem>, Orm) {
    let addons = Addons::new();
    addons
        .add("p", "name = 'P'\n")
        .add(
            "a",
            "name = 'A'\nversion = '20.0.1.0'\ndepends = ['p']\npost_init_hook = 'a_post'\n\
             data = ['data/a.xml']\n",
        )
        .file("a", "data/a.xml", ONE_RECORD_XML)
        .add(
            "f",
            "name = 'F'\ndepends = ['a']\npre_init_hook = 'f_pre'\npost_init_hook = 'f_post'\n\
             uninstall_hook = 'f_uninstall'\ndata = ['data/f.xml']\n",
        )
        .file("f", "data/f.xml", ONE_RECORD_XML)
        .add(
            "g",
            "name = 'G'\ndepends = ['f']\npost_init_hook = 'g_post'\n\
             uninstall_hook = 'g_uninstall'\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(scenario_system(&db, &addons, &[]));
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    committed(
        system
            .button_immediate_install(&su(&init.orm), &[n("p")])
            .await
            .unwrap(),
    );
    exec(&init.orm, "INSERT INTO test_p (name) VALUES ('p1')").await;
    take_log();
    (addons, db, system, init.orm)
}

fn scenario_system(db: &TestDb, addons: &Addons, more: &[&'static ModelDecl]) -> ModuleSystem {
    builder(db, addons)
        .decls(
            [
                model("p", "test.p"),
                extension("a", "test.p", "a_col"),
                model("a", "test.a"),
                model("f", "test.f"),
                model("g", "test.g"),
            ]
            .into_iter()
            .chain(more.iter().copied()),
        )
        .impls([&INIT_F])
        .hooks([
            &A_POST,
            &F_PRE,
            &F_POST,
            &G_POST,
            &F_UNINSTALL,
            &G_UNINSTALL,
        ])
        .data_loader(Arc::new(ScenarioLoader))
        .build()
        .unwrap()
}

/// The database, and the registry a plain load builds from it.
async fn state_of(system: &ModuleSystem, orm: &Orm) -> (Vec<(String, String)>, Vec<String>) {
    let db = snapshot(orm).await;
    let loaded = system.load().await.unwrap();
    assert!(loaded.forced_update.is_none());
    (db, registry_shape(&loaded.orm))
}

#[tokio::test]
async fn install_failures_leave_database_and_registry_unchanged() {
    let (_addons, db, system, orm) = scenario().await;
    let before = state_of(&system, &orm).await;
    let failures: [(Fail, ModuleStep, Option<&str>, [&str; 2]); 3] = [
        (
            Fail::PostInit,
            ModuleStep::Hook {
                kind: HookKind::PostInit,
                name: "f_post",
            },
            None,
            [
                "Module \"f\": post_init_hook 'f_post' failed",
                "post_init of f failed",
            ],
        ),
        (
            Fail::Loader,
            ModuleStep::DataFile,
            Some("data/f.xml"),
            [
                "Module \"f\": data file 'data/f.xml' failed",
                "the loader refused f's record",
            ],
        ),
        (
            Fail::Init,
            ModuleStep::ModelInit { model: "test.f" },
            None,
            [
                "Module \"f\": init() of test.f failed",
                "init of test.f failed",
            ],
        ),
    ];
    for (fail, expected_step, expected_file, message) in failures {
        set_fail(fail);
        let err = system
            .button_immediate_install(&su(&orm), &[n("g")])
            .await
            .unwrap_err();
        let (module, step, file, source) = step_failed(&err);
        assert_eq!(
            (module, step, file),
            ("f", expected_step, expected_file),
            "{fail:?}"
        );
        assert!(matches!(source, Error::Orm(_)), "{source:?}");
        assert_eq!(chain(&err), message);
        // a ran before f failed (its post_init wrote rows), g never ran
        let log = take_log();
        assert!(log.contains(&"a post_init".to_owned()), "{fail:?}: {log:?}");
        assert!(
            !log.contains(&"g post_init".to_owned()),
            "{fail:?}: {log:?}"
        );
        if fail == Fail::Init {
            assert!(
                log.contains(&"f init table_exists=true".to_owned()),
                "{log:?}"
            );
        }
        assert_eq!(state_of(&system, &orm).await, before, "{fail:?}");
    }

    // the operation future dropped while f's pre_init runs: the connection is detached, the
    // server rolls back, nothing persists and the lock is free again
    set_fail(Fail::Hang);
    let running = {
        let system = Arc::clone(&system);
        let env = su(&orm);
        tokio::spawn(async move { system.button_immediate_install(&env, &[n("g")]).await })
    };
    assert!(
        eventually(Duration::from_secs(5), || async {
            LOG.lock().unwrap().iter().any(|l| l == "f pre_init hangs")
        })
        .await
    );
    assert_eq!(
        take_log(),
        ["a:data/a.xml:init", "a post_init", "f pre_init hangs"]
    );
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    // the server ends the session of the detached connection shortly: probe pg_locks
    let released = eventually(Duration::from_secs(4), || async {
        !advisory_held(&orm, OPERATION_LOCK).await
    })
    .await;
    assert!(released, "the dropped operation's lock was never released");
    assert_eq!(
        system.button_reset_state(&su(&orm)).await.unwrap(),
        Vec::<ModuleName>::new()
    );
    assert_eq!(state_of(&system, &orm).await, before);

    // and the install then succeeds
    set_fail(Fail::Nowhere);
    committed(
        system
            .button_immediate_install(&su(&orm), &[n("g")])
            .await
            .unwrap(),
    );
    assert_eq!(state(&orm, "g").await, "installed");
    db.cleanup().await.unwrap();
}

/// Lead review N: an upgrade failing in a later module leaves the database and the registry as
/// before, after the earlier module's upgrade added a column to an existing table and loaded its
/// data, and after the failing module's own schema step added a column (delta review R9).
#[tokio::test]
async fn upgrade_failure_midway_leaves_database_and_registry_unchanged() {
    let (addons, db, system, orm) = scenario().await;
    committed(
        system
            .button_immediate_install(&su(&orm), &[n("g")])
            .await
            .unwrap(),
    );
    take_log();
    // a new version of a adds a field to test.p
    addons.add(
        "a",
        "name = 'A'\nversion = '20.0.1.1'\ndepends = ['p']\npost_init_hook = 'a_post'\n\
         data = ['data/a.xml']\n",
    );
    let upgraded = scenario_system(
        &db,
        &addons,
        &[
            extension("a", "test.p", "a_col2"),
            extension("f", "test.f", "f_col2"),
        ],
    );
    enroll_fixture_transition(&upgraded, &["a"]).await;
    let before = state_of(&upgraded, &orm).await;
    set_fail(Fail::Loader);
    let env = su_of(&upgraded);
    let err = upgraded
        .button_immediate_upgrade(&env, &[n("a")])
        .await
        .unwrap_err();
    let (module, step, file, _) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("f", ModuleStep::DataFile, Some("data/f.xml"))
    );
    assert_eq!(
        chain(&err),
        [
            "Module \"f\": data file 'data/f.xml' failed",
            "the loader refused f's record"
        ]
    );
    assert_eq!(take_log(), ["a:data/a.xml:update"], "a was upgraded first");
    assert_eq!(state_of(&upgraded, &orm).await, before);
    assert!(
        rows(
            &orm,
            "SELECT column_name::text FROM information_schema.columns \
             WHERE (table_name, column_name) IN (('test_p', 'a_col2'), ('test_f', 'f_col2'))"
        )
        .await
        .is_empty()
    );
    drop(system);
    db.cleanup().await.unwrap();
}

/// Lead review N, K: an uninstall failing in a hook after another module's hook changed data
/// leaves the database and the registry as before.
#[tokio::test]
async fn uninstall_failure_midway_leaves_database_and_registry_unchanged() {
    let (_addons, db, system, orm) = scenario().await;
    committed(
        system
            .button_immediate_install(&su(&orm), &[n("g")])
            .await
            .unwrap(),
    );
    take_log();
    let before = state_of(&system, &orm).await;
    set_fail(Fail::UninstallHook);
    let err = system
        .button_immediate_uninstall(&su(&orm), &[n("f")])
        .await
        .unwrap_err();
    let (module, step, _, _) = step_failed(&err);
    assert_eq!(
        (module, step),
        (
            "f",
            ModuleStep::Hook {
                kind: HookKind::Uninstall,
                name: "f_uninstall"
            }
        )
    );
    assert_eq!(
        chain(&err),
        [
            "Module \"f\": uninstall_hook 'f_uninstall' failed",
            "uninstall of f failed"
        ]
    );
    assert_eq!(
        take_log(),
        ["g uninstall"],
        "g's hook ran first (reverse order)"
    );
    assert_eq!(state_of(&system, &orm).await, before);
    db.cleanup().await.unwrap();
}

// ---- B: the data loader purges before the schema and external ids go ----------------------

/// Lead review B: on uninstall, the data loader's purge runs after the uninstall hooks, on the
/// registry that still has the modules (their tables and external ids exist), before the schema
/// and the external ids are removed; a purge that leaves records of models staying installed
/// fails the uninstall.
#[tokio::test]
async fn data_loader_purges_before_schema_removal() {
    let (_addons, db, system, orm) = scenario().await;
    committed(
        system
            .button_immediate_install(&su(&orm), &[n("g")])
            .await
            .unwrap(),
    );
    exec(
        &orm,
        "INSERT INTO test_p (id, name) VALUES (100, 'made by g'); \
         INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         VALUES ('g', 'rec_p', 'test.p', 100, false), ('g', 'rec_g', 'test.g', 1, false)",
    )
    .await;
    take_log();
    // a purge that leaves g's record in test.p (which stays installed)
    PURGE_RECORDS.store(false, Ordering::SeqCst);
    let before = snapshot(&orm).await;
    let err = system
        .button_immediate_uninstall(&su(&orm), &[n("f")])
        .await
        .unwrap_err();
    let Error::PurgeIncomplete { module, models } = &err else {
        panic!("expected PurgeIncomplete, got {err:?}");
    };
    let models: Vec<(&str, i64)> = models.iter().map(|o| (o.model.as_str(), o.count)).collect();
    assert_eq!((module.as_str(), models), ("g", vec![("test.p", 1)]));
    assert_eq!(
        err.to_string(),
        "Module \"g\" cannot be uninstalled: after the data loader's purge it still owns records \
         of models that stay installed (test.p: 1)"
    );
    assert_eq!(snapshot(&orm).await, before);
    take_log();
    // a purge that removes them: the uninstall runs, in this order
    PURGE_RECORDS.store(true, Ordering::SeqCst);
    let outcome = system
        .button_immediate_uninstall(&su(&orm), &[n("f")])
        .await
        .unwrap();
    assert_eq!(outcome.uninstalled, vec![n("f"), n("g")]);
    assert_eq!(
        take_log(),
        [
            "g uninstall",
            "f uninstall",
            "purge f,g tables=true xmlids=2"
        ]
    );
    assert!(
        rows(&orm, "SELECT id::text FROM test_p WHERE id = 100")
            .await
            .is_empty()
    );
    assert!(
        rows(
            &orm,
            "SELECT name FROM ir_model_data WHERE module IN ('f', 'g')"
        )
        .await
        .is_empty()
    );
    assert!(!table_exists(&orm, "test_g").await);
    db.cleanup().await.unwrap();
}

/// Delta review R5 (and lead item K): the data loader's purge failing after it changed data and
/// schema fails the uninstall with an error naming every module being removed (the loader purges
/// them together), keeping the loader's error; the database and the registry are as before.
#[tokio::test]
async fn failing_purge_names_every_module_and_changes_nothing() {
    let (_addons, db, system, orm) = scenario().await;
    committed(
        system
            .button_immediate_install(&su(&orm), &[n("g")])
            .await
            .unwrap(),
    );
    take_log();
    let before = state_of(&system, &orm).await;
    set_fail(Fail::Purge);
    let err = system
        .button_immediate_uninstall(&su(&orm), &[n("f")])
        .await
        .unwrap_err();
    let Error::PurgeFailed { modules, source } = &err else {
        panic!("expected PurgeFailed, got {err:?}");
    };
    assert_eq!(modules, &vec![n("f"), n("g")]);
    assert!(
        matches!(source.as_ref(), Error::Orm(e) if e.kind() == erp_fields::ErrorKind::UserError),
        "{source:?}"
    );
    assert_eq!(
        chain(&err),
        [
            "Modules f, g: the data loader's purge before uninstall failed",
            "purge of f, g failed"
        ]
    );
    assert_eq!(
        take_log(),
        [
            "g uninstall",
            "f uninstall",
            "purge f,g tables=true xmlids=0"
        ]
    );
    assert_eq!(state_of(&system, &orm).await, before);
    db.cleanup().await.unwrap();
}

// ---- D: lock and statement timeouts ------------------------------------------------------

fn sleep_hook(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.query("SELECT pg_sleep(5)", &[]).await?;
        Ok(())
    })
}

static SLEEP_HOOK: HookImpl =
    HookImpl::new("s", HookKind::PostInit, "s_sleep", sleep_hook).at(file!(), line!());

fn slow_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { schedule_post_commit(tx, &n("w"), &pk("slow"), &gate_action()).await })
}

static SLOW_STEP: HookImpl =
    HookImpl::new("w", HookKind::PostInit, "w_steps", slow_step).at(file!(), line!());

/// Lead review D: operations run with `lock_timeout` and `statement_timeout` (set right after the
/// operation lock), the post-commit phase with its own; a timeout is a typed error naming the
/// limit, and the limits can only be lowered.
#[tokio::test]
async fn operations_run_with_lock_and_statement_timeouts() {
    let addons = Addons::new();
    addons
        .add("p", "name = 'P'\n")
        .add("a", "name = 'A'\ndepends = ['p']\n")
        .add("s", "name = 'S'\npost_init_hook = 's_sleep'\n")
        .add("w", "name = 'W'\npost_init_hook = 'w_steps'\n");
    let db = TestDb::create().await.unwrap();
    for (limit, apply, name, max) in [
        (
            Duration::ZERO,
            ModuleSystemBuilder::lock_timeout
                as fn(ModuleSystemBuilder, Duration) -> ModuleSystemBuilder,
            LimitKind::LockTimeout,
            erp_modules::system::MAX_LOCK_TIMEOUT,
        ),
        (
            Duration::from_secs(31),
            ModuleSystemBuilder::lock_timeout,
            LimitKind::LockTimeout,
            erp_modules::system::MAX_LOCK_TIMEOUT,
        ),
        (
            Duration::ZERO,
            ModuleSystemBuilder::statement_timeout,
            LimitKind::StatementTimeout,
            erp_modules::system::MAX_STATEMENT_TIMEOUT,
        ),
        (
            Duration::from_secs(6 * 3600 + 1),
            ModuleSystemBuilder::post_commit_statement_timeout,
            LimitKind::PostCommitStatementTimeout,
            erp_modules::system::MAX_POST_COMMIT_STATEMENT_TIMEOUT,
        ),
        (
            Duration::from_micros(10),
            ModuleSystemBuilder::operation_wait_timeout,
            LimitKind::OperationWaitTimeout,
            erp_modules::system::MAX_OPERATION_WAIT,
        ),
    ] {
        let err = apply(builder(&db, &addons), limit).build().err().unwrap();
        assert!(
            matches!(&err, Error::InvalidTimeout { name: kind, value, max: m }
                if *kind == name && *value == limit && *m == max),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("{name} must be between 1ms and {max:?}, got {limit:?}")
        );
    }
    let init_system = builder(&db, &addons)
        .decls([model("p", "test.p"), extension("a", "test.p", "a_col")])
        .hooks([&SLEEP_HOOK, &SLOW_STEP])
        .build()
        .unwrap();
    committed(
        init_system
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap(),
    );
    drop(init_system);

    let system = builder(&db, &addons)
        .decls([model("p", "test.p"), extension("a", "test.p", "a_col")])
        .hooks([&SLEEP_HOOK, &SLOW_STEP])
        .lock_timeout(Duration::from_millis(200))
        .statement_timeout(Duration::from_millis(300))
        .post_commit_statement_timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let loaded = system.load().await.unwrap();
    let env = su(&loaded.orm);
    let defaults = timeout_settings(&loaded.orm).await;
    committed(
        system
            .button_immediate_install(&env, &[n("p")])
            .await
            .unwrap(),
    );
    let before = snapshot(&loaded.orm).await;

    // another session holds test_p: a's column cannot be added within the lock timeout
    let mut holder = loaded.orm.begin(SUPERUSER_ID).await.unwrap();
    holder
        .batch("LOCK TABLE test_p IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let err = system
        .button_immediate_install(&env, &[n("a")])
        .await
        .unwrap_err();
    let (module, step, _, source) = step_failed(&err);
    assert_eq!((module, step), ("a", ModuleStep::Schema));
    assert!(
        matches!(source, Error::LockNotAvailable { lock_timeout, source } if *lock_timeout == Duration::from_millis(200) && source.sqlstate() == Some("55P03")),
        "{source:?}"
    );
    // the error names the limit; PostgreSQL's message (the source) names the cause
    assert_eq!(
        chain(&err)[..3],
        [
            "Module \"a\": schema update failed",
            "A statement of a module operation could not get a database lock (SQLSTATE 55P03; \
             lock_timeout is 200 ms)",
            // the ORM names the limit it set with BEGIN and says the schema step changed it
            // (limits in the message; the merge with's operation limits)
            "canceling statement due to lock timeout (lock_timeout was 10000 ms when the \
             transaction began; changed inside the transaction since)",
        ]
    );
    holder.rollback().await.unwrap();
    drop(holder);
    assert_eq!(snapshot(&loaded.orm).await, before);

    // a hook running longer than the statement timeout
    let err = system
        .button_immediate_install(&env, &[n("s")])
        .await
        .unwrap_err();
    let (module, _, _, source) = step_failed(&err);
    assert_eq!(module, "s");
    assert!(
        matches!(
            source,
            Error::StatementCanceled { statement_timeout, .. }
                if *statement_timeout == Duration::from_millis(300)
        ),
        "{source:?}"
    );
    assert_eq!(
        chain(&err)[..3],
        [
            "Module \"s\": post_init_hook 's_sleep' failed",
            "A statement of a module operation was canceled (SQLSTATE 57014; statement_timeout \
             is 300 ms)",
            "canceling statement due to statement timeout",
        ]
    );
    assert_eq!(snapshot(&loaded.orm).await, before);

    // post-commit: a step that waits for a lock longer than the post-commit limit (300 ms, both
    // as lock and as statement timeout) is cancelled by the server and stays for a retry. The
    // two limits are equal, so the server reports whichever timer fires first: the lock timeout
    // (55P03) or the statement timeout (57014); both mean the phase's limit stopped the step.
    let gate = close_step_gate(&loaded.orm).await;
    let outcome = system
        .button_immediate_install(&env, &[n("w")])
        .await
        .unwrap();
    open_gate(gate).await;
    let failed: Vec<(&str, &str, Option<&str>)> = outcome
        .post_commit
        .failed
        .iter()
        .map(|f| (f.step.key.as_str(), f.error.as_str(), f.sqlstate.as_deref()))
        .collect();
    assert!(
        matches!(
            failed.as_slice(),
            [(
                "slow",
                "canceling statement due to statement timeout",
                Some("57014")
            )] | [(
                "slow",
                "canceling statement due to lock timeout",
                Some("55P03")
            )]
        ),
        "{failed:?}"
    );
    // the pooled connections are back to the session defaults
    for _ in 0..4 {
        assert_eq!(timeout_settings(&loaded.orm).await, defaults);
    }
    db.cleanup().await.unwrap();
}

/// `lock_timeout` and `statement_timeout` of a pooled connection.
async fn timeout_settings(orm: &Orm) -> Vec<Vec<Option<String>>> {
    rows(
        orm,
        "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')",
    )
    .await
}

// ---- F: a plain load resumes the post-commit steps an interrupted phase left ----------------

fn index_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let action = erp_modules::PostCommitAction::create_index(
            erp_modules::CreateIndex::new(
                erp_modules::TableName::parse("test_q").unwrap(),
                erp_modules::IndexName::parse("test_q_name_idx").unwrap(),
                vec![erp_modules::IndexColumn::new("name").unwrap()],
            )
            .unwrap(),
        );
        schedule_post_commit(tx, &n("q"), &pk("name_index"), &action).await
    })
}

static INDEX_STEP: HookImpl =
    HookImpl::new("q", HookKind::PostInit, "q_index", index_step).at(file!(), line!());

/// Lead review F, delta review R3: steps left `pending` (another phase held the lock, or the
/// process stopped) are reported by a plain load, which never runs them (startup never waits for
/// an index build), and by `pending_post_commit`; `run_post_commit` runs them.
#[tokio::test]
async fn load_reports_pending_post_commit_steps_and_run_post_commit_resumes_them() {
    let addons = Addons::new();
    addons.add("q", "name = 'Q'\npost_init_hook = 'q_index'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("q", "test.q")])
        .hooks([&INDEX_STEP])
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
        .button_immediate_install(&su(&init.orm), &[n("q")])
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
    holder
        .query(
            "SELECT pg_advisory_unlock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    drop(holder);
    let index_valid = || {
        rows(
            &init.orm,
            "SELECT i.indisvalid::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = 'test_q_name_idx'",
        )
    };
    let pending = |steps: &[PendingPostCommit]| -> Vec<PendingState> {
        steps.iter().map(|p| p.state.clone()).collect()
    };
    let expected = vec![PendingState::Pending {
        step: erp_modules::PostCommitStep::new(n("q"), pk("name_index")),
    }];
    // the load reports the step and leaves it pending (the lock is free: it chose not to run it)
    let loaded = system.load().await.unwrap();
    assert!(loaded.forced_update.is_none());
    assert_eq!(pending(&loaded.post_commit_pending), expected);
    assert!(index_valid().await.is_empty(), "the load built the index");
    assert_eq!(
        pending(&system.pending_post_commit().await.unwrap()),
        expected
    );
    // run_post_commit runs it
    let report = system.run_post_commit(&init.orm).await;
    assert!(report.is_clean(), "{report:?}");
    let done: Vec<(&str, &str)> = report
        .done
        .iter()
        .map(|s| (s.module.as_str(), s.key.as_str()))
        .collect();
    assert_eq!(done, [("q", "name_index")]);
    assert_eq!(index_valid().await, vec![vec![Some("true".to_owned())]]);
    // nothing left
    assert!(system.load().await.unwrap().post_commit_pending.is_empty());
    assert!(system.pending_post_commit().await.unwrap().is_empty());
    assert_eq!(
        system.run_post_commit(&init.orm).await,
        PostCommitReport::default()
    );
    db.cleanup().await.unwrap();
}

// ---- I: lock keys ---------------------------------------------------------------------------

/// Lead review I: the operation lock is a two-int4 advisory lock of its own class, distinct from
/// the schema lock `init_models` takes (one bigint key): holding one never blocks the other.
#[tokio::test]
async fn operation_lock_is_distinct_from_the_schema_lock() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let mut schema = init.orm.begin(SUPERUSER_ID).await.unwrap();
    schema.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await.unwrap();
    // an operation (it takes the operation lock without waiting) runs meanwhile
    assert_eq!(
        system.button_reset_state(&su(&init.orm)).await.unwrap(),
        Vec::<ModuleName>::new()
    );
    let mut op = init.orm.begin(SUPERUSER_ID).await.unwrap();
    let got: bool = op
        .query(
            "SELECT pg_try_advisory_xact_lock($1, $2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(got);
    let keys = rows(
        &init.orm,
        "SELECT classid::text, objid::text, objsubid::text FROM pg_locks \
         WHERE locktype = 'advisory' AND database = \
         (SELECT oid FROM pg_database WHERE datname = current_database()) ORDER BY objsubid",
    )
    .await;
    assert_eq!(
        keys,
        vec![
            vec![
                Some(u32::try_from(4_207_351_906_u64 >> 32).unwrap().to_string()),
                Some((4_207_351_906_u64 & 0xFFFF_FFFF).to_string()),
                Some("1".to_owned())
            ],
            vec![
                Some(OPERATION_LOCK.0.to_string()),
                Some(OPERATION_LOCK.1.to_string()),
                Some("2".to_owned())
            ],
        ]
    );
    schema.rollback().await.unwrap();
    op.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

// ---- L: stored states are checked by the database ----------------------------------------

/// Lead review L: a module state or a post-commit state outside the stored states is refused by
/// the database (`unavailable` is never stored), and the dependency and exclusion
/// tables are indexed on `module_id`.
#[tokio::test]
async fn stored_states_are_checked_and_module_links_indexed() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    for (sql, message) in [
        (
            "UPDATE ir_module_module SET state = 'unavailable' WHERE name = 'base'",
            "The database refused the change: A module state must be one of the stored states.",
        ),
        (
            "INSERT INTO ir_module_post_commit \
             (module, key, kind, table_name, state, revision, attempts) \
             VALUES ('base', 'k', 'analyze', 'ir_module_module', 'running', 1, 0)",
            "The database refused the change: A post-commit step state must be pending, done \
             or failed.",
        ),
    ] {
        let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
        let err = tx.execute(sql, &[]).await.unwrap_err();
        assert_eq!(
            (err.kind(), err.message(), err.sqlstate()),
            (erp_fields::ErrorKind::Validation, message, Some("23514"))
        );
        tx.rollback().await.unwrap();
    }
    let indexed = rows(
        &init.orm,
        "SELECT tablename::text FROM pg_indexes WHERE tablename IN \
         ('ir_module_module_dependency', 'ir_module_module_exclusion') \
         AND indexdef LIKE '%(module_id)%' ORDER BY 1",
    )
    .await;
    assert_eq!(
        indexed,
        vec![
            vec![Some("ir_module_module_dependency".to_owned())],
            vec![Some("ir_module_module_exclusion".to_owned())]
        ]
    );
    db.cleanup().await.unwrap();
}

// ---- O: drop reports at the database level -------------------------------------------------

/// Lead review O: missing installed artifacts refuse lifecycle effects; a plain load still
/// leaves the unavailable dependency out and reports its dependent with a typed reason.
#[tokio::test]
async fn drop_reports_carry_typed_reasons() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n")
        .add("z", "name = 'Z'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.a"), model("b", "test.b")])
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
    std::fs::remove_dir_all(addons.path().join("a")).unwrap();
    system.with_index(erp_modules::AddonIndex::invalidate_cache);
    let expected = DropReason::MissingDependency {
        dependency: n("a"),
        cascade_chain: vec![n("b"), n("a")],
    };
    let error = system
        .button_immediate_install(&env, &[n("z")])
        .await
        .unwrap_err();
    match error {
        Error::Orm(error) => {
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Addon 'a' installed artifact identity does not match available content"
            );
        }
        other => panic!("expected missing installed artifact refusal, got {other:?}"),
    }
    assert_eq!(state(&init.orm, "z").await, "uninstalled");
    let loaded = system.load().await.unwrap();
    assert_eq!(loaded.unavailable, vec![n("a")]);
    assert_eq!(loaded.drops.len(), 1);
    assert_eq!(loaded.drops.get(&n("b")), Some(&expected));
    db.cleanup().await.unwrap();
}
