//! The `addons/base` crate: its manifest, its descriptor and the registry it stands
//! for. No spec case: these check the skeleton other M3 slices fill.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::path::PathBuf;

use erp_modules::{ModuleName, read_manifest_from_dir};
use erp_registry::RegistryBuilder;

fn base_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../addons/base")
}

/// base_addon_manifest_matches_its_descriptor
#[test]
fn base_addon_manifest_matches_its_descriptor() {
    let name = ModuleName::new("base").unwrap();
    let manifest = read_manifest_from_dir(&base_dir(), &name)
        .unwrap()
        .expect("addons/base has a manifest.toml");
    let descriptor = &erp_addon_base::ADDON;
    assert_eq!(manifest.name().as_str(), descriptor.name);
    let manifest_depends: Vec<&str> = manifest.depends().iter().map(|d| d.as_str()).collect();
    assert_eq!(manifest_depends, descriptor.depends);
    assert!(descriptor.depends.is_empty(), "base depends on nothing");
    assert!(manifest.auto_install().is_enabled());
    assert!(manifest.installable());
    assert_eq!(manifest.license(), "MIT OR Apache-2.0");
}

/// base_addon_stands_for_the_core_base_in_a_registry
#[test]
fn base_addon_stands_for_the_core_base_in_a_registry() {
    let with_crate = RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    let core_only = RegistryBuilder::new([]).build().unwrap();
    assert!(core_only.model("res.users").is_some());
    assert!(core_only.model("res.company").is_some());
    assert!(with_crate.model("res.users").is_some());
    assert!(with_crate.model("res.company").is_some());
    assert!(with_crate.model("res.currency").is_some());
    assert!(with_crate.model("res.country").is_some());
    assert!(with_crate.model("res.country.state").is_some());
    assert!(with_crate.model("res.country.group").is_some());
    assert!(with_crate.model("res.lang").is_some());
    assert!(with_crate.model("ir.default").is_some());
    assert!(with_crate.model("ir.sequence").is_some());
    assert!(with_crate.model("ir.sequence.date_range").is_some());
    assert!(with_crate.model("ir.model").is_some());
    assert!(with_crate.model("ir.model.fields").is_some());
    assert!(with_crate.model("ir.model.relation").is_some());
    assert!(with_crate.model("decimal.precision").is_some());
}
