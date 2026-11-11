//! Manifest policy admission must fail loudly before registry publication.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_modules::Manifest;

#[test]
fn manifest_accepts_exact_only_opt_in() {
    let manifest = Manifest::from_toml("ordinary", "exact_only = true\n").unwrap();
    assert_eq!(serde_json::to_value(manifest).unwrap()["exact_only"], true);
}

#[test]
fn mandatory_manifest_policy_defaults_to_enabled() {
    for name in [
        "account",
        "account_extra",
        "stock",
        "stock_extra",
        "product",
        "uom",
    ] {
        let manifest = Manifest::from_toml(name, "").unwrap();
        assert_eq!(serde_json::to_value(manifest).unwrap()["exact_only"], true);
        let error = Manifest::from_toml(name, "exact_only = false\n").unwrap_err();
        assert!(matches!(error, erp_modules::Error::InvalidManifest { .. }));
        assert_eq!(
            error.to_string(),
            format!(
                "Module {name}: invalid manifest: exact_only cannot be disabled for this addon"
            )
        );
    }
}

#[test]
fn package_policy_matches_the_manifest_and_rejects_wrong_types() {
    use erp_modules::wasm_lifecycle::models_from_describe;
    let manifest = Manifest::from_toml("ordinary", "exact_only = true\n").unwrap();
    let doc = |flag: serde_json::Value| {
        serde_json::json!({"schema": 1, "contributions": [
        {"addon": "ordinary", "kind": "addon", "model": "", "name": "ordinary", "detail": {"depends": ["base"], "exact_only": flag}}
    ]}).to_string()
    };
    assert!(
        models_from_describe(&manifest, &doc(true.into()))
            .unwrap()
            .is_empty()
    );
    for (flag, expected) in [
        (
            false.into(),
            "describe() and the manifest disagree on exact_only",
        ),
        ("true".into(), "describe() exact_only must be a boolean"),
    ] {
        let error = models_from_describe(&manifest, &doc(flag)).unwrap_err();
        assert!(matches!(error, erp_modules::Error::WasmAddon { .. }));
        assert_eq!(
            error.to_string(),
            format!(
                "WebAssembly addon 'ordinary': describe() disagrees with the manifest: {expected}"
            )
        );
    }
}

mod common;

fn registry_error(error: &erp_modules::Error) {
    let mut source: &(dyn std::error::Error + 'static) = error;
    loop {
        let wrapped = source
            .downcast_ref::<erp_modules::Error>()
            .and_then(|error| match error {
                erp_modules::Error::Orm(inner) => Some(inner),
                erp_modules::Error::Registry { source, .. } => Some(source),
                _ => None, // Other module wrappers expose their nested error through source().
            });
        if let Some(error) = source.downcast_ref::<erp_fields::Error>().or(wrapped) {
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "Exact-only addon 'ordinary' refuses binary-float column 'ordinary.item.qty' (protecting model definition 'ordinary.item' of addon 'ordinary' (models/item.toml:1); declaring model definition 'ordinary.item' of addon 'ordinary' (models/item.toml:1); physical column 'ordinary.item.qty'); declare numeric digits instead."
            );
            break;
        }
        source = source
            .source()
            .unwrap_or_else(|| panic!("expected registry admission error, got {error:?}"));
    }
}

#[tokio::test]
async fn failed_exact_install_and_policy_upgrade_preserve_usable_state() {
    use common::*;
    use erp_modules::system::InitOptions;
    let addons = Addons::new();
    addons.add(
        "ordinary",
        "name = 'Ordinary'\nversion = '1.0'\nexact_only = true\n",
    );
    addons.file(
        "ordinary",
        "models/item.toml",
        "[[model]]\nname = 'ordinary.item'\n[[model.field]]\nname = 'qty'\ntype = 'float'\n",
    );
    let db = erp_testkit::TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let initial = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap()
        .orm;
    let error = system
        .button_immediate_install(&su(&initial), &[n("ordinary")])
        .await
        .unwrap_err();
    registry_error(&error);
    assert_eq!(state(&initial, "ordinary").await, "uninstalled");
    assert!(!table_exists(&initial, "ordinary_item").await);
    addons.add(
        "ordinary",
        "name = 'Ordinary'\nversion = '1.0'\nexact_only = false\n",
    );
    // The failed first install has no enrolled identity; retry is a fresh install.
    committed(
        system
            .button_immediate_install(&su(&initial), &[n("ordinary")])
            .await
            .unwrap(),
    );
    let installed = system.load().await.unwrap().orm;
    assert!(installed.registry().model("ordinary.item").is_some());
    addons.add(
        "ordinary",
        "name = 'Ordinary'\nversion = '1.1'\nexact_only = true\n",
    );
    enroll_fixture_transition(&system, &["ordinary"]).await;
    let error = system
        .button_immediate_upgrade(&su(&installed), &[n("ordinary")])
        .await
        .unwrap_err();
    registry_error(&error);
    assert_eq!(state(&installed, "ordinary").await, "installed");
    let mut tx = installed.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ordinary.item")
        .unwrap()
        .create(
            &mut tx,
            vec![erp_orm::RawVals::new().set_cell("qty", erp_fields::Cell::Float(1.25))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(table_exists(&installed, "ordinary_item").await);
    db.cleanup().await.unwrap();
}

#[test]
fn legacy_describe_policy_absence_uses_the_mandatory_name_rule() {
    for name in ["ordinary", "uom", "stock_extra"] {
        let manifest = Manifest::from_toml(name, "").unwrap();
        let described = serde_json::json!({"schema": 1, "contributions": [
            {"addon": name, "kind": "addon", "model": "", "name": name, "detail": {"depends": ["base"]}}
        ]}).to_string();
        assert!(
            erp_modules::wasm_lifecycle::models_from_describe(&manifest, &described)
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn native_manifest_policy_mismatch_is_refused_at_build() {
    use common::*;
    static NATIVE: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
        name: "ordinary",
        depends: &["base"],
        exact_only: true,
    };
    let addons = Addons::new();
    addons.add("ordinary", "name = 'Ordinary'\nexact_only = false\n");
    let db = erp_testkit::TestDb::create().await.unwrap();
    let error = match builder(&db, &addons).code([&NATIVE]).build() {
        Ok(_) => panic!("mismatched native policy was admitted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        erp_modules::Error::AddonCodeMismatch { .. }
    ));
    assert_eq!(
        error.to_string(),
        "Module 'ordinary': its code does not match manifest.toml: the code and manifest.toml disagree on exact_only"
    );
    db.cleanup().await.unwrap();
}
