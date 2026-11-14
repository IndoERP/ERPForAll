//! JSON-2 hardening from the reviews: retries on a replaced registry, concealed database
//! texts, `has_access` errors, bounded idempotency storage, caller kinds, the allowlist before
//! lookup, session-state functions in `read` calls, lock texts and carried context.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use support::currency_fixture;

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use erp_orm::{ModelAllow, Orm};
use erp_rpc::{CarriedContext, OrmSource, Route, RpcCall, RpcConfig, RpcService};
use erp_sdk::Value;
use erp_test_models::test_rpc::ATTEMPTS;
use serde_json::json;
use support::rpc::*;

/// Hands out `first` once, then `then`: a registry replaced between two attempts.
struct Upgrade {
    first: Orm,
    then: Orm,
    calls: AtomicUsize,
}

impl OrmSource for Upgrade {
    fn current(&self) -> erp_sdk::Result<Orm> {
        Ok(if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first.clone()
        } else {
            self.then.clone()
        })
    }
}

fn orm_with(h: &erp_testkit::RpcHarness, widen: bool) -> Orm {
    let mut addons: Vec<&'static erp_sdk::AddonDescriptor> = vec![
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_orm::ADDON,
        &erp_test_models::test_rpc::ADDON,
    ];
    if widen {
        addons.push(&erp_test_models::test_rpc::widen::ADDON);
    }
    let registry = Arc::new(
        erp_sdk::RegistryBuilder::new(addons)
            .with_decls(crate::currency_fixture::decls())
            .build()
            .unwrap(),
    );
    Orm::builder(registry, h.harness().db().pool().clone())
        .policy(erp_testkit::RulesPolicy::new(FixturePolicy).unwrap())
        .build()
        .unwrap()
}

/// A retry never runs a call under a declaration other than the one its gates and binding saw.
#[tokio::test]
async fn a_retry_on_a_replaced_registry_needs_the_same_declaration() {
    let h = harness().await;
    for (first_widened, change) in [
        (false, "effect read -> write"),
        (true, "effect write -> read"),
    ] {
        let service = RpcService::new(
            Upgrade {
                first: orm_with(&h, first_widened),
                then: orm_with(&h, !first_widened),
                calls: AtomicUsize::new(0),
            },
            FixtureHost,
            RpcConfig::default(),
        );
        ATTEMPTS.store(0, Ordering::SeqCst);
        let r = service
            .call(RpcCall::json2(
                "/json/2/test_rpc.effects/superseded_read_once",
                b"{}".to_vec(),
                alice(),
            ))
            .await;
        assert_error(
            &r,
            500,
            "erp.exceptions.ConcurrencyError",
            &format!(
                "the declaration of test_rpc.effects.superseded_read_once changed while the call \
                 was retried ({change}); send the call again"
            ),
        );
        assert_eq!(
            ATTEMPTS.load(Ordering::SeqCst),
            1,
            "the second attempt never ran"
        );
    }
    // the same declaration on the new registry: the retry runs
    let service = RpcService::new(
        Upgrade {
            first: orm_with(&h, false),
            then: orm_with(&h, false),
            calls: AtomicUsize::new(0),
        },
        FixtureHost,
        RpcConfig::default(),
    );
    ATTEMPTS.store(0, Ordering::SeqCst);
    let r = service
        .call(RpcCall::json2(
            "/json/2/test_rpc.effects/superseded_read_once",
            b"{}".to_vec(),
            alice(),
        ))
        .await;
    assert_eq!(ok(&r), json!(true));
    assert_eq!(r.attempts(), 2);
    h.cleanup().await.unwrap();
}

/// A unique violation without a declared message never shows the row's values.
#[tokio::test]
async fn unique_violations_conceal_the_detail_line() {
    let h = harness().await;
    sql_batch(
        &h,
        "CREATE UNIQUE INDEX test_rpc_effects_name_unique ON test_rpc_effects (name)",
    )
    .await;
    let body = r#"{"name": "secret-name-91c2"}"#;
    ok(&call(&h, "/json/2/test_rpc.effects/bump", body, alice()).await);
    let r = call(&h, "/json/2/test_rpc.effects/bump", body, alice()).await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "The database refused the change: duplicate key value violates unique constraint \
         \"test_rpc_effects_name_unique\"",
    );
    let text = String::from_utf8_lossy(r.body()).into_owned();
    assert!(
        !text.contains("secret-name-91c2") && !text.contains("Key ("),
        "{text}"
    );
    h.cleanup().await.unwrap();
}

/// `has_access` answers false only for a refusal; other policy failures propagate.
#[tokio::test]
async fn has_access_propagates_errors_other_than_refusals() {
    let h = harness().await;
    let g = sql_one(
        &h,
        "INSERT INTO test_http_galaxy (name) VALUES ('Andromeda') RETURNING id::text",
    )
    .await;
    let read = format!(r#"{{"ids": [{g}], "operation": "read"}}"#);
    assert_eq!(
        ok(&call(&h, "/json/2/test_http.galaxy/has_access", &read, alice()).await),
        json!(true)
    );
    let unlink = format!(r#"{{"ids": [{g}], "operation": "unlink"}}"#);
    let r = call(&h, "/json/2/test_http.galaxy/has_access", &unlink, alice()).await;
    assert_error(&r, 500, "erp.database", "A database error occurred.");
    h.cleanup().await.unwrap();
}

/// Claims delete a bounded batch of expired rows and drop responses past the replay window; a
/// response over `rpc.idempotency_response_bytes` is not stored.
#[tokio::test]
async fn idempotency_storage_stays_bounded() {
    let config = RpcConfig::builder()
        .idempotency_response_bytes(1024)
        .build()
        .unwrap();
    let h = harness_with(&[], config, false).await;
    sql_batch(
        &h,
        "INSERT INTO ir_idempotency_key (key, principal_kind, subject_uid, model, method, \
         request_digest, status, response, binding_digest, policy_generation, resolution_epoch, \
         created_at, expires_at) SELECT gen_random_uuid(), 'user', 3, 'm', 'f', \
         decode(repeat('00', 32), 'hex'), 200, '\\x01', decode(repeat('00', 32), 'hex'), 0, 1, \
         now() - interval '2 days', now() - interval '1 day' FROM generate_series(1, 20); \
         INSERT INTO ir_idempotency_key (key, principal_kind, subject_uid, model, method, \
         request_digest, status, response, binding_digest, policy_generation, resolution_epoch, \
         created_at, expires_at) SELECT gen_random_uuid(), 'user', 3, 'm', 'f', \
         decode(repeat('00', 32), 'hex'), 200, '\\x01', decode(repeat('00', 32), 'hex'), 0, 1, \
         now() - interval '2 hours', now() + interval '1 day' FROM generate_series(1, 3)",
    )
    .await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-0000000000c1");
    ok(&call_keyed(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "a"}"#,
        alice(),
        &k,
    )
    .await);
    assert_eq!(
        count(&h, "ir_idempotency_key WHERE expires_at <= now()").await,
        20 - erp_rpc::MAINTENANCE_BATCH,
        "one bounded batch of expired rows deleted"
    );
    assert_eq!(
        count(&h, "ir_idempotency_key WHERE subject_uid = 3 AND response IS NOT NULL AND expires_at > now()").await,
        0,
        "responses past the replay window dropped"
    );
    // over the cap: executed and deduplicated, but not replayed
    let big = key("018f3a1c-7b2d-7c4e-9a10-0000000000c2");
    let body = r#"{"name": "big", "size": 2000}"#;
    let first = call_keyed(&h, "/json/2/test_rpc.effects/bump_big", body, alice(), &big).await;
    ok(&first);
    let again = call_keyed(&h, "/json/2/test_rpc.effects/bump_big", body, alice(), &big).await;
    assert_error(
        &again,
        409,
        "erp.idempotency_result_expired",
        "the original call committed; its result is no longer available",
    );
    assert_eq!(count(&h, "test_rpc_effects WHERE name = 'big'").await, 1);
    h.cleanup().await.unwrap();
}

/// Only user principals call until; a scoped key's allowlist is checked before any
/// lookup.
#[tokio::test]
async fn caller_kinds_and_the_allowlist_before_lookup() {
    let h = harness().await;
    let system = h
        .orm()
        .env(erp_orm::SUPERUSER_ID, erp_orm::Context::new())
        .principal()
        .clone();
    let r = call(
        &h,
        "/json/2/test_rpc.effects/probe",
        "{}",
        erp_rpc::Caller::Principal(system),
    )
    .await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessDenied",
        "principals of kind 'system' are not accepted until a later release",
    );
    for (caller, kind) in [
        (erp_rpc::Caller::Public, "public"),
        (erp_rpc::Caller::Link, "link"),
    ] {
        let r = call(&h, "/json/2/test_rpc.effects/probe", "{}", caller).await;
        assert_error(
            &r,
            403,
            "erp.exceptions.AccessDenied",
            &format!("{kind} callers are not accepted until a later release"),
        );
        assert_eq!(r.attempts(), 0);
    }
    let bot = || {
        alice_key(scope(
            8,
            "effects bot",
            ModelAllow::named(["test_rpc.effects"]).unwrap(),
            false,
        ))
    };
    for (path, model, method) in [
        ("/json/2/res.nope/search", "res.nope", "search"),
        ("/json/2/res.users/_private", "res.users", "_private"),
    ] {
        let r = call(&h, path, "{}", bot()).await;
        let description = if model == "res.users" { "User" } else { model };
        assert_error(
            &r,
            403,
            "erp.exceptions.AccessError",
            &format!(
                "The API key 'effects bot' may not call '{method}' on '{description}' ({model}): \
                 outside its scope."
            ),
        );
        assert_eq!(
            r.attempts(),
            0,
            "no transaction for a name outside the scope"
        );
    }
    h.cleanup().await.unwrap();
}

/// A `read` call may not leave session state on the pooled connection.
#[tokio::test]
async fn read_calls_refuse_session_state_functions() {
    let h = harness().await;
    for function in ["set_config", "pg_advisory_lock", "pg_notify"] {
        let r = call(
            &h,
            "/json/2/test_rpc.effects/bad_session",
            &format!(r#"{{"function": "{function}"}}"#),
            alice(),
        )
        .await;
        assert_error(
            &r,
            500,
            "erp.programming",
            &format!(
                "'test_rpc.effects.bad_session' is declared effect = read but called \
                 {function}(), which a read-only transaction allows but whose effect outlives the \
                 call; declare effect = write (or wider)."
            ),
        );
    }
    h.cleanup().await.unwrap();
}

/// Lock errors carry the ORM's text, never PostgreSQL's (which names the relation).
#[tokio::test]
async fn lock_errors_do_not_name_relations() {
    let h = harness().await;
    let id = effect(&h, "held", 1).await;
    let mut holder = h.harness().begin().await.unwrap();
    holder
        .query(
            &format!("SELECT id FROM test_rpc_effects WHERE id = {id} FOR UPDATE"),
            &[],
        )
        .await
        .unwrap();
    for method in ["locked", "locked_raw"] {
        let r = call(
            &h,
            &format!("/json/2/test_rpc.effects/{method}"),
            &format!(r#"{{"ids": [{id}]}}"#),
            alice(),
        )
        .await;
        assert_error(
            &r,
            409,
            "erp.exceptions.LockError",
            "Cannot grab a lock on records",
        );
    }
    holder.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The request digest covers the context keys the transport carries.
#[tokio::test]
async fn carried_context_is_part_of_the_request_digest() {
    let h = harness().await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-0000000000c3");
    let keyed = |website: i64| {
        RpcCall::new(
            Route::call_kw("test_rpc.effects", "bump").unwrap(),
            br#"{"name": "w"}"#.to_vec(),
            alice(),
        )
        .carried(CarriedContext::call_kw([("website_id".to_owned(), Value::Int(website))]).unwrap())
        .idempotency_key(k.clone())
    };
    ok(&h.service().call(keyed(1)).await);
    assert!(h.service().call(keyed(1)).await.replayed());
    let other = h.service().call(keyed(2)).await;
    assert_error(
        &other,
        422,
        "erp.idempotency_mismatch",
        "Idempotency-Key 018f3a1c-7b2d-7c4e-9a10-0000000000c3 was already used for a different \
         request; use a new key.",
    );
    h.cleanup().await.unwrap();
}
