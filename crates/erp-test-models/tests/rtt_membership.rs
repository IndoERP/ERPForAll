//! the `membership` signal generation that lets the replica lag guard see a changed
//! group, company or user flag. This is a separate test binary
//! because it links `addons/base`, whose rules every harness of a binary would otherwise inherit.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use std::sync::Arc;

use erp_orm::{Orm, Signal};

/// A refresh hook that applies registry and policy bumps in place.
struct InPlace;

impl erp_orm::Refresh for InPlace {
    fn refresh<'a>(
        &'a self,
        _changes: &'a erp_orm::Changes,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::Refreshed>> {
        Box::pin(async { Ok(erp_orm::Refreshed::InPlace) })
    }
}

/// The tables the resolver reads bump the `membership` generation: group links, company links,
/// the user's `active`, `share` and `company_id`, and a deleted company; other columns do not.
#[tokio::test]
async fn the_tables_the_resolver_reads_bump_the_membership_generation() {
    let h = users_support::harness().await;
    let client = h.db().client().await.unwrap();
    let membership = || async {
        let rows = client
            .query(
                "SELECT COALESCE(sum(weight), 0)::int8 FROM erp_signal_generation \
                 WHERE key = 'membership'",
                &[],
            )
            .await
            .unwrap();
        rows[0].get::<_, i64>(0)
    };
    let currency = erp_testkit::fixtures::seed_currency(&client).await.unwrap();
    client
        .execute(
            "INSERT INTO res_company (id, name, currency_id) VALUES (10, 'c1', $1), (11, 'c2', $1)",
            &[&currency],
        )
        .await
        .unwrap();
    client
        .batch_execute(
            "INSERT INTO res_groups (id, name) VALUES (10, 'g'); \
             INSERT INTO res_users (id, name, login, active, share) \
             VALUES (10, 'u', 'u', true, false)",
        )
        .await
        .unwrap();
    let mut seen = membership().await;
    let steps = [
        (
            "INSERT INTO res_groups_users_rel (gid, uid) VALUES (10, 10)",
            true,
        ),
        ("DELETE FROM res_groups_users_rel WHERE uid = 10", true),
        (
            "INSERT INTO res_company_users_rel (cid, user_id) VALUES (10, 10)",
            true,
        ),
        ("DELETE FROM res_company_users_rel WHERE user_id = 10", true),
        ("UPDATE res_users SET active = false WHERE id = 10", true),
        ("UPDATE res_users SET share = true WHERE id = 10", true),
        ("UPDATE res_users SET company_id = 10 WHERE id = 10", true),
        ("DELETE FROM res_company WHERE id = 11", true),
        (
            "UPDATE res_users SET name = 'renamed', login = 'renamed' WHERE id = 10",
            false,
        ),
    ];
    for (sql, bumps) in steps {
        client.batch_execute(sql).await.unwrap();
        let now = membership().await;
        assert_eq!(now > seen, bumps, "{sql}");
        seen = now;
    }
    h.cleanup().await.unwrap();
}

/// The signal table as an older version made it: its `key` check does not know `membership`.
const OLD_KEY_CHECK: &str = "ALTER TABLE erp_signal_generation \
     DROP CONSTRAINT erp_signal_generation_key_check; \
     ALTER TABLE erp_signal_generation ADD CONSTRAINT erp_signal_generation_key_check \
     CHECK (key ~ '^(registry|policy|cache:[a-z0-9_.]{1,64})$')";

/// What a membership-firing write does (a statement trigger fires for zero rows too).
const MEMBERSHIP_WRITE: &str = "UPDATE res_users SET active = active WHERE false";

async fn key_check(h: &erp_testkit::Harness) -> (i64, String, bool) {
    let client = h.db().client().await.unwrap();
    let row = client
        .query_one(
            "SELECT oid::int8, pg_get_constraintdef(oid), convalidated FROM pg_constraint \
             WHERE conrelid = 'erp_signal_generation'::regclass \
               AND conname = 'erp_signal_generation_key_check'",
            &[],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1), row.get(2))
}

async fn signal_rows(h: &erp_testkit::Harness) -> Vec<(String, String, i64)> {
    let client = h.db().client().await.unwrap();
    client
        .query(
            "SELECT id::text, key, weight FROM erp_signal_generation ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

/// SEC2-1: a database whose signal table was made before the `membership` key existed keeps its
/// old `CHECK`; schema sync replaces it before the membership triggers can fire, rows and
/// generations are kept, and a second sync changes nothing.
#[tokio::test]
async fn schema_sync_upgrades_the_signal_table_of_an_existing_database() {
    let h = users_support::harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    let baseline = tx.signal_generations().await.unwrap();
    tx.commit().await.unwrap();
    let orm = Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .signal(Signal::new(baseline).with_refresh(InPlace))
        .build()
        .unwrap();
    let client = h.db().client().await.unwrap();
    // generations other than membership, so there is something to keep
    client
        .batch_execute(
            "INSERT INTO erp_signal_generation (key) VALUES ('policy'), ('registry'), ('policy')",
        )
        .await
        .unwrap();
    client
        .batch_execute("DELETE FROM erp_signal_generation WHERE key = 'membership'")
        .await
        .unwrap();
    client.batch_execute(OLD_KEY_CHECK).await.unwrap();
    let before = signal_rows(&h).await;
    assert!(before.len() >= 2, "{before:?}");

    // the old shape breaks every write a membership trigger fires on
    let error = client.batch_execute(MEMBERSHIP_WRITE).await.unwrap_err();
    assert_eq!(
        error.as_db_error().map(|e| e.code().code()),
        Some("23514"),
        "{error}"
    );
    assert!(!key_check(&h).await.1.contains("membership"));

    let report = orm.sync_schema().await.unwrap();
    assert!(report.is_empty(), "{report:?}");
    let (oid, definition, validated) = key_check(&h).await;
    assert!(definition.contains("membership"), "{definition}");
    assert!(validated, "the new check was validated");
    client.batch_execute(MEMBERSHIP_WRITE).await.unwrap();
    let after = signal_rows(&h).await;
    for row in &before {
        assert!(after.contains(row), "{row:?} was lost: {after:?}");
    }
    assert!(
        after.iter().any(|(_, key, _)| key == "membership"),
        "the write bumped the membership generation: {after:?}"
    );

    // a second sync finds the current shape and sends no DDL for it
    let report = orm.sync_schema().await.unwrap();
    assert!(report.is_empty(), "{report:?}");
    assert_eq!(key_check(&h).await, (oid, definition, true));
    h.cleanup().await.unwrap();
}

/// SEC2-1(a): a sync from an `Orm` WITHOUT a signal also brings an old signal table up to date,
/// so a membership-firing write succeeds afterwards.
#[tokio::test]
async fn sync_without_a_signal_upgrades_the_old_signal_table() {
    let h = users_support::harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(
            "DELETE FROM erp_signal_generation WHERE key = 'membership'; \
             INSERT INTO erp_signal_generation (key) VALUES ('policy'), ('registry')",
        )
        .await
        .unwrap();
    client.batch_execute(OLD_KEY_CHECK).await.unwrap();
    let before = signal_rows(&h).await;
    let error = client.batch_execute(MEMBERSHIP_WRITE).await.unwrap_err();
    assert_eq!(error.as_db_error().map(|e| e.code().code()), Some("23514"));

    let orm = Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .build()
        .unwrap();
    let _ = orm.sync_schema().await.unwrap();
    assert!(key_check(&h).await.1.contains("membership"));
    client.batch_execute(MEMBERSHIP_WRITE).await.unwrap();
    let after = signal_rows(&h).await;
    for row in &before {
        assert!(after.contains(row), "{row:?} was lost: {after:?}");
    }
    h.cleanup().await.unwrap();
}

/// SEC2-1(c): the old shape is told by the constraint's definition, not by a word: a check with
/// another older pattern is upgraded, and two stale key checks are both replaced by one current
/// check (no name collision).
#[tokio::test]
async fn other_older_patterns_and_two_stale_checks_are_upgraded() {
    let h = users_support::harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(
            "DELETE FROM erp_signal_generation WHERE key = 'membership'; \
             ALTER TABLE erp_signal_generation DROP CONSTRAINT erp_signal_generation_key_check; \
             ALTER TABLE erp_signal_generation ADD CONSTRAINT erp_signal_generation_key_check \
             CHECK (key ~ '^(registry|policy)$'); \
             ALTER TABLE erp_signal_generation ADD CONSTRAINT older_key_check \
             CHECK (key <> 'membership-not')",
        )
        .await
        .unwrap();
    let orm = Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .build()
        .unwrap();
    let _ = orm.sync_schema().await.unwrap();
    let rows = client
        .query(
            "SELECT conname::text, pg_get_constraintdef(oid), convalidated FROM pg_constraint \
             WHERE conrelid = 'erp_signal_generation'::regclass AND contype = 'c' \
             AND conname LIKE '%key%' ORDER BY conname",
            &[],
        )
        .await
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(names, ["erp_signal_generation_key_check"], "{names:?}");
    assert!(rows[0].get::<_, String>(1).contains("membership"));
    assert!(rows[0].get::<_, bool>(2));
    client.batch_execute(MEMBERSHIP_WRITE).await.unwrap();
    let after_first = key_check(&h).await;
    let report = orm.sync_schema().await.unwrap();
    assert!(report.is_empty(), "{report:?}");
    assert_eq!(
        key_check(&h).await,
        after_first,
        "a second sync leaves the same constraint (same oid) in place"
    );
    h.cleanup().await.unwrap();
}

/// SEC3-2: a check counts as current only when it is exactly the current rendering; a hand-made
/// `NOT (key ~ '<pattern>')` mentions the pattern and is still replaced.
#[tokio::test]
async fn a_negated_check_that_mentions_the_pattern_is_upgraded() {
    let h = users_support::harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let client = h.db().client().await.unwrap();
    let (_, current, _) = key_check(&h).await;
    let negated = current.replacen("CHECK ((", "CHECK (NOT (", 1);
    assert_ne!(negated, current, "{current}");
    client
        .batch_execute(&format!(
            "DELETE FROM erp_signal_generation; \
             ALTER TABLE erp_signal_generation DROP CONSTRAINT erp_signal_generation_key_check; \
             ALTER TABLE erp_signal_generation ADD CONSTRAINT erp_signal_generation_key_check \
             {negated}"
        ))
        .await
        .unwrap();
    let (old_oid, definition, _) = key_check(&h).await;
    assert!(definition.contains("NOT (key ~"), "{definition}");
    let orm = Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .build()
        .unwrap();
    let _ = orm.sync_schema().await.unwrap();
    let (oid, definition, validated) = key_check(&h).await;
    assert_ne!(oid, old_oid, "the negated check was replaced");
    assert_eq!(definition, current);
    assert!(validated);
    client.batch_execute(MEMBERSHIP_WRITE).await.unwrap();
    h.cleanup().await.unwrap();
}
