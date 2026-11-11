//! review fixes: operation transactions that hooks cannot end, a failure reset that
//! survives a dropped future, post-commit sessions that never return to the pool, limits that are
//! reported as they were in force, schema locks on uninstall, retry bookkeeping of post-commit
//! steps, malformed queue rows and audit columns of the module tables.
//!
//! The recording statics are per test process: the suite runs under nextest, which gives every
//! test its own process (see `common/mod.rs`). No test depends on how long something takes: they
//! synchronize on advisory locks (the gate) and on what the database reports (`eventually`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use erp_fields::ErrorKind;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::hooks::{HookImpl, HookKind};
use erp_modules::lifecycle::InstallOptions;
use erp_modules::system::{
    InitOptions, ModuleSystem, OPERATION_LOCK, POST_COMMIT_LOCK, PendingState, PostCommitStop,
    schedule_post_commit,
};
use erp_orm::{BoxFuture, Orm, SUPERUSER_ID, Transaction};
use erp_testkit::TestDb;

/// A fresh database's `base` and nothing else installed.
async fn initialized(system: &ModuleSystem) -> Orm {
    system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap()
        .orm
}

/// `SQL` as a string, for the lock key of the schema synchronization (`erp_sql::ddl::SCHEMA_LOCK`).
const SCHEMA_LOCK_SQL: &str = erp_sql::ddl::SCHEMA_LOCK;

// ---- A1: a hook cannot end the operation's transaction -------------------------------------

fn commit_hook(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.batch("CREATE TABLE h1_probe (v int)").await?;
        tx.commit().await
    })
}

static COMMIT_HOOK: HookImpl =
    HookImpl::new("h1", HookKind::PostInit, "h1_commit", commit_hook).at(file!(), line!());

/// A1: a hook calling `tx.commit()` gets a `Programming` error naming the call; the operation
/// fails with it, what the hook did before is rolled back and the transient states are reset.
#[tokio::test]
async fn a_hook_cannot_commit_the_operation_transaction() {
    let addons = Addons::new();
    addons.add("h1", "name = 'H1'\npost_init_hook = 'h1_commit'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).hooks([&COMMIT_HOOK]).build().unwrap();
    let orm = initialized(&system).await;
    let env = su(&orm);
    // a scheduled install is committed as 'to install' before the operation starts
    system
        .button_install(&env, &[n("h1")], &InstallOptions::new())
        .await
        .unwrap();
    assert_eq!(state(&orm, "h1").await, "to install");

    let err = system
        .button_immediate_install(&env, &[n("h1")])
        .await
        .unwrap_err();
    let (module, step, _, source) = step_failed(&err);
    assert_eq!(module, "h1");
    assert!(matches!(
        step,
        ModuleStep::Hook {
            kind: HookKind::PostInit,
            name: "h1_commit"
        }
    ));
    let Error::Orm(source) = source else {
        panic!("expected an ORM error, got {source:?}");
    };
    assert_eq!(source.kind(), ErrorKind::Programming);
    assert_eq!(
        source.message(),
        "Transaction::commit: this transaction belongs to a module operation, which commits or \
         rolls it back itself; a hook, data loader or model init must not end it (nothing was \
         committed or rolled back)"
    );
    assert!(
        !table_exists(&orm, "h1_probe").await,
        "what the hook did was rolled back"
    );
    assert_eq!(state(&orm, "h1").await, "uninstalled", "states reset");
    db.cleanup().await.unwrap();
}

fn raw_commit_hook(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.batch("CREATE TABLE h2_probe (v int)").await?;
        tx.batch("COMMIT").await
    })
}

static RAW_COMMIT_HOOK: HookImpl =
    HookImpl::new("h2", HookKind::PostInit, "h2_commit", raw_commit_hook).at(file!(), line!());

/// A1: a hook that sends `COMMIT` as a statement ends the transaction behind the module system's
/// back: the operation notices before its own commit, fails with
/// [`Error::OperationTransactionEnded`] and resets the transient states. What the hook committed
/// stays (the error says so), and nothing runs in autocommit unnoticed.
#[tokio::test]
async fn a_raw_commit_in_a_hook_is_detected() {
    let addons = Addons::new();
    addons.add("h2", "name = 'H2'\npost_init_hook = 'h2_commit'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&RAW_COMMIT_HOOK])
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let env = su(&orm);
    let err = system
        .button_immediate_install(&env, &[n("h2")])
        .await
        .unwrap_err();
    // stopped right after the hook, before the operation wrote anything in autocommit
    let (module, step, _, source) = step_failed(&err);
    assert_eq!(module, "h2");
    assert!(matches!(
        step,
        ModuleStep::Hook {
            kind: HookKind::PostInit,
            name: "h2_commit"
        }
    ));
    assert!(
        matches!(source, Error::OperationTransactionEnded { detail } if *detail == "the session runs another transaction than the one the operation began"),
        "{source:?}"
    );
    assert_eq!(
        source.to_string(),
        "The transaction of a module operation was ended by a hook, data loader or model method \
         (the session runs another transaction than the one the operation began); the database \
         may hold a partial change"
    );
    assert!(
        table_exists(&orm, "h2_probe").await,
        "the hook's COMMIT persisted its table: the error warned about it"
    );
    assert_eq!(state(&orm, "h2").await, "uninstalled", "states reset");
    assert!(!advisory_held(&orm, OPERATION_LOCK).await);
    db.cleanup().await.unwrap();
}

// ---- A2: the failure reset survives a dropped future ---------------------------------------

fn gated_failure(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        tx.query("SELECT pg_advisory_xact_lock($1, $2)", &[&GATE.0, &GATE.1])
            .await?;
        Err(erp_fields::Error::programming("boom"))
    })
}

static GATED_FAILURE: HookImpl =
    HookImpl::new("g", HookKind::PostInit, "g_boom", gated_failure).at(file!(), line!());

/// Whether a session waits for a row lock inside the transient-state reset.
async fn reset_waiting(orm: &Orm) -> bool {
    rows(
        orm,
        "SELECT (count(*) > 0)::text FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock' \
         AND query LIKE 'UPDATE ir_module_module SET state = CASE%'",
    )
    .await[0][0]
        .as_deref()
        == Some("true")
}

/// A2: the caller drops the operation's future after the rollback, while the reset waits for a
/// row lock: the reset runs to the end all the same and the module is no longer `to install`.
/// (Before the fix the reset was part of the dropped future and the state stayed.)
#[tokio::test]
async fn the_failure_reset_survives_a_dropped_operation_future() {
    let addons = Addons::new();
    addons.add("g", "name = 'G'\npost_init_hook = 'g_boom'\n");
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(
        builder(&db, &addons)
            .hooks([&GATED_FAILURE])
            .build()
            .unwrap(),
    );
    let orm = initialized(&system).await;
    system
        .button_install(&su(&orm), &[n("g")], &InstallOptions::new())
        .await
        .unwrap();
    assert_eq!(state(&orm, "g").await, "to install");

    let gate = close_gate(&orm).await;
    let running = {
        let system = Arc::clone(&system);
        let env = su(&orm);
        tokio::spawn(async move { system.button_immediate_install(&env, &[n("g")]).await })
    };
    assert!(
        eventually(Duration::from_secs(20), || advisory_waiting(&orm, GATE)).await,
        "the hook never reached the gate"
    );
    // the row of g is locked by another session: the reset after the failure has to wait
    let mut blocker = orm.begin(SUPERUSER_ID).await.unwrap();
    blocker
        .query(
            "SELECT id FROM ir_module_module WHERE name = 'g' FOR UPDATE NOWAIT",
            &[],
        )
        .await
        .unwrap();
    open_gate(gate).await;
    assert!(
        eventually(Duration::from_secs(20), || reset_waiting(&orm)).await,
        "the reset never started waiting"
    );
    // the caller gives up between the rollback and the reset
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    blocker.rollback().await.unwrap();
    drop(blocker);
    assert!(
        eventually(Duration::from_secs(20), || async {
            state(&orm, "g").await == "uninstalled"
        })
        .await,
        "the module stayed 'to install' after the future was dropped"
    );
    db.cleanup().await.unwrap();
}

// ---- A3: the post-commit session never returns to the pool ---------------------------------

fn setting_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        schedule_post_commit(tx, &n("s3"), &pk("leak"), &analyze("ir_module_module")).await
    })
}

static SETTING_STEP: HookImpl =
    HookImpl::new("s3", HookKind::PostInit, "s3_leak", setting_step).at(file!(), line!());

/// A3: the phase sets its own session timeouts (lock and statement); with a pool of one
/// connection the next user of the pool must not get that session back with them.
#[tokio::test]
async fn a_step_cannot_leak_session_state_into_the_pool() {
    let addons = Addons::new();
    addons.add("s3", "name = 'S3'\npost_init_hook = 's3_leak'\n");
    let db = TestDb::builder().pool_size(1).create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&SETTING_STEP])
        .lock_timeout(Duration::from_millis(300))
        .post_commit_statement_timeout(Duration::from_secs(7))
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let app_name = || async {
        let client = db.pool().get().await.unwrap();
        let row = client
            .query_one(
                "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')",
                &[],
            )
            .await
            .unwrap();
        (row.get::<_, String>(0), row.get::<_, String>(1))
    };
    let baseline = app_name().await;
    let outcome = system
        .button_immediate_install(&su(&orm), &[n("s3")])
        .await
        .unwrap();
    assert_eq!(
        outcome
            .post_commit
            .done
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>(),
        ["leak"]
    );
    committed(outcome);
    let after = app_name().await;
    assert_eq!(
        after, baseline,
        "the phase's timeouts reached the pooled session"
    );
    db.cleanup().await.unwrap();
}

// ---- A4: step waits and bookkeeping waits --------------------------------------------------

fn probe_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { schedule_post_commit(tx, &n("p4"), &pk("probe"), &gate_action()).await })
}

static PROBE_STEP: HookImpl =
    HookImpl::new("p4", HookKind::PostInit, "p4_probe", probe_step).at(file!(), line!());

/// Whether the session of the gated step has waited for its lock for over a second by the
/// server's clock (the observation is the server's, not a sleep of the test).
async fn step_waited_over_a_second(orm: &Orm) -> bool {
    rows(
        orm,
        &format!(
            "SELECT (count(*) > 0)::text FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
             AND query LIKE 'ANALYZE %{STEP_GATE_TABLE}%' \
             AND now() - query_start > interval '1 second' \
             AND datname = current_database()"
        ),
    )
    .await[0][0]
        .as_deref()
        == Some("true")
}

/// A4: a step waits for its locks as long as its statement may run, not as long as the short
/// `lock_timeout` of the operation (an index build waits for old transactions): the step still
/// waits after the operation's 300 ms limit, and runs once the lock is free.
#[tokio::test]
async fn a_step_waits_for_locks_up_to_the_phase_statement_limit() {
    let addons = Addons::new();
    addons.add("p4", "name = 'P4'\npost_init_hook = 'p4_probe'\n");
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(
        builder(&db, &addons)
            .hooks([&PROBE_STEP])
            .lock_timeout(Duration::from_millis(300))
            .post_commit_statement_timeout(Duration::from_secs(7))
            .build()
            .unwrap(),
    );
    let orm = initialized(&system).await;
    let gate = close_step_gate(&orm).await;
    let running = {
        let system = Arc::clone(&system);
        let env = su(&orm);
        tokio::spawn(async move { system.button_immediate_install(&env, &[n("p4")]).await })
    };
    assert!(
        eventually(Duration::from_secs(20), || step_waited_over_a_second(&orm)).await,
        "the step gave up waiting before the phase's statement limit"
    );
    open_gate(gate).await;
    let outcome = running.await.unwrap().unwrap();
    assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
    assert_eq!(
        outcome
            .post_commit
            .done
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>(),
        ["probe"]
    );
    committed(outcome);
    db.cleanup().await.unwrap();
}

fn gated_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { schedule_post_commit(tx, &n("w4"), &pk("slow"), &gate_action()).await })
}

static GATED_STEP_HOOK: HookImpl =
    HookImpl::new("w4", HookKind::PostInit, "w4_slow", gated_step).at(file!(), line!());

/// A4: the result of a step is recorded without waiting for a row another transaction holds (an
/// operation scheduling the step again would hold it): the step is reported as skipped (row
/// locked, not as superseded: its row is unchanged for everybody else), the
/// phase does not stop, and the step runs again in the next phase.
#[tokio::test]
async fn recording_a_result_does_not_wait_for_a_locked_row() {
    let addons = Addons::new();
    addons.add("w4", "name = 'W4'\npost_init_hook = 'w4_slow'\n");
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(
        builder(&db, &addons)
            .hooks([&GATED_STEP_HOOK])
            .lock_timeout(Duration::from_millis(300))
            .build()
            .unwrap(),
    );
    let orm = initialized(&system).await;
    let gate = close_step_gate(&orm).await;
    let running = {
        let system = Arc::clone(&system);
        let env = su(&orm);
        tokio::spawn(async move { system.button_immediate_install(&env, &[n("w4")]).await })
    };
    assert!(
        eventually(Duration::from_secs(20), || step_gate_waiting(&orm)).await,
        "the step never started"
    );
    let mut rescheduling = orm.begin(SUPERUSER_ID).await.unwrap();
    rescheduling
        .query(
            "SELECT id FROM ir_module_post_commit WHERE key = 'slow' FOR UPDATE NOWAIT",
            &[],
        )
        .await
        .unwrap();
    open_gate(gate).await;
    let outcome = running.await.unwrap().unwrap();
    let report = &outcome.post_commit;
    assert_eq!(report.stopped, None, "{report:?}");
    assert!(
        report.failed.is_empty() && report.done.is_empty(),
        "{report:?}"
    );
    assert!(report.superseded.is_empty(), "{report:?}");
    assert_eq!(
        report
            .skipped_row_locked
            .iter()
            .map(|s| (s.module.as_str(), s.key.as_str()))
            .collect::<Vec<_>>(),
        [("w4", "slow")]
    );
    rescheduling.rollback().await.unwrap();
    drop(rescheduling);
    let waiting = system.pending_post_commit().await.unwrap();
    assert_eq!(waiting.len(), 1);
    assert!(
        matches!(&waiting[0].state, PendingState::Pending { step }
            if step.module.as_str() == "w4" && step.key.as_str() == "slow"),
        "{:?}",
        waiting[0]
    );
    assert_eq!(waiting[0].attempts, 1, "the first run was counted");
    let again = system.run_post_commit(&orm).await;
    assert!(again.is_clean(), "{again:?}");
    assert_eq!(again.done.len(), 1);
    assert_eq!(
        rows(&orm, "SELECT attempts::text FROM ir_module_post_commit").await,
        vec![vec![Some("2".to_owned())]]
    );
    db.cleanup().await.unwrap();
}

// ---- A5: the limits reported are the limits in force ---------------------------------------

fn lock_status_hook(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let rows = tx
            .query(
                "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')",
                &[],
            )
            .await?;
        let lock: String = rows[0].get(0);
        let statement: String = rows[0].get(1);
        log(format!("{lock} {statement}"));
        Ok(())
    })
}

static LIMITS_A: HookImpl =
    HookImpl::new("l6a", HookKind::PostInit, "l6a_limits", lock_status_hook).at(file!(), line!());
static LIMITS_B: HookImpl =
    HookImpl::new("l6b", HookKind::PostInit, "l6b_limits", lock_status_hook).at(file!(), line!());

/// A6: the operation's limits only lower the session's: a session with lower values keeps them
/// (and the errors report them), a session with higher values or none gets the operation's.
#[tokio::test]
async fn operation_limits_only_lower_the_session_limits() {
    let addons = Addons::new();
    addons
        .add("l6a", "name = 'L6A'\npost_init_hook = 'l6a_limits'\n")
        .add("l6b", "name = 'L6B'\npost_init_hook = 'l6b_limits'\n");
    let db = TestDb::create().await.unwrap();
    let setup = builder(&db, &addons)
        .hooks([&LIMITS_A, &LIMITS_B])
        .build()
        .unwrap();
    initialized(&setup).await;
    let client = db.pool().get().await.unwrap();
    let alter = |settings: &str| format!("ALTER DATABASE \"{}\" SET {settings}", db.name());

    // the session's values are lower than the defaults of 30 s and 30 min
    client
        .batch_execute(&alter("lock_timeout = '5s'"))
        .await
        .unwrap();
    client
        .batch_execute(&alter("statement_timeout = '2min'"))
        .await
        .unwrap();
    let pool = erp_orm::pool::create_pool(db.config(), 2).unwrap();
    let lowered = ModuleSystem::builder(pool, addons.index())
        .linked_hooks(false)
        .hooks([&LIMITS_A, &LIMITS_B])
        .build()
        .unwrap();
    let orm = lowered.load().await.unwrap().orm;
    committed(
        lowered
            .button_immediate_install(&su(&orm), &[n("l6a")])
            .await
            .unwrap(),
    );
    assert_eq!(take_log(), ["5s 2min"]);

    // higher (or none) in the session: the operation's own limits
    client
        .batch_execute(&alter("lock_timeout = '10min'"))
        .await
        .unwrap();
    client
        .batch_execute(&alter("statement_timeout = 0"))
        .await
        .unwrap();
    let pool = erp_orm::pool::create_pool(db.config(), 2).unwrap();
    let raised = ModuleSystem::builder(pool, addons.index())
        .linked_hooks(false)
        .hooks([&LIMITS_A, &LIMITS_B])
        .build()
        .unwrap();
    let orm = raised.load().await.unwrap().orm;
    committed(
        raised
            .button_immediate_install(&su(&orm), &[n("l6b")])
            .await
            .unwrap(),
    );
    assert_eq!(take_log(), ["30s 30min"]);
    drop(client);
    db.cleanup().await.unwrap();
}

/// A5: a plain load that waits for another operation longer than the wait timeout fails with
/// [`Error::OperationWaitTimeout`] naming the limit that ran out.
#[tokio::test]
async fn a_load_that_waits_too_long_reports_the_wait_limit() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .operation_wait_timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    system
        .button_install(&su(&orm), &[n("a")], &InstallOptions::new())
        .await
        .unwrap();
    let mut holder = orm.begin(SUPERUSER_ID).await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await
        .unwrap();
    let err = system.load().await.unwrap_err();
    assert!(
        matches!(&err, Error::OperationWaitTimeout { limit } if *limit == Duration::from_millis(200)),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "A plain load waited longer than 200 ms for another module operation to finish"
    );
    holder.rollback().await.unwrap();
    drop(holder);
    // nothing was started: the scheduled state is still there
    assert_eq!(state(&orm, "a").await, "to install");
    db.cleanup().await.unwrap();
}

/// A5: a wait for the operation lock that someone cancels (`pg_cancel_backend`) is a canceled
/// statement with PostgreSQL's message as its source, not a timeout of the wait.
#[tokio::test]
async fn a_canceled_wait_for_the_operation_lock_is_not_reported_as_a_timeout() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\n");
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(builder(&db, &addons).build().unwrap());
    let orm = initialized(&system).await;
    system
        .button_install(&su(&orm), &[n("a")], &InstallOptions::new())
        .await
        .unwrap();
    let mut holder = orm.begin(SUPERUSER_ID).await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await
        .unwrap();
    let loading = {
        let system = Arc::clone(&system);
        tokio::spawn(async move { system.load().await })
    };
    assert!(
        eventually(Duration::from_secs(20), || advisory_waiting(
            &orm,
            OPERATION_LOCK
        ))
        .await,
        "the load never waited for the lock"
    );
    let canceled = rows(
        &orm,
        &format!(
            "SELECT pg_cancel_backend(pid)::text FROM pg_locks WHERE locktype = 'advisory' \
             AND NOT granted AND classid::bigint = {} AND objid::bigint = {} AND objsubid = 2 \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            OPERATION_LOCK.0, OPERATION_LOCK.1
        ),
    )
    .await;
    assert_eq!(canceled, vec![vec![Some("true".to_owned())]]);
    let err = loading.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, Error::StatementCanceled { statement_timeout, source }
            if statement_timeout.is_zero() && source.sqlstate() == Some("57014")),
        "{err:?}"
    );
    assert_eq!(
        chain(&err)[..2],
        [
            "A statement of a module operation was canceled (SQLSTATE 57014; statement_timeout \
             is none)",
            "canceling statement due to user request",
        ]
    );
    holder.rollback().await.unwrap();
    drop(holder);
    db.cleanup().await.unwrap();
}

// ---- A7: uninstall takes the schema lock; missing columns are named ------------------------

/// `t7_a` declares `t7.thing`, `t7_b` adds the column `b_code` to it.
fn uninstall_fixture() -> Addons {
    let addons = Addons::new();
    addons
        .add("t7_a", "name = 'T7A'\n")
        .add("t7_b", "name = 'T7B'\ndepends = ['t7_a']\n");
    addons
}

async fn columns_of(orm: &Orm, table: &str) -> Vec<String> {
    rows(
        orm,
        &format!(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = '{table}' ORDER BY 1"
        ),
    )
    .await
    .into_iter()
    .map(|r| r[0].clone().unwrap())
    .collect()
}

/// A7: dropping the columns and tables of an uninstalled module waits for the schema lock like
/// every schema change (here it is held, and the lock timeout is short), so the uninstall fails
/// with the lock error, nothing is dropped and the module stays installed; once the lock is free
/// the uninstall goes through.
#[tokio::test]
async fn uninstall_takes_the_schema_lock_before_dropping() {
    let addons = uninstall_fixture();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model("t7_a", "t7.thing"),
            extension("t7_b", "t7.thing", "b_code"),
        ])
        .lock_timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let env = su(&orm);
    committed(
        system
            .button_immediate_install(&env, &[n("t7_b")])
            .await
            .unwrap(),
    );
    assert!(
        columns_of(&orm, "t7_thing")
            .await
            .contains(&"b_code".to_owned())
    );

    let mut holder = orm.begin(SUPERUSER_ID).await.unwrap();
    holder.query(SCHEMA_LOCK_SQL, &[]).await.unwrap();
    let err = system
        .button_immediate_uninstall(&env, &[n("t7_b")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::LockNotAvailable { lock_timeout, source }
            if *lock_timeout == Duration::from_millis(200) && source.sqlstate() == Some("55P03")),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "A statement of a module operation could not get a database lock (SQLSTATE 55P03; \
         lock_timeout is 200 ms)"
    );
    holder.rollback().await.unwrap();
    drop(holder);
    assert!(
        columns_of(&orm, "t7_thing")
            .await
            .contains(&"b_code".to_owned())
    );
    assert_eq!(state(&orm, "t7_b").await, "installed");

    committed(
        system
            .button_immediate_uninstall(&env, &[n("t7_b")])
            .await
            .unwrap(),
    );
    assert!(
        !columns_of(&orm, "t7_thing")
            .await
            .contains(&"b_code".to_owned())
    );
    db.cleanup().await.unwrap();
}

/// A7: a column of the uninstalled module that is gone from the database is named, with its
/// table, instead of PostgreSQL's error for the first one.
#[tokio::test]
async fn uninstall_names_missing_columns() {
    let addons = uninstall_fixture();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model("t7_a", "t7.thing"),
            extension("t7_b", "t7.thing", "b_code"),
        ])
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let env = su(&orm);
    committed(
        system
            .button_immediate_install(&env, &[n("t7_b")])
            .await
            .unwrap(),
    );
    exec(&orm, "ALTER TABLE t7_thing DROP COLUMN b_code").await;
    let err = system
        .button_immediate_uninstall(&env, &[n("t7_b")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::SchemaColumnsMissing { modules, columns }
            if *modules == [n("t7_b")] && *columns == ["t7_thing.b_code"]),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Cannot remove the schema of modules t7_b: columns t7_thing.b_code do not exist (they \
         were removed outside ERPForAll)"
    );
    assert_eq!(state(&orm, "t7_b").await, "installed", "states reset");
    db.cleanup().await.unwrap();
}

// ---- A9: retries are counted; steps on dropped tables go with the uninstall ----------------

fn failing_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        schedule_post_commit(tx, &n("f9"), &pk("boom"), &analyze("no_such_table")).await
    })
}

static FAILING_STEP: HookImpl =
    HookImpl::new("f9", HookKind::PostInit, "f9_boom", failing_step).at(file!(), line!());

/// A9: a step that fails stays queued, runs again in every phase, and shows how often and why.
#[tokio::test]
async fn a_failing_step_shows_its_attempts_and_its_error() {
    let addons = Addons::new();
    addons.add("f9", "name = 'F9'\npost_init_hook = 'f9_boom'\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&FAILING_STEP])
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let outcome = system
        .button_immediate_install(&su(&orm), &[n("f9")])
        .await
        .unwrap();
    assert_eq!(outcome.post_commit.failed.len(), 1);
    assert!(!outcome.post_commit.failed[0].parked, "the first failure");
    let step = erp_modules::PostCommitStep::new(n("f9"), pk("boom"));
    let error = "relation \"public.no_such_table\" does not exist".to_owned();
    let sqlstate = Some("42P01".to_owned());
    // attempts 1 and 2 stay failed and run again; the 3rd failure parks the step
    for attempts in 1..=2_i64 {
        let pending = system.pending_post_commit().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempts, attempts);
        assert_eq!(
            pending[0].state,
            PendingState::Failed {
                step: step.clone(),
                error: error.clone(),
                sqlstate: sqlstate.clone(),
            }
        );
        let report = system.run_post_commit(&orm).await;
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert_eq!(report.failed[0].parked, attempts == 2, "{report:?}");
    }
    let pending = system.pending_post_commit().await.unwrap();
    assert_eq!(
        pending[0].attempts,
        i64::from(erp_modules::MAX_POST_COMMIT_ATTEMPTS)
    );
    assert_eq!(
        pending[0].state,
        PendingState::Parked {
            step: step.clone(),
            error,
            sqlstate,
        }
    );
    // parked: no phase runs it any more, and the report is clean
    for _ in 0..2 {
        let report = system.run_post_commit(&orm).await;
        assert_eq!(report, erp_modules::PostCommitReport::default());
    }
    assert_eq!(
        system.pending_post_commit().await.unwrap()[0].attempts,
        i64::from(erp_modules::MAX_POST_COMMIT_ATTEMPTS),
        "no further attempt was counted"
    );
    // scheduling it again starts the count over
    schedule_managed(&orm, &n("f9"), &pk("boom"), &analyze("no_such_table"))
        .await
        .unwrap();
    let pending = system.pending_post_commit().await.unwrap();
    assert_eq!(pending[0].attempts, 0);
    assert_eq!(pending[0].state, PendingState::Pending { step });
    let report = system.run_post_commit(&orm).await;
    assert_eq!(
        report.failed.len(),
        1,
        "it runs again once rescheduled: {report:?}"
    );
    db.cleanup().await.unwrap();
}

/// A9: the revision counter does not wrap around: past the range of its column, scheduling the
/// step again fails with PostgreSQL's out-of-range error.
#[tokio::test]
async fn the_revision_counter_fails_loudly_instead_of_wrapping() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let orm = initialized(&system).await;
    let schedule = |orm: &Orm| {
        let orm = orm.clone();
        async move { schedule_managed(&orm, &n("base"), &pk("k"), &analyze("ir_module_module")).await }
    };
    schedule(&orm).await.unwrap();
    exec(
        &orm,
        "UPDATE ir_module_post_commit SET revision = 2147483647",
    )
    .await;
    let err = schedule(&orm).await.unwrap_err();
    assert_eq!(err.sqlstate(), Some("22003"), "{err:?}");
    db.cleanup().await.unwrap();
}

fn name_index(table: &str, index: &str) -> erp_modules::PostCommitAction {
    erp_modules::PostCommitAction::create_index(
        erp_modules::CreateIndex::new(
            erp_modules::TableName::parse(table).unwrap(),
            erp_modules::IndexName::parse(index).unwrap(),
            vec![erp_modules::IndexColumn::new("name").unwrap()],
        )
        .unwrap(),
    )
}

fn index_step_on_other_table(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        schedule_post_commit(
            tx,
            &n("t9_other"),
            &pk("own_name"),
            &name_index("t9_own", "t9_own_name_idx"),
        )
        .await?;
        schedule_post_commit(
            tx,
            &n("t9_other"),
            &pk("keep_name"),
            &name_index("t9_keep", "t9_keep_name_idx"),
        )
        .await
    })
}

static OTHER_TABLE_STEPS: HookImpl = HookImpl::new(
    "t9_other",
    HookKind::PostInit,
    "t9_steps",
    index_step_on_other_table,
)
.at(file!(), line!());

/// A9: uninstalling a module drops its tables, and the queued index steps of other modules on
/// those tables go with them (they could only fail from then on); steps on tables that stay are
/// kept.
#[tokio::test]
async fn uninstall_removes_queued_index_steps_of_dropped_tables() {
    let addons = Addons::new();
    addons
        .add("t9_own", "name = 'T9Own'\n")
        .add("t9_keep", "name = 'T9Keep'\n")
        .add(
            "t9_other",
            "name = 'T9Other'\npost_init_hook = 't9_steps'\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("t9_own", "t9.own"), model("t9_keep", "t9.keep")])
        .hooks([&OTHER_TABLE_STEPS])
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let env = su(&orm);
    committed(
        system
            .button_immediate_install(&env, &[n("t9_own"), n("t9_keep")])
            .await
            .unwrap(),
    );
    // another phase holds the lock: the steps of t9_other stay queued
    let holder = db.pool().get().await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_lock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&env, &[n("t9_other")])
        .await
        .unwrap();
    assert!(matches!(
        installed.post_commit.stopped,
        Some(PostCommitStop::Busy { .. })
    ));
    let queued = |pending: Vec<erp_modules::PendingPostCommit>| {
        pending
            .iter()
            .map(|p| p.step().unwrap().key.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        queued(system.pending_post_commit().await.unwrap()),
        ["own_name", "keep_name"]
    );
    let uninstalled = system
        .button_immediate_uninstall(&env, &[n("t9_own")])
        .await
        .unwrap();
    assert!(matches!(
        uninstalled.post_commit.stopped,
        Some(PostCommitStop::Busy { .. })
    ));
    assert_eq!(
        queued(system.pending_post_commit().await.unwrap()),
        ["keep_name"],
        "the step on the dropped table went with it"
    );
    holder
        .query(
            "SELECT pg_advisory_unlock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    drop(holder);
    let report = system.run_post_commit(&orm).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.done.len(), 1);
    db.cleanup().await.unwrap();
}

// ---- A10: a malformed queue row does not stop a load ---------------------------------------

/// A10: a row of the queue the module system could not have written is listed as invalid, with
/// the reason, by the load and by `pending_post_commit`; the load still succeeds.
#[tokio::test]
async fn a_malformed_queue_row_is_listed_and_does_not_block_the_load() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let orm = initialized(&system).await;
    // the table's CHECK constraints refuse a table name outside the rule (defense in depth)
    let client = db.pool().get().await.unwrap();
    let err = client
        .execute(
            "INSERT INTO ir_module_post_commit \
             (module, key, kind, table_name, state, revision, attempts) \
             VALUES ('base', 'two', 'analyze', 'ir_module_module; DROP TABLE x', 'pending', 1, 0)",
            &[],
        )
        .await
        .unwrap_err();
    let db_error = err.as_db_error().unwrap();
    assert_eq!(db_error.code().code(), "23514");
    assert!(
        db_error.message().contains("_table_name_check"),
        "{}",
        db_error.message()
    );
    // and a kind without the columns it needs
    let err = client
        .execute(
            "INSERT INTO ir_module_post_commit \
             (module, key, kind, table_name, state, revision, attempts) \
             VALUES ('base', 'two', 'create_index', 'ir_module_module', 'pending', 1, 0)",
            &[],
        )
        .await
        .unwrap_err();
    let db_error = err.as_db_error().unwrap();
    assert_eq!(db_error.code().code(), "23514");
    assert!(
        db_error.message().contains("_kind_columns_check"),
        "{}",
        db_error.message()
    );
    drop(client);
    // what the constraints let through but the checks of scheduling refuse is listed
    exec(
        &orm,
        "INSERT INTO ir_module_post_commit \
         (module, key, kind, table_name, index_name, spec, state, revision, attempts) \
         VALUES ('Bad Module', 'k', 'analyze', 'ir_module_module', NULL, NULL, 'pending', 1, 0), \
                ('base', 'fine', 'analyze', 'ir_module_module', NULL, NULL, 'pending', 1, 0), \
                ('base', 'two', 'create_index', 'ir_module_module', 'two_idx', '{\"x\": 1}', \
                 'pending', 1, 0)",
    )
    .await;
    let loaded = system.load().await.unwrap();
    let listed = loaded.post_commit_pending;
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].step(), None);
    assert!(
        matches!(&listed[0].state, PendingState::Invalid(row)
            if row.module == "Bad Module" && row.key == "k"
                && row.reason.starts_with("module \"Bad Module\": ")),
        "{:?}",
        listed[0]
    );
    assert!(
        matches!(&listed[1].state, PendingState::Pending { step }
        if step.key.as_str() == "fine"),
        "{:?}",
        listed[1]
    );
    assert_eq!(listed[2].step(), None);
    let PendingState::Invalid(row) = &listed[2].state else {
        panic!("{:?}", listed[2]);
    };
    assert_eq!((row.module.as_str(), row.key.as_str()), ("base", "two"));
    assert_eq!(row.id, listed[2].id);
    assert!(
        row.reason
            .starts_with("invalid post-commit action: invalid spec: "),
        "{}",
        row.reason
    );
    assert_eq!(
        system.pending_post_commit().await.unwrap(),
        listed,
        "pending_post_commit reports the same"
    );
    // a phase marks them, counts the attempts and reports them; after three they are parked
    for round in 1..=3 {
        let report = system.run_post_commit(&orm).await;
        assert_eq!(report.invalid.len(), 2, "round {round}: {report:?}");
    }
    let report = system.run_post_commit(&orm).await;
    assert!(
        report.invalid.is_empty(),
        "parked rows are not read again: {report:?}"
    );
    db.cleanup().await.unwrap();
}

// ---- A14: audit columns --------------------------------------------------------------------

/// A14: the rows the module system writes itself carry the audit columns the ORM would have
/// filled (the system user, the time in UTC whatever the session's time zone), and a state change
/// moves `write_date`.
#[tokio::test]
async fn module_rows_have_audit_columns_in_utc() {
    let addons = Addons::new();
    addons.add("a", "name = 'A'\ncategory = 'Sales/Retail'\n");
    let db = TestDb::create().await.unwrap();
    let client = db.pool().get().await.unwrap();
    client
        .batch_execute(&format!(
            "ALTER DATABASE \"{}\" SET timezone = 'Asia/Jakarta'",
            db.name()
        ))
        .await
        .unwrap();
    drop(client);
    let pool = erp_orm::pool::create_pool(db.config(), 2).unwrap();
    let system = ModuleSystem::builder(pool, addons.index())
        .linked_hooks(false)
        .build()
        .unwrap();
    let orm = initialized(&system).await;
    let check = |table: &'static str| {
        let orm = orm.clone();
        async move {
            rows(
                &orm,
                &format!(
                    "SELECT count(*)::text, \
                     count(*) FILTER (WHERE create_uid = 1 AND write_uid = 1 \
                       AND create_date IS NOT NULL AND write_date IS NOT NULL \
                       AND abs(extract(epoch FROM ((now() AT TIME ZONE 'UTC') - create_date))) < 600 \
                       AND abs(extract(epoch FROM ((now() AT TIME ZONE 'UTC') - write_date))) < 600)::text \
                     FROM {table}"
                ),
            )
            .await
        }
    };
    for table in ["ir_module_module", "ir_module_category"] {
        let counts = check(table).await;
        assert_eq!(counts[0][0], counts[0][1], "{table}: every row is complete");
        assert_ne!(counts[0][0].as_deref(), Some("0"), "{table} has rows");
    }
    let before = rows(
        &orm,
        "SELECT write_date::text FROM ir_module_module WHERE name = 'a'",
    )
    .await;
    committed(
        system
            .button_immediate_install(&su(&orm), &[n("a")])
            .await
            .unwrap(),
    );
    let after = rows(
        &orm,
        "SELECT write_date::text FROM ir_module_module WHERE name = 'a'",
    )
    .await;
    assert_ne!(before, after, "the state change moved write_date");
    db.cleanup().await.unwrap();
}

// ---- A16: the lock holder is named ---------------------------------------------------------

/// A16: a phase that finds the post-commit lock held reports the holder's backend id.
#[tokio::test]
async fn a_busy_phase_names_the_lock_holder() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let orm = initialized(&system).await;
    let holder = db.pool().get().await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_lock($1, $2)",
            &[&POST_COMMIT_LOCK.0, &POST_COMMIT_LOCK.1],
        )
        .await
        .unwrap();
    let pid: i32 = holder
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let report = system.run_post_commit(&orm).await;
    assert_eq!(
        report.stopped,
        Some(PostCommitStop::Busy {
            holder_pid: Some(pid)
        })
    );
    drop(holder);
    db.cleanup().await.unwrap();
}
