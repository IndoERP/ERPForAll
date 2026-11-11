//! the session-end hook (lead note T-PUSH.8):
//! subscribers delete their rows in the transaction that deletes the sessions (logout, archive,
//! a failed check), and a failing hook fails the deletion. The hook of an addon that is not
//! installed never runs (every other auth test logs out without `test_auth`'s table).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

use auth_support::*;
use erp_sdk::{ErrorKind, Value};
use erp_security::auth::{
    AuthConfig, CredentialOptions, HashPoolConfig, SESSIONS_PER_USER, SessionId,
};
use erp_testkit::Harness;

async fn hooked_world() -> World {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_auth::ADDON,
        &erp_test_models::test_auth::scope_override::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");
    world_with(h, AuthConfig::DEFAULT, HashPoolConfig::DEFAULT).await
}

async fn device(w: &World, session: SessionId, name: &str) {
    let mut tx = w.h.begin().await.unwrap();
    create(
        &mut tx,
        "test_auth.device",
        &[("session_id", s(&session.to_text())), ("name", s(name))],
    )
    .await;
    tx.commit().await.unwrap();
}

async fn devices_of(w: &World, session: SessionId) -> i64 {
    count(
        &w.h,
        &format!("test_auth_device WHERE session_id = '{session}'"),
    )
    .await
}

/// Every caller must enforce the same raw-only subscriber contract.
#[tokio::test]
async fn repeatable_read_session_end_rejects_record_api_hooks() {
    let w = hooked_world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let logged = login(&w.auth, "bob", "pw").await.unwrap();
    let id = logged.session().id();
    device(&w, id, "records").await;
    let mut tx = w.h.begin().await.unwrap();
    let error =
        erp_security::auth::end_sessions(&mut tx, &[id], erp_security::auth::EndReason::Revoked)
            .await
            .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the record API is unavailable in a raw-statements-only scope (query, execute, batch)"
    );
    tx.rollback().await.unwrap();
    assert_eq!(devices_of(&w, id).await, 1);
    assert_eq!(count(&w.h, "erp_session").await, 1);
}

/// User mutations cannot make a record-API subscriber succeed by accident.
#[tokio::test]
async fn user_mutations_reject_record_api_hooks() {
    let w = hooked_world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let logged = login(&w.auth, "bob", "pw").await.unwrap();
    let id = logged.session().id();
    device(&w, id, "records").await;
    for (field, value) in [
        ("active", Value::Bool(false)),
        ("password", s("new")),
        ("login", s("renamed")),
    ] {
        let mut tx = w.h.begin().await.unwrap();
        let error = write(&mut tx, "res.users", &[bob], &[(field, value)])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming, "{field}: {error}");
        assert_eq!(
            error.message(),
            "the record API is unavailable in a raw-statements-only scope (query, execute, batch)"
        );
        tx.rollback().await.unwrap();
        assert_eq!(devices_of(&w, id).await, 1);
        assert_eq!(count(&w.h, "erp_session").await, 1);
    }
    assert_eq!(check_user(&w.auth, &cookie(&logged)).await.unwrap(), bob);
}

/// The restriction is scoped and nested guards never relax it.
#[tokio::test]
async fn raw_statement_scope_restores_record_access() {
    let w = hooked_world().await;
    let mut tx = w.h.begin().await.unwrap();
    let records = tx.env().records("test_auth.device").unwrap();
    records
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    {
        let mut raw = tx.raw_statements_only();
        {
            let mut nested = raw.raw_statements_only();
            let error = records
                .search(&mut nested, &erp_sdk::Domain::TRUE)
                .await
                .unwrap_err();
            assert_eq!(
                (error.kind(), error.message()),
                (
                    ErrorKind::Programming,
                    "the record API is unavailable in a raw-statements-only scope (query, execute, batch)"
                )
            );
        }
        let error = records
            .search(&mut raw, &erp_sdk::Domain::TRUE)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "the record API is unavailable in a raw-statements-only scope (query, execute, batch)"
        );
        let rows = raw
            .query("SELECT current_setting('transaction_isolation')", &[])
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, String>(0), "repeatable read");
    }
    records
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

/// Early error, cancellation and unwind restore the prior restriction.
#[tokio::test]
async fn raw_statement_scope_restores_after_error_and_cancellation() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let w = hooked_world().await;
    let mut tx = w.h.begin().await.unwrap();
    let records = tx.env().records("test_auth.device").unwrap();
    let error = async {
        let mut raw = tx.raw_statements_only();
        records.search(&mut raw, &erp_sdk::Domain::TRUE).await?;
        Ok::<(), erp_sdk::Error>(())
    }
    .await
    .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "the record API is unavailable in a raw-statements-only scope (query, execute, batch)"
        )
    );
    records
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    {
        let mut future = Box::pin(async {
            let mut raw = tx.raw_statements_only();
            let error = records
                .search(&mut raw, &erp_sdk::Domain::TRUE)
                .await
                .unwrap_err();
            assert_eq!(
                (error.kind(), error.message()),
                (
                    ErrorKind::Programming,
                    "the record API is unavailable in a raw-statements-only scope (query, execute, batch)"
                )
            );
            std::future::pending::<()>().await;
        });
        poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // Drop the pending future while it owns the live guard, without a wall-clock timeout.
    }
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _raw = tx.raw_statements_only();
        panic!("test unwind while raw-only scope is active");
    }));
    assert!(panicked.is_err());
    records
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

fn assert_scope_error(error: erp_sdk::Error) {
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the record API is unavailable in a raw-statements-only scope (query, execute, batch)"
    );
}

#[tokio::test]
async fn prebuilt_queries_cannot_bypass_raw_statement_scope() {
    let w = hooked_world().await;
    let mut tx = w.h.begin().await.unwrap();
    let records = tx.env().records("res.users").unwrap();
    for domain in [erp_sdk::Domain::TRUE, erp_sdk::Domain::FALSE] {
        for cached in [false, true] {
            let mut query = records
                .search_query(&mut tx, &domain, &erp_orm::SearchOptions::new())
                .await
                .unwrap();
            if cached {
                query.ids(&mut tx).await.unwrap();
            }
            let mut raw = tx.raw_statements_only();
            let before = raw.query_count();
            assert_scope_error(query.count(&mut raw).await.unwrap_err());
            assert_scope_error(query.ids(&mut raw).await.unwrap_err());
            assert_scope_error(query.records(&mut raw).await.unwrap_err());
            assert_eq!(raw.query_count(), before, "refusal happens before SQL");
        }
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn read_committed_scope_retains_its_record_diagnostic() {
    let w = hooked_world().await;
    let mut tx =
        w.h.orm()
            .begin_opts(
                erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID)
                    .isolation(erp_orm::Isolation::ReadCommitted),
            )
            .await
            .unwrap();
    let records = tx.env().records("test_auth.device").unwrap();
    let expected = format!(
        "{records}: the record API needs a REPEATABLE READ transaction; this one was opened with BeginOptions::isolation(ReadCommitted), which allows raw statements only (query, execute, batch)"
    );
    {
        let mut raw = tx.raw_statements_only();
        let error = records
            .search(&mut raw, &erp_sdk::Domain::TRUE)
            .await
            .unwrap_err();
        assert_eq!(
            (error.kind(), error.message()),
            (ErrorKind::Programming, expected.as_str())
        );
    }
    let error = records
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::Programming, expected.as_str())
    );
    tx.rollback().await.unwrap();
}

/// field shortcuts and streams reject before returning or fetching data.
#[tokio::test]
async fn field_and_stream_fast_paths_obey_raw_statement_scope() {
    let w = hooked_world().await;
    let mut tx = w.h.begin().await.unwrap();
    let records = tx.env().records("res.users").unwrap().sudo(true);
    let singleton = records.browse([w.admin]).unwrap();
    let mut stream = records
        .stream(
            &mut tx,
            &erp_sdk::Domain::TRUE,
            &["id"],
            &erp_orm::StreamOptions::new(),
        )
        .await
        .unwrap();
    {
        let mut raw = tx.raw_statements_only();
        let before = raw.query_count();
        assert_scope_error(records.cell(&mut raw, "id").await.unwrap_err());
        assert_scope_error(singleton.value(&mut raw, "id").await.unwrap_err());
        assert_scope_error(singleton.cells(&mut raw, "id").await.unwrap_err());
        assert_scope_error(stream.next(&mut raw).await.unwrap_err());
        assert_scope_error(stream.close(&mut raw).await.unwrap_err());
        assert_scope_error(raw.flush().await.unwrap_err());
        assert_scope_error(raw.flush_model(singleton.model_id()).await.unwrap_err());
        assert_scope_error(
            raw.flush_fields(singleton.model_id(), &[])
                .await
                .unwrap_err(),
        );
        assert_eq!(raw.query_count(), before, "refusal before FETCH or flush");
    }
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    stream.close(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
}

/// a prebuilt method continuation is still record API access.
#[tokio::test]
async fn method_continuation_obeys_raw_statement_scope() {
    let w = hooked_world().await;
    let mut tx = w.h.begin().await.unwrap();
    let records = tx.env().records("test_auth.device").unwrap();
    let result: erp_sdk::Result<bool> =
        erp_orm::call_method(&mut tx, &records, "_raw_scope_probe", ()).await;
    assert_scope_error(result.unwrap_err());
    records
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

/// DB7-3: refusal cleanup must recheck a row refreshed while it waited for the row lock.
#[tokio::test]
async fn idle_refusal_cleanup_preserves_a_concurrently_touched_session() {
    let w = hooked_world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    set_param(&w.h, "sessions.max_inactivity_seconds", "3600").await;
    let ok = login(&w.auth, "bob", "pw").await.unwrap();
    device(&w, ok.session().id(), "phone").await;
    let id = ok.session().id().to_text();
    let mut tx = w.h.begin().await.unwrap();
    tx.execute("UPDATE erp_session SET last_seen_at = now() - interval '2 hours' WHERE id = $1::text::uuid", &[&id]).await.unwrap();
    tx.commit().await.unwrap();
    let mut touch = w.h.begin().await.unwrap();
    touch
        .execute(
            "UPDATE erp_session SET last_seen_at = now() WHERE id = $1::text::uuid",
            &[&id],
        )
        .await
        .unwrap();
    let World { h, auth, .. } = w;
    let auth = std::sync::Arc::new(auth);
    let node = std::sync::Arc::clone(&auth);
    let secret = cookie(&ok);
    let checking = tokio::spawn(async move { check_user(&node, &secret).await });
    // The opening read sees the committed idle version. Its later DELETE waits for touch.
    until_lock_wait(&h).await;
    touch.commit().await.unwrap();
    let error = checking.await.unwrap().unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::SessionExpired, "user is not connected")
    );
    assert_eq!(check_user(&auth, &cookie(&ok)).await.unwrap(), bob);
    assert_eq!(
        count(&h, &format!("test_auth_device WHERE session_id = '{id}'")).await,
        1
    );
}

/// R7-RUST-2: supported duplicate services share the admission of their primary pool.
#[tokio::test]
async fn duplicate_auth_services_share_connection_admission() {
    let w = hooked_world().await;
    let mut tx = w.h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let pool = waiting_pool(1);
    let first = std::sync::Arc::new(node_with(&w.h, 2, pool).await);
    let second = erp_security::auth::Auth::new(first.orm().clone(), AuthConfig::DEFAULT, pool)
        .await
        .unwrap();
    let logged = login(&first, "bob", "pw").await.unwrap();
    let release = hold_permits(pool, 1).await;
    let node = std::sync::Arc::clone(&first);
    let attempt = tokio::spawn(async move { login(&node, "bob", "pw").await });
    until_waiting(pool, 1).await;
    // The real override is blocked in its hash with one primary connection held. Debug
    // exposes available admission permits already; checking it avoids sleeping to guess
    // whether a second request has reached its database wait. Both handles must see zero.
    let availability = format!("{second:?}");
    let checked = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        check_user(&second, &cookie(&logged)),
    )
    .await;
    drop(release);
    attempt.await.unwrap().unwrap();
    assert!(
        availability.contains("admission: 0"),
        "duplicate service can over-admit: {availability}"
    );
    assert_eq!(
        checked.expect("session check keeps a connection").unwrap(),
        bob
    );
}

/// transaction; a failing subscriber keeps both)
#[tokio::test]
async fn logout_runs_the_session_end_hooks() {
    let w = hooked_world().await;
    user_with_password(&w.h, "bob", "pw").await;
    let first = login(&w.auth, "bob", "pw").await.unwrap();
    let second = login(&w.auth, "bob", "pw").await.unwrap();
    device(&w, first.session().id(), "phone").await;
    device(&w, second.session().id(), "refuse").await;
    assert!(w.auth.logout(first.session().id()).await.unwrap());
    assert_eq!(devices_of(&w, first.session().id()).await, 0);
    assert_eq!(devices_of(&w, second.session().id()).await, 1);
    let e = w.auth.logout(second.session().id()).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        count(
            &w.h,
            &format!("erp_session WHERE id = '{}'::uuid", second.session().id())
        )
        .await,
        1,
        "the failing hook kept the session"
    );
    assert_eq!(devices_of(&w, second.session().id()).await, 1);
}

/// Archiving a user and a refused check end the sessions with their subscribers' rows.
#[tokio::test]
async fn archive_and_refused_checks_run_the_hooks() {
    let w = hooked_world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let carol = user_with_password(&w.h, "carol", "pw").await;
    let of_bob = login(&w.auth, "bob", "pw").await.unwrap();
    let of_carol = login(&w.auth, "carol", "pw").await.unwrap();
    device(&w, of_bob.session().id(), "laptop").await;
    device(&w, of_carol.session().id(), "tablet").await;
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
    assert_eq!(devices_of(&w, of_bob.session().id()).await, 0);
    // a login changed around the model (SQL; through the model a rename ends the sessions at
    // once, security review round 2 N3) makes carol's next check fail; its deletion runs the hook
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET login = 'caroline' WHERE id = $1",
        &[&carol],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(devices_of(&w, of_carol.session().id()).await, 1);
    let e = w
        .auth
        .begin_session(&cookie(&of_carol), CredentialOptions::new())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::SessionExpired);
    assert_eq!(devices_of(&w, of_carol.session().id()).await, 0);
    assert_eq!(w.auth.stats().refusal_delete_failures, 0);
}

/// Lead decision 6 (security review M4): setting a password ends every session of the user at
/// once, with the hooks, not at the sessions' next check.
#[tokio::test]
async fn setting_a_password_ends_the_sessions_eagerly() {
    let w = hooked_world().await;
    let carol = user_with_password(&w.h, "carol", "pw").await;
    let first = login(&w.auth, "carol", "pw").await.unwrap();
    let second = login(&w.auth, "carol", "pw").await.unwrap();
    device(&w, first.session().id(), "tablet").await;
    device(&w, second.session().id(), "phone").await;
    let mut tx = w.h.begin().await.unwrap();
    write(&mut tx, "res.users", &[carol], &[("password", s("new"))])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        count(&w.h, &format!("erp_session WHERE user_id = {carol}")).await,
        0
    );
    assert_eq!(devices_of(&w, first.session().id()).await, 0);
    assert_eq!(devices_of(&w, second.session().id()).await, 0);
}

/// Security review round 2 N3: renaming a user's `login` ends every session of the user at once,
/// with the hooks (their tokens cover the login), as a password set does; a write that keeps the
/// login, or renames another user, ends none.
#[tokio::test]
async fn renaming_the_login_ends_the_sessions_eagerly() {
    let w = hooked_world().await;
    let carol = user_with_password(&w.h, "carol", "pw").await;
    let dave = user_with_password(&w.h, "dave", "pw").await;
    let first = login(&w.auth, "carol", "pw").await.unwrap();
    let of_dave = login(&w.auth, "dave", "pw").await.unwrap();
    device(&w, first.session().id(), "tablet").await;
    let mut tx = w.h.begin().await.unwrap();
    write(&mut tx, "res.users", &[carol], &[("login", s("carol"))])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(devices_of(&w, first.session().id()).await, 1, "same login");
    let mut tx = w.h.begin().await.unwrap();
    write(&mut tx, "res.users", &[carol], &[("login", s("caroline"))])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        count(&w.h, &format!("erp_session WHERE user_id = {carol}")).await,
        0
    );
    assert_eq!(devices_of(&w, first.session().id()).await, 0);
    assert_eq!(check_user(&w.auth, &cookie(&of_dave)).await.unwrap(), dave);
    login(&w.auth, "caroline", "pw").await.unwrap();
}

/// Lead decision 6: a login that rehashes the stored password (a legacy hash) ends the user's
/// other sessions at once, with the hooks; the new session holds.
#[tokio::test]
async fn a_rehash_on_login_ends_the_other_sessions() {
    let w = hooked_world().await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let old = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(!old.auth().rehash());
    device(&w, old.session().id(), "laptop").await;
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    let new = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(new.auth().rehash());
    assert_eq!(devices_of(&w, old.session().id()).await, 0);
    assert_eq!(
        count(&w.h, &format!("erp_session WHERE user_id = {bob}")).await,
        1
    );
    assert_eq!(check_user(&w.auth, &cookie(&new)).await.unwrap(), bob);
    assert_eq!(w.auth.stats().session_end_failures, 0);
}

/// Lead decision 7 (security review M5): a user keeps at most 100 sessions; the login past it ends
/// the oldest, with the hooks.
#[tokio::test]
async fn a_login_past_the_session_cap_ends_the_oldest() {
    let w = hooked_world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let oldest = login(&w.auth, "bob", "pw").await.unwrap();
    device(&w, oldest.session().id(), "old phone").await;
    // 99 more rows, newer than the first (their ids are later UUIDv7s)
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at) \
         SELECT uuidv7(), $1, sha256(gen_random_uuid()::text::bytea), \
                sha256(gen_random_uuid()::text::bytea), 1, 'active', now() + interval '1 day' \
           FROM generate_series(1, 99)",
        &[&bob],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let sessions = format!("erp_session WHERE user_id = {bob}");
    let cap = i64::from(SESSIONS_PER_USER);
    assert_eq!(count(&w.h, &sessions).await, cap);
    let newest = login(&w.auth, "bob", "pw").await.unwrap();
    assert_eq!(count(&w.h, &sessions).await, cap);
    assert_eq!(
        count(
            &w.h,
            &format!("erp_session WHERE id = '{}'::uuid", oldest.session().id())
        )
        .await,
        0,
        "the oldest went"
    );
    assert_eq!(devices_of(&w, oldest.session().id()).await, 0);
    assert_eq!(check_user(&w.auth, &cookie(&newest)).await.unwrap(), bob);
}

/// Lead decision 9, database review H2: with `_check_credentials` overridden (`test_auth`), the
/// login keeps its first transaction open for the override; the override's writes commit with a
/// success and roll back with its refusal, which is `Denied` and counted like a wrong password.
#[tokio::test]
async fn an_overridden_credential_check_runs_in_the_login_transaction() {
    let w = hooked_world().await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    let gated = user_with_password(&w.h, "gated-carol", "pw").await;
    login(&w.auth, "bob", "pw").await.unwrap();
    assert_eq!(
        count(&w.h, &format!("test_auth_check WHERE uid = {bob}")).await,
        1
    );
    match login_from(&w.auth, "gated-carol", "pw", "192.0.2.61").await {
        Err(erp_security::auth::LoginError::Denied) => {}
        other => panic!("expected Denied, got {other:?}"),
    }
    assert_eq!(
        count(&w.h, &format!("test_auth_check WHERE uid = {gated}")).await,
        0,
        "the refusal rolled the override's write back"
    );
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'address' AND failures = 1"
        )
        .await,
        1,
        "the refusal counted"
    );
    // a wrong password is refused by `super` before the override writes anything
    match login(&w.auth, "bob", "wrong").await {
        Err(erp_security::auth::LoginError::Denied) => {}
        other => panic!("expected Denied, got {other:?}"),
    }
    assert_eq!(
        count(&w.h, &format!("test_auth_check WHERE uid = {bob}")).await,
        1
    );
}

/// Silent-failure review S1: an overridden `_check_credentials` that fails (not a refusal) is
/// `Failed` with its error, nothing counted, and the verdict task logs and counts it itself.
#[tokio::test]
async fn a_failing_credential_check_is_counted_by_the_verdict_task() {
    let w = hooked_world().await;
    user_with_password(&w.h, "broken-dave", "pw").await;
    match login_from(&w.auth, "broken-dave", "pw", "192.0.2.62").await {
        Err(erp_security::auth::LoginError::Failed(e)) => assert_eq!(
            (e.kind(), e.message()),
            (
                ErrorKind::Validation,
                "test_auth cannot check this credential"
            )
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(w.auth.stats().verdict_failures, 1);
    assert_eq!(
        count(&w.h, "erp_login_throttle").await,
        0,
        "nothing counted"
    );
}

/// The `test_auth.ended` rows: why each batch of sessions ended and the isolation level of the
/// transaction the hook ran in, oldest first.
async fn ended_rows(w: &World) -> Vec<(String, String)> {
    let mut tx = w.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT reason, isolation FROM test_auth_ended ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

/// Security review round 2 N7, database review round 2 N9: the sessions a login ends (after a
/// rehash, and past the cap) and a logout end in a READ COMMITTED transaction, as the design says;
/// the hook reads the isolation level of the transaction it runs in.
#[tokio::test]
async fn session_ends_after_a_login_and_at_logout_run_at_read_committed() {
    let w = hooked_world().await;
    set_param(&w.h, "password.hashing.scheme", "pbkdf2_sha512").await;
    let bob = user_with_password(&w.h, "bob", "pw").await;
    login(&w.auth, "bob", "pw").await.unwrap();
    set_param(&w.h, "password.hashing.scheme", "argon2id").await;
    // the rehash ends the first session
    let rehashed = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(rehashed.auth().rehash());
    // 99 more rows, newer than the rehashed login's: the next login is past the cap
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at) \
         SELECT uuidv7(), $1, sha256(gen_random_uuid()::text::bytea), \
                sha256(gen_random_uuid()::text::bytea), 1, 'active', now() + interval '1 day' \
           FROM generate_series(1, 99)",
        &[&bob],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let newest = login(&w.auth, "bob", "pw").await.unwrap();
    assert!(w.auth.logout(newest.session().id()).await.unwrap());
    let rc = "read committed".to_owned();
    assert_eq!(
        ended_rows(&w).await,
        [
            ("CredentialChanged".to_owned(), rc.clone()),
            ("OverCap".to_owned(), rc.clone()),
            ("Logout".to_owned(), rc),
        ]
    );
    assert_eq!(w.auth.stats().session_end_failures, 0);
}
