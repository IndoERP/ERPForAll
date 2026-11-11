//! horizontal story (`scale-out.md` rows
//! "Sessions" and "Login throttle"): two nodes (two `Orm`s with signals, each with its own pool,
//! hashing pool and `Auth`) on one database. Any node checks any session; failures through both
//! nodes add up; a `database.secret` change through one node refuses the other node's next
//! check of an old session (its cached keys go stale in the opening flight); a logout through
//! one node is final on the other. The OS-process form waits for SCALE.TEST.
//!
//! Untagged by design: these are the horizontal story's cases of `scale-out.md` §2 (rows
//! "Sessions" and "Login throttle") and design note §14.12, not spec cases; the spec cases they
//! touch are tagged where their whole behaviour is tested on one node (rust review Low).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

use auth_support::*;
use erp_sdk::{Error, ErrorKind};
use erp_security::auth::password::limits;
use erp_security::auth::{
    Auth, CredentialOptions, HashPoolConfig, HashPoolError, LoginError, SessionSecret, StoredHash,
};
use erp_testkit::Harness;

/// `line` with the 8-hex-digit login tag after "login key " replaced by `TAG`.
fn untagged(line: &str) -> String {
    let Some((head, rest)) = line.split_once("login key ") else {
        return line.to_owned();
    };
    let (tag, tail) = rest.split_at(8);
    assert!(tag.bytes().all(|b| b.is_ascii_hexdigit()), "{line}");
    format!("{head}login key TAG{tail}")
}

/// A node: its own ORM with signals, a pool of 4 connections, a hashing pool and service.
async fn node(h: &Harness) -> Auth {
    node_with(h, 4, own_pool(HashPoolConfig::DEFAULT)).await
}

/// A database ready for logins with the signal table, and two nodes.
async fn two_nodes() -> (Harness, Auth, Auth) {
    let h = builder().build().await.expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    prepare(&h).await;
    let a = node(&h).await;
    let b = node(&h).await;
    (h, a, b)
}

async fn refused(auth: &Auth, secret: &SessionSecret) -> String {
    match auth.begin_session(secret, CredentialOptions::new()).await {
        Err(e) => {
            assert_eq!(e.kind(), ErrorKind::SessionExpired);
            e.message().to_owned()
        }
        Ok(begun) => panic!("session {:?} accepted", begun.session),
    }
}

async fn accepted(auth: &Auth, secret: &SessionSecret) -> i64 {
    check_user(auth, secret).await.unwrap()
}

/// A typed-API write through `auth`'s node, as the system.
async fn set_param_on(auth: &Auth, key: &str, value: &str) {
    let mut tx = auth.orm().begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("ir.config_parameter").unwrap();
    let _: String = records
        .call(&mut tx, "set_str", (key.to_owned(), Some(value.to_owned())))
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Multi-node: a session made through one node is checked through the other.
#[tokio::test]
async fn any_node_checks_any_session() {
    let (h, a, b) = two_nodes().await;
    let bob = user_with_password(&h, "bob", "pw").await;
    let ok = login(&a, "bob", "pw").await.unwrap();
    assert_eq!(accepted(&b, &cookie(&ok)).await, bob);
    assert_eq!(accepted(&a, &cookie(&ok)).await, bob);
}

/// Multi-node: failures through both nodes add up to one cooldown.
#[tokio::test]
async fn failures_add_up_across_nodes() {
    let (h, a, b) = two_nodes().await;
    user_with_password(&h, "bob", "pw").await;
    set_param_on(&a, "base.login_cooldown_after", "3").await;
    // node b started before the write and has only logged users in since (rust review round 3
    // RD-7: its settings are those `Auth::new` read): it applies the signal first
    apply_signals(&b).await;
    for (node, n) in [(&a, 0), (&b, 1), (&a, 2)] {
        let r = login_from(node, "bob", "wrong", "192.0.2.50").await;
        assert!(matches!(r, Err(LoginError::Denied)), "attempt {n}: {r:?}");
    }
    let r = login_from(&b, "bob", "pw", "192.0.2.50").await;
    assert!(matches!(r, Err(LoginError::Cooldown)), "{r:?}");
}

/// Multi-node: a new `database.secret` written through one node reaches the other at its next
/// opening flight; the old session is refused there, after one reload of the keys.
#[tokio::test]
async fn a_secret_change_reaches_the_other_node() {
    let (h, a, b) = two_nodes().await;
    user_with_password(&h, "bob", "pw").await;
    let ok = login(&a, "bob", "pw").await.unwrap();
    accepted(&b, &cookie(&ok)).await;
    let loads = b.stats().settings_loads;
    set_param_on(
        &a,
        "database.secret",
        "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
    )
    .await;
    assert_eq!(refused(&b, &cookie(&ok)).await, "user is not connected");
    assert_eq!(
        b.stats().settings_loads,
        loads + 1,
        "one reload of the keys"
    );
    let fresh = login(&b, "bob", "pw").await.unwrap();
    accepted(&a, &cookie(&fresh)).await;
}

/// Multi-node: a logout through one node is final on the other.
#[tokio::test]
async fn logout_through_one_node_is_final_on_the_other() {
    let (h, a, b) = two_nodes().await;
    user_with_password(&h, "bob", "pw").await;
    let ok = login(&a, "bob", "pw").await.unwrap();
    accepted(&b, &cookie(&ok)).await;
    assert!(a.logout(ok.session().id()).await.unwrap());
    assert_eq!(refused(&b, &cookie(&ok)).await, "user is not connected");
}

/// Security review round 2 N2: a session idle past the old `sessions.max_inactivity_seconds` but
/// within the new one, raised through the other node, is accepted by a node whose cached settings
/// go stale in the opening flight: the stale cache is noticed before any refusal, so the session
/// is not refused and deleted under the old limit.
#[tokio::test]
async fn a_stale_cache_does_not_end_a_session_the_new_settings_accept() {
    let (h, a, b) = two_nodes().await;
    let bob = user_with_password(&h, "bob", "pw").await;
    set_param_on(&a, "sessions.max_inactivity_seconds", "3600").await;
    let ok = login(&a, "bob", "pw").await.unwrap();
    // node b caches the one-hour limit
    assert_eq!(accepted(&b, &cookie(&ok)).await, bob);
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE erp_session SET last_seen_at = now() - interval '2 hours' WHERE user_id = $1",
        &[&bob],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    set_param_on(&a, "sessions.max_inactivity_seconds", "86400").await;
    assert_eq!(accepted(&b, &cookie(&ok)).await, bob);
    assert_eq!(
        count(&h, &format!("erp_session WHERE user_id = {bob}")).await,
        1
    );
    assert_eq!(b.stats().refusal_delete_failures, 0);
}

/// DB7-1: collection's own opening signal must refresh its destructive idle policy.
#[tokio::test]
async fn gc_refreshes_idle_settings_before_deleting() {
    let (h, a, b) = two_nodes().await;
    let bob = user_with_password(&h, "bob", "pw").await;
    set_param_on(&a, "sessions.max_inactivity_seconds", "3600").await;
    let ok = login(&a, "bob", "pw").await.unwrap();
    assert_eq!(accepted(&b, &cookie(&ok)).await, bob);
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE erp_session SET last_seen_at = now() - interval '2 hours' WHERE user_id = $1",
        &[&bob],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    set_param_on(&a, "sessions.max_inactivity_seconds", "86400").await;
    let report = b
        .gc_from(erp_security::auth::gc::GcCursor::START)
        .await
        .unwrap();
    assert_eq!(report.idle_sessions, 0);
    assert_eq!(accepted(&b, &cookie(&ok)).await, bob);
}

/// DB7-1: a stale shorter cooldown must not erase counters under the refreshed policy.
#[tokio::test]
async fn gc_refreshes_cooldown_before_deleting_counters() {
    let (h, a, b) = two_nodes().await;
    set_param_on(&a, "base.login_cooldown_duration", "60").await;
    apply_signals(&b).await;
    b.settings().await.unwrap();
    let key = a
        .throttle_key(erp_security::auth::ThrottleKey::Login("missing"))
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at) VALUES ('login', $1, 1, now() - interval '3 minutes')",
        &[&key.as_slice()],
    ).await.unwrap();
    tx.commit().await.unwrap();
    set_param_on(&a, "base.login_cooldown_duration", "3600").await;
    b.gc_from(erp_security::auth::gc::GcCursor::START)
        .await
        .unwrap();
    assert_eq!(login_failures(&h, &a, "missing").await, 1);
}

/// Apply the signals pending for `auth`'s node: one statement in a transaction of its own (its
/// opening flight reads the generations), rolled back. A login's one-flight transactions read no
/// signal, so a node that has only logged users in since another node's settings write still
/// holds its older settings; any other transaction of the node brings them up to date.
async fn apply_signals(auth: &Auth) {
    let mut tx = auth.orm().begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
    tx.rollback().await.unwrap();
}

/// Failures counted on the login row of `login` (keyed by `auth`'s database secret), 0 without a
/// row.
async fn login_failures(h: &Harness, auth: &Auth, login: &str) -> i32 {
    let key = auth
        .throttle_key(erp_security::auth::ThrottleKey::Login(login))
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT failures FROM erp_login_throttle WHERE kind = 'login' AND key_hash = $1",
            &[&key.as_slice()],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows.first().map_or(0, |r| r.get(0))
}

/// Security review round 2 delta SD-1, mixed reservations: node `big` reserves twice the memory
/// per hashing permit that node `small` reserves (a deployment that broke the rule "every node has
/// the same `admission.hash_memory_kib`", `scale-out.md` §4). A hashing memory only `big` can run
/// is written through `big` (its write check accepts it), and a user who logs in on `big` is
/// rehashed to it. On `small`:
/// - its settings reload logs the parameters over its reservation once per settings generation
///   (counted);
/// - an unknown login verifies a dummy built within its reservation (the pool refuses nothing) and
///   is counted, as on any node;
/// - the rehashed user's correct password is refused like a wrong one (`Access Denied`), counted,
///   with a dummy verify for the time, never verified (the pool would refuse it).
#[tokio::test]
async fn a_node_with_a_smaller_hashing_reservation_counts_every_refusal() {
    let h = builder().build().await.expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    prepare(&h).await;
    let big_kib = 2 * limits::ARGON2_MEMORY_KIB_MIN;
    let big_pool = own_pool(HashPoolConfig::new(2, 5, big_kib).unwrap());
    let small_pool = own_pool(HashPoolConfig::DEFAULT);
    let big = node_with(&h, 4, big_pool).await;
    let small = node_with(&h, 4, small_pool).await;
    let bob = user_with_password(&h, "bob", "pw").await;
    set_param_on(
        &big,
        "password.hashing.argon2_memory_kib",
        &big_kib.to_string(),
    )
    .await;
    let rehashed = login(&big, "bob", "pw").await.unwrap();
    assert!(rehashed.auth().rehash());
    let stored = stored_hash(&h, bob).await.unwrap();
    assert_eq!(StoredHash::parse(&stored).unwrap().memory_kib(), big_kib);
    // the small node applies the write's signal in a transaction of its own (a login's one-flight
    // transactions read no signal), so exactly one new settings generation follows its start
    // (security review round 3 R3-f)
    apply_signals(&small).await;
    let loads = small.stats().settings_loads;
    // an unknown login on the small node
    let before = small_pool.stats();
    let refused = login_from(&small, "nobody", "pw", "192.0.2.70").await;
    assert!(matches!(refused, Err(LoginError::Denied)), "{refused:?}");
    let after = small_pool.stats();
    assert_eq!(
        (
            after.dummy_verifies - before.dummy_verifies,
            after.over_reservation - before.over_reservation
        ),
        (1, 0),
        "a dummy within the small node's reservation"
    );
    assert_eq!(login_failures(&h, &small, "nobody").await, 1, "counted");
    // the rehashed user on the small node: the answer of a wrong password, counted
    let before = small_pool.stats();
    let refused: erp_sdk::Error = match login_from(&small, "bob", "pw", "192.0.2.71").await {
        Err(e @ LoginError::Denied) => e.into(),
        other => panic!("expected Denied, got {other:?}"),
    };
    assert_eq!(
        (refused.kind(), refused.message()),
        (ErrorKind::AccessDenied, "Access Denied")
    );
    let after = small_pool.stats();
    assert_eq!(
        (
            after.dummy_verifies - before.dummy_verifies,
            after.over_reservation - before.over_reservation
        ),
        (1, 1),
        "refused by the pool, padded with a dummy"
    );
    assert_eq!(login_failures(&h, &small, "bob").await, 1, "counted");
    // logged once per settings generation: the one generation after the write was loaded once
    // and logged once, and more logins neither reload nor log again
    let stats = small.stats();
    assert_eq!(
        (
            stats.settings_loads - loads,
            stats.settings_over_reservation
        ),
        (1, 1),
        "{stats:?}"
    );
    for _ in 0..2 {
        let refused = login_from(&small, "nobody", "pw", "192.0.2.70").await;
        assert!(matches!(refused, Err(LoginError::Denied)), "{refused:?}");
    }
    let stats = small.stats();
    assert_eq!(
        (
            stats.settings_loads - loads,
            stats.settings_over_reservation
        ),
        (1, 1),
        "{stats:?}"
    );
    assert_eq!(big.stats().settings_over_reservation, 0);
    // the big node still logs bob in
    login(&big, "bob", "pw").await.unwrap();
}

/// Security review round 3 R3-a and R3-b, mixed reservations under a saturated hashing pool:
/// the small node's only permit is held and its queue wait is 1 s. A user whose stored hash only
/// the big node can run is refused by the small node's pool at once, and the dummy that pads that
/// refusal then waits for a permit like an unknown login's dummy does; when that wait expires,
/// both attempts get the same answer and neither is counted (before, the known user's refusal
/// was counted and the unknown login's was not). The padding's own counter names the case, not
/// the cost floor's.
#[tokio::test]
async fn under_a_full_pool_an_over_reservation_refusal_counts_like_an_unknown_login() {
    let h = builder().build().await.expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    prepare(&h).await;
    let big_kib = 2 * limits::ARGON2_MEMORY_KIB_MIN;
    let big = node_with(&h, 4, own_pool(HashPoolConfig::new(2, 5, big_kib).unwrap())).await;
    let small_pool = own_pool(HashPoolConfig::new(1, 1, limits::ARGON2_MEMORY_KIB_MIN).unwrap());
    let small = node_with(&h, 4, small_pool).await;
    let bob = user_with_password(&h, "bob", "pw").await;
    set_param_on(
        &big,
        "password.hashing.argon2_memory_kib",
        &big_kib.to_string(),
    )
    .await;
    assert!(login(&big, "bob", "pw").await.unwrap().auth().rehash());
    let stored = stored_hash(&h, bob).await.unwrap();
    assert_eq!(StoredHash::parse(&stored).unwrap().memory_kib(), big_kib);
    let release = hold_permits(small_pool, 1).await;
    let before = small_pool.stats();
    let warnings = warn_capture::start();
    for who in ["bob", "nobody"] {
        let refused = login_from(&small, who, "pw", "192.0.2.72").await;
        assert!(
            matches!(refused, Err(LoginError::Denied)),
            "{who}: {refused:?}"
        );
    }
    // round 4 C4-9: the padding's own line, naming `admission.hash_memory_kib` (R3-b), then each
    // attempt's answer, not counted; a login's tag is keyed by the database's secret, so it is
    // checked for its shape and replaced
    let overloaded = Error::from(HashPoolError::Overloaded {
        retry_after_secs: 1,
    });
    let answered = format!(
        "Login for login key TAG answered Denied, not counted: the verify could not run: \
         {overloaded}"
    );
    assert_eq!(
        warnings
            .take()
            .iter()
            .map(|line| untagged(line))
            .collect::<Vec<_>>(),
        [
            format!(
                "the dummy verify that pads the refusal of user {bob}'s stored hash (over this \
                 node's admission.hash_memory_kib) could not run; the attempt is answered as an \
                 unknown login's: {overloaded}"
            ),
            answered.clone(),
            answered,
        ]
    );
    drop(warnings);
    let after = small_pool.stats();
    assert_eq!(
        (
            after.over_reservation - before.over_reservation,
            after.overloaded - before.overloaded,
            after.over_reservation_pad_skipped - before.over_reservation_pad_skipped,
            after.floor_skipped - before.floor_skipped,
            after.dummy_verifies - before.dummy_verifies
        ),
        (1, 2, 1, 0, 0),
        "bob refused by the pool, both dummies past the queue wait, counted as padding"
    );
    drop(release);
    assert_eq!(login_failures(&h, &small, "bob").await, 0, "not counted");
    assert_eq!(login_failures(&h, &small, "nobody").await, 0, "not counted");
    assert_eq!(
        count(&h, "erp_login_throttle WHERE kind = 'address'").await,
        0,
        "the address not counted either"
    );
}
