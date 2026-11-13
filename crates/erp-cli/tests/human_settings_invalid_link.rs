// SPDX-License-Identifier: MIT OR Apache-2.0
//! A panel item whose link does not resolve must abort installation before publication/commit.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
use erp_addon_base::ADDON;
use erp_sdk::settings::{PanelItemDecl, PanelItemValue, SettingsScope};
fn items() -> erp_sdk::Result<Vec<PanelItemDecl>> {
    Ok(vec![
        PanelItemDecl::action_link(
            "base.test_broken_link",
            "Broken",
            "base.no_such_action",
            false,
        )?
        .access(&["base.group_system"])?,
    ])
}
fn resolve<'a>(
    _env: &'a erp_sdk::Env,
    _tx: &'a mut erp_sdk::Transaction,
    _scope: &'a SettingsScope,
    key: &'a str,
) -> erp_sdk::BoxFuture<'a, erp_sdk::Result<PanelItemValue>> {
    Box::pin(async move { panic!("a plain link has no resolver call ({key})") })
}
erp_sdk::settings_panel_items!(items, resolve);

#[tokio::test]
async fn unresolved_item_link_cannot_publish_an_initialized_database() {
    let db = erp_testkit::TestDb::create().await.unwrap();
    let mut index = erp_modules::AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    // Explicit provenance for this binary's private native fixture bytes.
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
    let system = erp_modules::system::ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        .code([&ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let result = system
        .initialize(erp_modules::system::InitOptions::new().with_skip_auto_install(true))
        .await;
    let error = match result {
        Ok(_) => panic!("an unresolved item link published"),
        Err(error) => error,
    };
    match error {
        erp_modules::Error::StepFailed {
            module,
            step: erp_modules::ModuleStep::RegisterHook { model },
            source,
            ..
        } => {
            assert_eq!(module.as_str(), "base");
            assert_eq!(model, "ir.config.setting");
            match *source {
                erp_modules::Error::Orm(error) => {
                    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
                    assert_eq!(
                        error.message(),
                        "Setting item references an unavailable action or menu."
                    );
                }
                other => panic!("unexpected source: {other:?}"),
            }
        }
        other => panic!("unexpected error: {other:?}"),
    }
    let client = db.client().await.unwrap();
    assert!(
        client
            .query_one("SELECT to_regclass('ir_config_setting_scope')::text", &[])
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .is_none()
    );
    drop(client);
    drop(system);
    db.cleanup().await.unwrap();
}
