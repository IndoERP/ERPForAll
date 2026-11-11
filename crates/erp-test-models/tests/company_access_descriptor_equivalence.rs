//! Native and package description of one company-scoped fixture produce the same registry policy.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]

use erp_fields::serde_json;
use erp_modules::Manifest;
use erp_modules::wasm_lifecycle::models_from_describe;
use erp_registry::{CompanyAccessState, Overlay, RegistryBuilder};
use erp_test_models::test_company_emit;

#[test]
fn company_access_survives_the_package_descriptor() {
    let native = RegistryBuilder::new([&erp_addon_base::ADDON, &test_company_emit::ADDON])
        .build()
        .unwrap();
    let manifest = Manifest::from_toml(
        "test_company_emit",
        "name = 'Company access fixture'\ndepends = ['base']\n[wasm]\nsdk_version = '1.0'\n",
    )
    .unwrap();
    let mut described: serde_json::Value = serde_json::from_str(&erp_sdk::describe()).unwrap();
    let entries = described["contributions"].as_array_mut().unwrap();
    entries.retain(|entry| entry["addon"] == "test_company_emit");
    for entry in entries {
        if let Some(args) = entry["detail"]["args"].as_object_mut() {
            args.remove("addon");
        }
    }
    let defs = models_from_describe(&manifest, &described.to_string()).unwrap();
    let mut overlay = Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let descriptor = erp_modules::model_files::addon_descriptor(&manifest).unwrap();
    let base_decls = erp_registry::MODEL_DECLS
        .iter()
        .filter(|decl| decl.addon == "base");
    let packaged = RegistryBuilder::new([&erp_addon_base::ADDON, descriptor])
        .linked(false)
        .with_decls(base_decls)
        .overlay(&overlay)
        .build()
        .unwrap();
    for model in ["test_company_emit.doc", "test_company_emit.line"] {
        let CompanyAccessState::Declared(native_access) =
            native.model(model).unwrap().company_access
        else {
            panic!("native {model} has no declaration");
        };
        let CompanyAccessState::Declared(packaged_access) =
            packaged.model(model).unwrap().company_access
        else {
            panic!("packaged {model} has no declaration");
        };
        assert_eq!(
            native_access.restriction(),
            packaged_access.restriction(),
            "{model}"
        );
        assert_eq!(
            native_access.declaration(),
            packaged_access.declaration(),
            "{model}"
        );
        assert_eq!(
            native_access.source().addon(),
            packaged_access.source().addon(),
            "{model}"
        );
        assert_eq!(
            native_access.source().definition(),
            packaged_access.source().definition(),
            "{model}"
        );
    }
}
