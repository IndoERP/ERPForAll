#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_orm::{Operation, PrincipalBinding};
use erp_registry::ModelSpec;
use erp_sdk::{Domain, Error, ErrorKind, Transaction, Value};
use erp_test_models::{base_users, test_sudo_commands};
use erp_testkit::{Harness, TestRules};
use support::{map, vmap};

/// The user the policy restricts; every other user is unrestricted.
const DEMO: i64 = 77;
/// The models only an unrestricted user may write.
const PROTECTED: [&str; 3] = ["test_sudo.group", "test_sudo.line", "test_sudo.free"];

struct DemoIsRestricted;

impl TestRules for DemoIsRestricted {
    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_sdk::Result<()> {
        if b.principal().subject().get() == DEMO
            && op != Operation::Read
            && PROTECTED.contains(&model.name)
        {
            return Err(refusal(model, op));
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

fn refusal(model: &ModelSpec, op: Operation) -> Error {
    Error::access(format!(
        "You are not allowed to {} '{}' ({}) records.",
        op.as_str(),
        model.description,
        model.name
    ))
}

fn denied(model: &str, op: Operation) -> String {
    format!(
        "You are not allowed to {} '{model}' ({model}) records.",
        op.as_str()
    )
}

fn s(text: &str) -> Value {
    Value::str(text)
}

fn int(n: i64) -> Value {
    Value::Int(n)
}

fn dict(pairs: &[(&str, Value)]) -> Value {
    Value::Map(std::sync::Arc::new(vmap(pairs)))
}

fn cmd(code: i64, id: i64, values: Value) -> Value {
    Value::list([int(code), int(id), values])
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_sudo_commands::ADDON])
        .rules(DemoIsRestricted)
        .build()
        .await
        .expect("harness")
}

#[track_caller]
fn assert_denied(result: erp_sdk::Result<impl std::fmt::Debug>, model: &str, op: Operation) {
    assert_denied_by("", result, model, op);
}

/// As [`assert_denied`], naming the variant (`sudo`, `with_user(admin)`) in a failure message.
#[track_caller]
fn assert_denied_by(
    label: &str,
    result: erp_sdk::Result<impl std::fmt::Debug>,
    model: &str,
    op: Operation,
) {
    let error = result.expect_err(&format!("{label}: the command must be refused"));
    assert_eq!(error.kind(), ErrorKind::Access, "{label}: {error}");
    assert_eq!(error.to_string(), denied(model, op), "{label}");
}

/// Users 1 (superuser row), `DEMO` and an admin, as database rows; one owner with one protected
/// line, one user with one protected group.
struct Fixture {
    admin: i64,
    owner: i64,
    line: i64,
    /// A line of another owner (LINK moves it).
    other_line: i64,
    user: i64,
    group: i64,
}

async fn create(tx: &mut Transaction, model: &str, pairs: &[(&str, Value)]) -> i64 {
    tx.env()
        .records(model)
        .unwrap()
        .create_values(tx, &[map(pairs)])
        .await
        .unwrap()
        .ids()[0]
}

async fn fixture(tx: &mut Transaction) -> Fixture {
    // the demo user must exist under the fixed id the policy names
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES ($1, 'demo', 'demo', true)",
        &[&DEMO],
    )
    .await
    .unwrap();
    let admin = create(
        tx,
        "res.users",
        &[("name", s("admin")), ("login", s("admin"))],
    )
    .await;
    let owner = create(tx, "test_sudo.owner", &[("name", s("owner"))]).await;
    let line = create(
        tx,
        "test_sudo.line",
        &[("name", s("line")), ("owner_id", int(owner))],
    )
    .await;
    let other_owner = create(tx, "test_sudo.owner", &[("name", s("other"))]).await;
    let other_line = create(
        tx,
        "test_sudo.line",
        &[("name", s("other line")), ("owner_id", int(other_owner))],
    )
    .await;
    let group = create(tx, "test_sudo.group", &[("name", s("public"))]).await;
    let user = create(
        tx,
        "test_sudo.user",
        &[
            ("name", s("foo")),
            (
                "group_ids",
                Value::list([cmd(6, 0, Value::list([int(group)]))]),
            ),
        ],
    )
    .await;
    Fixture {
        admin,
        owner,
        line,
        other_line,
        user,
        group,
    }
}

/// The two ways to reach the protected comodel with more rights than the transaction's user:
/// `sudo()` and `with_user(admin)`.
fn variants(
    tx: &Transaction,
    fx: &Fixture,
    model: &str,
    id: i64,
) -> Vec<(&'static str, erp_sdk::Records)> {
    let base = tx.env().records(model).unwrap().browse([id]).unwrap();
    vec![
        ("with_user(admin)", base.with_user(fx.admin)),
        ("sudo", base.sudo(true)),
    ]
}

/// `res.partner.user_ids`, which waits for, and its field-access message)
#[tokio::test]
async fn spec_rel_060_one2many_commands_run_as_the_original_user() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let fx = fixture(&mut setup).await;
    setup.commit().await.unwrap();

    let mut tx = h.begin_as(DEMO).await.unwrap();
    let line = "test_sudo.line";
    for (label, owner) in variants(&tx, &fx, "test_sudo.owner", fx.owner) {
        // CREATE through an owner created with the elevated environment
        let create_owner = tx.env().records("test_sudo.owner").unwrap();
        let create_owner = if label == "sudo" {
            create_owner.sudo(true)
        } else {
            create_owner.with_user(fx.admin)
        };
        let result = create_owner
            .create_values(
                &mut tx,
                &[map(&[
                    ("name", s("bar")),
                    (
                        "line_ids",
                        Value::list([cmd(0, 0, dict(&[("name", s("new"))]))]),
                    ),
                ])],
            )
            .await;
        assert_denied(result, line, Operation::Create);
        for (code, id, vals, op) in [
            (1, fx.line, dict(&[("rank", int(9))]), Operation::Write),
            (2, fx.line, int(0), Operation::Unlink),
            (3, fx.line, int(0), Operation::Write),
            (4, fx.other_line, int(0), Operation::Write),
        ] {
            let result = owner
                .write_values(
                    &mut tx,
                    &map(&[("line_ids", Value::list([cmd(code, id, vals)]))]),
                )
                .await;
            assert_denied(result, line, op);
        }
        let result = owner
            .write_values(
                &mut tx,
                &map(&[("line_ids", Value::list([cmd(5, 0, int(0))]))]),
            )
            .await;
        assert_denied(result, line, Operation::Write);
        let result = owner
            .write_values(
                &mut tx,
                &map(&[("line_ids", Value::list([cmd(6, 0, Value::list([]))]))]),
            )
            .await;
        assert_denied(result, line, Operation::Write);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_060_many2many_commands_run_as_the_original_user() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let fx = fixture(&mut setup).await;
    setup.commit().await.unwrap();

    let mut tx = h.begin_as(DEMO).await.unwrap();
    let group = "test_sudo.group";
    for (label, users) in [
        (
            "with_user(admin)",
            tx.env()
                .records("test_sudo.user")
                .unwrap()
                .with_user(fx.admin),
        ),
        (
            "sudo",
            tx.env().records("test_sudo.user").unwrap().sudo(true),
        ),
    ] {
        // CREATE: a user created with new group lines
        let result = users
            .create_values(
                &mut tx,
                &[map(&[
                    ("name", s("foo")),
                    ("group_ids", Value::list([cmd(0, 0, dict(&[]))])),
                ])],
            )
            .await;
        assert_denied_by(label, result, group, Operation::Create);
        let user = users.browse([fx.user]).unwrap();
        // UPDATE and DELETE of the linked group
        let result = user
            .write_values(
                &mut tx,
                &map(&[("group_ids", Value::list([cmd(1, fx.group, dict(&[]))]))]),
            )
            .await;
        assert_denied_by(label, result, group, Operation::Write);
        let result = user
            .write_values(
                &mut tx,
                &map(&[("group_ids", Value::list([cmd(2, fx.group, int(0))]))]),
            )
            .await;
        assert_denied_by(label, result, group, Operation::Unlink);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// behaviour, and the original user, when it is allowed, runs the same commands.
#[tokio::test]
async fn spec_rel_060_controls() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let fx = fixture(&mut setup).await;
    setup.commit().await.unwrap();

    // sudo() lifts the restriction on a comodel that allows sudo commands
    let mut tx = h.begin_as(DEMO).await.unwrap();
    let owner = tx
        .env()
        .records("test_sudo.owner")
        .unwrap()
        .sudo(true)
        .browse([fx.owner])
        .unwrap();
    owner
        .write_values(
            &mut tx,
            &map(&[(
                "free_ids",
                Value::list([cmd(0, 0, dict(&[("name", s("free"))]))]),
            )]),
        )
        .await
        .unwrap();
    let free = tx
        .env()
        .records("test_sudo.free")
        .unwrap()
        .sudo(true)
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(free.len(), 1);
    drop(tx);

    // the same commands succeed when the transaction's own user may do them
    let mut tx = h.begin_as(fx.admin).await.unwrap();
    let owner = tx
        .env()
        .records("test_sudo.owner")
        .unwrap()
        .sudo(true)
        .browse([fx.owner])
        .unwrap();
    owner
        .write_values(
            &mut tx,
            &map(&[(
                "line_ids",
                Value::list([cmd(1, fx.line, dict(&[("rank", int(9))]))]),
            )]),
        )
        .await
        .unwrap();
    let rank = tx
        .env()
        .records("test_sudo.line")
        .unwrap()
        .browse([fx.line])
        .unwrap()
        .value(&mut tx, "rank")
        .await
        .unwrap();
    assert_eq!(rank, int(9));
    drop(tx);
    h.cleanup().await.unwrap();
}
