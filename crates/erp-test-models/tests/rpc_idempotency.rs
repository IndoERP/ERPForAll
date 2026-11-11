//! Idempotency keys in-process (design note §10, lead rulings 2
//! and 4).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use erp_orm::{ModelAllow, Orm, SelfApplied, SignalKey};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcService};
use serde_json::json;
use support::rpc::*;

const BUMP: &str = "/json/2/test_rpc.effects/bump";

#[tokio::test]
async fn a_keyed_write_replays_byte_for_byte() {
    let h = harness().await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-000000000007");
    let body = r#"{"name": "once", "context": {"tz": "UTC"}}"#;
    let first = call_keyed(&h, BUMP, body, alice(), &k).await;
    let ids = ok(&first);
    assert!(!first.replayed());
    // key order and whitespace do not matter
    let same = r#"{ "context" : {"tz":"UTC"} , "name":"once" }"#;
    for _ in 0..3 {
        let again = call_keyed(&h, BUMP, same, alice(), &k).await;
        assert!(again.replayed());
        assert_eq!(again.status(), first.status());
        assert_eq!(again.body(), first.body(), "same bytes");
    }
    assert_eq!(count(&h, "test_rpc_effects").await, 1, "one effect");
    assert_eq!(
        sql_one(&h, "SELECT id::text FROM test_rpc_effects").await,
        ids[0].to_string()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_key_belongs_to_one_request_and_one_principal() {
    let h = harness().await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-000000000008");
    ok(&call_keyed(&h, BUMP, r#"{"name": "a"}"#, alice(), &k).await);
    let r = call_keyed(&h, BUMP, r#"{"name": "b"}"#, alice(), &k).await;
    assert_error(
        &r,
        422,
        "erp.idempotency_mismatch",
        "Idempotency-Key 018f3a1c-7b2d-7c4e-9a10-000000000008 was already used for a different \
         request; use a new key.",
    );
    // another principal, and the same user through an API key, have their own key spaces
    let admin = call_keyed(&h, BUMP, r#"{"name": "b"}"#, user(ADMIN), &k).await;
    assert!(!admin.replayed());
    ok(&admin);
    let via_key = alice_key(scope(11, "alice key", ModelAllow::All, false));
    let keyed = call_keyed(&h, BUMP, r#"{"name": "b"}"#, via_key, &k).await;
    assert!(!keyed.replayed());
    ok(&keyed);
    assert_eq!(count(&h, "test_rpc_effects").await, 3);
    assert_eq!(count(&h, "ir_idempotency_key").await, 3);
    h.cleanup().await.unwrap();
}

/// Poll (the server's view, not a sleep of the test) until a session of this database matches
/// `condition`, a `WHERE` fragment over `pg_stat_activity`.
async fn wait_for_sessions(h: &erp_testkit::RpcHarness, condition: &str) {
    let query = format!(
        "SELECT (count(*) > 0)::text FROM pg_stat_activity \
         WHERE datname = current_database() AND pid <> pg_backend_pid() AND {condition}"
    );
    for _ in 0..4000 {
        if sql_one(h, &query).await == "true" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no session where {condition}");
}

/// The test's hold on the advisory lock `gate`, in a transaction of its own: calls to
/// `gated_bump` stop after their write until [`Gate::open`] (or until the gate is dropped).
struct Gate {
    tx: erp_orm::Transaction,
}

impl Drop for Gate {
    /// Dropping the gate releases the advisory xact lock by ending the holding transaction
    /// (the dropped transaction connection rolls back on the server). The waiting RPC call's
    /// `gated_bump` polling loop then sees `pg_try_advisory_xact_lock` succeed and proceeds
    /// to commit.
    fn drop(&mut self) {}
}

impl Gate {
    async fn close(h: &erp_testkit::RpcHarness, key: i64) -> Self {
        let mut tx = h.harness().begin().await.unwrap();
        tx.query("SELECT pg_advisory_xact_lock($1)::text", &[&key])
            .await
            .unwrap();
        Self { tx }
    }

    /// A `gated_bump` call is parked on the gate.
    async fn wait_for_caller(h: &erp_testkit::RpcHarness) {
        wait_for_sessions(
            h,
            "state = 'active' AND query LIKE '%pg_try_advisory_xact_lock%'",
        )
        .await;
    }

    async fn open(mut self) {
        self.tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn concurrent_duplicates_have_one_effect() {
    let h = harness().await;
    // the duplicate waits on the unique index; when the first commits it gets 40001 and its
    // retry replays. The first call is parked on a gate until the duplicate is queued behind
    // it, so neither side's speed decides who sees what.
    let k = key("018f3a1c-7b2d-7c4e-9a10-000000000009");
    let path = "/json/2/test_rpc.effects/gated_bump";
    let body = r#"{"name": "raced", "gate": 9009, "millis": 30000}"#;
    let gate = Gate::close(&h, 9009).await;
    let (a, b) = tokio::join!(call_keyed(&h, path, body, alice(), &k), async {
        Gate::wait_for_caller(&h).await;
        let duplicate = call_keyed(&h, path, body, alice(), &k);
        let release = async {
            // the claim's INSERT waits for the first transaction to end
            wait_for_sessions(
                &h,
                "wait_event_type = 'Lock' AND wait_event = 'transactionid'",
            )
            .await;
            gate.open().await;
        };
        tokio::join!(duplicate, release).0
    });
    assert_eq!(ok(&a), ok(&b));
    assert!(!a.replayed() && b.replayed(), "the second replays");
    assert_eq!(b.attempts(), 2, "the claim's 40001 counted as an attempt");
    assert_eq!(count(&h, "test_rpc_effects").await, 1);

    // with a lock_timeout shorter than the first call, the duplicate gets 409 with Retry-After
    // (the ORM sets the limit with every BEGIN: a session option on the pool would be overridden)
    let pool = erp_orm::pool::create_pool(h.harness().db().config(), 4).unwrap();
    let orm_config = erp_orm::OrmConfig {
        timeouts: erp_orm::Timeouts::new(
            Duration::from_millis(300),
            Duration::from_secs(60),
            Duration::from_secs(600),
        )
        .unwrap(),
        ..erp_orm::OrmConfig::default()
    };
    let orm = Orm::builder(Arc::clone(h.orm().registry()), pool)
        .policy(erp_testkit::RulesPolicy::new(FixturePolicy).unwrap())
        .config(orm_config)
        .build()
        .unwrap();
    let service = RpcService::new(orm, FixtureHost, RpcConfig::default());
    let k2 = key("018f3a1c-7b2d-7c4e-9a10-000000000019");
    let gated = r#"{"name": "slow", "gate": 9019, "millis": 30000}"#;
    let keyed = |k: &erp_rpc::IdempotencyKey| {
        RpcCall::json2(path, gated.as_bytes().to_vec(), alice()).idempotency_key(k.clone())
    };
    let gate = Gate::close(&h, 9019).await;
    let (first, second) = tokio::join!(service.call(keyed(&k2)), async {
        Gate::wait_for_caller(&h).await;
        // the first call stays parked until the duplicate has given up on the key
        let second = service.call(keyed(&k2)).await;
        gate.open().await;
        second
    });
    ok(&first);
    assert_error(
        &second,
        409,
        "erp.idempotency_in_progress",
        "A request with Idempotency-Key 018f3a1c-7b2d-7c4e-9a10-000000000019 is still running; \
         retry after 1 seconds.",
    );
    assert_eq!(second.retry_after(), Some(1));
    let later = service.call(keyed(&k2)).await;
    assert!(later.replayed());
    assert_eq!(later.body(), first.body());
    assert_eq!(count(&h, "test_rpc_effects").await, 2);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn gated_bump_fails_loudly_when_gate_times_out() {
    let h = harness().await;
    let path = "/json/2/test_rpc.effects/gated_bump";
    let body = r#"{"name": "timeout", "gate": 9029, "millis": 50}"#;
    let _gate = Gate::close(&h, 9029).await;
    let r = call(&h, path, body, alice()).await;
    assert_error(
        &r,
        500,
        "erp.database",
        "A database error occurred (SQLSTATE P0001).",
    );
    assert_eq!(count(&h, "test_rpc_effects").await, 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn gate_drop_releases_waiting_caller() {
    let h = harness().await;
    let path = "/json/2/test_rpc.effects/gated_bump";
    let body = r#"{"name": "drop_releases", "gate": 9039, "millis": 30000}"#;
    let gate = Gate::close(&h, 9039).await;
    let (r, _) = tokio::join!(call(&h, path, body, alice()), async {
        Gate::wait_for_caller(&h).await;
        drop(gate);
    });
    ok(&r);
    assert_eq!(count(&h, "test_rpc_effects").await, 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_failed_keyed_call_stores_nothing() {
    let h = harness().await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-000000000010");
    let r = call_keyed(
        &h,
        "/json/2/test_rpc.effects/bump_then_fail",
        r#"{"name": "x"}"#,
        alice(),
        &k,
    )
    .await;
    assert_error(&r, 422, "erp.exceptions.ValidationError", "bump refused");
    assert_eq!(count(&h, "ir_idempotency_key").await, 0);
    assert_eq!(count(&h, "test_rpc_effects").await, 0);
    // nothing was stored: any request with the key executes
    let r = call_keyed(&h, BUMP, r#"{"name": "y"}"#, alice(), &k).await;
    ok(&r);
    assert!(!r.replayed());
    assert_eq!(count(&h, "test_rpc_effects").await, 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_expired_row_is_taken_over() {
    let h = harness().await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-000000000011");
    ok(&call_keyed(&h, BUMP, r#"{"name": "old"}"#, alice(), &k).await);
    sql(
        &h,
        "UPDATE ir_idempotency_key SET expires_at = now() - interval '1 second' RETURNING 'ok'",
    )
    .await;
    let r = call_keyed(&h, BUMP, r#"{"name": "new"}"#, alice(), &k).await;
    ok(&r);
    assert!(!r.replayed(), "a different body executes instead of 422");
    assert_eq!(count(&h, "test_rpc_effects").await, 2);
    assert_eq!(
        count(&h, "ir_idempotency_key").await,
        1,
        "the row was reused"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn replays_need_unchanged_access_inside_the_window() {
    let h = harness_with(&[], RpcConfig::default(), true).await;
    let body = r#"{"name": "x"}"#;
    let changed = |r: &erp_rpc::RpcResponse| {
        assert_error(
            r,
            409,
            "erp.idempotency_access_changed",
            "idempotency replay refused: access changed since the original call; the original \
             call committed",
        );
    };
    // company context of the session
    let k1 = key("018f3a1c-7b2d-7c4e-9a10-000000000101");
    let in_c1 = || Caller::Principal(principal_with(ALICE, &[1], 1).build().unwrap());
    let in_c2 = Caller::Principal(principal_with(ALICE, &[2, 1], 1).build().unwrap());
    ok(&call_keyed(&h, BUMP, body, in_c1(), &k1).await);
    changed(&call_keyed(&h, BUMP, body, in_c2, &k1).await);
    assert!(call_keyed(&h, BUMP, body, in_c1(), &k1).await.replayed());
    // the API key's scope
    let k2 = key("018f3a1c-7b2d-7c4e-9a10-000000000102");
    let wide = alice_key(scope(12, "k", ModelAllow::All, false));
    let narrow = alice_key(scope(
        12,
        "k",
        ModelAllow::named(["test_rpc.effects"]).unwrap(),
        false,
    ));
    ok(&call_keyed(&h, BUMP, body, wide, &k2).await);
    changed(&call_keyed(&h, BUMP, body, narrow, &k2).await);
    // the subject's resolution epoch (a group membership change)
    let k3 = key("018f3a1c-7b2d-7c4e-9a10-000000000103");
    ok(&call_keyed(&h, BUMP, body, in_c1(), &k3).await);
    let regrouped = Caller::Principal(principal_with(ALICE, &[1], 2).build().unwrap());
    changed(&call_keyed(&h, BUMP, body, regrouped, &k3).await);
    // a policy bump (SIGNAL generation)
    let k4 = key("018f3a1c-7b2d-7c4e-9a10-000000000104");
    ok(&call_keyed(&h, BUMP, body, in_c1(), &k4).await);
    let mut tx = h.orm().begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.signal_bump(&SignalKey::Policy, SelfApplied::No)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    changed(&call_keyed(&h, BUMP, body, in_c1(), &k4).await);
    let effects = count(&h, "test_rpc_effects").await;
    assert_eq!(effects, 4, "no refused replay ran the method");
    // past the replay window: 409, and the key still deduplicates
    let k5 = key("018f3a1c-7b2d-7c4e-9a10-000000000105");
    ok(&call_keyed(&h, BUMP, body, in_c1(), &k5).await);
    sql(
        &h,
        "UPDATE ir_idempotency_key SET created_at = now() - interval '2 hours' WHERE key = \
         '018f3a1c-7b2d-7c4e-9a10-000000000105' RETURNING 'ok'",
    )
    .await;
    let expired = call_keyed(&h, BUMP, body, in_c1(), &k5).await;
    assert_error(
        &expired,
        409,
        "erp.idempotency_result_expired",
        "the original call committed; its result is no longer available",
    );
    // a purged response (the retention job) reads the same
    let k6 = key("018f3a1c-7b2d-7c4e-9a10-000000000106");
    ok(&call_keyed(&h, BUMP, body, in_c1(), &k6).await);
    sql(
        &h,
        "UPDATE ir_idempotency_key SET response = NULL WHERE key = \
         '018f3a1c-7b2d-7c4e-9a10-000000000106' RETURNING 'ok'",
    )
    .await;
    assert_eq!(
        json(&call_keyed(&h, BUMP, body, in_c1(), &k6).await)["name"],
        json!("erp.idempotency_result_expired")
    );
    assert_eq!(
        count(&h, "test_rpc_effects").await,
        effects + 2,
        "never twice"
    );
    h.cleanup().await.unwrap();
}
