//! sessions (§14.8): the check in the opening
//! flight, rotation, touch, logout, the idle limit and the lifetime, the token over the user's
//! fields, keys the server does not hold, a `database.secret` change, collection, and the user
//! context.
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
use erp_sdk::{ErrorKind, Value, ValueMap};
use erp_security::auth::{
    AuthConfig, CheckedSession, CredentialOptions, HashPoolConfig, SessionSecret, Touch,
};
use erp_testkit::assert_round_trips;

fn opts() -> CredentialOptions {
    CredentialOptions::new()
}

async fn expired(w: &World, secret: &SessionSecret, message: &str) {
    match w.auth.begin_session(secret, opts()).await {
        Err(e) => assert_eq!(
            (e.kind(), e.message()),
            (ErrorKind::SessionExpired, message)
        ),
        Ok(begun) => panic!("session {:?} accepted", begun.session),
    }
}

/// Accept `secret` and end the request's transaction; the accepted session.
async fn check(w: &World, secret: &SessionSecret) -> CheckedSession {
    let begun = w.auth.begin_session(secret, opts()).await.unwrap();
    let mut tx = begun.tx;
    tx.rollback().await.unwrap();
    begun.session
}

async fn sql(w: &World, statement: &str) {
    let mut tx = w.h.begin().await.unwrap();
    tx.batch(statement).await.unwrap();
    tx.commit().await.unwrap();
}

/// body, the `tz` cookie and `_after_session_login` are's and's)
#[tokio::test]
async fn login_makes_a_session_the_check_accepts() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let mut tx = w.h.begin().await.unwrap();
    // A language that is no longer installed, as after a deactivation: the ORM refuses to assign
    // one that is not installed, so the stale value is set by SQL.
    tx.execute(
        "UPDATE res_users SET tz = $1, lang = $2 WHERE id = $3",
        &[&"Asia/Jakarta", &"fr_FR", &bob],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    // `fr_FR` is not installed (no `res.lang` yet): `en_US`
    assert_eq!(ok.context().lang(), "en_US");
    assert_eq!(ok.context().tz(), Some("Asia/Jakarta"));
    assert_eq!(ok.context().uid().get(), bob);
    assert_eq!(
        count(
            &w.h,
            &format!(
                "erp_session WHERE user_id = {bob} AND state = 'active' \
                 AND context = '{{\"lang\": \"en_US\", \"tz\": \"Asia/Jakarta\", \"uid\": {bob}}}'::jsonb"
            )
        )
        .await,
        1
    );
    assert_eq!(
        count(&w.h, &format!("res_users_log WHERE create_uid = {bob}")).await,
        1
    );
    let mut begun = w.auth.begin_session(&cookie(&ok), opts()).await.unwrap();
    assert_eq!(begun.session.user().get(), bob);
    assert_eq!(begun.session.id(), ok.session().id());
    assert!(!begun.session.via_previous());
    assert_eq!(begun.tx.env().uid(), bob);
    assert!(!begun.tx.env().su());
    begun.tx.query("SELECT 1", &[]).await.unwrap();
    begun.tx.commit().await.unwrap();
}

/// `res.users.context_get` (design note §14.9; `crates/erp-rpc/src/pending.rs` entry removed):
/// the calling user's `lang`, `tz` and `uid`.
#[tokio::test]
async fn context_get() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let mut tx = w.h.begin_as(bob).await.unwrap();
    let users = tx.env().records("res.users").unwrap();
    let context: ValueMap = users.call(&mut tx, "context_get", ()).await.unwrap();
    assert_eq!(context.get("lang"), Some(&Value::str("en_US")));
    assert_eq!(context.get("tz"), Some(&Value::False));
    assert_eq!(context.get("uid"), Some(&Value::Int(bob)));
    tx.rollback().await.unwrap();
}

/// session, and a login always creates a new row)
#[tokio::test]
async fn unknown_secrets_are_no_session() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    expired(
        &w,
        &SessionSecret::generate().unwrap(),
        "user is not connected",
    )
    .await;
    let a = login(&w.auth, "bob", "pw").await.unwrap();
    let b = login(&w.auth, "bob", "pw").await.unwrap();
    assert_ne!(a.session().id(), b.session().id());
    assert_eq!(count(&w.h, "erp_session").await, 2);
}

/// active flag; a change of the password, the login or the flag fails the next check, which
/// deletes the row; recomputing the current session's token on one's own password change is
/// 's `change_password`)
#[tokio::test]
async fn user_changes_fail_the_next_check() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let by_password = login(&w.auth, "bob", "pw").await.unwrap();
    let by_login = login(&w.auth, "bob", "pw").await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    write(&mut tx, "res.users", &[bob], &[("password", s("pw2"))])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    expired(&w, &cookie(&by_password), "user is not connected").await;
    assert_eq!(
        count(
            &w.h,
            &format!(
                "erp_session WHERE id = '{}'::uuid",
                by_password.session().id()
            )
        )
        .await,
        0,
        "the refused row is deleted"
    );
    let fresh = login(&w.auth, "bob", "pw2").await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    write(&mut tx, "res.users", &[bob], &[("login", s("robert"))])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    expired(&w, &cookie(&fresh), "user is not connected").await;
    expired(&w, &cookie(&by_login), "user is not connected").await;
    // archiving ends the sessions at once (design note §14.7)
    let current = login(&w.auth, "robert", "pw2").await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    write(
        &mut tx,
        "res.users",
        &[bob],
        &[("active", Value::Bool(false))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        count(&w.h, &format!("erp_session WHERE user_id = {bob}")).await,
        0
    );
    expired(&w, &cookie(&current), "user is not connected").await;
}

/// password change; the HTTP status is's)
#[tokio::test]
async fn session_expired_after_a_password_change() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    let mut tx = w.h.begin().await.unwrap();
    write(&mut tx, "res.users", &[bob], &[("password", s("other"))])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = w
        .auth
        .begin_session(&cookie(&ok), opts())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::SessionExpired);
    assert_eq!(e.message(), "user is not connected");
}

/// hooks; CSRF, redirect and cookie are's)
#[tokio::test]
async fn logout_ends_the_session() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(w.auth.logout(ok.session().id()).await.unwrap());
    assert!(!w.auth.logout(ok.session().id()).await.unwrap());
    expired(&w, &cookie(&ok), "user is not connected").await;
}

/// one successor, the previous secret is accepted within the grace; exempt paths and the header
/// are's)
#[tokio::test]
async fn rotation() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    let old = cookie(&ok);
    let checked = check(&w, &old).await;
    let next = w.auth.rotate(&checked, &old).await.unwrap().unwrap();
    assert!(
        w.auth.rotate(&checked, &old).await.unwrap().is_none(),
        "a second rotation of the same secret sets nothing"
    );
    let via_new = check(&w, &next).await;
    assert_eq!(via_new.id(), ok.session().id());
    assert!(!via_new.via_previous());
    let via_old = check(&w, &old).await;
    assert_eq!(via_old.id(), ok.session().id());
    assert!(via_old.via_previous());
    assert!(!via_old.rotation_due());
}

/// and the row stays for the holder of the new secret)
#[tokio::test]
async fn predecessor_after_grace_is_too_old() {
    let config = AuthConfig::DEFAULT.with_rotation_grace_secs(0).unwrap();
    let w = world_with(
        builder().build().await.unwrap(),
        config,
        HashPoolConfig::DEFAULT,
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    let old = cookie(&ok);
    let checked = check(&w, &old).await;
    let next = w.auth.rotate(&checked, &old).await.unwrap().unwrap();
    expired(&w, &old, "session is too old").await;
    assert_eq!(count(&w.h, "erp_session").await, 1);
    check(&w, &next).await;
}

#[tokio::test]
async fn logout_during_grace_ends_both_secrets() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    let old = cookie(&ok);
    let checked = check(&w, &old).await;
    let next = w.auth.rotate(&checked, &old).await.unwrap().unwrap();
    assert!(w.auth.logout(checked.id()).await.unwrap());
    expired(&w, &old, "user is not connected").await;
    expired(&w, &next, "user is not connected").await;
}

/// deletes the row at the check, and collection deletes idle rows)
#[tokio::test]
async fn idle_sessions_are_refused_and_collected() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "sessions.max_inactivity_seconds", "60").await;
    let idle = login(&w.auth, "bob", "pw").await.unwrap();
    let collected = login(&w.auth, "bob", "pw").await.unwrap();
    let kept = login(&w.auth, "bob", "pw").await.unwrap();
    sql(
        &w,
        &format!(
            "UPDATE erp_session SET last_seen_at = now() - interval '61 seconds' \
              WHERE id IN ('{}'::uuid, '{}'::uuid)",
            idle.session().id(),
            collected.session().id()
        ),
    )
    .await;
    expired(&w, &cookie(&idle), "user is not connected").await;
    let report = w.auth.gc().await.unwrap();
    assert_eq!(report.sessions(), 1);
    assert_eq!(count(&w.h, "erp_session").await, 1);
    check(&w, &cookie(&kept)).await;
}

/// Absolute lifetime (design note §9 new case, D-AUTH-5): a session past `expires_at` is refused
/// and deleted whatever its activity.
#[tokio::test]
async fn sessions_past_their_lifetime_are_refused() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    assert_eq!(ok.session().lifetime().as_secs(), 7_776_000);
    sql(
        &w,
        "UPDATE erp_session SET expires_at = now() - interval '1 second'",
    )
    .await;
    expired(&w, &cookie(&ok), "user is not connected").await;
    assert_eq!(count(&w.h, "erp_session").await, 0);
}

/// A `token_kid` the server does not hold is refused (design note §9 new case); the row stays,
/// since a node that holds the key may accept it during a key rollout (security review Low 7).
#[tokio::test]
async fn unknown_kid_is_refused_and_kept() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    sql(&w, "UPDATE erp_session SET token_kid = 7").await;
    expired(&w, &cookie(&ok), "user is not connected").await;
    assert_eq!(count(&w.h, "erp_session").await, 1);
    assert_eq!(w.auth.stats().refusal_delete_failures, 0);
}

/// A new `database.secret` (what `erp db rotate-secret` does) invalidates the cached keys at the
/// next check and every existing session with them.
#[tokio::test]
async fn a_new_database_secret_ends_every_session() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    check(&w, &cookie(&ok)).await;
    let loads = w.auth.stats().settings_loads;
    set_param(
        &w.h,
        "database.secret",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .await;
    expired(&w, &cookie(&ok), "user is not connected").await;
    assert!(
        w.auth.stats().settings_loads > loads,
        "the keys were reloaded"
    );
    let fresh = login(&w.auth, "bob", "pw").await.unwrap();
    check(&w, &cookie(&fresh)).await;
}

/// Touch writes `last_seen_at` at most once per interval, with the request's address (as is,
/// not its throttle key) and user agent (database review Low); the login stored the raw address
/// too.
#[tokio::test]
async fn touch_writes_at_most_once_per_interval() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login_from(&w.auth, "bob", "pw", "2001:db8:1:2:aa:bb:cc:dd")
        .await
        .unwrap();
    let id = ok.session().id();
    assert_eq!(
        count(
            &w.h,
            "erp_session WHERE first_addr = '2001:db8:1:2:aa:bb:cc:dd'::inet \
              AND last_addr = first_addr"
        )
        .await,
        1
    );
    let touch = Touch::new(id)
        .addr("192.0.2.44".parse().unwrap())
        .agent("probe/1.0");
    assert_eq!(
        w.auth.touch(std::slice::from_ref(&touch)).await.unwrap(),
        0,
        "just seen"
    );
    sql(
        &w,
        "UPDATE erp_session SET last_seen_at = now() - interval '120 seconds'",
    )
    .await;
    assert!(check(&w, &cookie(&ok)).await.touch_due());
    assert_eq!(
        w.auth.touch(&[touch.clone(), touch.clone()]).await.unwrap(),
        1,
        "one row, however often it is named"
    );
    assert_eq!(w.auth.touch(&[touch]).await.unwrap(), 0);
    assert_eq!(
        count(
            &w.h,
            "erp_session WHERE last_addr = '192.0.2.44'::inet AND last_agent = 'probe/1.0' \
              AND first_addr = '2001:db8:1:2:aa:bb:cc:dd'::inet"
        )
        .await,
        1
    );
}

/// Database review M1: touch skips a row another transaction holds instead of waiting for it (it
/// is being rotated, ended or touched; the next request touches it again).
#[tokio::test]
async fn touch_skips_rows_another_transaction_holds() {
    let w = world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let held = login(&w.auth, "bob", "pw").await.unwrap();
    let free = login(&w.auth, "bob", "pw").await.unwrap();
    sql(
        &w,
        "UPDATE erp_session SET last_seen_at = now() - interval '120 seconds'",
    )
    .await;
    let mut holder = w.h.begin().await.unwrap();
    holder
        .execute(
            "SELECT 1 FROM erp_session WHERE id = $1::text::uuid FOR UPDATE",
            &[&held.session().id().to_text()],
        )
        .await
        .unwrap();
    let touches = [
        Touch::new(held.session().id()),
        Touch::new(free.session().id()),
    ];
    let written = tokio::time::timeout(Duration::from_secs(5), w.auth.touch(&touches))
        .await
        .expect("touch does not wait for the held row")
        .unwrap();
    assert_eq!(written, 1);
    holder.rollback().await.unwrap();
    assert_eq!(w.auth.touch(&touches).await.unwrap(), 1, "the held row now");
}

/// Rust review M1, database review H1 (convergence): a rotation that waits for a
/// concurrent rotation of the same session (two transactions interleaved: the other one rotated
/// and holds the row) re-reads the row after the other commits, finds the digest changed and
/// returns `None`, not an error; the other's successor and the presented predecessor both hold.
#[tokio::test]
async fn a_rotation_behind_a_concurrent_one_converges() {
    let w = world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    let old = cookie(&ok);
    let checked = check(&w, &old).await;
    let World { h, auth, .. } = w;
    let auth = Arc::new(auth);
    let winner = SessionSecret::generate().unwrap();
    let hex = |d: [u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut other = h.begin().await.unwrap();
    other
        .execute(
            "UPDATE erp_session SET prev_digest = sid_digest, \
                    sid_digest = decode($2, 'hex'), rotated_at = now() \
              WHERE id = $1::text::uuid AND sid_digest = decode($3, 'hex')",
            &[
                &ok.session().id().to_text(),
                &hex(winner.digest()),
                &hex(old.digest()),
            ],
        )
        .await
        .unwrap();
    let node = Arc::clone(&auth);
    let presented = cookie(&ok);
    let rotation = tokio::spawn(async move { node.rotate(&checked, &presented).await });
    until_lock_wait(&h).await;
    other.commit().await.unwrap();
    assert!(rotation.await.unwrap().unwrap().is_none());
    assert_eq!(check_user(&auth, &winner).await.unwrap(), bob);
    assert_eq!(check_user(&auth, &old).await.unwrap(), bob);
}

/// Budget (design note §14.3, §14.11): the session check adds no round trip to the opening
/// flight once the settings are cached.
#[tokio::test]
async fn session_check_is_one_round_trip() {
    let w = world_with(
        builder().count_round_trips().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::DEFAULT,
    )
    .await;
    user_with_password(&w.h, "bob", "pw").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    let secret = cookie(&ok);
    check(&w, &secret).await;
    let wire = w.h.wire().unwrap().clone();
    let begun = assert_round_trips!(wire, == 1, {
        w.auth.begin_session(&secret, opts()).await.unwrap()
    });
    let mut tx = begun.tx;
    tx.rollback().await.unwrap();
}

/// The authentication tables are core tables: `Orm::sync_schema` installs them (idempotently,
/// on a database whose models are in place).
#[tokio::test]
async fn sync_schema_installs_the_authentication_tables() {
    let h = erp_testkit::Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .unwrap();
    let tables = "pg_class WHERE relname IN ('erp_session', 'erp_login_throttle') \
                  AND relkind = 'r'";
    // the harness created them as `sync_schema` does: remove them first (a throwaway database)
    let client = h.db().client().await.unwrap();
    client
        .batch_execute("DROP TABLE erp_session; DROP TABLE erp_login_throttle")
        .await
        .unwrap();
    assert_eq!(count(&h, tables).await, 0);
    let report = h.orm().sync_schema().await.unwrap();
    assert!(report.is_empty(), "{report:?}");
    assert_eq!(count(&h, tables).await, 2);
    let report = h.orm().sync_schema().await.unwrap();
    assert!(report.is_empty(), "{report:?}");
    assert_eq!(count(&h, tables).await, 2);
}

/// Boundary encodings are evaluated by PostgreSQL using the production SQL expression and
/// decoded by the production checked stamp decoder, not by a second algorithm in the test.
#[tokio::test]
async fn observation_stamp_sql_roundtrips_xid_boundaries_and_both_digests() {
    use erp_security::auth::test_support::{decode_observation_stamp, observation_stamp_sql};
    use tokio_postgres::types::Type;

    let w = world().await;
    let mut tx = w.h.begin().await.unwrap();
    let sql = format!(
        "SELECT {} FROM (SELECT $1::int8::text::xid AS xmin, decode('aa', 'hex') AS sid_digest) s \
         CROSS JOIN (SELECT CASE WHEN $2::bool THEN decode('aa', 'hex') \
         ELSE decode('bb', 'hex') END AS digest) d",
        observation_stamp_sql()
    );
    for version in [0_i64, 1, i64::from(u32::MAX)] {
        for current in [false, true] {
            let rows = tx
                .query_typed(&sql, &[(&version, Type::INT8), (&current, Type::BOOL)])
                .await
                .unwrap();
            let stamp: i64 = rows[0].get(0);
            assert_eq!(decode_observation_stamp(stamp).unwrap(), (version, current));
        }
    }
    tx.rollback().await.unwrap();
}

/// Values outside the wire contract are errors, never wrapped row versions or digest flags.
#[tokio::test]
async fn observation_stamp_rejects_negative_and_above_xid_range() {
    use erp_security::auth::test_support::decode_observation_stamp;

    let w = world().await;
    let mut tx = w.h.begin().await.unwrap();
    let rows = tx
        .query("SELECT -1::int8 UNION ALL SELECT 8589934592::int8", &[])
        .await
        .unwrap();
    for row in rows {
        let error = decode_observation_stamp(row.get(0)).unwrap_err();
        assert_eq!(
            (error.kind(), error.message()),
            (
                ErrorKind::Database,
                "session row stamp is outside the PostgreSQL xid/flag range"
            )
        );
    }
    tx.rollback().await.unwrap();
}
