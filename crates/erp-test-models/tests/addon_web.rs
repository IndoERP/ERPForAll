//! The `addons/web` crate: its manifest, its descriptor and its client package.
//! No spec case: these check the skeleton the M4 slices fill.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::path::PathBuf;

use erp_modules::{ModuleName, read_manifest_from_dir};
use erp_registry::RegistryBuilder;

fn web_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../addons/web")
}

/// web_addon_manifest_matches_its_descriptor
#[test]
fn web_addon_manifest_matches_its_descriptor() {
    let name = ModuleName::new("web").unwrap();
    let manifest = read_manifest_from_dir(&web_dir(), &name)
        .unwrap()
        .expect("addons/web has a manifest.toml");
    let descriptor = &erp_addon_web::ADDON;
    assert_eq!(manifest.name().as_str(), descriptor.name);
    let manifest_depends: Vec<&str> = manifest.depends().iter().map(|d| d.as_str()).collect();
    assert_eq!(manifest_depends, descriptor.depends);
    assert_eq!(descriptor.depends, ["base"]);
    assert!(manifest.auto_install().is_enabled());
    assert!(manifest.installable());
    assert_eq!(manifest.license(), "MIT OR Apache-2.0");
}

/// web_addon_static_dir_is_discovered
#[test]
fn web_addon_static_dir_is_discovered() {
    // Discovery attaches `static/` (erp-modules `read_manifest_from_dir`); the client package lives
    // there (addon layout) and is a member of the web/ pnpm workspace.
    let name = ModuleName::new("web").unwrap();
    let manifest = read_manifest_from_dir(&web_dir(), &name).unwrap().unwrap();
    assert_eq!(
        manifest.static_path(),
        Some(web_dir().join("static").as_path())
    );
    let package = web_dir().join("static/package.json");
    let text = std::fs::read_to_string(&package).unwrap();
    assert!(
        text.contains("\"name\": \"@erpforall-addon/web\""),
        "{text}"
    );
    assert!(
        text.contains("\"@erpforall/elang\": \"workspace:*\""),
        "{text}"
    );
}

/// web_addon_composes_with_base
#[test]
fn web_addon_composes_with_base() {
    let registry = RegistryBuilder::new([&erp_addon_base::ADDON, &erp_addon_web::ADDON])
        .build()
        .unwrap();
    assert!(registry.model("res.users").is_some());
}
