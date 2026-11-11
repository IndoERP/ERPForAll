//! Real addon installation, durable settings, authorization and tenant-local CSS publication.
#![allow(
    clippy::unwrap_used,
    reason = "integration-test helpers deliberately fail on unexpected fixture errors, matching the repository's test-only panic policy"
)]
use erp_addon_base::ir_config_setting::{describe, read_values, write_values};
use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID};
use erp_sdk::Value;
use erp_sdk::views::wire::{SettingValue, SettingsScope};
use std::{path::Path, sync::Arc};

const KEY: &str = "theme_office_blue.appearance";
const CSS: &str = "/theme_office_blue/static/src/theme.css";

fn system(db: &erp_testkit::TestDb) -> ModuleSystem {
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
    let options = erp_addon_web::assets::assembly::AssemblyOptions::new(
        "theme-test",
        erp_cli::ui_assets::RUNTIME_ABI,
        "en_US",
        "main",
        erp_sdk::ui_compiler::CompileOptions::new(erp_sdk::ui_compiler::BuildMode::Prod),
    )
    .unwrap();
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_theme_office_blue::ADDON,
        ])
        .lifecycle_hook(Arc::new(erp_cli::ui_assets::AssetLifecycle::new(options)))
        .build()
        .unwrap()
}
async fn assert_css(orm: &Orm, installed: bool) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let bundle = erp_addon_web::assets::store::current(
        &mut tx,
        "en_US",
        "main",
        erp_cli::ui_assets::RUNTIME_ABI,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(bundle.static_files().contains_key(CSS), installed);
    if installed {
        let url = &bundle.static_files()[CSS];
        assert!(url.starts_with("/erp/assets/"));
        assert!(url.ends_with(".css"));
        assert!(bundle.blobs().iter().any(|blob| blob.bytes()
            == include_bytes!("../../../addons/theme_office_blue/static/src/theme.css")));
    }
    tx.rollback().await.unwrap();
}
async fn theme(orm: &Orm) -> SettingValue {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let values = read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    values.values[0].value.clone()
}

#[test]
fn install_select_isolate_and_uninstall_real_theme() {
    // Same native install/RPC chain stack requirement as human_base_bootstrap.rs.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(theme_lifecycle());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn theme_lifecycle() {
    let a = erp_testkit::TestDb::create().await.unwrap();
    let b = erp_testkit::TestDb::create().await.unwrap();
    let sa = system(&a);
    let sb = system(&b);
    let initial = sa.initialize(InitOptions::new()).await.unwrap();
    let other = sb.initialize(InitOptions::new()).await.unwrap();
    assert_css(&initial.orm, false).await;
    assert_css(&other.orm, false).await;
    let installed = sa
        .button_immediate_install(
            &sa.system_env(Context::new()),
            &[ModuleName::new("theme_office_blue").unwrap()],
        )
        .await
        .unwrap();
    assert_css(&installed.orm, true).await;
    assert_css(&other.orm, false).await;
    assert!(matches!(theme(&installed.orm).await, SettingValue::String(ref v) if v == "default"));
    let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        "0".into(),
        vec![(KEY.into(), Value::str("office-blue"))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut audit_tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    let audit = audit_tx.query("SELECT setting_key, new_value, revision FROM ir_config_setting_audit WHERE setting_key=$1", &[&KEY]).await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].get::<_, String>(0), KEY);
    assert_eq!(audit[0].get::<_, String>(1), "\"office-blue\"");
    assert_eq!(audit[0].get::<_, String>(2), "1");
    audit_tx.rollback().await.unwrap();
    drop(audit_tx);
    let loaded = sa.load().await.unwrap();
    assert!(matches!(theme(&loaded.orm).await, SettingValue::String(ref v) if v == "office-blue"));
    let mut tx = other.orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    assert!(
        !describe(&env, &mut tx, SettingsScope::Global {})
            .await
            .unwrap()
            .declarations
            .iter()
            .any(|d| d.key == KEY)
    );
    tx.rollback().await.unwrap();
    let other_installed = sb
        .button_immediate_install(
            &sb.system_env(Context::new()),
            &[ModuleName::new("theme_office_blue").unwrap()],
        )
        .await
        .unwrap();
    assert_css(&other_installed.orm, true).await;
    assert!(
        matches!(theme(&other_installed.orm).await, SettingValue::String(ref v) if v == "default")
    );
    assert!(
        matches!(theme(&installed.orm).await, SettingValue::String(ref v) if v == "office-blue")
    );
    assert_internal_reader_cannot_save(&installed.orm, &a).await;
    let uninstalled = sa
        .button_immediate_uninstall(
            &sa.system_env(Context::new()),
            &[ModuleName::new("theme_office_blue").unwrap()],
        )
        .await
        .unwrap();
    assert_css(&uninstalled.orm, false).await;
    let mut tx = uninstalled.orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    assert!(
        !describe(&env, &mut tx, SettingsScope::Global {})
            .await
            .unwrap()
            .declarations
            .iter()
            .any(|d| d.key == KEY)
    );
    tx.rollback().await.unwrap();
    a.cleanup().await.unwrap();
    b.cleanup().await.unwrap();
}

async fn assert_internal_reader_cannot_save(installed: &Orm, db: &erp_testkit::TestDb) {
    let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
    let group: i64 = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_user'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let company: i64 = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='main_company'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let admin_group: i64 = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_system'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let administrators = tx
        .env()
        .records("res.users")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Theme administrator".into()))
                    .set_cell("login", Cell::Str("theme-administrator".into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("lang", Cell::Str("en_US".into()))
                    .set_cell("tz", Cell::Str("UTC".into()))
                    .set_commands(
                        "company_ids",
                        [Command::set([RecId::from_real(company).unwrap()])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::set([RecId::from_real(admin_group).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap();
    let users = tx
        .env()
        .records("res.users")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Theme reader".into()))
                    .set_cell("login", Cell::Str("theme-reader".into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("lang", Cell::Str("en_US".into()))
                    .set_cell("tz", Cell::Str("UTC".into()))
                    .set_commands(
                        "company_ids",
                        [Command::set([RecId::from_real(company).unwrap()])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::set([RecId::from_real(group).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap();
    let uid = users.ids()[0];
    let admin_uid = administrators.ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    let policy = erp_security::source::PolicySource::build(
        Arc::clone(installed.registry()),
        db.pool().clone(),
    )
    .await
    .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(policy);
    let orm = Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
        .resolver(erp_security::resolver::Resolver::for_registry(installed.registry()).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap();
    let mut admin_tx = orm
        .begin(erp_orm::UserId::new(admin_uid).unwrap())
        .await
        .unwrap();
    let admin_env = admin_tx.env();
    let admin_description = describe(&admin_env, &mut admin_tx, SettingsScope::Global {})
        .await
        .unwrap();
    assert!(
        admin_description
            .declarations
            .iter()
            .find(|d| d.key == KEY)
            .unwrap()
            .writable
    );
    let current = read_values(
        &admin_env,
        &mut admin_tx,
        SettingsScope::Global {},
        vec![KEY.into()],
    )
    .await
    .unwrap();
    let saved = write_values(
        &admin_env,
        &mut admin_tx,
        SettingsScope::Global {},
        current.revision,
        vec![(KEY.into(), Value::str("office-blue"))],
    )
    .await
    .unwrap();
    assert!(
        matches!(saved.values[0].value, SettingValue::String(ref value) if value == "office-blue")
    );
    admin_tx.commit().await.unwrap();
    drop(admin_tx);
    let mut tx = orm.begin(erp_orm::UserId::new(uid).unwrap()).await.unwrap();
    let env = tx.env();
    let declaration = describe(&env, &mut tx, SettingsScope::Global {})
        .await
        .unwrap()
        .declarations
        .into_iter()
        .find(|d| d.key == KEY)
        .unwrap();
    assert!(!declaration.writable);
    assert!(
        matches!(read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()]).await.unwrap().values[0].value, SettingValue::String(ref v) if v == "office-blue")
    );
    let denied = write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        "1".into(),
        vec![(KEY.into(), Value::str("default"))],
    )
    .await
    .unwrap_err();
    assert_eq!(denied.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        denied.message(),
        "Setting is unavailable or access is denied."
    );
    tx.rollback().await.unwrap();
}
