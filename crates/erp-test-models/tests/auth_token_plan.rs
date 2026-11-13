//! the session token field plan,
//! `res.users._get_session_token_fields`. The fixture addons `test_token` and `test_token_more`
//! override it (see their module docs). Each test shows a behavior of the plan: a changed added
//! field ends sessions like a base field, unrelated and `NULL` fields do not, two addons chain,
//! issuance and validation agree, a refused plan stops the service from being built, and the
//! denial is the one every other refusal gives.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

use std::sync::Arc;

use auth_support::*;
use erp_sdk::{Error, ErrorKind};
use erp_security::auth::apikey::ApiKeyHasher;
use erp_security::auth::apikey_store::{IssueRequest, IssuedSecret, KeyKind, KeyScope, issue};
use erp_security::auth::password::limits;
use erp_security::auth::secret::{DatabaseSecret, SessionKeys};
use erp_security::auth::session::{ExtraKind, ExtraValue, TokenFields};
use erp_security::auth::test_support;
use erp_security::auth::{
    ApiKeyRequest, Auth, AuthConfig, CredentialOptions, HashPoolConfig, IdentityCheck,
    PasswordReset, SessionId, SessionSecret,
};
use erp_test_models::{test_login_selection, test_token, test_token_more};
use erp_testkit::{Harness, assert_round_trips};

/// Serializes building an `Auth` with a scenario (the plan is resolved once, when it is built).
static SCENARIO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const GONE: (ErrorKind, &str) = (ErrorKind::SessionExpired, "user is not connected");

async fn harness(more: bool) -> Harness {
    let mut addons = vec![
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &test_token::ADDON,
    ];
    if more {
        addons.push(&test_token_more::ADDON);
    }
    Harness::builder(addons)
        .with_decls([erp_data::ir_model_data_decl()])
        .require_core_tables(erp_security::auth::core_tables())
        // key issuance writes an audited event: fixtures get an operation identity
        .identify_transactions()
        .build()
        .await
        .expect("harness")
}

/// The harness, and what building the `Auth` for `scenario` gave.
async fn try_world(
    scenario: &'static str,
    more: bool,
    pool: HashPoolConfig,
) -> (Harness, Result<Auth, Error>) {
    let h = harness(more).await;
    prepare(&h).await;
    let _serial = SCENARIO.lock().await;
    test_token::set_scenario(scenario);
    let auth = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, own_pool(pool)).await;
    (h, auth)
}

/// A world whose plan is `scenario`'s (which must be accepted).
async fn token_world(scenario: &'static str, more: bool) -> World {
    token_world_with(scenario, more, HashPoolConfig::DEFAULT).await
}

async fn token_world_with(scenario: &'static str, more: bool, pool: HashPoolConfig) -> World {
    let h = harness(more).await;
    let admin = prepare(&h).await;
    let pool = own_pool(pool);
    let _serial = SCENARIO.lock().await;
    test_token::set_scenario(scenario);
    let auth = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, pool)
        .await
        .unwrap();
    World {
        h,
        auth,
        pool,
        admin,
    }
}

async fn sql(h: &Harness, statement: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(statement).await.unwrap();
    tx.commit().await.unwrap();
}

async fn set_user(h: &Harness, uid: i64, assignment: &str) {
    sql(
        h,
        &format!("UPDATE res_users SET {assignment} WHERE id = {uid}"),
    )
    .await;
}

/// The session `secret` is refused with the one `SessionExpired` answer.
async fn gone(auth: &Auth, secret: &SessionSecret, what: &str) {
    match check_user(auth, secret).await {
        Err(e) => assert_eq!((e.kind(), e.message()), GONE, "{what}"),
        Ok(uid) => panic!("{what}: the session was accepted for user {uid}"),
    }
}

async fn sessions_of(h: &Harness, uid: i64) -> i64 {
    count(h, &format!("erp_session WHERE user_id = {uid}")).await
}

/// The token a session of `uid` has, as the production encoder makes it for `extra`, with the
/// user's current base fields and the key the stored token names.
async fn expected_token(h: &Harness, uid: i64, id: SessionId, extra: &[ExtraValue<'_>]) -> Vec<u8> {
    let mut tx = h.begin().await.unwrap();
    let user = tx
        .query(
            "SELECT login, password, active FROM res_users WHERE id = $1",
            &[&uid],
        )
        .await
        .unwrap();
    let login: String = user[0].get(0);
    let password: Option<String> = user[0].get(1);
    let active: bool = user[0].get(2);
    let secret = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key = 'database.secret'",
            &[],
        )
        .await
        .unwrap();
    let secret: String = secret[0].get(0);
    let stored = tx
        .query(
            "SELECT token_kid FROM erp_session WHERE id = $1::text::uuid",
            &[&id.to_text()],
        )
        .await
        .unwrap();
    let kid: i16 = stored[0].get(0);
    tx.rollback().await.unwrap();
    let keys = SessionKeys::derive(&DatabaseSecret::from_stored(&secret).unwrap()).unwrap();
    let fields = TokenFields {
        user: erp_orm::UserId::new(uid).unwrap(),
        login: &login,
        password: password.as_deref().unwrap_or(""),
        active,
        extra,
    };
    test_support::token(keys.key(kid).unwrap(), &id, &fields)
        .unwrap()
        .to_vec()
}

async fn stored_token(h: &Harness, id: SessionId) -> Vec<u8> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT token FROM erp_session WHERE id = $1::text::uuid",
            &[&id.to_text()],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

fn text<'a>(name: &'a str, value: Option<&'a str>) -> ExtraValue<'a> {
    ExtraValue {
        name,
        kind: ExtraKind::Text,
        value,
    }
}

fn int<'a>(name: &'a str, value: Option<&'a str>) -> ExtraValue<'a> {
    ExtraValue {
        name,
        kind: ExtraKind::Int,
        value,
    }
}

/// ordinary check; chaining, cross-node, cleanup and identity are separate tests)
#[tokio::test]
async fn a_changed_added_field_ends_the_existing_sessions() {
    let w = token_world("ok", false).await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let first = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    assert_eq!(check_user(&w.auth, &first).await.unwrap(), bob);

    // NULL -> a value: the token made without the field no longer matches
    set_user(&w.h, bob, "tok_a = 'x'").await;
    gone(&w.auth, &first, "after the field was set").await;
    assert_eq!(sessions_of(&w.h, bob).await, 0, "the row is deleted");

    // a session issued with the value passes; another value ends it
    let second = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    assert_eq!(check_user(&w.auth, &second).await.unwrap(), bob);
    set_user(&w.h, bob, "tok_a = 'y'").await;
    gone(&w.auth, &second, "after the value changed").await;

    // a value -> NULL ends it too
    let third = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    assert_eq!(check_user(&w.auth, &third).await.unwrap(), bob);
    set_user(&w.h, bob, "tok_a = NULL").await;
    gone(&w.auth, &third, "after the field was cleared").await;
}

/// sessions valid)
#[tokio::test]
async fn unchanged_fields_keep_the_sessions_valid() {
    let w = token_world("ok", false).await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let before = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    // a field in no plan, and the display name: not part of the token
    set_user(&w.h, bob, "tok_other = 'z', name = 'Robert'").await;
    assert_eq!(check_user(&w.auth, &before).await.unwrap(), bob);
    // a session issued while the added field is set survives changes of the others
    set_user(&w.h, bob, "tok_a = 'x'").await;
    let with_value = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    set_user(&w.h, bob, "tok_other = 'w'").await;
    assert_eq!(check_user(&w.auth, &with_value).await.unwrap(), bob);
    // writing the same value again changes nothing
    set_user(&w.h, bob, "tok_a = 'x'").await;
    assert_eq!(check_user(&w.auth, &with_value).await.unwrap(), bob);
}

/// check expects it, and is not the base-only token)
#[tokio::test]
async fn issuance_and_validation_agree_on_the_plan() {
    let w = token_world("ok", false).await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_user(&w.h, bob, "tok_a = 'x'").await;
    let logged = login(&w.auth, "bob", "pw").await.unwrap();
    let id = logged.session().id();
    assert_eq!(check_user(&w.auth, &cookie(&logged)).await.unwrap(), bob);
    let stored = stored_token(&w.h, id).await;
    assert_eq!(
        stored,
        expected_token(&w.h, bob, id, &[text("tok_a", Some("x"))]).await,
        "the issued token covers tok_a"
    );
    assert_ne!(
        stored,
        expected_token(&w.h, bob, id, &[]).await,
        "the issued token is not the base-only token"
    );

    // an added field that is NULL contributes nothing: the base-only token
    set_user(&w.h, bob, "tok_a = NULL").await;
    let plain = login(&w.auth, "bob", "pw").await.unwrap();
    assert_eq!(
        stored_token(&w.h, plain.session().id()).await,
        expected_token(&w.h, bob, plain.session().id(), &[]).await
    );
}

#[tokio::test]
async fn the_base_plan_issues_the_same_token_as_before() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let logged = login(&w.auth, "bob", "pw").await.unwrap();
    let id = logged.session().id();
    assert_eq!(
        stored_token(&w.h, id).await,
        expected_token(&w.h, bob, id, &[]).await
    );
    assert_eq!(check_user(&w.auth, &cookie(&logged)).await.unwrap(), bob);
}

/// addon's field and the first's each end sessions)
#[tokio::test]
async fn two_addons_chain_their_fields_through_super() {
    let w = token_world("ok", true).await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let logged = login(&w.auth, "bob", "pw").await.unwrap();
    let secret = cookie(&logged);
    // the second addon's field (tok_n)
    set_user(&w.h, bob, "tok_n = 7").await;
    gone(&w.auth, &secret, "after the second addon's field was set").await;
    // both set: the token covers both, in name order
    set_user(&w.h, bob, "tok_a = 'x'").await;
    let both = login(&w.auth, "bob", "pw").await.unwrap();
    let id = both.session().id();
    assert_eq!(
        stored_token(&w.h, id).await,
        expected_token(
            &w.h,
            bob,
            id,
            &[text("tok_a", Some("x")), int("tok_n", Some("7"))]
        )
        .await
    );
    assert_eq!(check_user(&w.auth, &cookie(&both)).await.unwrap(), bob);
    // the first addon's field
    set_user(&w.h, bob, "tok_a = 'y'").await;
    gone(
        &w.auth,
        &cookie(&both),
        "after the first addon's field changed",
    )
    .await;
    // a base field still ends sessions
    let again = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    set_user(&w.h, bob, "login = 'robert'").await;
    gone(&w.auth, &again, "after the login changed").await;
}

/// and whether or not the session exists)
#[tokio::test]
async fn the_denial_is_uniform() {
    let w = token_world("ok", false).await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let by_added = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    set_user(&w.h, bob, "tok_a = 'x'").await;
    let added = check_user(&w.auth, &by_added).await.unwrap_err();

    let unknown = SessionSecret::generate().unwrap();
    let missing = check_user(&w.auth, &unknown).await.unwrap_err();

    set_user(&w.h, bob, "tok_a = NULL").await;
    let by_login = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    set_user(&w.h, bob, "login = 'bobby'").await;
    let base = check_user(&w.auth, &by_login).await.unwrap_err();

    for (what, e) in [("added", &added), ("unknown", &missing), ("base", &base)] {
        assert_eq!((e.kind(), e.message()), GONE, "{what}");
    }
}

/// node's own writes ends the session there at its next request)
#[tokio::test]
async fn a_second_service_sees_a_change_at_once() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    prepare(&h).await;
    let bob = user_with_password(&h, "bob", "pw").await;
    let (a, b) = {
        let _serial = SCENARIO.lock().await;
        test_token::set_scenario("ok");
        (
            node_with(&h, 3, waiting_pool(2)).await,
            node_with(&h, 3, waiting_pool(2)).await,
        )
    };
    let secret = cookie(&login(&a, "bob", "pw").await.unwrap());
    assert_eq!(check_user(&b, &secret).await.unwrap(), bob);
    // committed by plain SQL: neither node's ORM saw it
    set_user(&h, bob, "tok_a = 'from-elsewhere'").await;
    gone(&b, &secret, "on the other node").await;
    gone(&a, &secret, "on the issuing node").await;
}

/// request)
#[tokio::test]
async fn the_plan_is_resolved_once_per_service() {
    let h = harness(true).await;
    prepare(&h).await;
    let _serial = SCENARIO.lock().await;
    test_token::set_scenario("ok");
    let before = test_token::calls();
    let auth = Auth::new(
        h.orm().clone(),
        AuthConfig::DEFAULT,
        own_pool(HashPoolConfig::DEFAULT),
    )
    .await
    .unwrap();
    assert_eq!(
        test_token::calls() - before,
        1,
        "one resolution per service"
    );
    let bob = user_with_password(&h, "bob", "pw").await;
    for _ in 0..3 {
        let secret = cookie(&login(&auth, "bob", "pw").await.unwrap());
        assert_eq!(check_user(&auth, &secret).await.unwrap(), bob);
    }
    assert_eq!(test_token::calls() - before, 1, "none per login or check");
}

/// seen by the restarted login; the session it writes covers the new value)
#[tokio::test]
async fn a_factor_changed_during_the_hash_is_not_kept_stale() {
    let w = token_world_with(
        "ok",
        false,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_user(&w.h, bob, "tok_a = 'early'").await;
    let release = hold_permits(w.pool, 1).await;
    let auth = Arc::new(w.auth);
    let task_auth = Arc::clone(&auth);
    let pending = tokio::spawn(async move { login(&task_auth, "bob", "pw").await });
    // phase A has read tok_a = 'early'; the hash waits for the permit
    until_waiting(w.pool, 1).await;
    set_user(&w.h, bob, "tok_a = 'late'").await;
    drop(release);
    let logged = pending.await.unwrap().unwrap();
    assert_eq!(auth.stats().login_restarts, 1, "the guard saw the change");
    let id = logged.session().id();
    assert_eq!(
        stored_token(&w.h, id).await,
        expected_token(&w.h, bob, id, &[text("tok_a", Some("late"))]).await
    );
    assert_eq!(check_user(&auth, &cookie(&logged)).await.unwrap(), bob);
}

/// user changes their own password; the other sessions end)
#[tokio::test]
async fn an_own_password_change_keeps_the_current_session_over_the_plan() {
    let w = token_world("ok", false).await;
    let bob = user_with_password(&w.h, "bob", "old").await;
    set_user(&w.h, bob, "tok_a = 'x'").await;
    let current = login(&w.auth, "bob", "old").await.unwrap();
    let other = login(&w.auth, "bob", "old").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "old", IN_PROCESS)
        .await
        .unwrap();
    assert_eq!(
        w.auth
            .change_password(Some(&secret), "new-pw")
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), bob);
    let id = current.session().id();
    assert_eq!(
        stored_token(&w.h, id).await,
        expected_token(&w.h, bob, id, &[text("tok_a", Some("x"))]).await
    );
    gone(&w.auth, &cookie(&other), "the other session").await;
}

/// rehashing login wrote, whose token covers the plan, and ends the older one)
#[tokio::test]
async fn rehash_cleanup_uses_the_plan() {
    let w = token_world("ok", false).await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_user(&w.h, bob, "tok_a = 'x'").await;
    // logs in while the stored hash is still the configured scheme: no rehash
    let older = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(!older.auth().rehash());
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    let newer = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(newer.auth().rehash());
    assert_eq!(check_user(&w.auth, &cookie(&newer)).await.unwrap(), bob);
    gone(&w.auth, &cookie(&older), "the session of the old hash").await;
    let id = newer.session().id();
    assert_eq!(
        stored_token(&w.h, id).await,
        expected_token(&w.h, bob, id, &[text("tok_a", Some("x"))]).await
    );
}

fn refused(result: Result<Auth, Error>, kind: ErrorKind, message: &str) {
    match result {
        Err(e) => assert_eq!((e.kind(), e.message()), (kind, message)),
        Ok(_) => panic!("the service was built for a refused plan"),
    }
}

/// failure shapes and the combination with custom login selection are separate tests)
#[tokio::test]
async fn an_invalid_field_in_the_plan_refuses_the_service() {
    let cases: [(&'static str, ErrorKind, &str); 5] = [
        (
            "unknown",
            ErrorKind::Programming,
            "_get_session_token_fields: res.users has no field 'no_such_field'",
        ),
        (
            "unstored",
            ErrorKind::Programming,
            "_get_session_token_fields: res.users.tok_calc is not stored, only stored columns can be part of the token",
        ),
        (
            "relation",
            ErrorKind::Programming,
            "_get_session_token_fields: res.users.tok_rel is a relation field, relation contributions are not supported until a later release",
        ),
        (
            "datetime",
            ErrorKind::Programming,
            "_get_session_token_fields: res.users.tok_dt has a type a token cannot cover (datetime)",
        ),
        (
            "base_dup",
            ErrorKind::Programming,
            "_get_session_token_fields failed: _get_session_token_fields: 'login' is already part of the base token",
        ),
    ];
    for (scenario, kind, message) in cases {
        let (_h, result) = try_world(scenario, false, HashPoolConfig::DEFAULT).await;
        refused(result, kind, message);
    }
}

#[tokio::test]
async fn a_malformed_plan_from_the_hook_refuses_the_service() {
    let cases: [(&'static str, &str); 3] = [
        (
            "badname",
            "_get_session_token_fields failed: _get_session_token_fields: 'tok_a; DROP TABLE res_users' is not a field name (lowercase letters, digits and underscores, at most 63 bytes, not starting with a digit)",
        ),
        (
            "dup",
            "_get_session_token_fields failed: _get_session_token_fields: 'tok_a' was added twice",
        ),
        (
            "toomany",
            "_get_session_token_fields failed: _get_session_token_fields: more than 16 added fields",
        ),
    ];
    for (scenario, message) in cases {
        let (_h, result) = try_world(scenario, false, HashPoolConfig::DEFAULT).await;
        refused(result, ErrorKind::Programming, message);
    }
}

/// base-only plan)
#[tokio::test]
async fn a_failing_panicking_or_querying_hook_refuses_the_service() {
    let (_h, result) = try_world("err", false, HashPoolConfig::DEFAULT).await;
    refused(
        result,
        ErrorKind::Programming,
        "_get_session_token_fields failed: token hook secret detail",
    );
    let (_h, result) = try_world("panic", false, HashPoolConfig::DEFAULT).await;
    refused(
        result,
        ErrorKind::Programming,
        "_get_session_token_fields panicked: token hook secret panic",
    );
    let (_h, result) = try_world("query", false, HashPoolConfig::DEFAULT).await;
    match result {
        Err(e) => {
            assert_eq!(e.kind(), ErrorKind::Programming);
            assert!(
                e.message()
                    .starts_with("_get_session_token_fields failed: "),
                "{}",
                e.message()
            );
        }
        Ok(_) => panic!("a querying hook built the service"),
    }
}

/// refused with its own named follow-up)
#[tokio::test]
async fn custom_selection_with_added_fields_is_refused() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &test_login_selection::ADDON,
        &test_token::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");
    prepare(&h).await;
    let _serial = SCENARIO.lock().await;
    test_token::set_scenario("ok");
    let result = test_support::login_selection_auth(
        h.orm().clone(),
        AuthConfig::DEFAULT,
        own_pool(HashPoolConfig::DEFAULT),
    )
    .await;
    refused(
        result,
        ErrorKind::Programming,
        "custom login selection combined with a session token plan that adds fields is unsupported until a later release",
    );
}

/// A native API key of `uid`.
async fn issued_key(w: &World, uid: i64) -> IssuedSecret {
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    // a counting harness leaves the audit store check to the first audited call
    w.h.orm().check_audit_store().await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    // the key's creator is the active superuser, a member of the system group
    tx.batch(
        "UPDATE res_users SET active=true WHERE id=1; \
         INSERT INTO res_groups_users_rel(uid,gid) SELECT 1,res_id FROM ir_model_data \
          WHERE module='base' AND name='group_system' AND model='res.groups' ON CONFLICT DO NOTHING",
    )
    .await
    .unwrap();
    let secret: String = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key='database.secret'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let hasher = ApiKeyHasher::derive(&DatabaseSecret::from_stored(&secret).unwrap()).unwrap();
    let expires: i64 = tx
        .query(
            "SELECT (extract(epoch FROM now()+interval '1 hour')*1000000)::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let request = IssueRequest::new(
        erp_orm::UserId::new(uid).unwrap(),
        "token plan",
        KeyKind::User,
        KeyScope::Rpc,
        Some(erp_fields::DateTime::from_micros(expires)),
    )
    .unwrap();
    let key = issue(&mut tx, &hasher, &request).await.unwrap();
    tx.commit().await.unwrap();
    key.into_secret()
}

/// statement carries the added fields before the key evidence)
#[tokio::test]
async fn a_cookie_beside_an_api_key_is_checked_over_the_plan() {
    let w = token_world("ok", true).await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_user(&w.h, bob, "tok_a = 'x', tok_n = 3").await;
    let key = issued_key(&w, bob).await;
    let secret = cookie(&login(&w.auth, "bob", "pw").await.unwrap());
    let request = || ApiKeyRequest::new(key.expose(), KeyScope::Rpc, "192.0.2.19".parse().unwrap());
    let mut begun = w
        .auth
        .begin_api_key(request().session(&secret), CredentialOptions::new())
        .await
        .unwrap();
    assert!(!begun.drop_cookie, "the cookie's token covers the plan");
    assert_eq!(begun.tx.env().uid(), bob);
    begun.tx.rollback().await.unwrap();
    // the key itself is unaffected by a token field; only the cookie is dropped
    set_user(&w.h, bob, "tok_n = 4").await;
    let mut begun = w
        .auth
        .begin_api_key(request().session(&secret), CredentialOptions::new())
        .await
        .unwrap();
    assert!(begun.drop_cookie, "the cookie no longer matches");
    assert_eq!(begun.tx.env().uid(), bob);
    begun.tx.rollback().await.unwrap();
}

/// whose token is recomputed over the plan)
#[tokio::test]
async fn an_administrator_reset_of_the_own_password_recomputes_over_the_plan() {
    let w = token_world("ok", false).await;
    // the reset's company scope (Amendment 1) needs the generated res.users row and a
    // company the actor belongs to
    company_scope(&w.h).await;
    let actor = user_with_password(&w.h, "reset-manager", "actor-old").await;
    let mut tx = w.h.begin().await.unwrap();
    let group = named_group(&mut tx, "base.group_erp_manager", "Access Administration").await;
    records(&tx, "res.users", &[actor])
        .write_values(
            &mut tx,
            &map(&[("group_ids", erp_sdk::Value::list([link_cmd(group)]))]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    set_user(&w.h, actor, "tok_a = 'x'").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let other = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let changes = [PasswordReset::new(erp_sdk::UserId::new(actor).unwrap(), "actor-new").unwrap()];
    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), actor);
    let id = current.session().id();
    assert_eq!(
        stored_token(&w.h, id).await,
        expected_token(&w.h, actor, id, &[text("tok_a", Some("x"))]).await
    );
    gone(&w.auth, &cookie(&other), "the other session").await;
}

/// are refused at construction, so an oversized value cannot occur; the runtime size check stays
/// as a backstop, unit-tested in `session.rs`)
#[tokio::test]
async fn unbounded_or_setting_dependent_fields_refuse_the_service() {
    let cases: [(&'static str, &str); 4] = [
        (
            "float",
            "_get_session_token_fields: res.users.tok_f has a type a token cannot cover (float)",
        ),
        (
            "text",
            "_get_session_token_fields: res.users.tok_txt is a text field, so its value is not bounded",
        ),
        (
            "unsized",
            "_get_session_token_fields: res.users.tok_unsized is a char with no size, so its value is not bounded (a token covers a char of size 1 to 16384)",
        ),
        (
            "bigsize",
            "_get_session_token_fields: res.users.tok_big is a char with size 20000, so its value is not bounded (a token covers a char of size 1 to 16384)",
        ),
    ];
    for (scenario, message) in cases {
        let (_h, result) = try_world(scenario, false, HashPoolConfig::DEFAULT).await;
        refused(result, ErrorKind::Programming, message);
    }
}

/// service is refused)
#[tokio::test]
async fn a_hanging_hook_is_cut_at_the_deadline() {
    let h = harness(false).await;
    prepare(&h).await;
    let config = AuthConfig::DEFAULT
        .with_token_plan_hook_deadline_ms(50)
        .unwrap();
    let _serial = SCENARIO.lock().await;
    test_token::set_scenario("hang");
    let started = std::time::Instant::now();
    let result = Auth::new(h.orm().clone(), config, own_pool(HashPoolConfig::DEFAULT)).await;
    refused(
        result,
        ErrorKind::Programming,
        "_get_session_token_fields exceeded its 50 ms deadline",
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// The wire round trips a login, a session check and a key-with-cookie check take in `w`
/// (the same counter the base-plan budget tests use).
async fn round_trips(w: &World, key: &IssuedSecret) -> [u64; 3] {
    let wire = w.h.wire().unwrap().clone();
    let request = || ApiKeyRequest::new(key.expose(), KeyScope::Rpc, "192.0.2.19".parse().unwrap());
    let measure = |used: u64| used;
    // warm: settings cache, prepared statements, the dummy hash
    let warm = login(&w.auth, "bob", "pw").await.unwrap();
    let secret = cookie(&warm);
    check_user(&w.auth, &secret).await.unwrap();
    let mut begun = w
        .auth
        .begin_api_key(request().session(&secret), CredentialOptions::new())
        .await
        .unwrap();
    begun.tx.rollback().await.unwrap();
    // A never-used key is due: dropping the ticket launches the hourly last-use write, which runs
    // outside the request and on its own connection. Let it end, or its statements are counted in
    // the first window below.
    drop(begun);
    let started = std::time::Instant::now();
    while w.auth.stats().last_use_writes < 1 {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the last-use write never ended"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    wire.settle_before_block().await;
    let generation = wire.reset_generation();
    let before = wire.count();
    login(&w.auth, "bob", "pw").await.unwrap();
    let logged_in = measure(wire.used_since(before, generation));
    wire.settle_before_block().await;
    let generation = wire.reset_generation();
    let before = wire.count();
    let begun = w
        .auth
        .begin_session(&secret, CredentialOptions::new())
        .await
        .unwrap();
    let checked = measure(wire.used_since(before, generation));
    let mut tx = begun.tx;
    tx.rollback().await.unwrap();
    drop(tx);
    wire.settle_before_block().await;
    let generation = wire.reset_generation();
    let before = wire.count();
    let mut begun = w
        .auth
        .begin_api_key(request().session(&secret), CredentialOptions::new())
        .await
        .unwrap();
    let keyed = measure(wire.used_since(before, generation));
    begun.tx.rollback().await.unwrap();
    drop(begun);
    [logged_in, checked, keyed]
}

async fn counted_world(plan: bool) -> World {
    let h = Harness::builder(if plan {
        vec![
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &test_token::ADDON,
            &test_token_more::ADDON,
        ]
    } else {
        vec![&erp_addon_base::ADDON, &erp_data::model_data::ADDON]
    })
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .identify_transactions()
    .count_round_trips()
    .build()
    .await
    .unwrap();
    let admin = prepare(&h).await;
    let pool = own_pool(HashPoolConfig::DEFAULT);
    let _serial = SCENARIO.lock().await;
    test_token::set_scenario("ok");
    let auth = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, pool)
        .await
        .unwrap();
    World {
        h,
        auth,
        pool,
        admin,
    }
}

/// base plan, for a login, a session check and a key-with-cookie check; the added fields travel
/// in the statements that already read the user, and nothing else is measured)
#[tokio::test]
async fn an_extended_plan_adds_no_round_trip() {
    let mut counts = Vec::new();
    for plan in [false, true] {
        let w = counted_world(plan).await;
        let bob = user_with_password(&w.h, "bob", "pw").await;
        if plan {
            set_user(&w.h, bob, "tok_a = 'x', tok_n = 3").await;
        }
        let key = issued_key(&w, bob).await;
        counts.push(round_trips(&w, &key).await);
    }
    assert_eq!(counts[0], [2, 1, 1], "the base plan keeps its budgets");
    assert_eq!(counts[1], counts[0], "the extended plan costs the same");
    // the macro of the budget tests agrees on one of them
    let w = counted_world(true).await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_user(&w.h, bob, "tok_a = 'x', tok_n = 3").await;
    login(&w.auth, "bob", "pw").await.unwrap();
    let wire = w.h.wire().unwrap().clone();
    assert_round_trips!(wire, == 2, {
        login(&w.auth, "bob", "pw").await.unwrap();
    });
}

/// it restart; the new hash and the new value are both in the session it writes)
#[tokio::test]
async fn a_factor_changed_during_a_rehash_is_not_kept_stale() {
    let w = token_world_with(
        "ok",
        false,
        HashPoolConfig::new(1, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
    )
    .await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    set_user(&w.h, bob, "tok_a = 'early'").await;
    let release = hold_permits(w.pool, 1).await;
    let auth = Arc::new(w.auth);
    let task_auth = Arc::clone(&auth);
    let pending = tokio::spawn(async move { login(&task_auth, "bob", "pw").await });
    until_waiting(w.pool, 1).await;
    set_user(&w.h, bob, "tok_a = 'late'").await;
    drop(release);
    let logged = pending.await.unwrap().unwrap();
    assert!(logged.auth().rehash());
    assert_eq!(auth.stats().login_restarts, 1);
    let id = logged.session().id();
    assert_eq!(
        stored_token(&w.h, id).await,
        expected_token(&w.h, bob, id, &[text("tok_a", Some("late"))]).await
    );
    assert_eq!(check_user(&auth, &cookie(&logged)).await.unwrap(), bob);
}
