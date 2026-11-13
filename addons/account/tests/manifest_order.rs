// SPDX-License-Identifier: MIT OR Apache-2.0
//! The data list of the `account` manifest keeps the order.
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
