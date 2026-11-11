//! the harness declares required core tables like the server's build site and refuses,
//! before any database exists, when one has no linked registration. The binary that does not
//! link the owner crate at all is `crates/erp-orm/tests/core_table_guard.rs`; here the missing
//! table is one no crate registers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::ErrorKind;
use erp_orm::CoreTableRequirement;
use erp_testkit::{Harness, HarnessError};

static ABSENT: [CoreTableRequirement; 1] =
    [CoreTableRequirement::new("erp_absent_table", "erp-absent")];

static NODE_TABLES: [CoreTableRequirement; 1] = [CoreTableRequirement::new("erp_node", "erp-orm")];

const MISSING: &str = "core table 'erp_absent_table' of 'erp-absent' is required but not \
                       registered in this binary: nothing linked references the core_ddl! \
                       registration of 'erp-absent', so the linker dropped it; \
                       reference the owner's list (e.g. erp_security::auth::core_tables()) from \
                       the binary";

fn orm_error(result: Result<(), HarnessError>) -> erp_fields::Error {
    match result {
        Err(HarnessError::Orm(e)) => e,
        other => panic!("expected an ORM error, got {other:?}"),
    }
}

/// `check` (the startup checks without a database) refuses.
#[test]
fn check_refuses_a_required_table_that_is_not_registered() {
    let e = orm_error(Harness::builder([]).require_core_tables(&ABSENT).check());
    assert_eq!((e.kind(), e.message()), (ErrorKind::Programming, MISSING));
}

/// `build` refuses before it creates a database: no test database is made or left behind.
#[tokio::test]
async fn build_refuses_before_any_database() {
    let e = orm_error(
        Harness::builder([])
            .require_core_tables(&ABSENT)
            .build()
            .await
            .map(drop),
    );
    assert_eq!((e.kind(), e.message()), (ErrorKind::Programming, MISSING));
}

/// A required table that is linked passes.
#[test]
fn a_linked_table_passes() {
    Harness::builder([])
        .require_core_tables(&NODE_TABLES)
        .check()
        .unwrap();
}
