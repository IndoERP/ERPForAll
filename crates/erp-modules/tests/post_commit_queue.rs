//! Module operations, fourth review round (lead review of 0533b70): the post-commit queue is
//! drained until empty, a phase never waits for a connection the operation still holds, a
//! rescheduled step is never recorded with the result of the run it superseded, a load that leaves
//! steps pending says so, and uninstall names the tables it cannot find.
//!
//! The recording statics are per test process: the suite runs under nextest, which gives every
//! test its own process (see `common/mod.rs`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use erp_fields::FieldType;
use erp_modules::error::Error;
use erp_modules::hooks::{HookImpl, HookKind};
use erp_modules::system::{
    InitOptions, ModuleSystem, PostCommitReport, PostCommitStop, schedule_post_commit,
};
use erp_orm::{BoxFuture, Orm, Transaction};
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;

/// `(module, key)` of each step of a list.
fn keys(steps: &[erp_modules::system::PostCommitStep]) -> Vec<(String, String)> {
    steps
        .iter()
        .map(|s| (s.module.as_str().to_owned(), s.key.to_string()))
        .collect()
}

fn pair(module: &str, key: &str) -> (String, String) {
    (module.to_owned(), key.to_owned())
}

/// `w`'s post_init: a step that blocks until the test opens the gate.
fn slow_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { schedule_post_commit(tx, &n("w"), &pk("slow"), &gate_action()).await })
}

static SLOW_STEP: HookImpl =
    HookImpl::new("w", HookKind::PostInit, "w_slow", slow_step).at(file!(), line!());

/// `v`'s post_init: a quick step.
fn quick_step(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        schedule_post_commit(tx, &n("v"), &pk("one"), &analyze("ir_module_module")).await
    })
}

static QUICK_STEP: HookImpl =
    HookImpl::new("v", HookKind::PostInit, "v_quick", quick_step).at(file!(), line!());

/// `r`'s post_init: schedules `w`'s step again, with the same statement.
fn reschedule_slow(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { schedule_post_commit(tx, &n("w"), &pk("slow"), &gate_action()).await })
}

static RESCHEDULE_SLOW: HookImpl =
    HookImpl::new("r", HookKind::PostInit, "r_again", reschedule_slow).at(file!(), line!());

/// Whether the gated step is running and waiting for the gate.
async fn slow_step_running(orm: &Orm) -> bool {
    step_gate_waiting(orm).await
}

fn addons_w_v_r() -> Addons {
    let addons = Addons::new();
    addons
        .add("w", "name = 'W'\npost_init_hook = 'w_slow'\n")
        .add("v", "name = 'V'\npost_init_hook = 'v_quick'\n")
        .add("r", "name = 'R'\npost_init_hook = 'r_again'\n");
    addons
}

/// Install `first` in the background, wait until its phase runs the slow step, then install
/// `second` (the gate holds the first phase's step meanwhile): returns the reports of both phases (the second's first).
async fn second_operation_during_a_phase(
    system: &Arc<ModuleSystem>,
    orm: &Orm,
    first: &str,
    second: &str,
) -> (PostCommitReport, PostCommitReport) {
    let gate = close_step_gate(orm).await;
    let running = {
        let system = Arc::clone(system);
        let env = su(orm);
        let first = n(first);
        tokio::spawn(async move { system.button_immediate_install(&env, &[first]).await })
    };
    assert!(
        eventually(Duration::from_secs(20), || slow_step_running(orm)).await,
        "the slow step never started"
    );
    let during = system
        .button_immediate_install(&su(orm), &[n(second)])
        .await
        .unwrap();
    // the second operation is done while the first phase is still inside its step: open the gate
    open_gate(gate).await;
    let first = running.await.unwrap().unwrap();
    (during.post_commit, first.post_commit)
}

// ---- 1: a phase drains the queue, also the steps committed while it ran ---------------------

/// Lead review 1: operation `v` commits while the phase of operation `w` runs; `v`'s phase finds
/// the lock held (`Busy`), and the running phase reads the queue again after each pass, so it
/// runs `v`'s step too. Nothing is left pending.
#[tokio::test]
async fn a_phase_runs_the_steps_committed_while_it_ran() {
    let addons = addons_w_v_r();
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(
        builder(&db, &addons)
            .hooks([&SLOW_STEP, &QUICK_STEP, &RESCHEDULE_SLOW])
            .build()
            .unwrap(),
    );
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let (during, first) = second_operation_during_a_phase(&system, &init.orm, "w", "v").await;
    assert!(
        matches!(during.stopped, Some(PostCommitStop::Busy { .. })),
        "{:?}",
        during.stopped
    );
    assert!(during.done.is_empty(), "{during:?}");
    assert!(first.is_clean(), "{first:?}");
    assert_eq!(keys(&first.done), [pair("w", "slow"), pair("v", "one")]);
    assert!(first.superseded.is_empty(), "{first:?}");
    assert!(system.pending_post_commit().await.unwrap().is_empty());
    assert_eq!(
        rows(
            &init.orm,
            "SELECT module, key, state FROM ir_module_post_commit ORDER BY id"
        )
        .await,
        vec![
            vec![Some("w".into()), Some("slow".into()), Some("done".into())],
            vec![Some("v".into()), Some("one".into()), Some("done".into())],
        ]
    );
    db.cleanup().await.unwrap();
}

// ---- 4: a step rescheduled with the same statement is not recorded as done ------------------

/// Lead review 4: while the phase runs `w`'s step, another operation schedules it again with the
/// same statement. The running pass does not record its result (the step's revision moved: it is
/// superseded), and the phase runs the rescheduled step in its next pass.
#[tokio::test]
async fn a_step_rescheduled_with_the_same_statement_runs_again() {
    let addons = addons_w_v_r();
    let db = TestDb::create().await.unwrap();
    let system = Arc::new(
        builder(&db, &addons)
            .hooks([&SLOW_STEP, &QUICK_STEP, &RESCHEDULE_SLOW])
            .build()
            .unwrap(),
    );
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let (during, first) = second_operation_during_a_phase(&system, &init.orm, "w", "r").await;
    assert!(
        matches!(during.stopped, Some(PostCommitStop::Busy { .. })),
        "{:?}",
        during.stopped
    );
    assert!(first.is_clean(), "{first:?}");
    assert_eq!(keys(&first.superseded), [pair("w", "slow")]);
    assert_eq!(keys(&first.done), [pair("w", "slow")]);
    assert_eq!(
        rows(
            &init.orm,
            "SELECT key, state, revision::text FROM ir_module_post_commit ORDER BY id"
        )
        .await,
        vec![vec![
            Some("slow".into()),
            Some("done".into()),
            Some("2".into())
        ]]
    );
    db.cleanup().await.unwrap();
}

// ---- 2: the phase never waits for the operation's own connection ----------------------------

/// Lead review 2: with a pool of one connection, initialization and an immediate install run
/// their post-commit phases: the operation's connection went back to the pool before the phase
/// asked for one (the test pool gives up after 30 s; holding it would report a stopped phase).
#[tokio::test]
async fn post_commit_phase_runs_with_a_one_connection_pool() {
    let addons = addons_w_v_r();
    let db = TestDb::builder().pool_size(1).create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&SLOW_STEP, &QUICK_STEP, &RESCHEDULE_SLOW])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(init.post_commit.is_clean(), "{:?}", init.post_commit);
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("v")])
        .await
        .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    assert_eq!(keys(&installed.post_commit.done), [pair("v", "one")]);
    drop(installed);
    drop(init);
    db.cleanup().await.unwrap();
}

// ---- 5: a load that leaves steps pending logs a warning -------------------------------------

/// The events of the module system's operations (`erp_modules::system`) logged while it is the
/// thread's default subscriber: (level, message). Manifest warnings and the like are left out.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<(tracing::Level, String)>>>);

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() != "erp_modules::system" {
            return;
        }
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), message.0));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Lead review 5: a plain load that leaves post-commit steps pending logs a warning with their
/// number (the load does not run them); a load with nothing pending logs none.
#[tokio::test]
async fn load_warns_when_steps_stay_pending() {
    let addons = addons_w_v_r();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .hooks([&SLOW_STEP, &QUICK_STEP, &RESCHEDULE_SLOW])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    // another phase holds the lock: v's step stays pending
    let holder = db.pool().get().await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_lock($1, $2)",
            &[
                &erp_modules::POST_COMMIT_LOCK.0,
                &erp_modules::POST_COMMIT_LOCK.1,
            ],
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
    // a pooled connection keeps its session locks: release before it goes back
    holder
        .query(
            "SELECT pg_advisory_unlock($1, $2)",
            &[
                &erp_modules::POST_COMMIT_LOCK.0,
                &erp_modules::POST_COMMIT_LOCK.1,
            ],
        )
        .await
        .unwrap();
    drop(holder);

    let capture = Capture::default();
    let guard = tracing::subscriber::set_default(capture.clone());
    let loaded = system.load().await.unwrap();
    assert_eq!(loaded.post_commit_pending.len(), 1);
    let warnings: Vec<String> = capture
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, _)| *level == tracing::Level::WARN)
        .map(|(_, message)| message.clone())
        .collect();
    assert_eq!(
        warnings,
        ["the load left 1 post-commit step pending; run ModuleSystem::run_post_commit"]
    );
    drop(loaded);

    // nothing pending: no warning
    let report = system.run_post_commit(&init.orm).await;
    assert!(report.is_clean(), "{report:?}");
    capture.0.lock().unwrap().clear();
    let loaded = system.load().await.unwrap();
    assert!(loaded.post_commit_pending.is_empty());
    assert!(
        !capture
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(level, _)| *level == tracing::Level::WARN),
        "{:?}",
        capture.0.lock().unwrap()
    );
    drop(guard);
    drop(loaded);
    db.cleanup().await.unwrap();
}

// ---- 8: uninstall names the tables it cannot find -------------------------------------------

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

/// Lead review 8: a relation table removed outside ERPForAll makes the uninstall fail with an
/// error naming it (not PostgreSQL's bare `42P01`); nothing changes.
#[tokio::test]
async fn uninstall_names_a_missing_relation_table() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            model("a", "test.a"),
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
    let relation = match &installed
        .orm
        .registry()
        .model("test.b")
        .unwrap()
        .field("a_ids")
        .unwrap()
        .ty
    {
        FieldType::Many2many {
            relation: Some(relation),
            ..
        } => relation.table.clone(),
        other => panic!("test.b.a_ids is not a stored many2many: {other:?}"),
    };
    exec(&init.orm, &format!("DROP TABLE {relation}")).await;
    let err = system
        .button_immediate_uninstall(&env, &[n("b")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::SchemaTablesMissing { modules, tables }
            if *modules == vec![n("b")] && *tables == vec![relation.clone()]),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "Cannot remove the schema of modules b: table {relation} does not exist (it was \
             removed outside ERPForAll)"
        )
    );
    assert_eq!(state(&init.orm, "b").await, "installed");
    assert!(table_exists(&init.orm, "test_b").await);
    drop(installed);
    db.cleanup().await.unwrap();
}

/// Lead review 8: several missing tables are named together, sorted, in the plural.
#[test]
fn missing_tables_message_names_every_table() {
    let err = Error::SchemaTablesMissing {
        modules: vec![n("b"), n("c")],
        tables: vec!["test_b".to_owned(), "test_b_rel".to_owned()],
    };
    assert_eq!(
        err.to_string(),
        "Cannot remove the schema of modules b, c: tables test_b, test_b_rel do not exist (they \
         were removed outside ERPForAll)"
    );
}

/// A hook cannot schedule a schema step, nor a step under a key the module system reserves for
/// them: either would let it replace the row of a schema step.
#[tokio::test]
async fn a_hook_cannot_schedule_a_schema_step_or_a_reserved_key() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let schema = erp_modules::PostCommitAction::Schema(
        erp_modules::SchemaStepRef::new(
            erp_modules::TableName::parse("ir_module_module").unwrap(),
            "backfill.ir_module_module",
            &["name"],
        )
        .unwrap(),
    );
    let err = schedule_managed(&init.orm, &n("base"), &pk("mine"), &schema)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "schedule_post_commit: schema steps are queued by the module system's schema update; a \
         hook cannot schedule one"
    );
    let err = schedule_managed(
        &init.orm,
        &n("base"),
        &pk("schema.thing"),
        &analyze("ir_module_module"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "schedule_post_commit: step keys starting with \"schema.\" are reserved for the schema \
         steps the module system queues; got \"schema.thing\""
    );
    assert_eq!(
        common::rows(
            &init.orm,
            "SELECT count(*)::text FROM ir_module_post_commit"
        )
        .await,
        vec![vec![Some("0".to_owned())]]
    );
    db.cleanup().await.unwrap();
}
