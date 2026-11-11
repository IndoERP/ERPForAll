//! review round 1 (lead decision 12; database review H4, M2, M3 and lows): the
//! authentication tables' DDL (indexes, the pending expiry check, the foreign key, storage
//! parameters, no `PUBLIC` privilege, idempotence) and the collection run (expired rows through
//! the expiry index, the idle sweep by heap page ranges, the advisory lock, throttle batches,
//! the login log kept at the newest row per user).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

use auth_support::*;
use erp_sdk::ErrorKind;
use erp_security::auth::gc::{
    EXPIRED_SQL, GC_LOCK, GcCursor, GcPhase, IDLE_SQL, ROW_BATCH, SWEEP_PAGES, THROTTLE_SQL,
    TRUSTED_SQL, USER_LOG_SQL,
};
use erp_security::auth::schema::{AUTH_AFTER_MODELS_DDL, AUTH_DDL};
use std::sync::Arc;

use erp_testkit::Harness;

async fn sql(h: &Harness, statement: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(statement).await.unwrap();
    tx.commit().await.unwrap();
}

async fn one_bool(
    h: &Harness,
    query: &str,
    params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)],
) -> bool {
    let mut tx = h.begin().await.unwrap();
    let rows = tx.query(query, params).await.unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

/// The plan of `statement` as JSON (`EXPLAIN (FORMAT JSON)`, not run).
async fn plan(
    h: &Harness,
    statement: &str,
    params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)],
) -> serde_json::Value {
    explain(h, "FORMAT JSON", statement, params).await
}

/// `EXPLAIN (<options>) <statement>` as JSON, in a transaction rolled back after (with ANALYZE the
/// statement runs, and its effects go with the rollback).
async fn explain(
    h: &Harness,
    options: &str,
    statement: &str,
    params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)],
) -> serde_json::Value {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(&format!("EXPLAIN ({options}) {statement}"), params)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

/// Every node of a JSON plan, depth first.
fn nodes(plan: &serde_json::Value) -> Vec<&serde_json::Value> {
    fn walk<'a>(node: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        out.push(node);
        if let Some(children) = node.get("Plans").and_then(serde_json::Value::as_array) {
            for child in children {
                walk(child, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&plan[0]["Plan"], &mut out);
    out
}

/// The scan under the plan's first `Limit` (the rows a batch picks), as `(node type, index
/// name)`.
fn limited_scan(plan: &serde_json::Value) -> (String, Option<String>) {
    let all = nodes(plan);
    let mut node = *all
        .iter()
        .find(|n| n["Node Type"] == "Limit")
        .unwrap_or_else(|| panic!("no Limit: {plan}"));
    while node.get("Relation Name").is_none() {
        node = &node["Plans"][0];
        assert!(!node.is_null(), "no scan under the Limit: {plan}");
    }
    (
        node["Node Type"].as_str().unwrap().to_owned(),
        node["Index Name"].as_str().map(str::to_owned),
    )
}

/// `n` active sessions of `user` expiring in a day, seen now.
async fn sessions(h: &Harness, user: i64, n: i64, tag: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at)
         SELECT gen_random_uuid(), $1::int8, sha256(($3::text || 's' || i)::bytea),
                sha256(($3::text || 't' || i)::bytea), 1, 'active', now() + interval '1 day'
           FROM generate_series(1, $2::int8) i",
        &[&user, &n, &tag],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// DB7-1: the first committed idle batch changes policy. The next destructive batch must
/// read that new policy; a limit captured once at run entry would delete the final row.
#[tokio::test]
async fn gc_refreshes_idle_policy_between_committed_batches() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "sessions.max_inactivity_seconds", "3600").await;
    let batch = i64::from(erp_security::auth::store::GC_BATCH);
    sessions(&w.h, bob, batch + 1, "policy-between-batches").await;
    sql(
        &w.h,
        "UPDATE erp_session SET last_seen_at = now() - interval '2 hours';
        CREATE FUNCTION test_raise_idle_policy() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
          UPDATE ir_config_parameter SET value = '86400'
           WHERE key = 'sessions.max_inactivity_seconds' AND value = '3600';
          RETURN OLD;
        END $$;
        CREATE TRIGGER test_raise_idle_policy AFTER DELETE ON erp_session
        FOR EACH ROW EXECUTE FUNCTION test_raise_idle_policy();",
    )
    .await;
    let report = w.auth.gc_from(GcCursor::START).await.unwrap();
    assert_eq!(report.idle_sessions, batch as u64);
    assert_eq!(count(&w.h, "erp_session").await, 1);
}

/// Database review low: the unique indexes the login and the administrator check rely on.
#[tokio::test]
async fn login_and_membership_have_unique_indexes() {
    let w = world().await;
    let unique_on = "SELECT EXISTS (
        SELECT 1 FROM pg_index i JOIN pg_class t ON t.oid = i.indrelid
         WHERE t.relname = $1::text AND i.indisunique
           AND (SELECT array_agg(a.attname::text ORDER BY a.attname)
                  FROM unnest(i.indkey) k (attnum)
                  JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum)
               = $2::text[])";
    let login = vec!["login".to_owned()];
    assert!(one_bool(&w.h, unique_on, &[&"res_users", &login]).await);
    let membership = vec!["gid".to_owned(), "uid".to_owned()];
    assert!(one_bool(&w.h, unique_on, &[&"res_groups_users_rel", &membership]).await);
}

/// Lead decision 12: the indexes, the pending check, the cascading foreign key, the storage
/// parameters and no privilege for `PUBLIC`.
#[tokio::test]
async fn auth_tables_have_their_indexes_keys_and_parameters() {
    let w = world().await;
    for index in [
        "erp_session_expires",
        "erp_login_throttle_counted_age",
        "erp_login_throttle_trusted_age",
    ] {
        assert!(
            one_bool(&w.h, "SELECT to_regclass($1::text) IS NOT NULL", &[&index]).await,
            "{index}"
        );
    }
    assert!(
        one_bool(
            &w.h,
            "SELECT EXISTS (SELECT 1 FROM pg_constraint
                 WHERE conname = 'erp_session_user_id_fkey' AND contype = 'f'
                   AND confdeltype = 'c' AND confrelid = 'res_users'::regclass AND convalidated)",
            &[],
        )
        .await
    );
    assert!(
        one_bool(
            &w.h,
            "SELECT EXISTS (SELECT 1 FROM pg_constraint
                 WHERE conname = 'erp_session_pending_expiry' AND contype = 'c')",
            &[],
        )
        .await
    );
    for (table, option) in [
        ("erp_session", "fillfactor=85"),
        ("erp_login_throttle", "fillfactor=80"),
    ] {
        assert!(
            one_bool(
                &w.h,
                "SELECT $2::text = ANY(reloptions) FROM pg_class WHERE oid = $1::text::regclass",
                &[&table, &option],
            )
            .await,
            "{table} {option}"
        );
        assert!(
            one_bool(
                &w.h,
                "SELECT NOT EXISTS (SELECT 1 FROM pg_class c,
                        aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
                  WHERE c.oid = $1::text::regclass AND a.grantee = 0)",
                &[&table],
            )
            .await,
            "{table}: PUBLIC holds a privilege"
        );
        assert!(
            one_bool(
                &w.h,
                "SELECT relpersistence = 'p' FROM pg_class WHERE oid = $1::text::regclass",
                &[&table],
            )
            .await,
            "{table} stays LOGGED"
        );
    }
}

/// The DDL converges: running it again on a database that has it changes nothing and fails
/// nothing.
#[tokio::test]
async fn auth_ddl_is_idempotent() {
    let w = world().await;
    sql(&w.h, AUTH_DDL).await;
    sql(&w.h, AUTH_AFTER_MODELS_DDL).await;
    sql(&w.h, AUTH_DDL).await;
    sql(&w.h, AUTH_AFTER_MODELS_DDL).await;
    assert_eq!(
        count(
            &w.h,
            "pg_constraint WHERE conrelid = 'erp_session'::regclass AND contype = 'f'"
        )
        .await,
        1
    );
}

/// A pending row must expire within the pending lifetime, so the expiry index collects it.
#[tokio::test]
async fn pending_rows_carry_their_expiry() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let insert = "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, \
                  expires_at) VALUES (gen_random_uuid(), $1, sha256('p'::bytea), \
                  sha256('q'::bytea), 1, 'pending', now() + make_interval(secs => $2))";
    let mut tx = w.h.begin().await.unwrap();
    let e = tx.execute(insert, &[&bob, &601.0f64]).await.unwrap_err();
    assert_eq!(
        (e.kind(), e.sqlstate()),
        (ErrorKind::Validation, Some("23514")),
        "{e}"
    );
    assert!(e.message().contains("erp_session_pending_expiry"), "{e}");
    tx.rollback().await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(insert, &[&bob, &600.0f64]).await.unwrap();
    tx.rollback().await.unwrap();
}

/// The foreign key cascades for a deletion outside the ORM.
#[tokio::test]
async fn deleting_a_user_by_sql_cascades_to_its_sessions() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    login(&w.auth, "bob", "pw").await.unwrap();
    assert_eq!(
        count(&w.h, &format!("erp_session WHERE user_id = {bob}")).await,
        1
    );
    sql(
        &w.h,
        &format!(
            "DELETE FROM res_groups_users_rel WHERE uid = {bob}; \
             DELETE FROM res_users_log WHERE create_uid = {bob}; \
             DELETE FROM res_users WHERE id = {bob}"
        ),
    )
    .await;
    assert_eq!(
        count(&w.h, &format!("erp_session WHERE user_id = {bob}")).await,
        0
    );
}

/// Database review H4: expired rows are found through `erp_session_expires` (the plan node, not
/// a timing), the idle sweep through a TID range scan, and the run collects them.
#[tokio::test]
async fn gc_reads_expired_rows_through_the_expiry_index() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    sessions(&w.h, bob, 20_000, "live").await;
    sessions(&w.h, bob, 5, "old").await;
    sql(
        &w.h,
        "UPDATE erp_session SET expires_at = now() - interval '1 second' \
          WHERE sid_digest IN (SELECT sha256(('old' || 's' || i)::bytea) \
                                 FROM generate_series(1, 5) i); \
         ANALYZE erp_session",
    )
    .await;
    let expired = plan(&w.h, EXPIRED_SQL, &[&1_000i64]).await;
    assert_eq!(
        limited_scan(&expired),
        (
            "Index Scan".to_owned(),
            Some("erp_session_expires".to_owned())
        ),
        "{expired}"
    );
    let idle = plan(
        &w.h,
        IDLE_SQL,
        &[&0i64, &0i32, &4_096i64, &60.0f64, &1_000i64],
    )
    .await;
    assert_eq!(
        limited_scan(&idle),
        ("Tid Range Scan".to_owned(), None),
        "{idle}"
    );
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.expired_sessions, 5);
    assert_eq!(report.idle_sessions, 0);
    assert_eq!(report.sessions(), 5);
    assert!(report.stopped_by_lock.is_none() && !report.capped);
    assert_eq!(report.next, GcCursor::START, "the sweep reached the end");
    assert_eq!(count(&w.h, "erp_session").await, 20_000);
}

/// The idle sweep deletes rows idle past the limit wherever they are in the heap, and a cursor
/// past the end starts over.
#[tokio::test]
async fn gc_sweeps_idle_rows() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "sessions.max_inactivity_seconds", "60").await;
    sessions(&w.h, bob, 3_000, "a").await;
    sql(
        &w.h,
        "UPDATE erp_session SET last_seen_at = now() - interval '61 seconds' \
          WHERE sid_digest IN (SELECT sha256(('a' || 's' || i)::bytea) \
                                 FROM generate_series(1, 3000, 2) i)",
    )
    .await;
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.idle_sessions, 1_500);
    assert_eq!(report.expired_sessions, 0);
    assert_eq!(count(&w.h, "erp_session").await, 1_500);
    assert_eq!(report.next, GcCursor::START, "the sweep reached the end");
    let again = w.auth.gc_from(report.next).await.unwrap();
    assert_eq!(again.sessions(), 0);
}

/// Lead decision 12: while another node holds the collection lock, a run does nothing and says
/// so; once the lock is free it collects.
#[tokio::test]
async fn gc_skips_while_another_node_collects() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    sessions(&w.h, bob, 3, "x").await;
    sql(
        &w.h,
        "UPDATE erp_session SET expires_at = now() - interval '1 second'",
    )
    .await;
    // the other node: a transaction holding the lock, named here by the project's lock class
    // (database review round 2 N1: the two-key form, not a hash of a name)
    assert_eq!(GC_LOCK.0, erp_orm::node::LOCK_CLASS);
    for taken in [
        erp_modules::system::OPERATION_LOCK,
        erp_modules::system::POST_COMMIT_LOCK,
        erp_orm::node::READINESS_LOCK,
    ] {
        assert_ne!(GC_LOCK, taken, "a second key of the class is reused");
    }
    let mut holder = w.h.begin().await.unwrap();
    holder
        .query(
            "SELECT pg_advisory_xact_lock($1::int4, $2::int4)",
            &[&erp_orm::node::LOCK_CLASS, &GC_LOCK.1],
        )
        .await
        .unwrap();
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.stopped_by_lock, Some(GcPhase::Expired), "{report:?}");
    assert_eq!(report.sessions(), 0);
    assert_eq!(count(&w.h, "erp_session").await, 3);
    holder.rollback().await.unwrap();
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.stopped_by_lock, None, "{report:?}");
    assert_eq!(report.expired_sessions, 3);
}

/// Database review M3: old throttle rows go in batches of `ROW_BATCH` until none is left;
/// recent rows stay.
#[tokio::test]
async fn gc_deletes_throttle_rows_in_batches() {
    let w = world().await;
    let old = i64::from(ROW_BATCH) * 2 + 7;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at)
         SELECT 'login', sha256(('k' || i)::bytea), 1, now() - interval '1 day'
           FROM generate_series(1, $1::int8) i",
        &[&old],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at)
         VALUES ('address', sha256('fresh'::bytea), 1, now())",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.throttle_rows, old.unsigned_abs());
    assert_eq!(count(&w.h, "erp_login_throttle").await, 1);
}

#[tokio::test]
async fn gc_keeps_the_newest_login_row_per_user() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    for _ in 0..3 {
        login(&w.auth, "bob", "pw").await.unwrap();
    }
    let admin = w.admin;
    sql(
        &w.h,
        &format!(
            "INSERT INTO res_users_log (create_uid, create_date, write_uid, write_date)
             SELECT {admin}, now() AT TIME ZONE 'UTC' - make_interval(days => i), {admin},
                    now() AT TIME ZONE 'UTC'
               FROM generate_series(1, 4) i"
        ),
    )
    .await;
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.user_logs, 2 + 3);
    assert_eq!(
        count(&w.h, &format!("res_users_log WHERE create_uid = {bob}")).await,
        1
    );
    assert_eq!(
        count(&w.h, &format!("res_users_log WHERE create_uid = {admin}")).await,
        1
    );
    assert!(
        one_bool(
            &w.h,
            "SELECT (SELECT create_date FROM res_users_log WHERE create_uid = $1) \
                  > now() AT TIME ZONE 'UTC' - interval '1 day 1 hour'",
            &[&admin],
        )
        .await,
        "the newest admin row (one day old) stayed"
    );
}

/// Review of R1B (silent 4): a failing batch returns the error with what the run committed
/// before it; the session-end hooks run in the READ COMMITTED batch (raw statements).
#[tokio::test]
async fn a_failing_batch_returns_the_partial_report() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_auth::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");
    let w = world_with(
        h,
        erp_security::auth::AuthConfig::DEFAULT,
        erp_security::auth::HashPoolConfig::DEFAULT,
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    let id = ok.session().id();
    let mut tx = w.h.begin().await.unwrap();
    create(
        &mut tx,
        "test_auth.device",
        &[("session_id", s(&id.to_text())), ("name", s("refuse"))],
    )
    .await;
    tx.commit().await.unwrap();
    sql(
        &w.h,
        "UPDATE erp_session SET expires_at = now() - interval '1 second'",
    )
    .await;
    let e = w.auth.gc().await.unwrap_err();
    assert_eq!(
        (e.error().kind(), e.error().message()),
        (
            ErrorKind::Validation,
            "test_auth refuses to end this session"
        )
    );
    assert_eq!(e.report().sessions(), 0);
    assert_eq!(e.report().next, GcCursor::START);
    assert_eq!(count(&w.h, "erp_session").await, 1, "the batch rolled back");
}

/// Types-delta S1: session-end hooks use raw statements only. A hook that reads through the
/// record API makes the collection batch fail loudly (the READ COMMITTED refusal, in the
/// `GcError`), and the session stays: nothing is skipped silently.
#[tokio::test]
async fn a_hook_using_the_record_api_fails_the_collection() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_auth::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");
    let w = world_with(
        h,
        erp_security::auth::AuthConfig::DEFAULT,
        erp_security::auth::HashPoolConfig::DEFAULT,
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    let id = ok.session().id();
    let mut tx = w.h.begin().await.unwrap();
    create(
        &mut tx,
        "test_auth.device",
        &[("session_id", s(&id.to_text())), ("name", s("records"))],
    )
    .await;
    tx.commit().await.unwrap();
    sql(
        &w.h,
        "UPDATE erp_session SET expires_at = now() - interval '1 second'",
    )
    .await;
    let mut tx = w.h.begin().await.unwrap();
    let expected = format!(
        "{}: the record API needs a REPEATABLE READ transaction; this one was opened with \
         BeginOptions::isolation(ReadCommitted), which allows raw statements only (query, \
         execute, batch)",
        tx.env().records("test_auth.device").unwrap()
    );
    tx.rollback().await.unwrap();
    let e = w.auth.gc().await.unwrap_err();
    assert_eq!(
        (e.error().kind(), e.error().message()),
        (ErrorKind::Programming, expected.as_str())
    );
    assert_eq!(e.report().sessions(), 0);
    assert_eq!(count(&w.h, "erp_session").await, 1, "the batch rolled back");
}

/// Review of R1B (types 6): a persisted cursor restores the sweep; one past the end of the table
/// starts it over.
#[tokio::test]
async fn a_restored_cursor_past_the_end_starts_over() {
    let w = world().await;
    assert_eq!(GcCursor::at(7).page(), 7);
    let report = w.auth.gc_from(GcCursor::at(u32::MAX)).await.unwrap();
    assert_eq!(report.next, GcCursor::START);
    assert_eq!(report.sessions(), 0);
}

/// Lead ruling (b): the authentication service refuses an ORM built without requiring its core
/// tables, naming the first one.
#[tokio::test]
async fn check_orm_refuses_an_orm_without_the_auth_tables() {
    let w = world().await;
    erp_security::auth::check_orm(w.h.orm()).unwrap();
    let bare = erp_orm::Orm::builder(Arc::clone(w.h.registry()), w.h.db().pool().clone())
        .build()
        .unwrap();
    let e = erp_security::auth::check_orm(&bare).unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Programming,
            "the ORM was built without requiring the core table 'erp_session' of \
             'erp-security': build it with \
             OrmBuilder::require_core_tables(erp_security::auth::core_tables())"
        )
    );
}

/// R1A's trusted pairs (claude2's item): a trusted row younger than 30 days survives
/// collection although it is older than twice the cooldown; an older one goes, in its own phase;
/// the address/login cooldown collection never deletes trusted rows.
#[tokio::test]
async fn gc_keeps_trusted_pairs_for_thirty_days() {
    let w = world().await;
    sql(
        &w.h,
        "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at) VALUES
           ('trusted', sha256('young'::bytea), 1, now() - interval '20 days'),
           ('trusted', sha256('old'::bytea), 1, now() - interval '31 days'),
           ('login', sha256('stale'::bytea), 1, now() - interval '20 days')",
    )
    .await;
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.throttle_rows, 1, "the stale login counter");
    assert_eq!(report.trusted_rows, 1, "the 31-day-old pair");
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'trusted' AND key_hash = sha256('young'::bytea)"
        )
        .await,
        1
    );
    assert_eq!(count(&w.h, "erp_login_throttle").await, 1);
}

/// The kind check of a table made by an earlier revision is widened by the next sync.
#[tokio::test]
async fn an_old_kind_check_is_widened() {
    let w = world().await;
    sql(
        &w.h,
        "ALTER TABLE erp_login_throttle DROP CONSTRAINT erp_login_throttle_kind_check, \
         ADD CONSTRAINT erp_login_throttle_kind_check CHECK (kind IN ('address', 'login'))",
    )
    .await;
    sql(&w.h, AUTH_DDL).await;
    sql(
        &w.h,
        "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at) \
         VALUES ('trusted', sha256('pair'::bytea), 1, now())",
    )
    .await;
    assert_eq!(
        count(&w.h, "erp_login_throttle WHERE kind = 'trusted'").await,
        1
    );
}

/// Integration of review round 1 (types T1 / types-delta S3): a trusted pair is one row of one
/// success, so the table refuses a trusted row whose failures column is not 1; a table of an
/// earlier revision gets the check when the DDL runs again.
#[tokio::test]
async fn a_trusted_row_counts_exactly_one() {
    let w = world().await;
    let insert = "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at) \
                  VALUES ($1, sha256('pair'::bytea), $2, now())";
    let refused = |e: erp_sdk::Error| {
        assert_eq!(
            (e.kind(), e.sqlstate()),
            (ErrorKind::Validation, Some("23514")),
            "{e}"
        );
        assert!(
            e.message().contains("erp_login_throttle_trusted_once"),
            "{e}"
        );
    };
    let mut tx = w.h.begin().await.unwrap();
    refused(tx.execute(insert, &[&"trusted", &2i32]).await.unwrap_err());
    tx.rollback().await.unwrap();
    // the counted kinds still count past 1
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(insert, &[&"login", &2i32]).await.unwrap();
    tx.execute(insert, &[&"trusted", &1i32]).await.unwrap();
    tx.rollback().await.unwrap();
    // an earlier revision's table, without the check: the DDL adds it
    sql(
        &w.h,
        "ALTER TABLE erp_login_throttle DROP CONSTRAINT erp_login_throttle_trusted_once",
    )
    .await;
    sql(&w.h, AUTH_DDL).await;
    let mut tx = w.h.begin().await.unwrap();
    refused(tx.execute(insert, &[&"trusted", &2i32]).await.unwrap_err());
    tx.rollback().await.unwrap();
}

/// The `xmin` of the catalog rows of both tables and their indexes, by name: a row rewritten by
/// a DDL statement gets a new one.
async fn catalog_versions(h: &Harness) -> Vec<(String, String)> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT c.relname::text, c.xmin::text FROM pg_class c
              WHERE c.oid IN ('erp_session'::regclass, 'erp_login_throttle'::regclass)
                 OR c.oid IN (SELECT indexrelid FROM pg_index
                               WHERE indrelid IN ('erp_session'::regclass,
                                                  'erp_login_throttle'::regclass))
              ORDER BY 1",
            &[],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

/// Database review round 2 N2 and N4: a sync of a database that has the DDL rewrites no
/// catalog row (no `REVOKE` when `PUBLIC` holds nothing, no `ALTER TABLE … SET` when no option
/// is missing).
#[tokio::test]
async fn a_second_ddl_run_rewrites_no_catalog_row() {
    let w = world().await;
    let before = catalog_versions(&w.h).await;
    assert!(before.len() >= 6, "{before:?}");
    sql(&w.h, AUTH_DDL).await;
    sql(&w.h, AUTH_AFTER_MODELS_DDL).await;
    assert_eq!(catalog_versions(&w.h).await, before);
}

/// Database review round 2 N2: a privilege granted to `PUBLIC` by hand is revoked by the next
/// sync.
#[tokio::test]
async fn a_public_grant_is_revoked_by_the_next_sync() {
    let w = world().await;
    sql(
        &w.h,
        "GRANT SELECT ON erp_session TO PUBLIC; GRANT INSERT ON erp_login_throttle TO PUBLIC",
    )
    .await;
    let public = "SELECT count(*) FROM pg_class c,
                   aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
                  WHERE c.oid IN ('erp_session'::regclass, 'erp_login_throttle'::regclass)
                    AND a.grantee = 0";
    assert_eq!(one_i64(&w.h, public).await, 2);
    sql(&w.h, AUTH_DDL).await;
    assert_eq!(one_i64(&w.h, public).await, 0);
}

/// The storage parameters of `table`, sorted.
async fn reloptions(h: &Harness, table: &str) -> Vec<String> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT coalesce(reloptions, '{}')::text[] FROM pg_class \
              WHERE oid = $1::text::regclass",
            &[&table],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let mut options: Vec<String> = rows[0].get(0);
    options.sort();
    options
}

/// Database review round 2 N4: the DDL sets only the storage parameters a table lacks; a value
/// an operator set stays.
#[tokio::test]
async fn storage_parameters_are_set_only_where_missing() {
    let w = world().await;
    sql(
        &w.h,
        "ALTER TABLE erp_session SET (autovacuum_vacuum_scale_factor = 0.005); \
         ALTER TABLE erp_session RESET (fillfactor); \
         ALTER TABLE erp_login_throttle RESET (autovacuum_vacuum_threshold)",
    )
    .await;
    sql(&w.h, AUTH_DDL).await;
    assert_eq!(
        reloptions(&w.h, "erp_session").await,
        [
            "autovacuum_analyze_scale_factor=0.05",
            "autovacuum_vacuum_cost_delay=2",
            "autovacuum_vacuum_insert_scale_factor=0.05",
            "autovacuum_vacuum_scale_factor=0.005",
            "autovacuum_vacuum_threshold=500",
            "fillfactor=85",
        ]
    );
    assert_eq!(
        reloptions(&w.h, "erp_login_throttle").await,
        [
            "autovacuum_analyze_scale_factor=0.05",
            "autovacuum_vacuum_cost_delay=0",
            "autovacuum_vacuum_scale_factor=0.01",
            "autovacuum_vacuum_threshold=1000",
            "fillfactor=80",
        ]
    );
}

/// Database review round 2 N3: a table of an earlier revision with rows that break a check the
/// DDL adds is made to comply first (a pending row's expiry is cut to the pending lifetime, a
/// trusted row counts one), then the check is added.
#[tokio::test]
async fn older_rows_are_made_to_comply_before_a_check_is_added() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    sql(
        &w.h,
        "ALTER TABLE erp_session DROP CONSTRAINT erp_session_pending_expiry; \
         ALTER TABLE erp_login_throttle DROP CONSTRAINT erp_login_throttle_trusted_once",
    )
    .await;
    sql(
        &w.h,
        &format!(
            "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, \
                                      created_at, expires_at) \
             VALUES (gen_random_uuid(), {bob}, sha256('p'::bytea), sha256('q'::bytea), 1, \
                     'pending', now(), now() + interval '2 hours'); \
             INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at) \
             VALUES ('trusted', sha256('pair'::bytea), 3, now())"
        ),
    )
    .await;
    sql(&w.h, AUTH_DDL).await;
    for constraint in [
        "erp_session_pending_expiry",
        "erp_login_throttle_trusted_once",
    ] {
        assert!(
            one_bool(
                &w.h,
                "SELECT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = $1::text \
                                   AND convalidated)",
                &[&constraint],
            )
            .await,
            "{constraint}"
        );
    }
    assert!(
        one_bool(
            &w.h,
            "SELECT expires_at = created_at + interval '600 seconds' FROM erp_session \
              WHERE state = 'pending'",
            &[],
        )
        .await
    );
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'trusted' AND failures = 1"
        )
        .await,
        1
    );
}

/// Database review round 2 S1: the two collection deletes of `erp_login_throttle` each read
/// their own partial index, so the counted kinds' delete never walks the trusted pairs (which
/// live 30 days and would sort first in one shared index); an index of the earlier revision is
/// dropped by the DDL.
#[tokio::test]
async fn throttle_collection_uses_one_partial_index_per_kind() {
    let w = world().await;
    sql(
        &w.h,
        "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at)
         SELECT 'trusted', sha256(('t' || i)::bytea), 1, now() - interval '10 days'
           FROM generate_series(1, 30000) i;
         INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at)
         SELECT 'login', sha256(('l' || i)::bytea), 1, now() - interval '1 day'
           FROM generate_series(1, 3000) i;
         ANALYZE erp_login_throttle",
    )
    .await;
    let counted = plan(&w.h, THROTTLE_SQL, &[&120.0f64, &i64::from(ROW_BATCH)]).await;
    assert_eq!(
        limited_scan(&counted),
        (
            "Index Scan".to_owned(),
            Some("erp_login_throttle_counted_age".to_owned())
        ),
        "{counted}"
    );
    let trusted = plan(
        &w.h,
        TRUSTED_SQL,
        &[&(30.0f64 * 86_400.0), &i64::from(ROW_BATCH)],
    )
    .await;
    assert_eq!(
        limited_scan(&trusted),
        (
            "Index Scan".to_owned(),
            Some("erp_login_throttle_trusted_age".to_owned())
        ),
        "{trusted}"
    );
    let report = w.auth.gc().await.unwrap();
    assert_eq!((report.throttle_rows, report.trusted_rows), (3_000, 0));
    // a database of the earlier revision: its single age index goes
    sql(
        &w.h,
        "CREATE INDEX erp_login_throttle_age ON erp_login_throttle (last_failure_at)",
    )
    .await;
    sql(&w.h, AUTH_DDL).await;
    assert!(
        one_bool(
            &w.h,
            "SELECT to_regclass('erp_login_throttle_age') IS NULL",
            &[]
        )
        .await
    );
}

/// The index of `res_users_log` the retention reads (looked up by its definition).
async fn user_log_index(h: &Harness) -> String {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT indexname::text FROM pg_indexes WHERE tablename = 'res_users_log' \
               AND indexdef LIKE '%(create_uid, create_date DESC NULLS LAST, id DESC)%'",
            &[],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one index (create_uid, create_date DESC, id DESC)"
    );
    rows[0].get(0)
}

/// Database review round 2 S2: each login-log row is compared with its user's newest row,
/// found by one top-1 read of `(create_uid, create_date DESC NULLS LAST, id DESC)` (no per-row
/// walk of the user's rows); the single-column index of the earlier revision is that index's
/// prefix, so it is not kept beside it.
#[tokio::test]
async fn user_log_retention_reads_the_newest_row_through_the_index() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let admin = w.admin;
    sql(
        &w.h,
        &format!(
            "INSERT INTO res_users_log (create_uid, create_date, write_uid, write_date)
             SELECT CASE WHEN i % 2 = 0 THEN {admin} ELSE {bob} END,
                    now() AT TIME ZONE 'UTC' - make_interval(secs => i), {admin},
                    now() AT TIME ZONE 'UTC'
               FROM generate_series(1, 20000) i;
             ANALYZE res_users_log"
        ),
    )
    .await;
    let index = user_log_index(&w.h).await;
    assert_eq!(
        count(&w.h, "pg_indexes WHERE tablename = 'res_users_log'").await,
        2,
        "the primary key and the retention index"
    );
    let log = plan(&w.h, USER_LOG_SQL, &[&i64::from(ROW_BATCH)]).await;
    let top1: Vec<_> = nodes(&log)
        .into_iter()
        .filter(|n| n["Index Name"] == index.as_str())
        .collect();
    assert_eq!(top1.len(), 1, "{log}");
    assert!(
        ["Index Only Scan", "Index Scan"].contains(&top1[0]["Node Type"].as_str().unwrap()),
        "{log}"
    );
    assert_eq!(top1[0]["Scan Direction"], "Forward", "{log}");
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.user_logs, 20_000 - 2);
    assert_eq!(count(&w.h, "res_users_log").await, 2);
}

#[tokio::test]
async fn user_log_rows_without_date_or_user_stay() {
    let w = world().await;
    let admin = w.admin;
    sql(
        &w.h,
        &format!(
            "INSERT INTO res_users_log (create_uid, create_date, write_uid, write_date) VALUES
               ({admin}, NULL, {admin}, NULL),
               ({admin}, now() AT TIME ZONE 'UTC' - interval '2 days', {admin}, NULL),
               ({admin}, now() AT TIME ZONE 'UTC' - interval '1 day', {admin}, NULL),
               (NULL, now() AT TIME ZONE 'UTC' - interval '3 days', NULL, NULL),
               (NULL, now() AT TIME ZONE 'UTC' - interval '1 day', NULL, NULL)"
        ),
    )
    .await;
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.user_logs, 1, "the two-day-old admin row");
    assert_eq!(
        count(
            &w.h,
            &format!(
                "res_users_log WHERE create_uid = {admin} AND (create_date IS NULL \
                   OR create_date > now() AT TIME ZONE 'UTC' - interval '1 day 1 hour')"
            )
        )
        .await,
        2
    );
    assert_eq!(
        count(&w.h, "res_users_log WHERE create_uid IS NULL").await,
        2
    );
}

/// `n` sessions of `user` idle for two hours, packed from the start of the heap.
async fn idle_sessions(h: &Harness, user: i64, n: i64, tag: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at,
                                  last_seen_at)
         SELECT gen_random_uuid(), $1::int8, sha256(($3::text || 's' || i)::bytea),
                sha256(($3::text || 't' || i)::bytea), 1, 'active', now() + interval '1 day',
                now() - interval '2 hours'
           FROM generate_series(1, $2::int8) i",
        &[&user, &n, &tag],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// One row of `query` as `i64`.
async fn one_i64(h: &Harness, query: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let n: i64 = tx.query(query, &[]).await.unwrap()[0].get(0);
    tx.rollback().await.unwrap();
    n
}

/// Shared blocks (hit and read) of the plan's TID range scan of `erp_session`.
fn tid_scan_blocks(plan: &serde_json::Value) -> i64 {
    let scans: Vec<_> = nodes(plan)
        .into_iter()
        .filter(|n| n["Node Type"] == "Tid Range Scan")
        .collect();
    assert_eq!(scans.len(), 1, "{plan}");
    scans[0]["Shared Hit Blocks"].as_i64().unwrap()
        + scans[0]["Shared Read Blocks"].as_i64().unwrap()
}

/// Database review round 2 S3: an idle-sweep batch reads the heap from just after the last row
/// it handled and stops at its batch's last row, so a dense range is read once, not once per
/// full batch. The statement's own block reads are measured (EXPLAIN ANALYZE, BUFFERS).
#[tokio::test]
async fn an_idle_batch_reads_only_the_pages_up_to_its_last_row() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    idle_sessions(&w.h, bob, 3_500, "dense").await;
    let pages = one_i64(
        &w.h,
        "SELECT (pg_relation_size('erp_session') / current_setting('block_size')::int8)::int8",
    )
    .await;
    // the page of the 1,000th and of the 2,000th row in heap order
    let page_of = |nth: i64| {
        format!(
            "SELECT (ctid::text::point)[0]::int8 FROM erp_session ORDER BY ctid \
              OFFSET {} LIMIT 1",
            nth - 1
        )
    };
    let first_end = one_i64(&w.h, &page_of(1_000)).await;
    let second_end = one_i64(&w.h, &page_of(2_000)).await;
    assert!(second_end < pages, "{second_end} {pages}");
    let analyze = "ANALYZE, BUFFERS, FORMAT JSON";
    let to = i64::from(SWEEP_PAGES);
    let first = explain(
        &w.h,
        analyze,
        IDLE_SQL,
        &[&0i64, &0i32, &to, &60.0f64, &1_000i64],
    )
    .await;
    // the pages up to the last row's, plus the read stream's look-ahead (PostgreSQL 17+ pins the
    // next blocks ahead of the scan; with every page in shared buffers its distance stays small)
    const LOOK_AHEAD: i64 = 4;
    assert!(
        tid_scan_blocks(&first) <= first_end + 1 + LOOK_AHEAD,
        "{first_end} {first}"
    );
    // resuming after the 1,000th row reads from its page only
    let offset = one_i64(
        &w.h,
        "SELECT (ctid::text::point)[1]::int8 FROM erp_session ORDER BY ctid OFFSET 999 LIMIT 1",
    )
    .await;
    let offset = i32::try_from(offset).unwrap();
    let second = explain(
        &w.h,
        analyze,
        IDLE_SQL,
        &[&first_end, &offset, &to, &60.0f64, &1_000i64],
    )
    .await;
    // a re-read of the range from its start would read `second_end + 1` pages
    assert!(second_end - first_end + 1 + LOOK_AHEAD < second_end + 1);
    assert!(
        tid_scan_blocks(&second) <= second_end - first_end + 1 + LOOK_AHEAD,
        "{first_end} {second_end} {second}"
    );
}

/// Database review round 2 S3: a run over a dense range deletes every idle row, keeps the fresh
/// ones, and counts the pages it actually read toward its sweep cap: the table once, plus the
/// one page each full batch shares with the next.
#[tokio::test]
async fn the_idle_sweep_counts_the_pages_it_reads() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "sessions.max_inactivity_seconds", "60").await;
    idle_sessions(&w.h, bob, 3_500, "dense").await;
    sessions(&w.h, bob, 200, "fresh").await;
    let pages = one_i64(
        &w.h,
        "SELECT (pg_relation_size('erp_session') / current_setting('block_size')::int8)::int8",
    )
    .await;
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.idle_sessions, 3_500);
    assert_eq!(count(&w.h, "erp_session").await, 200);
    assert_eq!(report.next, GcCursor::START, "the sweep reached the end");
    let pages = u64::try_from(pages).unwrap();
    // three full batches of 1,000, then the rest of the table
    assert!(
        (pages..=pages + 3).contains(&report.swept_pages),
        "{} pages read of a {pages}-page table",
        report.swept_pages
    );
}

/// Database review round 2 N10: once the `password` column exists, the statement that adds it
/// takes no lock on `res_users` (`ADD COLUMN IF NOT EXISTS` would take ACCESS EXCLUSIVE and wait
/// behind any open reader).
#[tokio::test]
async fn the_password_column_statement_takes_no_lock_when_the_column_exists() {
    let w = world().await;
    let mut reader = w.h.begin().await.unwrap();
    reader
        .query("SELECT count(*) FROM res_users", &[])
        .await
        .unwrap();
    let mut tx = w.h.begin().await.unwrap();
    tx.batch("SET LOCAL lock_timeout = '200ms'").await.unwrap();
    tx.batch(erp_addon_base::res_users_auth::PASSWORD_COLUMN_DDL)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    reader.rollback().await.unwrap();
}
