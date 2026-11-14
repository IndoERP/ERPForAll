// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "test assertions")]
use erp_orm::{SchemaSyncOptions, SqlRoleBindings};
use erp_registry::SqlBinding;

#[test]
fn configured_role_names_are_owned() {
    let options = {
        let role = String::from("request \"é");
        SchemaSyncOptions::new().sql_roles(SqlRoleBindings::new().request_owned(role))
    };
    let copy = options.clone();
    drop(options);
    assert_eq!(
        copy.sql_role_bindings().role(SqlBinding::Request),
        Some("request \"é")
    );
}
