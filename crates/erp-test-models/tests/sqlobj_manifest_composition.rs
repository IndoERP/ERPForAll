// SPDX-License-Identifier: MIT OR Apache-2.0
//! Expected registrations must survive real module and test-harness composition.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_fields::ErrorKind;
use erp_registry::SqlObjectManifest;
use erp_testkit::{Harness, HarnessError, TestDb};

static MISSING: SqlObjectManifest = SqlObjectManifest::new("base", &["missing_guard"]);
const MESSAGE: &str = "SQL object 'base/missing_guard' required by addon 'base' was not registered";

#[test]
fn harness_preserves_required_sql_manifest() {
    let error = Harness::builder([])
        .require_sql_objects([&MISSING])
        .registry_only()
        .unwrap_err();
    let HarnessError::Orm(error) = error else {
        panic!("wrong failure: {error}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), MESSAGE);
}

#[tokio::test]
async fn module_system_preserves_required_sql_manifest() {
    let db = TestDb::create().await.unwrap();
    let error = match erp_modules::ModuleSystem::builder(
        db.pool().clone(),
        erp_modules::AddonIndex::new(true),
    )
    .require_sql_objects([&MISSING])
    .build()
    {
        Ok(_) => panic!("missing registration was accepted"),
        Err(error) => error,
    };
    let erp_modules::Error::Orm(error) = error else {
        panic!("wrong failure: {error}")
    };
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), MESSAGE);
}
