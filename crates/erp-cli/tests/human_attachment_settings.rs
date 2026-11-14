// SPDX-License-Identifier: MIT OR Apache-2.0
//! Settings and Technical edit the same attachment policy in a real installed database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_addon_base::ir_config_setting::{read_values, write_values};
use erp_sdk::views::wire::{SettingValue, SettingsScope};
use erp_sdk::{ErrorKind, Value};

const KEY: &str = "base.attachment_max_bytes";

#[test]
fn attachment_policy_settings_and_technical_share_storage_and_revision() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(walk());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn walk() {
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
    let identities = index
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
        .native_artifact_identities(identities)
        .code([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let initialized = system
        .initialize(erp_modules::system::InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let mut tx = initialized.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let scope = SettingsScope::Global {};
    let initial = read_values(&env, &mut tx, scope.clone(), vec![KEY.into()])
        .await
        .unwrap();
    assert!(matches!(
        initial.values[0].value,
        SettingValue::Integer(134217728)
    ));
    let saved = write_values(
        &env,
        &mut tx,
        scope.clone(),
        initial.revision,
        vec![(KEY.into(), Value::Int(1024))],
    )
    .await
    .unwrap();
    let immediate = read_values(&env, &mut tx, scope.clone(), vec![KEY.into()])
        .await
        .unwrap();
    assert!(matches!(
        immediate.values[0].value,
        SettingValue::Integer(1024)
    ));
    let params = env.records("ir.config_parameter").unwrap();
    let technical: i64 = params
        .call(&mut tx, "get_int", (KEY.to_owned(), None::<i64>))
        .await
        .unwrap();
    assert_eq!(technical, 1024);
    let _: String = params
        .call(
            &mut tx,
            "set_str",
            (KEY.to_owned(), Some("2048".to_owned())),
        )
        .await
        .unwrap();
    let read = read_values(&env, &mut tx, scope.clone(), vec![KEY.into()])
        .await
        .unwrap();
    assert!(matches!(read.values[0].value, SettingValue::Integer(2048)));
    assert_ne!(read.revision, saved.revision);
    let stale = write_values(
        &env,
        &mut tx,
        scope.clone(),
        saved.revision,
        vec![(KEY.into(), Value::Int(4096))],
    )
    .await
    .unwrap_err();
    assert_eq!(stale.kind(), ErrorKind::Conflict);
    assert_eq!(stale.message(), "Settings changed; reload before saving.");
    let rows = tx.query("SELECT old_value,new_value FROM ir_config_setting_audit WHERE setting_key=$1 ORDER BY id", &[&KEY]).await.unwrap();
    let last = rows.last().unwrap();
    assert_eq!(last.get::<_, String>(0), "1024");
    assert_eq!(last.get::<_, String>(1), "2048");
    let count: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM ir_config_setting_value WHERE setting_key=$1",
            &[&KEY],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    // Invalid Technical edits leave value, revision and audit unchanged.
    // Model-method Validation errors poison their transaction. A caller catching
    // one must own an outer savepoint; the bridge's internal atomicity does not change that rule.
    let error = tx
        .savepoint(async |tx| {
            params
                .call::<_, String>(tx, "set_str", (KEY.to_owned(), Some("0".to_owned())))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "base.attachment_max_bytes must be an integer from 1 to 2147483647."
    );
    // The Settings route refuses with the declaration's own sentence, for zero and above the hard bound.
    for bad in [0, 2_147_483_648] {
        let error = tx
            .savepoint(async |tx| {
                write_values(
                    &env,
                    tx,
                    scope.clone(),
                    read.revision.clone(),
                    vec![(KEY.into(), Value::Int(bad))],
                )
                .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "base.attachment_max_bytes takes a whole number from 1 to 2147483647."
        );
    }
    let unchanged = read_values(&env, &mut tx, scope.clone(), vec![KEY.into()])
        .await
        .unwrap();
    assert_eq!(unchanged.revision, read.revision);
    assert!(matches!(
        unchanged.values[0].value,
        SettingValue::Integer(2048)
    ));
    let _: String = params
        .call(&mut tx, "set_str", (KEY.to_owned(), None::<String>))
        .await
        .unwrap();
    let reset = read_values(&env, &mut tx, scope.clone(), vec![KEY.into()])
        .await
        .unwrap();
    assert!(matches!(
        reset.values[0].value,
        SettingValue::Integer(134217728)
    ));
    let parameter_id: i64 = tx
        .query(
            "SELECT id::int8 FROM ir_config_parameter WHERE key=$1",
            &[&KEY],
        )
        .await
        .unwrap()[0]
        .get(0);
    let parameter = params.browse([parameter_id]).unwrap();
    let error = tx
        .savepoint(async |tx| {
            parameter
                .write(
                    tx,
                    erp_sdk::RawVals::new()
                        .set_cell("key", erp_sdk::Cell::Str("renamed.policy".into())),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "You cannot rename config parameters with keys base.attachment_max_bytes"
    );
    let error = tx
        .savepoint(async |tx| parameter.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "You cannot delete the base.attachment_max_bytes record."
    );
    tx.commit().await.unwrap();
    drop(tx);

    // A Technical edit committed in a different context invalidates a previously observed CAS.
    let mut reader = initialized.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let reader_env = reader.env();
    let before = read_values(&reader_env, &mut reader, scope.clone(), vec![KEY.into()])
        .await
        .unwrap();
    reader.rollback().await.unwrap();
    let mut writer = initialized.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let _: String = writer
        .env()
        .records("ir.config_parameter")
        .unwrap()
        .call(
            &mut writer,
            "set_str",
            (KEY.to_owned(), Some("4096".to_owned())),
        )
        .await
        .unwrap();
    writer.commit().await.unwrap();
    let mut reader = initialized.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let reader_env = reader.env();
    let error = write_values(
        &reader_env,
        &mut reader,
        scope,
        before.revision,
        vec![(KEY.into(), Value::Int(8192))],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(error.message(), "Settings changed; reload before saving.");
    reader.rollback().await.unwrap();
    drop(reader);
    // Exercise an overlapping Technical writer and Settings CAS writer, not just two
    // sequential requests. Prove PostgreSQL has blocked the second writer on the first.
    let mut technical = initialized.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let technical_pid: i32 = technical
        .query("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()[0]
        .get(0);
    let _: String = technical
        .env()
        .records("ir.config_parameter")
        .unwrap()
        .call(
            &mut technical,
            "set_str",
            (KEY.to_owned(), Some("8192".to_owned())),
        )
        .await
        .unwrap();
    let mut settings = initialized.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let settings_env = settings.env();
    let settings_pid: i32 = settings
        .query("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()[0]
        .get(0);
    let stale_revision = read_values(
        &settings_env,
        &mut settings,
        SettingsScope::Global {},
        vec![KEY.into()],
    )
    .await
    .unwrap()
    .revision;
    let observer_pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let observer = observer_pool.get().await.unwrap();
    let (refused, ()) = tokio::join!(
        async {
            write_values(
                &settings_env,
                &mut settings,
                SettingsScope::Global {},
                stale_revision,
                vec![(KEY.into(), Value::Int(16384))],
            )
            .await
        },
        async {
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    let blocked: bool = observer
                        .query_one(
                            "SELECT $1::int4=ANY(pg_blocking_pids($2::int4))",
                            &[&technical_pid, &settings_pid],
                        )
                        .await
                        .unwrap()
                        .get(0);
                    if blocked {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("Settings writer must block on the Technical writer's scope lock");
            technical.commit().await.unwrap();
        }
    );
    let error = refused.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(error.message(), "Settings changed; reload before saving.");
    settings.rollback().await.unwrap();
    drop(settings);
    drop(technical);
    let mut verify = initialized.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let _: String = verify
        .env()
        .records("ir.config_parameter")
        .unwrap()
        .call(
            &mut verify,
            "set_str",
            (KEY.to_owned(), Some("4096".to_owned())),
        )
        .await
        .unwrap();
    verify.commit().await.unwrap();
    drop(verify);
    drop(observer);
    drop(observer_pool);
    reader_policy(&initialized.orm, &db).await;
    drop(initialized);
    drop(system);
    db.cleanup().await.unwrap();
}

async fn reader_policy(orm: &erp_orm::Orm, db: &erp_testkit::TestDb) {
    use erp_sdk::{Cell, Command, RawVals, RecId};
    use std::sync::Arc;
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let company: i64 = tx
        .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()[0]
        .get(0);
    let group: i64 = tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_user' AND model='res.groups'", &[]).await.unwrap()[0].get(0);
    // The initialized profile has no active administrator yet; preserve the users invariant
    // before creating the ordinary reader, as the existing Settings integration fixture does.
    let admin_group: i64 = tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_system' AND model='res.groups'", &[]).await.unwrap()[0].get(0);
    tx.env()
        .records("res.users")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Policy administrator".into()))
                    .set_cell("login", Cell::Str("attachment-policy-admin".into()))
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
    let uid = tx
        .env()
        .records("res.users")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Attachment policy reader".into()))
                    .set_cell("login", Cell::Str("attachment-policy-reader".into()))
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
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    let registry = Arc::clone(orm.registry());
    let policy =
        erp_security::source::PolicySource::build(Arc::clone(&registry), db.pool().clone())
            .await
            .unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(
        erp_sdk::blob::LocalStore::open(root.path().join("store"))
            .await
            .unwrap(),
    );
    let runtime = Arc::new(erp_sdk::blob::AttachmentRuntime::single(
        "main",
        store,
        erp_sdk::blob::AttachmentLimits::default(),
    ));
    let users = erp_orm::Orm::builder(Arc::clone(&registry), db.pool().clone())
        .resolver(erp_security::resolver::Resolver::for_registry(&registry).unwrap())
        .policy(Arc::new(policy) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .generation_attachment(erp_orm::GenerationAttachment::new(registry, runtime))
        .build()
        .unwrap();
    let mut tx = users
        .begin(erp_orm::UserId::new(uid).unwrap())
        .await
        .unwrap();
    let env = tx.env();
    let value: Value = env
        .records("ir.attachment")
        .unwrap()
        .call(&mut tx, "erp_upload_limits", ())
        .await
        .unwrap();
    let Value::Map(value) = value else {
        panic!("policy object required")
    };
    assert_eq!(value.len(), 2);
    assert!(matches!(
        value.get("max_file_bytes"),
        Some(Value::Int(4096))
    ));
    assert!(matches!(
        value.get("inline_max_bytes"),
        Some(Value::Int(2048))
    ));
    let denied = read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::Access);
    assert_eq!(
        denied.message(),
        format!("Setting {KEY} is unavailable or access is denied.")
    );
    // A caller outside the Settings administrators gets the access refusal, never the bounds.
    let denied = write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        "0".into(),
        vec![(KEY.into(), Value::Int(0))],
    )
    .await
    .unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::Access);
    assert_eq!(
        denied.message(),
        format!("Setting {KEY} is unavailable or access is denied.")
    );
    let denied = env
        .records("ir.config_parameter")
        .unwrap()
        .call::<_, String>(&mut tx, "get_str", (KEY.to_owned(), None::<String>))
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::Access);
    // The middle lists administrator-editable group names; assert both stable message boundaries.
    assert!(denied.message().starts_with(
        "Your access rights do not let you access 'System Parameter' (ir.config_parameter) records.\n\n"
    ));
    assert!(
        denied
            .message()
            .ends_with("\n\nAsk an administrator if you need this access.")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(users);
}
