//! Explicit standalone selection never duplicates or replaces the real base currency root.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "support/currency_fixture.rs"]
mod currency_fixture;

use erp_registry::{MODEL_DECLS, RegistryBuilder};
use erp_test_models::{base_users, test_impex, test_orm};

#[test]
fn linked_base_keeps_its_own_currency_without_a_stub() {
    let registry =
        RegistryBuilder::new([&erp_addon_base::ADDON, &test_orm::ADDON, &test_impex::ADDON])
            .with_decls(crate::currency_fixture::decls())
            .build()
            .unwrap();
    assert!(crate::currency_fixture::has_currency_root(
        MODEL_DECLS.iter()
    ));
    assert_eq!(crate::currency_fixture::decls().count(), 0);
    assert!(
        registry
            .model("res.currency")
            .unwrap()
            .field("rounding")
            .is_some()
    );
    assert!(registry.model("ir.default").is_some());
}

#[test]
fn filtered_declarations_decide_from_the_actual_selected_root() {
    // The binary links the base addon, while this registry deliberately excludes its declarations.
    let _linked = &erp_addon_base::ADDON;
    assert!(crate::currency_fixture::has_currency_root(
        MODEL_DECLS.iter()
    ));
    let selected: Vec<_> = MODEL_DECLS
        .iter()
        .filter(|d| ["test_orm", "test_import_export", "base_users"].contains(&d.addon))
        .collect();
    let has_root = crate::currency_fixture::has_currency_root(selected.iter().copied());
    assert!(!has_root);
    let registry = RegistryBuilder::new([&base_users::ADDON, &test_orm::ADDON, &test_impex::ADDON])
        .linked(false)
        .with_decls(selected)
        .with_decls(crate::currency_fixture::for_selected(has_root))
        .build()
        .unwrap();
    assert!(
        registry
            .model("res.currency")
            .unwrap()
            .field("rounding")
            .is_none()
    );
    assert!(registry.model("ir.default").is_none());
}

#[test]
fn duplicate_currency_roots_are_still_rejected() {
    let stub = crate::currency_fixture::for_selected(false).next().unwrap();
    let error = RegistryBuilder::new([])
        .linked(false)
        .with_decls([stub, stub])
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        error.message(),
        format!(
            "{} defines model 'res.currency', which already exists; extend it with inherit = ['res.currency']",
            stub.describe()
        )
    );
}
