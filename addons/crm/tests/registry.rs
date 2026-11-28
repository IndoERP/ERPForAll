// SPDX-License-Identifier: MIT OR Apache-2.0
//! SQL-free CRM registry and package smoke tests.
#![allow(clippy::expect_used, reason = "test assertions")]
use erp_sdk::AddonDescriptor;
use std::{collections::BTreeSet, fs, path::Path};

const CONTACTS: AddonDescriptor = AddonDescriptor {
    name: "contacts",
    depends: &["base", "web"],
    exact_only: false,
};

#[test]
fn registry_composes_crm_with_its_real_sales_and_mail_parents() {
    let registry = erp_sdk::RegistryBuilder::new([
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_reporting::ADDON,
        &CONTACTS,
        &erp_addon_mail::ADDON,
        &erp_addon_sales_team::ADDON,
        &erp_addon_crm::ADDON,
    ])
    .build()
    .expect("CRM registry composition");
    assert!(
        erp_sdk::client_actions::registry_of(&registry)
            .expect("client action declarations")
            .get("crm.overview")
            .is_some(),
        "the CRM home menu requires its registered client action"
    );
    let lead = registry.model("crm.lead").expect("CRM lead");
    for field in [
        "activity_ids",
        "message_ids",
        "team_id",
        "pipeline_id",
        "stage_id",
        "won_amount",
        "next_activity_date",
        "score",
    ] {
        assert!(lead.field(field).is_some(), "missing {field}");
    }
    // Metadata drives guided import templates; server-managed values must stay out of them.
    for field in [
        "state",
        "originated_as_lead",
        "stage_since",
        "close_reason",
        "closed_date",
        "won_amount",
        "won_user_id",
        "won_team_id",
        "won_currency_id",
        "won_rate",
        "history_ids",
        "weighted_revenue",
        "next_activity_date",
    ] {
        assert!(
            lead.field(field).expect("managed CRM field").readonly,
            "importable lifecycle field: {field}"
        );
    }
    assert!(!lead.field("kind").expect("importable CRM kind").readonly);
    for model in [
        "crm.stage.history",
        "crm.lead.contact",
        "crm.rule",
        "crm.quota",
        "crm.qualify.wizard",
        "crm.submit.wizard",
    ] {
        assert!(registry.model(model).is_some(), "missing {model}");
    }
}

#[test]
fn manifest_lists_all_views_rules_and_data_and_matches_the_native_addon() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest: toml::Table = include_str!("../manifest.toml")
        .parse()
        .expect("valid manifest");
    let values = |field: &str| -> BTreeSet<String> {
        manifest[field]
            .as_array()
            .expect("array")
            .iter()
            .map(|s| s.as_str().expect("text").to_owned())
            .collect()
    };
    let mut shipped = BTreeSet::new();
    for dir in ["security", "data", "views"] {
        for file in fs::read_dir(root.join(dir)).expect("addon directory") {
            let path = file.expect("file").path();
            if path.is_file() {
                shipped.insert(
                    path.strip_prefix(root)
                        .expect("relative")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    assert_eq!(values("data"), shipped);
    let mut demo = BTreeSet::new();
    for file in fs::read_dir(root.join("demo")).expect("addon demo directory") {
        let path = file.expect("file").path();
        if path.is_file() {
            demo.insert(
                path.strip_prefix(root)
                    .expect("relative")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    assert_eq!(values("demo"), demo);
    assert_eq!(
        values("depends"),
        erp_addon_crm::ADDON
            .depends
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    );
}
