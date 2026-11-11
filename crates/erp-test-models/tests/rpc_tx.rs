//! JSON-2 units of work in-process (and 7): the read-only marker,
//! the whole-call retry, integrity errors, the result limit and the commit outcome.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use erp_orm::{CommitFault, Orm};
use erp_rpc::{OrmSource, RpcCall, RpcConfig, RpcService};
use erp_test_models::test_rpc::{ATTEMPTS, HOOKS, READS};
use serde_json::json;
use support::rpc::*;

fn read_guard(method: &str, op: &str) -> String {
    format!(
        "'test_rpc.effects.{method}' is declared effect = read but tried to {op}; declare effect \
         = write (or wider)."
    )
}

#[tokio::test]
async fn a_read_method_that_writes_fails_once() {
    let h = harness().await;
    let id = effect(&h, "before", 1).await;
    let with_ids = format!(r#"{{"ids": [{id}]}}"#);
    let cases = [
        ("bad_create", "{}".to_owned(), "create 'test_rpc.effects'"),
        ("bad_write", with_ids.clone(), "write 'test_rpc.effects'"),
        ("bad_unlink", with_ids.clone(), "unlink 'test_rpc.effects'"),
        (
            "bad_lock",
            with_ids.clone(),
            "lock rows of 'test_rpc.effects'",
        ),
        ("bad_sql", "{}".to_owned(), "write through SQL"),
        // a SELECT the guard lets through: the database refuses it (25006)
        ("bad_nextval", "{}".to_owned(), "write through SQL"),
    ];
    for (method, body, op) in cases {
        let r = call(
            &h,
            &format!("/json/2/test_rpc.effects/{method}"),
            &body,
            alice(),
        )
        .await;
        assert_error(&r, 500, "erp.programming", &read_guard(method, op));
        assert_eq!(
            r.attempts(),
            1,
            "{method}: no replay on a read/write cursor"
        );
    }
    assert_eq!(
        sql(&h, "SELECT name || ':' || counter FROM test_rpc_effects").await,
        vec![Some("before:0".to_owned())],
        "nothing written"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_read_only_method_that_writes_is_not_replayed() {
    let h = harness().await;
    let r = call(&h, "/json/2/test_rpc.effects/bad_create", "{}", alice()).await;
    assert_error(
        &r,
        500,
        "erp.programming",
        &read_guard("bad_create", "create 'test_rpc.effects'"),
    );
    assert_eq!(r.attempts(), 1);
    assert_eq!(count(&h, "test_rpc_effects").await, 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn call_kw_read_methods_that_write_are_not_retried() {
    let h = harness().await;
    let route = erp_rpc::Route::call_kw("test_rpc.effects", "bad_create").unwrap();
    let r = h
        .service()
        .call(RpcCall::new(route, b"{}".to_vec(), alice()))
        .await;
    assert_error(
        &r,
        500,
        "erp.programming",
        &read_guard("bad_create", "create 'test_rpc.effects'"),
    );
    assert_eq!(r.attempts(), 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn conflicts_are_retried_and_integrity_errors_are_not() {
    let h = harness().await;
    let id = effect(&h, "shared", 1).await;
    ATTEMPTS.store(0, Ordering::SeqCst);
    READS.store(0, Ordering::SeqCst);
    // `millis` only bounds the rendezvous of the two callers (both read before either writes)
    let body = format!(r#"{{"ids": [{id}], "millis": 30000}}"#);
    let (a, b) = tokio::join!(
        call(
            &h,
            "/json/2/test_rpc.effects/slow_increment",
            &body,
            alice()
        ),
        call(
            &h,
            "/json/2/test_rpc.effects/slow_increment",
            &body,
            alice()
        ),
    );
    let mut results = vec![ok(&a), ok(&b)];
    results.sort_by_key(|v| v.as_i64());
    assert_eq!(
        results,
        vec![json!(1), json!(2)],
        "the loser replayed and succeeded"
    );
    assert_eq!(a.attempts() + b.attempts(), 3, "one retry");
    assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 3);
    assert_eq!(
        sql_one(
            &h,
            &format!("SELECT counter::text FROM test_rpc_effects WHERE id = {id}")
        )
        .await,
        "2"
    );

    let b1 = sql_one(
        &h,
        "INSERT INTO test_rpc_model_b (name, value) VALUES ('B', 1) RETURNING id::text",
    )
    .await;
    let create = format!(r#"{{"vals_list": {{"name": "dup", "field_b1": {b1}}}}}"#);
    let first = call(&h, "/json/2/test_rpc.model_a/create", &create, alice()).await;
    assert!(ok(&first).is_i64());
    let second = call(&h, "/json/2/test_rpc.model_a/create", &create, alice()).await;
    assert_error(
        &second,
        422,
        "erp.exceptions.ValidationError",
        "The operation cannot be completed: The name must be unique",
    );
    assert_eq!(second.attempts(), 1, "integrity errors are not retried");
    assert_eq!(count(&h, "test_rpc_model_a").await, 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn database_ondelete_through_rpc() {
    let h = harness().await;
    let b1 = sql_one(
        &h,
        "INSERT INTO test_rpc_model_b (name, value) VALUES ('B1', 1) RETURNING id::text",
    )
    .await;
    let b2 = sql_one(
        &h,
        "INSERT INTO test_rpc_model_b (name, value) VALUES ('B2', 2) RETURNING id::text",
    )
    .await;
    sql(
        &h,
        &format!(
            "INSERT INTO test_rpc_model_a (name, field_b1, field_b2) VALUES ('A1', {b1}, {b2}) \
             RETURNING id::text"
        ),
    )
    .await;
    for (b, constraint) in [
        (&b1, "'required field' (field_b1)"),
        (&b2, "'restricted field' (field_b2)"),
    ] {
        let r = call(
            &h,
            "/json/2/test_rpc.model_b/unlink",
            &format!(r#"{{"ids": [{b}]}}"#),
            alice(),
        )
        .await;
        assert_error(
            &r,
            422,
            "erp.exceptions.ValidationError",
            &format!(
                "The operation cannot be completed: Another model is using the record you are \
                 trying to delete.\n\nThe troublemaker is: 'Model A' (test_rpc.model_a)\nThanks \
                 to the following constraint: {constraint}\nHow about archiving the record \
                 instead?"
            ),
        );
        assert_eq!(r.attempts(), 1);
    }
    assert_eq!(count(&h, "test_rpc_model_b").await, 2, "nothing deleted");
    h.cleanup().await.unwrap();
}

/// An [`OrmSource`] that counts how often the service asks for the current ORM.
struct Counting {
    orm: Orm,
    calls: Arc<AtomicUsize>,
}

impl OrmSource for Counting {
    fn current(&self) -> erp_sdk::Result<Orm> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.orm.clone())
    }
}

#[tokio::test]
async fn the_retry_loop() {
    let h = harness().await;
    ATTEMPTS.store(0, Ordering::SeqCst);
    let r = call(
        &h,
        "/json/2/test_rpc.effects/always_conflict",
        "{}",
        alice(),
    )
    .await;
    assert_error(
        &r,
        500,
        "psycopg2.errors.SerializationFailure",
        "could not serialize access due to concurrent update",
    );
    assert_eq!((r.attempts(), ATTEMPTS.load(Ordering::SeqCst)), (5, 5));

    // a new transaction (and cache) per attempt; after-commit hooks run once
    ATTEMPTS.store(0, Ordering::SeqCst);
    HOOKS.store(0, Ordering::SeqCst);
    let r = call(
        &h,
        "/json/2/test_rpc.effects/conflict_once",
        r#"{"name": "once"}"#,
        alice(),
    )
    .await;
    let ids = ok(&r);
    assert_eq!(r.attempts(), 2);
    assert_eq!(
        HOOKS.load(Ordering::SeqCst),
        1,
        "after-commit hooks run once"
    );
    assert_eq!(
        sql(
            &h,
            "SELECT id::text FROM test_rpc_effects WHERE name = 'once'"
        )
        .await,
        vec![Some(ids[0].to_string())],
        "the first attempt rolled back"
    );

    // 55P03 is 409 without retry
    let id = effect(&h, "held", 1).await;
    let mut holder = h.harness().begin().await.unwrap();
    holder
        .query(
            &format!("SELECT id FROM test_rpc_effects WHERE id = {id} FOR UPDATE"),
            &[],
        )
        .await
        .unwrap();
    ATTEMPTS.store(0, Ordering::SeqCst);
    let r = call(
        &h,
        "/json/2/test_rpc.effects/locked",
        &format!(r#"{{"ids": [{id}]}}"#),
        alice(),
    )
    .await;
    assert_eq!(r.status(), 409, "{}", String::from_utf8_lossy(r.body()));
    assert_eq!(json(&r)["name"], "erp.exceptions.LockError");
    assert_eq!((r.attempts(), ATTEMPTS.load(Ordering::SeqCst)), (1, 1));
    holder.rollback().await.unwrap();
    drop(holder);

    // the backoff never sleeps past the deadline: the last error stands. The pause is pinned
    // to 10 s against a 5 s deadline, so the first failed attempt (however long it took, short
    // of the deadline) must end the call: with the jittered backoff the outcome depended on
    // how long each attempt ran.
    ATTEMPTS.store(0, Ordering::SeqCst);
    h.service()
        .inject_retry_pause(Some(Duration::from_secs(10)));
    let r = call_with_deadline(
        &h,
        "/json/2/test_rpc.effects/always_conflict",
        "{}",
        alice(),
        Duration::from_secs(5),
    )
    .await;
    h.service().inject_retry_pause(None);
    assert_error(
        &r,
        500,
        "psycopg2.errors.SerializationFailure",
        "could not serialize access due to concurrent update",
    );
    assert_eq!((r.attempts(), ATTEMPTS.load(Ordering::SeqCst)), (1, 1));

    // a superseded ORM: the retry asks for the current one
    let calls = Arc::new(AtomicUsize::new(0));
    let service = RpcService::new(
        Counting {
            orm: h.orm().clone(),
            calls: Arc::clone(&calls),
        },
        FixtureHost,
        RpcConfig::default(),
    );
    ATTEMPTS.store(0, Ordering::SeqCst);
    let r = service
        .call(RpcCall::json2(
            "/json/2/test_rpc.effects/superseded_once",
            b"{}".to_vec(),
            alice(),
        ))
        .await;
    assert_eq!(ok(&r), json!(true));
    assert_eq!(r.attempts(), 2);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the retry moved to the current ORM"
    );
    h.cleanup().await.unwrap();
}

/// The deadline: 504 when the work outlives it.
#[tokio::test]
async fn the_deadline_is_enforced() {
    let h = harness().await;
    let r = call_with_deadline(
        &h,
        "/json/2/test_rpc.effects/slow_bump",
        r#"{"name": "late", "millis": 3000}"#,
        alice(),
        Duration::from_secs(1),
    )
    .await;
    assert_error(
        &r,
        504,
        "erp.deadline_exceeded",
        "The request exceeded its deadline of 1 s (limits.rpc_deadline_secs).",
    );
    assert_eq!(count(&h, "test_rpc_effects").await, 0, "rolled back");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_result_over_the_limit_rolls_back() {
    let config = RpcConfig::builder().result_bytes(1 << 20).build().unwrap();
    let h = harness_with(&[], config, false).await;
    let r = call(
        &h,
        "/json/2/test_rpc.effects/bump_big",
        &format!(r#"{{"name": "big", "size": {}}}"#, 1 << 20),
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.result_too_large",
        "The result of test_rpc.effects.bump_big is larger than the server limit of 1048576 \
         bytes (limits.rpc_result_bytes); ask for fewer records or fields, or set bin_size.",
    );
    assert_eq!(
        count(&h, "test_rpc_effects").await,
        0,
        "the write rolled back"
    );
    let r = call(
        &h,
        "/json/2/test_rpc.effects/bump_big",
        r#"{"name": "small", "size": 10}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&r), json!("xxxxxxxxxx"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_lost_commit_answer_is_unknown_and_never_retried() {
    let h = harness().await;
    let body = r#"{"name": "landed"}"#;
    // the commit landed, the answer was lost
    let k1 = key("018f3a1c-7b2d-7c4e-9a10-000000000001");
    h.service()
        .inject_commit_fault(CommitFault::AnswerLostAfterCommit);
    let r = call_keyed(&h, "/json/2/test_rpc.effects/bump", body, alice(), &k1).await;
    assert_error(
        &r,
        500,
        "erp.commit_unknown",
        "The database connection was lost while committing test_rpc.effects.bump; the change \
         may or may not have been applied. Retry with the same Idempotency-Key to get the \
         outcome.",
    );
    assert_eq!(r.attempts(), 1, "never retried");
    assert_eq!(count(&h, "test_rpc_effects").await, 1);
    let replay = call_keyed(&h, "/json/2/test_rpc.effects/bump", body, alice(), &k1).await;
    assert!(replay.replayed());
    let landed_id = sql_one(
        &h,
        "SELECT id::text FROM test_rpc_effects WHERE name = 'landed'",
    )
    .await;
    assert_eq!(ok(&replay), json!([landed_id.parse::<i64>().unwrap()]));
    assert_eq!(count(&h, "test_rpc_effects").await, 1);

    // the connection closed before COMMIT: nothing landed, the retry executes
    let k2 = key("018f3a1c-7b2d-7c4e-9a10-000000000002");
    h.service()
        .inject_commit_fault(CommitFault::ConnectionClosedBeforeCommit);
    let body2 = r#"{"name": "lost"}"#;
    let r = call_keyed(&h, "/json/2/test_rpc.effects/bump", body2, alice(), &k2).await;
    assert_eq!(json(&r)["name"], "erp.commit_unknown");
    assert_eq!(count(&h, "test_rpc_effects").await, 1);
    let retry = call_keyed(&h, "/json/2/test_rpc.effects/bump", body2, alice(), &k2).await;
    assert!(!retry.replayed(), "executed, not replayed");
    ok(&retry);
    assert_eq!(count(&h, "test_rpc_effects").await, 2);

    // without a key the message says to check the data
    h.service()
        .inject_commit_fault(CommitFault::AnswerLostAfterCommit);
    let r = call(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "unkeyed"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        500,
        "erp.commit_unknown",
        "The database connection was lost while committing test_rpc.effects.bump; the change \
         may or may not have been applied. Check the data before retrying.",
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_failing_store_is_a_definite_rollback() {
    let h = harness().await;
    sql_batch(
        &h,
        "CREATE FUNCTION refuse_store() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         IF NEW.status IS NOT NULL THEN RAISE EXCEPTION 'store refused'; END IF; RETURN NEW; END \
         $$; CREATE TRIGGER refuse_store BEFORE UPDATE ON ir_idempotency_key FOR EACH ROW \
         EXECUTE FUNCTION refuse_store()",
    )
    .await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-000000000020");
    let body = r#"{"name": "stored?"}"#;
    let r = call_keyed(&h, "/json/2/test_rpc.effects/bump", body, alice(), &k).await;
    assert_error(
        &r,
        500,
        "erp.database",
        "A database error occurred (SQLSTATE P0001).",
    );
    assert_eq!(r.attempts(), 1);
    assert_eq!(count(&h, "test_rpc_effects").await, 0, "nothing committed");
    assert_eq!(
        count(&h, "ir_idempotency_key").await,
        0,
        "the claim rolled back too"
    );
    sql_batch(&h, "DROP TRIGGER refuse_store ON ir_idempotency_key").await;
    let r = call_keyed(&h, "/json/2/test_rpc.effects/bump", body, alice(), &k).await;
    ok(&r);
    assert!(!r.replayed());
    assert_eq!(count(&h, "test_rpc_effects").await, 1);
    h.cleanup().await.unwrap();
}

/// Budgets (design note §14): a refused call opens no transaction; a `read` call adds no
/// statement to the method's own.
#[tokio::test]
async fn statement_budgets() {
    let h = harness().await;
    for (path, body) in [
        ("/json/2/res.users/search", "not json"),
        ("/json/2/res.users/_private", "{}"),
        ("/json/2/res.users/search", "{}"),
    ] {
        let r = call(&h, path, body, alice()).await;
        assert_eq!(r.attempts(), 0, "{path} {body}: no transaction");
    }
    // the read marker, exposure, gates and binding add no statement: probe runs its one query
    let r = call(&h, "/json/2/test_rpc.effects/probe", "{}", alice()).await;
    ok(&r);
    assert_eq!((r.attempts(), r.statements()), (1, 1));
    // a keyed write: +2 statements (claim, store); a replay: the claim only
    let plain = call(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "a"}"#,
        alice(),
    )
    .await;
    ok(&plain);
    let k = key("018f3a1c-7b2d-7c4e-9a10-0000000000b0");
    let keyed = call_keyed(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "b"}"#,
        alice(),
        &k,
    )
    .await;
    ok(&keyed);
    assert_eq!(keyed.statements(), plain.statements() + 2);
    let replay = call_keyed(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "b"}"#,
        alice(),
        &k,
    )
    .await;
    assert!(replay.replayed());
    assert_eq!(replay.statements(), 1);
    h.cleanup().await.unwrap();
}
