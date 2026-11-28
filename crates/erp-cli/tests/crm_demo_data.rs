// SPDX-License-Identifier: MIT OR Apache-2.0
//! Demo data of the `crm` and `sale_crm` addons, plus root application menu ordering.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;

use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::{Context, SUPERUSER_ID};
use erp_testkit::TestDb;

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
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
    ModuleSystem::builder(db.pool().clone(), index)
        .require_sql_objects([&erp_addon_account::reconcile::history_guard::MANIFEST])
        .require_sql_uninstall_guards([&erp_addon_account::reconcile::history_guard::UNINSTALL])
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_product_mail::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
            &erp_addon_account_mail::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_reporting::ADDON,
            &erp_addon_sales_team::ADDON,
            &erp_addon_sale::ADDON,
            &erp_addon_crm::ADDON,
            &erp_addon_sale_crm::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap()
}

/// Run `body` on a thread with a large stack: a full install chain exceeds the default one.
fn big_stack(body: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(body);
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn install_and_read(demo: bool, queries: &[&str]) -> Vec<Vec<String>> {
    let db = TestDb::create().await.unwrap();
    let system = system(&db);
    let init = Box::pin(
        system.initialize(
            InitOptions::new()
                .with_demo(demo)
                .with_skip_auto_install(true),
        ),
    )
    .await
    .unwrap();
    assert!(
        init.data_notices.is_empty(),
        "initialize notices: {:?}",
        init.data_notices
    );
    let env = system.system_env(Context::new());
    let installed = Box::pin(system.button_immediate_install(
        &env,
        &[
            ModuleName::new("crm").unwrap(),
            ModuleName::new("sale_crm").unwrap(),
        ],
    ))
    .await
    .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    // A failed demo file is rolled back and reported here, not raised.
    assert!(
        installed.data_notices.is_empty(),
        "install notices: {:?}",
        installed.data_notices
    );
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    let mut out = Vec::with_capacity(queries.len());
    for sql in queries {
        let rows = tx.query(sql, &[]).await.unwrap();
        out.push(rows.iter().map(|row| row.get::<_, String>(0)).collect());
    }
    tx.rollback().await.unwrap();
    drop(tx);
    drop(installed);
    drop(init);
    drop(system);
    db.cleanup().await.unwrap();
    out
}

#[test]
fn crm_and_sale_crm_demo_data_loads_cleanly_and_settings_menu_is_last() {
    big_stack(async {
        let with_demo = install_and_read(
            true,
            &[
                "SELECT count(*)::text FROM crm_rule",
                "SELECT kind || ':' || count(*)::text FROM crm_lead GROUP BY kind ORDER BY kind",
                "SELECT count(DISTINCT stage_id)::text FROM crm_lead WHERE kind = 'opportunity'",
                "SELECT count(*)::text FROM crm_lead_contact",
                "SELECT count(*)::text FROM sale_order WHERE opportunity_id IS NOT NULL",
                "SELECT name FROM ir_ui_menu WHERE parent_id IS NULL ORDER BY sequence, id",
            ],
        )
        .await;
        assert_eq!(with_demo[0], vec!["3"]);
        assert_eq!(with_demo[1], vec!["lead:3", "opportunity:6"]);
        assert_eq!(with_demo[2], vec!["4"]);
        assert_eq!(with_demo[3], vec!["4"]);
        assert_eq!(with_demo[4], vec!["1"]);
        assert_eq!(
            with_demo[5],
            vec![
                "Discuss",
                "CRM",
                "Sales",
                "Accounting",
                "Contacts",
                "Catalog",
                "Settings",
            ]
        );

        let without_demo = install_and_read(
            false,
            &[
                "SELECT count(*)::text FROM crm_rule",
                "SELECT count(*)::text FROM crm_lead",
                "SELECT count(*)::text FROM crm_lead_contact",
                "SELECT count(*)::text FROM sale_order WHERE opportunity_id IS NOT NULL",
            ],
        )
        .await;
        assert_eq!(without_demo[0], vec!["0"]);
        assert_eq!(without_demo[1], vec!["0"]);
        assert_eq!(without_demo[2], vec!["0"]);
        assert_eq!(without_demo[3], vec!["0"]);
    });
}
