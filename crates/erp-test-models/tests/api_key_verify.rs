//! Native API-key request verification and cookie precedence.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod auth_support;
use auth_support::*;
use erp_fields::DateTime;
use erp_orm::{Actor, ApiKeyId, UserId};
use erp_sdk::{ErrorKind, Transaction};
use erp_security::auth::apikey::ApiKeyHasher;
use erp_security::auth::apikey_store::{IssueRequest, IssuedSecret, KeyKind, KeyScope, issue};
use erp_security::auth::secret::DatabaseSecret;
use erp_security::auth::{ApiKeyRequest, CredentialOptions};

fn request(raw: &str) -> ApiKeyRequest<'_> {
    ApiKeyRequest::new(raw, KeyScope::Rpc, "192.0.2.19".parse().unwrap())
}
async fn issued(w: &World, uid: i64) -> (ApiKeyId, IssuedSecret) {
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    // a round-trip-counting harness leaves the audit store check to the first audited call (the
    // RPC service checks lazily); issuance is audited, so this fixture checks it directly
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
        "native verifier",
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
async fn node_param(auth: &erp_security::auth::Auth, key: &str, value: &str) {
    let mut tx = auth.orm().begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("ir.config_parameter").unwrap();
    let _: String = records
        .call(&mut tx, "set_str", (key.to_owned(), Some(value.to_owned())))
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
async fn sql(w: &World, text: &str) {
    let mut tx = w.h.begin().await.unwrap();
    tx.batch(text).await.unwrap();
    tx.commit().await.unwrap();
}
async fn refuse(w: &World, req: ApiKeyRequest<'_>, kind: ErrorKind, message: &str) {
    let e = w
        .auth
        .begin_api_key(req, CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!((e.kind(), e.message()), (kind, message));
}
async fn accepted(w: &World, req: ApiKeyRequest<'_>) -> Transaction {
    w.auth
        .begin_api_key(req, CredentialOptions::new())
        .await
        .unwrap()
        .tx
}

#[tokio::test]
async fn native_key_resolves_subject_scope_and_actor() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (id, key) = issued(&w, uid).await;
    let mut tx = accepted(&w, request(key.expose()).uid(UserId::new(uid).unwrap())).await;
    let p = tx.env().principal().clone();
    assert_eq!(p.subject().get(), uid);
    assert_eq!(p.scope().unwrap().key(), id);
    assert_eq!(p.scope().unwrap().name(), "native verifier");
    assert!(matches!(p.actor(),Some(Actor::ApiKey{id: actor_id,..}) if *actor_id==id));
    tx.rollback().await.unwrap();
    refuse(
        &w,
        request(key.expose()).uid(UserId::new(uid + 1).unwrap()),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    refuse(
        &w,
        ApiKeyRequest::new(key.expose(), KeyScope::Mcp, "192.0.2.20".parse().unwrap()),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    refuse(&w, request(""), ErrorKind::AccessDenied, "Invalid apikey").await;
    sql(
        &w,
        &format!("UPDATE res_users SET active=false WHERE id={uid}"),
    )
    .await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
}

#[tokio::test]
async fn committed_expiry_and_revocation_are_seen_by_next_request() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (id, key) = issued(&w, uid).await;
    let mut tx = accepted(&w, request(key.expose())).await;
    tx.rollback().await.unwrap();
    sql(&w,&format!("UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' - interval '1 second' WHERE id={}",id.get())).await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    sql(
        &w,
        &format!(
            "UPDATE res_users_apikeys SET expiration_date=NULL WHERE id={}",
            id.get()
        ),
    )
    .await;
    let mut tx = accepted(&w, request(key.expose())).await;
    tx.rollback().await.unwrap();
    sql(
        &w,
        &format!("DELETE FROM res_users_apikeys WHERE id={}", id.get()),
    )
    .await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
}

#[tokio::test]
async fn cookie_never_supplies_key_authority_and_stale_cookie_is_not_mutated() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    user_with_password(&w.h, "other", "pw").await;
    let (_, key) = issued(&w, uid).await;
    let session = login(&w.auth, "bearer", "pw").await.unwrap();
    let same_cookie = cookie(&session);
    let mut begin = w
        .auth
        .begin_api_key(
            request(key.expose()).session(&same_cookie),
            CredentialOptions::new(),
        )
        .await
        .unwrap();
    assert!(!begin.drop_cookie);
    assert!(begin.tx.env().principal().scope().is_some());
    begin.tx.rollback().await.unwrap();
    refuse(
        &w,
        request("bad").session(&same_cookie),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    let other = login(&w.auth, "other", "pw").await.unwrap();
    let other_cookie = cookie(&other);
    refuse(
        &w,
        request(key.expose()).session(&other_cookie),
        ErrorKind::Access,
        "Session user does not match the used apikey.",
    )
    .await;
    sql(
        &w,
        "UPDATE erp_session SET expires_at=now()-interval '1 second'",
    )
    .await;
    let before = count(&w.h, "erp_session").await;
    let mut begin = w
        .auth
        .begin_api_key(
            request(key.expose()).session(&other_cookie),
            CredentialOptions::new(),
        )
        .await
        .unwrap();
    assert!(begin.drop_cookie);
    assert_eq!(begin.tx.env().uid(), uid);
    begin.tx.rollback().await.unwrap();
    assert_eq!(count(&w.h, "erp_session").await, before);
}

#[tokio::test]
async fn legacy_refusal_and_request_debug_do_not_expose_submitted_text() {
    let w = world().await;
    let legacy = "0123456789abcdef0123456789abcdef01234567";
    let req = request(legacy);
    assert!(!format!("{req:?}").contains(legacy));
    refuse(
        &w,
        req,
        ErrorKind::AccessDenied,
        "Imported legacy API keys are not accepted until a later release",
    )
    .await;
}

#[tokio::test]
async fn address_failures_are_shared_between_nodes_without_login_rows() {
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
    set_param(&w.h, "base.login_cooldown_after", "2").await;
    let other = node_with(
        &w.h,
        4,
        own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
    )
    .await;
    for auth in [&w.auth, &other] {
        let e = auth
            .begin_api_key(request("bad"), CredentialOptions::new())
            .await
            .err()
            .unwrap();
        assert_eq!(
            (e.kind(), e.message()),
            (ErrorKind::AccessDenied, "Invalid apikey")
        );
    }
    let e = other
        .begin_api_key(request("bad"), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::AccessDenied,
            erp_security::auth::COOLDOWN_MESSAGE
        )
    );
    let source = std::error::Error::source(&e)
        .unwrap()
        .downcast_ref::<erp_security::auth::ApiKeyCooldown>()
        .unwrap();
    assert!(source.retry_after() > std::time::Duration::ZERO);
    assert_eq!(
        count(&w.h, "erp_login_throttle WHERE kind='address'").await,
        1
    );
    assert_eq!(
        count(&w.h, "erp_login_throttle WHERE kind IN ('login','trusted')").await,
        0
    );
    // A separate tenant does not inherit this address's failures.
    let isolated = world().await;
    refuse(
        &isolated,
        request("bad"),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
}

#[tokio::test]
async fn another_node_observes_revocation_and_secret_rotation() {
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
    let mut begun = other
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .unwrap();
    begun.tx.rollback().await.unwrap();
    sql(
        &w,
        &format!("DELETE FROM res_users_apikeys WHERE id={}", id.get()),
    )
    .await;
    let e = other
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
    let (_, key) = issued(&w, uid).await;
    let mut begun = other
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .unwrap();
    begun.tx.rollback().await.unwrap();
    let writer = node_with(
        &w.h,
        4,
        own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
    )
    .await;
    node_param(
        &writer,
        "database.secret",
        "fedcba9876543210fedcba9876543210",
    )
    .await;
    let e = other
        .begin_api_key(request(key.expose()), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
}

#[tokio::test]
async fn contradictory_authority_and_unsupported_kinds_fail_closed() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (id, key) = issued(&w, uid).await;
    sql(
        &w,
        &format!(
            "UPDATE res_users_apikeys SET kind='agent',capabilities='none' WHERE id={}",
            id.get()
        ),
    )
    .await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::AccessDenied,
        "API keys of kind 'agent' are not accepted until a later release",
    )
    .await;
    sql(
        &w,
        &format!(
            "UPDATE res_users_apikeys SET kind='service' WHERE id={}",
            id.get()
        ),
    )
    .await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::AccessDenied,
        "API keys of kind 'service' are not accepted until a later release",
    )
    .await;
    sql(&w,&format!("ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_user; UPDATE res_users_apikeys SET kind='user',models='[\"res.users\"]' WHERE id={}",id.get())).await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::Database,
        "Invalid stored API key authority",
    )
    .await;
    sql(&w,&format!("ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_name; ALTER TABLE res_users_apikeys ALTER COLUMN name TYPE text; UPDATE res_users_apikeys SET models=NULL,capabilities='subject',name=repeat('x',129) WHERE id={}",id.get())).await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::Database,
        "Invalid stored API key authority",
    )
    .await;
}

#[tokio::test]
async fn expiration_equal_to_opening_transaction_clock_is_accepted() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (_, key) = issued(&w, uid).await;
    // A view projects expiry from the check transaction's own now(), avoiding wall-clock
    // races and exercising the real verifier SQL rather than reproducing its comparison.
    sql(&w,"ALTER TABLE res_users_apikeys RENAME TO key_expiry_fixture; CREATE VIEW res_users_apikeys AS SELECT (jsonb_populate_record(NULL::key_expiry_fixture,to_jsonb(k)||jsonb_build_object('expiration_date',now() AT TIME ZONE 'utc'))).* FROM key_expiry_fixture k").await;
    let mut tx = accepted(&w, request(key.expose())).await;
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn stale_cooldown_state_reloads_before_refusing_after_rotation_or_policy_change() {
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
    set_param(&w.h, "base.login_cooldown_after", "1").await;
    let other = node_with(
        &w.h,
        4,
        own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
    )
    .await;
    async fn check(auth: &erp_security::auth::Auth) -> erp_sdk::Error {
        auth.begin_api_key(request("bad"), CredentialOptions::new())
            .await
            .err()
            .unwrap()
    }
    let e = check(&other).await;
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
    let e = check(&other).await;
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::AccessDenied,
            erp_security::auth::COOLDOWN_MESSAGE
        )
    );
    let writer = node_with(
        &w.h,
        4,
        own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
    )
    .await;
    node_param(
        &writer,
        "database.secret",
        "fedcba9876543210fedcba9876543210",
    )
    .await;
    let e = check(&other).await;
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
    sql(
        &w,
        "UPDATE erp_login_throttle SET last_failure_at=now()-interval '2 seconds'",
    )
    .await;
    // The warmed node still has the old 60-second cooldown until the opening signal is read.
    node_param(&writer, "base.login_cooldown_duration", "1").await;
    let e = check(&other).await;
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
}

/// The opening-flight protocol contract includes key, cookie and shared-throttle evidence.
#[tokio::test]
async fn key_cookie_and_address_evidence_share_one_opening_flight() {
    let w = world_with(
        builder().count_round_trips().build().await.unwrap(),
        erp_security::auth::AuthConfig::DEFAULT,
        erp_security::auth::HashPoolConfig::DEFAULT,
    )
    .await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (_, key) = issued(&w, uid).await;
    let session = login(&w.auth, "bearer", "pw").await.unwrap();
    let cookie = cookie(&session);
    // A real persisted address row is included without putting the address on cooldown.
    refuse(
        &w,
        request("bad"),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    let mut warm = accepted(&w, request(key.expose()).session(&cookie)).await;
    warm.rollback().await.unwrap();
    // Transaction retains its pooled Object after rollback for possible reuse; drop it before
    // measuring the next request so connection admission is already warm, as in session tests.
    drop(warm);
    let wire = w.h.wire().unwrap().clone();
    let mut begun = erp_testkit::assert_round_trips!(wire, == 1, {
        w.auth.begin_api_key(request(key.expose()).session(&cookie), CredentialOptions::new()).await.unwrap()
    });
    assert_eq!(begun.tx.env().uid(), uid);
    assert!(!begun.drop_cookie);
    begun.tx.rollback().await.unwrap();
}

#[tokio::test]
async fn null_and_unknown_stored_kinds_are_not_accepted_as_user_keys() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (id, key) = issued(&w, uid).await;
    sql(&w, &format!("ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_kind; ALTER TABLE res_users_apikeys ALTER COLUMN kind DROP NOT NULL; UPDATE res_users_apikeys SET kind=NULL WHERE id={}", id.get())).await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::Database,
        "Invalid stored API key authority",
    )
    .await;
    sql(
        &w,
        &format!(
            "UPDATE res_users_apikeys SET kind='future' WHERE id={}",
            id.get()
        ),
    )
    .await;
    refuse(
        &w,
        request(key.expose()),
        ErrorKind::Database,
        "Invalid stored API key authority",
    )
    .await;
}

#[tokio::test]
async fn failure_counter_storage_error_propagates_after_releasing_request_connection() {
    let w = world().await;
    let mut tx = w.h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.batch("CREATE FUNCTION refuse_key_counter() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'native failure counter unavailable' USING ERRCODE='P0001'; END $$; CREATE TRIGGER refuse_key_counter BEFORE INSERT OR UPDATE ON erp_login_throttle FOR EACH ROW EXECUTE FUNCTION refuse_key_counter()").await.unwrap();
    tx.commit().await.unwrap();
    let pool = pool_of(&w.h, 2);
    let node = erp_security::auth::Auth::new(
        orm_on(&w.h, pool.clone()).await,
        erp_security::auth::AuthConfig::DEFAULT,
        own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
    )
    .await
    .unwrap();
    // With one slot held, the refusal must release its request transaction before its
    // separate failure-count transaction can run. Holding that request would deadlock here.
    let held = pool.get().await.unwrap();
    let e = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        node.begin_api_key(request("bad"), CredentialOptions::new()),
    )
    .await
    .expect("refusal releases its request slot before counting")
    .err()
    .unwrap();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::Database, "native failure counter unavailable")
    );
    assert_eq!(e.sqlstate(), Some("P0001"));
    assert_eq!(node.stats().failure_write_errors, 1);
    let reused = tokio::time::timeout(std::time::Duration::from_secs(30), pool.get())
        .await
        .expect("error response returns the counter slot")
        .unwrap();
    assert_eq!(
        reused
            .query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
    assert_eq!(
        held.query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
    assert_eq!(count(&w.h, "erp_login_throttle").await, 0);
}

#[tokio::test]
async fn cancelling_the_opening_check_releases_its_request_connection() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let (_, key) = issued(&w, uid).await;
    let mut setup = w.h.begin().await.unwrap();
    setup.install_signal().await.unwrap();
    setup.commit().await.unwrap();
    let pool = pool_of(&w.h, 2);
    let auth = std::sync::Arc::new(
        erp_security::auth::Auth::new(
            orm_on(&w.h, pool.clone()).await,
            erp_security::auth::AuthConfig::DEFAULT,
            own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
        )
        .await
        .unwrap(),
    );
    let held = pool.get().await.unwrap();
    let mut blocker = w.h.begin().await.unwrap();
    blocker
        .batch("LOCK TABLE res_users_apikeys IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let raw = erp_security::auth::Zeroizing::new(key.expose().to_owned());
    let check = {
        let auth = std::sync::Arc::clone(&auth);
        tokio::spawn(async move {
            auth.begin_api_key(request(&raw), CredentialOptions::new())
                .await
        })
    };
    until_lock_wait(&w.h).await;
    check.abort();
    assert!(check.await.unwrap_err().is_cancelled());
    blocker.rollback().await.unwrap();
    let reused = tokio::time::timeout(std::time::Duration::from_secs(30), pool.get())
        .await
        .expect("cancelled check releases or replaces its connection")
        .unwrap();
    assert_eq!(
        reused
            .query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
    assert_eq!(
        held.query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
    assert_eq!(count(&w.h, "erp_login_throttle").await, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_after_counted_verdict_still_persists_exactly_one_failure() {
    let w = world().await;
    let mut setup = w.h.begin().await.unwrap();
    setup.install_signal().await.unwrap();
    setup.commit().await.unwrap();
    let pool = pool_of(&w.h, 2);
    let auth = std::sync::Arc::new(
        erp_security::auth::Auth::new(
            orm_on(&w.h, pool.clone()).await,
            erp_security::auth::AuthConfig::DEFAULT,
            own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
        )
        .await
        .unwrap(),
    );
    let held = pool.get().await.unwrap();
    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    erp_security::auth::test_support::api_key_verdict_gate(&auth, reached_tx, release_rx).unwrap();
    let task = {
        let auth = std::sync::Arc::clone(&auth);
        tokio::spawn(async move {
            auth.begin_api_key(request("bad"), CredentialOptions::new())
                .await
        })
    };
    let abort = task.abort_handle();
    let interrupter = std::thread::spawn(move || {
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("counted verdict reached");
        abort.abort();
        release_tx.send(()).expect("release counted verdict gate");
    });
    assert!(task.await.unwrap_err().is_cancelled());
    interrupter.join().unwrap();
    // The current-thread runtime could not process ROLLBACK's answer while finish was gated;
    // cancellation therefore dropped the known refusal while the ORM was ending its request.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if count(
            &w.h,
            "erp_login_throttle WHERE kind='address' AND failures=1",
        )
        .await
            == 1
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "counted verdict was lost during rollback cancellation"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind='address' AND failures=1"
        )
        .await,
        1
    );
    assert_eq!(count(&w.h, "erp_login_throttle").await, 1);
    let reused = tokio::time::timeout(std::time::Duration::from_secs(30), pool.get())
        .await
        .expect("counted cancellation releases the pool slot")
        .unwrap();
    assert_eq!(
        reused
            .query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
    assert_eq!(
        held.query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
}

#[tokio::test]
async fn detached_failure_count_keeps_lineage_admission_until_write_settles() {
    let w = world().await;
    let mut setup = w.h.begin().await.unwrap();
    setup.install_signal().await.unwrap();
    setup.batch("CREATE FUNCTION hold_key_counter() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(554433221::bigint); RETURN NEW; END $$; CREATE TRIGGER hold_key_counter BEFORE INSERT OR UPDATE ON erp_login_throttle FOR EACH ROW EXECUTE FUNCTION hold_key_counter()").await.unwrap();
    setup.commit().await.unwrap();
    let wire = erp_testkit::WireCounter::start(&w.h.db().config())
        .await
        .unwrap();
    let pool = wire.pool(2).unwrap();
    let hashing = own_pool(
        erp_security::auth::HashPoolConfig::new(
            1,
            1,
            erp_security::auth::password::limits::ARGON2_MEMORY_KIB_MIN,
        )
        .unwrap(),
    );
    let auth = std::sync::Arc::new(
        erp_security::auth::Auth::new(
            orm_on(&w.h, pool.clone()).await,
            erp_security::auth::AuthConfig::DEFAULT,
            hashing,
        )
        .await
        .unwrap(),
    );
    let other = erp_security::auth::Auth::new(
        auth.orm().clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        hashing,
    )
    .await
    .unwrap();
    let held = pool.get().await.unwrap();
    let mut blocker = w.h.begin().await.unwrap();
    blocker
        .query("SELECT pg_advisory_xact_lock(554433221::bigint)", &[])
        .await
        .unwrap();
    let task = {
        let auth = std::sync::Arc::clone(&auth);
        tokio::spawn(async move {
            auth.begin_api_key(request("bad"), CredentialOptions::new())
                .await
        })
    };
    until_lock_wait(&w.h).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // The wire is intentionally unsettled: the detached counter waits for the fixture lock.
    // PostgreSQL's observed lock wait proves that request was forwarded before this snapshot.
    let before = wire.messages();
    let error = other
        .begin_api_key(request("bad"), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::ResourceExhausted,
            "Server busy; retry after 1 second."
        )
    );
    assert_eq!(error.condition(), Some(erp_fields::Condition::Overloaded));
    assert_eq!(other.stats().admission_overloaded, 1);
    assert_eq!(
        wire.messages(),
        before,
        "admission refusal performs no credential or settings SQL"
    );
    blocker.rollback().await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if count(
            &w.h,
            "erp_login_throttle WHERE kind='address' AND failures=1",
        )
        .await
            == 1
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached failure counter did not settle"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let error = other
        .begin_api_key(request("bad"), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind='address' AND failures=2"
        )
        .await,
        1
    );
    assert_eq!(
        held.query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
}

#[tokio::test]
async fn detached_writers_keep_admission_binding_when_every_auth_is_reconstructed() {
    let w = world().await;
    let mut setup = w.h.begin().await.unwrap();
    setup.install_signal().await.unwrap();
    setup.batch("CREATE FUNCTION hold_reconstructed_counter() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(554433222::bigint); RETURN NEW; END $$; CREATE TRIGGER hold_reconstructed_counter BEFORE INSERT OR UPDATE ON erp_login_throttle FOR EACH ROW EXECUTE FUNCTION hold_reconstructed_counter()").await.unwrap();
    setup.commit().await.unwrap();
    let wire = erp_testkit::WireCounter::start(&w.h.db().config())
        .await
        .unwrap();
    let pool = wire.pool(4).unwrap();
    let hashing = own_pool(
        erp_security::auth::HashPoolConfig::new(
            1,
            1,
            erp_security::auth::password::limits::ARGON2_MEMORY_KIB_MIN,
        )
        .unwrap(),
    );
    let orm = orm_on(&w.h, pool.clone()).await;
    let auth = std::sync::Arc::new(
        erp_security::auth::Auth::new(
            orm.clone(),
            erp_security::auth::AuthConfig::DEFAULT,
            hashing,
        )
        .await
        .unwrap(),
    );
    // Four pool slots and one hashing permit produce two lineage admission leases. Holding
    // one slot leaves a fourth slot available to reconstruct Auth while both writers wait.
    let held = pool.get().await.unwrap();
    let mut blocker = w.h.begin().await.unwrap();
    blocker
        .query("SELECT pg_advisory_xact_lock(554433222::bigint)", &[])
        .await
        .unwrap();
    let mut callers = Vec::new();
    for address in ["192.0.2.31", "192.0.2.32"] {
        let auth = std::sync::Arc::clone(&auth);
        callers.push(tokio::spawn(async move {
            auth.begin_api_key(
                ApiKeyRequest::new("bad", KeyScope::Rpc, address.parse().unwrap()),
                CredentialOptions::new(),
            )
            .await
        }));
    }
    let observer = w.h.db().client().await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: i64 = observer.query_one("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'", &[]).await.unwrap().get(0);
        if waiting >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "two counter writers did not reach the fixture lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    for caller in callers {
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
    }
    // Keep only the ORM and raw hashing pool, neither of which owns the registry's weak
    // lineage binding. The detached writers must retain that binding themselves.
    drop(auth);
    let rebuilt =
        erp_security::auth::Auth::new(orm, erp_security::auth::AuthConfig::DEFAULT, hashing)
            .await
            .unwrap();
    let before = wire.messages();
    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        rebuilt.begin_api_key(request("bad"), CredentialOptions::new()),
    )
    .await;
    let after = wire.messages();
    // Always unblock fixture work before asserting, including the expected RED timeout path.
    blocker.rollback().await.unwrap();
    let error = refused
        .expect("reconstructed Auth must retain exhausted lineage admission")
        .err()
        .unwrap();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::ResourceExhausted,
            "Server busy; retry after 1 second."
        )
    );
    assert_eq!(error.condition(), Some(erp_fields::Condition::Overloaded));
    assert_eq!(
        after, before,
        "reconstructed admission refusal sends no credential SQL"
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if count(
            &w.h,
            "erp_login_throttle WHERE kind='address' AND failures=1",
        )
        .await
            == 2
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached writers did not settle after reconstruction"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let error = rebuilt
        .begin_api_key(request("bad"), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::AccessDenied, "Invalid apikey")
    );
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind='address' AND failures=1"
        )
        .await,
        3
    );
    assert_eq!(
        held.query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
}
