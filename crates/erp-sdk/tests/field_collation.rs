// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(dead_code, clippy::unwrap_used, reason = "metadata fixture")]
use erp_sdk::TextCollation;
use erp_sdk::model;
erp_sdk::addon!(name = "collation_sdk", depends = ["base"]);
#[model(name = "collation_sdk.item")]
struct Item {
    #[field(char, collation = "C")]
    code: Option<String>,
    #[field(text, collation = "default")]
    note: Option<String>,
}
#[test]
fn macro_keeps_collation() {
    let registry = erp_sdk::RegistryBuilder::new([&ADDON]).build().unwrap();
    assert_eq!(
        registry
            .model("collation_sdk.item")
            .unwrap()
            .field("code")
            .unwrap()
            .collation,
        TextCollation::C
    );
    let description = erp_sdk::describe();
    let document: serde_json::Value = serde_json::from_str(&description).unwrap();
    let entry = document["contributions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["model"] == "collation_sdk.item" && e["kind"] == "model")
        .unwrap();
    assert_eq!(entry["detail"]["fields"][0]["attrs"]["collation"], "\"C\"");
}
