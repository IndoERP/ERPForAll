//! The transient cleanup policy declared in Rust (`#[model]` / `#[extend]`) and read back from the
//! addon's `describe()` description (the route a WebAssembly addon takes) composes the same
//! validated metadata (package A). Pure: no database. The
//! cleanup itself is a later package and is not active.
#![allow(clippy::unwrap_used, reason = "tests")]

use erp_fields::serde_json;
use erp_modules::Manifest;
use erp_modules::wasm_lifecycle::models_from_describe;
use erp_registry::{ModelKind, Overlay, Registry, RegistryBuilder, TransientPolicy};
use erp_test_models::test_transient_policy;

const MODELS: [&str; 5] = [
    "test_tp.defaults",
    "test_tp.tuned",
    "test_tp.count_only",
    "test_tp.plain",
    // the extension has no model of its own
    "test_tp.tuned",
];

fn native() -> Registry {
    RegistryBuilder::new([&test_transient_policy::ADDON])
        .build()
        .unwrap()
}

fn from_describe() -> Registry {
    let manifest = Manifest::from_toml(
        "test_transient_policy",
        "name = 'Policy fixture'\ndepends = ['base']\n[wasm]\nsdk_version = '1.0'\n",
    )
    .unwrap();
    // The fixtures share one crate, so they name their addon with `addon = "…"`; a WebAssembly
    // component is one addon and writes no such argument. Drop it from the description, nothing
    // else.
    let mut doc: serde_json::Value = serde_json::from_str(&erp_sdk::describe()).unwrap();
    for entry in doc["contributions"].as_array_mut().unwrap() {
        if let Some(args) = entry["detail"]["args"].as_object_mut() {
            args.remove("addon");
        }
    }
    let defs = models_from_describe(&manifest, &doc.to_string()).unwrap();
    assert_eq!(defs.len(), 5, "four models and one extension");
    let descriptor = erp_modules::model_files::addon_descriptor(&manifest).unwrap();
    let mut overlay = Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    RegistryBuilder::new([descriptor])
        .linked(false)
        .overlay(&overlay)
        .build()
        .unwrap()
}

fn policy(r: &Registry, model: &str) -> Option<TransientPolicy> {
    r.model(model).unwrap().transient_policy
}

#[test]
fn the_macro_declares_the_policy() {
    let r = native();
    let defaults = policy(&r, "test_tp.defaults").unwrap();
    assert_eq!(
        (defaults.max_age_seconds(), defaults.max_count()),
        (Some(3_600), None)
    );
    let tuned = policy(&r, "test_tp.tuned").unwrap();
    // the extension overrides the count (10) and inherits the age
    assert_eq!(
        (tuned.max_age_seconds(), tuned.max_count()),
        (Some(7_200), Some(10))
    );
    let count_only = policy(&r, "test_tp.count_only").unwrap();
    assert_eq!(
        (count_only.max_age_seconds(), count_only.max_count()),
        (None, Some(25))
    );
    assert_eq!(policy(&r, "test_tp.plain"), None);
    assert_eq!(r.model("test_tp.plain").unwrap().kind, ModelKind::Regular);
}

#[test]
fn native_and_wasm_declare_equivalent_policies() {
    let (n, w) = (native(), from_describe());
    for model in MODELS {
        assert_eq!(policy(&n, model), policy(&w, model), "{model}");
        assert_eq!(
            n.model(model).unwrap().kind,
            w.model(model).unwrap().kind,
            "{model}"
        );
    }
}
