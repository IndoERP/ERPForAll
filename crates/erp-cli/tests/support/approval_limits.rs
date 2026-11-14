// SPDX-License-Identifier: MIT OR Apache-2.0
//! Operating-limit regressions share the installed policy fixture and its real access engine.
use super::*;
use erp_addon_base::ir_config_setting::{describe, read_values, write_values};
use erp_sdk::views::wire::{SettingValue, SettingsScope};

const KEY: &str = "base.approval_max_approvers";
fn integer(v: &SettingValue) -> i64 {
    match v {
        SettingValue::Integer(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

const RANGE: &str = "base.approval_max_approvers takes a whole number from 1 to 64.";

#[test]
fn lowering_conceals_unreadable_active_revisions() {
    run(|| async {
        let mut f = fixture().await;
        let second = f.company("Hidden policy company").await;
        let admin = f
            .user("scope-admin", &[f.main], &[f.group_user, f.group_system])
            .await;
        let other = f
            .user("scope-other", &[second], &[f.group_user, f.group_system])
            .await;
        let a = f.user("scope-a", &[second], &[f.group_user]).await;
        let b = f.user("scope-b", &[second], &[f.group_user]).await;
        f.refresh().await;
        f.create_unfenced(
            other,
            approvers(
                select(
                    policy("foreign-confidential", "write", "approve"),
                    Some(second),
                    None,
                    None,
                ),
                &[a, b],
                &[],
            ),
        )
        .await
        .unwrap();
        let mut tx = f.begin(admin).await;
        let env = tx.env();
        let error = write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            "0".into(),
            vec![(KEY.into(), Value::Int(1))],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "base.approval_max_approvers cannot be lowered: an active approval revision is unavailable or access is denied."
        );
        assert!(std::error::Error::source(&error).is_none());
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn database_read_failure_never_becomes_the_default() {
    run(|| async {
        let f = fixture().await;
        let id = f
            .record(
                MODEL,
                policy("read-fault", "write", "approve").set_cell("active", Cell::Bool(false)),
            )
            .await;
        let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
        tx.batch("ALTER TABLE ir_config_setting_value RENAME TO hidden_setting_value")
            .await
            .unwrap();
        let error = eligible_approvers(&mut tx, revision(id), &[], &[company_id(f.main)])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database);
        assert_eq!(error.sqlstate(), Some("42P01"));
        // Driver context/statement formatting is version-dependent; SQLSTATE names the fault.
        assert!(!error.message().is_empty());
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn lowering_reports_expanded_groups_and_first_ten_revisions() {
    run(|| async {
        let mut f = fixture().await;
        let admin = f
            .user("report-admin", &[f.main], &[f.group_user, f.group_system])
            .await;
        let group = f
            .record(
                "res.groups",
                RawVals::new().set_cell("name", Cell::Str("Report approvers".into())),
            )
            .await;
        f.user("report-a", &[f.main], &[f.group_user, group]).await;
        f.user("report-b", &[f.main], &[f.group_user, group]).await;
        f.refresh().await;
        let mut named = Vec::new();
        for (i, model) in [
            "res.partner",
            "res.company",
            "res.users",
            "res.groups",
            "ir.ui.view",
            "ir.ui.menu",
            "ir.config_parameter",
            "ir.config.setting",
            "ir.actions.act_window",
            "ir.model.data",
            "ir.module.module",
        ]
        .iter()
        .enumerate()
        {
            let name = format!("report-{i}");
            let id = f
                .create_unfenced(
                    admin,
                    approvers(
                        select(policy(&name, "write", "approve"), None, Some(model), None),
                        &[],
                        &[group],
                    ),
                )
                .await
                .unwrap();
            named.push((id, name));
        }
        let mut tx = f.begin(admin).await;
        let env = tx.env();
        let labels = named
            .iter()
            .take(10)
            .map(|(id, name)| format!("'{name}' (id {id})"))
            .collect::<Vec<_>>()
            .join(", ");
        validation(
            &write_values(
                &env,
                &mut tx,
                SettingsScope::Global {},
                "0".into(),
                vec![(KEY.into(), Value::Int(1))],
            )
            .await
            .unwrap_err(),
            &format!(
                "base.approval_max_approvers cannot be set to 1: active approval revisions exceed this limit: {labels}; and 1 more. Archive or replace these revisions first."
            ),
        );
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn policy_and_settings_writers_conflict_on_stale_snapshots() {
    run(|| async {
        let mut f = fixture().await;
        let admin = f
            .user("race-admin", &[f.main], &[f.group_user, f.group_system])
            .await;
        let a = f.user("race-a", &[f.main], &[f.group_user]).await;
        let b = f.user("race-b", &[f.main], &[f.group_user]).await;
        f.refresh().await;
        // Agent revisions are configurable but not resolved; unlike User revisions they do not
        // hit the delivery fence, so these are real ORM activations, without test-only SQL.
        let id = f
            .create(
                admin,
                approvers(
                    policy("race", "write", "approve")
                        .set_cell("principal_kind", Cell::Str("agent".into()))
                        .set_cell("active", Cell::Bool(false)),
                    &[a, b],
                    &[],
                ),
            )
            .await
            .unwrap();
        let mut old_policy = f.begin(admin).await;
        old_policy
            .query(
                "SELECT revision FROM ir_config_setting_scope WHERE identity='global'",
                &[],
            )
            .await
            .unwrap();
        configured(&f, 1).await;
        let records = old_policy
            .env()
            .records(MODEL)
            .unwrap()
            .browse([id])
            .unwrap();
        let error = records
            .write(
                &mut old_policy,
                RawVals::new().set_cell("active", Cell::Bool(true)),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Concurrency);
        assert_eq!(error.sqlstate(), Some("40001"));
        // PostgreSQL driver detail/context varies by version; SQLSTATE fixes the error contract.
        assert!(!error.message().is_empty());
        old_policy.rollback().await.unwrap();
        drop(old_policy);
        configured(&f, 2).await;
        let mut old_settings = f.begin(admin).await;
        let env = old_settings.env();
        let before = read_values(
            &env,
            &mut old_settings,
            SettingsScope::Global {},
            vec![KEY.into()],
        )
        .await
        .unwrap();
        f.write(
            admin,
            id,
            RawVals::new().set_cell("active", Cell::Bool(true)),
        )
        .await
        .unwrap();
        let error = write_values(
            &env,
            &mut old_settings,
            SettingsScope::Global {},
            before.revision,
            vec![(KEY.into(), Value::Int(1))],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert_eq!(error.message(), "Settings changed; reload before saving.");
        old_settings.rollback().await.unwrap();
        drop(old_settings);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn lowering_below_active_revision_is_refused_before_audit() {
    run(|| async {
        let mut f = fixture().await;
        let admin = f
            .user("active-admin", &[f.main], &[f.group_user, f.group_system])
            .await;
        let a = f.user("active-a", &[f.main], &[f.group_user]).await;
        let b = f.user("active-b", &[f.main], &[f.group_user]).await;
        f.refresh().await;
        let id = f
            .create_unfenced(
                admin,
                approvers(policy("active-limit", "write", "approve"), &[a, b], &[]),
            )
            .await
            .unwrap();
        let mut tx = f.begin(admin).await;
        let env = tx.env();
        validation(
            &write_values(
                &env,
                &mut tx,
                SettingsScope::Global {},
                "0".into(),
                vec![(KEY.into(), Value::Int(1))],
            )
            .await
            .unwrap_err(),
            &format!(
                "base.approval_max_approvers cannot be set to 1: active approval revisions exceed this limit: 'active-limit' (id {id}). Archive or replace these revisions first."
            ),
        );
        let unchanged = read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
            .await
            .unwrap();
        assert_eq!(unchanged.revision, "0");
        assert_eq!(integer(&unchanged.values[0].value), 64);
        let audit: i64 = tx
            .query(
                "SELECT count(*) FROM ir_config_setting_audit WHERE setting_key=$1",
                &[&KEY],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(audit, 0);
        tx.commit().await.unwrap();
        drop(tx);
        f.archive(admin, id).await;
        configured(&f, 1).await;
        let mut tx = f.begin(admin).await;
        validation(
            &eligible_approvers(&mut tx, revision(id), &[], &[company_id(f.main)])
                .await
                .unwrap_err(),
            &format!(
                "Approval policy revision {id} names more than 1 approvers and groups (base.approval_max_approvers)."
            ),
        );
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

async fn configured(f: &Fixture, n: i64) {
    let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let old = read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
        .await
        .unwrap();
    write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        old.revision,
        vec![(KEY.into(), Value::Int(n))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
}

// Inject storage only to isolate a lowered-policy RED from the missing-declaration RED.
async fn inject(f: &Fixture, text: &str) {
    f.sql("INSERT INTO ir_config_setting_scope(identity, revision) VALUES ('global','0') ON CONFLICT(identity) DO NOTHING", &[]).await;
    f.sql("INSERT INTO ir_config_setting_value(scope_id,setting_key,value_text) SELECT id,$1,$2 FROM ir_config_setting_scope WHERE identity='global' ON CONFLICT(scope_id,setting_key) DO UPDATE SET value_text=EXCLUDED.value_text", &[&KEY, &text]).await;
}

#[test]
fn operating_setting_is_admin_only_bounded_and_audited() {
    run(|| async {
        let mut f = fixture().await;
        let admin = f
            .user("limit-admin", &[f.main], &[f.group_user, f.group_system])
            .await;
        let user = f.user("limit-user", &[f.main], &[f.group_user]).await;
        f.refresh().await;
        let mut tx = f.begin(admin).await;
        let env = tx.env();
        let description = describe(&env, &mut tx, SettingsScope::Global {})
            .await
            .unwrap();
        let decl = description
            .declarations
            .iter()
            .find(|d| d.key == KEY)
            .expect("approval limit declaration missing");
        assert_eq!(integer(&decl.default), 64);
        assert!(decl.writable);
        assert_eq!(decl.label, "Maximum approvers");
        let defaults = read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
            .await
            .unwrap();
        let mut fixture_description = serde_json::to_value(&description).unwrap();
        fixture_description["declarations"]
            .as_array_mut()
            .unwrap()
            .retain(|d| d["key"] == KEY);
        fixture_description["items"] = serde_json::json!([]);
        std::fs::create_dir_all("logs").unwrap();
        std::fs::write(
            "logs/approval-settings-fixture.json",
            serde_json::to_vec(
                &serde_json::json!({"description":fixture_description,"values":defaults}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(integer(&defaults.values[0].value), 64);
        let initial_revision = defaults.revision.clone();
        for bad in [
            Value::Int(0),
            Value::Int(-1),
            Value::Int(65),
            Value::Int(i64::MAX),
            Value::str("2"),
            Value::Bool(true),
            Value::False,
            Value::Float(1.5),
        ] {
            validation(
                &write_values(
                    &env,
                    &mut tx,
                    SettingsScope::Global {},
                    initial_revision.clone(),
                    vec![(KEY.into(), bad)],
                )
                .await
                .unwrap_err(),
                RANGE,
            );
        }
        let after = read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
            .await
            .unwrap();
        assert_eq!(after.revision, initial_revision);
        assert_eq!(
            serde_json::to_value(&after.values).unwrap(),
            serde_json::to_value(&defaults.values).unwrap()
        );
        let mut current = initial_revision.clone();
        for n in [1, 3, 64] {
            let saved = write_values(
                &env,
                &mut tx,
                SettingsScope::Global {},
                current,
                vec![(KEY.into(), Value::Int(n))],
            )
            .await
            .unwrap();
            assert_eq!(integer(&saved.values[0].value), n);
            current = saved.revision;
        }
        let stale = write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            initial_revision,
            vec![(KEY.into(), Value::Int(2))],
        )
        .await
        .unwrap_err();
        assert_eq!(stale.kind(), ErrorKind::Conflict);
        assert_eq!(stale.message(), "Settings changed; reload before saving.");
        let audit: i64 = tx
            .query(
                "SELECT count(*) FROM ir_config_setting_audit WHERE setting_key=$1",
                &[&KEY],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(audit, 3);
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = f.begin(admin).await;
        let env = tx.env();
        let after = read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
            .await
            .unwrap();
        assert_eq!(integer(&after.values[0].value), 64);
        assert_eq!(after.revision, "0");
        let audit: i64 = tx
            .query(
                "SELECT count(*) FROM ir_config_setting_audit WHERE setting_key=$1",
                &[&KEY],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(audit, 0);
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = f.begin(user).await;
        let env = tx.env();
        assert!(
            describe(&env, &mut tx, SettingsScope::Global {})
                .await
                .unwrap()
                .declarations
                .iter()
                .all(|d| d.key != KEY)
        );
        for writing in [false, true] {
            let error = if writing {
                write_values(
                    &env,
                    &mut tx,
                    SettingsScope::Global {},
                    "0".into(),
                    vec![(KEY.into(), Value::Int(1))],
                )
                .await
                .unwrap_err()
            } else {
                read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
                    .await
                    .unwrap_err()
            };
            assert_eq!(error.kind(), ErrorKind::Access);
            // Service write access is checked before declaration access.
            assert_eq!(
                error.message(),
                format!("Setting {KEY} is unavailable or access is denied.")
            );
        }
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
        let error = tx
            .env()
            .records("ir.config.setting.value")
            .unwrap()
            .create(&mut tx, vec![RawVals::new()])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Settings storage can only be changed by the audited Settings service."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        configured(&f, 2).await;
        let mut tx = f.begin(admin).await;
        let env = tx.env();
        assert_eq!(
            integer(
                &read_values(&env, &mut tx, SettingsScope::Global {}, vec![KEY.into()])
                    .await
                    .unwrap()
                    .values[0]
                    .value
            ),
            2
        );
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn operating_limit_is_shared_with_independent_processes() {
    run(|| async {
        let mut f = fixture().await;
        let admin = f
            .user("nodes-admin", &[f.main], &[f.group_user, f.group_system])
            .await;
        let a = f.user("nodes-a", &[f.main], &[f.group_user]).await;
        let b = f.user("nodes-b", &[f.main], &[f.group_user]).await;
        f.refresh().await;
        let id = f
            .create(
                admin,
                approvers(
                    policy("nodes", "write", "approve").set_cell("active", Cell::Bool(false)),
                    &[a, b],
                    &[],
                ),
            )
            .await
            .unwrap();
        let child = |limit: i64| {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "approval_limits::independent_process_reader",
                    "--nocapture",
                ])
                .env("APPROVAL_LIMIT_DB", f.db.name())
                .env("APPROVAL_LIMIT_POLICY", id.to_string())
                .env("APPROVAL_LIMIT_COMPANY", f.main.to_string())
                .env("APPROVAL_LIMIT_EXPECT", limit.to_string())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "child: {} {}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(
                String::from_utf8_lossy(&result.stdout).contains("approval-limit-reader-complete")
            );
        };
        child(64);
        // Establish a repeatable-read snapshot before another transaction commits the setting.
        let mut old = f.begin(admin).await;
        assert_eq!(
            eligible_approvers(&mut old, revision(id), &[], &[company_id(f.main)])
                .await
                .unwrap()
                .ids(),
            user_ids(&[a, b])
        );
        configured(&f, 1).await;
        child(1);
        let mut fresh = f.begin(admin).await;
        validation(
            &eligible_approvers(&mut fresh, revision(id), &[], &[company_id(f.main)])
                .await
                .unwrap_err(),
            &format!(
                "Approval policy revision {id} names more than 1 approvers and groups (base.approval_max_approvers)."
            ),
        );
        fresh.rollback().await.unwrap();
        drop(fresh);
        assert_eq!(
            eligible_approvers(&mut old, revision(id), &[], &[company_id(f.main)])
                .await
                .unwrap()
                .ids(),
            user_ids(&[a, b])
        );
        old.rollback().await.unwrap();
        drop(old);
        configured(&f, 2).await;
        child(2);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn independent_process_reader() {
    let Ok(database) = std::env::var("APPROVAL_LIMIT_DB") else {
        return;
    };
    run(|| async move {
        let mut config: tokio_postgres::Config = erp_testkit::database_url_from_env()
            .unwrap()
            .parse()
            .unwrap();
        config.dbname(&database);
        let pool = erp_orm::pool::create_pool(config, 4).unwrap();
        let mut index = AddonIndex::new(true);
        index.add_dev_path(repo_addons());
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
        let system = ModuleSystem::builder(pool, index)
            .native_artifact_identities(identities)
            .code([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .require_core_tables(erp_security::auth::core_tables())
            .build()
            .unwrap();
        let init = system.load().await.unwrap();
        let id: i64 = std::env::var("APPROVAL_LIMIT_POLICY")
            .unwrap()
            .parse()
            .unwrap();
        let company: i64 = std::env::var("APPROVAL_LIMIT_COMPANY")
            .unwrap()
            .parse()
            .unwrap();
        let limit: i64 = std::env::var("APPROVAL_LIMIT_EXPECT")
            .unwrap()
            .parse()
            .unwrap();
        let mut tx = init.orm.begin(SUPERUSER_ID).await.unwrap();
        let result = eligible_approvers(&mut tx, revision(id), &[], &[company_id(company)]).await;
        if limit == 1 {
            validation(
                &result.unwrap_err(),
                &format!(
                    "Approval policy revision {id} names more than 1 approvers and groups (base.approval_max_approvers)."
                ),
            );
        } else {
            assert_eq!(result.unwrap().ids().len(), 2);
        }
        tx.rollback().await.unwrap();
        drop(tx);
        println!("approval-limit-reader-complete");
    });
}

#[test]
fn lowered_limit_refuses_existing_named_and_expanded_sets() {
    run(|| async {
        let mut f = fixture().await;
        let admin = f
            .user("limit-admin", &[f.main], &[f.group_user, f.group_system])
            .await;
        let requester = f.user("limit-requester", &[f.main], &[f.group_user]).await;
        let group = f
            .record(
                "res.groups",
                RawVals::new().set_cell("name", Cell::Str("Limit approvers".into())),
            )
            .await;
        let a = f.user("limit-a", &[f.main], &[f.group_user, group]).await;
        let b = f.user("limit-b", &[f.main], &[f.group_user, group]).await;
        let c = f.user("limit-c", &[f.main], &[f.group_user]).await;
        f.refresh().await;
        let id = f
            .create_unfenced(
                admin,
                approvers(
                    select(
                        policy("named-limit", "write", "approve"),
                        None,
                        Some("res.partner"),
                        None,
                    ),
                    &[a, b],
                    &[],
                ),
            )
            .await
            .unwrap();
        inject(&f, "1").await;
        let mut tx = f.begin(requester).await;
        let error = eligible_approvers(
            &mut tx,
            revision(id),
            &user_ids(&[requester]),
            &[company_id(f.main)],
        )
        .await
        .unwrap_err();
        validation(
            &error,
            &format!(
                "Approval policy revision {id} names more than 1 approvers and groups (base.approval_max_approvers)."
            ),
        );
        let env = tx.env();
        let error = resolve(&env, &mut tx, &partner_write(f.main))
            .await
            .unwrap_err();
        validation(&error, erp_addon_base::ir_approval_policy::UNAPPROVABLE);
        assert!(std::error::Error::source(&error).is_none());
        tx.rollback().await.unwrap();
        drop(tx);
        f.archive(admin, id).await;
        let error = f
            .write(
                admin,
                id,
                RawVals::new().set_cell("active", Cell::Bool(true)),
            )
            .await
            .unwrap_err();
        validation(
            &error,
            &refused(
                id,
                "named-limit",
                "names more than 1 approvers and groups (base.approval_max_approvers).",
            ),
        );
        let error = f
            .create(
                admin,
                approvers(
                    policy("too-many", "write", "approve").set_cell("active", Cell::Bool(false)),
                    &[a],
                    &[group],
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        let before = "names more than 1 approvers and groups (base.approval_max_approvers).";
        let prefix = "Approval policy revision 'too-many' (id ";
        let suffix = error.message().strip_prefix(prefix).unwrap();
        let (allocated_id, reason) = suffix.split_once("): ").unwrap();
        assert!(allocated_id.parse::<i64>().unwrap() > id);
        assert_eq!(reason, before);
        inject(&f, "2").await;
        let mut tx = f.begin(requester).await;
        assert_eq!(
            eligible_approvers(
                &mut tx,
                revision(id),
                &user_ids(&[requester]),
                &[company_id(f.main)]
            )
            .await
            .unwrap()
            .ids(),
            user_ids(&[a, b])
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let grouped = f
            .create_unfenced(
                admin,
                approvers(
                    select(
                        policy("group-limit", "write", "approve"),
                        None,
                        Some("res.partner"),
                        None,
                    ),
                    &[a],
                    &[group],
                ),
            )
            .await
            .unwrap();
        let mut tx = f.begin(requester).await;
        assert_eq!(
            eligible_approvers(&mut tx, revision(grouped), &[], &[company_id(f.main)])
                .await
                .unwrap()
                .ids(),
            user_ids(&[a, b])
        );
        tx.rollback().await.unwrap();
        drop(tx);
        // Membership changes after policy creation grow the eligible set; no named-count change.
        f.sql(
            "INSERT INTO res_groups_users_rel(gid,uid) VALUES ($1,$2)",
            &[&group, &c],
        )
        .await;
        let mut tx = f.begin(requester).await;
        validation(
            &eligible_approvers(&mut tx, revision(grouped), &[], &[company_id(f.main)])
                .await
                .unwrap_err(),
            &format!(
                "Approval policy revision {grouped} has more than 2 eligible approvers (base.approval_max_approvers)."
            ),
        );
        tx.rollback().await.unwrap();
        drop(tx);
        inject(&f, "3").await;
        let mut tx = f.begin(requester).await;
        assert_eq!(
            eligible_approvers(&mut tx, revision(grouped), &[], &[company_id(f.main)])
                .await
                .unwrap()
                .ids(),
            user_ids(&[a, b, c])
        );
        tx.rollback().await.unwrap();
        drop(tx);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn corrupt_operating_limit_refuses_instead_of_defaulting() {
    run(|| async {
        let mut f = fixture().await;
        let admin = f
            .user("corrupt-admin", &[f.main], &[f.group_user, f.group_system])
            .await;
        let a = f.user("corrupt-a", &[f.main], &[f.group_user]).await;
        f.refresh().await;
        let id = f
            .create(
                admin,
                approvers(
                    policy("corrupt", "write", "approve").set_cell("active", Cell::Bool(false)),
                    &[a],
                    &[],
                ),
            )
            .await
            .unwrap();
        for text in [
            "null",
            "true",
            "\"2\"",
            "1.5",
            "0",
            "65",
            "9223372036854775808",
            "broken",
            &"9".repeat(10000),
        ] {
            inject(&f, text).await;
            let mut tx = f.begin(admin).await;
            validation(
                &eligible_approvers(&mut tx, revision(id), &[], &[company_id(f.main)])
                    .await
                    .unwrap_err(),
                "Stored setting base.approval_max_approvers is invalid.",
            );
            tx.rollback().await.unwrap();
            drop(tx);
        }
        f.db.cleanup().await.unwrap();
    });
}
