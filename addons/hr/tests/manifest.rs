// SPDX-License-Identifier: MIT OR Apache-2.0
//! Static consistency of the manifest and the declared model.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn manifest() -> toml::Table {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest.toml"))
        .expect("manifest.toml");
    text.parse().expect("manifest.toml is TOML")
}

fn strings(table: &toml::Table, key: &str) -> BTreeSet<String> {
    table[key]
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_owned())
        .collect()
}

/// Every listed data file exists and no view or security file is left out of the manifest.
#[test]
fn manifest_lists_exactly_the_shipped_data_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let listed = strings(&manifest(), "data");
    let mut shipped = BTreeSet::new();
    for dir in ["security", "views"] {
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
    let from_manifest = strings(&manifest(), "depends");
    let from_descriptor: BTreeSet<String> = erp_addon_hr::ADDON
        .depends
        .iter()
        .map(|d| (*d).to_owned())
        .collect();
    assert_eq!(from_manifest, from_descriptor);
}

/// `hr.job` links with a required name, a company link and an `active` flag.
#[test]
fn hr_job_declares_its_fields() {
    let registry = erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON, &erp_addon_hr::ADDON])
        .build()
        .expect("real linked registry");
    let job = registry.model("hr.job").expect("hr.job is linked");
    assert!(job.field("name").expect("name").required);
    assert_eq!(
        job.field("company_id").expect("company_id").ty.type_name(),
        "many2one"
    );
    assert_eq!(
        job.field("active").expect("active").ty.type_name(),
        "boolean"
    );
}
