// SPDX-License-Identifier: MIT OR Apache-2.0
//! `res.users.apikeys.last_used_at`: written at most hourly per key, outside the request
//! transaction (design).
//!
//! No specification case covers last use, so these tests carry no `SPEC-` tag.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod auth_support;
use std::time::{Duration, Instant};

use auth_support::*;
use erp_fields::DateTime;
use erp_orm::{ApiKeyId, UserId};
use erp_sdk::ErrorKind;
use erp_security::auth::apikey::ApiKeyHasher;
use erp_security::auth::apikey_store::{IssueRequest, IssuedSecret, KeyKind, KeyScope, issue};
use erp_security::auth::secret::DatabaseSecret;
use erp_security::auth::{ApiKeyLastUse, ApiKeyRequest, Auth, AuthStats, CredentialOptions};

fn request(raw: &str) -> ApiKeyRequest<'_> {
    ApiKeyRequest::new(raw, KeyScope::Rpc, "192.0.2.19".parse().unwrap())
}

async fn issued(w: &World, uid: i64) -> (ApiKeyId, IssuedSecret) {
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    w.h.orm().check_audit_store().await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    tx.batch("UPDATE res_users SET active=true WHERE id=1; INSERT INTO res_groups_users_rel(uid,gid) SELECT 1,res_id FROM ir_model_data WHERE module='base' AND name='group_system' AND model='res.groups' ON CONFLICT DO NOTHING").await.unwrap();
    let secret: String = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key='database.secret'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let secret = DatabaseSecret::from_stored(&secret).unwrap();
    let hasher = ApiKeyHasher::derive(&secret).unwrap();
    let expires: i64 = tx
        .query(
            "SELECT (extract(epoch FROM now()+interval '1 hour')*1000000)::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let request = IssueRequest::new(
        UserId::new(uid).unwrap(),
        "last use",
        KeyKind::User,
        KeyScope::Rpc,
        Some(DateTime::from_micros(expires)),
    )
    .unwrap();
    let key = issue(&mut tx, &hasher, &request).await.unwrap();
    let id = key.id();
    tx.commit().await.unwrap();
    (id, key.into_secret())
}

async fn sql(w: &World, text: &str) {
    let mut tx = w.h.begin().await.unwrap();
    tx.batch(text).await.unwrap();
    tx.commit().await.unwrap();
}

/// The first column of the first row, as text.
async fn text(w: &World, query: &str) -> Option<String> {
    let mut tx = w.h.begin().await.unwrap();
    // every query of this file returns one column; cast it so booleans read as text too
    let rows = tx
        .query(&format!("SELECT x::text FROM ({query}) AS t(x)"), &[])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

fn stored(id: ApiKeyId) -> String {
    format!(
        "SELECT last_used_at::text FROM res_users_apikeys WHERE id={}",
        id.get()
    )
}

/// The database clock as naive UTC text, the same clock the column uses.
async fn db_now(w: &World) -> String {
    text(w, "SELECT (clock_timestamp() AT TIME ZONE 'utc')::text")
        .await
        .unwrap()
}

const DEADLINE: Duration = Duration::from_secs(30);

/// Poll `query` (one boolean column) until it is true.
async fn until_true(w: &World, what: &str, query: &str) {
    let started = Instant::now();
    while text(w, query).await.as_deref() != Some("true") {
        assert!(started.elapsed() < DEADLINE, "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Poll the counters of `auth` until `ready` holds; the counters at that point.
async fn until_stats(auth: &Auth, what: &str, ready: impl Fn(&AuthStats) -> bool) -> AuthStats {
    let started = Instant::now();
    loop {
        let stats = auth.stats();
        if ready(&stats) {
            return stats;
        }
        assert!(started.elapsed() < DEADLINE, "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One accepted use whose request transaction ends in a rollback; its last-use ticket.
async fn use_key(auth: &Auth, raw: &str) -> Option<ApiKeyLastUse> {
    let mut begin = auth
        .begin_api_key(request(raw), CredentialOptions::new())
        .await
        .unwrap();
    begin.tx.rollback().await.unwrap();
    begin.last_use.take()
}

async fn fixture() -> (World, ApiKeyId, IssuedSecret) {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (id, key) = issued(&w, uid).await;
    (w, id, key)
}

/// the first accepted use of a new key carries a ticket; once the
/// request transaction has committed and the ticket is dropped, the column holds the database
/// clock.
#[tokio::test]
async fn first_use_writes_the_database_clock_after_the_ticket_drops() {
    let (w, id, key) = fixture().await;
    assert_eq!(text(&w, &stored(id)).await, None);
    let before = db_now(&w).await;
    let mut begin = w
        .auth
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .unwrap();
    let ticket = begin.last_use.take().expect("a never-used key is due");
    begin.tx.commit().await.unwrap();
    drop(ticket);
    until_true(
        &w,
        "the column was written",
        &format!(
            "SELECT last_used_at >= '{before}'::timestamp \
                AND last_used_at <= (now() AT TIME ZONE 'utc') \
               FROM res_users_apikeys WHERE id={}",
            id.get()
        ),
    )
    .await;
    let stats = until_stats(&w.auth, "the write is counted", |s| s.last_use_writes == 1).await;
    assert_eq!(
        (
            stats.last_use_skipped,
            stats.last_use_noops,
            stats.last_use_errors
        ),
        (0, 0, 0)
    );
}

/// a second use within the hour carries no ticket and leaves the
/// stored value alone.
#[tokio::test]
async fn second_use_within_the_hour_has_no_ticket_and_keeps_the_value() {
    let (w, id, key) = fixture().await;
    drop(
        use_key(&w.auth, key.expose())
            .await
            .expect("first use is due"),
    );
    until_stats(&w.auth, "first write", |s| s.last_use_writes == 1).await;
    let first = text(&w, &stored(id)).await;
    assert!(first.is_some());
    assert!(use_key(&w.auth, key.expose()).await.is_none());
    assert_eq!(text(&w, &stored(id)).await, first);
    assert_eq!(w.auth.stats().last_use_writes, 1);
}

/// a stored value older than the hour is due again and moves.
#[tokio::test]
async fn value_older_than_the_hour_is_due_and_moves() {
    let (w, id, key) = fixture().await;
    sql(
        &w,
        &format!(
            "UPDATE res_users_apikeys SET last_used_at=(now() AT TIME ZONE 'utc')-interval '2 hours' WHERE id={}",
            id.get()
        ),
    )
    .await;
    let old = text(&w, &stored(id)).await.unwrap();
    let ticket = use_key(&w.auth, key.expose())
        .await
        .expect("older than the hour is due");
    drop(ticket);
    until_true(
        &w,
        "the value moved",
        &format!(
            "SELECT last_used_at > '{old}'::timestamp \
                AND last_used_at > (now() AT TIME ZONE 'utc')-interval '1 hour' \
               FROM res_users_apikeys WHERE id={}",
            id.get()
        ),
    )
    .await;
}

/// the write is independent of the request transaction; a request
/// that rolls back still records the use.
#[tokio::test]
async fn rolled_back_request_still_records_the_use() {
    let (w, id, key) = fixture().await;
    let mut begin = w
        .auth
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .unwrap();
    let ticket = begin.last_use.take().expect("due");
    begin.tx.rollback().await.unwrap();
    drop(ticket);
    until_true(
        &w,
        "the column was written",
        &format!(
            "SELECT last_used_at IS NOT NULL FROM res_users_apikeys WHERE id={}",
            id.get()
        ),
    )
    .await;
}

/// refused attempts never get a ticket and write nothing.
#[tokio::test]
async fn refused_attempts_get_no_ticket_and_write_nothing() {
    let (w, id, key) = fixture().await;
    let e = w
        .auth
        .begin_api_key(request("bad"), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
    sql(
        &w,
        &format!(
            "UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' - interval '1 second' WHERE id={}",
            id.get()
        ),
    )
    .await;
    let e = w
        .auth
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
    assert_eq!(text(&w, &stored(id)).await, None);
    let stats = w.auth.stats();
    assert_eq!(
        (
            stats.last_use_writes,
            stats.last_use_skipped,
            stats.last_use_noops,
            stats.last_use_errors
        ),
        (0, 0, 0, 0)
    );
}

/// two nodes verify one due key before either writes. Both get a
/// ticket; the second write meets a key that is no longer due and changes nothing.
#[tokio::test]
async fn two_nodes_with_one_due_key_write_once() {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let w = world_with(
        h,
        erp_security::auth::AuthConfig::DEFAULT,
        erp_security::auth::HashPoolConfig::DEFAULT,
    )
    .await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (id, key) = issued(&w, uid).await;
    let other = node_with(
        &w.h,
        4,
        own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
    )
    .await;
    let first = use_key(&w.auth, key.expose()).await.expect("due on node 1");
    let second = use_key(&other, key.expose()).await.expect("due on node 2");
    drop(first);
    until_stats(&w.auth, "node 1 wrote", |s| s.last_use_writes == 1).await;
    let version = format!(
        "SELECT last_used_at::text || ' ' || xmin::text FROM res_users_apikeys WHERE id={}",
        id.get()
    );
    let after_first = text(&w, &version).await;
    drop(second);
    until_stats(&other, "node 2 found nothing to do", |s| {
        s.last_use_noops == 1
    })
    .await;
    assert_eq!(text(&w, &version).await, after_first);
    assert_eq!(other.stats().last_use_writes, 0);
    assert_eq!(other.stats().last_use_errors, 0);
}

/// a key row locked by another transaction is skipped without
/// waiting and without an error; the next use is due again.
#[tokio::test]
async fn locked_key_row_is_skipped_without_waiting_or_error() {
    let (w, id, key) = fixture().await;
    let mut lock = w.h.begin().await.unwrap();
    lock.query(
        &format!(
            "SELECT id FROM res_users_apikeys WHERE id={} FOR UPDATE",
            id.get()
        ),
        &[],
    )
    .await
    .unwrap();
    let ticket = use_key(&w.auth, key.expose()).await.expect("due");
    drop(ticket);
    // the lock is still held: the write finished without waiting for it
    let stats = until_stats(&w.auth, "the write skipped", |s| s.last_use_noops == 1).await;
    assert_eq!((stats.last_use_writes, stats.last_use_errors), (0, 0));
    assert_eq!(text(&w, &stored(id)).await, None);
    lock.rollback().await.unwrap();
    assert!(
        use_key(&w.auth, key.expose()).await.is_some(),
        "still due after the skipped write"
    );
}

/// one write per process and ORM lineage at a time. While the first
/// write is blocked on a table lock, a second due ticket is skipped at once and counted.
#[tokio::test]
async fn second_ticket_is_skipped_while_the_gate_is_held() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (id_a, key_a) = issued(&w, uid).await;
    let (id_b, key_b) = issued(&w, uid).await;
    let ticket_a = use_key(&w.auth, key_a.expose()).await.expect("a due");
    let ticket_b = use_key(&w.auth, key_b.expose()).await.expect("b due");
    // SHARE blocks the UPDATE (ROW EXCLUSIVE) but not the row lock of its subquery
    let mut lock = w.h.begin().await.unwrap();
    lock.batch("LOCK TABLE res_users_apikeys IN SHARE MODE")
        .await
        .unwrap();
    drop(ticket_a);
    until_lock_wait(&w.h).await;
    drop(ticket_b);
    // the skip is decided synchronously by the drop
    assert_eq!(w.auth.stats().last_use_skipped, 1);
    lock.commit().await.unwrap();
    until_true(
        &w,
        "a was written",
        &format!(
            "SELECT last_used_at IS NOT NULL FROM res_users_apikeys WHERE id={}",
            id_a.get()
        ),
    )
    .await;
    assert_eq!(text(&w, &stored(id_b)).await, None);
    until_stats(&w.auth, "a's write is counted", |s| s.last_use_writes == 1).await;
}

/// a failing write is logged at warn with the key id only and
/// counted; the request that held the ticket is unaffected.
#[tokio::test]
async fn failed_write_is_logged_and_counted_and_the_request_is_unaffected() {
    let (w, id, key) = fixture().await;
    sql(
        &w,
        "CREATE FUNCTION refuse_last_use() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'last use refused for the test'; END $$; \
         CREATE TRIGGER refuse_last_use BEFORE UPDATE ON res_users_apikeys \
         FOR EACH ROW EXECUTE FUNCTION refuse_last_use()",
    )
    .await;
    let capture = warn_capture::start();
    let mut begin = w
        .auth
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .unwrap();
    let ticket = begin.last_use.take().expect("due");
    begin.tx.commit().await.unwrap();
    drop(ticket);
    let stats = until_stats(&w.auth, "the write error", |s| s.last_use_errors == 1).await;
    assert_eq!((stats.last_use_writes, stats.last_use_skipped), (0, 0));
    let lines = capture.take();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains(&format!("API key {}", id.get())),
        "{lines:?}"
    );
    assert!(
        lines[0].contains("last use refused for the test"),
        "{lines:?}"
    );
    assert!(!lines[0].contains(key.expose()), "{lines:?}");
    assert_eq!(text(&w, &stored(id)).await, None);
}

/// risk R1 of the plan. The request transaction is REPEATABLE READ
/// (`BeginOptions` default), so a ticket dropped early lets the write commit after the request's
/// snapshot; the same request then deleting that key row meets a serialization failure.
/// The test pins the error kind, SQLSTATE and whole `Display` text of that failure, as
/// `api_key_acceptance.rs` does for the same error.
#[tokio::test]
async fn ticket_dropped_early_then_same_request_deleting_the_key_fails_to_serialize() {
    let (w, id, key) = fixture().await;
    let mut begin = w
        .auth
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .unwrap();
    drop(begin.last_use.take().expect("due"));
    until_stats(&w.auth, "the write", |s| s.last_use_writes == 1).await;
    let e = begin
        .tx
        .execute("DELETE FROM res_users_apikeys WHERE id=$1", &[&id.get()])
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind(), ErrorKind::Concurrency);
    assert_eq!(e.sqlstate(), Some("40001"));
    assert_eq!(
        e.to_string(),
        "could not serialize access due to concurrent update"
    );
    begin.tx.rollback().await.unwrap();
}

/// complement of the R1 pin: a ticket held until after the request
/// transaction committed cannot meet that case; the delete succeeds and the late write finds no
/// row and changes nothing.
#[tokio::test]
async fn ticket_held_until_after_commit_lets_the_key_delete_succeed() {
    let (w, id, key) = fixture().await;
    let mut begin = w
        .auth
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .unwrap();
    let ticket = begin.last_use.take().expect("due");
    let deleted = begin
        .tx
        .execute("DELETE FROM res_users_apikeys WHERE id=$1", &[&id.get()])
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    begin.tx.commit().await.unwrap();
    drop(ticket);
    let stats = until_stats(&w.auth, "the late write found no row", |s| {
        s.last_use_noops == 1
    })
    .await;
    assert_eq!((stats.last_use_writes, stats.last_use_errors), (0, 0));
}

/// the write is bookkeeping and leaves `write_date` and `write_uid`
/// alone.
#[tokio::test]
async fn last_use_write_leaves_write_date_and_write_uid_alone() {
    let (w, id, key) = fixture().await;
    let audit = format!(
        "SELECT write_date::text || ' ' || coalesce(write_uid::text,'-') \
           FROM res_users_apikeys WHERE id={}",
        id.get()
    );
    let before = text(&w, &audit).await;
    drop(use_key(&w.auth, key.expose()).await.expect("due"));
    until_stats(&w.auth, "the write", |s| s.last_use_writes == 1).await;
    assert_eq!(text(&w, &audit).await, before);
}

/// `discard` throws a ticket away unfired. With the table locked, a
/// firing `discard` would hold the one-permit gate, so a second due ticket dropped right after it
/// would be counted as skipped at once. It is not: the second ticket takes the gate and writes, the
/// discarded key is untouched and still due on its next use.
#[tokio::test]
async fn discarded_ticket_writes_nothing_and_does_not_hold_the_gate() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (id_a, key_a) = issued(&w, uid).await;
    let (id_b, key_b) = issued(&w, uid).await;
    let ticket_a = use_key(&w.auth, key_a.expose()).await.expect("a due");
    let ticket_b = use_key(&w.auth, key_b.expose()).await.expect("b due");
    // SHARE blocks the UPDATE of a write but not the reads of the checks
    let mut lock = w.h.begin().await.unwrap();
    lock.batch("LOCK TABLE res_users_apikeys IN SHARE MODE")
        .await
        .unwrap();
    ticket_a.discard();
    drop(ticket_b);
    // decided synchronously by the drop: the gate was free
    assert_eq!(w.auth.stats().last_use_skipped, 0);
    until_lock_wait(&w.h).await;
    lock.commit().await.unwrap();
    until_true(
        &w,
        "b was written",
        &format!(
            "SELECT last_used_at IS NOT NULL FROM res_users_apikeys WHERE id={}",
            id_b.get()
        ),
    )
    .await;
    let stats = until_stats(&w.auth, "b's write is counted", |s| s.last_use_writes == 1).await;
    assert_eq!(
        (
            stats.last_use_skipped,
            stats.last_use_noops,
            stats.last_use_errors
        ),
        (0, 0, 0)
    );
    assert_eq!(text(&w, &stored(id_a)).await, None);
    assert!(
        use_key(&w.auth, key_a.expose()).await.is_some(),
        "still due after a discarded ticket"
    );
}
