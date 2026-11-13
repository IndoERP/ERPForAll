// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company-map deltas preserve explicit null and have independent cache identities.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::ColumnType;
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sql::version::{VersionedUpdate, update_from_arrays, update_from_arrays_cached};

#[test]
fn company_delta_does_not_strip_explicit_nulls() {
    let sql = update_from_arrays(
        "items",
        &[("cost", ColumnType::Jsonb)],
        &[],
        &VersionedUpdate::new()
            .merge_company(&["cost"])
            .returning_changed(&["cost"]),
    );
    assert!(!sql.contains("jsonb_strip_nulls"), "{sql}");
    assert!(sql.contains("jsonb_object_agg"), "{sql}");
    assert!(sql.contains("NULLIF("), "{sql}");
    assert!(sql.contains("IS DISTINCT FROM"), "{sql}");
}

#[test]
fn company_and_translation_updates_use_distinct_cache_entries() {
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_company_delta",
        depends: &[],
        exact_only: false,
    };
    let fields =
        Box::leak(vec![FieldDecl::char("value").company_dependent(true)].into_boxed_slice());
    let model = Box::leak(Box::new(
        ModelDecl::new("test_company_delta", "test.company.delta").fields(fields),
    ));
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*model])
        .build()
        .unwrap();
    let spec = registry.model("test.company.delta").unwrap();
    let columns = [("value", ColumnType::Jsonb)];
    let company = VersionedUpdate::new().merge_company(&["value"]);
    let translated = VersionedUpdate::new().merge_json(&["value"]);
    let c = update_from_arrays_cached(&registry, spec, &columns, &[], &company);
    let t = update_from_arrays_cached(&registry, spec, &columns, &[], &translated);
    assert_ne!(c, t);
    assert_eq!(
        &*c,
        update_from_arrays(&spec.table, &columns, &[], &company)
    );
    assert_eq!(
        &*t,
        update_from_arrays(&spec.table, &columns, &[], &translated)
    );
    assert_eq!(
        c,
        update_from_arrays_cached(&registry, spec, &columns, &[], &company)
    );
}
