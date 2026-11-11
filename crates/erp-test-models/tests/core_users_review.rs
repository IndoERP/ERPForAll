//! core fixes from the review (D4 constraint ordering with x2many
//! fields, R5 `acting_as` keeps the debug bit, X5/R6 sudo commands on new records, S1 stale-policy
//! refresh on cached reads).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use erp_orm::{Operation, PrincipalBinding};
use erp_registry::ModelSpec;
use erp_sdk::{Command, Domain, Error, ErrorKind, RawVals, Transaction, Value};
use erp_test_models::test_mixed_constraint::{self, RUNS};
use erp_test_models::{base_users, test_sudo_commands};
use erp_testkit::{Harness, TestRules};
use support::{map, vmap};

fn s(text: &str) -> Value {
    Value::str(text)
}

fn cmd(code: i64, id: i64, values: Value) -> Value {
    Value::list([Value::Int(code), Value::Int(id), values])
}

fn dict(pairs: &[(&str, Value)]) -> Value {
    Value::Map(std::sync::Arc::new(vmap(pairs)))
}

async fn mixc_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_mixed_constraint::ADDON])
        .build()
        .await
        .expect("harness")
}

async fn create_group(tx: &mut Transaction, pairs: &[(&str, Value)]) -> erp_sdk::Result<i64> {
    Ok(tx
        .env()
        .records("test_mixc.group")?
        .create_values(tx, &[map(pairs)])
        .await?
        .ids()[0])
}

#[track_caller]
fn assert_needs_line(error: &Error) {
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    assert_eq!(error.message(), "A flagged group needs a line");
}

/// A constraint naming a scalar and an x2many field sees the lines the same create sets, and
/// runs once.
#[tokio::test]
async fn d4_create_runs_mixed_constraint_after_the_x2many_is_written() {
    let h = mixc_harness().await;
    let mut tx = h.begin().await.unwrap();
    let before = RUNS.load(Ordering::SeqCst);
    create_group(
        &mut tx,
        &[
            ("name", s("ok")),
            ("flagged", Value::Bool(true)),
            (
                "line_ids",
                Value::list([cmd(0, 0, dict(&[("name", s("l"))]))]),
            ),
        ],
    )
    .await
    .expect("the group has a line");
    assert_eq!(RUNS.load(Ordering::SeqCst) - before, 1, "ran exactly once");
    tx.rollback().await.unwrap();

    // without lines the same constraint refuses
    let mut tx = h.begin().await.unwrap();
    let error = create_group(&mut tx, &[("flagged", Value::Bool(true))])
        .await
        .unwrap_err();
    assert_needs_line(&error);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The write counterpart: the new lines count, and removing the last line is refused rather
/// than skipped.
#[tokio::test]
async fn d4_write_runs_mixed_constraint_after_the_x2many_is_written() {
    let h = mixc_harness().await;
    let mut tx = h.begin().await.unwrap();
    let id = create_group(&mut tx, &[("name", s("g"))]).await.unwrap();
    let group = tx
        .env()
        .records("test_mixc.group")
        .unwrap()
        .browse([id])
        .unwrap();
    let before = RUNS.load(Ordering::SeqCst);
    group
        .write_values(
            &mut tx,
            &map(&[
                ("flagged", Value::Bool(true)),
                (
                    "line_ids",
                    Value::list([cmd(0, 0, dict(&[("name", s("l"))]))]),
                ),
            ]),
        )
        .await
        .expect("flagged together with its first line");
    assert_eq!(RUNS.load(Ordering::SeqCst) - before, 1, "ran exactly once");
    tx.commit().await.unwrap();

    // dropping the only line of a flagged group is refused
    let mut tx = h.begin().await.unwrap();
    let group = tx
        .env()
        .records("test_mixc.group")
        .unwrap()
        .browse([id])
        .unwrap();
    let error = group
        .write_values(
            &mut tx,
            &map(&[("line_ids", Value::list([cmd(5, 0, Value::Int(0))]))]),
        )
        .await
        .unwrap_err();
    assert_needs_line(&error);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// R5 and R6/X5: the environment x2many commands run with on a model that refuses sudo commands

/// One `check_model` call on `test_sudo.line`: subject, elevated, debug, operation.
type Seen = (i64, bool, bool, Operation);

/// Allows everything and records who asked for `test_sudo.line`.
struct Recorder(Arc<Mutex<Vec<Seen>>>);

impl TestRules for Recorder {
    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_sdk::Result<()> {
        if model.name == "test_sudo.line" {
            let p = b.principal();
            self.0.lock().unwrap().push((
                p.subject().get(),
                p.elevation().is_full(),
                p.debug(),
                op,
            ));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> erp_sdk::Result<Domain> {
        Ok(Domain::TRUE)
    }
}

const DEMO: i64 = 77;

async fn sudo_harness() -> (Harness, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let h = Harness::builder([&base_users::ADDON, &test_sudo_commands::ADDON])
        .rules(Recorder(Arc::clone(&seen)))
        .build()
        .await
        .expect("harness");
    (h, seen)
}

async fn demo_with_owner_and_line(h: &Harness) -> (i64, i64) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES ($1, 'demo', 'demo', true)",
        &[&DEMO],
    )
    .await
    .unwrap();
    let owner = tx
        .env()
        .records("test_sudo.owner")
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", s("owner"))])])
        .await
        .unwrap()
        .ids()[0];
    let line = tx
        .env()
        .records("test_sudo.line")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[("name", s("line")), ("owner_id", Value::Int(owner))])],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    (owner, line)
}

/// R5: commands on a protected comodel run as the original user, and an owner environment in
/// debug mode keeps the debug bit (it gates `base.group_no_one`).
#[tokio::test]
async fn r5_sudo_commands_keep_the_debug_flag() {
    let (h, seen) = sudo_harness().await;
    let (owner, _line) = demo_with_owner_and_line(&h).await;
    for debug in [true, false] {
        let mut tx = h.begin_as(DEMO).await.unwrap();
        seen.lock().unwrap().clear();
        let env = tx.env().with_debug(debug);
        env.records("test_sudo.owner")
            .unwrap()
            .sudo(true)
            .browse([owner])
            .unwrap()
            .write_values(
                &mut tx,
                &map(&[(
                    "line_ids",
                    Value::list([cmd(0, 0, dict(&[("name", s("new"))]))]),
                )]),
            )
            .await
            .unwrap();
        let seen_now = seen.lock().unwrap().clone();
        assert!(
            seen_now.contains(&(DEMO, false, debug, Operation::Create)),
            "debug={debug}: the line was created as the original, unelevated user: {seen_now:?}"
        );
        tx.rollback().await.unwrap();
    }
    h.cleanup().await.unwrap();
}

/// R6/X5: commands on a new record (the onchange path) reach a protected comodel as the
/// original user as well. UPDATE of a database line writes it, so it is checked.
#[tokio::test]
async fn x5_commands_on_new_records_run_as_the_original_user() {
    let (h, seen) = sudo_harness().await;
    let (owner, line) = demo_with_owner_and_line(&h).await;
    let mut tx = h.begin_as(DEMO).await.unwrap();
    seen.lock().unwrap().clear();
    let owners = tx.env().records("test_sudo.owner").unwrap().sudo(true);
    let new_owner = tx
        .new_record(
            &owners,
            RawVals::new(),
            Some(erp_sdk::RecId::from_real(owner).unwrap()),
            None,
        )
        .await
        .unwrap();
    new_owner
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "line_ids",
                [Command::update(
                    erp_sdk::RecId::from_real(line).unwrap(),
                    RawVals::new().set_cell("rank", erp_sdk::Cell::Int(5)),
                )],
            ),
        )
        .await
        .unwrap();
    let seen_now = seen.lock().unwrap().clone();
    assert!(
        seen_now
            .iter()
            .all(|(subject, su, _, _)| *subject == DEMO && !su)
            && !seen_now.is_empty(),
        "every check on the protected line ran as the unelevated original user: {seen_now:?}"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
