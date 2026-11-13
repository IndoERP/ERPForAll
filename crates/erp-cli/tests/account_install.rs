// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installing `account` (and so `analytic`) on a fresh database with `base` and `product`.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::path::{Path, PathBuf};

use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::{Context, SUPERUSER_ID};
use erp_testkit::TestDb;

fn addons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

#[tokio::test]
async fn account_and_analytic_install_on_base_and_product() {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons_dir());
    let identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome = Box::pin(system.button_immediate_install(
        &env,
        &[
            ModuleName::new("account").unwrap(),
            ModuleName::new("product").unwrap(),
        ],
    ))
    .await
    .unwrap();
    assert!(outcome.post_commit.is_clean());

    let client = db.client().await.unwrap();
    for name in ["account", "analytic", "product"] {
        let rows = client
            .query(
                "SELECT state FROM ir_module_module WHERE name = $1::text",
                &[&name],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "module row {name}");
        assert_eq!(rows[0].get::<_, String>(0), "installed", "state of {name}");
    }
}
