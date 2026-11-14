// SPDX-License-Identifier: MIT OR Apache-2.0
//! A setting whose policy names a group that no data file of the operation creates is refused
//! by name at the end of the operation, and nothing is published.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
use erp_addon_base::ADDON;
use erp_sdk::settings::{ScopeKind, SettingDecl};
fn declarations() -> erp_sdk::Result<Vec<SettingDecl>> {
    Ok(vec![
        SettingDecl::boolean(
            "base.unshipped_group",
            "Unshipped",
            ScopeKind::Global,
            false,
        )?
        .access(&["base.group_never_shipped"], &["base.group_system"])?,
    ])
}
erp_sdk::setting_declarations!(declarations);

/// No `SPEC-` tag: the refusal text is ours (`ir_config_setting.rs`, `resolve_groups`); this pins
/// that `_register_hook` still checks groups after the operation's data has loaded.
#[tokio::test]
async fn a_policy_group_no_data_file_creates_refuses_the_operation() {
    let db = erp_testkit::TestDb::create().await.unwrap();
    let mut index = erp_modules::AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../addons")
            .canonicalize()
            .unwrap(),
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
    let system = erp_modules::system::ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        .code([&ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let error = match system
        .initialize(erp_modules::system::InitOptions::new().with_skip_auto_install(true))
        .await
    {
        Ok(_) => panic!("a policy naming a missing group published"),
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
                        "Setting policy references an unavailable group."
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
            .is_none(),
        "the refused operation committed nothing"
    );
    drop(client);
    drop(system);
    db.cleanup().await.unwrap();
}
