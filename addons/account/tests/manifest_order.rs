// SPDX-License-Identifier: MIT OR Apache-2.0
//! The data and demo lists of the `account` manifest keep the order.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

#[test]
fn data_list_order_security_first_access_last() {
    let manifest: toml::Table = include_str!("../manifest.toml").parse().unwrap();
    let data: Vec<&str> = manifest["data"]
        .as_array()
        .expect("data is a list")
        .iter()
        .map(|v| v.as_str().expect("data entries are strings"))
        .collect();
    if let Some(i) = data
        .iter()
        .position(|f| *f == "security/account_security.xml")
    {
        assert_eq!(
            i, 0,
            "security/account_security.xml must be first: {data:?}"
        );
    }
    if let Some(i) = data.iter().position(|f| *f == "security/ir.access.csv") {
        assert_eq!(
            i,
            data.len() - 1,
            "security/ir.access.csv must be last: {data:?}"
        );
    }
}

/// Demo files sit under `demo/`, are listed only in `demo` (never in `data`) and exist on disk.
#[test]
fn demo_files_are_listed_apart_from_data() {
    let manifest: toml::Table = include_str!("../manifest.toml").parse().unwrap();
    let list = |key: &str| -> Vec<String> {
        manifest[key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} is a list"))
            .iter()
            .map(|v| v.as_str().expect("entries are strings").to_owned())
            .collect()
    };
    let (data, demo) = (list("data"), list("demo"));
    assert!(!demo.is_empty(), "account ships demo files");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in &demo {
        assert!(file.starts_with("demo/"), "{file} is not under demo/");
        assert!(!data.contains(file), "{file} is listed in data too");
        assert!(root.join(file).is_file(), "{file} does not exist");
    }
    assert!(
        data.iter().all(|f| !f.starts_with("demo/")),
        "a demo file is listed in data: {data:?}"
    );
}
