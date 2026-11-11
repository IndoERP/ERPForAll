//! Binary capability declarations must reach durable membership, not only its local digest.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_orm::node::{
    AddonsDigest, BuildIdentity, FeatureKey, FeatureSet, FeatureVersion, NodeIdentity,
    NodeMembership, NodeOptions, NodeRole,
};
use erp_testkit::Harness;
use std::sync::Arc;

#[tokio::test]
async fn membership_persists_the_declared_capability_map() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &NodeOptions::default()).unwrap();
    let build = BuildIdentity::__from_build(
        "erp-test-node",
        "1",
        false,
        Some("0123456789abcdef0123456789abcdef01234567"),
        Some("0"),
    );
    let identity = NodeIdentity::builder(
        NodeRole::App,
        "feature-node",
        "host",
        build,
        AddonsDigest::builder().build().unwrap(),
    )
    .features(
        FeatureSet::new([(
            FeatureKey::new("auth.login_principal").unwrap(),
            FeatureVersion::new(1).unwrap(),
        )])
        .unwrap(),
    )
    .create(&pool)
    .await
    .unwrap();
    let id = identity.id().to_string();
    let membership = NodeMembership::register(Arc::new(identity), pool)
        .await
        .unwrap();
    let client = h.db().client().await.unwrap();
    let row = client.query_one("SELECT (capabilities->>'auth.login_principal')::int4 FROM public.erp_node WHERE id=$1::text::uuid", &[&id]).await.unwrap();
    let persisted: Option<i32> = row.get(0);
    membership.shutdown().await.unwrap();
    assert_eq!(persisted, Some(1));
}

async fn renewal_fixture() -> (Harness, NodeMembership, String) {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let options = NodeOptions::builder()
        .heartbeat_interval_secs(60)
        .build()
        .unwrap();
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &options).unwrap();
    let build = BuildIdentity::__from_build(
        "erp-test-feature-renewal",
        "1",
        false,
        Some("0123456789abcdef0123456789abcdef01234567"),
        Some("0"),
    );
    let features = FeatureSet::new((0..8).map(|index| {
        (
            FeatureKey::new(&format!("feature{index}")).unwrap(),
            FeatureVersion::new(index + 1).unwrap(),
        )
    }))
    .unwrap();
    let identity = NodeIdentity::builder(
        NodeRole::App,
        "feature-renewal",
        "host",
        build,
        AddonsDigest::builder().build().unwrap(),
    )
    .options(options)
    .features(features)
    .create(&pool)
    .await
    .unwrap();
    let id = identity.id().to_string();
    let membership = NodeMembership::register(Arc::new(identity), pool)
        .await
        .unwrap();
    (h, membership, id)
}

async fn persisted_map(h: &Harness, id: &str) -> serde_json::Value {
    let client = h.db().client().await.unwrap();
    // Read as text: this integration binary need not add the driver's JSON decoding feature.
    let row = client
        .query_one(
            "SELECT capabilities::text FROM public.erp_node WHERE id = $1::text::uuid",
            &[&id],
        )
        .await
        .unwrap();
    serde_json::from_str(row.get::<_, &str>(0)).unwrap()
}

fn expected_eight_features() -> serde_json::Value {
    serde_json::json!({
        "feature0": 1, "feature1": 2, "feature2": 3, "feature3": 4,
        "feature4": 5, "feature5": 6, "feature6": 7, "feature7": 8,
    })
}

#[tokio::test]
async fn renewal_preserves_the_complete_capability_map() {
    let (h, membership, id) = renewal_fixture().await;
    let before = persisted_map(&h, &id).await;
    let previous_beat = membership.view().beat();
    let renewed = membership.renew_now_for_tests().await.unwrap();
    let after = persisted_map(&h, &id).await;
    membership.shutdown().await.unwrap();
    assert_eq!(before, expected_eight_features());
    assert_eq!(after, expected_eight_features());
    assert!(renewed.beat() > previous_beat);
}

#[tokio::test]
async fn renewal_reinsertion_restores_the_complete_capability_map() {
    let (h, membership, id) = renewal_fixture().await;
    let client = h.db().client().await.unwrap();
    let deleted = client
        .execute(
            "DELETE FROM public.erp_node WHERE id = $1::text::uuid",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    let missing = client
        .query_one(
            "SELECT count(*) FROM public.erp_node WHERE id = $1::text::uuid",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(missing.get::<_, i64>(0), 0);
    membership.renew_now_for_tests().await.unwrap();
    let after = persisted_map(&h, &id).await;
    membership.shutdown().await.unwrap();
    assert_eq!(after, expected_eight_features());
}
