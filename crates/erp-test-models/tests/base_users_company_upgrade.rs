//! Upgrading a database from base 1.9 to 1.10 (ADR
//! 0170 decision 7): every user gets a default company by a fixed rule, or the upgrade refuses.
//!
//! The old shape is made with raw SQL: users inserted the way a 1.9 database could hold them
//! (no default company, a default company outside the allowed companies). The 1.10 post-migration
//! body runs against them, as the module lifecycle runs it after the schema and data steps. This
//! is a simulation of old rows on the new schema, not a genuine old-release upgrade.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use erp_addon_base::migrations::assign_user_companies;
use erp_sdk::{ErrorKind, Transaction};
use users_support::{assert_error, harness};

async fn currency(tx: &mut Transaction) -> i64 {
    erp_testkit::fixtures::seed_currency_in(tx).await.unwrap()
}

async fn company(tx: &mut Transaction, name: &str, currency: i64, active: bool) -> i64 {
    tx.query(
        "INSERT INTO res_company (name, currency_id, active) VALUES ($1, $2, $3) \
         RETURNING id::int8",
        &[&name, &currency, &active],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// A user row as an old database could hold it: `default` is its `company_id`, `allowed` its
/// rows in `res_company_users_rel`.
async fn old_user(
    tx: &mut Transaction,
    login: &str,
    active: bool,
    default: Option<i64>,
    allowed: &[i64],
) -> i64 {
    let id: i64 = tx
        .query(
            "INSERT INTO res_users (name, login, active, share, company_id) \
             VALUES ($1, $1, $2, true, $3) RETURNING id::int8",
            &[&login, &active, &default],
        )
        .await
        .unwrap()[0]
        .get(0);
    for company in allowed {
        tx.execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
            &[&id, company],
        )
        .await
        .unwrap();
    }
    id
}

/// `(id, company_id)` of every user, superuser included.
async fn defaults(tx: &mut Transaction) -> Vec<(i64, Option<i64>)> {
    tx.query(
        "SELECT id::int8, company_id FROM res_users ORDER BY id",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1)))
    .collect()
}

/// `(user_id, cid)` of every membership row.
async fn memberships(tx: &mut Transaction) -> Vec<(i64, i64)> {
    tx.query(
        "SELECT user_id::int8, cid::int8 FROM res_company_users_rel ORDER BY user_id, cid",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1)))
    .collect()
}

fn of(rows: &[(i64, Option<i64>)], id: i64) -> Option<i64> {
    rows.iter().find(|r| r.0 == id).unwrap().1
}

fn refusal(ids: &str, companies: usize) -> String {
    format!(
        "base 1.10 refused: active users {ids} have no default company and none can be \
         chosen ({companies} companies). Set each user's default and allowed companies in \
         Settings > Users with the current release, then upgrade again."
    )
}

#[tokio::test]
async fn upgrade_fills_defaults_without_widening_memberships() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx).await;
    let first = company(&mut tx, "First", usd, true).await;
    let archived = company(&mut tx, "Archived", usd, false).await;
    let third = company(&mut tx, "Third", usd, true).await;
    // R1a: a default company that is not among the allowed ones: the user takes its lowest-id
    // active allowed company and gets no new row (a migration never widens access)
    let r1 = old_user(&mut tx, "r1", true, Some(third), &[first, archived]).await;
    // R1b: inactive users without an active allowed company stay unchanged
    let r1_archived = old_user(&mut tx, "r1_archived", false, Some(first), &[archived]).await;
    // R1b again: no allowed company at all
    let r1_inactive = old_user(&mut tx, "r1_inactive", false, Some(first), &[]).await;
    // R2: no default, allowed companies: the lowest-id active one among them
    let r2 = old_user(&mut tx, "r2", true, None, &[third, archived, first]).await;
    // R2 with an archived company only: left alone while the user is inactive
    let r2_inactive = old_user(&mut tx, "r2_inactive", false, None, &[archived]).await;
    // inactive without anything: left alone
    let nothing_inactive = old_user(&mut tx, "nothing_inactive", false, None, &[]).await;
    // already right: not touched
    let right = old_user(&mut tx, "right", true, Some(first), &[first, third]).await;
    let superuser = erp_sdk::SUPERUSER_ID.get();
    let before = defaults(&mut tx).await;
    assert_eq!(of(&before, superuser), None);
    assert_eq!(of(&before, r2), None);
    let rows_before = memberships(&mut tx).await;

    assign_user_companies(&mut tx).await.unwrap();

    let after = defaults(&mut tx).await;
    assert_eq!(of(&after, r1), Some(first));
    assert_eq!(of(&after, r1_archived), Some(first));
    assert_eq!(of(&after, r1_inactive), Some(first));
    assert_eq!(of(&after, r2), Some(first));
    assert_eq!(of(&after, r2_inactive), None);
    assert_eq!(of(&after, nothing_inactive), None);
    assert_eq!(of(&after, right), Some(first));
    assert_eq!(of(&after, superuser), None);
    let rows_after = memberships(&mut tx).await;
    assert_eq!(rows_after, rows_before);

    // the ORM reads the new values, not a cached old one
    let read = users_support::records(&tx, "res.users", &[r1, r2])
        .cells(&mut tx, "company_id")
        .await
        .unwrap();
    assert_eq!(read, [erp_sdk::Cell::Id(first), erp_sdk::Cell::Id(first)]);
    // a second run changes nothing
    assign_user_companies(&mut tx).await.unwrap();
    assert_eq!(defaults(&mut tx).await, after);
    assert_eq!(memberships(&mut tx).await, rows_after);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upgrade_gives_the_only_company_to_users_without_any() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx).await;
    let only = company(&mut tx, "Only", usd, true).await;
    let active = old_user(&mut tx, "active", true, None, &[]).await;
    let inactive = old_user(&mut tx, "inactive", false, None, &[]).await;
    let superuser = erp_sdk::SUPERUSER_ID.get();
    assert_eq!(of(&defaults(&mut tx).await, superuser), None);

    assign_user_companies(&mut tx).await.unwrap();

    let after = defaults(&mut tx).await;
    assert_eq!(of(&after, active), Some(only));
    assert_eq!(of(&after, inactive), Some(only));
    assert_eq!(of(&after, superuser), None);
    assert_eq!(
        memberships(&mut tx).await,
        [(active.min(inactive), only), (active.max(inactive), only)]
    );
    let rows = memberships(&mut tx).await;
    assign_user_companies(&mut tx).await.unwrap();
    assert_eq!(defaults(&mut tx).await, after);
    assert_eq!(memberships(&mut tx).await, rows);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upgrade_refuses_unassignable_active_users() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx).await;
    let first = company(&mut tx, "First", usd, true).await;
    let archived = company(&mut tx, "Archived", usd, false).await;
    // fillable users are in the same database: nothing of them is written when it refuses
    let fillable = old_user(&mut tx, "fillable", true, None, &[first]).await;
    let none = old_user(&mut tx, "none", true, None, &[]).await;
    let only_archived = old_user(&mut tx, "only_archived", true, None, &[archived]).await;
    // inactive users never refuse
    old_user(&mut tx, "inactive", false, None, &[]).await;
    let before = (defaults(&mut tx).await, memberships(&mut tx).await);

    let error = assign_user_companies(&mut tx).await.unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        &refusal(&format!("{none}, {only_archived}"), 2),
    );
    // nothing was written
    assert_eq!(defaults(&mut tx).await, before.0);
    assert_eq!(memberships(&mut tx).await, before.1);
    assert_eq!(of(&before.0, fillable), None);
    drop(tx);
    h.cleanup().await.unwrap();
}

fn r1b_refusal(ids: &str) -> String {
    format!(
        "base 1.10 refused: active users {ids} have a default company outside their allowed \
         companies and no active allowed company. Set each user's default and allowed companies \
         in Settings > Users with the current release, then upgrade again."
    )
}

#[tokio::test]
async fn upgrade_refuses_active_r1b_before_filling_other_users() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx).await;
    let first = company(&mut tx, "First", usd, true).await;
    let archived = company(&mut tx, "Archived", usd, false).await;
    let third = company(&mut tx, "Third", usd, true).await;
    let empty = old_user(&mut tx, "empty", true, Some(first), &[]).await;
    let archived_only = old_user(&mut tx, "archived_only", true, Some(first), &[archived]).await;
    old_user(&mut tx, "r1a", true, Some(third), &[first]).await;
    old_user(&mut tx, "r2", true, None, &[first]).await;
    let before = (defaults(&mut tx).await, memberships(&mut tx).await);

    let error = assign_user_companies(&mut tx).await.unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        &r1b_refusal(&format!("{empty}, {archived_only}")),
    );
    assert_eq!(defaults(&mut tx).await, before.0);
    assert_eq!(memberships(&mut tx).await, before.1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upgrade_refuses_active_r1b_with_the_only_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx).await;
    let only = company(&mut tx, "Only", usd, true).await;
    let user = old_user(&mut tx, "outside", true, Some(only), &[]).await;
    old_user(&mut tx, "r3a", true, None, &[]).await;
    let before = (defaults(&mut tx).await, memberships(&mut tx).await);

    let error = assign_user_companies(&mut tx).await.unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        &r1b_refusal(&user.to_string()),
    );
    assert_eq!(defaults(&mut tx).await, before.0);
    assert_eq!(memberships(&mut tx).await, before.1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upgrade_preserves_inactive_r1b_and_allowed_archived_defaults() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx).await;
    let first = company(&mut tx, "First", usd, true).await;
    let archived = company(&mut tx, "Archived", usd, false).await;
    old_user(&mut tx, "inactive_empty", false, Some(first), &[]).await;
    old_user(
        &mut tx,
        "inactive_archived",
        false,
        Some(first),
        &[archived],
    )
    .await;
    old_user(
        &mut tx,
        "allowed_archived",
        true,
        Some(archived),
        &[archived],
    )
    .await;
    let before = (defaults(&mut tx).await, memberships(&mut tx).await);

    for _ in 0..2 {
        assign_user_companies(&mut tx).await.unwrap();
        assert_eq!(defaults(&mut tx).await, before.0);
        assert_eq!(memberships(&mut tx).await, before.1);
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upgrade_bounds_r1b_refusal_ids() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = currency(&mut tx).await;
    let only = company(&mut tx, "Only", usd, true).await;
    let mut ids = Vec::new();
    for n in 0..51 {
        ids.push(old_user(&mut tx, &format!("outside_{n}"), true, Some(only), &[]).await);
    }
    let before = (defaults(&mut tx).await, memberships(&mut tx).await);
    let mut listed = ids[..50].iter().map(i64::to_string).collect::<Vec<_>>();
    listed.push("and 1 more".to_owned());
    let error = assign_user_companies(&mut tx).await.unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        &r1b_refusal(&listed.join(", ")),
    );
    assert_eq!(defaults(&mut tx).await, before.0);
    assert_eq!(memberships(&mut tx).await, before.1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[test]
fn assign_user_companies_is_base_1_10_post_step() {
    let steps: Vec<(&str, &str)> = erp_data::migration::MIGRATIONS
        .iter()
        .filter(|m| m.addon() == "base" && m.version() == "1.10")
        .map(|m| (m.stage().as_str(), m.name()))
        .collect();
    assert_eq!(steps, [("post", "assign_user_companies_base_1_10")]);
    let manifest = include_str!("../../../addons/base/manifest.toml");
    let version: Vec<u32> = manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = "))
        .unwrap()
        .trim_matches('"')
        .split('.')
        .map(|part| part.parse().unwrap())
        .collect();
    assert!(version >= vec![1, 10], "{version:?}");
}
