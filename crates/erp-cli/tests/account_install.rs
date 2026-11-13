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

async fn strings(tx: &mut erp_sdk::Transaction, sql: &str) -> Vec<String> {
    tx.query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect()
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
    // the chain model's access rows and its declared lock-wait setting. Boxed: the two halves of
    // this test in one future overflow the default test-thread stack in a debug build.
    Box::pin(async {
        let access = client
            .query(
                "SELECT coalesce(g.module || '.' || g.name, '-') || '/' || a.operation, \
                 coalesce(a.generated, false) FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
                 LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
                 WHERE m.model = $1::text ORDER BY 1",
                &[&erp_addon_account::numbering::CHAIN_MODEL],
            )
            .await
            .unwrap();
        let permissions: Vec<String> = access
            .iter()
            .filter(|r| !r.get::<_, bool>(1))
            .map(|r| r.get(0))
            .collect();
        assert_eq!(permissions, ["base.group_user/r"], "chain permissions");
        assert_eq!(
            access.iter().filter(|r| r.get::<_, bool>(1)).count(),
            1,
            "the company restriction generated from company_access"
        );
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
        let wait = erp_addon_account::numbering::LockWait::from_settings(&mut tx)
            .await
            .unwrap();
        assert_eq!(
            wait.millis(),
            erp_addon_account::numbering::LOCK_WAIT_DEFAULT_MS
        );
        tx.rollback().await.unwrap();
    })
    .await;

    let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();

    // mappings (accounts come from a chart template, roots follow the codes)
    let tags = strings(
        &mut tx,
        "SELECT d.name || '=' || t.name || '/' || t.applicability FROM ir_model_data d \
         JOIN account_account_tag t ON t.id = d.res_id \
         WHERE d.module = 'account' AND d.model = 'account.account.tag' ORDER BY d.name",
    )
    .await;
    assert_eq!(
        tags,
        [
            "account_tag_financing=Financing Activities/accounts",
            "account_tag_investing=Investing & Extraordinary Activities/accounts",
            "account_tag_operating=Operating Activities/accounts",
        ]
    );
    for table in ["account_account", "account_root", "account_account_code"] {
        let count = strings(&mut tx, &format!("SELECT count(*)::text FROM {table}")).await;
        assert_eq!(count, ["0"], "{table} has no default records");
    }

    // the Accounting > Configuration menus and the three groups are installed
    let menus = strings(&mut tx, "SELECT name FROM ir_model_data WHERE module = 'account' AND model = 'ir.ui.menu' ORDER BY name").await;
    assert_eq!(
        menus,
        [
            "menu_account_tag",
            "menu_action_account_form",
            "menu_finance",
            "menu_finance_configuration"
        ]
    );
    let groups = strings(&mut tx, "SELECT name FROM ir_model_data WHERE module = 'account' AND model = 'res.groups' ORDER BY name").await;
    assert_eq!(
        groups,
        [
            "group_account_manager",
            "group_account_readonly",
            "group_account_user"
        ]
    );

    let tag = tx.env().records("account.account.tag").unwrap();
    let master = tag
        .search(
            &mut tx,
            &erp_sdk::Domain::condition("name", erp_sdk::Operator::Eq, "Operating Activities")
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(master.len(), 1);
    let error = master.unlink(&mut tx).await.unwrap_err();
    assert!(
        error.message().contains(
            "You cannot delete this account tag (Operating Activities), it is used on the chart of account definition."
        ),
        "{:?}",
        error.message()
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}
