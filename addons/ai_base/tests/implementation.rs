// SPDX-License-Identifier: MIT OR Apache-2.0
//! Offline contract for the implementation catalogue and its native records.
#![allow(clippy::expect_used, reason = "test assertions")]

use erp_addon_ai_base::implementation::{Coverage, coverage};
use std::{fs, path::Path};

#[test]
fn every_shipped_addon_has_reviewed_coverage_or_an_explicit_platform_exclusion() {
    let addons = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("addons");
    for entry in fs::read_dir(addons).expect("addons directory") {
        let path = entry.expect("addon entry").path();
        if path.join("manifest.toml").exists() {
            let name = path
                .file_name()
                .expect("addon name")
                .to_str()
                .expect("UTF-8");
            assert!(
                !matches!(coverage(name), Coverage::Unknown),
                "{name} requires a scenario"
            );
        }
    }
    assert!(matches!(coverage("customer_extension"), Coverage::Unknown));
    assert!(matches!(coverage("web"), Coverage::Platform));
}

#[test]
fn business_domains_have_a_master_data_entry_and_measurable_acceptance() {
    for (addon, expected) in [
        ("crm", "crm.pipeline"),
        ("sale_crm", "crm.lead"),
        ("purchase_stock", "purchase.order"),
        ("quality_control", "quality.point"),
        ("hr", "hr.department"),
        ("account", "account.journal"),
    ] {
        let Coverage::Scenario(rule) = coverage(addon) else {
            panic!("missing {addon}")
        };
        assert_eq!(rule.model, expected);
        assert!(rule.setup.len() > 20 && rule.acceptance.len() > 30);
    }
}

#[test]
fn native_project_and_steps_compose_with_base_alone() {
    let registry =
        erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON, &erp_addon_ai_base::ADDON])
            .build()
            .expect("compose implementation records");
    let project = registry
        .model("ai.implementation.project")
        .expect("project");
    let step = registry.model("ai.implementation.step").expect("step");
    for field in ["company_id", "step_ids", "requirements"] {
        assert!(project.field(field).is_some());
    }
    for field in [
        "project_id",
        "addon",
        "state",
        "acceptance",
        "evidence",
        "gap",
        "standard_feature",
        "example",
    ] {
        assert!(step.field(field).is_some(), "{field}");
    }
}

#[test]
fn import_receipts_identify_both_the_effective_mapping_and_values() {
    use erp_addon_ai_base::implementation::import_fingerprint;
    let rows = vec![vec!["ext-1".to_owned(), "Alice".to_owned()]];
    let original = import_fingerprint("res.partner", &["id", "name"], &rows);
    assert_eq!(
        original,
        import_fingerprint("res.partner", &["id", "name"], &rows)
    );
    assert_ne!(
        original,
        import_fingerprint("res.partner", &["id", "email"], &rows)
    );
    assert_ne!(
        original,
        import_fingerprint("res.company", &["id", "name"], &rows)
    );
    assert_ne!(
        original,
        import_fingerprint(
            "res.partner",
            &["id", "name"],
            &[vec!["ext-1A".into(), "lice".into()]]
        )
    );
}

#[test]
fn setup_contacts_claim_a_persistent_company_key_without_changing_native_references() {
    let registry =
        erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON, &erp_addon_ai_base::ADDON])
            .build()
            .expect("registry");
    let key = registry
        .model("ai.implementation.contact.key")
        .expect("setup business key");
    assert!(
        key.table_objects
            .iter()
            .any(|object| object.name == "_company_reference")
    );
    let partner = registry.model("res.partner").expect("native partner");
    assert!(
        !partner
            .table_objects
            .iter()
            .any(|object| object.name == "_company_reference")
    );
}
