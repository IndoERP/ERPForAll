// SPDX-License-Identifier: MIT OR Apache-2.0
//! A linked guard of an unrelated addon is not a contribution to every module system.
#![allow(clippy::unwrap_used, reason = "tests")]
mod common;
use common::*;
use erp_modules::sql_uninstall::{SqlGuardRead, SqlUninstallGuard};
use erp_orm::BoxFuture;
use erp_testkit::TestDb;
fn unused<'a>(_: &'a mut SqlGuardRead<'_>) -> BoxFuture<'a, erp_fields::Result<u64>> {
    Box::pin(async { Ok(0) })
}
#[linkme::distributed_slice(erp_modules::sql_uninstall::SQL_UNINSTALL_GUARDS)]
static UNRELATED: SqlUninstallGuard = SqlUninstallGuard::new(
    "unrelated",
    "protected",
    &["unrelated.item"],
    "blocked",
    "Blocked",
    unused,
);
#[tokio::test]
async fn unrelated_linked_guard_is_filtered_with_its_native_descriptor() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    // Only base is composed; unrelated's declaration is not this system's expected registration.
    builder(&db, &addons).build().unwrap();
    db.cleanup().await.unwrap();
}
