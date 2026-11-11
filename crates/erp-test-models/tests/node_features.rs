//! Pure declaration and digest contracts for. Detached identities here never grant admission.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::ErrorKind;
use erp_orm::node::{
    AddonsDigest, BuildIdentity, FeatureKey, FeatureSet, FeatureVersion, NodeError, NodeIdentity,
    NodeRole, SectionName, SettingsWriter, SharedSettings,
};

fn declarations(items: &[(&str, i32)]) -> FeatureSet {
    FeatureSet::new(items.iter().map(|(key, version)| {
        (
            FeatureKey::new(key).unwrap(),
            FeatureVersion::new(*version).unwrap(),
        )
    }))
    .unwrap()
}

fn assert_programming(error: NodeError, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.to_string(), message);
}

#[test]
fn feature_keys_validate_every_boundary_without_normalization() {
    for key in ["a", "auth.login_principal", "a0_.-", &"a".repeat(63)] {
        assert_eq!(FeatureKey::new(key).unwrap().as_str(), key);
    }
    for key in [
        "",
        "A",
        "1a",
        ".a",
        "_a",
        "-a",
        "a b",
        "a/b",
        "a\n",
        "a\r\n",
        "é",
        "aé",
        &"a".repeat(64),
    ] {
        assert_programming(
            FeatureKey::new(key).unwrap_err(),
            &format!("invalid feature key {key:?}: 1 to 63 ASCII bytes matching [a-z][a-z0-9_.-]*"),
        );
    }
}

#[test]
fn feature_versions_require_positive_int4() {
    for value in [1, i32::MAX] {
        assert_eq!(FeatureVersion::new(value).unwrap().get(), value);
    }
    for value in [i32::MIN, -1, 0] {
        assert_programming(
            FeatureVersion::new(value).unwrap_err(),
            &format!("invalid feature version {value}: expected a positive int4"),
        );
    }
}

#[test]
fn feature_sets_are_sorted_bounded_and_reject_duplicate_keys() {
    assert_eq!(FeatureSet::new([]).unwrap().iter().count(), 0);
    let eight: Vec<_> = (0..8)
        .map(|i| {
            (
                FeatureKey::new(&format!("feature{i}")).unwrap(),
                FeatureVersion::new(1).unwrap(),
            )
        })
        .collect();
    assert_eq!(FeatureSet::new(eight.clone()).unwrap().iter().count(), 8);
    let mut nine = eight;
    nine.push((
        FeatureKey::new("feature8").unwrap(),
        FeatureVersion::new(1).unwrap(),
    ));
    assert_programming(
        FeatureSet::new(nine).unwrap_err(),
        "a node identity declares at most 8 features",
    );
    for second in [1, 2] {
        assert_programming(
            FeatureSet::new([
                (
                    FeatureKey::new("auth.login_principal").unwrap(),
                    FeatureVersion::new(1).unwrap(),
                ),
                (
                    FeatureKey::new("auth.login_principal").unwrap(),
                    FeatureVersion::new(second).unwrap(),
                ),
            ])
            .unwrap_err(),
            "setting node_features.auth.login_principal is described twice in section node_features",
        );
    }
    let set = declarations(&[("z", 2), ("a", 1)]);
    assert_eq!(
        set.iter()
            .map(|(key, version)| (key.as_str(), version.get()))
            .collect::<Vec<_>>(),
        vec![("a", 1), ("z", 2)]
    );
}

fn builder() -> erp_orm::node::NodeIdentityBuilder {
    let build = BuildIdentity::__from_build(
        "erp-test-node",
        "1.0.0",
        false,
        Some("0123456789abcdef0123456789abcdef01234567"),
        Some("0"),
    );
    NodeIdentity::builder(
        NodeRole::App,
        "app-a",
        "host-1",
        build,
        AddonsDigest::builder().build().unwrap(),
    )
}

async fn identity(set: FeatureSet) -> NodeIdentity {
    builder()
        .features(set)
        .create_detached_for_benches()
        .await
        .unwrap()
}

#[tokio::test]
async fn feature_declarations_bind_identity_independently_of_input_order() {
    let default = builder().create_detached_for_benches().await.unwrap();
    let empty = identity(declarations(&[])).await;
    assert_eq!(default.features().iter().count(), 0);
    assert_eq!(default.digest(), empty.digest());
    let one = identity(declarations(&[("auth.login_principal", 1)])).await;
    assert_ne!(empty.digest(), one.digest());
    assert_ne!(
        one.digest(),
        identity(declarations(&[("other", 1)])).await.digest()
    );
    assert_ne!(
        one.digest(),
        identity(declarations(&[("auth.login_principal", 2)]))
            .await
            .digest()
    );
    let ordered = identity(declarations(&[("a", 1), ("z", 2)])).await;
    let reversed = identity(declarations(&[("z", 2), ("a", 1)])).await;
    assert_eq!(ordered.digest(), reversed.digest());
    assert_eq!(ordered.features(), &declarations(&[("z", 2), ("a", 1)]));
}

struct Reserved;
impl SharedSettings for Reserved {
    fn section(&self) -> SectionName {
        SectionName::new("node_features")
    }
    fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
        out.shared_u64("node_features.auth.login_principal", 1)
    }
}

#[tokio::test]
async fn arbitrary_settings_cannot_replace_the_reserved_feature_section() {
    let error = builder()
        .settings(&Reserved)
        .unwrap()
        .create_detached_for_benches()
        .await
        .unwrap_err();
    assert_programming(error, "settings section node_features is described twice");
}

#[test]
fn rejected_oversized_feature_key_diagnostic_is_bounded() {
    for key in ["a".repeat(1024 * 1024), "é".repeat(512 * 1024)] {
        let error = FeatureKey::new(&key).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        let message = error.to_string();
        assert!(
            message.len() <= 256,
            "diagnostic copied {} bytes of rejected input",
            message.len()
        );
        assert_eq!(
            message,
            format!(
                "invalid feature key ({} bytes): 1 to 63 ASCII bytes matching [a-z][a-z0-9_.-]*",
                key.len()
            )
        );
    }
}

#[test]
fn feature_registration_lock_does_not_collide_with_other_subsystems() {
    let feature = erp_orm::node::feature::FEATURE_REGISTRY_LOCK;
    assert_ne!(feature, erp_addon_web::assets::store::ASSET_LOCK);
    assert_ne!(feature, erp_orm::node::READINESS_LOCK);
    assert_ne!(feature, erp_security::auth::gc::GC_LOCK);
}
