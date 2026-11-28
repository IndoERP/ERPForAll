// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installing `project` with demo data on a demo database that already has the business modules
//! of the one-command local install (`scripts/install.sh`): contacts, approvals, HR, accounting,
//! inventory, sales and purchase with their bridges and chatter addons. Their demo data and the
//! fields and rules they add to shared models (partners, companies) must not make the project
//! demo fail.

use std::path::Path;

use erp_data::load::NoticeKind;
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, SUPERUSER_ID};
use erp_testkit::TestDb;

use crate::support::{project_module, run, stages_of, template_stages, xid};

/// The business modules `scripts/install.sh` installs with demo data before anything else.
const BUSINESS_MODULES: [&str; 15] = [
    "contacts",
    "approvals",
    "hr",
    "account",
    "account_reporting",
    "stock",
    "sale",
    "sale_management",
    "purchase",
    "sale_stock",
    "purchase_stock",
    "account_mail",
    "purchase_mail",
    "stock_mail",
    "product_mail",
];

/// The module system of the business modules, their dependencies and `project`, with the SQL
/// objects and guards the serving binary registers for them.
fn business_system(pool: erp_testkit::deadpool_postgres::Pool) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../addons")
            .canonicalize()
            .unwrap(),
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
    ModuleSystem::builder(pool, index)
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_sql_objects([
            &erp_addon_account::reconcile::history_guard::MANIFEST,
            &erp_addon_stock::schema_guards::MANIFEST,
            &erp_addon_stock_account::history_guards::MANIFEST,
            &erp_addon_valuation::MANIFEST,
            &erp_addon_account_reporting::schema::MANIFEST,
        ])
        .require_sql_uninstall_guards([
            &erp_addon_account::reconcile::history_guard::UNINSTALL,
            &erp_addon_stock_account::uninstall_guard::MANIFEST,
            &erp_addon_valuation::lifecycle::MANIFEST,
        ])
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_barcodes::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_product_mail::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_approvals::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
            &erp_addon_account_mail::ADDON,
            &erp_addon_purchase::ADDON,
            &erp_addon_purchase_mail::ADDON,
            &erp_addon_barcodes_gs1_nomenclature::ADDON,
            &erp_addon_reporting::ADDON,
            &erp_addon_account_reporting::ADDON,
            &erp_addon_stock::ADDON,
            &erp_addon_stock_account::ADDON,
            &erp_addon_valuation::ADDON,
            &erp_addon_stock_mail::ADDON,
            &erp_addon_purchase_stock::ADDON,
            &erp_addon_sales_team::ADDON,
            &erp_addon_sale::ADDON,
            &erp_addon_sale_stock::ADDON,
            &erp_addon_hr::ADDON,
            &erp_addon_project::ADDON,
        ])
        .build()
        .unwrap()
}

#[test]
fn project_demo_loads_next_to_the_business_modules() {
    run(|| async {
        let db = TestDb::create().await.unwrap();
        let system = business_system(db.pool().clone());
        let init = Box::pin(system.initialize(InitOptions::new().with_demo(true)))
            .await
            .unwrap();
        assert!(
            init.data_notices.is_empty(),
            "initialize notices: {:?}",
            init.data_notices
        );
        let env = init.orm.env(SUPERUSER_ID, Context::new());
        let business: Vec<ModuleName> = BUSINESS_MODULES
            .iter()
            .map(|name| ModuleName::new(*name).unwrap())
            .collect();
        let outcome = Box::pin(system.button_immediate_install(&env, &business))
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
        assert!(
            outcome.data_notices.is_empty(),
            "business module notices: {:?}",
            outcome.data_notices
        );
        drop(outcome);

        // `erp module install project --with-demo` on that database: a new operation.
        let system = business_system(db.pool().clone());
        let loaded = Box::pin(system.load()).await.unwrap();
        let env = loaded.orm.env(SUPERUSER_ID, Context::new());
        let outcome = Box::pin(system.button_immediate_install(&env, &[project_module()]))
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
        let failures: Vec<String> = outcome
            .data_notices
            .iter()
            .filter_map(|notice| match notice.kind() {
                NoticeKind::DemoFailed { error } => {
                    Some(format!("{} {}: {error}", notice.module(), notice.file()))
                }
                _ => None,
            })
            .collect();
        assert!(failures.is_empty(), "demo failures: {failures:#?}");
        assert!(
            outcome.data_notices.is_empty(),
            "project notices: {:?}",
            outcome.data_notices
        );

        let client = db.client().await.unwrap();
        for name in ["project_demo_website", "project_demo_warehouse"] {
            let project = xid(&client, "project", name).await;
            assert_eq!(
                stages_of(&client, project).await,
                template_stages(),
                "{name} has its own copy of the template stages"
            );
        }
        let tasks: i64 = client
            .query_one(
                "SELECT count(*) FROM project_task t JOIN ir_model_data d \
                 ON d.model = 'project.task' AND d.res_id = t.id AND d.module = 'project'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(tasks, 9, "every demo task loaded");

        drop(outcome);
        drop(loaded);
        drop(init);
        drop(system);
        drop(client);
        db.cleanup().await.unwrap();
    });
}
