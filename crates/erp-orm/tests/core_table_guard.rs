//! (review round 1, lead decisions 2 and 11): a core table registration
//! that nothing references is dropped by the linker, and the schema step would skip its table
//! silently. The build site declares the tables it needs; `OrmBuilder::build` (and so
//! `Orm::sync_schema`, which needs a built ORM and checks again) refuses when one has no linked
//! registration, naming the table and its owner crate.
//!
//! This binary cannot hold the authentication tables' registration: `erp-orm` does not depend on
//! `erp-security`, so the crate is not in the link at all — exactly a binary that names no code of
//! the owner crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_fields::ErrorKind;
use erp_orm::{CoreTableRequirement, Orm, OrmBuilder};
use erp_registry::{AddonDescriptor, RegistryBuilder};

static AUTH_TABLES: [CoreTableRequirement; 2] = [
    CoreTableRequirement::new("erp_session", "erp-security"),
    CoreTableRequirement::new("erp_login_throttle", "erp-security"),
];

static NODE_TABLES: [CoreTableRequirement; 1] = [CoreTableRequirement::new("erp_node", "erp-orm")];

const MISSING: &str = "core table 'erp_session' of 'erp-security' is required but not \
                       registered in this binary: nothing linked references the core_ddl! \
                       registration of 'erp-security', so the linker dropped it; \
                       reference the owner's list (e.g. erp_security::auth::core_tables()) from \
                       the binary";

/// A builder over an empty registry and a pool that never connects.
fn builder() -> OrmBuilder {
    let registry = Arc::new(
        RegistryBuilder::new(Vec::<&'static AddonDescriptor>::new())
            .build()
            .unwrap(),
    );
    let pool = erp_orm::pool::create_pool(tokio_postgres::Config::new(), 1).unwrap();
    Orm::builder(registry, pool)
}

/// The precondition: nothing in this binary registers the authentication tables.
#[test]
fn this_binary_does_not_register_the_auth_tables() {
    for need in &AUTH_TABLES {
        assert!(
            erp_orm::CORE_DDL
                .iter()
                .all(|ddl| !ddl.tables.contains(&need.table())),
            "{} is registered in a binary without erp-security",
            need.table()
        );
    }
}

/// The build refuses, naming the first missing table and its owner.
#[test]
fn building_refuses_a_required_table_that_is_not_registered() {
    let Err(e) = builder().require_core_tables(&AUTH_TABLES).build() else {
        panic!("an ORM was built without the required erp_session registration");
    };
    assert_eq!((e.kind(), e.message()), (ErrorKind::Programming, MISSING));
}

/// The check the harness and the schema step call: the same error, nothing for a linked table.
#[test]
fn validate_names_the_missing_table_and_its_owner() {
    let registry = RegistryBuilder::new(Vec::<&'static AddonDescriptor>::new())
        .build()
        .unwrap();
    let e = erp_orm::core_ddl_validate(&registry, &[&AUTH_TABLES[0]]).unwrap_err();
    assert_eq!((e.kind(), e.message()), (ErrorKind::Programming, MISSING));
    erp_orm::core_ddl_validate(&registry, &[&NODE_TABLES[0]]).unwrap();
}

/// A required table that is linked (the ORM's own node registry) passes, and the requirement is
/// kept on the ORM.
#[test]
fn a_linked_table_passes() {
    let orm = builder().require_core_tables(&NODE_TABLES).build().unwrap();
    assert_eq!(orm.required_core_tables(), [&NODE_TABLES[0]]);
}

/// Review of R1B (types 3): the owner is checked too: a requirement naming another crate than
/// the registration's is refused, naming both.
#[test]
fn a_requirement_with_the_wrong_owner_is_refused() {
    static WRONG: [CoreTableRequirement; 1] = [CoreTableRequirement::new("erp_node", "erp-rpc")];
    let Err(e) = builder().require_core_tables(&WRONG).build() else {
        panic!("a requirement with the wrong owner passed");
    };
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Programming,
            "core table 'erp_node' is required as a table of 'erp-rpc' but is registered by \
             'erp-orm'"
        )
    );
}
