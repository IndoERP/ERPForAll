#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

use auth_support::*;
use erp_fields::{ErrorKind, Value};
use erp_security::auth::throttle::ThrottleKey;
use erp_security::auth::{Auth, AuthConfig, HashPoolConfig, LoginDomain, LoginError};
use erp_testkit::Harness;

async fn selection_world() -> World {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_login_selection::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");
    selection_fixture(h).await
}

async fn selection_fixture(h: Harness) -> World {
    let admin = prepare(&h).await;
    let pool = own_pool(HashPoolConfig::DEFAULT);
    let auth = erp_security::auth::test_support::login_selection_auth(
        h.orm().clone(),
        AuthConfig::DEFAULT,
        pool,
    )
    .await
    .unwrap();
    World {
        h,
        auth,
        pool,
        admin,
    }
}

async fn user_with_alias_and_priority(
    h: &Harness,
    login: &str,
    password: &str,
    alias: &str,
    priority: i32,
) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id = create(
        &mut tx,
        "res.users",
        &[
            ("name", s(login)),
            ("login", s(login)),
            ("password", s(password)),
            ("alias", s(alias)),
            ("priority", Value::from(priority)),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    id
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn lc01_effect_guard_rejects_db_queries_in_selection_hook() {
    let w = selection_world().await;
    user_with_alias_and_priority(&w.h, "alice", "pw123", "alice_alias", 10).await;

    let res = login(&w.auth, "query_effect:alice", "pw123").await;
    match res {
        Err(LoginError::Failed(e)) => {
            assert_eq!(e.kind(), ErrorKind::Programming);
            assert!(
                e.message().contains("ForbiddenEffect"),
                "expected ForbiddenEffect, got: {e}"
            );
        }
        other => panic!("expected LoginError::Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn lc02_super_chaining_returns_exact_login_and_id_asc() {
    let w = selection_world().await;
    user_with_alias_and_priority(&w.h, "bob", "pw_bob", "bob_alias", 5).await;

    // super_chain:bob delegates to sup.call
    let logged = login(&w.auth, "super_chain:bob", "pw_bob").await.unwrap();
    assert_eq!(logged.context().uid(), logged.auth().uid());
}

#[tokio::test]
async fn lc03_active_user_invariant_enforces_active_true() {
    let w = selection_world().await;
    let id = user_with_alias_and_priority(&w.h, "charlie", "pw_charlie", "charlie_alias", 1).await;

    // Deactivate user
    let mut tx = w.h.begin().await.unwrap();
    tx.execute("UPDATE res_users SET active = false WHERE id = $1", &[&id])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let res = login(&w.auth, "alias:charlie_alias", "pw_charlie").await;
    assert!(matches!(res, Err(LoginError::Denied)));
}

#[tokio::test]
async fn lc04_total_ordering_auto_appends_id_asc() {
    let w = selection_world().await;
    let u1 =
        user_with_alias_and_priority(&w.h, "user_first", "pw_shared", "shared_alias", 100).await;
    let _u2 =
        user_with_alias_and_priority(&w.h, "user_second", "pw_shared", "shared_alias", 100).await;

    // Both match shared_alias and have priority 100.
    // Order is priority DESC, then auto-appended id ASC.
    // So u1 (smaller id) must be selected!
    let logged = login(&w.auth, "alias:shared_alias", "pw_shared")
        .await
        .unwrap();
    assert_eq!(logged.auth().uid().get(), u1);
}

#[tokio::test]
async fn lc05_bounded_ast_limits() {
    let w = selection_world().await;

    // Deep domain exceeding max depth (16)
    let res = login(&w.auth, "deep_domain:test", "pw").await;
    match res {
        Err(LoginError::Failed(e)) => {
            assert!(e.message().contains("exceeds maximum limit"));
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // Too many nodes exceeding 1024
    let res2 = login(&w.auth, "too_many_nodes:test", "pw").await;
    match res2 {
        Err(LoginError::Failed(e)) => {
            assert!(e.message().contains("exceeds maximum limit"));
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn lc06_allowed_field_types_rejects_relational_or_invalid() {
    let w = selection_world().await;

    let res = login(&w.auth, "bad_field:test", "pw").await;
    match res {
        Err(LoginError::Failed(e)) => {
            assert!(e.message().contains("has no field"));
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let res2 = login(&w.auth, "relational_field:test", "pw").await;
    match res2 {
        Err(LoginError::Failed(e)) => {
            assert!(
                e.message()
                    .contains("relational traversal and property paths are unsupported")
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn lc08_candidate_selection_via_custom_alias() {
    let w = selection_world().await;
    let uid =
        user_with_alias_and_priority(&w.h, "dave_canonical", "secret123", "dave_alias", 42).await;

    let logged = login(&w.auth, "alias:dave_alias", "secret123")
        .await
        .unwrap();
    assert_eq!(logged.auth().uid().get(), uid);
}

#[test]
fn lc09_selection_witness_construction_and_validation() {
    use erp_orm::UserId;
    use erp_security::auth::SelectionWitness;

    let witness = SelectionWitness {
        uid: UserId::new(10).unwrap(),
        canonical_login: "test_user".to_string(),
        active: true,
        stored_hash: Some("$argon2id$v=19$m=4096,t=3,p=1$fake".to_string()),
        dependency_values: vec![Some("dep1".to_string())],
        plan_digest: [1u8; 32],
    };
    assert!(witness.validate_size(65_536).is_ok());
    assert!(witness.validate_size(10).is_err());
}

#[tokio::test]
async fn lc10_token_stores_canonical_login_not_alias() {
    let w = selection_world().await;
    user_with_alias_and_priority(&w.h, "eve_canonical", "eve_secret", "eve_alias", 10).await;

    let logged = login(&w.auth, "alias:eve_alias", "eve_secret")
        .await
        .unwrap();
    let mut session = w
        .auth
        .begin_session(
            logged.session().secret(),
            erp_security::auth::CredentialOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(session.session.user(), logged.auth().uid());
    let uid_val = logged.auth().uid().get();
    let rows = session
        .tx
        .query("SELECT login FROM res_users WHERE id = $1", &[&uid_val])
        .await
        .unwrap();
    let login: String = rows[0].get(0);
    assert_eq!(login, "eve_canonical");
}

#[tokio::test]
async fn initial_priority_selects_highest_candidate() {
    let w = selection_world().await;
    let u1 =
        user_with_alias_and_priority(&w.h, "candidate_low", "pass123", "dynamic_target", 10).await;
    let _u2 =
        user_with_alias_and_priority(&w.h, "candidate_high", "pass123", "dynamic_target", 5).await;

    // Initial order selects candidate_low (priority 10 > 5).
    let logged = login(&w.auth, "alias:dynamic_target", "pass123")
        .await
        .unwrap();
    assert_eq!(logged.auth().uid().get(), u1);
}

#[tokio::test]
async fn lc12_failed_auth_charges_principal_throttle() {
    let w = selection_world().await;
    let uid = user_with_alias_and_priority(&w.h, "frank", "frank_pw", "frank_alias", 1).await;

    let client_addr = "198.51.100.5".parse().unwrap();
    let req = erp_security::auth::LoginRequest::new(
        "alias:frank_alias",
        "wrong_password",
        erp_security::auth::ClientAddr::Known(client_addr),
    );
    let res = w.auth.authenticate(req).await;
    assert!(matches!(res, Err(LoginError::Denied)));

    // Verify that principal throttle row exists for UserId
    let principal_key = w
        .auth
        .throttle_key(ThrottleKey::Principal(erp_orm::UserId::new(uid).unwrap()))
        .await
        .unwrap();
    let hex_key = hex(&principal_key);
    let rows = count(
        &w.h,
        &format!(
            "erp_login_throttle WHERE kind = 'principal' AND key_hash = decode('{hex_key}', 'hex')"
        ),
    )
    .await;
    assert_eq!(rows, 1, "expected principal throttle row to be recorded");
}

#[tokio::test]
async fn lc13_successful_auth_updates_principal_trusted_throttle() {
    let w = selection_world().await;
    let uid = user_with_alias_and_priority(&w.h, "grace", "grace_pw", "grace_alias", 1).await;

    let client_ip = "198.51.100.12".parse().unwrap();
    let addr_key = erp_security::auth::throttle::address_key(client_ip);
    let req = erp_security::auth::LoginRequest::new(
        "alias:grace_alias",
        "grace_pw",
        erp_security::auth::ClientAddr::Known(client_ip),
    );
    let logged = w.auth.authenticate(req).await.unwrap();
    assert_eq!(logged.auth().uid().get(), uid);

    let p_trusted_key = w
        .auth
        .throttle_key(ThrottleKey::PrincipalTrusted(
            &addr_key,
            erp_orm::UserId::new(uid).unwrap(),
        ))
        .await
        .unwrap();
    let hex_key = hex(&p_trusted_key);
    let rows = count(
        &w.h,
        &format!("erp_login_throttle WHERE kind = 'principal_trusted' AND key_hash = decode('{hex_key}', 'hex')"),
    )
    .await;
    assert_eq!(
        rows, 1,
        "expected principal_trusted throttle row to be recorded"
    );
}

#[tokio::test]
async fn lc14_composition_rejection_with_credential_override() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_login_selection::ADDON,
        &erp_test_models::test_login_selection::credential_override::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .await
    .expect("harness");

    let _admin = prepare(&h).await;
    let pool = own_pool(HashPoolConfig::DEFAULT);
    for fixture in [false, true] {
        let result = if fixture {
            erp_security::auth::test_support::login_selection_auth(
                h.orm().clone(),
                AuthConfig::DEFAULT,
                pool,
            )
            .await
        } else {
            Auth::new(h.orm().clone(), AuthConfig::DEFAULT, pool).await
        };
        match result {
            Err(error) => assert_eq!(
                (error.kind(), error.message()),
                (
                    ErrorKind::Programming,
                    "custom login selection combined with credential override is unsupported until a later release"
                ),
            ),
            Ok(_) => {
                panic!("constructor accepted selection plus credential override; fixture={fixture}")
            }
        }
    }
}

#[tokio::test]
async fn spec_auth_040_alias_session_selection() {
    let w = selection_world().await;
    let uid =
        user_with_alias_and_priority(&w.h, "whole_case_user", "correct_pw", "whole_alias", 50)
            .await;

    // Login via alias
    let logged = login(&w.auth, "alias:whole_alias", "correct_pw")
        .await
        .unwrap();
    assert_eq!(logged.auth().uid().get(), uid);

    // Verify token has canonical login
    let mut session = w
        .auth
        .begin_session(
            logged.session().secret(),
            erp_security::auth::CredentialOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(session.session.user(), logged.auth().uid());
    let uid_val = logged.auth().uid().get();
    let rows = session
        .tx
        .query("SELECT login FROM res_users WHERE id = $1", &[&uid_val])
        .await
        .unwrap();
    let login: String = rows[0].get(0);
    assert_eq!(login, "whole_case_user");
}

#[tokio::test]
async fn recovery_failed_password_never_creates_principal_trust() {
    let w = selection_world().await;
    user_with_alias_and_priority(&w.h, "trust_canonical", "pw", "trust_alias", 1).await;
    assert!(matches!(
        login_from(&w.auth, "alias:trust_alias", "wrong", "198.51.100.44").await,
        Err(LoginError::Denied)
    ));
    assert_eq!(
        count(&w.h, "erp_login_throttle WHERE kind = 'principal_trusted'").await,
        0
    );
}

#[tokio::test]
async fn recovery_repeated_failures_charge_principal_atomically() {
    let w = selection_world().await;
    user_with_alias_and_priority(&w.h, "repeat_canonical", "pw", "repeat_alias", 1).await;
    for _ in 0..2 {
        assert!(matches!(
            login_from(&w.auth, "alias:repeat_alias", "wrong", "198.51.100.45").await,
            Err(LoginError::Denied)
        ));
    }
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind = 'principal' AND failures = 2"
        )
        .await,
        1
    );
    assert_eq!(w.auth.stats().failure_write_errors, 0);
}

#[tokio::test]
async fn recovery_principal_cooldown_survives_alias_and_address_rotation() {
    let w = selection_world().await;
    user_with_alias_and_priority(&w.h, "budget_canonical", "pw", "budget_alias", 1).await;
    set_param(&w.h, "base.login_cooldown_after", "100").await;
    set_param(&w.h, "base.login_cooldown_after_login", "2").await;
    assert!(matches!(
        login_from(&w.auth, "budget_canonical", "wrong", "198.51.100.46").await,
        Err(LoginError::Denied)
    ));
    assert!(matches!(
        login_from(&w.auth, "alias:budget_alias", "wrong", "198.51.100.47").await,
        Err(LoginError::Denied)
    ));
    let before = w.pool.stats().jobs;
    let error: erp_fields::Error =
        login_from(&w.auth, "order_desc:budget_alias", "pw", "198.51.100.48")
            .await
            .unwrap_err()
            .into();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::AccessDenied,
            erp_security::auth::COOLDOWN_MESSAGE
        )
    );
    assert_eq!(
        w.pool.stats().jobs,
        before,
        "cooldown precedes verification"
    );
}

#[tokio::test]
async fn recovery_session_insert_keeps_selection_guard() {
    let w = selection_world().await;
    let uid = user_with_alias_and_priority(&w.h, "guard_canonical", "pw", "guard_alias", 1).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute("CREATE FUNCTION assert_login_selection_guard() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('transaction_isolation') <> 'read committed' OR NOT EXISTS (SELECT 1 FROM pg_locks WHERE pid = pg_backend_pid() AND relation = 'res_users'::regclass AND mode = 'ShareRowExclusiveLock' AND granted) THEN RAISE EXCEPTION 'login selection guard missing at persistence'; END IF; RETURN NEW; END $$", &[]).await.unwrap();
    tx.execute("CREATE TRIGGER assert_login_selection_guard BEFORE INSERT ON erp_session FOR EACH ROW EXECUTE FUNCTION assert_login_selection_guard()", &[]).await.unwrap();
    tx.commit().await.unwrap();
    let logged = login(&w.auth, "alias:guard_alias", "pw").await.unwrap();
    assert_eq!(
        check_user(&w.auth, logged.session().secret())
            .await
            .unwrap(),
        uid
    );
}

#[tokio::test]
async fn recovery_equality_operand_bytes_are_bounded() {
    let w = selection_world().await;
    let raw = erp_domain::Domain::condition("alias", erp_domain::Operator::Eq, "x".repeat(65_537))
        .unwrap();
    let result = LoginDomain::from_domain(
        raw,
        &erp_security::auth::LoginSelectionLimits::default(),
        w.h.orm().registry(),
    );
    assert!(
        result.is_err(),
        "huge equality operand must be rejected before SQL or copying"
    );
}

#[tokio::test]
async fn recovery_pure_selection_rejects_before_query_and_restores() {
    let w = selection_world().await;
    let mut tx = w.h.begin().await.unwrap();
    let before = tx.query_count();
    {
        let mut pure = tx.pure_methods("res.users", &["_get_login_domain", "_get_login_order"]);
        let error = pure.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(
            (error.kind(), error.message()),
            (
                ErrorKind::Programming,
                "ForbiddenEffect: pure method scope refuses database, record, transaction or environment effects"
            )
        );
        assert_eq!(pure.query_count(), before);
        assert!(
            pure.finish().is_err(),
            "swallowing an effect error must remain sticky"
        );
    }
    tx.query("SELECT 1", &[]).await.unwrap();
    assert_eq!(tx.query_count(), before + 1);
    tx.rollback().await.unwrap();
}

static LD1_CALLBACK_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[tokio::test]
async fn recovery_pure_selection_refuses_selection_callbacks_before_running() {
    use std::sync::atomic::Ordering;
    const REFUSED: &str = "ForbiddenEffect: pure method scope refuses database, record, transaction or environment effects";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_login_selection::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .selection_callback(erp_orm::SelectionCallback::new(
        "ld1_cb",
        |_tx, _model, _ids| {
            Box::pin(async move {
                LD1_CALLBACK_RUNS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        },
    ))
    .build()
    .await
    .expect("harness");
    let w = selection_fixture(h).await;
    let ids = [erp_fields::RecId::new_unique(1).unwrap()];

    for in_company in [false, true] {
        let mut tx = w.h.begin().await.unwrap();
        let env_before = tx.env();
        {
            let mut pure = tx.pure_methods("res.users", &["_get_login_domain"]);
            let error = if in_company {
                pure.call_selection_callback_in_company("ld1_cb", "res.users", &ids, Some(1))
                    .await
                    .unwrap_err()
            } else {
                pure.call_selection_callback("ld1_cb", "res.users", &ids)
                    .await
                    .unwrap_err()
            };
            assert_eq!(
                (error.kind(), error.message()),
                (ErrorKind::Programming, REFUSED)
            );
            let sticky = pure.finish().unwrap_err();
            assert_eq!(
                (sticky.kind(), sticky.message()),
                (ErrorKind::Programming, REFUSED)
            );
        }
        assert!(
            tx.env() == env_before,
            "refused call must not change the environment"
        );
        tx.rollback().await.unwrap();
    }
    assert_eq!(
        LD1_CALLBACK_RUNS.load(Ordering::SeqCst),
        0,
        "the callback body must never run inside the pure scope"
    );
}

/// `Denied`, for canonical and alias inputs through the selection-overridden path, LD2)
#[tokio::test]
async fn recovery_failure_accounting_store_error_is_not_a_denied_verdict() {
    let w = selection_world().await;
    user_with_alias_and_priority(&w.h, "ld2_canonical", "pw", "ld2_alias", 1).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.batch(
        "CREATE FUNCTION ld2_refuse_counter() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         RAISE EXCEPTION 'ld2 failure counter unavailable' USING ERRCODE='P0001'; END $$; \
         CREATE TRIGGER ld2_refuse_counter BEFORE INSERT OR UPDATE ON erp_login_throttle \
         FOR EACH ROW EXECUTE FUNCTION ld2_refuse_counter()",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    for submitted in ["ld2_canonical", "boolfalse:ld2_alias"] {
        let before = w.auth.stats().failure_write_errors;
        match login(&w.auth, submitted, "wrong").await {
            Err(LoginError::Failed(e)) => {
                assert_eq!(
                    (e.kind(), e.message(), e.sqlstate()),
                    (
                        ErrorKind::Database,
                        "ld2 failure counter unavailable",
                        Some("P0001")
                    ),
                    "{submitted}"
                );
            }
            other => panic!("{submitted}: expected LoginError::Failed, got {other:?}"),
        }
        assert_eq!(
            w.auth.stats().failure_write_errors,
            before + 1,
            "{submitted}"
        );
    }

    let mut tx = w.h.begin().await.unwrap();
    tx.batch("DROP TRIGGER ld2_refuse_counter ON erp_login_throttle")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        count(&w.h, "erp_login_throttle").await,
        0,
        "a refused write must leave no partial address/login/principal accounting"
    );
    assert!(matches!(
        login(&w.auth, "ld2_canonical", "wrong").await,
        Err(LoginError::Denied)
    ));
}

#[tokio::test]
async fn recovery_boolean_false_matches_false_not_just_null() {
    let w = selection_world().await;
    let uid = user_with_alias_and_priority(&w.h, "false_canonical", "pw", "false_alias", 1).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET selection_flag = false WHERE id = $1",
        &[&uid],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let logged = login(&w.auth, "boolfalse:false_alias", "pw").await.unwrap();
    assert_eq!(logged.auth().uid().get(), uid);
}

#[tokio::test]
async fn recovery_gc_expires_principal_counter_and_trust() {
    let w = selection_world().await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute("INSERT INTO erp_login_throttle(kind,key_hash,failures,last_failure_at) VALUES ('principal',decode(repeat('ab',32),'hex'),1,now()-interval '100 days'),('principal_trusted',decode(repeat('cd',32),'hex'),1,now()-interval '100 days')", &[]).await.unwrap();
    tx.commit().await.unwrap();
    w.auth.gc().await.unwrap();
    assert_eq!(
        count(
            &w.h,
            "erp_login_throttle WHERE kind IN ('principal','principal_trusted')"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn recovery_legacy_throttle_schema_converges() {
    let w = selection_world().await;
    let mut tx = w.h.begin().await.unwrap();
    tx.batch("ALTER TABLE erp_login_throttle DROP CONSTRAINT erp_login_throttle_trusted_once; ALTER TABLE erp_login_throttle ADD CONSTRAINT erp_login_throttle_trusted_once CHECK(kind <> 'trusted' OR failures=1); DROP INDEX erp_login_throttle_counted_age; CREATE INDEX erp_login_throttle_counted_age ON erp_login_throttle(last_failure_at) WHERE kind IN ('address','login'); DROP INDEX erp_login_throttle_trusted_age; CREATE INDEX erp_login_throttle_trusted_age ON erp_login_throttle(last_failure_at) WHERE kind='trusted'").await.unwrap();
    tx.batch(erp_security::auth::schema::AUTH_DDL)
        .await
        .unwrap();
    let rows = tx.query("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='erp_login_throttle'::regclass AND conname='erp_login_throttle_trusted_once'", &[]).await.unwrap();
    assert!(rows[0].get::<_, String>(0).contains("principal_trusted"));
    let rows = tx.query("SELECT indexdef FROM pg_indexes WHERE tablename='erp_login_throttle' AND indexname IN ('erp_login_throttle_counted_age','erp_login_throttle_trusted_age') ORDER BY indexname", &[]).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].get::<_, String>(0).contains("principal"));
    assert!(rows[1].get::<_, String>(0).contains("principal_trusted"));
    tx.rollback().await.unwrap();
}

#[test]
fn recovery_witness_debug_redacts_credentials_and_selectors() {
    let witness = erp_security::auth::SelectionWitness {
        uid: erp_orm::UserId::new(10).unwrap(),
        canonical_login: "private-login".into(),
        active: true,
        stored_hash: Some("private-password-hash".into()),
        dependency_values: vec![Some("private-selector".into())],
        plan_digest: [0; 32],
    };
    let debug = format!("{witness:?}");
    for secret in ["private-login", "private-password-hash", "private-selector"] {
        assert!(
            !debug.contains(secret),
            "witness debug exposes sensitive material"
        );
    }
}

/// while the password was verified)
#[tokio::test]
async fn recovery_selection_login_rename_during_hash_writes_no_session() {
    let w = selection_world().await;
    let uid = user_with_alias_and_priority(&w.h, "rename_me", "pw", "unused_alias", 1).await;
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let task_auth = std::sync::Arc::clone(&auth);
    let pending = tokio::spawn(async move { login(&task_auth, "rename_me", "pw").await });
    until_waiting(w.pool, 1).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET login='renamed_user' WHERE id=$1",
        &[&uid],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(release);
    assert!(
        matches!(pending.await.unwrap(), Err(LoginError::Denied)),
        "a login renamed during the verify must not become a session for the old name"
    );
    assert_eq!(
        count(&w.h, &format!("erp_session WHERE user_id = {uid}")).await,
        0
    );
}

/// Control for the rename test: an unrelated column changing during the verify still succeeds.
#[tokio::test]
async fn recovery_selection_login_unrelated_update_during_hash_still_succeeds() {
    let w = selection_world().await;
    let uid = user_with_alias_and_priority(&w.h, "keep_me", "pw", "unused_alias2", 1).await;
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let task_auth = std::sync::Arc::clone(&auth);
    let pending = tokio::spawn(async move { login(&task_auth, "keep_me", "pw").await });
    until_waiting(w.pool, 1).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute("UPDATE res_users SET tz='UTC' WHERE id=$1", &[&uid])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(release);
    let logged = pending.await.unwrap().unwrap();
    assert_eq!(logged.auth().uid().get(), uid);
}

#[tokio::test]
async fn recovery_competitor_reorder_during_hash_restarts_to_new_winner() {
    let w = selection_world().await;
    user_with_alias_and_priority(&w.h, "old_winner", "pw", "race_alias", 10).await;
    let new_uid = user_with_alias_and_priority(&w.h, "new_winner", "pw", "race_alias", 5).await;
    let release = hold_permits(w.pool, w.pool.config().permits()).await;
    let auth = std::sync::Arc::new(w.auth);
    let task_auth = std::sync::Arc::clone(&auth);
    let pending = tokio::spawn(async move { login(&task_auth, "alias:race_alias", "pw").await });
    until_waiting(w.pool, 1).await;
    let mut tx = w.h.begin().await.unwrap();
    tx.execute("UPDATE res_users SET priority=20 WHERE id=$1", &[&new_uid])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(release);
    let logged = pending.await.unwrap().unwrap();
    assert_eq!(logged.auth().uid().get(), new_uid);
    assert_eq!(auth.stats().login_restarts, 1);
    assert_eq!(
        check_user(&auth, logged.session().secret()).await.unwrap(),
        new_uid
    );
}

#[tokio::test]
async fn unbound_serving_constructor_refuses_selection_overrides() {
    use erp_test_models::test_login_selection::{domain_only, order_only};
    for addon in [
        &domain_only::ADDON,
        &order_only::ADDON,
        &erp_test_models::test_login_selection::ADDON,
    ] {
        let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON, addon])
            .with_decls([erp_data::ir_model_data_decl()])
            .require_core_tables(erp_security::auth::core_tables())
            .build()
            .await
            .expect("harness");
        prepare(&h).await;
        let pool = own_pool(HashPoolConfig::DEFAULT);
        match Auth::new(h.orm().clone(), AuthConfig::DEFAULT, pool).await {
            Err(error) => assert_eq!(
                (error.kind(), error.message()),
                (
                    ErrorKind::Programming,
                    "custom login selection requires an active membership-bound rollout"
                ),
            ),
            Ok(_) => panic!(
                "serving constructor enabled custom login selection without rollout admission"
            ),
        }
    }
}
