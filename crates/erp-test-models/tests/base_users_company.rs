//! the default company rule of `res.users` (
//! becomes the allowed company, an active user's default company is one of its allowed companies,
//! a default company cannot be deleted, and a user changes their own default within their
//! allowed companies.
//!
//! Every refusal runs in a transaction of its own: a refused write leaves the transaction
//! unusable, and nothing of it is committed. The databases hold the groups and the administrator
//! an installed base has (`users_support`) but none of base's data, so every company is made here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use std::sync::Arc;

use erp_sdk::{Cell, Env, Error, ErrorKind, Records, Transaction, Value};
use erp_testkit::{FieldPolicy, Harness};
use users_support::{
    assert_error, create, harness, ids_of, int, map, records, s, set_cmd, unlink_cmd,
    user_types_and_admin,
};

/// The text of the foreign-key refusal (`erp-orm` `dberror`), for the model and field named.
const IN_USE: &str = "The database refused the change: Other records still point to the record you want to delete.\n\nReferring model: 'User' (res.users)\nConstraint: 'Company' (company_id)\nArchive the record instead of deleting it.";

/// The refusal of an active user without a default company.
fn no_default(user: &str) -> String {
    format!("The user {user} has no default company. Choose one of the user's allowed companies.")
}

/// The refusal of a default company outside the allowed companies; `allowed` is already the
/// listed names, in id order, or `none`. The chosen company is not named.
fn not_allowed(user: &str, allowed: &str) -> String {
    format!(
        "The default company of the user {user} is not one of the user's allowed companies \
         ({allowed})."
    )
}

/// What a database that has `base` installed has, plus two companies and two committed users:
/// `dual` (default `a`, allowed `a` and `b`) and `solo` (default `a`, allowed `a`).
struct Fixture {
    a: i64,
    b: i64,
    dual: i64,
    solo: i64,
}

async fn company(tx: &mut Transaction, name: &str, currency: i64) -> i64 {
    create(
        tx,
        "res.company",
        &[("name", s(name)), ("currency_id", int(currency))],
    )
    .await
}

async fn fixture(h: &Harness) -> Fixture {
    let mut tx = h.begin().await.unwrap();
    user_types_and_admin(&mut tx).await;
    let usd = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    let a = company(&mut tx, "Alpha", usd).await;
    let b = company(&mut tx, "Beta", usd).await;
    let dual = create(
        &mut tx,
        "res.users",
        &[
            ("name", s("Dual")),
            ("login", s("dual")),
            ("company_id", int(a)),
            ("company_ids", set_cmd(&[a, b])),
        ],
    )
    .await;
    let solo = create(
        &mut tx,
        "res.users",
        &[
            ("name", s("Solo")),
            ("login", s("solo")),
            ("company_id", int(a)),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);
    Fixture { a, b, dual, solo }
}

/// Create one user in `env`, as the dynamic surface does.
async fn try_create(
    tx: &mut Transaction,
    env: &Env,
    name: &str,
    extra: &[(&str, Value)],
) -> Result<Records, Error> {
    let mut pairs = vec![("name", s(name)), ("login", s(name))];
    pairs.extend(extra.iter().cloned());
    env.records("res.users")
        .unwrap()
        .create_values(tx, &[map(&pairs)])
        .await
}

async fn default_company(tx: &mut Transaction, user: i64) -> Cell {
    records(tx, "res.users", &[user])
        .cell(tx, "company_id")
        .await
        .unwrap()
}

async fn allowed(tx: &mut Transaction, user: i64) -> Vec<i64> {
    let users = records(tx, "res.users", &[user]);
    ids_of(tx, &users, "company_ids").await
}

#[tokio::test]
async fn new_user_takes_the_environment_company() {
    let h = harness().await;
    let fx = fixture(&h).await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(fx.b).unwrap();
    let defaults = env
        .records("res.users")
        .unwrap()
        .default_get(&mut tx, &["company_id", "company_ids"])
        .await
        .unwrap();
    assert_eq!(defaults.get("company_id"), Some(&int(fx.b)));
    assert_eq!(defaults.get("company_ids"), Some(&set_cmd(&[fx.b])));

    let created = try_create(&mut tx, &env, "fresh", &[]).await.unwrap();
    let id = created.ids()[0];
    assert_eq!(default_company(&mut tx, id).await, Cell::Id(fx.b));
    assert_eq!(allowed(&mut tx, id).await, [fx.b]);
    // the other company is the default of the other environment
    let env = tx.env().with_company(fx.a).unwrap();
    let created = try_create(&mut tx, &env, "fresh_a", &[]).await.unwrap();
    assert_eq!(
        default_company(&mut tx, created.ids()[0]).await,
        Cell::Id(fx.a)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn system_default_is_the_only_company() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    user_types_and_admin(&mut setup).await;
    let usd = erp_testkit::fixtures::seed_currency_in(&mut setup)
        .await
        .unwrap();
    let only = company(&mut setup, "Only", usd).await;
    setup.commit().await.unwrap();
    drop(setup);

    // one active company, no company in the environment: it is the default
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    assert_eq!(env.company().unwrap(), None);
    let created = try_create(&mut tx, &env, "first", &[]).await.unwrap();
    let id = created.ids()[0];
    assert_eq!(default_company(&mut tx, id).await, Cell::Id(only));
    assert_eq!(allowed(&mut tx, id).await, [only]);
    tx.commit().await.unwrap();
    drop(tx);

    // a second company: nothing to choose from, the create is refused
    let mut setup = h.begin().await.unwrap();
    let usd: i64 = setup
        .query("SELECT id FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()[0]
        .get(0);
    company(&mut setup, "Second", usd).await;
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let error = try_create(&mut tx, &env, "second", &[]).await.unwrap_err();
    assert_error(&error, ErrorKind::Validation, &no_default("second"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_company_alone_becomes_allowed() {
    let h = harness().await;
    let fx = fixture(&h).await;
    let mut tx = h.begin().await.unwrap();
    // the environment's company is the other one: the given default wins, and is the allowed one
    let env = tx.env().with_company(fx.b).unwrap();
    let created = try_create(&mut tx, &env, "given", &[("company_id", int(fx.a))])
        .await
        .unwrap();
    let id = created.ids()[0];
    assert_eq!(default_company(&mut tx, id).await, Cell::Id(fx.a));
    assert_eq!(allowed(&mut tx, id).await, [fx.a]);
    // the fixture's `solo` was made the same way
    assert_eq!(allowed(&mut tx, fx.solo).await, [fx.a]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn active_user_company_must_be_allowed() {
    let h = harness().await;
    let fx = fixture(&h).await;

    // created without any company, in an environment that names none (two companies exist)
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let error = try_create(&mut tx, &env, "nobody", &[("company_id", Value::False)])
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, &no_default("nobody"));
    drop(tx);

    // created with a default outside the allowed companies
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let error = try_create(
        &mut tx,
        &env,
        "outsider",
        &[("company_id", int(fx.b)), ("company_ids", set_cmd(&[fx.a]))],
    )
    .await
    .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        &not_allowed("outsider", "Alpha"),
    );
    drop(tx);

    // the default company dropped from the allowed companies
    let mut tx = h.begin().await.unwrap();
    let error = records(&tx, "res.users", &[fx.dual])
        .write_values(
            &mut tx,
            &map(&[("company_ids", Value::list([unlink_cmd(fx.a)]))]),
        )
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, &not_allowed("Dual", "Beta"));
    drop(tx);

    // a default company the user is not allowed in
    let mut tx = h.begin().await.unwrap();
    let error = records(&tx, "res.users", &[fx.solo])
        .write_values(&mut tx, &map(&[("company_id", int(fx.b))]))
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, &not_allowed("Solo", "Alpha"));
    drop(tx);

    // a default inside the allowed companies is accepted
    let mut tx = h.begin().await.unwrap();
    records(&tx, "res.users", &[fx.dual])
        .write_values(&mut tx, &map(&[("company_id", int(fx.b))]))
        .await
        .unwrap();
    assert_eq!(default_company(&mut tx, fx.dual).await, Cell::Id(fx.b));
    drop(tx);

    // an inactive user needs no company; reactivating it is refused until it has one
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let ghost = try_create(
        &mut tx,
        &env,
        "ghost",
        &[("active", Value::Bool(false)), ("company_id", Value::False)],
    )
    .await
    .unwrap();
    let ghost = ghost.ids()[0];
    assert_eq!(default_company(&mut tx, ghost).await, Cell::Null);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let error = records(&tx, "res.users", &[ghost])
        .write_values(&mut tx, &map(&[("active", Value::Bool(true))]))
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, &no_default("ghost"));
    drop(tx);
    // given a company at the same time, the reactivation is accepted
    let mut tx = h.begin().await.unwrap();
    records(&tx, "res.users", &[ghost])
        .write_values(
            &mut tx,
            &map(&[
                ("active", Value::Bool(true)),
                ("company_id", int(fx.a)),
                ("company_ids", set_cmd(&[fx.a])),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(default_company(&mut tx, ghost).await, Cell::Id(fx.a));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn archived_default_company_keeps_its_own_message() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    user_types_and_admin(&mut tx).await;
    let usd = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    let kept = company(&mut tx, "Kept", usd).await;
    let gone = company(&mut tx, "Gone", usd).await;
    assert_ne!(kept, gone);
    let env = tx.env();
    let user = try_create(
        &mut tx,
        &env,
        "archived_home",
        &[("active", Value::Bool(false)), ("company_id", int(gone))],
    )
    .await
    .unwrap();
    records(&tx, "res.company", &[gone])
        .action_archive(&mut tx)
        .await
        .unwrap();
    // the archived company is still one of the user's allowed companies: the lifecycle message
    // is the refusal, not the membership one
    let error = user
        .write_values(&mut tx, &map(&[("active", Value::Bool(true))]))
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        "An active user must have an active default company.",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn no_company_database_is_exempt() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    user_types_and_admin(&mut tx).await;
    let count: i64 = tx
        .query("SELECT count(*) FROM res_company", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    let env = tx.env();
    let created = try_create(&mut tx, &env, "companyless", &[]).await.unwrap();
    assert_eq!(default_company(&mut tx, created.ids()[0]).await, Cell::Null);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A second node commits a change of the user's default company and allowed companies after this
/// transaction took its snapshot. The check of this transaction relies on the rows the other one
/// replaced, so it locks them and conflicts instead of passing on the old state.
#[tokio::test]
async fn concurrent_default_and_membership_change_conflict() {
    let h = harness().await;
    let fx = fixture(&h).await;
    let second = erp_orm::Orm::builder(Arc::clone(h.orm().registry()), h.db().pool().clone())
        .trust_caller_resolutions()
        .build()
        .unwrap();
    assert_ne!(h.orm().id(), second.id());

    let mut stale = h.begin().await.unwrap();
    // the snapshot of this transaction: `dual` has default `a`, allowed `a` and `b`
    stale
        .query("SELECT id FROM res_users WHERE id = $1", &[&fx.dual])
        .await
        .unwrap();
    let mut writer = second.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    records(&writer, "res.users", &[fx.dual])
        .write_values(
            &mut writer,
            &map(&[("company_id", int(fx.b)), ("company_ids", set_cmd(&[fx.b]))]),
        )
        .await
        .unwrap();
    writer.commit().await.unwrap();
    drop(writer);

    let users = records(&stale, "res.users", &[fx.dual]);
    let error = erp_addon_base::res_users::check_user_companies(&users, &mut stale)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency, "{error}");
    assert_eq!(error.sqlstate(), Some("40001"));
    stale.rollback().await.unwrap();
    drop(stale);

    // what the other node committed stands, and is consistent
    let mut check = h.begin().await.unwrap();
    assert_eq!(default_company(&mut check, fx.dual).await, Cell::Id(fx.b));
    assert_eq!(allowed(&mut check, fx.dual).await, [fx.b]);
    check.rollback().await.unwrap();
    drop(second);
    h.cleanup().await.unwrap();
}

/// A second node deletes only the membership row of the user's default company (a plain
/// statement on the relation table, no `res_users` column is written) after this transaction took
/// its snapshot. The user row is unchanged, so only the lock on the membership row can make the
/// check conflict instead of passing on the old state.
#[tokio::test]
async fn concurrent_membership_removal_alone_conflicts() {
    let h = harness().await;
    let fx = fixture(&h).await;
    let second = erp_orm::Orm::builder(Arc::clone(h.orm().registry()), h.db().pool().clone())
        .trust_caller_resolutions()
        .build()
        .unwrap();
    assert_ne!(h.orm().id(), second.id());

    let mut stale = h.begin().await.unwrap();
    // the snapshot of this transaction: `dual` has default `a`, allowed `a` and `b`
    stale
        .query("SELECT id FROM res_users WHERE id = $1", &[&fx.dual])
        .await
        .unwrap();
    let mut writer = second.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let removed = writer
        .execute(
            "DELETE FROM res_company_users_rel WHERE user_id = $1 AND cid = $2",
            &[&fx.dual, &fx.a],
        )
        .await
        .unwrap();
    assert_eq!(removed, 1);
    writer.commit().await.unwrap();
    drop(writer);

    let users = records(&stale, "res.users", &[fx.dual]);
    let error = erp_addon_base::res_users::check_user_companies(&users, &mut stale)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency, "{error}");
    assert_eq!(error.sqlstate(), Some("40001"));
    stale.rollback().await.unwrap();
    drop(stale);

    // what the other node committed stands: the default is unchanged, `b` alone is allowed
    let mut check = h.begin().await.unwrap();
    assert_eq!(default_company(&mut check, fx.dual).await, Cell::Id(fx.a));
    assert_eq!(allowed(&mut check, fx.dual).await, [fx.b]);
    check.rollback().await.unwrap();
    drop(second);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_company_cannot_be_deleted() {
    let h = harness().await;
    let fx = fixture(&h).await;
    let mut tx = h.begin().await.unwrap();
    let error = records(&tx, "res.company", &[fx.a])
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, IN_USE);
    drop(tx);
    // the company is still there
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query("SELECT count(*) FROM res_company WHERE id = $1", &[&fx.a])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The testkit's policy applies the field mask of `res.users` its own way; the whole case on the
/// compiled policy is in `base_users_policy.rs`.
#[tokio::test]
async fn user_changes_own_default_company_within_allowed() {
    let policy = FieldPolicy::new().with_overrides();
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .policy(policy.clone())
        .build()
        .await
        .expect("harness");
    policy.attach(h.registry()).unwrap();
    let mut tx = h.begin().await.unwrap();
    user_types_and_admin(&mut tx).await;
    let usd = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    let a = company(&mut tx, "Alpha", usd).await;
    let b = company(&mut tx, "Beta", usd).await;
    let c = company(&mut tx, "Gamma", usd).await;
    let me = create(
        &mut tx,
        "res.users",
        &[
            ("name", s("Me")),
            ("login", s("me")),
            ("company_id", int(a)),
            ("company_ids", set_cmd(&[a, b])),
        ],
    )
    .await;
    policy.join(me, "base.group_user");
    tx.commit().await.unwrap();
    drop(tx);

    // inside the allowed companies
    let mut tx = h.begin().await.unwrap();
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .with_user(me)
        .browse([me])
        .unwrap();
    own.write_values(&mut tx, &map(&[("company_id", int(b))]))
        .await
        .unwrap();
    assert_eq!(own.cell(&mut tx, "company_id").await.unwrap(), Cell::Id(b));
    tx.commit().await.unwrap();
    drop(tx);

    // the allowed companies stay with the administrators
    let mut tx = h.begin().await.unwrap();
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .with_user(me)
        .browse([me])
        .unwrap();
    let error = own
        .write_values(&mut tx, &map(&[("company_ids", set_cmd(&[a, b, c]))]))
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Access,
        "You do not have enough rights to access the field \"company_ids\" on User \
         (res.users). Please contact your system administrator.\n\nOperation: write",
    );
    drop(tx);

    // outside them, in a fresh transaction after the refusal
    let mut tx = h.begin().await.unwrap();
    let own = tx
        .env()
        .records("res.users")
        .unwrap()
        .with_user(me)
        .browse([me])
        .unwrap();
    let error = own
        .write_values(&mut tx, &map(&[("company_id", int(c))]))
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        &not_allowed("Me", "Alpha, Beta"),
    );
    assert!(!error.to_string().contains("Gamma"), "{error}");
    drop(tx);
    h.cleanup().await.unwrap();
}
