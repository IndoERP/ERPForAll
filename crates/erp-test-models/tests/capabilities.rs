//! `capability!` from an addon's point of view: the declaration
//! is a registered static and the ORM build validates every linked one. No spec case.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

erp_sdk::capability!(
    /// Declared the way an addon declares one.
    pub TEST_EXPORT = "test_models.export_all"
);

/// capability_declared_through_the_sdk_is_registered
#[test]
fn capability_declared_through_the_sdk_is_registered() {
    assert_eq!(TEST_EXPORT.name(), "test_models.export_all");
    assert_eq!(TEST_EXPORT.declared_in(), "capabilities");
    let registered: Vec<&erp_sdk::Capability> = erp_orm::CAPABILITIES
        .iter()
        .filter(|c| c.name() == "test_models.export_all")
        .collect();
    assert_eq!(registered.len(), 1);
    assert!(std::ptr::eq(registered[0], &TEST_EXPORT));
}

/// orm_build_accepts_the_linked_capabilities
#[test]
fn orm_build_accepts_the_linked_capabilities() {
    // the registry/ORM checks of a server start, without a database
    erp_testkit::Harness::builder([&erp_test_models::base_users::ADDON])
        .check()
        .unwrap();
}

/// A capability the policy is asked about must be a registered one, not a value built by hand
/// through the hidden `__declare` (BASE-0 follow-up).
#[test]
fn registered_capability_refuses_values_built_outside_the_macro() {
    let registered = erp_orm::registered_capability(&TEST_EXPORT).unwrap();
    assert!(std::ptr::eq(registered, &TEST_EXPORT));
    // same name, same module path, not a member of the slice
    let forged = erp_orm::Capability::__declare("test_models.export_all", "capabilities");
    let error = erp_orm::registered_capability(&forged).unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        "capability 'test_models.export_all' (declared in capabilities) is not a registered \
         capability: declare it with capability!, never build it by hand"
    );
    // a by-value copy of the registered one is refused as well
    let copy = erp_orm::Capability::__declare(TEST_EXPORT.name(), TEST_EXPORT.declared_in());
    assert!(erp_orm::registered_capability(&copy).is_err());
}
