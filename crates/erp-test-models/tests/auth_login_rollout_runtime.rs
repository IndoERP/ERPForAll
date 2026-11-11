//! Served-constructor rollout integration with isolated test enrollment and real memberships.
//! These in-process fixtures are not deployment activation or external credential-fencing evidence.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;
use auth_support::*;
use std::sync::Arc;

use erp_fields::{ErrorKind, Value};
use erp_orm::node::{
    AddonContent, AddonsDigest, BuildIdentity, FeatureKey, FeatureRollout, FeatureSet,
    FeatureVersion, LegacyEnrollment, NodeIdentity, NodeMembership, NodeOptions, NodeRole,
};
use erp_security::auth::{Auth, AuthConfig, HashPool, HashPoolConfig, LoginError};
use erp_testkit::{Harness, assert_round_trips};

const KEY: &str = "auth.login_principal";

fn selection_builder() -> erp_testkit::HarnessBuilder {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_login_selection::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
}

struct Fixture {
    h: Harness,
    member: NodeMembership,
    pool: &'static HashPool,
    maintenance: String,
    request: String,
    operator: erp_testkit::deadpool_postgres::Pool,
}

async fn member(h: &Harness, name: &str) -> NodeMembership {
    // heartbeat_config uses interval / 4 for TCP keepalives; real Linux sockets
    // require that value to contain at least one whole second.
    let options = NodeOptions::builder()
        .heartbeat_interval_for_tests(std::time::Duration::from_secs(4))
        .lease_intervals(10)
        .build()
        .unwrap();
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &options).unwrap();
    FeatureRollout::new(
        FeatureKey::new(KEY).unwrap(),
        FeatureVersion::new(1).unwrap(),
    )
    .register(&pool)
    .await
    .unwrap();
    let mut addons = AddonsDigest::builder();
    for addon in h.registry().addons() {
        addons = addons
            .addon(addon.name, "fixture-1", AddonContent::Embedded)
            .unwrap();
    }
    let identity = NodeIdentity::builder(
        NodeRole::App,
        name,
        "host",
        BuildIdentity::__from_build(
            "erp-test-rollout-auth",
            "1",
            false,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("0"),
        ),
        addons.build().unwrap(),
    )
    .features(
        FeatureSet::new([(
            FeatureKey::new(KEY).unwrap(),
            FeatureVersion::new(1).unwrap(),
        )])
        .unwrap(),
    )
    .options(options)
    .create(&pool)
    .await
    .unwrap();
    NodeMembership::register(Arc::new(identity), pool)
        .await
        .unwrap()
}

async fn serve(member: &NodeMembership) {
    let mut changes = member.subscribe();
    member.request_ready().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !member.is_serving() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("membership did not reach actual serving admission");
    assert!(member.is_serving());
}

impl Fixture {
    async fn new(selection: bool) -> Self {
        let h = if selection {
            selection_builder()
        } else {
            builder()
        }
        .build()
        .await
        .unwrap();
        Self::from_h(h).await
    }

    async fn from_h(h: Harness) -> Self {
        prepare(&h).await;
        let member = member(&h, "auth-rollout").await;
        let maintenance = format!("am_{}", h.db().name());
        let request = format!("ar_{}", h.db().name());
        let mut tx = h.begin().await.unwrap();
        tx.batch(&format!(
            "CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN",
            erp_sql::quote(&maintenance),
            erp_sql::quote(&request)
        ))
        .await
        .unwrap();
        tx.grant_node_feature_roles(&maintenance, &request)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let operator =
            erp_orm::node::heartbeat_pool(h.db().config(), &NodeOptions::default()).unwrap();
        Self {
            h,
            member,
            pool: waiting_pool(1),
            maintenance,
            request,
            operator,
        }
    }

    async fn auth(&self) -> Auth {
        Auth::with_membership(
            self.h.orm().clone(),
            AuthConfig::DEFAULT,
            self.pool,
            self.member.admission(),
        )
        .await
        .unwrap()
    }

    async fn user(&self) -> i64 {
        let mut tx = self.h.begin().await.unwrap();
        let uid = create(
            &mut tx,
            "res.users",
            &[
                ("name", s("Canonical")),
                ("login", s("canonical")),
                ("password", s("correct")),
                ("alias", s("alias_name")),
                ("priority", Value::from(1)),
            ],
        )
        .await;
        tx.commit().await.unwrap();
        uid
    }

    /// Isolated test enrollment: these fixtures have no legacy standalone service credentials.
    async fn enable(&self, members: &[&NodeMembership]) {
        let feature = FeatureRollout::new(
            FeatureKey::new(KEY).unwrap(),
            FeatureVersion::new(1).unwrap(),
        );
        for membership in members {
            feature
                .approve(&self.operator, membership.identity().digest())
                .await
                .unwrap();
        }
        feature
            .enable(
                &self.operator,
                LegacyEnrollment::confirmed_drained_and_fenced(),
            )
            .await
            .unwrap();
    }

    async fn disable(&self) {
        let feature = FeatureRollout::new(
            FeatureKey::new(KEY).unwrap(),
            FeatureVersion::new(1).unwrap(),
        );
        // A queued verify must hold no feature protection. This is a bounded synchronization
        // check, not a product latency measurement or performance acceptance gate.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            feature.disable(&self.operator),
        )
        .await
        .expect("disable blocked behind verification waiting for a hashing permit")
        .unwrap();
    }

    async fn cleanup(self) {
        self.member.shutdown().await.unwrap();
        let mut tx = self.h.begin().await.unwrap();
        tx.batch(&format!(
            "DROP OWNED BY {}, {} CASCADE; DROP ROLE {}, {}",
            erp_sql::quote(&self.maintenance),
            erp_sql::quote(&self.request),
            erp_sql::quote(&self.maintenance),
            erp_sql::quote(&self.request)
        ))
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

fn programming(result: Result<erp_security::auth::Login, LoginError>, message: &str) {
    let error = match result {
        Err(LoginError::Failed(error)) => error,
        other => panic!("expected Failed Programming, got {other:?}"),
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), message);
}

fn denied(result: Result<erp_security::auth::Login, LoginError>) {
    let error: erp_fields::Error = result.unwrap_err().into();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::AccessDenied, "Access Denied")
    );
}

fn assert_selection(auth: &Auth, overridden: bool) {
    let id = auth.orm().registry().model_id("res.users").unwrap();
    assert_eq!(
        auth.orm()
            .dispatch()
            .is_base_method(id, "_get_login_domain"),
        !overridden
    );
    assert_eq!(
        auth.orm().dispatch().is_base_method(id, "_get_login_order"),
        !overridden
    );
    assert!(
        auth.orm()
            .dispatch()
            .is_base_method(id, "_check_credentials")
    );
}

#[tokio::test]
async fn bound_starting_construction_preserves_default_off_selection() {
    let fixture = Fixture::new(true).await;
    assert!(!fixture.member.is_serving());
    let auth = fixture.auth().await;
    assert_selection(&auth, true);
    serve(&fixture.member).await;
    assert!(fixture.member.is_serving());
    let jobs = fixture.pool.stats().jobs;
    // The fixture hook panics if reached, so disabled admission must precede hook dispatch too.
    programming(
        login(&auth, "panic_hook:probe", "password").await,
        "login selection rollout is disabled",
    );
    assert_eq!(fixture.pool.stats().jobs, jobs);
    assert_eq!(count(&fixture.h, "erp_session").await, 0);
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test]
async fn enabled_bound_alias_login_and_session_validation() {
    let fixture = Fixture::new(true).await;
    let auth = fixture.auth().await;
    let uid = fixture.user().await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let logged = login(&auth, "alias:alias_name", "correct").await.unwrap();
    assert_eq!(logged.uid().get(), uid);
    assert_eq!(
        check_user(&auth, logged.session().secret()).await.unwrap(),
        uid
    );
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test]
async fn drained_bound_login_refuses_before_hashing() {
    let fixture = Fixture::new(true).await;
    let auth = fixture.auth().await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    fixture.member.drain().await.unwrap();
    let jobs = fixture.pool.stats().jobs;
    programming(
        login(&auth, "panic_hook:probe", "correct").await,
        "login selection membership is not serving",
    );
    assert_eq!(fixture.pool.stats().jobs, jobs);
    assert_eq!(count(&fixture.h, "erp_session").await, 0);
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hash_waiting_attempt_cannot_succeed_after_generation_changes() {
    let fixture = Fixture::new(true).await;
    let auth = Arc::new(fixture.auth().await);
    fixture.user().await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let release = hold_permits(fixture.pool, 1).await;
    let task_auth = Arc::clone(&auth);
    let attempt =
        tokio::spawn(async move { login(&task_auth, "alias:alias_name", "correct").await });
    until_waiting(fixture.pool, 1).await;
    fixture.disable().await;
    fixture.enable(&[&fixture.member]).await;
    drop(release);
    programming(
        attempt.await.unwrap(),
        "login selection rollout generation changed",
    );
    assert_eq!(count(&fixture.h, "erp_session").await, 0);
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_accounting_survives_disabling_the_feature() {
    let fixture = Fixture::new(true).await;
    let auth = Arc::new(fixture.auth().await);
    fixture.user().await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let release = hold_permits(fixture.pool, 1).await;
    let task_auth = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login(&task_auth, "alias:alias_name", "wrong").await });
    until_waiting(fixture.pool, 1).await;
    fixture.disable().await;
    drop(release);
    denied(attempt.await.unwrap());
    assert_eq!(
        count(
            &fixture.h,
            "erp_login_throttle WHERE kind='principal' AND failures=1"
        )
        .await,
        1
    );
    assert_eq!(count(&fixture.h, "erp_session").await, 0);
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_accounting_survives_caller_abort_and_disable() {
    let fixture = Fixture::new(true).await;
    let auth = Arc::new(fixture.auth().await);
    fixture.user().await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let release = hold_permits(fixture.pool, 1).await;
    let task_auth = Arc::clone(&auth);
    let attempt = tokio::spawn(async move { login(&task_auth, "alias:alias_name", "wrong").await });
    until_waiting(fixture.pool, 1).await;
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    fixture.disable().await;
    drop(release);
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while count(
            &fixture.h,
            "erp_login_throttle WHERE kind='principal' AND failures=1",
        )
        .await
            != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached evaluated guess did not persist principal accounting");
    assert_eq!(count(&fixture.h, "erp_session").await, 0);
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test]
async fn base_peer_and_alias_peer_enforce_one_principal_budget() {
    let fixture = Fixture::new(true).await;
    let alias = fixture.auth().await;
    fixture.user().await;
    set_param(&fixture.h, "base.login_cooldown_after", "100").await;
    set_param(&fixture.h, "base.login_cooldown_after_login", "2").await;
    let base_registry = builder().registry_only().unwrap();
    let base_orm = fixture.h.orm().rebuild(base_registry).build().unwrap();
    let peer = member(&fixture.h, "base-peer").await;
    let base = Auth::with_membership(
        base_orm,
        AuthConfig::DEFAULT,
        fixture.pool,
        peer.admission(),
    )
    .await
    .unwrap();
    assert_selection(&alias, true);
    assert_selection(&base, false);
    tokio::join!(serve(&fixture.member), serve(&peer));
    fixture.enable(&[&fixture.member, &peer]).await;
    denied(login_from(&alias, "alias:alias_name", "wrong", "198.51.100.201").await);
    denied(login_from(&base, "canonical", "wrong", "198.51.100.202").await);
    assert_eq!(
        count(
            &fixture.h,
            "erp_login_throttle WHERE kind='principal' AND failures=2"
        )
        .await,
        1
    );
    let jobs = fixture.pool.stats().jobs;
    for (auth, submitted, addr) in [
        (&alias, "alias:alias_name", "198.51.100.203"),
        (&base, "canonical", "198.51.100.204"),
    ] {
        let error: erp_fields::Error = login_from(auth, submitted, "correct", addr)
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
    }
    assert_eq!(fixture.pool.stats().jobs, jobs);
    drop(base);
    drop(alias);
    peer.shutdown().await.unwrap();
    fixture.cleanup().await;
}

#[tokio::test]
async fn unbound_base_created_before_activation_cannot_bypass_principal_accounting() {
    let fixture = Fixture::new(false).await;
    let unbound = Auth::new(fixture.h.orm().clone(), AuthConfig::DEFAULT, fixture.pool)
        .await
        .unwrap();
    assert_selection(&unbound, false);
    user_with_password(&fixture.h, "canonical", "correct").await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let jobs = fixture.pool.stats().jobs;
    programming(
        login(&unbound, "canonical", "correct").await,
        "base login requires membership-bound principal accounting while login selection rollout is active",
    );
    assert_eq!(fixture.pool.stats().jobs, jobs);
    assert_eq!(count(&fixture.h, "erp_session").await, 0);
    drop(unbound);
    fixture.cleanup().await;
}

#[tokio::test]
async fn inactive_unbound_base_retains_its_semantic_flight_contract() {
    let w = world_with(
        builder().count_round_trips().build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::DEFAULT,
    )
    .await;
    assert_selection(&w.auth, false);
    user_with_password(&w.h, "canonical", "correct").await;
    login(&w.auth, "canonical", "correct").await.unwrap();
    denied(login(&w.auth, "missing", "wrong").await);
    let wire = w.h.wire().unwrap().clone();
    assert_round_trips!(wire, == 2, { login(&w.auth, "canonical", "correct").await.unwrap(); });
    assert_round_trips!(wire, == 2, { denied(login(&w.auth, "canonical", "wrong").await); });
}

#[tokio::test]
async fn bound_constructor_preserves_session_composition_refusal() {
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
    .unwrap();
    let fixture = Fixture::from_h(h).await;
    let error = Auth::with_membership(
        fixture.h.orm().clone(),
        AuthConfig::DEFAULT,
        fixture.pool,
        fixture.member.admission(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "custom login selection combined with credential override is unsupported until a later release"
        )
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn bound_identity_confirmation_refuses_after_disable_before_hashing() {
    let fixture = Fixture::new(true).await;
    let auth = fixture.auth().await;
    fixture.user().await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let logged = login(&auth, "alias:alias_name", "correct").await.unwrap();
    fixture.disable().await;
    let jobs = fixture.pool.stats().jobs;
    let error = auth
        .confirm_identity(logged.session().secret(), "correct", IN_PROCESS)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "login selection rollout is disabled"
        )
    );
    assert_eq!(fixture.pool.stats().jobs, jobs);
    assert_eq!(
        auth.check_identity(Some(logged.session().secret()))
            .await
            .unwrap(),
        erp_security::auth::IdentityCheck::Required
    );
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test]
async fn active_unbound_base_identity_confirmation_refuses_before_hashing() {
    let fixture = Fixture::new(false).await;
    let auth = Auth::new(fixture.h.orm().clone(), AuthConfig::DEFAULT, fixture.pool)
        .await
        .unwrap();
    assert_selection(&auth, false);
    user_with_password(&fixture.h, "canonical", "correct").await;
    let logged = login(&auth, "canonical", "correct").await.unwrap();
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let jobs = fixture.pool.stats().jobs;
    let error = auth
        .confirm_identity(logged.session().secret(), "correct", IN_PROCESS)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "base login requires membership-bound principal accounting while login selection rollout is active"
        )
    );
    assert_eq!(fixture.pool.stats().jobs, jobs);
    assert_eq!(
        auth.check_identity(Some(logged.session().secret()))
            .await
            .unwrap(),
        erp_security::auth::IdentityCheck::Required
    );
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_confirmation_cannot_publish_after_generation_changes() {
    let fixture = Fixture::new(true).await;
    let auth = Arc::new(fixture.auth().await);
    fixture.user().await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let logged = login(&auth, "alias:alias_name", "correct").await.unwrap();
    let secret = cookie(&logged);
    let release = hold_permits(fixture.pool, 1).await;
    let task_auth = Arc::clone(&auth);
    let attempt = tokio::spawn(async move {
        task_auth
            .confirm_identity(&secret, "correct", IN_PROCESS)
            .await
    });
    until_waiting(fixture.pool, 1).await;
    fixture.disable().await;
    fixture.enable(&[&fixture.member]).await;
    drop(release);
    let error = attempt.await.unwrap().unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "login selection rollout generation changed"
        )
    );
    assert_eq!(
        auth.check_identity(Some(logged.session().secret()))
            .await
            .unwrap(),
        erp_security::auth::IdentityCheck::Required
    );
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test]
async fn identity_confirmation_uses_the_same_principal_budget() {
    let fixture = Fixture::new(true).await;
    let auth = fixture.auth().await;
    fixture.user().await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let logged = login(&auth, "alias:alias_name", "correct").await.unwrap();
    set_param(&fixture.h, "base.login_cooldown_after", "100").await;
    set_param(&fixture.h, "base.login_cooldown_after_login", "2").await;
    let error = auth
        .confirm_identity(logged.session().secret(), "wrong", IN_PROCESS)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::AccessDenied, "Access Denied")
    );
    denied(login_from(&auth, "alias:alias_name", "wrong", "198.51.100.211").await);
    assert_eq!(
        count(
            &fixture.h,
            "erp_login_throttle WHERE kind='principal' AND failures=2"
        )
        .await,
        1
    );
    let jobs = fixture.pool.stats().jobs;
    let error = auth
        .confirm_identity(logged.session().secret(), "correct", IN_PROCESS)
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::AccessDenied,
            erp_security::auth::COOLDOWN_MESSAGE
        )
    );
    assert_eq!(fixture.pool.stats().jobs, jobs);
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test]
async fn bound_base_credential_override_requires_session_support() {
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
    let fixture = Fixture::from_h(h).await;
    let auth = fixture.auth().await;
    let id = auth.orm().registry().model_id("res.users").unwrap();
    assert!(
        auth.orm()
            .dispatch()
            .is_base_method(id, "_get_login_domain")
    );
    assert!(auth.orm().dispatch().is_base_method(id, "_get_login_order"));
    assert!(
        !auth
            .orm()
            .dispatch()
            .is_base_method(id, "_check_credentials")
    );
    user_with_password(&fixture.h, "canonical", "correct").await;
    serve(&fixture.member).await;
    let jobs = fixture.pool.stats().jobs;
    programming(
        login(&auth, "canonical", "correct").await,
        "membership-bound credential overrides are unsupported until a later release",
    );
    fixture.enable(&[&fixture.member]).await;
    programming(
        login(&auth, "canonical", "correct").await,
        "active login principal accounting combined with credential override is unsupported until a later release",
    );
    assert_eq!(fixture.pool.stats().jobs, jobs);
    assert_eq!(count(&fixture.h, "erp_session").await, 0);
    assert_eq!(
        count(
            &fixture.h,
            "pg_class WHERE relnamespace='public'::regnamespace AND relname='test_auth_check'"
        )
        .await,
        0
    );
    drop(auth);
    fixture.cleanup().await;
}

#[tokio::test]
async fn fresh_auth_cannot_bind_an_unregistered_selection_addon() {
    let fixture = Fixture::new(false).await;
    serve(&fixture.member).await;
    fixture.enable(&[&fixture.member]).await;
    let registry = selection_builder().registry_only().unwrap();
    let orm = fixture.h.orm().rebuild(registry).build().unwrap();
    let jobs = fixture.pool.stats().jobs;
    let result = Auth::with_membership(
        orm,
        AuthConfig::DEFAULT,
        fixture.pool,
        fixture.member.admission(),
    )
    .await;
    match result {
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "feature binding registry addon is absent from node catalogue"
            );
        }
        Ok(_) => {
            panic!("served Auth accepted a selection addon absent from its membership catalogue")
        }
    }
    assert_eq!(fixture.pool.stats().jobs, jobs);
    assert_eq!(count(&fixture.h, "erp_session").await, 0);
    fixture.cleanup().await;
}
