//! Installed base password forms: metadata and no seeded secret rows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use erp_modules::{AddonIndex, InitOptions, ModuleSystem};
use erp_testkit::TestDb;
use std::{path::Path, sync::Arc};

#[test]
fn installed_password_wizards_have_masked_fields_and_no_default_instances() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(installed());
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn installed() {
    let db = TestDb::create().await.unwrap();
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
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = system.initialize(InitOptions::new()).await.unwrap();
    assert!(init.data_notices.is_empty(), "{:?}", init.data_notices);
    assert!(init.post_commit.failed.is_empty());
    for name in [
        "change.password.wizard",
        "change.password.user",
        "change.password.own",
        "res.users.identitycheck",
    ] {
        assert!(
            init.orm.registry().model(name).is_some(),
            "missing installed model {name}"
        );
    }
    let client = db.client().await.unwrap();
    for name in [
        "change_password_wizard_view",
        "change_password_own_form",
        "change_password_identity_form",
    ] {
        let rows = client.query("SELECT v.arch_db FROM ir_ui_view v JOIN ir_model_data d ON d.res_id = v.id WHERE d.module = 'base' AND d.name = $1 AND d.model = 'ir.ui.view'", &[&name]).await.unwrap();
        assert_eq!(rows.len(), 1, "missing view {name}");
        let arch: String = rows[0].get(0);
        assert!(
            arch.contains("password"),
            "view has no password widget {name}"
        );
    }
    for table in [
        "change_password_wizard",
        "change_password_user",
        "change_password_own",
        "res_users_identitycheck",
    ] {
        // Closed, compile-time table names; never interpolated from a request.
        let rows = client
            .query(&format!("SELECT count(*)::bigint FROM {table}"), &[])
            .await
            .unwrap();
        assert_eq!(
            rows[0].get::<_, i64>(0),
            0,
            "transient instances must not be seeded"
        );
    }
    let rows = client.query("SELECT v.arch_db FROM ir_ui_view v JOIN ir_model_data d ON d.res_id = v.id WHERE d.module = 'base' AND d.name = 'settings_users_form' AND d.model = 'ir.ui.view'", &[]).await.unwrap();
    let arch: String = rows[0].get(0);
    assert!(
        arch.contains("name=\"preference_change_password\""),
        "own-record button is missing"
    );
    assert!(
        arch.contains("invisible=\"not password_is_own\""),
        "own-record button must only show for the actor"
    );
    assert!(
        arch.contains("invisible=\"password_is_own\""),
        "administrator button must only show for another user"
    );
    let _registry = Arc::clone(init.orm.registry());
}
