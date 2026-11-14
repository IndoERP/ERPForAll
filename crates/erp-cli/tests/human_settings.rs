//! Persisted declared settings through the base service, never an in-memory substitute.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
use erp_addon_base::ADDON;
use erp_sdk::prelude::*;
use erp_sdk::settings::{ScopeKind, SettingDecl};
fn declarations() -> Result<Vec<SettingDecl>> {
    Ok(vec![
        SettingDecl::boolean("base.test_enabled", "Enabled", ScopeKind::Global, false)?
            .access(&["base.group_system"], &["base.group_system"])?,
        SettingDecl::boolean(
            "base.test_company",
            "Company setting",
            ScopeKind::Company,
            false,
        )?
        .access(&["base.group_system"], &["base.group_system"])?,
    ])
}
erp_sdk::setting_declarations!(declarations);

#[test]
fn settings_persist_revision_audit_and_reject_partial_batches() {
    // The test future is large; the default 2 MiB test-thread stack overflows under the workspace build.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(persisted_settings());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[allow(clippy::too_many_lines, reason = "one persisted-settings walk")]
async fn persisted_settings() {
    use erp_addon_base::ir_config_setting::{read_values, write_values};
    use erp_sdk::views::wire::{SettingValue, SettingsScope};
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
        .code([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let initialized = system
        .initialize(erp_modules::system::InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let orm = &initialized.orm;
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let scope = SettingsScope::Global {};
    let admin_names = [
        "settings_users_action",
        "settings_companies_action",
        "settings_currencies_action",
        "settings_languages_action",
        "settings_groups_action",
        "settings_access_action",
    ];
    let admin_actions = tx.query("SELECT a.id::int8 FROM ir_act_window a JOIN ir_model_data d ON d.res_id=a.id AND d.model='ir.actions.act_window' WHERE d.module='base' AND d.name=ANY($1)", &[&admin_names.as_slice()]).await.unwrap();
    assert_eq!(admin_actions.len(), admin_names.len());
    for action in admin_actions {
        let id: i64 = action.get(0);
        let views = env
            .records("ir.actions.act_window")
            .unwrap()
            .browse([id])
            .unwrap()
            .cell(&mut tx, "views")
            .await
            .unwrap();
        let erp_sdk::Cell::Json(views) = views else {
            panic!("computed views must be JSON");
        };
        assert_eq!(views[0][1], "list");
        assert_eq!(views[1][1], "form");
    }
    let initial = read_values(
        &env,
        &mut tx,
        scope.clone(),
        vec!["base.test_enabled".into()],
    )
    .await
    .unwrap();
    assert_eq!(initial.revision, "0");
    assert!(matches!(
        initial.values[0].value,
        SettingValue::Boolean(false)
    ));
    tx.rollback().await.unwrap();
    // Two independent first writers must share the newly inserted durable identity.
    let first_writer = async || {
        let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        let env = tx.env();
        let result = write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            "0".into(),
            vec![("base.test_enabled".into(), Value::Bool(true))],
        )
        .await;
        if result.is_ok() {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
        result
    };
    let (first, second) = tokio::join!(first_writer(), first_writer());
    let results = [first, second];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let error = results.into_iter().find_map(Result::err).unwrap();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Conflict, "{error:?}");
    assert_eq!(error.message(), "Settings changed; reload before saving.");
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let stored = read_values(
        &env,
        &mut tx,
        scope.clone(),
        vec!["base.test_enabled".into()],
    )
    .await
    .unwrap();
    assert!(matches!(
        stored.values[0].value,
        SettingValue::Boolean(true)
    ));
    let stale = write_values(
        &env,
        &mut tx,
        scope.clone(),
        "0".into(),
        vec![("base.test_enabled".into(), Value::Bool(false))],
    )
    .await
    .unwrap_err();
    assert_eq!(stale.kind(), erp_sdk::ErrorKind::Conflict);
    assert_eq!(stale.message(), "Settings changed; reload before saving.");
    let invalid = write_values(
        &env,
        &mut tx,
        scope,
        "1".into(),
        vec![
            ("base.test_enabled".into(), Value::Bool(false)),
            ("database.secret".into(), Value::str("no")),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(invalid.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        invalid.message(),
        "Setting database.secret is unavailable or access is denied."
    );
    for (changes, expected) in [
        (vec![], "Invalid settings request."),
        (
            vec![
                ("base.test_enabled".into(), Value::Bool(false)),
                ("base.test_enabled".into(), Value::Bool(true)),
            ],
            "Invalid settings request.",
        ),
        (
            vec![("base.test_enabled".into(), Value::str("true"))],
            "base.test_enabled takes true or false.",
        ),
    ] {
        let error = write_values(&env, &mut tx, SettingsScope::Global {}, "1".into(), changes)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(error.message(), expected);
    }
    let too_many = (0..257)
        .map(|i| (format!("base.unknown_{i}"), Value::Bool(true)))
        .collect();
    let oversized = write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        "1".into(),
        too_many,
    )
    .await
    .unwrap_err();
    assert_eq!(oversized.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(oversized.message(), "Invalid settings request.");
    for revision in ["01", "18446744073709551616"] {
        let error = write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            revision.into(),
            vec![("base.test_enabled".into(), Value::Bool(true))],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(error.message(), "Invalid settings request.");
    }
    let revision_before = read_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        vec!["base.test_enabled".into()],
    )
    .await
    .unwrap();
    assert_eq!(revision_before.revision, "1");
    let audit = tx
        .query(
            "SELECT setting_key, old_value, new_value FROM ir_config_setting_audit",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].get::<_, String>(0), "base.test_enabled");
    assert_eq!(audit[0].get::<_, String>(1), "false");
    assert_eq!(audit[0].get::<_, String>(2), "true");
    let company_id: i64 = tx
        .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()[0]
        .get(0);
    let company_scope = SettingsScope::Company {
        company_id: erp_sdk::views::wire::WireId::try_from(u64::try_from(company_id).unwrap())
            .unwrap(),
    };
    let denied_company = read_values(
        &env,
        &mut tx,
        company_scope.clone(),
        vec!["base.test_company".into()],
    )
    .await
    .unwrap_err();
    assert_eq!(denied_company.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        denied_company.message(),
        "Choose a company available to your account."
    );
    let company_env = env.with_company(company_id).unwrap();
    let company_default = read_values(
        &company_env,
        &mut tx,
        company_scope.clone(),
        vec!["base.test_company".into()],
    )
    .await
    .unwrap();
    assert_eq!(company_default.revision, "0");
    assert!(matches!(
        company_default.values[0].value,
        SettingValue::Boolean(false)
    ));
    let company_changed = write_values(
        &company_env,
        &mut tx,
        company_scope,
        "0".into(),
        vec![("base.test_company".into(), Value::Bool(true))],
    )
    .await
    .unwrap();
    assert_eq!(company_changed.revision, "1");
    assert_eq!(
        read_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            vec!["base.test_enabled".into()]
        )
        .await
        .unwrap()
        .revision,
        "1"
    );
    let direct = env
        .records("ir.config.setting.audit")
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap_err();
    assert_eq!(direct.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        direct.message(),
        "Settings storage can only be changed by the audited Settings service."
    );
    tx.execute("UPDATE ir_config_setting_value SET value_text='123'", &[])
        .await
        .unwrap();
    let corrupt = read_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        vec!["base.test_enabled".into()],
    )
    .await
    .unwrap_err();
    assert_eq!(corrupt.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        corrupt.message(),
        "Stored setting base.test_enabled is invalid."
    );
    // Safety-admission regression: corrupt storage must be rejected before its oversized
    // text crosses the PostgreSQL boundary, while the present key must never become a default.
    let key = "base.test_enabled".to_owned();
    tx.execute(
        "UPDATE ir_config_setting_value SET value_text=repeat('x',$1) WHERE setting_key=$2",
        &[&400_001_i32, &key],
    )
    .await
    .unwrap();
    tx.capture_statements(true);
    let oversized = read_values(&env, &mut tx, SettingsScope::Global {}, vec![key.clone()])
        .await
        .unwrap_err();
    // Reuse the statement that the real service just executed, not a handwritten lookalike.
    // Inspect its actual wire row without allocating or decoding the corrupt text as a String.
    let production_read = tx
        .captured()
        .iter()
        .find(|sql| sql.starts_with("SELECT COALESCE(s.revision,'0'),v.setting_key,"))
        .unwrap()
        .clone();
    tx.capture_statements(false);
    assert_eq!(oversized.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        oversized.message(),
        "Stored setting base.test_enabled is invalid."
    );
    let rows = tx
        .query(
            &production_read,
            &[&"global", &vec![key.clone()], &"base.attachment_max_bytes"],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, Option<&str>>(1), Some(key.as_str()));
    assert!(
        rows[0].get::<_, Option<&str>>(2).is_none(),
        "oversized stored text must be withheld by PostgreSQL before transfer"
    );
    tx.rollback().await.unwrap();
    // Independent transactions/connections serialize against the same durable scope identity.
    let writer = async |value| {
        let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        let env = tx.env();
        let result = write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            "1".into(),
            vec![("base.test_enabled".into(), Value::Bool(value))],
        )
        .await;
        if result.is_ok() {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
        result
    };
    let (first, second) = tokio::join!(writer(false), writer(true));
    let results = [first, second];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let error = results.into_iter().find_map(Result::err).unwrap();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Conflict, "{error:?}");
    assert_eq!(error.message(), "Settings changed; reload before saving.");
    drop(initialized);
    drop(system);
    db.cleanup().await.unwrap();
}
