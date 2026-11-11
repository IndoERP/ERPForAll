//! the `_on_login_cooldown` policy hook. The fixture addon
//! `test_cooldown` overrides it by the submitted login's prefix (see its module doc). Every test
//! shows a behavior: the extra rules refuse before any hash, the core rule still applies, a hook
//! failure refuses without counting a guess or leaking the hook's text, and the shared counters
//! decide across nodes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

use std::sync::Arc;
use std::time::Duration;

use auth_support::*;
use erp_sdk::{Error, ErrorKind};
use erp_security::auth::password::{HashParams, limits};
use erp_security::auth::throttle::{self, ThrottleKey};
use erp_security::auth::{Auth, AuthConfig, COOLDOWN_MESSAGE, HashPoolConfig, LoginError};
use erp_test_models::test_cooldown::{calls, dropped};
use erp_testkit::Harness;

async fn hooked() -> World {
    hooked_with(AuthConfig::DEFAULT).await
}

async fn hooked_with(config: AuthConfig) -> World {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_cooldown::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");
    world_with(h, config, HashPoolConfig::DEFAULT).await
}

fn denied(r: Result<impl std::fmt::Debug, LoginError>) {
    assert!(matches!(r, Err(LoginError::Denied)), "{r:?}");
}

fn cooldown(r: Result<impl std::fmt::Debug, LoginError>) {
    let error: Error = r.unwrap_err().into();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::AccessDenied, COOLDOWN_MESSAGE)
    );
}

/// The text a failed hook is reported with: never the hook's own.
fn hook_failed(r: Result<impl std::fmt::Debug, LoginError>) {
    match r {
        Err(LoginError::Failed(e)) => {
            assert_eq!(e.kind(), ErrorKind::Programming);
            assert_eq!(
                e.message(),
                "the login cooldown policy could not be evaluated"
            );
            assert!(!e.to_string().contains("secret"), "{e}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `erp_login_throttle` row of `login`, keyed by `auth`'s database secret.
async fn login_row(auth: &Auth, login: &str) -> String {
    format!(
        "erp_login_throttle WHERE kind = 'login' AND key_hash = decode('{}', 'hex')",
        hex(&auth.throttle_key(ThrottleKey::Login(login)).await.unwrap())
    )
}

/// Core login rule off, address rule out of reach: only the policy decides.
async fn only_policy_decides(h: &Harness) {
    set_param(h, "base.login_cooldown_after", "100").await;
    set_param(h, "base.login_cooldown_after_login", "0").await;
}

/// before any hash, for a known and an unknown login alike, and counts nothing for the refusal)
#[tokio::test]
async fn an_additional_login_rule_refuses_before_any_hash() {
    let w = hooked().await;
    user_with_password(&w.h, "strict-bob", "pw").await;
    user_with_password(&w.h, "plain-bob", "pw").await;
    only_policy_decides(&w.h).await;
    for name in ["strict-bob", "strict-nobody"] {
        denied(login(&w.auth, name, "wrong").await);
        denied(login(&w.auth, name, "wrong").await);
        let jobs = w.pool.stats().jobs;
        cooldown(login(&w.auth, name, "pw").await);
        assert_eq!(
            w.pool.stats().jobs,
            jobs,
            "{name}: the password is not checked"
        );
        assert_eq!(calls(name), 3);
        assert_eq!(
            count(
                &w.h,
                &format!("{} AND failures = 2", login_row(&w.auth, name).await)
            )
            .await,
            1,
            "{name}: the refused attempt is not counted"
        );
    }
    // another login has no extra rule
    denied(login(&w.auth, "plain-bob", "wrong").await);
    denied(login(&w.auth, "plain-bob", "wrong").await);
    login(&w.auth, "plain-bob", "pw").await.unwrap();
}

#[tokio::test]
async fn an_additional_address_rule_needs_an_address() {
    let w = hooked().await;
    user_with_password(&w.h, "addr-bob", "pw").await;
    only_policy_decides(&w.h).await;
    denied(login_from(&w.auth, "addr-bob", "wrong", "192.0.2.1").await);
    denied(login_from(&w.auth, "addr-bob", "wrong", "192.0.2.1").await);
    cooldown(login_from(&w.auth, "addr-bob", "pw", "192.0.2.1").await);
    login_from(&w.auth, "addr-bob", "pw", "192.0.2.2")
        .await
        .unwrap();
    // in process: no address, the address rule cannot apply (the login has none of its own)
    login(&w.auth, "addr-bob", "pw").await.unwrap();
}

#[tokio::test]
async fn a_weak_policy_cannot_loosen_the_core_rule() {
    let w = hooked().await;
    user_with_password(&w.h, "weak-bob", "pw").await;
    set_param(&w.h, "base.login_cooldown_after", "100").await;
    set_param(&w.h, "base.login_cooldown_after_login", "2").await;
    denied(login(&w.auth, "weak-bob", "wrong").await);
    denied(login(&w.auth, "weak-bob", "wrong").await);
    cooldown(login(&w.auth, "weak-bob", "pw").await);
}

#[tokio::test]
async fn a_trusted_pair_is_exempt_from_the_additional_login_rule() {
    let w = hooked().await;
    let bob = user_with_password(&w.h, "strict-trusted", "pw").await;
    only_policy_decides(&w.h).await;
    login_from(&w.auth, "strict-trusted", "pw", "192.0.2.1")
        .await
        .unwrap();
    denied(login_from(&w.auth, "strict-trusted", "x", "198.51.100.1").await);
    denied(login_from(&w.auth, "strict-trusted", "x", "198.51.100.2").await);
    cooldown(login_from(&w.auth, "strict-trusted", "pw", "203.0.113.9").await);
    let ok = login_from(&w.auth, "strict-trusted", "pw", "192.0.2.1")
        .await
        .unwrap();
    assert_eq!(ok.uid().get(), bob);
}

/// as Failed with a fixed text; nothing is verified or counted; the hook's text is not in it)
#[tokio::test]
async fn a_failing_hook_refuses_without_counting_or_leaking() {
    let w = hooked().await;
    for name in ["hookerr-bob", "hookpanic-bob", "badrule-bob", "writer-bob"] {
        user_with_password(&w.h, name, "pw").await;
        let jobs = w.pool.stats().jobs;
        hook_failed(login(&w.auth, name, "pw").await);
        hook_failed(login(&w.auth, name, "wrong").await);
        assert_eq!(w.pool.stats().jobs, jobs, "{name}: nothing verified");
        assert_eq!(
            count(
                &w.h,
                &format!("{} AND failures >= 1", login_row(&w.auth, name).await)
            )
            .await,
            0,
            "{name}: nothing counted"
        );
    }
    assert_eq!(w.auth.stats().cooldown_hook_failures, 8);
    assert_eq!(
        count(&w.h, "pg_tables WHERE tablename = 'test_cooldown_write'").await,
        0,
        "the hook's transaction is read-only"
    );
}

/// rows read again)
#[tokio::test]
async fn a_restarted_login_asks_the_hook_again() {
    let w = world_with(
        Harness::builder([
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &erp_test_models::test_cooldown::ADDON,
        ])
        .with_decls([erp_data::ir_model_data_decl()])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .await
        .unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    only_policy_decides(&w.h).await;
    let bob = user_with_password(&w.h, "strict-restart", "pw").await;
    let World { h, auth, pool, .. } = w;
    let auth = Arc::new(auth);
    let key = auth
        .throttle_key(ThrottleKey::Login("strict-restart"))
        .await
        .unwrap();
    let release = hold_permits(pool, 1).await;
    let node = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login(&node, "strict-restart", "pw").await });
    until_waiting(pool, 1).await;
    let replaced = erp_security::auth::password::hash(b"new", &HashParams::DEFAULT).unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET password = $1 WHERE id = $2",
        &[&replaced, &bob],
    )
    .await
    .unwrap();
    upsert_failure_pair(&mut tx, &key).await;
    tx.commit().await.unwrap();
    drop(release);
    cooldown(attempt.await.unwrap());
    assert_eq!(auth.stats().login_restarts, 1);
    assert_eq!(calls("strict-restart"), 2, "once per phase A");
}

async fn upsert_failure_pair(tx: &mut erp_sdk::Transaction, key: &[u8; 32]) {
    let address: Option<Vec<u8>> = None;
    let key = key.to_vec();
    let cap = throttle::UNKNOWN_LOGIN_ROWS_PER_ADDRESS;
    for _ in 0..2 {
        tx.execute(throttle::FAILURE_SQL, &[&address, &key, &cap])
            .await
            .unwrap();
    }
}

/// node A put the login on the additional rule's cooldown on node B at once)
#[tokio::test]
async fn two_nodes_share_the_counters() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_cooldown::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    prepare(&h).await;
    only_policy_decides(&h).await;
    user_with_password(&h, "strict-shared", "pw").await;
    let pool = waiting_pool(2);
    let a = node_with(&h, 4, pool).await;
    let b = node_with(&h, 4, pool).await;
    denied(login(&a, "strict-shared", "wrong").await);
    denied(login(&a, "strict-shared", "wrong").await);
    cooldown(login(&b, "strict-shared", "pw").await);
}

/// primary pool: with a hook installed, a session check still gets a connection while logins
/// queue)
#[tokio::test]
async fn hooked_logins_leave_a_connection_for_session_checks() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_cooldown::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    prepare(&h).await;
    let bob = user_with_password(&h, "plain-admit", "pw").await;
    let pool = waiting_pool(1);
    let auth = Arc::new(node_with(&h, 2, pool).await);
    let first = login(&auth, "plain-admit", "pw").await.unwrap();
    let secret = cookie(&first);
    let release = hold_permits(pool, 1).await;
    let attempts: Vec<_> = (0..4)
        .map(|_| {
            let node = Arc::clone(&auth);
            tokio::spawn(async move { login(&node, "plain-admit", "pw").await })
        })
        .collect();
    until_waiting(pool, 1).await;
    let user = tokio::time::timeout(Duration::from_secs(5), check_user(&auth, &secret))
        .await
        .expect("the session check got a connection")
        .unwrap();
    assert_eq!(user, bob);
    drop(release);
    for attempt in attempts {
        attempt.await.unwrap().unwrap();
    }
}

/// never reaches the hook)
#[tokio::test]
async fn refusals_before_the_database_do_not_call_the_hook() {
    let w = hooked().await;
    user_with_password(&w.h, "strict-long", "pw").await;
    let long = format!("strict-{}", "b".repeat(1_025));
    denied(login(&w.auth, &long, "pw").await);
    denied(login(&w.auth, "strict-long", &"p".repeat(1_025)).await);
    assert_eq!(calls(&long), 0);
    assert_eq!(calls("strict-long"), 0);
}

/// Polls until no transaction of the test's database stays open (an aborted hook's connection
/// is rolled back and returned asynchronously).
async fn until_no_open_transaction(h: &Harness) {
    for _ in 0..100 {
        let open = count(
            h,
            "pg_stat_activity WHERE datname = current_database() \
             AND state LIKE 'idle in transaction%'",
        )
        .await;
        if open == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("a transaction stayed open");
}

/// Polls until `login`'s hook wait was cancelled.
async fn until_dropped(login: &str) {
    for _ in 0..100 {
        if dropped(login) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the hook wait of {login} was not cancelled");
}

fn short_deadline() -> AuthConfig {
    AuthConfig::DEFAULT
        .with_cooldown_hook_deadline_ms(1_500)
        .unwrap()
}

/// the fixed text, is cancelled, and leaves nothing verified or counted)
#[tokio::test]
async fn a_hook_past_its_deadline_fails_closed() {
    let w = hooked_with(short_deadline()).await;
    user_with_password(&w.h, "hookslow-bob", "pw").await;
    let jobs = w.pool.stats().jobs;
    let started = std::time::Instant::now();
    hook_failed(login(&w.auth, "hookslow-bob", "pw").await);
    assert!(started.elapsed() < Duration::from_secs(10), "bounded");
    until_dropped("hookslow-bob").await;
    until_no_open_transaction(&w.h).await;
    assert_eq!(w.pool.stats().jobs, jobs, "nothing verified");
    assert_eq!(
        count(
            &w.h,
            &format!(
                "{} AND failures >= 1",
                login_row(&w.auth, "hookslow-bob").await
            )
        )
        .await,
        0,
        "nothing counted"
    );
    assert_eq!(w.auth.stats().cooldown_hook_failures, 1);
    user_with_password(&w.h, "plain-after", "pw").await;
    login(&w.auth, "plain-after", "pw").await.unwrap();
}

#[tokio::test]
async fn a_caller_that_hangs_up_cancels_the_hook() {
    let w = hooked().await;
    user_with_password(&w.h, "hookslow-hangup", "pw").await;
    user_with_password(&w.h, "plain-hangup", "pw").await;
    let auth = Arc::new(w.auth);
    let node = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login(&node, "hookslow-hangup", "pw").await });
    for _ in 0..100 {
        if calls("hookslow-hangup") >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(calls("hookslow-hangup"), 1, "the hook began");
    attempt.abort();
    until_dropped("hookslow-hangup").await;
    until_no_open_transaction(&w.h).await;
    login(&auth, "plain-hangup", "pw").await.unwrap();
}

/// attempt still gets through; session checks are covered by another case)
#[tokio::test]
async fn blocked_hooks_release_the_admission_at_the_deadline() {
    let w = hooked_with(short_deadline()).await;
    user_with_password(&w.h, "plain-blocked", "pw").await;
    let auth = Arc::new(w.auth);
    let attempts: Vec<_> = (0..8)
        .map(|i| {
            let node = Arc::clone(&auth);
            tokio::spawn(async move { login(&node, &format!("hookslow-blocked{i}"), "pw").await })
        })
        .collect();
    let mut failed = 0;
    for attempt in attempts {
        let r = tokio::time::timeout(Duration::from_secs(20), attempt)
            .await
            .expect("an attempt outlived the deadline")
            .unwrap();
        match r {
            Err(LoginError::Failed(_)) => failed += 1,
            Err(LoginError::Overloaded { .. }) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(failed >= 1, "at least one hook ran into its deadline");
    login(&auth, "plain-blocked", "pw").await.unwrap();
}

#[tokio::test]
async fn a_hook_failing_on_the_restart_path_refuses() {
    let w = world_with(
        Harness::builder([
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &erp_test_models::test_cooldown::ADDON,
        ])
        .with_decls([erp_data::ir_model_data_decl()])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .await
        .unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    let bob = user_with_password(&w.h, "flaky-restart", "pw").await;
    let World { h, auth, pool, .. } = w;
    let auth = Arc::new(auth);
    let release = hold_permits(pool, 1).await;
    let node = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login(&node, "flaky-restart", "pw").await });
    until_waiting(pool, 1).await;
    let replaced = erp_security::auth::password::hash(b"new", &HashParams::DEFAULT).unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET password = $1 WHERE id = $2",
        &[&replaced, &bob],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(release);
    hook_failed(attempt.await.unwrap());
    assert_eq!(auth.stats().login_restarts, 1);
    assert_eq!(auth.stats().cooldown_hook_failures, 1);
    assert_eq!(calls("flaky-restart"), 2, "asked again before the restart");
}

#[tokio::test]
async fn a_panicking_hook_leaves_no_open_transaction() {
    let w = hooked().await;
    user_with_password(&w.h, "plain-panic", "pw").await;
    for i in 0..6 {
        hook_failed(login(&w.auth, &format!("hookpanic-{i}"), "pw").await);
        hook_failed(login(&w.auth, &format!("hookerr-{i}"), "pw").await);
    }
    until_no_open_transaction(&w.h).await;
    login(&w.auth, "plain-panic", "pw").await.unwrap();
}
