// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installing `hr` into a fresh database: the table, the views, the action, the menus and the
//! access rows exist, and a job created without `active` is active.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::path::Path;

use erp_fields::Cell;
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, RawVals, SUPERUSER_ID};
use erp_testkit::TestDb;

const STACK_SIZE: usize = 16 * 1024 * 1024;

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
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
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_hr::ADDON,
        ])
        .build()
        .unwrap()
}

async fn names(client: &tokio_postgres::Client, model: &str) -> Vec<String> {
    client
        .query(
            "SELECT name FROM ir_model_data WHERE module = 'hr' AND model = $1::text ORDER BY name",
            &[&model],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn install() {
    let db = TestDb::create().await.unwrap();
    let system = system(&db);
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("hr").unwrap()]))
            .await
            .unwrap();
    assert!(outcome.post_commit.is_clean());

    let client = db.client().await.unwrap();
    assert_eq!(
        names(&client, "ir.ui.view").await,
        ["hr_job_view_form", "hr_job_view_list"]
    );
    assert_eq!(
        names(&client, "ir.actions.act_window").await,
        ["hr_job_action"]
    );
    assert_eq!(
        names(&client, "ir.ui.menu").await,
        ["hr_job_menu", "hr_menu_root"]
    );

    let access: Vec<String> = client
        .query(
            "SELECT coalesce(g.module || '.' || g.name, '-') || '/' || a.operation \
             FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
             LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
             WHERE m.model = 'hr.job' AND NOT coalesce(a.generated, false) ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(access, ["base.group_system/crud", "base.group_user/r"]);

    let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
    let created = tx
        .env()
        .records("hr.job")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Engineer".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    let job: bool = client
        .query_one("SELECT active FROM hr_job WHERE id = $1", &[&created])
        .await
        .unwrap()
        .get(0);
    assert!(job, "a new job position is active by default");
}

#[test]
fn hr_installs_its_model_views_menus_and_access() {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(install());
        })
        .unwrap()
        .join()
        .unwrap();
}
