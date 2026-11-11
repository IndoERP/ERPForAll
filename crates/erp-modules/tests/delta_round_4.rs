//! delta-round fixes (D3, D6, D7, D9): the schema lock on uninstall, a phase that
//! fails twice, unreadable addon folders, transaction identity across `SET`s and in plain loads,
//! and the message for a step whose table is missing.
//!
//! The recording statics are per test process: the suite runs under nextest, which gives every
//! test its own process (see `common/mod.rs`). No test depends on how long something takes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::hooks::{HookImpl, HookKind};
use erp_modules::loader::{DataFile, FileLoader, Operations};
use erp_modules::manifest::ModuleName;
use erp_modules::system::{InitOptions, ModuleSystem, PostCommitStop, schedule_post_commit};
use erp_orm::{BoxFuture, ImplKind, MethodFn, MethodImpl, Orm, Records, Super, Transaction};
use erp_testkit::TestDb;

async fn initialized(system: &ModuleSystem) -> Orm {
    system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap()
        .orm
}

// ---- D3: the schema lock is taken before the uninstall hooks and the purge -----------------

/// Whether this backend holds the schema lock of the ORM's schema step (a one-`bigint` advisory
/// key, taken by `erp_sql::ddl::SCHEMA_LOCK`).
async fn holds_schema_lock(tx: &mut Transaction) -> erp_fields::Result<bool> {
    let key: i64 = erp_sql::ddl::SCHEMA_LOCK
        .trim_start_matches("SELECT pg_advisory_xact_lock(")
        .trim_end_matches(')')
        .parse()
        .unwrap();
    let rows = tx
        .query(
            "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND granted \
             AND pid = pg_backend_pid() AND objsubid = 1 AND ((classid::int8 << 32) | objid::int8) = $1)",
            &[&key],
        )
        .await?;
    Ok(rows[0].get(0))
}

fn l3_uninstall(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let held = holds_schema_lock(tx).await?;
        log(format!("uninstall hook: schema lock held={held}"));
        Ok(())
    })
}

static L3_UNINSTALL: HookImpl =
    HookImpl::new("l3", HookKind::Uninstall, "l3_uninstall", l3_uninstall).at(file!(), line!());

struct LockProbeLoader;

impl FileLoader for LockProbeLoader {
    fn purge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        _modules: &'a [ModuleName],
    ) -> BoxFuture<'a, erp_modules::Result<Vec<erp_data::load::DataNotice>>> {
        Box::pin(async move {
            let held = holds_schema_lock(tx).await?;
            log(format!("purge: schema lock held={held}"));
            Ok(Vec::new())
        })
    }

    fn load<'a, 'o: 'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            for op in operations {
                op?;
            }
            Ok(())
        })
    }
}

/// D3: an uninstall holds the schema lock while its hooks and the loader's purge run (operation
/// lock, then schema lock, then table locks), not only once the schema drops start.
#[tokio::test]
async fn uninstall_takes_the_schema_lock_before_hooks_and_purge() {
    let addons = Addons::new();
    addons.add("l3", "name = 'L3'\nuninstall_hook = 'l3_uninstall'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&L3_UNINSTALL])
        .data_loader(Arc::new(LockProbeLoader))
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let env = su(&orm);
    committed(
        system
            .button_immediate_install(&env, &[n("l3")])
            .await
            .unwrap(),
    );
    take_log();
    committed(
        system
            .button_immediate_uninstall(&env, &[n("l3")])
            .await
            .unwrap(),
    );
    assert_eq!(
        take_log(),
        [
            "uninstall hook: schema lock held=true",
            "purge: schema lock held=true"
        ]
    );
    db.cleanup().await.unwrap();
}

// ---- D6: a phase that fails twice reports both ---------------------------------------------

fn gated(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { schedule_post_commit(tx, &n("g6"), &pk("slow"), &gate_action()).await })
}

static GATED: HookImpl =
    HookImpl::new("g6", HookKind::PostInit, "g6_slow", gated).at(file!(), line!());

/// D6: the phase's backend is terminated while a step runs: the step fails, restoring the
/// bookkeeping timeout fails, recording the result fails and so does releasing the lock. The stop
/// carries all of them, not only the first (the step itself ran into a closed connection).
#[tokio::test]
async fn a_phase_whose_connection_dies_reports_every_failure() {
    let addons = Addons::new();
    addons.add("g6", "name = 'G6'\npost_init_hook = 'g6_slow'\n");
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(builder(&db, &addons).hooks([&GATED]).build().unwrap());
    let orm = initialized(&system).await;
    let gate = close_step_gate(&orm).await;
    let running = {
        let system = Arc::clone(&system);
        let env = su(&orm);
        tokio::spawn(async move { system.button_immediate_install(&env, &[n("g6")]).await })
    };
    assert!(
        eventually(Duration::from_secs(20), || step_gate_waiting(&orm)).await,
        "the step never started"
    );
    let killed = rows(
        &orm,
        &format!(
            "SELECT pg_terminate_backend(pid)::text FROM pg_stat_activity \
             WHERE datname = current_database() \
             AND query LIKE 'ANALYZE %{STEP_GATE_TABLE}%'"
        ),
    )
    .await;
    assert_eq!(killed, vec![vec![Some("true".to_owned())]]);
    let outcome = running.await.unwrap().unwrap();
    open_gate(gate).await;
    let Some(PostCommitStop::Failed { message, .. }) = &outcome.post_commit.stopped else {
        panic!("{:?}", outcome.post_commit);
    };
    for part in [
        "setting the post-commit lock timeout",
        "recording step",
        "releasing the post-commit lock",
    ] {
        assert!(message.contains(part), "{part:?} missing from {message:?}");
    }
    db.cleanup().await.unwrap();
}

// ---- D7: an unreadable folder is an error, not "nothing there" -----------------------------

/// D7: `static/` that cannot be read (mode 000) makes the manifest read fail with an I/O error
/// naming the path, where `Path::is_dir` / `is_file` answered `false` and the icon vanished
/// silently. Skipped when the tests run as root (root reads everything).
#[test]
fn an_unreadable_static_folder_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let addons = Addons::new();
    addons.add("u7", "name = 'U7'\n");
    let static_dir = addons.path().join("u7").join("static");
    std::fs::create_dir_all(static_dir.join("description")).unwrap();
    std::fs::set_permissions(&static_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&static_dir).is_ok();
    let result = erp_modules::read_manifest_from_dir(&addons.path().join("u7"), &n("u7"));
    // the tempdir cleanup needs the folder readable again
    std::fs::set_permissions(&static_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("skipped: running as a user that reads mode-000 folders (root)");
        return;
    }
    let err = result.unwrap_err();
    let icon = static_dir.join("description").join("icon.png");
    assert!(
        matches!(&err, Error::Io { path, source }
            if *path == icon && source.kind() == std::io::ErrorKind::PermissionDenied),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "I/O error reading '{}': Permission denied (os error 13)",
            icon.display()
        )
    );
}

// ---- D9a: the transaction's identity survives SET and covers plain loads -------------------

fn set_session_hook(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.batch("SET TIME ZONE 'Asia/Jakarta'; SET datestyle = 'SQL, DMY'")
            .await
    })
}

static SET_SESSION: HookImpl =
    HookImpl::new("s9", HookKind::PostInit, "s9_set", set_session_hook).at(file!(), line!());

/// D9a: a hook that changes `TimeZone` and `DateStyle` is not mistaken for one that ended the
/// transaction (the identity is compared as `timestamptz`, not as text).
#[tokio::test]
async fn set_time_zone_in_a_hook_is_not_a_false_positive() {
    let addons = Addons::new();
    addons.add("s9", "name = 'S9'\npost_init_hook = 's9_set'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).hooks([&SET_SESSION]).build().unwrap();
    let orm = initialized(&system).await;
    committed(
        system
            .button_immediate_install(&su(&orm), &[n("s9")])
            .await
            .unwrap(),
    );
    assert_eq!(state(&orm, "s9").await, "installed");
    db.cleanup().await.unwrap();
}

static REGISTER_COMMITS: AtomicBool = AtomicBool::new(false);

fn committing_register<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        if REGISTER_COMMITS.load(Ordering::SeqCst) {
            tx.batch("COMMIT").await?;
        }
        sup.call(tx, records, args).await
    })
}

fn committing_register_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = committing_register;
    &F
}

static COMMITTING_REGISTER: MethodImpl = MethodImpl {
    addon: "c9",
    model: "test.c9",
    method: "_register_hook",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: committing_register_entry,
    file: file!(),
    line: line!(),
};

/// D9a: a raw `COMMIT` in `_register_hook` during a plain load is detected, naming the model and
/// the module (the plain load's transaction has no operation lock but has an identity).
#[tokio::test]
async fn a_raw_commit_in_register_hook_of_a_plain_load_is_detected() {
    let addons = Addons::new();
    addons.add("c9", "name = 'C9'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("c9", "test.c9")])
        .impls([&COMMITTING_REGISTER])
        .build()
        .unwrap();
    let init = initialized(&system).await;
    committed(
        system
            .button_immediate_install(&su(&init), &[n("c9")])
            .await
            .unwrap(),
    );
    REGISTER_COMMITS.store(true, Ordering::SeqCst);
    let err = system.load().await.err().unwrap();
    let (module, step, _, source) = step_failed(&err);
    assert_eq!(
        (module, step),
        ("c9", ModuleStep::RegisterHook { model: "test.c9" })
    );
    assert!(
        matches!(source, Error::OperationTransactionEnded { detail }
            if *detail == "the session runs another transaction than the one the operation began"),
        "{source:?}"
    );
    db.cleanup().await.unwrap();
}

// ---- D9e: a step on a table that does not exist says so -------------------------------------

fn missing_table_index(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        // the index name exists (the primary key of ir_module_module); the table does not
        let action = erp_modules::PostCommitAction::create_index(
            erp_modules::CreateIndex::new(
                erp_modules::TableName::parse("no_such_table").unwrap(),
                erp_modules::IndexName::parse("ir_module_module_pkey").unwrap(),
                vec![erp_modules::IndexColumn::new("id").unwrap()],
            )
            .unwrap(),
        );
        schedule_post_commit(tx, &n("m9"), &pk("idx"), &action).await
    })
}

static MISSING_TABLE: HookImpl =
    HookImpl::new("m9", HookKind::PostInit, "m9_idx", missing_table_index).at(file!(), line!());

/// D9e: an index step whose table is missing fails with "table ... does not exist in schema ...",
/// not with a claim that the index is on another table.
#[tokio::test]
async fn an_index_step_on_a_missing_table_says_the_table_is_missing() {
    let addons = Addons::new();
    addons.add("m9", "name = 'M9'\npost_init_hook = 'm9_idx'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&MISSING_TABLE])
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let outcome = system
        .button_immediate_install(&su(&orm), &[n("m9")])
        .await
        .unwrap();
    let schema = rows(&orm, "SELECT current_schema()::text").await[0][0]
        .clone()
        .unwrap();
    assert_eq!(
        outcome.post_commit.failed.len(),
        1,
        "{:?}",
        outcome.post_commit
    );
    assert_eq!(
        outcome.post_commit.failed[0].error,
        format!("table no_such_table does not exist in schema {schema}")
    );
    assert!(!outcome.post_commit.failed[0].parked);
    db.cleanup().await.unwrap();
}
