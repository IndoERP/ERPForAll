//! Bounded administrator resets reuse the shared identity/session service.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod auth_support;
use auth_support::*;
use erp_sdk::{ErrorKind, UserId};
use erp_security::auth::{IdentityCheck, PasswordReset};

/// The access manager `reset-manager` of the world's one company ([`company_scope`]: users the
/// test creates afterwards share that company, so the reset's company scope admits them).
async fn manager(w: &World) -> i64 {
    company_scope(&w.h).await;
    let uid = user_with_password(&w.h, "reset-manager", "actor-old").await;
    let mut tx = w.h.begin().await.unwrap();
    let grp = named_group(&mut tx, "base.group_erp_manager", "Access Administration").await;
    records(&tx, "res.users", &[uid])
        .write_values(
            &mut tx,
            &map(&[("group_ids", erp_sdk::Value::list([link_cmd(grp)]))]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    uid
}

#[tokio::test]
async fn manager_resets_multiple_users_and_retains_its_cookie_when_selected() {
    let w = world().await;
    let actor = manager(&w).await;
    let a = user_with_password(&w.h, "reset-a", "target-old-a").await;
    let b = user_with_password(&w.h, "reset-b", "target-old-b").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let actor_other = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let target = login(&w.auth, "reset-a", "target-old-a").await.unwrap();
    let secret = cookie(&current);
    let changes = [
        PasswordReset::new(UserId::new(a).unwrap(), "new-a").unwrap(),
        PasswordReset::new(UserId::new(b).unwrap(), "").unwrap(),
        PasswordReset::new(UserId::new(actor).unwrap(), "actor-new").unwrap(),
    ];
    let before = stored_hash(&w.h, a).await;
    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap(),
        IdentityCheck::Required
    );
    assert_eq!(stored_hash(&w.h, a).await, before);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), actor);
    for gone in [cookie(&actor_other), cookie(&target)] {
        let error = check_user(&w.auth, &gone).await.unwrap_err();
        assert_eq!(
            (error.kind(), error.message()),
            (ErrorKind::SessionExpired, "user is not connected")
        );
    }
    assert_eq!(
        login(&w.auth, "reset-a", "new-a")
            .await
            .unwrap()
            .uid()
            .get(),
        a
    );
    assert_eq!(
        login(&w.auth, "reset-b", "target-old-b")
            .await
            .unwrap()
            .uid()
            .get(),
        b
    );
    assert_eq!(
        login(&w.auth, "reset-manager", "actor-new")
            .await
            .unwrap()
            .uid()
            .get(),
        actor
    );
}

#[tokio::test]
async fn confirmation_does_not_grant_an_ordinary_user_reset_authority() {
    let w = world().await;
    manager(&w).await;
    let uid = user_with_password(&w.h, "ordinary-reset", "old").await;
    let target = user_with_password(&w.h, "ordinary-target", "unchanged").await;
    let current = login(&w.auth, "ordinary-reset", "old").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, target).await;
    let changes = [PasswordReset::new(UserId::new(target).unwrap(), "refused").unwrap()];
    let error = w
        .auth
        .reset_passwords(Some(&secret), &changes)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Access,
            "Password resets require access administration authority."
        )
    );
    assert_eq!(stored_hash(&w.h, target).await, before);
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), uid);
}

#[test]
fn reset_input_does_not_debug_a_secret_and_rejects_excess_bytes() {
    let uid = UserId::new(2).unwrap();
    let change = PasswordReset::new(uid, "sensitive-probe").unwrap();
    assert!(!format!("{change:?}").contains("sensitive-probe"));
    let error = PasswordReset::new(uid, &"x".repeat(1025)).unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Validation,
            "The new password exceeds the configured credential byte limit."
        )
    );
}

#[tokio::test]
async fn invalid_batch_never_partially_changes_a_target() {
    let w = world().await;
    manager(&w).await;
    let uid = user_with_password(&w.h, "reset-valid", "before").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, uid).await;
    let duplicate = [
        PasswordReset::new(UserId::new(uid).unwrap(), "one").unwrap(),
        PasswordReset::new(UserId::new(uid).unwrap(), "two").unwrap(),
    ];
    let error = w
        .auth
        .reset_passwords(Some(&secret), &duplicate)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Validation,
            "A password reset may name each user only once."
        )
    );
    let missing = [
        PasswordReset::new(UserId::new(uid).unwrap(), "one").unwrap(),
        PasswordReset::new(UserId::new(i64::MAX).unwrap(), "two").unwrap(),
    ];
    let jobs = w.pool.stats().jobs;
    let error = w
        .auth
        .reset_passwords(Some(&secret), &missing)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Missing,
            "A selected password reset user no longer exists."
        )
    );
    assert_eq!(stored_hash(&w.h, uid).await, before);
    assert_eq!(
        w.pool.stats().jobs,
        jobs,
        "missing targets must be refused before hashing"
    );
}

#[tokio::test]
async fn one_session_end_hook_failure_rolls_back_the_whole_batch() {
    let h = erp_testkit::Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_auth::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .unwrap();
    let w = world_with(
        h,
        erp_security::auth::AuthConfig::DEFAULT,
        erp_security::auth::HashPoolConfig::DEFAULT,
    )
    .await;
    manager(&w).await;
    let a = user_with_password(&w.h, "rollback-a", "before-a").await;
    let b = user_with_password(&w.h, "rollback-b", "before-b").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let target = login(&w.auth, "rollback-b", "before-b").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let mut tx = w.h.begin().await.unwrap();
    create(
        &mut tx,
        "test_auth.device",
        &[
            ("session_id", s(&target.session().id().to_text())),
            ("name", s("refuse")),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    let before_a = stored_hash(&w.h, a).await;
    let before_b = stored_hash(&w.h, b).await;
    let changes = [
        PasswordReset::new(UserId::new(a).unwrap(), "after-a").unwrap(),
        PasswordReset::new(UserId::new(b).unwrap(), "after-b").unwrap(),
    ];
    let error = w
        .auth
        .reset_passwords(Some(&secret), &changes)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Validation,
            "test_auth refuses to end this session"
        )
    );
    assert_eq!(stored_hash(&w.h, a).await, before_a);
    assert_eq!(stored_hash(&w.h, b).await, before_b);
    assert_eq!(check_user(&w.auth, &cookie(&target)).await.unwrap(), b);
}

#[tokio::test]
async fn revoked_actor_during_hashing_cannot_publish_any_reset() {
    let w = world().await;
    manager(&w).await;
    let target = user_with_password(&w.h, "race-target", "before").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    w.auth
        .confirm_identity(&cookie(&current), "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, target).await;
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let changing = std::sync::Arc::clone(&auth);
    let secret = cookie(&current);
    let work = tokio::spawn(async move {
        changing
            .reset_passwords(
                Some(&secret),
                &[PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()],
            )
            .await
    });
    until_waiting(w.pool, 1).await;
    auth.logout(current.session().id()).await.unwrap();
    drop(release);
    let error = work.await.unwrap().unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::SessionExpired, "user is not connected")
    );
    assert_eq!(stored_hash(&w.h, target).await, before);
}

#[tokio::test]
async fn manager_confirmation_and_revocation_are_shared_by_two_services() {
    let w = world().await;
    let mut tx = w.h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let actor = manager(&w).await;
    let target = user_with_password(&w.h, "node-target", "before").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let target_session = login(&w.auth, "node-target", "before").await.unwrap();
    let node = node_with(
        &w.h,
        4,
        own_pool(erp_security::auth::HashPoolConfig::DEFAULT),
    )
    .await;
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    assert_eq!(
        node.reset_passwords(
            Some(&secret),
            &[PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()]
        )
        .await
        .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), actor);
    let error = check_user(&w.auth, &cookie(&target_session))
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::SessionExpired, "user is not connected")
    );
}

#[tokio::test]
async fn final_rpc_policy_gate_can_refuse_without_any_password_effect() {
    let w = world().await;
    let actor = manager(&w).await;
    let target = user_with_password(&w.h, "gate-target", "before").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, target).await;
    let gate = |principal: &erp_orm::Principal| {
        assert_eq!(principal.subject().get(), actor);
        Err(erp_sdk::Error::access("test policy refuses this operation"))
    };
    let error = w
        .auth
        .reset_passwords_authorized(
            Some(&secret),
            &[PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()],
            &gate,
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::Access, "test policy refuses this operation")
    );
    assert_eq!(stored_hash(&w.h, target).await, before);
}

#[tokio::test]
async fn target_session_admission_refuses_excess_and_accepts_exact_boundary() {
    let w = world().await;
    manager(&w).await;
    let target = user_with_password(&w.h, "session-target", "before").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let mut tx = w.h.begin().await.unwrap();
    tx.execute("INSERT INTO erp_session (id,user_id,sid_digest,token,token_kid,state,expires_at) SELECT uuidv7(), $1, sha256(gen_random_uuid()::text::bytea), sha256(gen_random_uuid()::text::bytea), 1, 'active', now() - interval '1 day' FROM generate_series(1,101)",&[&target]).await.unwrap();
    tx.commit().await.unwrap();
    let before = stored_hash(&w.h, target).await;
    let changes = [PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()];
    let error = w
        .auth
        .reset_passwords(Some(&secret), &changes)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Validation,
            "Identity operations support at most 100 stored sessions per user; end excess sessions and retry."
        )
    );
    assert_eq!(stored_hash(&w.h, target).await, before);
    let mut tx = w.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT count(*)::bigint FROM erp_session WHERE user_id = $1",
            &[&target],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 101);
    tx.execute("DELETE FROM erp_session WHERE id = (SELECT id FROM erp_session WHERE user_id = $1 ORDER BY id LIMIT 1)",&[&target]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    let mut tx = w.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT count(*)::bigint FROM erp_session WHERE user_id = $1",
            &[&target],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 0);
    tx.rollback().await.unwrap();
}

mod commit_probe {
    erp_sdk::addon!(name = "password_commit_probe", depends = ["base"]);
    erp_sdk::auth::session_end_hook!(LOSE_COMMIT, addon = "password_commit_probe", run = inject);
    fn inject<'a>(
        tx: &'a mut erp_sdk::Transaction,
        _ended: &'a erp_sdk::auth::SessionEnd,
    ) -> erp_sdk::BoxFuture<'a, erp_sdk::Result<()>> {
        Box::pin(async move {
            tx.inject_commit_fault(erp_orm::CommitFault::AnswerLostAfterCommit);
            Ok(())
        })
    }
}

#[tokio::test]
async fn an_applied_reset_with_lost_commit_answer_is_unknown_and_not_replayed() {
    let h = erp_testkit::Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &commit_probe::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .unwrap();
    let w = world_with(
        h,
        erp_security::auth::AuthConfig::DEFAULT,
        erp_security::auth::HashPoolConfig::DEFAULT,
    )
    .await;
    manager(&w).await;
    let target = user_with_password(&w.h, "unknown-target", "before").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let target_session = login(&w.auth, "unknown-target", "before").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, target).await;
    let error = w
        .auth
        .reset_passwords(
            Some(&secret),
            &[PasswordReset::new(UserId::new(target).unwrap(), "after-unknown").unwrap()],
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Database,
            "Password operation commit outcome is unknown."
        )
    );
    use std::error::Error as _;
    assert!(matches!(
        error
            .source()
            .and_then(|e| e.downcast_ref::<erp_orm::CommitError>()),
        Some(erp_orm::CommitError::Unknown(_))
    ));
    assert_ne!(
        stored_hash(&w.h, target).await,
        before,
        "fault was after actual COMMIT"
    );
    let error = check_user(&w.auth, &cookie(&target_session))
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::SessionExpired, "user is not connected")
    );
    assert_eq!(
        check_user(&w.auth, &secret).await.unwrap(),
        current.uid().get()
    );
}

#[derive(Debug)]
struct RpcHost;
impl erp_rpc::RpcHost for RpcHost {
    fn language_active(&self, code: &str) -> erp_sdk::Result<bool> {
        Ok(code == "en_US")
    }
    fn has_capability(
        &self,
        _: &erp_orm::Principal,
        _: &erp_orm::Capability,
    ) -> erp_sdk::Result<bool> {
        Ok(false)
    }
}

#[tokio::test]
async fn rpc_without_browser_credential_has_no_managed_password_authority() {
    let w = world().await;
    let uid = user_with_password(&w.h, "no-http", "old").await;
    let signed = login(&w.auth, "no-http", "old").await.unwrap();
    let begun = w
        .auth
        .begin_session(
            &cookie(&signed),
            erp_security::auth::CredentialOptions::new(),
        )
        .await
        .unwrap();
    let principal = begun.tx.env().principal().clone();
    let mut tx = begun.tx;
    tx.rollback().await.unwrap();
    drop(tx);
    let service = erp_rpc::RpcService::new(
        w.h.orm().clone(),
        RpcHost,
        erp_rpc::RpcConfig::builder().build().unwrap(),
    );
    let capture = erp_rpc::RpcCall::json2(
        "/erp/json/2/res.users/preference_change_password",
        format!("{{\"ids\":[{uid}]}}").into_bytes(),
        erp_rpc::Caller::Principal(principal.clone()),
    )
    .capture_statements();
    let captured = service.call(capture).await;
    assert_eq!(captured.status(), 422);
    let failure: erp_sdk::serde_json::Value =
        erp_sdk::serde_json::from_slice(captured.body()).unwrap();
    assert_eq!(failure["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        failure["message"],
        "Password operations do not accept statement capture."
    );
    let call = erp_rpc::RpcCall::json2(
        "/erp/json/2/res.users/preference_change_password",
        format!("{{\"ids\":[{uid}]}}").into_bytes(),
        erp_rpc::Caller::Principal(principal),
    );
    let reply = service.call(call).await;
    assert_eq!(reply.status(), 422);
    let value: erp_sdk::serde_json::Value = erp_sdk::serde_json::from_slice(reply.body()).unwrap();
    assert_eq!(value["name"], "erp.exceptions.UserError");
    assert_eq!(
        value["message"],
        "This method can only be accessed over HTTP"
    );
}

#[tokio::test]
async fn removed_manager_authority_during_hash_work_refuses_the_reset() {
    let w = world().await;
    let actor = manager(&w).await;
    let target = user_with_password(&w.h, "authority-target", "before").await;
    let signed = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&signed);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, target).await;
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let changing = std::sync::Arc::clone(&auth);
    let work = tokio::spawn(async move {
        changing
            .reset_passwords(
                Some(&secret),
                &[PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()],
            )
            .await
    });
    until_waiting(w.pool, 1).await;
    let mut tx = w.h.begin().await.unwrap();
    let changed=tx.execute("DELETE FROM res_groups_users_rel WHERE uid = $1 AND gid = (SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'group_erp_manager')",&[&actor]).await.unwrap();
    assert_eq!(changed, 1);
    tx.commit().await.unwrap();
    drop(release);
    let error = work.await.unwrap().unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Access,
            "Password resets require access administration authority."
        )
    );
    assert_eq!(stored_hash(&w.h, target).await, before);
}

#[tokio::test]
async fn selected_user_admission_accepts_one_hundred_and_refuses_one_hundred_one() {
    let w = world().await;
    let actor = manager(&w).await;
    let signed = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&signed);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let mut tx = w.h.begin().await.unwrap();
    let values: Vec<_> = (0..100)
        .map(|i| {
            map(&[
                ("name", s(&format!("Selected {i}"))),
                ("login", s(&format!("selected-{i}"))),
            ])
        })
        .collect();
    let users = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(&mut tx, &values)
        .await
        .unwrap();
    let mut changes: Vec<_> = users
        .ids()
        .iter()
        .map(|id| PasswordReset::new(UserId::new(*id).unwrap(), "").unwrap())
        .collect();
    tx.commit().await.unwrap();
    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    changes.push(PasswordReset::new(UserId::new(actor).unwrap(), "").unwrap());
    let error = w
        .auth
        .reset_passwords(Some(&secret), &changes)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Validation,
            "A password reset requires between 1 and 100 selected users."
        )
    );
}

#[tokio::test]
async fn configured_reset_byte_limit_applies_before_whitespace_normalization() {
    let h = builder().build().await.unwrap();
    let w = world_with(
        h,
        erp_security::auth::AuthConfig::DEFAULT
            .with_credential_bytes(64)
            .unwrap(),
        erp_security::auth::HashPoolConfig::DEFAULT,
    )
    .await;
    manager(&w).await;
    let target = user_with_password(&w.h, "byte-target", "before").await;
    let before = stored_hash(&w.h, target).await;
    let signed = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&signed);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let padded = format!("new{}", " ".repeat(64));
    let error = w
        .auth
        .reset_passwords(
            Some(&secret),
            &[PasswordReset::new(UserId::new(target).unwrap(), &padded).unwrap()],
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Validation,
            "The new password exceeds the configured credential byte limit."
        )
    );
    assert_eq!(stored_hash(&w.h, target).await, before);
}

#[tokio::test]
async fn whitespace_only_lines_change_nothing_and_non_ascii_passwords_work() {
    let w = world().await;
    manager(&w).await;
    let blank = user_with_password(&w.h, "reset-blank", "blank-old").await;
    let wide = user_with_password(&w.h, "reset-wide", "wide-old").await;
    let current = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&current);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let before = stored_hash(&w.h, blank).await;
    let unicode = "pässwörd-日本語-🔑";
    let changes = [
        PasswordReset::new(UserId::new(blank).unwrap(), " \t  ").unwrap(),
        PasswordReset::new(UserId::new(wide).unwrap(), unicode).unwrap(),
    ];
    assert!(changes[0].is_blank() && !changes[1].is_blank());
    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(stored_hash(&w.h, blank).await, before);
    assert!(login(&w.auth, "reset-blank", "blank-old").await.is_ok());
    assert!(login(&w.auth, "reset-wide", "wide-old").await.is_err());
    assert_eq!(
        login(&w.auth, "reset-wide", unicode)
            .await
            .unwrap()
            .uid()
            .get(),
        wide
    );
}

/// sessions end, presented cookie stays valid)
#[tokio::test]
async fn own_change_ends_the_old_password_and_other_sessions_and_keeps_the_cookie() {
    let w = world().await;
    let uid = user_with_password(&w.h, "own-change", "own-old").await;
    let current = login(&w.auth, "own-change", "own-old").await.unwrap();
    let other = login(&w.auth, "own-change", "own-old").await.unwrap();
    let secret = cookie(&current);
    assert_eq!(
        w.auth
            .change_password(Some(&secret), "own-new")
            .await
            .unwrap(),
        IdentityCheck::Required
    );
    w.auth
        .confirm_identity(&secret, "own-old", IN_PROCESS)
        .await
        .unwrap();
    let unicode = "pässwörd-日本語-🔑";
    assert_eq!(
        w.auth
            .change_password(Some(&secret), unicode)
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(check_user(&w.auth, &secret).await.unwrap(), uid);
    let error = check_user(&w.auth, &cookie(&other)).await.unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::SessionExpired, "user is not connected")
    );
    assert!(login(&w.auth, "own-change", "own-old").await.is_err());
    assert_eq!(
        login(&w.auth, "own-change", unicode)
            .await
            .unwrap()
            .uid()
            .get(),
        uid
    );
}

/// The default company of user `uid`.
async fn company_of(w: &World, uid: i64) -> i64 {
    let mut tx = w.h.begin().await.unwrap();
    let id: i64 = tx
        .query("SELECT company_id FROM res_users WHERE id = $1", &[&uid])
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    id
}

/// The signed-in, identity-confirmed session of `reset-manager`.
async fn confirmed_manager(w: &World) -> erp_security::auth::SessionSecret {
    let signed = login(&w.auth, "reset-manager", "actor-old").await.unwrap();
    let secret = cookie(&signed);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    secret
}

/// The `(kind, message)` the reset answers for a user id that never existed: the reference every
/// hidden target's answer must equal (design D3: existence is not revealed).
async fn unknown_user_answer(
    w: &World,
    secret: &erp_security::auth::SessionSecret,
) -> (ErrorKind, String) {
    let never = [PasswordReset::new(UserId::new(i64::from(i32::MAX)).unwrap(), "never").unwrap()];
    let error = w
        .auth
        .reset_passwords(Some(secret), &never)
        .await
        .unwrap_err();
    (error.kind(), error.message().to_owned())
}

#[tokio::test]
async fn manager_with_two_companies_resets_portal_users_of_either() {
    let w = world().await;
    let actor = manager(&w).await;
    let portal = base_group(&w.h, "group_portal").await;
    let in_a = user_in_groups(&w.h, "portal-a", "before-a", &[portal]).await;
    let in_b = user_in_groups(&w.h, "portal-b", "before-b", &[portal]).await;
    let a = company_of(&w, actor).await;
    let b = add_company(&w.h, "Second company").await;
    set_companies(&w.h, actor, &[a, b]).await;
    set_companies(&w.h, in_a, &[a]).await;
    set_companies(&w.h, in_b, &[b]).await;
    let secret = confirmed_manager(&w).await;
    for (target, login_name) in [(in_a, "portal-a"), (in_b, "portal-b")] {
        let changes = [PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()];
        assert_eq!(
            w.auth
                .reset_passwords(Some(&secret), &changes)
                .await
                .unwrap(),
            IdentityCheck::Verified,
            "{login_name}"
        );
        assert_eq!(
            login(&w.auth, login_name, "after")
                .await
                .unwrap()
                .uid()
                .get(),
            target
        );
    }
}

/// the unknown-user answer, before any hashing)
#[tokio::test]
async fn manager_cannot_reset_a_hidden_portal_user_and_the_answer_matches_an_unknown_user() {
    let w = world().await;
    manager(&w).await;
    let portal = base_group(&w.h, "group_portal").await;
    let far = user_in_groups(&w.h, "portal-far", "far-before", &[portal]).await;
    let other = add_company(&w.h, "Other company").await;
    set_companies(&w.h, far, &[other]).await;
    let secret = confirmed_manager(&w).await;
    let reference = unknown_user_answer(&w, &secret).await;
    let before = stored_hash(&w.h, far).await;
    let jobs = w.pool.stats().jobs;
    let changes = [PasswordReset::new(UserId::new(far).unwrap(), "far-after").unwrap()];
    let error = w
        .auth
        .reset_passwords(Some(&secret), &changes)
        .await
        .unwrap_err();
    assert_eq!((error.kind(), error.message().to_owned()), reference);
    assert_eq!(stored_hash(&w.h, far).await, before);
    assert_eq!(
        w.pool.stats().jobs,
        jobs,
        "a hidden target is refused before hashing"
    );
}

/// while the hashes are computed is refused after the final locks)
#[tokio::test]
async fn final_transaction_rechecks_company_visibility_after_locks() {
    let w = world().await;
    manager(&w).await;
    let portal = base_group(&w.h, "group_portal").await;
    let target = user_in_groups(&w.h, "portal-moved", "before", &[portal]).await;
    let other = add_company(&w.h, "Other company").await;
    let secret = confirmed_manager(&w).await;
    let reference = unknown_user_answer(&w, &secret).await;
    let before = stored_hash(&w.h, target).await;
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let changing = std::sync::Arc::clone(&auth);
    let work = tokio::spawn(async move {
        changing
            .reset_passwords(
                Some(&secret),
                &[PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()],
            )
            .await
    });
    // the preflight passed: the reset waits for a hashing permit
    until_waiting(w.pool, 1).await;
    set_companies(&w.h, target, &[other]).await;
    drop(release);
    let error = work.await.unwrap().unwrap_err();
    assert_eq!((error.kind(), error.message().to_owned()), reference);
    assert_eq!(stored_hash(&w.h, target).await, before);
}

/// the hashes are computed is refused after the final locks)
#[tokio::test]
async fn final_transaction_rechecks_a_settings_administrator_target() {
    let w = world().await;
    manager(&w).await;
    let target = user_with_password(&w.h, "promoted-target", "before").await;
    let system = base_group(&w.h, "group_system").await;
    let secret = confirmed_manager(&w).await;
    let before = stored_hash(&w.h, target).await;
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let changing = std::sync::Arc::clone(&auth);
    let work = tokio::spawn(async move {
        changing
            .reset_passwords(
                Some(&secret),
                &[PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()],
            )
            .await
    });
    until_waiting(w.pool, 1).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO res_groups_users_rel (gid, uid) VALUES ($1, $2)",
        &[&system, &target],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(release);
    let error = work.await.unwrap().unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Access,
            "Only a Settings administrator can reset the password of a Settings administrator."
        )
    );
    assert_eq!(stored_hash(&w.h, target).await, before);
}

#[tokio::test]
async fn settings_administrator_may_reset_another_settings_administrator() {
    let w = world().await;
    // binds base.group_erp_manager, which the reset's authority check names
    manager(&w).await;
    let system = base_group(&w.h, "group_system").await;
    user_in_groups(&w.h, "settings-actor", "actor-old", &[system]).await;
    let target = user_in_groups(&w.h, "settings-target", "before", &[system]).await;
    let signed = login(&w.auth, "settings-actor", "actor-old").await.unwrap();
    let secret = cookie(&signed);
    w.auth
        .confirm_identity(&secret, "actor-old", IN_PROCESS)
        .await
        .unwrap();
    let changes = [PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()];
    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(
        login(&w.auth, "settings-target", "after")
            .await
            .unwrap()
            .uid()
            .get(),
        target
    );
}

/// The signed-in, identity-confirmed session of a new Settings administrator `login` whose
/// companies are exactly `companies`.
async fn confirmed_settings_administrator(
    w: &World,
    login_name: &str,
    companies: &[i64],
) -> erp_security::auth::SessionSecret {
    let system = base_group(&w.h, "group_system").await;
    let uid = user_in_groups(&w.h, login_name, "settings-old", &[system]).await;
    set_companies(&w.h, uid, companies).await;
    let signed = login(&w.auth, login_name, "settings-old").await.unwrap();
    let secret = cookie(&signed);
    w.auth
        .confirm_identity(&secret, "settings-old", IN_PROCESS)
        .await
        .unwrap();
    secret
}

/// access manager with the unknown-user answer before any hashing; a Settings administrator of
/// the manager's company resets it)
#[tokio::test]
async fn manager_cannot_reset_an_internal_user_of_another_company() {
    let w = world().await;
    let actor = manager(&w).await;
    let a = company_of(&w, actor).await;
    let internal = base_group(&w.h, "group_user").await;
    let target = user_in_groups(&w.h, "internal-b", "b-before", &[internal]).await;
    let system = base_group(&w.h, "group_system").await;
    let settings = user_in_groups(&w.h, "settings-a", "settings-old", &[system]).await;
    let b = add_company(&w.h, "Company B").await;
    set_companies(&w.h, actor, &[a]).await;
    set_companies(&w.h, target, &[b]).await;
    let mut tx = w.h.begin().await.unwrap();
    let share: bool = tx
        .query("SELECT share FROM res_users WHERE id = $1", &[&target])
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    assert!(!share, "the target is an internal user");
    let secret = confirmed_manager(&w).await;
    let reference = unknown_user_answer(&w, &secret).await;
    let before = stored_hash(&w.h, target).await;
    let jobs = w.pool.stats().jobs;
    let changes = [PasswordReset::new(UserId::new(target).unwrap(), "b-after").unwrap()];
    let error = w
        .auth
        .reset_passwords(Some(&secret), &changes)
        .await
        .unwrap_err();
    assert_eq!((error.kind(), error.message().to_owned()), reference);
    assert_eq!(stored_hash(&w.h, target).await, before);
    assert_eq!(
        w.pool.stats().jobs,
        jobs,
        "an internal target of another company is refused before hashing"
    );
    set_companies(&w.h, settings, &[a]).await;
    let signed = login(&w.auth, "settings-a", "settings-old").await.unwrap();
    let secret = cookie(&signed);
    w.auth
        .confirm_identity(&secret, "settings-old", IN_PROCESS)
        .await
        .unwrap();
    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(
        login(&w.auth, "internal-b", "b-after")
            .await
            .unwrap()
            .uid()
            .get(),
        target
    );
}

/// manager and for a Settings administrator, with the unknown-user answer; its hash is kept)
#[tokio::test]
async fn superuser_row_is_refused_as_a_reset_target_for_every_actor() {
    let w = world().await;
    let actor = manager(&w).await;
    let a = company_of(&w, actor).await;
    let root = erp_orm::SUPERUSER_ID.get();
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES ($1, 'System', '__system__', false) \
         ON CONFLICT (id) DO NOTHING",
        &[&root],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let before = stored_hash(&w.h, root).await;
    let changes = [PasswordReset::new(erp_orm::SUPERUSER_ID, "root-after").unwrap()];
    let manager_secret = confirmed_manager(&w).await;
    let settings_secret = confirmed_settings_administrator(&w, "settings-actor", &[a]).await;
    for (actor, secret) in [
        ("access manager", manager_secret),
        ("Settings administrator", settings_secret),
    ] {
        let reference = unknown_user_answer(&w, &secret).await;
        let error = w
            .auth
            .reset_passwords(Some(&secret), &changes)
            .await
            .unwrap_err();
        assert_eq!(
            (error.kind(), error.message().to_owned()),
            reference,
            "{actor}"
        );
        assert_eq!(stored_hash(&w.h, root).await, before, "{actor}");
    }
}

/// refused on a Settings administrator of the same company before any hashing)
#[tokio::test]
async fn preflight_refuses_a_settings_administrator_target_for_a_plain_manager() {
    let w = world().await;
    manager(&w).await;
    let system = base_group(&w.h, "group_system").await;
    let target = user_in_groups(&w.h, "settings-target", "before", &[system]).await;
    let secret = confirmed_manager(&w).await;
    let before = stored_hash(&w.h, target).await;
    let jobs = w.pool.stats().jobs;
    let changes = [PasswordReset::new(UserId::new(target).unwrap(), "after").unwrap()];
    let error = w
        .auth
        .reset_passwords(Some(&secret), &changes)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Access,
            "Only a Settings administrator can reset the password of a Settings administrator."
        )
    );
    assert_eq!(stored_hash(&w.h, target).await, before);
    assert_eq!(
        w.pool.stats().jobs,
        jobs,
        "a Settings administrator target is refused before hashing"
    );
}

/// not carry a hidden or a Settings administrator target through; a refusal changes no password)
#[tokio::test]
async fn the_actor_among_the_targets_carries_no_refused_target_through() {
    let w = world().await;
    let actor = manager(&w).await;
    let portal = base_group(&w.h, "group_portal").await;
    let system = base_group(&w.h, "group_system").await;
    let far = user_in_groups(&w.h, "portal-far", "far-before", &[portal]).await;
    let admin = user_in_groups(&w.h, "settings-target", "admin-before", &[system]).await;
    let near = user_in_groups(&w.h, "portal-near", "near-before", &[portal]).await;
    let other = add_company(&w.h, "Other company").await;
    set_companies(&w.h, far, &[other]).await;
    let secret = confirmed_manager(&w).await;
    let reference = unknown_user_answer(&w, &secret).await;
    let actor_before = stored_hash(&w.h, actor).await;
    let reset = |target: i64| {
        [
            PasswordReset::new(UserId::new(actor).unwrap(), "actor-new").unwrap(),
            PasswordReset::new(UserId::new(target).unwrap(), "target-new").unwrap(),
        ]
    };

    let far_before = stored_hash(&w.h, far).await;
    let error = w
        .auth
        .reset_passwords(Some(&secret), &reset(far))
        .await
        .unwrap_err();
    assert_eq!((error.kind(), error.message().to_owned()), reference);
    assert_eq!(stored_hash(&w.h, far).await, far_before);
    assert_eq!(stored_hash(&w.h, actor).await, actor_before);

    let admin_before = stored_hash(&w.h, admin).await;
    let error = w
        .auth
        .reset_passwords(Some(&secret), &reset(admin))
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Access,
            "Only a Settings administrator can reset the password of a Settings administrator."
        )
    );
    assert_eq!(stored_hash(&w.h, admin).await, admin_before);
    assert_eq!(stored_hash(&w.h, actor).await, actor_before);

    assert_eq!(
        w.auth
            .reset_passwords(Some(&secret), &reset(near))
            .await
            .unwrap(),
        IdentityCheck::Verified
    );
    assert_eq!(
        login(&w.auth, "portal-near", "target-new")
            .await
            .unwrap()
            .uid()
            .get(),
        near
    );
    assert_eq!(
        login(&w.auth, "reset-manager", "actor-new")
            .await
            .unwrap()
            .uid()
            .get(),
        actor
    );
}
