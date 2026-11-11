//! Module operations, third review round (delta review R1–R10, S1–S10): schema epochs at
//! commit, post-commit sessions and validation, column removal, stored states.
//!
//! The recording statics (`LOG`, the flags below) are per test process: the suite runs under
//! nextest, which gives every test its own process (see `common/mod.rs`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use std::any::Any;

use common::*;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::hooks::{HookImpl, HookKind};
use erp_modules::lifecycle::InstallOptions;
use erp_modules::manifest::{Manifest, ModuleName};
use erp_modules::system::{
    InitOptions, POST_COMMIT_LOCK, PendingState, PostCommitStop, schedule_post_commit,
};
use erp_orm::{
    BoxFuture, ImplKind, MethodFn, MethodImpl, Records, SUPERUSER_ID, Super, Transaction,
};
use erp_registry::AddonDescriptor;
use erp_testkit::TestDb;

// ---- R1: the schema epoch moves again at commit ---------------------------------------------

/// Gates between the hook and the test (a stored permit, so the order of the two sides does not
/// matter): the hook says the schema is done, the test says go on.
static SCHEMA_DONE: tokio::sync::Notify = tokio::sync::Notify::const_new();
static GO_ON: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// `a`'s post_init: its column is added (uncommitted); wait at the gate until the test says go on.
fn a_wait(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        SCHEMA_DONE.notify_one();
        GO_ON.notified().await;
        Ok(())
    })
}

static A_WAIT: HookImpl =
    HookImpl::new("a", HookKind::PostInit, "a_wait", a_wait).at(file!(), line!());

/// R1: a connection that began a transaction after an operation's DDL but before its commit
/// prepared statements against the old schema; the commit invalidates them again, so its next
/// transaction starts with an empty statement cache and sees the new shape.
#[tokio::test]
async fn commit_after_ddl_invalidates_statements_prepared_meanwhile() {
    let addons = Addons::new();
    addons.add("p", "name = 'P'\n").add(
        "a",
        "name = 'A'\ndepends = ['p']\npost_init_hook = 'a_wait'\n",
    );
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(
        builder(&db, &addons)
            .decls([model("p", "test.p"), extension("a", "test.p", "a_col")])
            .hooks([&A_WAIT])
            .build()
            .unwrap(),
    );
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
    // connection B keeps its connection across commits
    let mut b = init.orm.begin(SUPERUSER_ID).await.unwrap();
    b.query("SELECT 1", &[]).await.unwrap();
    b.commit().await.unwrap();
    let install = {
        let system = Arc::clone(&system);
        let env = su(&init.orm);
        tokio::spawn(async move { system.button_immediate_install(&env, &[n("a")]).await })
    };
    // the timeout only guards against a hang; the test proceeds when the hook signals
    assert!(
        tokio::time::timeout(Duration::from_secs(60), SCHEMA_DONE.notified())
            .await
            .is_ok(),
        "a's post_init never ran"
    );
    // B begins after the DDL, before the commit: it prepares a statement meanwhile
    b.query("SELECT 2", &[]).await.unwrap();
    b.commit().await.unwrap();
    GO_ON.notify_one();
    committed(install.await.unwrap().unwrap());
    // B's next transaction drops what it prepared before the commit
    b.query("SELECT 3", &[]).await.unwrap();
    assert_eq!(
        b.prepared_statements(),
        1,
        "statements of before the commit"
    );
    let rows = b
        .query("SELECT * FROM test_p ORDER BY id", &[])
        .await
        .unwrap();
    assert!(rows.is_empty());
    b.query("SELECT a_col FROM test_p", &[]).await.unwrap();
    b.rollback().await.unwrap();
    drop(b);
    db.cleanup().await.unwrap();
}

// ---- R2: a dropped post-commit phase never returns its session to the pool ------------------

fn slow_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { schedule_post_commit(tx, &n("w"), &pk("slow"), &gate_action()).await })
}

static SLOW_STEP: HookImpl =
    HookImpl::new("w", HookKind::PostInit, "w_slow", slow_step).at(file!(), line!());

/// R2: when the future running the post-commit phase is dropped during a step, its connection
/// (session lock, session timeouts) is closed instead of going back to the pool: the lock is
/// released once the server ends the step, later phases are not `Busy`, and pooled connections
/// have the default timeouts.
#[tokio::test]
async fn dropped_post_commit_phase_closes_its_session() {
    let addons = Addons::new();
    addons
        .add("w", "name = 'W'\npost_init_hook = 'w_slow'\n")
        .add("z", "name = 'Z'\n");
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(builder(&db, &addons).hooks([&SLOW_STEP]).build().unwrap());
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let defaults = timeout_settings(&init.orm).await;
    // the step blocks at the gate; the phase is dropped while it waits and the gate opens
    // afterwards, so the server's backend finishes the step and finds the connection closed
    let gate = close_step_gate(&init.orm).await;
    let running = {
        let system = Arc::clone(&system);
        let env = su(&init.orm);
        tokio::spawn(async move { system.button_immediate_install(&env, &[n("w")]).await })
    };
    assert!(
        eventually(Duration::from_secs(20), || step_gate_waiting(&init.orm)).await,
        "the slow step never started"
    );
    assert!(advisory_held(&init.orm, POST_COMMIT_LOCK).await);
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    // the server notices the closed connection once the step it was running ends: let it end
    open_gate(gate).await;
    assert!(
        eventually(Duration::from_secs(15), || async {
            !advisory_held(&init.orm, POST_COMMIT_LOCK).await
        })
        .await,
        "the dropped phase's session still holds the post-commit lock"
    );
    // every pooled connection has the session defaults (all four held at once)
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
        let settings = tx
            .query(
                "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')",
                &[],
            )
            .await
            .unwrap();
        let settings: Vec<Option<String>> = (0..2).map(|i| settings[0].get(i)).collect();
        assert_eq!(vec![settings], defaults);
        held.push(tx);
    }
    for mut tx in held {
        tx.rollback().await.unwrap();
    }
    // the next phase is not Busy and runs the step the dropped one left pending
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("z")])
        .await
        .unwrap();
    assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
    let done: Vec<(&str, &str)> = outcome
        .post_commit
        .done
        .iter()
        .map(|s| (s.module.as_str(), s.key.as_str()))
        .collect();
    assert_eq!(done, [("w", "slow")]);
    db.cleanup().await.unwrap();
}

async fn timeout_settings(orm: &erp_orm::Orm) -> Vec<Vec<Option<String>>> {
    rows(
        orm,
        "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')",
    )
    .await
}

// ---- R8: removing a column never drops other objects that also use columns that stay -------

fn objects_on_test_a(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.batch(
            "CREATE INDEX c_mixed_idx ON test_a (b_note, name); \
             CREATE INDEX c_note_idx ON test_a (b_note); \
             ALTER TABLE test_a ADD CONSTRAINT c_mixed_check \
             CHECK (b_note IS NULL OR name IS NOT NULL)",
        )
        .await
    })
}

static OBJECTS_ON_TEST_A: HookImpl = HookImpl::new(
    "c",
    HookKind::PostInit,
    "objects_on_test_a",
    objects_on_test_a,
)
.at(file!(), line!());

/// R8: `DROP COLUMN` (without `CASCADE`) silently drops indexes and constraints on the column,
/// also those using other columns that stay: the uninstall refuses them, naming each; an object
/// using only removed columns goes with them.
#[tokio::test]
async fn column_removal_refuses_objects_that_use_columns_that_stay() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n")
        .add(
            "c",
            "name = 'C'\ndepends = ['b']\npost_init_hook = 'objects_on_test_a'\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.a"), extension("b", "test.a", "b_note")])
        .hooks([&OBJECTS_ON_TEST_A])
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
    // c's row goes away behind the module system's back (its objects stay)
    exec(
        &init.orm,
        "UPDATE ir_module_module SET state = 'uninstalled', latest_version = NULL WHERE name = 'c'",
    )
    .await;
    let before = snapshot(&init.orm).await;
    let err = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot remove the schema of modules b: cannot drop columns that other objects use \
         together with columns that stay (constraint c_mixed_check on test_a (b_note, name); \
         index c_mixed_idx on test_a (b_note, name)); remove the objects that use it first"
    );
    assert_eq!(snapshot(&init.orm).await, before);
    exec(
        &init.orm,
        "DROP INDEX c_mixed_idx; ALTER TABLE test_a DROP CONSTRAINT c_mixed_check",
    )
    .await;
    committed(
        system
            .button_immediate_uninstall(&env, &[n("b")])
            .await
            .unwrap(),
    );
    assert!(!table_exists(&init.orm, "c_note_idx").await);
    assert!(table_exists(&init.orm, "test_a").await);
    db.cleanup().await.unwrap();
}

// ---- S5: rows read from ir_module_post_commit are validated again ----------------------------

fn two_steps(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        schedule_post_commit(tx, &n("v"), &pk("one"), &analyze("ir_module_module")).await?;
        schedule_post_commit(tx, &n("v"), &pk("two"), &analyze("ir_module_module")).await
    })
}

static TWO_STEPS: HookImpl =
    HookImpl::new("v", HookKind::PostInit, "v_steps", two_steps).at(file!(), line!());

/// S5, S3: a step row whose key or action no longer passes the scheduling checks (changed in the
/// table directly: a module name with a space, a key with spaces) is not run: it is marked failed
/// with the reason and reported as invalid.
#[tokio::test]
async fn post_commit_rows_are_validated_before_they_run() {
    let addons = Addons::new();
    addons
        .add("v", "name = 'V'\npost_init_hook = 'v_steps'\n")
        .add("z", "name = 'Z'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).hooks([&TWO_STEPS]).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    // another phase holds the lock: v's steps stay pending
    let holder = db.pool().get().await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_lock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("v")])
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
    exec(
        &init.orm,
        "UPDATE ir_module_post_commit \
         SET module = 'Bad Module' WHERE key = 'one'; \
         UPDATE ir_module_post_commit SET key = 'Two Words' WHERE key = 'two'",
    )
    .await;
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("z")])
        .await
        .unwrap();
    // the rows the checks of scheduling refuse are not run and are reported apart from the
    // failures of steps that ran
    assert!(outcome.post_commit.failed.is_empty());
    assert!(!outcome.post_commit.is_clean());
    let invalid: Vec<&str> = outcome
        .post_commit
        .invalid
        .iter()
        .map(|r| r.reason.as_str())
        .collect();
    let bad_module = format!(
        "module \"Bad Module\": {}",
        ModuleName::new("Bad Module").unwrap_err()
    );
    assert_eq!(
        invalid,
        [
            bad_module.as_str(),
            "invalid post-commit key \"Two Words\": 1 to 64 bytes of a-z, 0-9, '_', '.', '-'",
        ]
    );
    assert!(outcome.post_commit.done.is_empty());
    assert!(table_exists(&init.orm, "ir_module_module").await);
    assert_eq!(
        rows(
            &init.orm,
            "SELECT key, state, error FROM ir_module_post_commit ORDER BY id"
        )
        .await,
        vec![
            vec![
                Some("one".to_owned()),
                Some("failed".to_owned()),
                Some(bad_module)
            ],
            vec![
                Some("Two Words".to_owned()),
                Some("failed".to_owned()),
                Some(
                    "invalid post-commit key \"Two Words\": 1 to 64 bytes of a-z, 0-9, '_', \
                     '.', '-'"
                        .to_owned()
                )
            ],
        ]
    );
    db.cleanup().await.unwrap();
}

// ---- S6/S3: an invalid index of the same name on another table is not dropped ----------------

fn index_on_other_table(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let action = erp_modules::PostCommitAction::create_index(
            erp_modules::CreateIndex::new(
                erp_modules::TableName::parse("u_two").unwrap(),
                erp_modules::IndexName::parse("u_name_idx").unwrap(),
                vec![erp_modules::IndexColumn::new("name").unwrap()],
            )
            .unwrap(),
        );
        schedule_post_commit(tx, &n("u"), &pk("idx"), &action).await
    })
}

static INDEX_ON_OTHER_TABLE: HookImpl =
    HookImpl::new("u", HookKind::PostInit, "u_index", index_on_other_table).at(file!(), line!());

/// S3: before it drops an invalid index, the runner checks the index belongs to the step's table;
/// an invalid index of that name on another table is neither dropped nor accepted.
#[tokio::test]
async fn invalid_index_guard_refuses_an_index_on_another_table() {
    let addons = Addons::new();
    addons.add("u", "name = 'U'\npost_init_hook = 'u_index'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&INDEX_ON_OTHER_TABLE])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    // an interrupted build of `u_name_idx` on `u_one` left it invalid (a unique build over
    // duplicate values fails after the index entry exists); `u_two` has no such index
    exec(
        &init.orm,
        "CREATE TABLE u_one (name text); INSERT INTO u_one VALUES ('x'), ('x'); \
         CREATE TABLE u_two (name text)",
    )
    .await;
    let client = db.pool().get().await.unwrap();
    let build = client
        .batch_execute("CREATE UNIQUE INDEX CONCURRENTLY u_name_idx ON u_one (name)")
        .await
        .unwrap_err();
    assert_eq!(build.code().map(|c| c.code()), Some("23505"));
    drop(client);
    let invalid_on = || {
        rows(
            &init.orm,
            "SELECT i.indrelid::regclass::text, i.indisvalid::text FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'u_name_idx'",
        )
    };
    assert_eq!(
        invalid_on().await,
        vec![vec![Some("u_one".to_owned()), Some("false".to_owned())]]
    );
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("u")])
        .await
        .unwrap();
    let failed: Vec<(&str, &str)> = outcome
        .post_commit
        .failed
        .iter()
        .map(|f| (f.step.key.as_str(), f.error.as_str()))
        .collect();
    assert_eq!(
        failed,
        [(
            "idx",
            "index u_name_idx exists in schema public but not on table u_two: it is not touched"
        )]
    );
    assert_eq!(
        invalid_on().await,
        vec![vec![Some("u_one".to_owned()), Some("false".to_owned())]],
        "the index of the other table was left alone"
    );
    db.cleanup().await.unwrap();
}

// ---- S9: stored post-commit states are checked before the check constraint is added ---------

/// S9: on a database whose `ir_module_post_commit` has no state check yet (an older database),
/// rows with a state outside the stored states are named before the upgrade of `base` adds the
/// check, instead of failing with the constraint's generic message.
#[tokio::test]
async fn out_of_range_post_commit_states_are_named_before_the_check_is_added() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let check = rows(
        &init.orm,
        "SELECT conname::text FROM pg_constraint \
         WHERE conrelid = 'ir_module_post_commit'::regclass AND contype = 'c' \
         AND conname::text LIKE '%state_check'",
    )
    .await;
    let check = check[0][0].clone().unwrap();
    exec(
        &init.orm,
        &format!(
            "ALTER TABLE ir_module_post_commit DROP CONSTRAINT \"{check}\"; \
             INSERT INTO ir_module_post_commit (module, key, kind, table_name, state, revision, attempts) \
             VALUES ('base', 'k1', 'analyze', 'ir_module_module', 'running', 1, 0), \
             ('base', 'k2', 'analyze', 'ir_module_module', 'done', 1, 0)"
        ),
    )
    .await;
    let err = system
        .button_immediate_upgrade(&su(&init.orm), &[n("base")])
        .await
        .unwrap_err();
    assert_eq!(
        chain(&err),
        [
            "Module \"base\": schema update failed",
            "Table ir_module_post_commit has rows whose state is not one of pending, done, \
             failed (ids and states: 1 'running'); correct or delete them, then run the \
             operation again",
        ]
    );
    db.cleanup().await.unwrap();
}

// ---- R3 / R4: a forced load leaves its post-commit steps pending and lists them --------------

fn failing_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        schedule_post_commit(tx, &n("k"), &pk("boom"), &analyze("no_such_table")).await
    })
}

static FAILING_STEP: HookImpl =
    HookImpl::new("k", HookKind::PostInit, "k_boom", failing_step).at(file!(), line!());

/// R3, R4: the forced update of a plain load does not run post-commit steps either; the load's
/// one list of waiting steps includes those the forced update scheduled, and `run_post_commit`
/// reports a failing one.
#[tokio::test]
async fn forced_load_lists_the_steps_its_update_scheduled() {
    let addons = Addons::new();
    addons.add("k", "name = 'K'\npost_init_hook = 'k_boom'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&FAILING_STEP])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    system
        .button_install(&su(&init.orm), &[n("k")], &InstallOptions::new())
        .await
        .unwrap();
    let loaded = system.load().await.unwrap();
    let forced = loaded.forced_update.as_ref().expect("a forced update");
    assert_eq!(forced.installed, vec![n("k")]);
    let step = erp_modules::PostCommitStep::new(n("k"), pk("boom"));
    let pending: Vec<(PendingState, i64)> = loaded
        .post_commit_pending
        .iter()
        .map(|p| (p.state.clone(), p.attempts))
        .collect();
    assert_eq!(pending, [(PendingState::Pending { step: step.clone() }, 0)]);
    let report = system.run_post_commit(&init.orm).await;
    let failed: Vec<(&str, &str, Option<&str>)> = report
        .failed
        .iter()
        .map(|f| (f.step.key.as_str(), f.error.as_str(), f.sqlstate.as_deref()))
        .collect();
    assert_eq!(
        failed,
        [(
            "boom",
            "relation \"public.no_such_table\" does not exist",
            Some("42P01")
        )]
    );
    let waiting = system.pending_post_commit().await.unwrap();
    let waiting: Vec<(PendingState, i64)> = waiting
        .iter()
        .map(|p| (p.state.clone(), p.attempts))
        .collect();
    assert_eq!(
        waiting,
        [(
            PendingState::Failed {
                step,
                error: "relation \"public.no_such_table\" does not exist".to_owned(),
                sqlstate: Some("42P01".to_owned()),
            },
            1
        )]
    );
    db.cleanup().await.unwrap();
}

// ---- R7: a lock refused with NOWAIT is not reported as a timeout ----------------------------

fn lock_nowait(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.batch("LOCK TABLE test_p IN ACCESS EXCLUSIVE MODE NOWAIT")
            .await
    })
}

static LOCK_NOWAIT: HookImpl =
    HookImpl::new("nw", HookKind::PostInit, "nw_lock", lock_nowait).at(file!(), line!());

/// R7: SQLSTATE 55P03 from `NOWAIT` is typed like a lock timeout, but the message only names
/// the limit; PostgreSQL's message (the source) gives the real cause.
#[tokio::test]
async fn lock_refused_with_nowait_names_the_real_cause() {
    let addons = Addons::new();
    addons.add("p", "name = 'P'\n").add(
        "nw",
        "name = 'NW'\ndepends = ['p']\npost_init_hook = 'nw_lock'\n",
    );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("p", "test.p")])
        .hooks([&LOCK_NOWAIT])
        .build()
        .unwrap();
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
    let mut holder = init.orm.begin(SUPERUSER_ID).await.unwrap();
    holder
        .batch("LOCK TABLE test_p IN ACCESS SHARE MODE")
        .await
        .unwrap();
    // The lock is held until after the call: PostgreSQL's own message below (NOWAIT's, not a
    // lock timeout's) shows the statement did not wait.
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("nw")])
        .await
        .unwrap_err();
    let (module, _, _, source) = step_failed(&err);
    assert_eq!(module, "nw");
    assert!(
        matches!(
            source,
            Error::LockNotAvailable { lock_timeout, .. }
                if *lock_timeout == Duration::from_secs(30)
        ),
        "{source:?}"
    );
    assert_eq!(
        chain(&err)[..3],
        [
            "Module \"nw\": post_init_hook 'nw_lock' failed",
            "A statement of a module operation could not get a database lock (SQLSTATE 55P03; \
             lock_timeout is 30000 ms)",
            "could not obtain lock on relation \"test_p\"",
        ]
    );
    holder.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

// ---- S1: a failing _register_hook names its model and module --------------------------------

static REGISTER_FAILS: AtomicBool = AtomicBool::new(false);

fn failing_register<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        if REGISTER_FAILS.load(Ordering::SeqCst) {
            return Err(erp_fields::Error::user("_register_hook of test.a failed"));
        }
        sup.call(tx, records, args).await
    })
}

fn failing_register_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = failing_register;
    &F
}

static FAILING_REGISTER: MethodImpl = MethodImpl {
    addon: "x",
    model: "test.a",
    method: "_register_hook",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: failing_register_entry,
    file: file!(),
    line: line!(),
};

/// S1: `_register_hook` failing in an operation or in a plain load is a step error naming the
/// model and the module declaring it (`a`; the override that failed is `x`'s), with the hook's
/// own error as the source.
#[tokio::test]
async fn failing_register_hook_names_model_and_module() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("x", "name = 'X'\ndepends = ['a']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "test.a"), extension("x", "test.a", "x_note")])
        .impls([&FAILING_REGISTER])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let expected = [
        "Module \"a\": _register_hook() of test.a failed",
        "_register_hook of test.a failed",
    ];
    let check = |err: &Error| {
        let (module, step, file, source) = step_failed(err);
        assert_eq!(
            (module, step, file),
            ("a", ModuleStep::RegisterHook { model: "test.a" }, None)
        );
        assert!(
            matches!(source, Error::Orm(e) if e.kind() == erp_fields::ErrorKind::UserError),
            "{source:?}"
        );
        assert_eq!(chain(err), expected);
    };
    REGISTER_FAILS.store(true, Ordering::SeqCst);
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap_err();
    check(&err);
    assert_eq!(state(&init.orm, "x").await, "uninstalled");
    REGISTER_FAILS.store(false, Ordering::SeqCst);
    committed(
        system
            .button_immediate_install(&su(&init.orm), &[n("x")])
            .await
            .unwrap(),
    );
    REGISTER_FAILS.store(true, Ordering::SeqCst);
    check(&system.load().await.err().unwrap());
    db.cleanup().await.unwrap();
}

// ---- S4: a FIFO at a data file path is refused, without blocking ----------------------------

/// S4: a data file that is not a regular file (a FIFO here, which would block the open) is an
/// artifact admission error naming the addon; the install neither hangs nor persists.
#[tokio::test]
async fn fifo_data_file_is_refused_without_blocking() {
    let addons = Addons::new();
    addons.add("p", "name = 'P'\ndata = ['data/pipe.xml']\n");
    let dir = addons.path().join("p").join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("pipe.xml");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    take_log();
    let err = tokio::time::timeout(
        Duration::from_secs(20),
        system.button_immediate_install(&su(&init.orm), &[n("p")]),
    )
    .await
    .expect("the install blocked on the FIFO")
    .unwrap_err();
    assert!(
        matches!(&err, Error::NodeDigest(erp_orm::node::NodeError::AddonSource { addon, message })
            if addon == "p" && message == "artifact entry is not a regular file or directory"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "addon p: artifact entry is not a regular file or directory"
    );
    assert_eq!(state(&init.orm, "p").await, "uninstalled");
    assert!(take_log().is_empty(), "nothing reached the loader");
    db.cleanup().await.unwrap();
}

// ---- R9: step and source errors not covered elsewhere, kind and message ---------------------

/// R9: a demo file that fails for a reason other than its data (here a `.sql` file the database
/// refuses with a syntax error) is a `DemoFile` step error naming the file, and fails the
/// operation: only data failures are rolled back and noticed.
#[tokio::test]
async fn failing_demo_file_is_a_demo_step_error() {
    let addons = Addons::new();
    addons
        .add("d", "name = 'D'\ndemo = ['demo/broken.sql']\n")
        .file("d", "demo/broken.sql", "SELEC 1;");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .data_loader(Arc::new(RecordingLoader))
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
        .button_immediate_install(&su(&init.orm), &[n("d")])
        .await
        .unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("d", ModuleStep::DemoFile, Some("demo/broken.sql"))
    );
    assert!(
        matches!(source, Error::Orm(e) if e.sqlstate() == Some("42601")),
        "{source:?}"
    );
    assert_eq!(
        chain(&err),
        [
            "Module \"d\": demo file 'demo/broken.sql' failed",
            "syntax error at or near \"SELEC\"",
            "db error",
            "ERROR: syntax error at or near \"SELEC\"",
        ]
    );
    assert_eq!(state(&init.orm, "d").await, "uninstalled");
    db.cleanup().await.unwrap();
}

static A_CODE_1: AddonDescriptor = AddonDescriptor {
    name: "a",
    depends: &["base"],
    exact_only: false,
};
static A_CODE_2: AddonDescriptor = AddonDescriptor {
    name: "a",
    depends: &["base"],
    exact_only: false,
};
static A_CODE_WEB: AddonDescriptor = AddonDescriptor {
    name: "a",
    depends: &["web"],
    exact_only: false,
};

/// R9: a native descriptor given twice, or declaring other dependencies than its manifest, is
/// refused when the system is built.
#[tokio::test]
async fn addon_code_must_match_its_manifest() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n");
    let db = TestDb::create().await.unwrap();
    for (code, message) in [
        (
            [&A_CODE_1, &A_CODE_2],
            "Module 'a': its code does not match manifest.toml: two code descriptors for this addon",
        ),
        (
            [&A_CODE_WEB, &A_CODE_WEB],
            "Module 'a': its code does not match manifest.toml: two code descriptors for this addon",
        ),
    ] {
        let err = builder(&db, &addons).code(code).build().err().unwrap();
        assert!(
            matches!(&err, Error::AddonCodeMismatch { module, .. } if module.as_str() == "a"),
            "{err:?}"
        );
        assert_eq!(err.to_string(), message);
    }
    let err = builder(&db, &addons)
        .code([&A_CODE_WEB])
        .build()
        .err()
        .unwrap();
    assert!(
        matches!(&err, Error::AddonCodeMismatch { module, .. } if module.as_str() == "a"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module 'a': its code does not match manifest.toml: the code declares depends {\"web\"} \
         but manifest.toml declares {\"base\"}"
    );
    assert!(builder(&db, &addons).code([&A_CODE_1]).build().is_ok());
    db.cleanup().await.unwrap();
}

/// R9: unattested embedded and package sources fail artifact admission before data loading,
/// naming the module and the missing provenance; no installation persists.
#[tokio::test]
async fn data_files_of_embedded_and_package_addons_are_refused() {
    for (name, label, expected) in [
        (
            "emb",
            "embedded",
            "Addon 'emb' linked native artifact identity is unknown",
        ),
        (
            "pkg",
            "package",
            "Addon 'pkg' package artifact identity is unknown",
        ),
    ] {
        let addons = Addons::new();
        let db = TestDb::create().await.unwrap();
        let system = builder(&db, &addons)
            .data_loader(Arc::new(RecordingLoader))
            .build()
            .unwrap();
        let init = system
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap();
        system.with_index(|index| {
            let manifest =
                Manifest::from_toml(name, "name = 'Unattested'\ndata = ['data/file.xml']\n")
                    .unwrap();
            match label {
                "embedded" => index.add_embedded(manifest, "builtin:emb"),
                "package" => index.add_package(manifest, "store:pkg"),
                _ => panic!("unknown fixture source"),
            }
            .unwrap();
        });
        let env = su(&init.orm);
        system.update_list(&env).await.unwrap();
        take_log();
        let err = system
            .button_immediate_install(&env, &[n(name)])
            .await
            .unwrap_err();
        match err {
            Error::Orm(error) => {
                assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
                assert_eq!(error.message(), expected);
            }
            other => panic!("unexpected artifact admission error: {other:?}"),
        }
        assert_eq!(state(&init.orm, name).await, "uninstalled");
        assert!(take_log().is_empty(), "nothing reached the loader");
        db.cleanup().await.unwrap();
    }
}
