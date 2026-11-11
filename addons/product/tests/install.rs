//! Static consistency of the manifest. The real install, the seeded records and the ACL rows are
//! checked by `crates/erp-cli/tests/product_install.rs`, which installs the addon into a database.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn manifest_data() -> BTreeSet<String> {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest.toml"))
        .expect("manifest.toml");
    let table: toml::Table = text.parse().expect("manifest.toml is TOML");
    table["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|v| v.as_str().expect("path").to_owned())
        .collect()
}

/// Every listed data file exists, and no data, security or view file is left out of the manifest
/// (an unlisted file would silently never load).
#[test]
fn manifest_lists_exactly_the_shipped_data_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let listed = manifest_data();
    for path in &listed {
        assert!(root.join(path).is_file(), "{path} is listed but missing");
    }
    let mut shipped = BTreeSet::new();
    for dir in ["data", "security", "views"] {
        for entry in fs::read_dir(root.join(dir)).expect("addon directory") {
            let name = entry.unwrap().file_name().into_string().unwrap();
            shipped.insert(format!("{dir}/{name}"));
        }
    }
    assert_eq!(listed, shipped, "manifest data and shipped files differ");
}

/// `manifest.toml` and the `addon!` descriptor name the same dependencies.
#[test]
fn manifest_depends_match_the_descriptor() {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest.toml"))
        .expect("manifest.toml");
    let table: toml::Table = text.parse().unwrap();
    let from_manifest: BTreeSet<&str> = table["depends"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let from_descriptor: BTreeSet<&str> =
        erp_addon_product::ADDON.depends.iter().copied().collect();
    assert_eq!(from_manifest, from_descriptor);
}
