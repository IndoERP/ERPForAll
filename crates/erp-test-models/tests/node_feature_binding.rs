//! Membership-derived feature bindings: detached seams never grant database authority.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;
use std::time::Duration;

use erp_fields::{Error, ErrorKind};
use erp_orm::node::{
    AddonContent, AddonsDigest, BuildIdentity, FeatureBinding, FeatureKey, FeatureRollout,
    FeatureSet, FeatureVersion, NodeAdmission, NodeIdentity, NodeMembership, NodeOptions, NodeRole,
};
use erp_testkit::Harness;

const KEY: &str = "auth.login_principal";
static AVAILABLE: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "binding_catalogue_available",
    depends: &[],
    exact_only: false,
};

fn key() -> FeatureKey {
    FeatureKey::new(KEY).unwrap()
}
fn version() -> FeatureVersion {
    FeatureVersion::new(1).unwrap()
}

fn assert_programming(error: Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), message);
}

struct Fixture {
    h: Harness,
    member: NodeMembership,
    maintenance: String,
    request: String,
}

impl Fixture {
    async fn new() -> Self {
        let h = Harness::builder([&erp_test_models::base_users::ADDON])
            .build()
            .await
            .unwrap();
        let options = NodeOptions::builder()
            .heartbeat_interval_secs(60)
            .build()
            .unwrap();
        let pool = erp_orm::node::heartbeat_pool(h.db().config(), &options).unwrap();
        FeatureRollout::new(key(), version())
            .register(&pool)
            .await
            .unwrap();
        let mut addons = AddonsDigest::builder();
        for addon in h.registry().addons() {
            addons = addons
                .addon(addon.name, "fixture-1", AddonContent::Embedded)
                .unwrap();
        }
        // An available embedded addon need not be installed in the initial registry.
        addons = addons
            .addon(AVAILABLE.name, "fixture-1", AddonContent::Embedded)
            .unwrap();
        let identity = NodeIdentity::builder(
            NodeRole::App,
            "binding-node",
            "host",
            BuildIdentity::__from_build(
                "erp-test-binding",
                "1",
                false,
                Some("0123456789abcdef0123456789abcdef01234567"),
                Some("0"),
            ),
            addons.build().unwrap(),
        )
        .options(options)
        .features(FeatureSet::new([(key(), version())]).unwrap())
        .create(&pool)
        .await
        .unwrap();
        let member = NodeMembership::register(Arc::new(identity), pool)
            .await
            .unwrap();
        let maintenance = format!("bm_{}", h.db().name());
        let request = format!("br_{}", h.db().name());
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
        Self {
            h,
            member,
            maintenance,
            request,
        }
    }

    async fn binding(&self) -> FeatureBinding {
        FeatureBinding::new(self.member.admission(), self.h.orm(), key(), version())
            .await
            .unwrap()
    }

    async fn request_tx(&self) -> erp_orm::Transaction {
        let mut tx = self.h.begin().await.unwrap();
        tx.batch(&format!("SET LOCAL ROLE {}", erp_sql::quote(&self.request)))
            .await
            .unwrap();
        tx
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

#[tokio::test]
async fn registered_starting_and_ready_membership_bind_without_detached_authority() {
    let fixture = Fixture::new().await;
    assert!(!fixture.member.is_serving());
    let binding = fixture.binding().await;
    let mut tx = fixture.request_tx().await;
    let starting = binding.snapshot_in(&mut tx).await.unwrap();
    assert!(!starting.enabled());
    assert_eq!(starting.generation(), 0);
    assert!(!starting.is_serving());
    tx.rollback().await.unwrap();
    fixture.member.request_ready().await.unwrap();
    assert!(fixture.member.is_serving());
    let mut tx = fixture.request_tx().await;
    let ready = binding.snapshot_in(&mut tx).await.unwrap();
    assert!(!ready.enabled());
    assert_eq!(ready.generation(), 0);
    assert!(ready.is_serving());
    binding.protect_in(&mut tx, &ready).await.unwrap();
    tx.commit().await.unwrap();
    fixture.cleanup().await;
}

#[tokio::test]
async fn detached_clone_of_registered_identity_is_rejected() {
    let fixture = Fixture::new().await;
    let detached = NodeAdmission::detached_for_benches(
        Arc::clone(fixture.member.identity()),
        fixture.h.db().name(),
        Duration::from_secs(60),
    );
    assert!(detached.is_serving());
    let error = FeatureBinding::new(detached, fixture.h.orm(), key(), version())
        .await
        .unwrap_err();
    assert_programming(error, "feature binding requires registered node admission");
    fixture.cleanup().await;
}

#[tokio::test]
async fn binding_factory_rejects_the_wrong_database() {
    let fixture = Fixture::new().await;
    let other = Fixture::new().await;
    let error = FeatureBinding::new(fixture.member.admission(), other.h.orm(), key(), version())
        .await
        .unwrap_err();
    assert_programming(
        error,
        "feature binding database does not match node membership",
    );
    fixture.cleanup().await;
    other.cleanup().await;
}

#[tokio::test]
async fn binding_requires_the_exact_binary_declaration() {
    let fixture = Fixture::new().await;
    for (required_key, required_version) in [
        (FeatureKey::new("other.feature").unwrap(), version()),
        (key(), FeatureVersion::new(2).unwrap()),
    ] {
        let error = FeatureBinding::new(
            fixture.member.admission(),
            fixture.h.orm(),
            required_key,
            required_version,
        )
        .await
        .unwrap_err();
        assert_programming(
            error,
            "feature binding declaration does not match required version",
        );
    }
    fixture.cleanup().await;
}

#[tokio::test]
async fn binding_is_scoped_to_its_registry() {
    let fixture = Fixture::new().await;
    fixture.member.request_ready().await.unwrap();
    let binding = fixture.binding().await;
    let other_registry = Harness::builder([&erp_test_models::base_users::ADDON])
        .registry_only()
        .unwrap();
    assert!(!Arc::ptr_eq(fixture.h.registry(), &other_registry));
    // Same physical database and ORM lineage; only the published registry differs.
    let other_orm = fixture.h.orm().rebuild(other_registry).build().unwrap();
    let mut other_tx = other_orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let error = binding.snapshot_in(&mut other_tx).await.unwrap_err();
    assert_programming(
        error,
        "feature binding transaction does not match its registry",
    );
    let mut own_tx = fixture.request_tx().await;
    let snapshot = binding.snapshot_in(&mut own_tx).await.unwrap();
    own_tx.rollback().await.unwrap();
    let error = binding
        .protect_in(&mut other_tx, &snapshot)
        .await
        .unwrap_err();
    assert_programming(
        error,
        "feature binding transaction does not match its registry",
    );
    assert_eq!(
        other_tx.query("SELECT 1::int4", &[]).await.unwrap()[0].get::<_, i32>(0),
        1
    );
    other_tx.rollback().await.unwrap();
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_snapshot_cannot_authorize_another_binding() {
    let fixture = Fixture::new().await;
    fixture.member.request_ready().await.unwrap();
    let first = fixture.binding().await;
    let second = fixture.binding().await;
    let mut tx = fixture.request_tx().await;
    let snapshot = first.snapshot_in(&mut tx).await.unwrap();
    let error = second.protect_in(&mut tx, &snapshot).await.unwrap_err();
    assert_programming(error, "feature snapshot does not belong to this binding");
    // Rejection occurred before a database fault and did not poison the valid caller transaction.
    first.protect_in(&mut tx, &snapshot).await.unwrap();
    tx.rollback().await.unwrap();
    fixture.cleanup().await;
}

#[tokio::test]
async fn protected_generation_change_preserves_the_database_policy_source() {
    use std::error::Error as _;

    let fixture = Fixture::new().await;
    fixture.member.request_ready().await.unwrap();
    let binding = fixture.binding().await;
    let mut earlier = fixture.request_tx().await;
    let snapshot = binding.snapshot_in(&mut earlier).await.unwrap();
    earlier.rollback().await.unwrap();
    // Trusted fixture transition only; no claim of operator enrollment or real activation.
    let mut admin = fixture.h.begin().await.unwrap();
    admin
        .execute(
            "UPDATE public.erp_node_feature SET generation = generation + 1 WHERE key = $1",
            &[&KEY],
        )
        .await
        .unwrap();
    admin.commit().await.unwrap();
    let mut tx = fixture.request_tx().await;
    let error = binding.protect_in(&mut tx, &snapshot).await.unwrap_err();
    assert_programming_ref(&error, "login selection rollout generation changed");
    assert_eq!(error.sqlstate(), Some("ERF01"));
    assert!(error.source().is_some());
    tx.rollback().await.unwrap();
    fixture.cleanup().await;
}

fn assert_programming_ref(error: &Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn binding_factory_rejects_a_login_helper_owner() {
    let fixture = Fixture::new().await;
    let client = fixture.h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "ALTER ROLE {} LOGIN",
            erp_sql::quote(&fixture.maintenance)
        ))
        .await
        .unwrap();
    let result = FeatureBinding::new(
        fixture.member.admission(),
        fixture.h.orm(),
        key(),
        version(),
    )
    .await;
    client
        .batch_execute(&format!(
            "ALTER ROLE {} NOLOGIN",
            erp_sql::quote(&fixture.maintenance)
        ))
        .await
        .unwrap();
    drop(client);
    fixture.cleanup().await;
    assert_programming(
        result.unwrap_err(),
        "node feature helper public.erp_node_feature_snapshot(text) is not safely published",
    );
}
#[tokio::test]
async fn binding_factory_rejects_an_update_only_membership_trigger() {
    let fixture = Fixture::new().await;
    let client = fixture.h.db().client().await.unwrap();
    client.batch_execute("DROP TRIGGER erp_node_feature_guard ON erp_node;CREATE TRIGGER erp_node_feature_guard BEFORE UPDATE ON erp_node FOR EACH ROW EXECUTE FUNCTION public.erp_node_feature_guard();ALTER TABLE erp_node ENABLE ALWAYS TRIGGER erp_node_feature_guard").await.unwrap();
    let result = FeatureBinding::new(
        fixture.member.admission(),
        fixture.h.orm(),
        key(),
        version(),
    )
    .await;
    drop(client);
    fixture.cleanup().await;
    assert_programming(
        result.unwrap_err(),
        "node feature membership trigger differs from this binary",
    );
}

#[tokio::test]
async fn binding_debug_is_bounded_to_identity_metadata() {
    let fixture = Fixture::new().await;
    let binding = fixture.binding().await;
    let diagnostic = format!("{binding:?}");
    fixture.cleanup().await;
    assert!(
        diagnostic.len() <= 600,
        "binding diagnostic exceeded its identity bound"
    );
    assert!(!diagnostic.contains("Registry {"));
}

#[tokio::test]
async fn caller_search_path_cannot_redirect_membership_helpers() {
    let fixture = Fixture::new().await;
    fixture.member.request_ready().await.unwrap();
    let binding = fixture.binding().await;
    let client = fixture.h.db().client().await.unwrap();
    let schema = format!("shadow_{}", fixture.h.db().name());
    let quoted = erp_sql::quote(&schema);
    client.batch_execute(&format!("CREATE SCHEMA {quoted};GRANT USAGE ON SCHEMA {quoted} TO {};CREATE TABLE {quoted}.erp_node_feature(generation bigint);INSERT INTO {quoted}.erp_node_feature VALUES(999);CREATE FUNCTION {quoted}.erp_node_feature_member(text,integer,uuid,bytea) RETURNS TABLE(required_version integer,enabled boolean,generation bigint,node_serving boolean) LANGUAGE sql AS 'SELECT 1,true,999::bigint,true'",erp_sql::quote(&fixture.request))).await.unwrap();
    let mut tx = fixture.request_tx().await;
    tx.batch(&format!("SET LOCAL search_path={quoted},pg_catalog"))
        .await
        .unwrap();
    let snapshot = binding.snapshot_in(&mut tx).await.unwrap();
    assert!(!snapshot.enabled());
    assert_eq!(snapshot.generation(), 0);
    binding.protect_in(&mut tx, &snapshot).await.unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    client
        .batch_execute(&format!("DROP SCHEMA {quoted} CASCADE"))
        .await
        .unwrap();
    drop(client);
    fixture.cleanup().await;
}

#[tokio::test]
async fn new_binding_cannot_adopt_an_addon_outside_the_registered_catalogue() {
    let fixture = Fixture::new().await;
    let extra = Box::leak(Box::new(erp_registry::AddonDescriptor {
        name: "binding_catalogue_extra",
        depends: &[],
        exact_only: false,
    }));
    let other_registry = Harness::builder([&erp_test_models::base_users::ADDON, extra])
        .registry_only()
        .unwrap();
    let other_orm = fixture.h.orm().rebuild(other_registry).build().unwrap();
    let result =
        FeatureBinding::new(fixture.member.admission(), &other_orm, key(), version()).await;
    fixture.cleanup().await;
    match result {
        Err(error) => assert_programming(
            error,
            "feature binding registry addon is absent from node catalogue",
        ),
        Ok(_) => panic!("new binding accepted an addon absent from its registered catalogue"),
    }
}

#[tokio::test]
async fn new_binding_accepts_a_registry_rebuild_within_the_same_catalogue() {
    let fixture = Fixture::new().await;
    let registry = Harness::builder([&erp_test_models::base_users::ADDON, &AVAILABLE])
        .registry_only()
        .unwrap();
    assert!(!Arc::ptr_eq(fixture.h.registry(), &registry));
    assert!(
        registry
            .addons()
            .iter()
            .any(|addon| addon.name == AVAILABLE.name)
    );
    let orm = fixture.h.orm().rebuild(registry).build().unwrap();
    let binding = FeatureBinding::new(fixture.member.admission(), &orm, key(), version())
        .await
        .unwrap();
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert!(!binding.snapshot_in(&mut tx).await.unwrap().is_serving());
    tx.rollback().await.unwrap();
    drop(tx);
    fixture.cleanup().await;
}
