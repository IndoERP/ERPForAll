//! Identity confirmation and current-session password mutation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod auth_support;
use auth_support::*;
use erp_sdk::ErrorKind;
use erp_security::auth::{AuthConfig, HashPoolConfig, IdentityCheck};
use erp_testkit::Harness;

fn assert_error(error: erp_sdk::Error, kind: ErrorKind, message: &str) {
    assert_eq!((error.kind(), error.message()), (kind, message));
}

#[tokio::test]
async fn identity_window_and_password_change_preserve_only_current_session() {
    let w = world().await;
    let uid = user_with_password(&w.h, "identity", "old").await;
    assert_error(
        w.auth.check_identity(None).await.unwrap_err(),
        ErrorKind::UserError,
        "This method can only be accessed over HTTP",
    );
    let current = login(&w.auth, "identity", "old").await.unwrap();
    let other = login(&w.auth, "identity", "old").await.unwrap();
    let secret = cookie(&current);
    assert_eq!(
        w.auth.check_identity(Some(&secret)).await.unwrap(),
        IdentityCheck::Required
    );
    let action = IdentityCheck::Required.action().unwrap();
    assert_eq!(
        action.get("res_model"),
        Some(&erp_sdk::Value::str("res.users.identitycheck"))
    );
    let before = stored_hash(&w.h, uid).await;
    assert_eq!(
        w.auth
            .change_password(Some(&secret), "päss🔐")
            .await
            .unwrap(),
        IdentityCheck::Required
    );
    assert_eq!(stored_hash(&w.h, uid).await, before);
    assert_error(
        w.auth
            .confirm_identity(&secret, "wrong", IN_PROCESS)
            .await
            .unwrap_err(),
        ErrorKind::AccessDenied,
        "Access Denied",
    );
    assert_eq!(
        w.auth.check_identity(Some(&secret)).await.unwrap(),
        IdentityCheck::Required
    );
    w.auth
        .confirm_identity(&secret, "old", IN_PROCESS)
        .await
        .unwrap();
    assert_eq!(
        w.auth.check_identity(Some(&secret)).await.unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(
        w.auth.check_identity(Some(&cookie(&other))).await.unwrap(),
        IdentityCheck::Required
    );
    assert_eq!(
        w.auth
            .change_password(Some(&secret), "päss🔐")
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), uid);
    assert_error(
        check_user(&w.auth, &cookie(&other)).await.unwrap_err(),
        ErrorKind::SessionExpired,
        "user is not connected",
    );
    assert_ne!(stored_hash(&w.h, uid).await, before);
    let error = login(&w.auth, "identity", "old").await.unwrap_err();
    assert_eq!(error.to_string(), "Access Denied");
    assert!(matches!(error, erp_security::auth::LoginError::Denied));
    assert_eq!(
        login(&w.auth, "identity", "päss🔐")
            .await
            .unwrap()
            .uid()
            .get(),
        uid
    );
}

#[tokio::test]
async fn stale_and_future_identity_times_require_confirmation_and_inputs_are_bounded() {
    let w = world().await;
    user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    let secret = cookie(&current);
    let id = current.session().id().to_text();
    for offset in ["-10 minutes", "1 hour"] {
        let mut tx = w.h.begin().await.unwrap();
        tx.execute("UPDATE erp_session SET identity_checked_at = clock_timestamp() + $1::text::interval WHERE id = $2::text::uuid", &[&offset, &id]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            w.auth.change_password(Some(&secret), "new").await.unwrap(),
            IdentityCheck::Required
        );
    }
    assert_error(
        w.auth.change_password(Some(&secret), "").await.unwrap_err(),
        ErrorKind::UserError,
        "Setting empty passwords is not allowed for security reasons!",
    );
    assert_error(
        w.auth
            .change_password(Some(&secret), &"x".repeat(1025))
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "The new password exceeds the configured credential byte limit.",
    );
}

#[tokio::test]
async fn confirmation_and_revocation_are_visible_on_another_node() {
    let w = world().await;
    let mut tx = w.h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let uid = user_with_password(&w.h, "identity", "old").await;
    let node = node_with(&w.h, 4, own_pool(HashPoolConfig::DEFAULT)).await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    let other = login(&node, "identity", "old").await.unwrap();
    w.auth
        .confirm_identity(&cookie(&current), "old", IN_PROCESS)
        .await
        .unwrap();
    assert_eq!(
        node.check_identity(Some(&cookie(&current))).await.unwrap(),
        IdentityCheck::Verified
    );
    node.change_password(Some(&cookie(&current)), "new")
        .await
        .unwrap();
    assert_eq!(check_user(&w.auth, &cookie(&current)).await.unwrap(), uid);
    assert_error(
        check_user(&w.auth, &cookie(&other)).await.unwrap_err(),
        ErrorKind::SessionExpired,
        "user is not connected",
    );
}

async fn hooked() -> World {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_auth::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .unwrap();
    world_with(h, AuthConfig::DEFAULT, HashPoolConfig::DEFAULT).await
}

#[tokio::test]
async fn hook_failure_rolls_back_password_and_sessions() {
    let w = hooked().await;
    let uid = user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    let other = login(&w.auth, "identity", "old").await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    create(
        &mut tx,
        "test_auth.device",
        &[
            ("session_id", s(&other.session().id().to_text())),
            ("name", s("refuse")),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    w.auth
        .confirm_identity(&cookie(&current), "old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, uid).await;
    assert_error(
        w.auth
            .change_password(Some(&cookie(&current)), "new")
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "test_auth refuses to end this session",
    );
    assert_eq!(stored_hash(&w.h, uid).await, before);
    assert_eq!(check_user(&w.auth, &cookie(&current)).await.unwrap(), uid);
    assert_eq!(check_user(&w.auth, &cookie(&other)).await.unwrap(), uid);
}

#[tokio::test]
async fn logout_during_password_hash_cannot_resurrect_current_session() {
    let w = world().await;
    let uid = user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    w.auth
        .confirm_identity(&cookie(&current), "old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, uid).await;
    let pool = w.pool;
    let release = hold_permits(pool, pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let actor = std::sync::Arc::clone(&auth);
    let secret = cookie(&current);
    let changing = tokio::spawn(async move { actor.change_password(Some(&secret), "new").await });
    until_waiting(pool, 1).await;
    auth.logout(current.session().id()).await.unwrap();
    drop(release);
    assert_error(
        changing.await.unwrap().unwrap_err(),
        ErrorKind::SessionExpired,
        "user is not connected",
    );
    assert_eq!(stored_hash(&w.h, uid).await, before);
}

#[tokio::test]
async fn successful_password_change_runs_hooks_only_for_other_sessions() {
    let w = hooked().await;
    user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    let other = login(&w.auth, "identity", "old").await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    for session in [current.session(), other.session()] {
        create(
            &mut tx,
            "test_auth.device",
            &[
                ("session_id", s(&session.id().to_text())),
                ("name", s("phone")),
            ],
        )
        .await;
    }
    tx.commit().await.unwrap();
    w.auth
        .confirm_identity(&cookie(&current), "old", IN_PROCESS)
        .await
        .unwrap();
    w.auth
        .change_password(Some(&cookie(&current)), "new")
        .await
        .unwrap();
    let mut tx = w.h.begin().await.unwrap();
    let rows = tx
        .query("SELECT session_id FROM test_auth_device", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, String>(0),
        current.session().id().to_text()
    );
    let ended = tx
        .query("SELECT reason FROM test_auth_ended", &[])
        .await
        .unwrap();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].get::<_, String>(0), "CredentialChanged");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn identity_failure_cooldown_precedes_hash_work() {
    let w = world().await;
    user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    set_param(&w.h, "base.login_cooldown_after_login", "1").await;
    assert_error(
        w.auth
            .confirm_identity(&cookie(&current), "wrong", IN_PROCESS)
            .await
            .unwrap_err(),
        ErrorKind::AccessDenied,
        "Access Denied",
    );
    let before = w.pool.stats();
    assert_error(
        w.auth
            .confirm_identity(&cookie(&current), "old", IN_PROCESS)
            .await
            .unwrap_err(),
        ErrorKind::AccessDenied,
        erp_security::auth::COOLDOWN_MESSAGE,
    );
    assert_eq!(w.pool.stats().jobs, before.jobs);
    assert_eq!(
        w.auth
            .check_identity(Some(&cookie(&current)))
            .await
            .unwrap(),
        IdentityCheck::Required
    );
}

#[tokio::test]
async fn cancelled_identity_guess_is_still_counted() {
    let w = world().await;
    user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    set_param(&w.h, "base.login_cooldown_after_login", "1").await;
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let actor = std::sync::Arc::clone(&auth);
    let secret = cookie(&current);
    let guessing =
        tokio::spawn(async move { actor.confirm_identity(&secret, "wrong", IN_PROCESS).await });
    until_waiting(w.pool, 1).await;
    guessing.abort();
    assert!(guessing.await.unwrap_err().is_cancelled());
    drop(release);
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            if count(
                &w.h,
                "erp_login_throttle WHERE kind = 'login' AND failures = 1",
            )
            .await
                == 1
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached verification persists the failure");
    assert_error(
        auth.confirm_identity(&cookie(&current), "old", IN_PROCESS)
            .await
            .unwrap_err(),
        ErrorKind::AccessDenied,
        erp_security::auth::COOLDOWN_MESSAGE,
    );
    assert_eq!(
        auth.check_identity(Some(&cookie(&current))).await.unwrap(),
        IdentityCheck::Required
    );
}

#[tokio::test]
async fn identity_confirmation_rechecks_revocation_after_verifying_password() {
    let w = world().await;
    user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let actor = std::sync::Arc::clone(&auth);
    let secret = cookie(&current);
    let confirming =
        tokio::spawn(async move { actor.confirm_identity(&secret, "old", IN_PROCESS).await });
    until_waiting(w.pool, 1).await;
    auth.logout(current.session().id()).await.unwrap();
    drop(release);
    assert_error(
        confirming.await.unwrap().unwrap_err(),
        ErrorKind::SessionExpired,
        "user is not connected",
    );
    assert_eq!(count(&w.h, "erp_session").await, 0);
}

#[tokio::test]
async fn identity_operations_take_admission_before_cookie_database_work() {
    let w = world().await;
    let mut tx = w.h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let uid = user_with_password(&w.h, "identity", "old").await;
    let pool = own_pool(
        HashPoolConfig::new(
            1,
            1,
            erp_security::auth::password::limits::ARGON2_MEMORY_KIB_MIN,
        )
        .unwrap(),
    );
    let node = std::sync::Arc::new(node_with(&w.h, 2, pool).await);
    let current = login(&node, "identity", "old").await.unwrap();
    node.confirm_identity(&cookie(&current), "old", IN_PROCESS)
        .await
        .unwrap();
    let mut holder = w.h.begin().await.unwrap();
    holder
        .query("SELECT id FROM res_users WHERE id = $1 FOR UPDATE", &[&uid])
        .await
        .unwrap();
    let actor = std::sync::Arc::clone(&node);
    let secret = cookie(&current);
    let changing = tokio::spawn(async move { actor.change_password(Some(&secret), "new").await });
    until_lock_wait(&w.h).await;
    // The first change holds the sole admission while waiting on the user row. A bogus cookie
    // would fail SessionExpired if either second operation checked the database before admission.
    let bogus = erp_security::auth::SessionSecret::generate().unwrap();
    let jobs = pool.stats().jobs;
    for error in [
        node.confirm_identity(&bogus, "old", IN_PROCESS)
            .await
            .unwrap_err(),
        node.change_password(Some(&bogus), "other")
            .await
            .unwrap_err(),
    ] {
        assert_eq!(error.condition(), Some(erp_sdk::Condition::Overloaded));
        assert_error(
            error,
            ErrorKind::ResourceExhausted,
            "Server busy; retry after 1 second.",
        );
    }
    assert_eq!(pool.stats().jobs, jobs);
    holder.rollback().await.unwrap();
    assert_eq!(changing.await.unwrap().unwrap(), IdentityCheck::Verified);
}

// The identity service admits at most 100 stored sessions, including expired rows.
async fn seed_identity_sessions(w: &World, uid: i64, extra: i64) {
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at) \
         SELECT uuidv7(), $1, sha256(gen_random_uuid()::text::bytea), \
                sha256(gen_random_uuid()::text::bytea), 1, 'active', now() - interval '1 day' \
         FROM generate_series(1, $2::bigint)",
        &[&uid, &extra],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO test_auth_device (session_id, name) \
         SELECT id::text, 'phone' FROM erp_session WHERE user_id = $1",
        &[&uid],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

// Snapshot values come from the database, including the current token and confirmation.
async fn identity_state(w: &World, uid: i64) -> (String, String, String, String) {
    let mut tx = w.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT \
         (SELECT row_to_json(u)::text FROM res_users u WHERE id = $1), \
         (SELECT json_agg(s ORDER BY id)::text FROM erp_session s WHERE user_id = $1), \
         (SELECT COALESCE(json_agg(d ORDER BY id)::text, '[]') FROM test_auth_device d), \
         (SELECT COALESCE(json_agg(e ORDER BY id)::text, '[]') FROM test_auth_ended e)",
            &[&uid],
        )
        .await
        .unwrap();
    let row = &rows[0];
    let result = (row.get(0), row.get(1), row.get(2), row.get(3));
    tx.rollback().await.unwrap();
    result
}

#[tokio::test]
async fn identity_session_limit_accepts_boundary_and_runs_every_end_hook() {
    let w = hooked().await;
    let uid = user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    let secret = cookie(&current);
    seed_identity_sessions(&w, uid, 99).await;
    w.auth
        .confirm_identity(&secret, "old", IN_PROCESS)
        .await
        .unwrap();
    assert_eq!(
        w.auth.change_password(Some(&secret), "new").await.unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), uid);
    assert_eq!(count(&w.h, "erp_session").await, 1);
    assert_eq!(count(&w.h, "test_auth_device").await, 1);
    assert_eq!(count(&w.h, "test_auth_ended").await, 1);
}

#[tokio::test]
async fn identity_session_limit_refuses_excess_without_any_mutation() {
    let w = hooked().await;
    let uid = user_with_password(&w.h, "identity", "old").await;
    let current = login(&w.auth, "identity", "old").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "old", IN_PROCESS)
        .await
        .unwrap();
    seed_identity_sessions(&w, uid, 100).await;
    let before = identity_state(&w, uid).await;
    assert_error(
        w.auth
            .confirm_identity(&secret, "old", IN_PROCESS)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Identity operations support at most 100 stored sessions per user; end excess sessions and retry.",
    );
    assert_eq!(identity_state(&w, uid).await, before);
    assert_error(
        w.auth
            .change_password(Some(&secret), "new")
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Identity operations support at most 100 stored sessions per user; end excess sessions and retry.",
    );
    assert_eq!(identity_state(&w, uid).await, before);
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), uid);
}
