//! Native base industry defaults and preservation across real module lifecycle operations.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use erp_modules::AddonIndex;
use erp_modules::manifest::ModuleName;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_sdk::Context;
use erp_testkit::TestDb;

const INDUSTRY_IDS: &[&str] = &[
    "industry_administration",
    "industry_arts",
    "industry_building",
    "industry_education",
    "industry_energy",
    "industry_finance",
    "industry_health",
    "industry_home_services",
    "industry_hospitality",
    "industry_information",
    "industry_international",
    "industry_land",
    "industry_manufacturing",
    "industry_mining",
    "industry_other_services",
    "industry_primary",
    "industry_professional",
    "industry_public_services",
    "industry_trade",
    "industry_transport",
    "industry_water",
];

async fn industry_ids(client: &tokio_postgres::Client) -> Vec<(String, i64)> {
    client.query(
        "SELECT name, res_id FROM ir_model_data WHERE module='base' AND model='res.partner.industry' ORDER BY name",
        &[],
    ).await.unwrap().into_iter().map(|row| (row.get(0), row.get(1))).collect()
}

#[tokio::test]
async fn base_industries_install_upgrade_and_preserve_tenant_records() {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    let native_artifact_identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                std::path::Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let initialized = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(initialized.post_commit.is_clean());
    assert!(
        initialized.data_notices.is_empty(),
        "{:?}",
        initialized.data_notices
    );
    let client = db.client().await.unwrap();
    let fresh = industry_ids(&client).await;
    assert_eq!(
        fresh
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        INDUSTRY_IDS
    );
    let records = client.query(
        "SELECT i.id, i.name, i.full_name, i.active, d.noupdate FROM res_partner_industry i JOIN ir_model_data d ON d.res_id=i.id AND d.model='res.partner.industry' WHERE d.module='base' ORDER BY d.name",
        &[],
    ).await.unwrap();
    for row in &records {
        assert!(!row.get::<_, String>(1).trim().is_empty());
        assert!(!row.get::<_, String>(2).trim().is_empty());
        assert!(row.get::<_, bool>(3));
        assert!(row.get::<_, bool>(4));
    }

    // Model a pre-seed tenant: its own record can share a seed label without becoming
    // a base-owned XML identity. Only the fresh fixture's own seed rows are removed.
    let tenant_name: String = records[0].get(1);
    let seeded_ids = fresh.iter().map(|(_, id)| *id).collect::<Vec<_>>();
    client
        .execute(
            "DELETE FROM ir_model_data WHERE module='base' AND model='res.partner.industry'",
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            "DELETE FROM res_partner_industry WHERE id=ANY($1)",
            &[&seeded_ids],
        )
        .await
        .unwrap();
    let tenant_id: i64 = client.query_one(
        "INSERT INTO res_partner_industry(name, full_name, active) VALUES($1, 'Tenant-owned classification', false) RETURNING id",
        &[&tenant_name],
    ).await.unwrap().get(0);
    let env = system.system_env(Context::new());
    let base = ModuleName::new("base").unwrap();
    let upgraded = system
        .button_immediate_upgrade(&env, std::slice::from_ref(&base))
        .await
        .unwrap();
    assert!(upgraded.post_commit.is_clean());
    assert!(
        upgraded.data_notices.is_empty(),
        "{:?}",
        upgraded.data_notices
    );
    drop(upgraded);
    let upgrade_ids = industry_ids(&client).await;
    assert_eq!(
        upgrade_ids
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        INDUSTRY_IDS
    );
    assert!(upgrade_ids.iter().all(|(_, id)| *id != tenant_id));
    let edited_id = upgrade_ids[0].1;
    client.execute("UPDATE res_partner_industry SET name='Tenant seed label', full_name='Tenant seed description', active=false WHERE id=$1", &[&edited_id]).await.unwrap();

    let repeated_install = system
        .button_immediate_install(&env, std::slice::from_ref(&base))
        .await
        .unwrap();
    assert!(repeated_install.post_commit.is_clean());
    drop(repeated_install);
    for _ in 0..2 {
        let upgraded = system
            .button_immediate_upgrade(&env, std::slice::from_ref(&base))
            .await
            .unwrap();
        assert!(upgraded.post_commit.is_clean());
        assert!(
            upgraded.data_notices.is_empty(),
            "{:?}",
            upgraded.data_notices
        );
        assert_eq!(industry_ids(&client).await, upgrade_ids);
        let edited = client
            .query_one(
                "SELECT name, full_name, active FROM res_partner_industry WHERE id=$1",
                &[&edited_id],
            )
            .await
            .unwrap();
        assert_eq!(edited.get::<_, String>(0), "Tenant seed label");
        assert_eq!(edited.get::<_, String>(1), "Tenant seed description");
        assert!(!edited.get::<_, bool>(2));
        let tenant = client
            .query_one(
                "SELECT name, full_name, active FROM res_partner_industry WHERE id=$1",
                &[&tenant_id],
            )
            .await
            .unwrap();
        assert_eq!(tenant.get::<_, String>(0), tenant_name);
        assert_eq!(tenant.get::<_, String>(1), "Tenant-owned classification");
        assert!(!tenant.get::<_, bool>(2));
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM res_partner_industry", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            i64::try_from(INDUSTRY_IDS.len() + 1).unwrap()
        );
    }
    drop(client);
    drop(initialized);
    drop(system);
    db.cleanup().await.unwrap();
}
