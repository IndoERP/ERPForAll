//! password login: the outcomes, the throttle
//! and its pre-filter, the hash formats and their migration, the `res.users` rules, the dummy
//! verify (BUG-082) and the statement and round-trip budget.
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
use erp_orm::{BeginOptions, Isolation, SUPERUSER_ID};
use erp_sdk::{Error, ErrorKind, Value};
use erp_security::auth::password::{self, HashParams, limits};
use erp_security::auth::throttle::{self, ThrottleKey};
use erp_security::auth::{
    Auth, AuthConfig, COOLDOWN_MESSAGE, HashPoolConfig, LoginError, LoginRequest, StoredHash,
};
use erp_testkit::{Harness, assert_round_trips};

fn helper_rebind_refused(result: erp_sdk::Result<Auth>) {
    let error = result.unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "this ORM lineage already hashes on another hashing pool (an Auth of the same ORM was handed a different one): every Auth of an ORM lineage takes the same pool"
        )
    );
}

/// A canceled helper cannot release a lineage while its blocking job still owns a permit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_hash_retains_lineage_until_blocking_work_finishes() {
    let World {
        h,
        auth,
        pool: first,
        ..
    } = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    let env = h.orm().env(SUPERUSER_ID, Default::default());
    let binding = erp_security::auth::pool::for_env(&env);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let running = tokio::spawn(async move {
        binding
            .run(limits::ARGON2_MEMORY_KIB_MIN, move || {
                started_tx.send(()).unwrap();
                // Dropping the test sender is also a deliberate release.
                let _released_or_disconnected = release_rx.recv();
            })
            .await
    });
    started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    drop(auth);
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    let rebound = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, waiting_pool(1)).await;
    drop(release_tx);
    helper_rebind_refused(rebound);
    // A subsequent job on the same one-permit pool proves that the old blocking job
    // finished and released its permit. Its captured lineage lease is dropped before that.
    first
        .run(limits::ARGON2_MEMORY_KIB_MIN, || ())
        .await
        .unwrap();
    Auth::new(h.orm().clone(), AuthConfig::DEFAULT, waiting_pool(1))
        .await
        .expect("completed blocking work must release its lineage");
}

/// Data loading before the first Auth also retains its global-pool binding under cancellation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_hash_before_auth_retains_global_lineage_binding() {
    let h = builder().build().await.unwrap();
    prepare(&h).await;
    let env = h.orm().env(SUPERUSER_ID, Default::default());
    let binding = erp_security::auth::pool::for_env(&env);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let running = tokio::spawn(async move {
        binding
            .run(limits::ARGON2_MEMORY_KIB_MIN, move || {
                started_tx.send(()).unwrap();
                let _released_or_disconnected = release_rx.recv();
            })
            .await
    });
    started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    let rebound = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, waiting_pool(1)).await;
    drop(release_tx);
    helper_rebind_refused(rebound);
}

fn denied(result: Result<impl std::fmt::Debug, LoginError>) {
    match result {
        Err(LoginError::Denied) => {}
        other => panic!("expected Denied, got {other:?}"),
    }
}

fn cooldown(result: Result<impl std::fmt::Debug, LoginError>) {
    match result {
        Err(LoginError::Cooldown) => {}
        other => panic!("expected Cooldown, got {other:?}"),
    }
}

fn assert_base_login_hooks(h: &Harness) {
    let model = h.orm().registry().model_id("res.users").unwrap();
    for method in [
        "_get_login_domain",
        "_get_login_order",
        "_check_credentials",
    ] {
        assert!(
            h.orm().dispatch().is_base_method(model, method),
            "the base-only regression must not install an override of {method}"
        );
    }
}

/// types are not representable by this base API and their input-boundary refusal is untested)
#[tokio::test]
async fn password_login_and_its_refusals() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    assert_eq!(ok.uid().get(), bob);
    assert_eq!(ok.auth().method().as_str(), "password");
    assert_eq!(ok.auth().mfa().as_str(), "default");
    assert_eq!(
        count(&w.h, &format!("res_users_log WHERE create_uid = {bob}")).await,
        1,
        "one login log row"
    );
    for (who, password) in [("bob", "wrong"), ("bob", ""), ("nobody", "pw")] {
        let refused = login(&w.auth, who, password).await;
        let error: Error = match refused {
            Err(e) => e.into(),
            Ok(_) => panic!("{who}/{password} logged in"),
        };
        assert_eq!(
            (error.kind(), error.message()),
            (ErrorKind::AccessDenied, "Access Denied")
        );
    }
    assert_eq!(
        count(&w.h, "erp_session").await,
        1,
        "failures make no session"
    );
    assert_eq!(
        count(&w.h, &format!("res_users_log WHERE create_uid = {bob}")).await,
        1
    );
}

/// empty password and an inactive user, each counted, no session; the JSON-RPC body is
/// 's)
#[tokio::test]
async fn the_four_failures_look_the_same_and_count() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let carol = user_with_password(&w.h, "carol", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    write(
        &mut tx,
        "res.users",
        &[carol],
        &[("active", Value::Bool(false))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut texts = Vec::new();
    for (who, password) in [
        ("bob", "wrong"),
        ("nobody", "pw"),
        ("bob", ""),
        ("carol", "pw"),
    ] {
        let error: Error = login_from(&w.auth, who, password, "192.0.2.10")
            .await
            .unwrap_err()
            .into();
        texts.push((error.kind(), error.message().to_owned()));
    }
    texts.dedup();
    assert_eq!(
        texts,
        [(ErrorKind::AccessDenied, "Access Denied".to_owned())]
    );
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'address' AND failures = 4"
        )
        .await,
        1,
        "every failure counted on the address"
    );
    assert_eq!(count(&w.h, "erp_session").await, 0);
}

/// store error rather than reporting a counted denial, LD2)
#[tokio::test]
async fn recovery_base_failure_accounting_store_error_is_not_a_denied_verdict() {
    let w = world().await;
    assert_base_login_hooks(&w.h);
    let uid = user_with_password(&w.h, "ld2_base", "pw").await;
    let before = w.auth.stats().failure_write_errors;
    let mut tx = w.h.begin().await.unwrap();
    tx.batch(
        "CREATE FUNCTION ld2_base_refuse_counter() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.kind = 'login' THEN \
         RAISE EXCEPTION 'ld2 base failure counter unavailable' USING ERRCODE='P0001'; \
         END IF; RETURN NEW; END $$; \
         CREATE TRIGGER ld2_base_refuse_counter BEFORE INSERT OR UPDATE ON erp_login_throttle \
         FOR EACH ROW EXECUTE FUNCTION ld2_base_refuse_counter()",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    match login_from(&w.auth, "ld2_base", "wrong", "192.0.2.58").await {
        Err(LoginError::Failed(error)) => assert_eq!(
            (error.kind(), error.message(), error.sqlstate()),
            (
                ErrorKind::Database,
                "ld2 base failure counter unavailable",
                Some("P0001")
            )
        ),
        other => panic!("expected the base failure-write error, got {other:?}"),
    }
    assert_eq!(w.auth.stats().failure_write_errors, before + 1);
    assert_eq!(
        count(&w.h, "erp_login_throttle").await,
        0,
        "the login-row fault must roll back the preceding address row too"
    );
    assert_eq!(count(&w.h, "erp_session").await, 0);
    assert_eq!(
        count(&w.h, &format!("res_users_log WHERE create_uid = {uid}")).await,
        0
    );

    let mut tx = w.h.begin().await.unwrap();
    tx.batch(
        "DROP TRIGGER ld2_base_refuse_counter ON erp_login_throttle; \
         DROP FUNCTION ld2_base_refuse_counter()",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    denied(login_from(&w.auth, "ld2_base", "wrong", "192.0.2.58").await);
    assert_eq!(w.auth.stats().failure_write_errors, before + 1);
    for row in [
        login_row(&w.auth, "ld2_base").await,
        address_row(&w.auth, "192.0.2.58").await,
    ] {
        assert_eq!(
            count(&w.h, &format!("{row} AND failures = 1")).await,
            1,
            "after removing the fault a wrong-password verdict must count once"
        );
    }
    assert_eq!(count(&w.h, "erp_login_throttle").await, 2);
    assert_eq!(count(&w.h, "erp_session").await, 0);
}

/// after hashing without publishing a session)
#[tokio::test]
async fn recovery_base_login_rename_during_hash_writes_no_session() {
    base_login_rename_during_hash_writes_no_session(false).await;
}

/// after hashing without publishing a session or replacing the stored hash)
#[tokio::test]
async fn recovery_base_login_rename_during_hash_with_rehash_writes_no_session() {
    base_login_rename_during_hash_writes_no_session(true).await;
}

async fn base_login_rename_during_hash_writes_no_session(rehash: bool) {
    let w = world().await;
    assert_base_login_hooks(&w.h);
    if rehash {
        set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    }
    let uid = user_with_password(&w.h, "base_rename_me", "pw").await;
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    let stored_before = stored_hash(&w.h, uid).await.unwrap();
    assert_eq!(stored_before.starts_with("$pbkdf2-sha512$"), rehash);

    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = Arc::new(w.auth);
    let task_auth = Arc::clone(&auth);
    let pending = tokio::spawn(async move { login(&task_auth, "base_rename_me", "pw").await });
    // Phase A has captured the canonical login and password before the verify permit.
    until_waiting(w.pool, 1).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET login = 'base_renamed_user' WHERE id = $1",
        &[&uid],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(release);

    denied(pending.await.unwrap());
    assert_eq!(auth.stats().login_restarts, 1, "rehash={rehash}");
    assert_eq!(stored_hash(&w.h, uid).await.unwrap(), stored_before);
    assert_eq!(
        count(&w.h, &format!("erp_session WHERE user_id = {uid}")).await,
        0,
        "rehash={rehash}"
    );
    assert_eq!(
        count(&w.h, &format!("res_users_log WHERE create_uid = {uid}")).await,
        0,
        "rehash={rehash}"
    );
}

/// update during hashing; the resulting session validates against the canonical stored login)
#[tokio::test]
async fn recovery_base_login_unrelated_update_during_hash_still_succeeds() {
    for rehash in [false, true] {
        let w = world().await;
        assert_base_login_hooks(&w.h);
        if rehash {
            set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
        }
        let uid = user_with_password(&w.h, "base_keep_me", "pw").await;
        set_param(&w.h, "password.hashing.scheme", "argon2id").await;
        let stored_before = stored_hash(&w.h, uid).await.unwrap();
        assert_eq!(stored_before.starts_with("$pbkdf2-sha512$"), rehash);

        let release = hold_permits(w.pool, w.pool.config().permits()).await;
        let auth = Arc::new(w.auth);
        let task_auth = Arc::clone(&auth);
        let pending = tokio::spawn(async move { login(&task_auth, "base_keep_me", "pw").await });
        until_waiting(w.pool, 1).await;
        let mut tx = w.h.begin().await.unwrap();
        tx.execute(
            "UPDATE res_users SET name = 'Updated during base verify' WHERE id = $1",
            &[&uid],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(release);

        let logged = pending.await.unwrap().unwrap();
        assert_eq!(logged.auth().uid().get(), uid);
        assert_eq!(logged.auth().rehash(), rehash);
        assert_eq!(auth.stats().login_restarts, 0, "rehash={rehash}");
        assert_eq!(
            check_user(&auth, logged.session().secret()).await.unwrap(),
            uid
        );
        if rehash {
            assert!(
                stored_hash(&w.h, uid)
                    .await
                    .unwrap()
                    .starts_with("$argon2id$")
            );
        } else {
            assert_eq!(stored_hash(&w.h, uid).await.unwrap(), stored_before);
        }
        assert_eq!(
            count(&w.h, &format!("erp_session WHERE user_id = {uid}")).await,
            1
        );
    }
}

/// Dummy verify (BUG-082, design note §9 new case): an unknown login, an inactive user and an
/// empty password each verify a dummy hash; a known user verifies the real one.
#[tokio::test]
async fn refusals_without_a_hash_verify_a_dummy_one() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let carol = user_with_password(&w.h, "carol", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    write(
        &mut tx,
        "res.users",
        &[carol],
        &[("active", Value::Bool(false))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let before = w.pool.stats();
    denied(login(&w.auth, "nobody", "pw").await);
    denied(login(&w.auth, "carol", "pw").await);
    denied(login(&w.auth, "bob", "").await);
    let after_dummy = w.pool.stats();
    assert_eq!(after_dummy.dummy_verifies - before.dummy_verifies, 3);
    denied(login(&w.auth, "bob", "wrong").await);
    let after_real = w.pool.stats();
    assert_eq!(after_real.dummy_verifies, after_dummy.dummy_verifies);
    assert_eq!(after_real.jobs - after_dummy.jobs, 1, "one real verify");
}

/// An over-long login or password is refused before any statement or hash (design note §14.4
/// step 1).
#[tokio::test]
async fn over_long_credentials_are_refused_without_work() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let before = w.pool.stats();
    denied(login(&w.auth, &"b".repeat(1_025), "pw").await);
    denied(login(&w.auth, "bob", &"p".repeat(1_025)).await);
    assert_eq!(w.pool.stats().jobs, before.jobs);
    assert_eq!(count(&w.h, "erp_login_throttle").await, 0);
}

/// An unrecognised stored hash is refused after a dummy verify (design note §9 new case; the
/// `error` log line with the user id is not asserted).
#[tokio::test]
async fn an_unknown_hash_format_is_refused() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET password = '$2b$12$abcdefghijklmnopqrstuv' WHERE id = $1",
        &[&bob],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let before = w.pool.stats();
    denied(login(&w.auth, "bob", "pw").await);
    assert_eq!(w.pool.stats().dummy_verifies - before.dummy_verifies, 1);
}

/// written instead of disabling the rule; the refusal is tested in auth_config_parameter.rs)
#[tokio::test]
async fn address_cooldown() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "base.login_cooldown_after", "3").await;
    set_param(&w.h, "base.login_cooldown_duration", "1").await;
    for _ in 0..3 {
        denied(login_from(&w.auth, "bob", "wrong", "192.0.2.1").await);
    }
    let jobs = w.pool.stats().jobs;
    let refused = login_from(&w.auth, "bob", "pw", "192.0.2.1").await;
    let error: Error = refused.unwrap_err().into();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::AccessDenied, COOLDOWN_MESSAGE)
    );
    assert_eq!(w.pool.stats().jobs, jobs, "the password is not checked");
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'address' AND failures = 3"
        )
        .await,
        1,
        "the refused attempt is not counted"
    );
    // another address is not on cooldown
    let other = login_from(&w.auth, "bob", "pw", "192.0.2.2").await.unwrap();
    assert_eq!(other.uid().get(), bob);
    // the cooldown ends: the address's last failure is moved past the 1 s duration (rust review
    // round 2 N-8: no sleep, the clock the rule reads is the row's)
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE erp_login_throttle SET last_failure_at = last_failure_at - interval '2 seconds' \
          WHERE kind = 'address'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    login_from(&w.auth, "bob", "pw", "192.0.2.1").await.unwrap();
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'address' AND failures = 3"
        )
        .await,
        1,
        "a success leaves the address counter (security review Low 1): it decays"
    );
    // 0 would turn the rule off: refused when written (round 1 decision 5; tested in
    // auth_config_parameter.rs)
}

/// which has its own threshold, `base.login_cooldown_after_login`; HTTP 429 is
/// 's)
#[tokio::test]
async fn login_cooldown_across_addresses() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "base.login_cooldown_after", "100").await;
    set_param(&w.h, "base.login_cooldown_after_login", "2").await;
    denied(login_from(&w.auth, "bob", "wrong", "192.0.2.1").await);
    denied(login_from(&w.auth, "bob", "wrong", "192.0.2.2").await);
    cooldown(login_from(&w.auth, "bob", "pw", "192.0.2.3").await);
    // without a factor of its own, the login threshold is 4 × the address one
    set_param(&w.h, "base.login_cooldown_after_login", "").await;
    set_param(&w.h, "base.login_cooldown_after", "1").await;
    // login failures are 2 < 4: only the address rule applies, and 192.0.2.4 has none
    login_from(&w.auth, "bob", "pw", "192.0.2.4").await.unwrap();
}

/// across R replicas it admits R × burst, `scale-out.md` §2)
#[tokio::test]
async fn prefilter_refuses_bursts_before_the_database() {
    let config = AuthConfig::DEFAULT.with_login_prefilter(5, 0.1).unwrap();
    let w = world_with(
        builder().build().await.unwrap(),
        config,
        HashPoolConfig::DEFAULT,
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "base.login_cooldown_after", "100").await;
    for _ in 0..5 {
        denied(login_from(&w.auth, "bob", "wrong", "198.51.100.7").await);
    }
    cooldown(login_from(&w.auth, "bob", "pw", "198.51.100.7").await);
    assert_eq!(
        (w.auth.prefilter().refused(), w.auth.prefilter().evicted()),
        (1, 0)
    );
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'address' AND failures = 5"
        )
        .await,
        1,
        "the refused attempt reached no statement"
    );
    login_from(&w.auth, "bob", "pw", "198.51.100.8")
        .await
        .unwrap();
}

/// `pbkdf2_sha512` with at least 600,000 rounds when selected)
#[tokio::test]
async fn passwords_are_stored_hashed() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let stored = stored_hash(&w.h, bob).await.unwrap();
    assert!(
        stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "{stored}"
    );
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    let carol = user_with_password(&w.h, "carol", "pw").await;
    let stored = stored_hash(&w.h, carol).await.unwrap();
    assert!(stored.starts_with("$pbkdf2-sha512$600000$"), "{stored}");
    // a plaintext password written by SQL is hashed by init
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET password = 'plain' WHERE id = $1",
        &[&bob],
    )
    .await
    .unwrap();
    erp_orm::model_hooks::call_init(&mut tx, "res.users")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let stored = stored_hash(&w.h, bob).await.unwrap();
    assert!(StoredHash::parse(&stored).is_ok(), "{stored}");
    login(&w.auth, "bob", "plain").await.unwrap();
    login(&w.auth, "carol", "pw").await.unwrap();
    // reading the field never shows the hash
    let mut tx = w.h.begin().await.unwrap();
    let value = records(&tx, "res.users", &[bob])
        .value(&mut tx, "password")
        .await
        .unwrap();
    assert!(!value.is_truthy(), "{value:?}");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn reference_hashes_are_migrated_on_login() {
    let w = world().await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(ok.auth().rehash());
    let stored = stored_hash(&w.h, bob).await.unwrap();
    assert!(stored.starts_with("$argon2id$"), "{stored}");
    let begun = w
        .auth
        .begin_session(&cookie(&ok), erp_security::auth::CredentialOptions::new())
        .await
        .unwrap();
    assert_eq!(begun.session.user().get(), bob);
    let mut tx = begun.tx;
    tx.rollback().await.unwrap();
    let again = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(!again.auth().rehash());
}

#[tokio::test]
async fn a_malformed_setting_is_an_error_naming_the_key() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    // written around the model: the typed API refuses it (round 1 decision 5)
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_config_parameter SET value = 'ten' WHERE key = 'base.login_cooldown_after'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // the service caches what `Auth::new` read (rust review round 3 RD-7): a write around the
    // model is seen once the generation moves, as after any write through the model
    make_settings_stale(&w.h).await;
    match login(&w.auth, "bob", "pw").await {
        Err(LoginError::Failed(e)) => {
            assert_eq!(e.kind(), ErrorKind::Validation);
            assert!(e.message().contains("base.login_cooldown_after"), "{e}");
        }
        other => panic!("expected a validation error, got {other:?}"),
    }
}

#[tokio::test]
async fn unique_login_and_activation_rules() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    let e = try_user(&mut tx, "bob", &[]).await.unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            "The database refused the change: You can not have two users with the same login!"
        )
    );
    tx.rollback().await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    let e = write(&mut tx, "res.users", &[1], &[("active", Value::Bool(true))])
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::UserError, "You cannot activate the superuser.")
    );
    tx.rollback().await.unwrap();
    let mut tx = w.h.begin_as(bob).await.unwrap();
    let e = write(
        &mut tx,
        "res.users",
        &[bob],
        &[("active", Value::Bool(false))],
    )
    .await
    .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::UserError,
            "You cannot deactivate the user you're currently logged in as."
        )
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn empty_passwords_are_refused() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    let e = write(&mut tx, "res.users", &[bob], &[("password", s(""))])
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::UserError,
            "Setting empty passwords is not allowed for security reasons!"
        )
    );
    tx.rollback().await.unwrap();
}

/// Budget (design note §14.4, §14.11; ceiling 3, lead decision 9): a login is 2 round trips once
/// the settings are cached (phase A: `BEGIN`, the read and `COMMIT` in one flight; phase B: `BEGIN`,
/// the verdict and `COMMIT` in one flight), and a failure the same.
#[tokio::test]
async fn login_round_trips() {
    let w = world_with(
        builder().count_round_trips().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::DEFAULT,
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    let wire = w.h.wire().unwrap().clone();
    // warm: the settings cache, the prepared login statement, the dummy hash
    login(&w.auth, "bob", "pw").await.unwrap();
    denied(login(&w.auth, "nobody", "pw").await);
    let loads = w.auth.stats().settings_loads;
    assert_round_trips!(wire, == 2, {
        login(&w.auth, "bob", "pw").await.unwrap();
    });
    assert_round_trips!(wire, == 2, {
        denied(login(&w.auth, "bob", "wrong").await);
    });
    assert_eq!(w.auth.stats().settings_loads, loads, "the cache held");
    // a non-interactive caller goes through the same path
    let _ = w
        .auth
        .authenticate(LoginRequest::new("bob", "pw", IN_PROCESS).interactive(false))
        .await
        .unwrap();
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `erp_login_throttle WHERE kind = <kind> AND key_hash = <key>` (the key keyed by `auth`'s
/// database secret, security review round 2 S4).
async fn throttle_row(auth: &Auth, key: ThrottleKey<'_>) -> String {
    format!(
        "erp_login_throttle WHERE kind = '{}' AND key_hash = decode('{}', 'hex')",
        key.kind().as_str(),
        hex(&auth.throttle_key(key).await.unwrap())
    )
}

/// The throttle row of `login`.
async fn login_row(auth: &Auth, login: &str) -> String {
    throttle_row(auth, ThrottleKey::Login(login)).await
}

/// The throttle row of the client address `addr`.
async fn address_row(auth: &Auth, addr: &str) -> String {
    throttle_row(
        auth,
        ThrottleKey::Address(&throttle::address_key(addr.parse().unwrap())),
    )
    .await
}

/// The failure statement of the login, in `tx`, for the login key `key`.
async fn upsert_failure(tx: &mut erp_sdk::Transaction, key: &[u8; 32]) -> Result<u64, Error> {
    let address: Option<Vec<u8>> = None;
    let key = key.to_vec();
    let cap = throttle::UNKNOWN_LOGIN_ROWS_PER_ADDRESS;
    tx.execute(throttle::FAILURE_SQL, &[&address, &key, &cap])
        .await
}

/// Rust review H2, database review H1, lead decision 9 (a): two failure upserts on one key from
/// two transactions interleaved (the second read before the first committed). READ COMMITTED,
/// as the login writes them now: both count. REPEATABLE READ, as the login wrote them before:
/// the second is a serialization failure and one guess is lost.
#[tokio::test]
async fn failure_upserts_of_two_interleaved_transactions_both_count() {
    let w = world().await;
    let orm = w.h.orm();
    let options = |isolation| BeginOptions::new(SUPERUSER_ID).isolation(isolation);
    for (isolation, login) in [
        (Isolation::ReadCommitted, "rc"),
        (Isolation::RepeatableRead, "rr"),
    ] {
        let key = w
            .auth
            .throttle_key(ThrottleKey::Login(login))
            .await
            .unwrap();
        let mut first = orm.begin_opts(options(isolation)).await.unwrap();
        let mut second = orm.begin_opts(options(isolation)).await.unwrap();
        // the second reads first (phase A's read): its snapshot predates the first's commit
        second
            .query("SELECT count(*) FROM erp_login_throttle", &[])
            .await
            .unwrap();
        upsert_failure(&mut first, &key).await.unwrap();
        first.commit().await.unwrap();
        let outcome = upsert_failure(&mut second, &key).await;
        match isolation {
            Isolation::ReadCommitted => {
                assert_eq!(outcome.unwrap(), 1);
                second.commit().await.unwrap();
                assert_eq!(
                    count(
                        &w.h,
                        &format!("{} AND failures = 2", login_row(&w.auth, login).await)
                    )
                    .await,
                    1,
                    "both failures landed"
                );
            }
            _ => {
                let e = outcome.unwrap_err();
                assert_eq!(
                    (e.kind(), e.sqlstate()),
                    (ErrorKind::Concurrency, Some("40001"))
                );
                assert_eq!(
                    e.message(),
                    "could not serialize access due to concurrent update"
                );
                second.rollback().await.unwrap();
            }
        }
    }
}

/// Lead decision 9 (b), database review H3, database review round 2 N7: the password changes
/// between phase A and phase B (the login waits for the hashing permit meanwhile). The verdict
/// writes nothing for the old hash: no rehash over the new hash, no session, no login log row. The
/// attempt reads phase A again once and is decided on the new hash: `Denied`, and counted (the
/// presented password is wrong now). Once with a legacy hash (the rehash path), once without.
#[tokio::test]
async fn a_password_change_between_the_phases_writes_nothing() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    let carol = user_with_password(&w.h, "carol", "pw").await;
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let World { h, auth, pool, .. } = w;
    let auth = Arc::new(auth);
    for (who, uid) in [("carol", carol), ("bob", bob)] {
        let release = hold_permits(pool, 1).await;
        let node = Arc::clone(&auth);
        let attempt = tokio::spawn(async move { login(&node, who, "pw").await });
        // phase A is done: the attempt waits for the permit of its verify
        until_waiting(pool, 1).await;
        let replaced = password::hash(b"new", &HashParams::DEFAULT).unwrap();
        let mut tx = h.begin().await.unwrap();
        tx.execute(
            "UPDATE res_users SET password = $1 WHERE id = $2",
            &[&replaced, &uid],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(release);
        denied(attempt.await.unwrap());
        assert_eq!(stored_hash(&h, uid).await.unwrap(), replaced, "{who}");
        assert_eq!(
            count(&h, &format!("erp_session WHERE user_id = {uid}")).await,
            0
        );
        assert_eq!(
            count(&h, &format!("res_users_log WHERE create_uid = {uid}")).await,
            0
        );
        assert_eq!(
            count(
                &h,
                &format!("{} AND failures = 1", login_row(&auth, who).await)
            )
            .await,
            1,
            "decided on the new hash after one restart"
        );
        login(&auth, who, "new").await.unwrap();
    }
    assert_eq!(auth.stats().login_restarts, 2);
}

/// Database review round 2 N7: two correct logins of a legacy user verify concurrently; the first
/// rehashes, so the second's phase B finds the credential changed. It reads phase A again once,
/// verifies the new hash and logs in (no `Denied` for a correct password).
#[tokio::test]
async fn a_login_behind_a_concurrent_rehash_restarts_once() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    let carol = user_with_password(&w.h, "carol", "pw").await;
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    let World { h, auth, pool, .. } = w;
    let auth = Arc::new(auth);
    let release = hold_permits(pool, 1).await;
    let attempts: Vec<_> = (0..2)
        .map(|_| {
            let node = Arc::clone(&auth);
            tokio::spawn(async move { login(&node, "carol", "pw").await })
        })
        .collect();
    // both passed phase A on the legacy hash and wait for the permit of their verify
    until_waiting(pool, 2).await;
    drop(release);
    let mut rehashed = 0;
    let mut sessions = Vec::new();
    for attempt in attempts {
        let logged_in = attempt.await.unwrap().unwrap();
        if logged_in.auth().rehash() {
            rehashed += 1;
        }
        sessions.push(cookie(&logged_in));
    }
    assert!(rehashed >= 1, "the first rehashed");
    assert_eq!(auth.stats().login_restarts, 1);
    let stored = stored_hash(&h, carol).await.unwrap();
    assert!(stored.starts_with("$argon2id$"), "{stored}");
    // DB7-2: a successful restart must return a cookie surviving the other login's cleanup.
    for secret in sessions {
        assert_eq!(check_user(&auth, &secret).await.unwrap(), carol);
    }
}

/// Lead decision 9 (c), rust review H1: one hashing permit, a primary pool of 2 connections and 4
/// logins in flight (one waiting for the permit, three for the admission): no login holds a
/// connection while it waits, so a session check still gets one.
#[tokio::test]
async fn logins_in_flight_leave_a_connection_for_session_checks() {
    let h = builder().build().await.expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    prepare(&h).await;
    let bob = user_with_password(&h, "bob", "pw").await;
    let pool = waiting_pool(1);
    let auth = Arc::new(node_with(&h, 2, pool).await);
    let first = login(&auth, "bob", "pw").await.unwrap();
    let secret = cookie(&first);
    let release = hold_permits(pool, 1).await;
    let attempts: Vec<_> = (0..4)
        .map(|_| {
            let node = Arc::clone(&auth);
            tokio::spawn(async move { login(&node, "bob", "pw").await })
        })
        .collect();
    until_waiting(pool, 1).await;
    let user = tokio::time::timeout(Duration::from_secs(5), check_user(&auth, &secret))
        .await
        .expect("the session check got a connection")
        .unwrap();
    assert_eq!(user, bob);
    assert!(
        attempts.iter().all(|a| !a.is_finished()),
        "logins in flight"
    );
    drop(release);
    for attempt in attempts {
        attempt.await.unwrap().unwrap();
    }
}

/// Lead decision 3 (security review M1): an attacker keeps bob's login on cooldown from other
/// addresses; a correct password from an (address, login) pair that logged in before still logs
/// in, a wrong one from it still counts, and a new address stays refused. A success clears no
/// counter (security review round 2 N4), so the login stays on cooldown for every other address.
#[tokio::test]
async fn a_trusted_pair_logs_in_through_the_login_cooldown() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "base.login_cooldown_after", "100").await;
    set_param(&w.h, "base.login_cooldown_after_login", "2").await;
    login_from(&w.auth, "bob", "pw", "192.0.2.1").await.unwrap();
    login_from(&w.auth, "bob", "pw", "2001:db8:1:2::1")
        .await
        .unwrap();
    for attacker in ["198.51.100.1", "198.51.100.2"] {
        denied(login_from(&w.auth, "bob", "guess", attacker).await);
    }
    cooldown(login_from(&w.auth, "bob", "pw", "198.51.100.3").await);
    cooldown(login_from(&w.auth, "bob", "pw", "203.0.113.5").await);
    // the trusted pair's wrong password is verified and counted
    denied(login_from(&w.auth, "bob", "typo", "192.0.2.1").await);
    assert_eq!(
        count(
            &w.h,
            &format!("{} AND failures = 3", login_row(&w.auth, "bob").await)
        )
        .await,
        1
    );
    let ok = login_from(&w.auth, "bob", "pw", "192.0.2.1").await.unwrap();
    assert_eq!(ok.uid().get(), bob);
    // the success cleared nothing: a new address is still refused
    cooldown(login_from(&w.auth, "bob", "pw", "203.0.113.6").await);
    // the IPv6 pair is the /64 and the login: another address of the same /64 is the same pair
    login_from(&w.auth, "bob", "pw", "2001:db8:1:2::77")
        .await
        .unwrap();
    set_param(&w.h, "base.login_cooldown_after_login", "1").await;
    cooldown(login_from(&w.auth, "bob", "guess", "198.51.100.1").await);
    login_from(&w.auth, "bob", "pw", "2001:db8:1:2:ffff::9")
        .await
        .unwrap();
}

/// Security review round 2 S2: one login keeps at most `TRUSTED_PAIRS_PER_LOGIN` trusted pairs;
/// a success from one more address drops the pair that logged in longest ago, in the same
/// statement, and leaves another login's pairs alone. The dropped pair is refused by the login
/// cooldown again; the newest pairs still log in through it.
#[tokio::test]
async fn trusted_pairs_are_bounded_per_login() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    user_with_password(&w.h, "carol", "pw").await;
    set_param(&w.h, "base.login_cooldown_after", "100").await;
    set_param(&w.h, "base.login_cooldown_after_login", "1").await;
    login_from(&w.auth, "carol", "pw", "192.0.2.200")
        .await
        .unwrap();
    let max = throttle::TRUSTED_PAIRS_PER_LOGIN;
    let addrs: Vec<String> = (0..max + 4).map(|i| format!("192.0.2.{}", i + 1)).collect();
    for addr in &addrs {
        login_from(&w.auth, "bob", "pw", addr).await.unwrap();
    }
    let trusted = "erp_login_throttle WHERE kind = 'trusted'";
    assert_eq!(
        count(&w.h, trusted).await,
        i64::from(max) + 1,
        "bob's 16, carol's 1"
    );
    // bob's login on cooldown from elsewhere
    denied(login_from(&w.auth, "bob", "guess", "203.0.113.1").await);
    cooldown(login_from(&w.auth, "bob", "pw", "203.0.113.2").await);
    for (i, addr) in addrs.iter().enumerate() {
        let outcome = login_from(&w.auth, "bob", "pw", addr).await;
        if i < 4 {
            cooldown(outcome);
        } else {
            outcome.unwrap();
        }
    }
    login_from(&w.auth, "carol", "pw", "192.0.2.200")
        .await
        .unwrap();
}

/// Lead decision 4 (security review M2), by cost accounting: every denial costs at least one
/// verify at the current parameters. An unknown login: one dummy. A current hash: its verify. A
/// legacy (cheaper) hash: its verify and a dummy at the current parameters. A success on the
/// legacy hash: its verify and the rehash, no dummy.
#[tokio::test]
async fn denials_cost_at_least_the_current_parameters() {
    let w = world().await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    user_with_password(&w.h, "old", "pw").await;
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    user_with_password(&w.h, "new", "pw").await;
    let cost = |before: erp_security::auth::HashPoolStats| {
        let now = w.pool.stats();
        (
            now.jobs - before.jobs,
            now.dummy_verifies - before.dummy_verifies,
        )
    };
    let before = w.pool.stats();
    denied(login(&w.auth, "nobody", "pw").await);
    assert_eq!(cost(before), (1, 1), "unknown login");
    let before = w.pool.stats();
    denied(login(&w.auth, "new", "wrong").await);
    assert_eq!(cost(before), (1, 0), "current hash");
    let before = w.pool.stats();
    denied(login(&w.auth, "old", "wrong").await);
    assert_eq!(cost(before), (2, 1), "legacy hash: the floor adds a dummy");
    let before = w.pool.stats();
    login(&w.auth, "old", "pw").await.unwrap();
    assert_eq!(cost(before), (2, 0), "legacy success: verify and rehash");
}

/// Lead decision 7 (security review M5), security review round 2 S1: failures add new login rows
/// only while their address has fewer than 32 failures, whether the login names a user or not; a
/// login row that exists counts, and an attempt without an address always counts on its own row.
#[tokio::test]
async fn new_login_rows_are_bounded_per_address() {
    let config = AuthConfig::DEFAULT
        .with_login_prefilter(1_000, 1_000.0)
        .unwrap();
    let w = world_with(
        builder().build().await.unwrap(),
        config,
        HashPoolConfig::DEFAULT,
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "base.login_cooldown_after", "1000").await;
    for i in 0..40 {
        denied(login_from(&w.auth, &format!("nobody{i}"), "pw", "192.0.2.77").await);
    }
    let logins = "erp_login_throttle WHERE kind = 'login'";
    assert_eq!(
        count(&w.h, logins).await,
        i64::from(throttle::UNKNOWN_LOGIN_ROWS_PER_ADDRESS)
    );
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'address' AND failures = 40"
        )
        .await,
        1,
        "the address counts every attempt"
    );
    // a known login gets no new row from the address either (round 2 S1)
    denied(login_from(&w.auth, "bob", "wrong", "192.0.2.77").await);
    assert_eq!(count(&w.h, &login_row(&w.auth, "bob").await).await, 0);
    // a login that has a row already counts on it
    denied(login_from(&w.auth, "nobody0", "pw", "192.0.2.77").await);
    denied(login(&w.auth, "nobody-in-process", "pw").await);
    assert_eq!(
        count(&w.h, logins).await,
        i64::from(throttle::UNKNOWN_LOGIN_ROWS_PER_ADDRESS) + 1
    );
    assert_eq!(
        count(
            &w.h,
            &format!("{} AND failures = 2", login_row(&w.auth, "nobody0").await)
        )
        .await,
        1
    );
}

/// Security review round 2 S1: from addresses over the unknown-login cap, a login that names a
/// user and one that names nobody get the same throttle state, so the per-login cooldown tells
/// nothing about which logins exist: no login row for either, and a next attempt from a fresh
/// address is `Denied` for both, not `Cooldown`.
#[tokio::test]
async fn the_row_cap_does_not_tell_which_logins_exist() {
    let config = AuthConfig::DEFAULT
        .with_login_prefilter(1_000, 1_000.0)
        .unwrap();
    let w = world_with(
        builder().build().await.unwrap(),
        config,
        HashPoolConfig::DEFAULT,
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "base.login_cooldown_after", "1000").await;
    set_param(&w.h, "base.login_cooldown_after_login", "3").await;
    let cap = throttle::UNKNOWN_LOGIN_ROWS_PER_ADDRESS;
    let spent = ["198.51.100.1", "198.51.100.2", "198.51.100.3"];
    for addr in spent {
        for i in 0..cap {
            denied(login_from(&w.auth, &format!("filler{i}"), "pw", addr).await);
        }
    }
    let mut states = Vec::new();
    for who in ["bob", "ghost"] {
        for addr in spent {
            denied(login_from(&w.auth, who, "guess", addr).await);
        }
        let row = count(&w.h, &login_row(&w.auth, who).await).await;
        let next = login_from(&w.auth, who, "guess", "203.0.113.9").await;
        states.push((row, matches!(next, Err(LoginError::Cooldown))));
        denied(next);
    }
    assert_eq!(states, [(0, false), (0, false)]);
}

/// Security review Low 1, database review M4, security review round 2 N4: a success clears no
/// failure counter: not the address's, not another login's, and not its own login's (an attacker
/// spreading guesses over many addresses gets no fresh burst from the real user's logins; the
/// trusted pair keeps the real user in).
#[tokio::test]
async fn a_success_clears_no_failure_counter() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    user_with_password(&w.h, "carol", "pw").await;
    denied(login_from(&w.auth, "carol", "wrong", "192.0.2.5").await);
    denied(login_from(&w.auth, "bob", "wrong", "192.0.2.5").await);
    login_from(&w.auth, "bob", "pw", "192.0.2.5").await.unwrap();
    for (row, failures) in [
        (login_row(&w.auth, "bob").await, 1),
        (login_row(&w.auth, "carol").await, 1),
        (address_row(&w.auth, "192.0.2.5").await, 2),
    ] {
        assert_eq!(
            count(&w.h, &format!("{row} AND failures = {failures}")).await,
            1,
            "{row}"
        );
    }
}

/// Security review Low 6: a hash the pool cannot run in time is `Denied` (the same text as a wrong
/// password), and it is not counted.
#[tokio::test]
async fn a_hash_the_pool_cannot_run_is_denied_and_not_counted() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 1, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    let release = hold_permits(w.pool, 1).await;
    let refused = login_from(&w.auth, "bob", "pw", "192.0.2.9").await;
    drop(release);
    let error: Error = match refused {
        Err(LoginError::Denied) => LoginError::Denied.into(),
        other => panic!("expected Denied, got {other:?}"),
    };
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::AccessDenied, "Access Denied")
    );
    assert_eq!(w.pool.stats().overloaded, 1);
    assert_eq!(count(&w.h, "erp_login_throttle").await, 0, "not counted");
}

/// Set `password.hashing.argon2_memory_kib` to `kib` around the model (the typed API refuses a
/// value over the pool's reservation, security review round 2 M1): a row an older revision or
/// SQL wrote.
async fn store_argon2_memory(h: &erp_testkit::Harness, kib: u32) {
    let mut tx = h.begin().await.unwrap();
    let key = "password.hashing.argon2_memory_kib";
    // `init` creates no row for this key
    tx.execute("DELETE FROM ir_config_parameter WHERE key = $1", &[&key])
        .await
        .unwrap();
    tx.execute(
        "INSERT INTO ir_config_parameter (key, value) VALUES ($1, $2)",
        &[&key, &kib.to_string()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Silent-failure review S3, security review round 2 M1: current parameters that need more memory
/// than a hashing permit reserves cannot rehash a verified password. The login goes on without
/// the rehash (refusing it would lock every user out and protect nothing): a session, the stored
/// hash unchanged, the skip counted apart from an overloaded pool (`over_reservation` in the pool,
/// `rehash_skipped` in the service, logged at `error` naming the settings), no failure counted.
#[tokio::test]
async fn a_rehash_over_the_memory_reservation_logs_in_without_it() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let before = stored_hash(&w.h, bob).await;
    store_argon2_memory(&w.h, w.pool.config().memory_kib() * 2).await;
    // the service caches what `Auth::new` read (rust review round 3 RD-7): a write around the
    // model is seen once the generation moves, as after any write through the model
    make_settings_stale(&w.h).await;
    let ok = login_from(&w.auth, "bob", "pw", "192.0.2.10")
        .await
        .unwrap();
    assert_eq!(ok.uid().get(), bob);
    assert!(ok.auth().rehash(), "the check asked for a rehash");
    let stats = w.pool.stats();
    assert_eq!((stats.over_reservation, stats.overloaded), (1, 0));
    assert_eq!(w.auth.stats().rehash_skipped, 1);
    assert_eq!(stored_hash(&w.h, bob).await, before, "no rehash written");
    assert_eq!(
        count(&w.h, "erp_login_throttle WHERE kind <> 'trusted'").await,
        0,
        "not counted"
    );
    assert_eq!(count(&w.h, "erp_session").await, 1, "a session");
    // the session holds: its token covers the hash that stayed
    assert_eq!(check_user(&w.auth, &cookie(&ok)).await.unwrap(), bob);
}

#[tokio::test]
async fn over_long_passwords_cannot_be_set() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    let e = write(
        &mut tx,
        "res.users",
        &[bob],
        &[("password", s(&"p".repeat(1_025)))],
    )
    .await
    .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::UserError,
            "A password may have at most 1024 bytes."
        )
    );
    tx.rollback().await.unwrap();
    let longest = "p".repeat(1_024);
    let mut tx = w.h.begin().await.unwrap();
    write(&mut tx, "res.users", &[bob], &[("password", s(&longest))])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    login(&w.auth, "bob", &longest).await.unwrap();
}

/// Silent-failure review M3 (lead ruling e): `init` refuses a stored value shaped like a hash it
/// does not parse (another scheme, or out-of-bounds parameters), naming the user and never the
/// value, and leaves it in place; it never hashes over it.
#[tokio::test]
async fn init_refuses_a_hash_it_does_not_parse() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    // an unknown shape, another scheme, and an Argon2id hash whose memory is below the bounds
    for value in [
        "$a$b$c",
        "$6$rounds=5000$saltsalt$abcdefghijklmnopqrstuv",
        "$argon2id$v=19$m=4,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA",
    ] {
        let mut tx = w.h.begin().await.unwrap();
        tx.execute(
            "UPDATE res_users SET password = $1 WHERE id = $2",
            &[&value, &bob],
        )
        .await
        .unwrap();
        let e = erp_orm::model_hooks::call_init(&mut tx, "res.users")
            .await
            .unwrap_err();
        assert_eq!(
            (e.kind(), e.message()),
            (
                ErrorKind::Validation,
                format!(
                    "res.users {bob}: 1 stored password(s) shaped like a hash but not one this \
                     server verifies (another scheme, or parameters outside the accepted bounds) \
                     are left in place: set a new password for these users"
                )
                .as_str()
            )
        );
        assert!(!e.message().contains(value));
        tx.rollback().await.unwrap();
    }
}

/// Silent-delta N-5: every hash-shaped value is named in one refusal, so an upgrade is fixed in
/// one pass; nothing is replaced.
#[tokio::test]
async fn init_names_every_hash_shaped_user_at_once() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let ann = user_with_password(&w.h, "ann", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET password = '$a$b$c' WHERE id = ANY($1)",
        &[&vec![bob, ann]],
    )
    .await
    .unwrap();
    let e = erp_orm::model_hooks::call_init(&mut tx, "res.users")
        .await
        .unwrap_err();
    let (first, second) = (bob.min(ann), bob.max(ann));
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Validation,
            format!(
                "res.users {first}, {second}: 2 stored password(s) shaped like a hash but not one \
                 this server verifies (another scheme, or parameters outside the accepted \
                 bounds) are left in place: set a new password for these users"
            )
            .as_str()
        )
    );
    tx.rollback().await.unwrap();
}

/// Rust review Low: one hashing pool per database: setting a password hashes on the pool handed
/// to the database's `Auth`.
#[tokio::test]
async fn setting_a_password_hashes_on_the_auth_pool() {
    let w = world().await;
    let before = w.pool.stats().jobs;
    user_with_password(&w.h, "bob", "pw").await;
    assert_eq!(w.pool.stats().jobs, before + 1);
}

const CHEAP_LEGACY_HASH: &str = "$pbkdf2-sha512$1000$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Silent-failure review M1: a wrong password on a legacy (PBKDF2) hash is decided once its
/// verify ran; when the denial cost floor's dummy then cannot get a permit (its wait expires),
/// the attempt is still `Denied` and counted, and the skipped floor is counted.
///
/// The second holder queues behind the real verify with the same 1 s queue wait as the floor's
/// dummy, so the verify must take far less than 1 s: the stored hash is a cheap one
/// ([`CHEAP_LEGACY_HASH`]).
#[tokio::test]
async fn a_floor_that_cannot_run_still_counts_the_failure() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 1, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    // a legacy hash of another scheme: its denial runs the floor's dummy (lead decision 4)
    let old = user_with_password(&w.h, "old", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET password = $1 WHERE id = $2",
        &[&CHEAP_LEGACY_HASH, &old],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let parsed = StoredHash::parse(CHEAP_LEGACY_HASH).unwrap();
    assert_eq!(parsed.scheme(), password::Scheme::Pbkdf2Sha512);
    let World { h, auth, pool, .. } = w;
    let auth = Arc::new(auth);
    // the permit's queue, in order (FIFO): a first holder, the real verify, a second holder
    let first = hold_permits(pool, 1).await;
    let node = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login(&node, "old", "wrong").await });
    until_waiting(pool, 1).await;
    let second = tokio::spawn(hold_permits(pool, 1));
    until_waiting(pool, 2).await;
    // the verify runs and does not match; the second holder then keeps the permit past the
    // floor dummy's queue wait
    drop(first);
    denied(attempt.await.unwrap());
    drop(second.await.unwrap());
    let stats = pool.stats();
    assert_eq!(
        (stats.floor_skipped, stats.floor_failures),
        (1, 0),
        "an expired wait is load"
    );
    assert_eq!(
        count(
            &h,
            &format!("{} AND failures = 1", login_row(&auth, "old").await)
        )
        .await,
        1,
        "the failure counted"
    );
}

/// Security review round 2 delta SD-1, the scheme switch: `password.hashing.argon2_memory_kib`
/// above this node's reservation was written around the model while the scheme was PBKDF2 (so the
/// write check and `Auth::new` saw no Argon2id memory), then the scheme is switched to `argon2id`.
/// The reload logs the parameters over the reservation once (`settings_over_reservation`), and
/// every refusal still runs and counts as before: an unknown login verifies a dummy built within
/// the reservation (nothing refused by the pool) and is counted; a wrong password on the legacy
/// hash is denied and counted, its cost floor's dummy runs within the reservation too (silent-delta
/// S-1's floor failure no longer happens); the correct password logs in without its rehash.
#[tokio::test]
async fn a_scheme_switch_over_a_larger_stored_memory_still_counts_and_pads_refusals() {
    let w = world().await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    user_with_password(&w.h, "old", "pw").await;
    store_argon2_memory(&w.h, w.pool.config().memory_kib() * 2).await;
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    let before = w.pool.stats();
    denied(login_from(&w.auth, "nobody", "pw", "192.0.2.11").await);
    let stats = w.pool.stats();
    assert_eq!(
        (
            stats.dummy_verifies - before.dummy_verifies,
            stats.over_reservation - before.over_reservation
        ),
        (1, 0),
        "the unknown login verified a dummy within the reservation"
    );
    assert_eq!(
        count(
            &w.h,
            &format!("{} AND failures = 1", login_row(&w.auth, "nobody").await)
        )
        .await,
        1,
        "the unknown login counted"
    );
    let before = w.pool.stats();
    denied(login_from(&w.auth, "old", "wrong", "192.0.2.11").await);
    let stats = w.pool.stats();
    assert_eq!(
        (
            stats.jobs - before.jobs,
            stats.dummy_verifies - before.dummy_verifies,
            stats.floor_skipped,
            stats.floor_failures,
            stats.over_reservation - before.over_reservation
        ),
        (2, 1, 0, 0, 0),
        "the legacy verify and its floor's dummy"
    );
    assert_eq!(
        count(
            &w.h,
            &format!("{} AND failures = 1", login_row(&w.auth, "old").await)
        )
        .await,
        1,
        "the wrong password counted"
    );
    let ok = login_from(&w.auth, "old", "pw", "192.0.2.11")
        .await
        .unwrap();
    assert!(ok.auth().rehash());
    let stats = w.auth.stats();
    assert_eq!(stats.rehash_skipped, 1);
    assert_eq!(stats.settings_over_reservation, 1, "logged once");
}

/// Type design T7 (types-delta N-f: per ORM lineage): a lineage hashes on one pool; a second
/// `Auth` with another pool is refused,
/// the same pool again is accepted.
#[tokio::test]
async fn a_second_auth_with_another_pool_is_refused() {
    let w = world().await;
    let other = own_pool(HashPoolConfig::DEFAULT);
    let Err(e) = erp_security::auth::Auth::new(w.h.orm().clone(), AuthConfig::DEFAULT, other).await
    else {
        panic!("a second pool was accepted");
    };
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Programming,
            "this ORM lineage already hashes on another hashing pool (an Auth of the same ORM \
             was handed a different one): every Auth of an ORM lineage takes the same pool"
        )
    );
    let again = erp_security::auth::Auth::new(w.h.orm().clone(), AuthConfig::DEFAULT, w.pool)
        .await
        .unwrap();
    drop(again);
    // the registration of the first is still there
    let before = w.pool.stats().jobs;
    user_with_password(&w.h, "bob", "pw").await;
    assert_eq!(w.pool.stats().jobs, before + 1);
}

/// R7-RUST-1: a detached verdict retains the lineage binding after its caller and service die.
#[tokio::test]
async fn detached_login_retains_its_pool_binding() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    let World { h, auth, pool, .. } = w;
    let failed = format!("{} AND failures = 1", login_row(&auth, "bob").await);
    let release = hold_permits(pool, 1).await;
    let attempt = tokio::spawn(async move { login(&auth, "bob", "wrong").await });
    until_waiting(pool, 1).await;
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    let replacement = Auth::new(
        h.orm().clone(),
        AuthConfig::DEFAULT,
        own_pool(HashPoolConfig::DEFAULT),
    )
    .await;
    // Release before asserting so a failed regression cannot strand a blocking hash worker.
    drop(release);
    let error = replacement.expect_err("live detached work must refuse a different pool");
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "this ORM lineage already hashes on another hashing pool (an Auth of the same ORM was handed a different one): every Auth of an ORM lineage takes the same pool"
        )
    );
    until_count(&h, &failed, 1).await;
}

/// Rust review round 2 S-1, database review round 2 S4 (claude2 ruling, lead to confirm), security
/// review round 2 delta N-e: setting the passwords of a batch hashes them as many at once as the
/// hashing pool has permits but one, in the caller's transaction, so logins keep a permit: with
/// the three permits of a 3-permit pool held, a write of four passwords waits with two hashes at
/// once (one at a time would wait with one, every permit with three).
#[tokio::test]
async fn setting_passwords_of_a_batch_leaves_a_permit_for_logins() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(3, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    let mut ids = Vec::new();
    for who in ["u1", "u2", "u3", "u4"] {
        ids.push(user_with_password(&w.h, who, "pw").await);
    }
    let release = hold_permits(w.pool, 3).await;
    let mut tx = w.h.begin().await.unwrap();
    let written = ids.clone();
    let writing = tokio::spawn(async move {
        write(&mut tx, "res.users", &written, &[("password", s("new"))]).await?;
        tx.commit().await
    });
    until_waiting(w.pool, 2).await;
    assert_eq!(
        w.pool.stats().waiting,
        2,
        "two at once: the permits but one"
    );
    let jobs = w.pool.stats().jobs;
    drop(release);
    writing.await.unwrap().unwrap();
    assert_eq!(w.pool.stats().jobs - jobs, 4, "one hash per user");
    for who in ["u1", "u2", "u3", "u4"] {
        login(&w.auth, who, "new").await.unwrap();
    }
}

/// Poll `from` until it has `n` rows (the verdict task is detached: poll, never sleep blindly).
async fn until_count(h: &erp_testkit::Harness, from: &str, n: i64) {
    let started = std::time::Instant::now();
    while count(h, from).await != n {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{from} never had {n} rows"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Rust review round 2 S-3 (round 1 M2): a login future aborted after phase A (the client hung
/// up while the attempt waited for its hashing permit) still has its guess verified and counted,
/// and a correct password aborted the same way still leaves its session row.
#[tokio::test]
async fn a_login_aborted_after_phase_a_still_counts() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let World { h, auth, pool, .. } = w;
    let auth = Arc::new(auth);
    let failed = format!("{} AND failures = 1", login_row(&auth, "bob").await);
    for (password, from) in [
        ("wrong", failed.clone()),
        ("pw", format!("erp_session WHERE user_id = {bob}")),
    ] {
        let release = hold_permits(pool, 1).await;
        let node = Arc::clone(&auth);
        let attempt =
            tokio::spawn(async move { login_from(&node, "bob", password, "192.0.2.30").await });
        // phase A is done: the verdict task waits for the permit of its verify
        until_waiting(pool, 1).await;
        attempt.abort();
        assert!(attempt.await.unwrap_err().is_cancelled());
        drop(release);
        until_count(&h, &from, 1).await;
    }
    assert_eq!(count(&h, &failed).await, 1, "the failure counted once");
    assert_eq!(auth.stats().verdict_failures, 0);
}

/// Rust review round 2 S-4: the login's own failure write runs at READ COMMITTED. A holder
/// counts a failure of bob's login key and keeps the row locked; the login's phase B upsert waits
/// for it (gated on the lock wait), then counts on top of the holder's commit. At REPEATABLE READ
/// it would end in a serialization failure: one failure lost, `failure_write_errors` 1.
#[tokio::test]
async fn the_failure_write_of_a_login_runs_at_read_committed() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let World { h, auth, .. } = w;
    let auth = Arc::new(auth);
    let key = auth.throttle_key(ThrottleKey::Login("bob")).await.unwrap();
    let mut holder = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    assert_eq!(upsert_failure(&mut holder, &key).await.unwrap(), 1);
    let node = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login(&node, "bob", "wrong").await });
    until_lock_wait(&h).await;
    holder.commit().await.unwrap();
    denied(attempt.await.unwrap());
    assert_eq!(
        count(
            &h,
            &format!("{} AND failures = 2", login_row(&auth, "bob").await)
        )
        .await,
        1,
        "both failures counted"
    );
    assert_eq!(auth.stats().failure_write_errors, 0);
}

/// Security review round 2 S4: a throttle row's key is keyed by the database's secret, not the
/// plain SHA-256 of the login (a password typed into the login field is no fast unsalted hash in
/// the table): the row is found by the service's key and not by the plain hash.
#[tokio::test]
async fn throttle_keys_are_not_a_plain_hash_of_the_login() {
    let w = world().await;
    denied(login(&w.auth, "hunter2", "pw").await);
    assert_eq!(count(&w.h, &login_row(&w.auth, "hunter2").await).await, 1);
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE key_hash IN (sha256('hunter2'::bytea), \
             sha256('login'::bytea || '\\x00'::bytea || 'hunter2'::bytea))"
        )
        .await,
        0
    );
}

/// Security review round 2 delta N-a: a login restarted because its credential changed while it
/// was verified reads phase A again and applies the cooldown again before a second verify. Here
/// the password changes and the login reaches its cooldown (two failures counted elsewhere) while
/// the attempt waits for its permit: the correct old password verifies, phase B finds the hash
/// changed, and the restart answers `Cooldown` without a second verify and without counting.
#[tokio::test]
async fn a_restarted_login_applies_the_cooldown_again() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    set_param(&w.h, "base.login_cooldown_after_login", "2").await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let World { h, auth, pool, .. } = w;
    let auth = Arc::new(auth);
    let key = auth.throttle_key(ThrottleKey::Login("bob")).await.unwrap();
    let release = hold_permits(pool, 1).await;
    let node = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login(&node, "bob", "pw").await });
    // phase A is done (no cooldown then): the attempt waits for the permit of its verify
    until_waiting(pool, 1).await;
    let replaced = password::hash(b"new", &HashParams::DEFAULT).unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET password = $1 WHERE id = $2",
        &[&replaced, &bob],
    )
    .await
    .unwrap();
    upsert_failure(&mut tx, &key).await.unwrap();
    upsert_failure(&mut tx, &key).await.unwrap();
    tx.commit().await.unwrap();
    let jobs = pool.stats().jobs;
    drop(release);
    cooldown(attempt.await.unwrap());
    assert_eq!(auth.stats().login_restarts, 1);
    assert_eq!(
        pool.stats().jobs - jobs,
        1,
        "one verify, none after the restart"
    );
    assert_eq!(
        count(
            &h,
            &format!("{} AND failures = 2", login_row(&auth, "bob").await)
        )
        .await,
        1,
        "the refused restart is not counted"
    );
}

/// Backends of `h`'s database waiting for a lock.
async fn lock_waiters(h: &erp_testkit::Harness) -> i64 {
    let client = h.db().client().await.unwrap();
    client
        .query_one(
            "SELECT count(*) FROM pg_stat_activity \
              WHERE datname = current_database() AND wait_event_type = 'Lock'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// A transaction of `h` holding `ir_config_parameter` locked: a settings reload waits for it in
/// the database until it ends.
async fn settings_blocked(h: &erp_testkit::Harness) -> erp_sdk::Transaction {
    let mut holder = h.begin().await.unwrap();
    holder
        .batch("LOCK TABLE ir_config_parameter IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    holder
}

/// Wait until `n` callers of `auth` wait for the settings reload in progress.
async fn until_settings_waiting(auth: &Auth, n: u64) {
    let started = std::time::Instant::now();
    while auth.stats().settings_waiting < n {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{n} settings waiters never came: {:?}",
            auth.stats()
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Database review round 2 N8: the settings reload runs one at a time. Step by step: the first
/// caller's reload waits in the database (a holder locks `ir_config_parameter`); a second caller
/// waits for that reload in the service, not in the database (one backend waits for the lock);
/// once the holder ends, the first loads and the second finds the cache filled (one load).
#[tokio::test]
async fn the_settings_reload_runs_one_at_a_time() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    let World { h, auth, .. } = w;
    let auth = Arc::new(auth);
    // `Auth::new` cached the settings (rust review round 3 RD-7)
    make_settings_stale(&h).await;
    let loads = auth.stats().settings_loads;
    let mut holder = settings_blocked(&h).await;
    let node = Arc::clone(&auth);
    let first = tokio::spawn(async move { node.settings().await.map(|_| ()) });
    until_lock_wait(&h).await;
    let node = Arc::clone(&auth);
    let second = tokio::spawn(async move { node.settings().await.map(|_| ()) });
    until_settings_waiting(&auth, 1).await;
    assert_eq!(lock_waiters(&h).await, 1, "one reload in the database");
    assert_eq!(auth.stats().settings_loads, loads, "nothing loaded yet");
    holder.rollback().await.unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let stats = auth.stats();
    assert_eq!(
        stats.settings_loads,
        loads + 1,
        "the second found the cache filled"
    );
    assert_eq!(
        (stats.settings_waiting, stats.settings_wait_expired),
        (0, 0)
    );
}

/// Security review round 2 delta N-b and round 3 R3-c: every caller of the settings spends at
/// most the hashing pool's queue wait there, waiting for another caller's reload or running its
/// own, then fails loudly: `ResourceExhausted` with `Condition::Overloaded` from `settings` (and a
/// session check), `Overloaded` with the wait as its `Retry-After` from a login; each counted.
/// Step by step: the first caller's reload waits in the database (`ir_config_parameter` locked);
/// the second waits for it in the service (the gauge shows it); both fail after the wait. Which of
/// the two counters the second lands in depends on whether the first gave up before the second's
/// own wait ended (it then runs a reload of its own, bounded the same way), so the test pins the
/// sum and each caller's error.
#[tokio::test]
async fn a_settings_reload_wait_is_bounded_by_the_queue_wait() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 1, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    let World { h, auth, .. } = w;
    let auth = Arc::new(auth);
    make_settings_stale(&h).await;
    let mut holder = settings_blocked(&h).await;
    let node = Arc::clone(&auth);
    let first = tokio::spawn(async move { node.settings().await.map(|_| ()) });
    until_lock_wait(&h).await;
    let node = Arc::clone(&auth);
    let second = tokio::spawn(async move { node.settings().await.map(|_| ()) });
    until_settings_waiting(&auth, 1).await;
    settings_overloaded(&second.await.unwrap().unwrap_err());
    settings_overloaded(&first.await.unwrap().unwrap_err());
    match login(&auth, "bob", "pw").await {
        Err(LoginError::Overloaded { retry_after }) => {
            assert_eq!(retry_after, Duration::from_secs(1));
        }
        other => panic!("expected Overloaded, got {other:?}"),
    }
    let stats = auth.stats();
    assert_eq!(
        (
            stats.settings_wait_expired + stats.settings_load_expired,
            stats.settings_waiting
        ),
        (3, 0),
        "{stats:?}"
    );
    holder.rollback().await.unwrap();
    auth.settings().await.unwrap();
}

/// The text of an `Overloaded` settings read with a 1 s queue wait (waiting for another
/// caller's reload, or running one's own: security review round 3 R3-c).
const SETTINGS_OVERLOADED_1S: &str = "Server busy; the authentication settings could not be read \
     within the 1 s queue wait (admission.hash_queue_wait_secs); retry.";

fn settings_overloaded(e: &Error) {
    assert_eq!(
        (e.kind(), e.message(), e.condition()),
        (
            ErrorKind::ResourceExhausted,
            SETTINGS_OVERLOADED_1S,
            Some(erp_sdk::Condition::Overloaded)
        )
    );
}

/// Security review round 3 R3-c: the caller that runs the settings reload itself is bounded by
/// the queue wait too (before, only callers waiting for another's reload were): with
/// `ir_config_parameter` locked, a reload fails after the 1 s queue wait with the `Overloaded`
/// error, counted in `settings_load_expired`, keeping its cause (round 4 C4-6), and nothing is
/// cached; once the lock is gone the next caller loads.
#[tokio::test]
async fn a_settings_reload_of_its_own_is_bounded_by_the_queue_wait() {
    let w = world_with(
        builder().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::new(1, 1, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    let World { h, auth, .. } = w;
    make_settings_stale(&h).await;
    let loads = auth.stats().settings_loads;
    let mut holder = settings_blocked(&h).await;
    // the outer limit only keeps a broken bound from hanging the test run
    let e = tokio::time::timeout(Duration::from_secs(20), auth.settings())
        .await
        .expect("the reload is bounded by the queue wait")
        .unwrap_err();
    settings_overloaded(&e);
    // round 4 C4-6: the cause is kept. Which of the two fires first is timing: the lock limit
    // (the queue wait less 1 ms) runs from the statement's start, the deadline (the queue wait)
    // from the caller's first look at the cache, before the connection and `BEGIN`.
    let source = std::error::Error::source(&e).expect("the cause as the source");
    if let Some(limit) = source.downcast_ref::<Error>() {
        assert_eq!(
            limit.condition(),
            Some(erp_sdk::Condition::LockTimeout),
            "{limit:?}"
        );
    } else {
        assert!(
            source.is::<tokio::time::error::Elapsed>(),
            "neither the lock limit nor the deadline: {source:?}"
        );
    }
    let stats = auth.stats();
    assert_eq!(
        (
            stats.settings_load_expired,
            stats.settings_wait_expired,
            stats.settings_loads
        ),
        (1, 0, loads)
    );
    holder.rollback().await.unwrap();
    auth.settings().await.unwrap();
    assert_eq!(auth.stats().settings_loads, loads + 1);
}

/// Rust review round 3 RD-7: `Auth::new` keeps the settings and keys it read at start as the
/// first cached generation (labelled with the generation read before that read), so the first
/// login does not read them again; a settings write still makes the next login reload.
#[tokio::test]
async fn the_first_login_uses_the_settings_auth_new_read() {
    let w = world().await;
    assert_eq!(w.auth.stats().settings_loads, 1, "the load of Auth::new");
    user_with_password(&w.h, "bob", "pw").await;
    login(&w.auth, "bob", "pw").await.unwrap();
    assert_eq!(w.auth.stats().settings_loads, 1, "the first login reloaded");
    make_settings_stale(&w.h).await;
    login(&w.auth, "bob", "pw").await.unwrap();
    assert_eq!(w.auth.stats().settings_loads, 2, "a write reloads");
}

/// Round 6 C5-2: a refused blocking request of the other side panics with the reason, not a bare
/// unwrap of the 40P01.
#[test]
#[should_panic(
    expected = "the other side was the deadlock victim (sqlstate Some(\"40P01\")): \
                           the arranged login-victim ordering was not reached: deadlock detected"
)]
fn a_refused_other_side_says_it_was_the_victim() {
    let refused: Result<(), Error> = Err(Error::new(ErrorKind::Conflict, "deadlock detected")
        .with_sqlstate(Some("40P01".to_owned())));
    the_other_side_won(refused);
}

/// Database review round 3 DD-1, rust review round 3 RD-5, the success side: the success
/// statement of a login is retried after a deadlock (40P01), and the retry is counted. Bob has
/// his pair from 192.0.2.80 and 16 other trusted pairs, the oldest 40 days old, so the success
/// statement removes that one (16 pairs per login). Step by step, deterministic within the
/// login's `deadlock_timeout` (the gate checks it):
/// 1. the other side removes the old pair first, as the collection's `TRUSTED_SQL` would
///    (oldest first), and keeps its transaction open;
/// 2. the login's success statement locks bob's user row (`FOR SHARE`, its first step) and waits
///    for the old pair (gated on the lock wait);
/// 3. the other side asks for bob's user row (`FOR UPDATE`, as a password change does): a cycle.
///    The login waited first, so its deadlock check runs first and it is the one refused (40P01),
///    as long as the request arrives within the login's `deadlock_timeout` (checked by the gate,
///    [`until_the_login_waits`], round 4 C4-3);
/// 4. the other side commits; the login's retry runs after it and succeeds.
///
/// The collection itself takes no user row; it is the other side's second step here because the
/// order in which PostgreSQL runs the statement's two pair-changing CTEs is not pinned, so a cycle
/// over two pairs could not be forced deterministically. What is under test is the retry of the
/// success statement.
#[tokio::test]
async fn a_deadlocked_login_success_is_retried() {
    let (h, auth, orm) = deadlock_world().await;
    let bob = user_with_password(&h, "bob", "pw").await;
    let mut keys = Vec::new();
    for n in 80..=96 {
        let addr = throttle::address_key(format!("192.0.2.{n}").parse().unwrap());
        keys.push(
            auth.throttle_key(ThrottleKey::Trusted(&addr, "bob"))
                .await
                .unwrap()
                .to_vec(),
        );
    }
    let mut tx = h.begin().await.unwrap();
    for (n, key) in keys.iter().enumerate() {
        // the login's own pair an hour old, the oldest other pair 40 days, the rest a day
        let age = match n {
            0 => "1 hour",
            1 => "40 days",
            _ => "1 day",
        };
        tx.execute(
            &format!(
                "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at) \
                 VALUES ('trusted', $1, 1, now() - interval '{age}')"
            ),
            &[key],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let mut other = other_side(&orm).await;
    let removed = other
        .execute(
            "DELETE FROM erp_login_throttle WHERE kind = 'trusted' AND key_hash = $1",
            &[&keys[1]],
        )
        .await
        .unwrap();
    assert_eq!(removed, 1);
    let node = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login_from(&node, "bob", "pw", "192.0.2.80").await });
    until_the_login_waits(&mut other).await;
    // waits for the login's share lock until the login is refused by the deadlock check
    the_other_side_won(
        other
            .query("SELECT 1 FROM res_users WHERE id = $1 FOR UPDATE", &[&bob])
            .await,
    );
    other.commit().await.unwrap();
    let ok = attempt.await.unwrap().unwrap();
    assert_eq!(ok.uid().get(), bob);
    let stats = auth.stats();
    assert_eq!(stats.deadlock_retries.success, 1, "{stats:?}");
    assert_eq!(stats.verdict_failures, 0);
    assert_eq!(count(&h, "erp_session").await, 1);
    assert_eq!(
        count(&h, "erp_login_throttle WHERE kind = 'trusted'").await,
        16,
        "the login's pair and 15 others"
    );
}

/// Database review round 3 DD-1, rust review round 3 RD-5, the failure side: the failure count of
/// a login is retried after a deadlock (40P01) and counted once. Step by step, deterministic
/// within the login's `deadlock_timeout` (the gate checks it):
/// 1. one failure of bob from 192.0.2.90 makes the address row and the login row;
/// 2. the other side removes the login row first, as the collection's `THROTTLE_SQL` would
///    (oldest first), and keeps its transaction open;
/// 3. a wrong password: the failure statement upserts the address row (key order: address
///    first) and waits for the login row (gated on the lock wait);
/// 4. the other side removes the address row: a cycle. The login waited first, so it is the one
///    refused (40P01), as long as the request arrives within the login's `deadlock_timeout`
///    (checked by the gate, [`until_the_login_waits`], round 4 C4-3); the other side commits, the
///    retry counts on fresh rows.
#[tokio::test]
async fn a_deadlocked_failure_count_is_retried() {
    let (h, auth, orm) = deadlock_world().await;
    user_with_password(&h, "bob", "pw").await;
    denied(login_from(&auth, "bob", "wrong", "192.0.2.90").await);
    let address = address_row(&auth, "192.0.2.90").await;
    let login_key = login_row(&auth, "bob").await;
    let mut other = other_side(&orm).await;
    let removed = other
        .execute(&format!("DELETE FROM {login_key}"), &[])
        .await
        .unwrap();
    assert_eq!(removed, 1);
    let node = Arc::clone(&auth);
    let attempt =
        tokio::spawn(async move { login_from(&node, "bob", "wrong", "192.0.2.90").await });
    until_the_login_waits(&mut other).await;
    // waits for the login's row lock until the login is refused by the deadlock check
    let removed = the_other_side_won(other.execute(&format!("DELETE FROM {address}"), &[]).await);
    assert_eq!(removed, 1);
    other.commit().await.unwrap();
    denied(attempt.await.unwrap());
    let stats = auth.stats();
    assert_eq!(stats.deadlock_retries.failure_write, 1, "{stats:?}");
    assert_eq!(stats.failure_write_errors, 0);
    for row in [&address, &login_key] {
        assert_eq!(
            count(&h, &format!("{row} AND failures = 1")).await,
            1,
            "{row}: counted once, after the other side's delete"
        );
    }
}
