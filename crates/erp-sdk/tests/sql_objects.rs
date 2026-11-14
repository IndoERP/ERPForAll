// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native addons register trusted SQL declarations through the SDK layer, not core dependencies.
#![allow(clippy::unwrap_used, reason = "test registry assertions")]
#![cfg(not(target_family = "wasm"))]
use erp_sdk::sql_objects::{SqlConditionDecl, SqlConditionKind, SqlObjectDecl, SqlObjectManifest};

erp_sdk::addon!(name = "test_sdk_sql", depends = []);

#[erp_sdk::sql_objects::linkme::distributed_slice(erp_sdk::sql_objects::SQL_OBJECT_DECLS)]
#[linkme(crate = erp_sdk::sql_objects::linkme)]
static CONDITION: SqlObjectDecl = SqlObjectDecl::condition(
    "test_sdk_sql",
    "guard",
    SqlConditionDecl::new("ERP12", "sdk_guard", "The SDK guard refused this change.")
        .kind(SqlConditionKind::Validation),
);
static EXPECTED: SqlObjectManifest = SqlObjectManifest::new("test_sdk_sql", &["guard"]);

#[test]
fn native_sdk_registration_reaches_the_registry_collector() {
    let registry = erp_registry::RegistryBuilder::new([&ADDON])
        .require_sql_objects([&EXPECTED])
        .build()
        .unwrap();
    let (_, condition) = registry
        .sql_objects()
        .condition("ERP12", "sdk_guard")
        .unwrap();
    assert_eq!(condition.kind(), SqlConditionKind::Validation);
    assert_eq!(condition.message(), "The SDK guard refused this change.");
}
