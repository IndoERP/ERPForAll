//! Transactional native issuance, expiry and authority boundaries.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_fields::{DateTime, ErrorKind};
use erp_orm::UserId;
use erp_security::auth::apikey::{ApiKeyHasher, NativeApiKey};
use erp_security::auth::apikey_store::{IssueRequest, KeyKind, KeyScope, issue};
use erp_security::auth::secret::DatabaseSecret;

fn hasher() -> ApiKeyHasher {
    ApiKeyHasher::derive(&DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef").unwrap())
        .unwrap()
}
async fn users(h: &erp_testkit::Harness) {
    let c = support::connect(h).await;
    assert_eq!(
        c.execute("UPDATE res_users SET active=true WHERE id=1", &[])
            .await
            .unwrap(),
        1
    );
    c.batch_execute("INSERT INTO res_users(id, name, login, active, share) VALUES (20,'Ann','ann',true, false), (21,'Old','old',false, false); INSERT INTO res_groups(id,name) VALUES(10,'Settings'); INSERT INTO ir_model_data(module,name,model,res_id) VALUES('base','group_system','res.groups',10); INSERT INTO res_groups_users_rel(uid,gid) VALUES(1,10); INSERT INTO ir_config_parameter(key,value) VALUES('base.enable_programmatic_api_keys','True')").await.unwrap();
}
fn request(expiry: Option<DateTime>) -> IssueRequest {
    IssueRequest::new(
        UserId::new(20).unwrap(),
        "sample",
        KeyKind::User,
        KeyScope::Rpc,
        expiry,
    )
    .unwrap()
}

#[tokio::test]
async fn issuance_accepts_restored_constraints() {
    let h = support::harness().await;
    users(&h).await;
    let c = support::connect(&h).await;
    for _ in 0..3 {
        support::restore_api_key_constraints(&c).await;
        let mut tx = h.begin().await.unwrap();
        let expiry = DateTime::from_micros(now(&mut tx).await.micros() + 3_600_000_000);
        let issued = issue(&mut tx, &hasher(), &request(Some(expiry)))
            .await
            .unwrap();
        assert!(issued.id().get() > 0);
        tx.rollback().await.unwrap();
    }
    c.batch_execute("ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_name; ALTER TABLE res_users_apikeys ADD CONSTRAINT apikey_name CHECK (TRUE)").await.unwrap();
    support::restore_api_key_constraints(&c).await;
    let mut tx = h.begin().await.unwrap();
    let err = issue(&mut tx, &hasher(), &request(None))
        .await
        .err()
        .unwrap();
    error(
        err,
        ErrorKind::Database,
        "a later release constraint apikey_name differs",
    );
    tx.rollback().await.unwrap();
}
async fn now(tx: &mut erp_orm::Transaction) -> DateTime {
    DateTime::from_micros(
        tx.query("SELECT (extract(epoch FROM now()) * 1000000)::int8", &[])
            .await
            .unwrap()[0]
            .get(0),
    )
}
fn error(e: erp_fields::Error, kind: ErrorKind, message: &str) {
    assert_eq!(e.kind(), kind);
    assert_eq!(e.to_string(), message);
}

#[tokio::test]
async fn persists_digest_and_rollback_stores_nothing() {
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let mut tx = h.begin().await.unwrap();
    let clock = now(&mut tx).await;
    let expiry = DateTime::from_micros(clock.micros() + 3_600_000_000);
    let issued = issue(&mut tx, &hash, &request(Some(expiry))).await.unwrap();
    let id = issued.id().get();
    let issued_kid = issued.kid();
    let issued_prefix = issued.prefix().to_owned();
    assert_eq!(issued.expiration_date(), Some(expiry));
    tx.commit().await.unwrap();
    let token = issued.into_secret();
    let parsed = NativeApiKey::parse(token.expose()).unwrap();
    let expected = hash.digest(&parsed);
    let c = support::connect(&h).await;
    let row = c.query_one("SELECT digest, digest_kid, prefix, expiration_date, legacy_hash FROM res_users_apikeys WHERE id=$1", &[&id]).await.unwrap();
    assert_eq!(row.get::<_, Vec<u8>>(0), expected.bytes());
    assert_eq!(row.get::<_, i32>(1), i32::from(expected.kid()));
    assert_eq!(row.get::<_, String>(2), parsed.display_prefix());
    assert_eq!(issued_prefix, parsed.display_prefix());
    assert_eq!(issued_kid, expected.kid());
    assert_eq!(row.get::<_, DateTime>(3), expiry);
    assert!(row.get::<_, Option<String>>(4).is_none());
    let row_json: String = c
        .query_one(
            "SELECT row_to_json(k)::text FROM res_users_apikeys k WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!row_json.contains(token.expose()));
    let mut tx = h.begin().await.unwrap();
    let pending = issue(&mut tx, &hash, &request(Some(expiry))).await.unwrap();
    let rolled_id = pending.id().get();
    tx.rollback().await.unwrap();
    assert_eq!(
        c.query_one(
            "SELECT count(*) FROM res_users_apikeys WHERE id=$1",
            &[&rolled_id]
        )
        .await
        .unwrap()
        .get::<_, i64>(0),
        0
    );
    c.execute("DELETE FROM res_users WHERE id=20", &[])
        .await
        .unwrap();
    assert_eq!(
        c.query_one("SELECT count(*) FROM res_users_apikeys", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
}

#[tokio::test]
async fn expiry_boundaries_include_implied_groups() {
    let h = support::harness().await;
    users(&h).await;
    let c = support::connect(&h).await;
    c.batch_execute("INSERT INTO res_groups(id,name,api_key_duration) VALUES (30,'Direct',0),(31,'Implied',0.5); INSERT INTO res_groups_implied_rel(gid,hid) VALUES(30,31); INSERT INTO res_groups_users_rel(uid,gid) VALUES(20,30)").await.unwrap();
    let hash = hasher();
    let mut tx = h.begin().await.unwrap();
    let clock = now(&mut tx).await;
    error(
        issue(&mut tx, &hash, &request(None)).await.err().unwrap(),
        ErrorKind::Validation,
        "The API key must have an expiration date",
    );
    error(
        issue(
            &mut tx,
            &hash,
            &request(Some(DateTime::from_micros(clock.micros() - 1))),
        )
        .await
        .err()
        .unwrap(),
        ErrorKind::Validation,
        "You cannot set an expiration date in the past.",
    );
    issue(
        &mut tx,
        &hash,
        &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
    )
    .await
    .unwrap();
    error(
        issue(&mut tx, &hash, &request(Some(clock)))
            .await
            .err()
            .unwrap(),
        ErrorKind::Validation,
        "You cannot set an expiration date in the past.",
    );
    let max = DateTime::from_micros(clock.micros() + 43_200_000_000);
    issue(&mut tx, &hash, &request(Some(max))).await.unwrap();
    error(
        issue(
            &mut tx,
            &hash,
            &request(Some(DateTime::from_micros(max.micros() + 1))),
        )
        .await
        .err()
        .unwrap(),
        ErrorKind::Validation,
        "You cannot exceed 0.5 days.",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn validation_and_missing_subject() {
    for (kind, name) in [(KeyKind::Agent, "agent"), (KeyKind::Service, "service")] {
        error(
            IssueRequest::new(UserId::new(20).unwrap(), "label", kind, KeyScope::Rpc, None)
                .err()
                .unwrap(),
            ErrorKind::AccessDenied,
            &format!("API keys of kind '{name}' are not accepted until a later release"),
        );
    }
    error(
        request_bad_name("  "),
        ErrorKind::Validation,
        "name must be nonempty after trimming and at most 128 bytes",
    );
    error(
        request_bad_name(&"x".repeat(129)),
        ErrorKind::Validation,
        "name must be nonempty after trimming and at most 128 bytes",
    );
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    for id in [21, 999] {
        let req = IssueRequest::new(
            UserId::new(id).unwrap(),
            "label",
            KeyKind::User,
            KeyScope::Rpc,
            None,
        )
        .unwrap();
        let mut tx = h.begin().await.unwrap();
        error(
            issue(&mut tx, &hash, &req).await.err().unwrap(),
            ErrorKind::Access,
            "API key subject is missing or inactive",
        );
        tx.rollback().await.unwrap();
    }
}
fn request_bad_name(name: &str) -> erp_fields::Error {
    IssueRequest::new(
        UserId::new(20).unwrap(),
        name,
        KeyKind::User,
        KeyScope::Rpc,
        None,
    )
    .err()
    .unwrap()
}

#[tokio::test]
async fn last_slot_race_retries_whole_transaction() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let h = support::harness().await;
    users(&h).await;
    support::connect(&h).await.batch_execute("INSERT INTO ir_config_parameter(key,value) VALUES('base.programmatic_api_keys_limit','1')").await.unwrap();
    let barrier = tokio::sync::Barrier::new(2);
    let a = AtomicUsize::new(0);
    let b = AtomicUsize::new(0);
    let hash = hasher();
    let expiry = DateTime::parse_odoo("2090-01-01 00:00:00").unwrap();
    // Subject is a settings user, so expiry is unconstrained; caller flags cannot grant it.
    support::connect(&h)
        .await
        .batch_execute("INSERT INTO res_groups_users_rel(uid,gid) VALUES(20,10)")
        .await
        .unwrap();
    let req = request(Some(expiry));
    let run = async |attempts: &AtomicUsize| {
        h.orm()
            .run(h.principal(), erp_orm::Context::new(), async |tx| {
                let first = attempts.fetch_add(1, Ordering::SeqCst) == 0;
                now(tx).await;
                if first {
                    barrier.wait().await;
                }
                issue(tx, &hash, &req).await
            })
            .await
    };
    let (one, two) = tokio::join!(run(&a), run(&b));
    let loser = match (one, two) {
        (Ok(_), Err(e)) | (Err(e), Ok(_)) => e,
        _ => panic!("race must yield one committed key and one refusal"),
    };
    error(
        loser,
        ErrorKind::UserError,
        "Limit of 1 API keys is reached for programmatic creation",
    );
    let mut attempts = [a.load(Ordering::SeqCst), b.load(Ordering::SeqCst)];
    attempts.sort();
    assert_eq!(attempts, [1, 2]);
    assert_eq!(
        support::connect(&h)
            .await
            .query_one("SELECT count(*) FROM res_users_apikeys", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
}

#[tokio::test]
async fn runtime_schema_guard_fails_closed() {
    for (ddl, expected) in [
        (
            "ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_digest_length",
            "a later release constraint apikey_digest_length missing; run schema sync",
        ),
        (
            "ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_digest_length; ALTER TABLE res_users_apikeys ADD CONSTRAINT apikey_digest_length CHECK(TRUE)",
            "a later release constraint apikey_digest_length differs",
        ),
        (
            "ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_kind; ALTER TABLE res_users_apikeys ADD CONSTRAINT apikey_kind CHECK (kind IN ('user', 'agent', 'service', 'other'))",
            "a later release constraint apikey_kind differs",
        ),
        (
            "ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_digest_length; ALTER TABLE res_users_apikeys ADD CONSTRAINT apikey_digest_length CHECK (digest IS NULL OR octet_length(digest) = 32) NOT VALID",
            "a later release constraint apikey_digest_length differs",
        ),
        (
            "ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_digest_unique; ALTER TABLE res_users_apikeys ADD CONSTRAINT apikey_digest_unique UNIQUE (digest) DEFERRABLE",
            "a later release constraint apikey_digest_unique differs",
        ),
    ] {
        let h = support::harness().await;
        users(&h).await;
        support::connect(&h).await.batch_execute(ddl).await.unwrap();
        let mut tx = h.begin().await.unwrap();
        let err = issue(&mut tx, &hasher(), &request(None))
            .await
            .err()
            .unwrap();
        assert_eq!(err.kind(), ErrorKind::Database);
        assert_eq!(err.to_string(), expected);
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn rollback_and_tenant_separation() {
    let first = support::harness().await;
    users(&first).await;
    let second = support::harness().await;
    users(&second).await;
    for h in [&first, &second] {
        support::connect(h).await.batch_execute("INSERT INTO res_groups_users_rel(uid,gid) VALUES(20,10); INSERT INTO ir_config_parameter(key,value) VALUES('base.programmatic_api_keys_limit','1')").await.unwrap();
    }
    let hash = hasher();
    let req = request(None);
    let mut pending = first.begin().await.unwrap();
    issue(&mut pending, &hash, &req).await.unwrap();
    // The other tenant can issue while the first tenant's fence is still held.
    let other = second
        .run(async |tx| issue(tx, &hash, &req).await)
        .await
        .unwrap();
    assert!(other.id().get() > 0);
    let started = tokio::sync::Notify::new();
    let waiter = first.run(async |tx| {
        now(tx).await;
        started.notify_one();
        issue(tx, &hash, &req).await
    });
    let release = async {
        started.notified().await;
        pending.rollback().await.unwrap();
    };
    let (after, _) = tokio::join!(waiter, release);
    assert!(after.unwrap().id().get() > 0);
    for h in [&first, &second] {
        assert_eq!(
            support::connect(h)
                .await
                .query_one("SELECT count(*) FROM res_users_apikeys", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
    }
}

#[tokio::test]
async fn self_issuance_and_delegation() {
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let mut tx = h.begin_as(20).await.unwrap();
    let clock = now(&mut tx).await;
    let expiry = DateTime::from_micros(clock.micros() + 86_400_000_000);
    issue(&mut tx, &hash, &request(Some(expiry))).await.unwrap();
    let req = IssueRequest::new(
        UserId::new(1).unwrap(),
        "no",
        KeyKind::User,
        KeyScope::Rpc,
        Some(expiry),
    )
    .unwrap();
    error(
        issue(&mut tx, &hash, &req).await.err().unwrap(),
        ErrorKind::Access,
        "API key delegation requires base.group_system",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn active_count_includes_equal_and_persistent_not_expired() {
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let c = support::connect(&h).await;
    c.batch_execute("INSERT INTO res_groups_users_rel(uid,gid) VALUES(20,10)")
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let clock = now(&mut tx).await;
    let equal = issue(
        &mut tx,
        &hash,
        &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
    )
    .await
    .unwrap();
    issue(&mut tx, &hash, &request(None)).await.unwrap();
    tx.execute(
        "UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' WHERE id=$1",
        &[&equal.id().get()],
    )
    .await
    .unwrap();
    // The default cap allows both. Install a smaller explicit cap in this tx.
    tx.execute(
        "INSERT INTO ir_config_parameter(key,value) VALUES('base.programmatic_api_keys_limit','2')",
        &[],
    )
    .await
    .unwrap();
    error(
        issue(&mut tx, &hash, &request(None)).await.err().unwrap(),
        ErrorKind::UserError,
        "Limit of 2 API keys is reached for programmatic creation",
    );
    tx.execute("UPDATE res_users_apikeys SET expiration_date=expiration_date-interval '1 microsecond' WHERE id=$1",&[&equal.id().get()]).await.unwrap();
    issue(&mut tx, &hash, &request(None)).await.unwrap();
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn duration_fallback_and_corrupt_policy_refuse() {
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let mut tx = h.begin().await.unwrap();
    let clock = now(&mut tx).await;
    let max = DateTime::from_micros(clock.micros() + 86_400_000_000);
    issue(&mut tx, &hash, &request(Some(max))).await.unwrap();
    error(
        issue(
            &mut tx,
            &hash,
            &request(Some(DateTime::from_micros(max.micros() + 1))),
        )
        .await
        .err()
        .unwrap(),
        ErrorKind::Validation,
        "You cannot exceed 1.0 days.",
    );
    tx.rollback().await.unwrap();
    let c = support::connect(&h).await;
    c.batch_execute("ALTER TABLE res_groups DROP CONSTRAINT res_groups_check_api_key_duration; INSERT INTO res_groups(id,name,api_key_duration) VALUES(30,'Broken',0); INSERT INTO res_groups_users_rel(uid,gid) VALUES(20,30)").await.unwrap();
    for value in ["NaN", "Infinity", "-Infinity", "-1"] {
        c.execute(
            "UPDATE res_groups SET api_key_duration=$1::text::float8 WHERE id=30",
            &[&value],
        )
        .await
        .unwrap();
        let mut tx = h.begin().await.unwrap();
        let clock = now(&mut tx).await;
        error(
            issue(
                &mut tx,
                &hash,
                &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
            )
            .await
            .err()
            .unwrap(),
            ErrorKind::Validation,
            "api_key_duration must be finite and nonnegative",
        );
        tx.rollback().await.unwrap();
    }
    c.execute(
        "UPDATE res_groups SET api_key_duration=1e300 WHERE id=30",
        &[],
    )
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let clock = now(&mut tx).await;
    error(
        issue(
            &mut tx,
            &hash,
            &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
        )
        .await
        .err()
        .unwrap(),
        ErrorKind::Validation,
        "api_key_duration exceeds timestamp range",
    );
}

#[test]
fn typed_admission_is_closed() {
    error(
        KeyKind::parse("unknown").err().unwrap(),
        ErrorKind::Validation,
        "kind must be user, agent or service",
    );
    error(
        KeyScope::parse("unknown").err().unwrap(),
        ErrorKind::Validation,
        "scope must be rpc or mcp",
    );
    error(
        IssueRequest::new(
            UserId::new(20).unwrap(),
            "mcp",
            KeyKind::User,
            KeyScope::Mcp,
            None,
        )
        .err()
        .unwrap(),
        ErrorKind::Validation,
        "scope mcp requires kind agent and capabilities none",
    );
    assert!(
        IssueRequest::new(
            UserId::new(20).unwrap(),
            &"é".repeat(64),
            KeyKind::User,
            KeyScope::Rpc,
            None
        )
        .is_ok()
    );
    error(
        request_bad_name(&"é".repeat(65)),
        ErrorKind::Validation,
        "name must be nonempty after trimming and at most 128 bytes",
    );
}

#[tokio::test]
async fn configured_cap_is_strict() {
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let c = support::connect(&h).await;
    c.batch_execute("INSERT INTO ir_config_parameter(key,value) VALUES('base.programmatic_api_keys_limit','bad')").await.unwrap();
    for value in ["bad", "0", "-1", "+1", " 1", "4294967296", ""] {
        c.execute(
            "UPDATE ir_config_parameter SET value=$1 WHERE key='base.programmatic_api_keys_limit'",
            &[&value],
        )
        .await
        .unwrap();
        let mut tx = h.begin().await.unwrap();
        error(
            issue(&mut tx, &hash, &request(None)).await.err().unwrap(),
            ErrorKind::Validation,
            "base.programmatic_api_keys_limit must be a positive 32-bit integer",
        );
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn exhausted_retry_surfaces_concurrency() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let h = erp_testkit::Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .identify_transactions()
        .config(erp_orm::OrmConfig {
            max_attempts: 1,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    users(&h).await;
    let hash = hasher();
    let barrier = tokio::sync::Barrier::new(2);
    let attempts = AtomicUsize::new(0);
    let work = async |tx: &mut erp_orm::Transaction| {
        attempts.fetch_add(1, Ordering::SeqCst);
        let clock = now(tx).await;
        barrier.wait().await;
        issue(
            tx,
            &hash,
            &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
        )
        .await
    };
    let (a, b) = tokio::join!(
        h.orm()
            .run(h.principal(), erp_orm::Context::new(), async |tx| work(tx)
                .await),
        h.orm()
            .run(h.principal(), erp_orm::Context::new(), async |tx| work(tx)
                .await)
    );
    let loser = match (a, b) {
        (Ok(_), Err(e)) | (Err(e), Ok(_)) => e,
        _ => panic!("one contender must commit"),
    };
    assert_eq!(loser.kind(), ErrorKind::Concurrency);
    assert_eq!(loser.sqlstate(), Some("40001"));
    assert_eq!(
        loser.to_string(),
        "could not serialize access due to concurrent update"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        support::connect(&h)
            .await
            .query_one("SELECT count(*) FROM res_users_apikeys", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
}

#[tokio::test]
async fn connection_loss_rolls_back_pending_key() {
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let mut lost = h.begin().await.unwrap();
    let clock = now(&mut lost).await;
    let pending = issue(
        &mut lost,
        &hash,
        &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
    )
    .await
    .unwrap();
    let pid: i32 = lost.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    let c = support::connect(&h).await;
    assert!(
        c.query_one("SELECT pg_terminate_backend($1)", &[&pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let lost_error = lost.commit().await.unwrap_err();
    // Issuance is audited, so the commit now fails first at the audit INSERT: a typed
    // (insertion)` Internal error carrying the server's termination SQLSTATE or the
    // closed connection, and the key change still rolls back.
    assert_eq!(lost_error.kind(), ErrorKind::Internal, "{lost_error}");
    // Driver-close versus server FATAL depends on delivery timing after SIGTERM;
    // pin the typed failure and stable cause rather than transport suffix.
    let text = lost_error.to_string();
    assert!(text.starts_with("a later release (insertion)"), "{text}");
    // A third delivery: the driver reports the failed INSERT before it has either the server's FATAL
    // or its own close text, and the error carries no SQLSTATE (seen on the staging run of 2026-11-11).
    assert!(
        text.contains("connection closed")
            || text.contains("57P01")
            || text.contains("SQLSTATE none"),
        "{text}"
    );
    let mut replacement = h.begin().await.unwrap();
    let clock = now(&mut replacement).await;
    let committed = issue(
        &mut replacement,
        &hash,
        &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
    )
    .await
    .unwrap();
    replacement.commit().await.unwrap();
    assert_ne!(pending.id().get(), committed.id().get());
    assert_eq!(
        c.query_one(
            "SELECT count(*) FROM res_users_apikeys WHERE id=$1",
            &[&pending.id().get()]
        )
        .await
        .unwrap()
        .get::<_, i64>(0),
        0
    );
}

#[tokio::test]
async fn restricted_principal_cannot_escape_scope() {
    use erp_orm::{ApiKeyId, BeginOptions, PrincipalBuilder, Scope};
    let h = support::harness().await;
    users(&h).await;
    let scope = Scope::builder(ApiKeyId::new(1).unwrap(), "restricted")
        .models(erp_orm::ModelAllow::All)
        .methods(erp_orm::MethodAllow::All)
        .read_only(true)
        .build()
        .unwrap();
    let principal = PrincipalBuilder::user(UserId::new(20).unwrap())
        .scope(scope)
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap();
    error(
        issue(&mut tx, &hasher(), &request(None))
            .await
            .err()
            .unwrap(),
        ErrorKind::Access,
        "Restricted principals cannot issue unrestricted API keys",
    );
}

#[tokio::test]
async fn storage_constraints_reject_corrupt_rows() {
    use erp_testkit::tokio_postgres::error::SqlState;
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let mut tx = h.begin().await.unwrap();
    let clock = now(&mut tx).await;
    let issued = issue(
        &mut tx,
        &hash,
        &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
    )
    .await
    .unwrap();
    let another = issue(
        &mut tx,
        &hash,
        &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let c = support::connect(&h).await;
    for (sql, constraint) in [
        (
            "UPDATE res_users_apikeys SET digest='\\x00'::bytea WHERE id=$1",
            "apikey_digest_length",
        ),
        (
            "UPDATE res_users_apikeys SET digest=NULL WHERE id=$1",
            "apikey_representation",
        ),
        (
            "UPDATE res_users_apikeys SET kind='unknown' WHERE id=$1",
            "apikey_kind",
        ),
        (
            "UPDATE res_users_apikeys SET scope='unknown' WHERE id=$1",
            "apikey_scope",
        ),
        (
            "UPDATE res_users_apikeys SET kind='agent',capabilities='unknown' WHERE id=$1",
            "apikey_capabilities",
        ),
        (
            "UPDATE res_users_apikeys SET scope='mcp' WHERE id=$1",
            "apikey_mcp",
        ),
        (
            "UPDATE res_users_apikeys SET models='[]'::jsonb WHERE id=$1",
            "apikey_user",
        ),
        (
            "UPDATE res_users_apikeys SET kind='agent',methods='{}'::jsonb WHERE id=$1",
            "apikey_methods",
        ),
        (
            "UPDATE res_users_apikeys SET digest_kid=0 WHERE id=$1",
            "apikey_kid",
        ),
        (
            "UPDATE res_users_apikeys SET prefix='short' WHERE id=$1",
            "apikey_prefix",
        ),
        (
            "UPDATE res_users_apikeys SET name='   ' WHERE id=$1",
            "apikey_name",
        ),
    ] {
        let e = c.execute(sql, &[&issued.id().get()]).await.unwrap_err();
        assert_eq!(e.code(), Some(&SqlState::CHECK_VIOLATION), "{e}: {sql}");
        assert_eq!(e.as_db_error().unwrap().constraint(), Some(constraint));
        assert_eq!(
            e.as_db_error().unwrap().message(),
            format!(
                "new row for relation \"res_users_apikeys\" violates check constraint \"{constraint}\""
            )
        );
    }
    let e=c.execute("UPDATE res_users_apikeys SET digest=(SELECT digest FROM res_users_apikeys WHERE id=$1) WHERE id=$2",&[&issued.id().get(),&another.id().get()]).await.unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::UNIQUE_VIOLATION));
    assert_eq!(
        e.as_db_error().unwrap().message(),
        "duplicate key value violates unique constraint \"apikey_digest_unique\""
    );
    c.execute("DELETE FROM res_users WHERE id=1", &[])
        .await
        .unwrap();
    assert_eq!(
        c.query_one("SELECT count(*) FROM res_users_apikeys", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
}

#[tokio::test]
async fn default_limit_and_programmatic_gate() {
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let c = support::connect(&h).await;
    c.execute(
        "DELETE FROM ir_config_parameter WHERE key='base.enable_programmatic_api_keys'",
        &[],
    )
    .await
    .unwrap();
    let mut tx = h.begin_as(20).await.unwrap();
    let clock = now(&mut tx).await;
    let req = request(Some(DateTime::from_micros(clock.micros() + 3_600_000_000)));
    error(
        issue(&mut tx, &hash, &req).await.err().unwrap(),
        ErrorKind::UserError,
        "Programmatic API keys are not enabled",
    );
    tx.rollback().await.unwrap();
    c.execute("INSERT INTO ir_config_parameter(key,value) VALUES('base.enable_programmatic_api_keys','maybe')",&[]).await.unwrap();
    let mut tx = h.begin_as(20).await.unwrap();
    error(
        issue(&mut tx, &hash, &req).await.err().unwrap(),
        ErrorKind::Validation,
        "base.enable_programmatic_api_keys must be a boolean",
    );
    tx.rollback().await.unwrap();
    c.execute(
        "UPDATE ir_config_parameter SET value='True' WHERE key='base.enable_programmatic_api_keys'",
        &[],
    )
    .await
    .unwrap();
    let mut tx = h.begin_as(20).await.unwrap();
    for _ in 0..10 {
        issue(&mut tx, &hash, &req).await.unwrap();
    }
    error(
        issue(&mut tx, &hash, &req).await.err().unwrap(),
        ErrorKind::UserError,
        "Limit of 10 API keys is reached for programmatic creation",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn core_without_group_is_not_delegation_authority() {
    let h = support::harness().await;
    users(&h).await;
    support::connect(&h)
        .await
        .execute("DELETE FROM res_groups_users_rel WHERE uid=1", &[])
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let clock = now(&mut tx).await;
    error(
        issue(
            &mut tx,
            &hasher(),
            &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
        )
        .await
        .err()
        .unwrap(),
        ErrorKind::Access,
        "API key delegation requires base.group_system",
    );
}

#[tokio::test]
async fn inactive_or_missing_creator_is_refused() {
    let h = support::harness().await;
    users(&h).await;
    for id in [21, 999] {
        let mut tx = h.begin_as(id).await.unwrap();
        let clock = now(&mut tx).await;
        error(
            issue(
                &mut tx,
                &hasher(),
                &request(Some(DateTime::from_micros(clock.micros() + 1_000_000))),
            )
            .await
            .err()
            .unwrap(),
            ErrorKind::Access,
            "API key creator is missing or inactive",
        );
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn runtime_private_column_admission() {
    for (ddl, message) in [
        (
            "DROP TABLE res_users_apikeys",
            "a later release requires res_users_apikeys table",
        ),
        (
            "ALTER TABLE res_users_apikeys DROP COLUMN digest CASCADE",
            "a later release requires res_users_apikeys.digest bytea",
        ),
        (
            "ALTER TABLE res_users_apikeys DROP COLUMN digest CASCADE; ALTER TABLE res_users_apikeys ADD COLUMN digest text",
            "a later release requires res_users_apikeys.digest bytea",
        ),
    ] {
        let h = support::harness().await;
        users(&h).await;
        support::connect(&h).await.batch_execute(ddl).await.unwrap();
        let mut tx = h.begin().await.unwrap();
        let e = issue(&mut tx, &hasher(), &request(None))
            .await
            .err()
            .unwrap();
        assert_eq!(e.kind(), ErrorKind::Database);
        assert_eq!(e.to_string(), message);
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn null_and_oversized_settings_refuse() {
    let h = support::harness().await;
    users(&h).await;
    let hash = hasher();
    let c = support::connect(&h).await;
    for (key, message) in [
        (
            "base.programmatic_api_keys_limit",
            "base.programmatic_api_keys_limit must be a positive 32-bit integer",
        ),
        (
            "base.enable_programmatic_api_keys",
            "base.enable_programmatic_api_keys must be a boolean",
        ),
    ] {
        c.execute("INSERT INTO ir_config_parameter(key,value) VALUES($1,NULL) ON CONFLICT(key) DO UPDATE SET value=NULL",&[&key]).await.unwrap();
        let mut tx = h.begin().await.unwrap();
        error(
            issue(&mut tx, &hash, &request(None)).await.err().unwrap(),
            ErrorKind::Validation,
            message,
        );
        tx.rollback().await.unwrap();
        let huge = "True".to_owned() + &" ".repeat(4096);
        c.execute(
            "UPDATE ir_config_parameter SET value=$1 WHERE key=$2",
            &[&huge, &key],
        )
        .await
        .unwrap();
        let mut tx = h.begin().await.unwrap();
        error(
            issue(&mut tx, &hash, &request(None)).await.err().unwrap(),
            ErrorKind::Validation,
            message,
        );
        tx.rollback().await.unwrap();
        c.execute("DELETE FROM ir_config_parameter WHERE key=$1", &[&key])
            .await
            .unwrap();
    }
}
