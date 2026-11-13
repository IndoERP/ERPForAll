// SPDX-License-Identifier: MIT OR Apache-2.0
//! Shared jobs policy through the installed Settings service and real PostgreSQL transactions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    reason = "integration assertions"
)]
use erp_addon_base::ir_config_setting::{describe, read_values, write_values};
use erp_sdk::prelude::*;
use erp_sdk::views::wire::{SettingValue, SettingsScope};
use erp_sdk::{ErrorKind, SUPERUSER_ID};

fn integer(value: &SettingValue) -> i64 {
    match value {
        SettingValue::Integer(value) => *value,
        other => panic!("not an integer: {other:?}"),
    }
}
fn refused(error: Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message);
}
#[test]
fn shared_jobs_settings_are_validated_and_audited_atomically() {
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
        .view_types(erp_addon_web::view_type_contributions())
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_web::ADDON,
        ])
        .build()
        .unwrap();
    let initialized = system
        .initialize(erp_modules::system::InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(
            &system.system_env(erp_orm::Context::new()),
            &[erp_modules::ModuleName::new("web").unwrap()],
        )
        .await
        .unwrap();
    let orm = &installed.orm;

    let scope = SettingsScope::Global {};
    // Exercise an ordinary Settings administrator as well as direct internal-user refusal.
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let company: i64 = setup
        .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()[0]
        .get(0);
    let mut user_ids = Vec::new();
    for (login, group) in [
        ("jobs-settings-admin", "group_system"),
        ("jobs-settings-user", "group_user"),
    ] {
        let group_id: i64 = setup.query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name=$1 AND model='res.groups'", &[&group]).await.unwrap()[0].get(0);
        let records = setup
            .env()
            .records("res.users")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(login.into()))
                        .set_cell("login", Cell::Str(login.into()))
                        .set_cell("active", Cell::Bool(true))
                        .set_cell("company_id", Cell::Id(company))
                        .set_commands(
                            "company_ids",
                            [Command::set([RecId::from_real(company).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set([RecId::from_real(group_id).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        user_ids.push(records.ids()[0]);
    }
    setup.commit().await.unwrap();
    drop(setup);
    // Lifecycle ORMs intentionally lack user policy; construct the same caller-bound runtime adapter as the served stack.
    let policy =
        erp_security::source::PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
            .await
            .unwrap();
    let runtime_orm = erp_orm::Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(orm.registry()),
            orm.env(SUPERUSER_ID, erp_orm::Context::new())
                .generation_attachment::<erp_sdk::views::PublishedViewTypes>()
                .unwrap(),
        ))
        .resolver(erp_security::resolver::Resolver::for_registry(orm.registry()).unwrap())
        .policy(Arc::new(policy) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap();
    let orm = &runtime_orm;
    let admin = erp_orm::UserId::new(user_ids[0]).unwrap();
    let internal = erp_orm::UserId::new(user_ids[1]).unwrap();
    let mut denied_tx = orm.begin(internal).await.unwrap();
    let denied_env = denied_tx.env();
    let error = read_values(
        &denied_env,
        &mut denied_tx,
        scope.clone(),
        vec!["base.jobs.history_days".into()],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Setting is unavailable or access is denied."
    );
    denied_tx.rollback().await.unwrap();
    drop(denied_tx);
    let base = "base.jobs.backoff_base_secs";
    let cap = "base.jobs.backoff_cap_secs";
    let mut tx = orm.begin(admin).await.unwrap();
    let env = tx.env();
    let description = describe(&env, &mut tx, scope.clone()).await.unwrap();
    assert!(description.items.iter().any(|item| matches!(item,
        erp_sdk::views::wire::SettingsItem::Info { key, detail: Some(detail), .. }
            if key == "base.jobs_configuration_status" && detail == "Values saved here are stored configuration. Running jobs use process configuration until live settings integration is delivered."
    )));
    // Serve the shipped primary service view through the exposed producer at desktop and phone options.
    let view_id: i64 = tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='technical_jobs_settings_form' AND model='ir.ui.view'", &[]).await.unwrap()[0].get(0);
    for mobile in [false, true] {
        let records = env.records("ir.config.setting").unwrap();
        let erp_orm::rpc::Exposure::Exposed(method) = tx
            .exposure()
            .get(records.model_id(), "get_views")
            .unwrap()
            .clone()
        else {
            panic!("get_views unavailable");
        };
        let args = erp_fields::ValueMap::from_iter([
            (
                "views".into(),
                Value::List(
                    vec![Value::List(
                        vec![Value::Int(view_id), Value::str("form")].into(),
                    )]
                    .into(),
                ),
            ),
            ("wire_major".into(), Value::Int(1)),
            (
                "options".into(),
                Value::Map(Arc::new(erp_fields::ValueMap::from_iter([(
                    "mobile".into(),
                    Value::Bool(mobile),
                )]))),
            ),
        ]);
        let served = method
            .call(&mut tx, records, erp_orm::rpc::RpcArgs::new(args))
            .await
            .unwrap();
        let served = erp_fields::convert::value_to_json(&served).unwrap();
        let nodes = served["views"][0]["nodes"].as_array().unwrap();
        let widget = nodes
            .iter()
            .find(|node| node["widget"] == "web.settings")
            .unwrap();
        assert_eq!(
            widget["options"]["scope_kinds"],
            erp_sdk::serde_json::json!(["global"])
        );
        assert_eq!(widget["options"]["app"], "base");
    }
    let jobs: Vec<_> = description
        .declarations
        .iter()
        .filter(|d| d.key.starts_with("base.jobs."))
        .collect();
    let shared: Vec<_> = erp_orm::jobs::Setting::ALL
        .iter()
        .copied()
        .filter(|s| {
            !matches!(
                s,
                erp_orm::jobs::Setting::Workers
                    | erp_orm::jobs::Setting::WorkersPerDb
                    | erp_orm::jobs::Setting::ClaimBatch
            )
        })
        .collect();
    assert_eq!(jobs.len(), shared.len());
    let job_keys = jobs.iter().map(|decl| decl.key.clone()).collect();
    let fixture_values = read_values(&env, &mut tx, scope.clone(), job_keys)
        .await
        .unwrap();
    let mut fixture_description = erp_sdk::serde_json::to_value(&description).unwrap();
    fixture_description["declarations"]
        .as_array_mut()
        .unwrap()
        .retain(|decl| decl["key"].as_str().unwrap().starts_with("base.jobs."));
    fixture_description["items"]
        .as_array_mut()
        .unwrap()
        .retain(|item| item["key"] == "base.jobs_configuration_status");
    std::fs::create_dir_all("logs").unwrap();
    std::fs::write("logs/jobs-settings-fixture.json", erp_sdk::serde_json::to_vec(&erp_sdk::serde_json::json!({"description": fixture_description, "values": fixture_values})).unwrap()).unwrap();
    for setting in shared {
        let key = format!("base.{}", setting.name());
        let decl = jobs.iter().find(|d| d.key == key).unwrap();
        assert_eq!(
            integer(&decl.default),
            i64::try_from(setting.default_value()).unwrap()
        );
        assert_eq!(decl.block, "Background Jobs");
    }
    for key in [
        "base.jobs.workers",
        "base.jobs.workers_per_db",
        "base.jobs.claim_batch",
        "base.jobs.run_in_app",
    ] {
        let error = read_values(&env, &mut tx, scope.clone(), vec![key.into()])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Setting is unavailable or access is denied."
        );
    }
    let error = read_values(
        &env,
        &mut tx,
        SettingsScope::Company {
            company_id: erp_sdk::views::wire::WireId::try_from(u64::try_from(company).unwrap())
                .unwrap(),
        },
        vec![base.into()],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Setting is unavailable or access is denied."
    );
    let defaults = read_values(&env, &mut tx, scope.clone(), vec![base.into(), cap.into()])
        .await
        .unwrap();
    assert_eq!(defaults.revision, "0");
    assert_eq!(integer(&defaults.values[0].value), 10);
    assert_eq!(integer(&defaults.values[1].value), 3600);
    refused(
        write_values(
            &env,
            &mut tx,
            scope.clone(),
            "0".into(),
            vec![(base.into(), Value::Bool(true))],
        )
        .await
        .unwrap_err(),
        "Invalid value for base.jobs.backoff_base_secs; use the declared integer range.",
    );
    // Individually admissible, invalid against an unchanged default base. No prefix survives even when caught.
    refused(
        write_values(
            &env,
            &mut tx,
            scope.clone(),
            "0".into(),
            vec![(cap.into(), Value::Int(5))],
        )
        .await
        .unwrap_err(),
        "base.jobs.backoff_cap_secs = 5 is outside 10..=86400 (base.jobs.backoff_base_secs)",
    );
    let untouched = tx
        .query("SELECT count(*)::int8 FROM ir_config_setting_audit", &[])
        .await
        .unwrap();
    assert_eq!(untouched[0].get::<_, i64>(0), 0);
    let saved = write_values(
        &env,
        &mut tx,
        scope.clone(),
        "0".into(),
        vec![(base.into(), Value::Int(20)), (cap.into(), Value::Int(20))],
    )
    .await
    .unwrap();
    assert_eq!(saved.revision, "1");
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(admin).await.unwrap();
    let env = tx.env();
    let read = read_values(&env, &mut tx, scope.clone(), vec![base.into(), cap.into()])
        .await
        .unwrap();
    assert_eq!(integer(&read.values[0].value), 20);
    assert_eq!(integer(&read.values[1].value), 20);
    // Reset one member to its declared default is checked against the resulting group.
    write_values(
        &env,
        &mut tx,
        scope.clone(),
        "1".into(),
        vec![(base.into(), Value::Int(10))],
    )
    .await
    .unwrap();
    let audit = tx.query("SELECT setting_key,old_value,new_value,revision FROM ir_config_setting_audit ORDER BY id", &[]).await.unwrap();
    assert_eq!(audit.len(), 3);
    assert_eq!(audit[2].get::<_, String>(0), base);
    assert_eq!(audit[2].get::<_, String>(1), "20");
    assert_eq!(audit[2].get::<_, String>(2), "10");
    assert_eq!(audit[2].get::<_, String>(3), "2");
    tx.rollback().await.unwrap();
    drop(tx);
    // Restoring a default can be invalid against a coupled saved value; no partial reset persists.
    let mut reset = orm.begin(admin).await.unwrap();
    let reset_env = reset.env();
    write_values(
        &reset_env,
        &mut reset,
        scope.clone(),
        "1".into(),
        vec![(base.into(), Value::Int(1)), (cap.into(), Value::Int(1))],
    )
    .await
    .unwrap();
    refused(
        write_values(
            &reset_env,
            &mut reset,
            scope.clone(),
            "2".into(),
            vec![(base.into(), Value::Int(10))],
        )
        .await
        .unwrap_err(),
        "base.jobs.backoff_cap_secs = 1 is outside 10..=86400 (base.jobs.backoff_base_secs)",
    );
    reset.rollback().await.unwrap();
    drop(reset);
    // An audit INSERT failure rolls back the preceding value write and the scope revision.
    let mut fault = orm.begin(SUPERUSER_ID).await.unwrap();
    let fault_env = fault.env();
    fault.execute("ALTER TABLE ir_config_setting_audit ADD CONSTRAINT fixture_reject_jobs_audit CHECK (revision <> '2') NOT VALID", &[]).await.unwrap();
    let error = write_values(
        &fault_env,
        &mut fault,
        scope.clone(),
        "1".into(),
        vec![(base.into(), Value::Int(15))],
    )
    .await
    .unwrap_err();
    assert_eq!(error.sqlstate(), Some("23514"));
    refused(
        error,
        "The operation cannot be completed: new row for relation \"ir_config_setting_audit\" violates check constraint \"fixture_reject_jobs_audit\"",
    );
    let unchanged = read_values(&fault_env, &mut fault, scope.clone(), vec![base.into()])
        .await
        .unwrap();
    assert_eq!(unchanged.revision, "1");
    assert_eq!(integer(&unchanged.values[0].value), 20);
    assert_eq!(
        fault
            .query("SELECT count(*)::int8 FROM ir_config_setting_audit", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        2
    );
    fault.rollback().await.unwrap();
    drop(fault);
    // Invalid timeout equality in either class refuses the entire batch, including another valid key.
    for class in ["default", "maintenance"] {
        let lock = format!("base.jobs.chunk.{class}.lock_ms");
        let statement = format!("base.jobs.chunk.{class}.statement_ms");
        let message = format!("{lock} = 5000 must be below {statement} = 5000");
        let mut tx = orm.begin(admin).await.unwrap();
        let env = tx.env();
        refused(
            write_values(
                &env,
                &mut tx,
                scope.clone(),
                "1".into(),
                vec![
                    (statement, Value::Int(5000)),
                    ("base.jobs.history_days".into(), Value::Int(9)),
                ],
            )
            .await
            .unwrap_err(),
            &message,
        );
        let read = read_values(
            &env,
            &mut tx,
            scope.clone(),
            vec!["base.jobs.history_days".into()],
        )
        .await
        .unwrap();
        assert_eq!(read.revision, "1");
        assert_eq!(integer(&read.values[0].value), 7);
        tx.commit().await.unwrap();
        drop(tx);
    }
    // Corrupt existing values never turn into defaults, even if a writer proposes to replace them.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    tx.execute(
        "UPDATE ir_config_setting_value SET value_text='false' WHERE setting_key=$1",
        &[&base],
    )
    .await
    .unwrap();
    refused(
        read_values(&env, &mut tx, scope.clone(), vec![cap.into()])
            .await
            .unwrap_err(),
        "Stored setting base.jobs.backoff_base_secs is invalid.",
    );
    refused(
        write_values(
            &env,
            &mut tx,
            scope.clone(),
            "1".into(),
            vec![(base.into(), Value::Int(10))],
        )
        .await
        .unwrap_err(),
        "Stored setting base.jobs.backoff_base_secs is invalid.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let writer = async |value| {
        let mut tx = orm.begin(admin).await.unwrap();
        let env = tx.env();
        let result = write_values(
            &env,
            &mut tx,
            scope.clone(),
            "1".into(),
            vec![(base.into(), Value::Int(value))],
        )
        .await;
        if result.is_ok() {
            tx.commit().await.unwrap();
            drop(tx);
        } else {
            tx.rollback().await.unwrap();
            drop(tx);
        }
        result
    };
    let (first, second) = tokio::join!(writer(15), writer(16));
    assert_eq!([&first, &second].iter().filter(|v| v.is_ok()).count(), 1);
    let error = [first, second].into_iter().find_map(Result::err).unwrap();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(error.message(), "Settings changed; reload before saving.");
    // Independently constructed storage readers see the committed winner; another tenant keeps defaults.
    let independent = erp_orm::Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap();
    let mut observed = independent.begin(SUPERUSER_ID).await.unwrap();
    let observed_env = observed.env();
    let committed = read_values(
        &observed_env,
        &mut observed,
        scope.clone(),
        vec![base.into()],
    )
    .await
    .unwrap();
    assert_eq!(committed.revision, "2");
    assert!([15, 16].contains(&integer(&committed.values[0].value)));
    observed.rollback().await.unwrap();
    drop(observed);
    drop(independent);

    let second_db = erp_testkit::TestDb::create().await.unwrap();
    let mut second_index = erp_modules::AddonIndex::new(true);
    second_index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    let identities = second_index
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
    let second_system =
        erp_modules::system::ModuleSystem::builder(second_db.pool().clone(), second_index)
            .native_artifact_identities(identities)
            .code([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
            .build()
            .unwrap();
    let second_init = second_system
        .initialize(erp_modules::system::InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let mut second_tx = second_init.orm.begin(SUPERUSER_ID).await.unwrap();
    let second_env = second_tx.env();
    let isolated = read_values(
        &second_env,
        &mut second_tx,
        scope.clone(),
        vec![base.into()],
    )
    .await
    .unwrap();
    assert_eq!(isolated.revision, "0");
    assert_eq!(
        integer(&isolated.values[0].value),
        i64::try_from(erp_orm::jobs::Setting::BackoffBaseSecs.default_value()).unwrap()
    );
    second_tx.rollback().await.unwrap();
    drop(second_tx);
    drop(second_init);
    drop(second_system);
    second_db.cleanup().await.unwrap();
    drop(runtime_orm);
    drop(installed);
    drop(initialized);
    drop(system);
    db.cleanup().await.unwrap();
}
