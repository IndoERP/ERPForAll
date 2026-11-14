// SPDX-License-Identifier: MIT OR Apache-2.0
//! Inventory settings through the real installer and durable settings service.
use super::*;
use erp_addon_base::ir_config_setting::{describe, read_values, write_values};
use erp_sdk::Value;
use erp_sdk::views::wire::{SettingsScope, WireId};
use std::collections::BTreeMap;

// A mixed-policy group proves that diagnostics never expose an expanded hidden sibling key.
fn diagnostic_declarations() -> erp_sdk::Result<Vec<erp_sdk::settings::SettingDecl>> {
    use erp_sdk::settings::{ScopeKind, SettingDecl};
    Ok(vec![
        SettingDecl::boolean(
            "base.test_visible",
            "Visible diagnostic fixture",
            ScopeKind::Global,
            false,
        )?
        .access(&["base.group_user"], &["base.group_user"])?,
        SettingDecl::boolean(
            "base.test_hidden",
            "Hidden diagnostic fixture",
            ScopeKind::Global,
            false,
        )?
        .access(&["base.group_system"], &["base.group_system"])?,
    ])
}
fn diagnostic_groups() -> erp_sdk::Result<Vec<erp_sdk::settings::SettingGroupDecl>> {
    Ok(vec![erp_sdk::settings::SettingGroupDecl::new(
        "base.test_diagnostic_pair",
        &["base.test_visible", "base.test_hidden"],
        |_| Ok(()),
    )?])
}
erp_sdk::setting_declarations!(diagnostic_declarations);
erp_sdk::setting_groups!(diagnostic_groups);

fn company_scope(id: i64) -> SettingsScope {
    SettingsScope::Company {
        company_id: WireId::try_from(u64::try_from(id).unwrap()).unwrap(),
    }
}

#[test]
fn inventory_settings_publish_typed_scoped_defaults_after_install() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let company = super::security::xid(&orm, "base", "main_company").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        let scope = company_scope(company);
        let description = describe(&env, &mut tx, scope.clone()).await.unwrap();
        let actual: BTreeMap<_, _> = description
            .declarations
            .iter()
            .filter(|d| d.key.starts_with("stock."))
            .map(|d| (d.key.as_str(), serde_json::to_value(&d.default).unwrap()))
            .collect();
        let expected = BTreeMap::from([
            ("stock.picking_policy", serde_json::json!("direct")),
            ("stock.horizon_days", serde_json::json!(365)),
            ("stock.annual_inventory_month", serde_json::json!("12")),
            ("stock.annual_inventory_day", serde_json::json!(31)),
            (
                "stock.stock_move_email_validation",
                serde_json::json!(false),
            ),
            ("stock.stock_text_confirmation", serde_json::json!(false)),
            ("stock.stock_confirmation_type", serde_json::json!("sms")),
        ]);
        assert_eq!(actual, expected);
        let values = read_values(
            &env,
            &mut tx,
            scope,
            expected.keys().map(|k| (*k).to_owned()).collect(),
        )
        .await
        .unwrap();
        let actual: BTreeMap<_, _> = values
            .values
            .iter()
            .map(|entry| {
                (
                    entry.key.as_str(),
                    serde_json::to_value(&entry.value).unwrap(),
                )
            })
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(values.revision, "0");
        assert!(
            values
                .values
                .iter()
                .all(|entry| matches!(&entry.source, erp_sdk::views::wire::ValueSource::Default))
        );
        let global = read_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            vec!["stock.barcode_separator".into()],
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(&global.values[0].value).unwrap(),
            serde_json::json!(",")
        );
        assert_eq!(global.revision, "0");
        assert!(matches!(
            &global.values[0].source,
            erp_sdk::views::wire::ValueSource::Default
        ));
        tx.rollback().await.unwrap();
    });
}

#[test]
fn inventory_settings_validate_partial_dates_atomically() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let company = super::security::xid(&orm, "base", "main_company").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        let scope = company_scope(company);
        let error = write_values(
            &env,
            &mut tx,
            scope.clone(),
            "0".into(),
            vec![
                ("stock.annual_inventory_month".into(), Value::str("4")),
                ("stock.horizon_days".into(), Value::Int(10)),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "stock.annual_inventory_day takes 1 to 30 for stock.annual_inventory_month 4."
        );
        let values = read_values(
            &env,
            &mut tx,
            scope.clone(),
            vec!["stock.horizon_days".into()],
        )
        .await
        .unwrap();
        assert_eq!(values.revision, "0");
        assert_eq!(
            serde_json::to_value(&values.values[0].value).unwrap(),
            serde_json::json!(365)
        );
        let audits: i64 = tx.query("SELECT count(*)::int8 FROM ir_config_setting_audit WHERE setting_key LIKE 'stock.%'", &[]).await.unwrap()[0].get(0);
        assert_eq!(audits, 0);
        let saved = write_values(
            &env,
            &mut tx,
            scope.clone(),
            "0".into(),
            vec![
                ("stock.annual_inventory_month".into(), Value::str("4")),
                ("stock.annual_inventory_day".into(), Value::Int(30)),
            ],
        )
        .await
        .unwrap();
        assert_eq!(saved.revision, "1");
        let error = write_values(
            &env,
            &mut tx,
            scope.clone(),
            "1".into(),
            vec![("stock.annual_inventory_month".into(), Value::str("2"))],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "stock.annual_inventory_day takes 1 to 28 for stock.annual_inventory_month 2."
        );
        let saved = write_values(
            &env,
            &mut tx,
            scope.clone(),
            "1".into(),
            vec![
                ("stock.annual_inventory_day".into(), Value::Int(28)),
                ("stock.annual_inventory_month".into(), Value::str("2")),
            ],
        )
        .await
        .unwrap();
        assert_eq!(saved.revision, "2");
        let stale = write_values(
            &env,
            &mut tx,
            scope,
            "1".into(),
            vec![("stock.horizon_days".into(), Value::Int(1))],
        )
        .await
        .unwrap_err();
        assert_eq!(stale.kind(), ErrorKind::Conflict);
        assert_eq!(stale.message(), "Settings changed; reload before saving.");
        tx.rollback().await.unwrap();
    });
}

#[test]
fn inventory_settings_denials_do_not_disclose_key_existence() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let actors = super::security::roles(&installed).await;
        let company = super::security::xid(&installed, "base", "main_company").await;
        let orm = super::security::secure(&db, &installed).await;
        let uid = erp_orm::UserId::new(actors[3]).unwrap();
        let mut tx = orm.begin(uid).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        for (key, scope) in [
            ("stock.missing", company_scope(company)),
            ("stock.barcode_separator", SettingsScope::Global {}),
            ("stock.picking_policy", SettingsScope::Global {}),
            ("stock.group_stock_multi_locations", company_scope(company)),
        ] {
            let error = read_values(&env, &mut tx, scope, vec![key.into()])
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(
                error.message(),
                format!("Setting {key} is unavailable or access is denied.")
            );
            let failure = erp_rpc::RpcFailure::from(error);
            assert_eq!(failure.status(), 403);
        }
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(uid).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        for write in [false, true] {
            let error = if write {
                write_values(
                    &env,
                    &mut tx,
                    SettingsScope::Global {},
                    "0".into(),
                    vec![("base.test_visible".into(), Value::Bool(true))],
                )
                .await
                .unwrap_err()
            } else {
                read_values(
                    &env,
                    &mut tx,
                    SettingsScope::Global {},
                    vec!["base.test_visible".into()],
                )
                .await
                .unwrap_err()
            };
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(
                error.message(),
                "Setting base.test_visible is unavailable or access is denied."
            );
            assert_eq!(erp_rpc::RpcFailure::from(error).status(), 403);
        }
        tx.rollback().await.unwrap();
        drop(tx);
        // Inventory managers read company values but cannot save them.
        let mut tx = orm.begin(uid).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        read_values(
            &env,
            &mut tx,
            company_scope(company),
            vec!["stock.picking_policy".into()],
        )
        .await
        .unwrap();
        let error = write_values(
            &env,
            &mut tx,
            company_scope(company),
            "0".into(),
            vec![("stock.picking_policy".into(), Value::str("one"))],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Setting stock.picking_policy is unavailable or access is denied."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        // A settings administrator has scalar write authority, but cannot name another company.
        let mut tx = orm
            .begin(erp_orm::UserId::new(actors[0]).unwrap())
            .await
            .unwrap();
        let env = tx.env().with_company(company).unwrap();
        write_values(
            &env,
            &mut tx,
            company_scope(company),
            "0".into(),
            vec![("stock.horizon_days".into(), Value::Int(1))],
        )
        .await
        .unwrap();
        let error = read_values(
            &env,
            &mut tx,
            company_scope(999_999),
            vec!["stock.picking_policy".into()],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Choose a company available to your account."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        // Ordinary internal users cannot read hidden Inventory declarations.
        let mut tx = orm
            .begin(erp_orm::UserId::new(actors[1]).unwrap())
            .await
            .unwrap();
        let env = tx.env().with_company(company).unwrap();
        let error = read_values(
            &env,
            &mut tx,
            company_scope(company),
            vec!["stock.picking_policy".into()],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Setting stock.picking_policy is unavailable or access is denied."
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn inventory_settings_isolate_companies_and_describe_deferred_features() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let a = super::security::xid(&orm, "base", "main_company").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(a).unwrap();
        let currency: i64 = tx
            .query("SELECT currency_id FROM res_company WHERE id=$1", &[&a])
            .await
            .unwrap()[0]
            .get(0);
        let b = env
            .records("res.company")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Second settings company".into()))
                        .set_cell("currency_id", Cell::Id(currency)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let env = env.with_company(b).unwrap();
        let groups = tx
            .query(
                "SELECT gid::text || ':' || hid::text FROM res_groups_implied_rel ORDER BY gid,hid",
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>();
        let modules = tx
            .query(
                "SELECT name || ':' || state FROM ir_module_module ORDER BY name",
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>();
        for (id, policy) in [(a, "one"), (b, "direct")] {
            write_values(
                &env,
                &mut tx,
                company_scope(id),
                "0".into(),
                vec![("stock.picking_policy".into(), Value::str(policy))],
            )
            .await
            .unwrap();
        }
        for (id, policy) in [(a, "one"), (b, "direct")] {
            let value = read_values(
                &env,
                &mut tx,
                company_scope(id),
                vec!["stock.picking_policy".into()],
            )
            .await
            .unwrap();
            assert_eq!(
                serde_json::to_value(&value.values[0].value).unwrap(),
                serde_json::json!(policy)
            );
        }
        let description = describe(&env, &mut tx, company_scope(a)).await.unwrap();
        let json = serde_json::to_value(&description).unwrap();
        let item = json["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == "stock.group_stock_multi_locations")
            .unwrap();
        assert_eq!(item["kind"], "info");
        assert_eq!(item["value"], "Multiple locations (unavailable)");
        assert_eq!(
            item["detail"],
            "Unavailable until a later release supplies feature-group changes."
        );
        let tracking = json["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == "stock.group_stock_production_lot")
            .unwrap();
        assert_eq!(
            tracking["detail"],
            "Unavailable until a later release and a later release supply tracking controls."
        );
        let route = json["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == "stock.replenish_on_order")
            .unwrap();
        assert_eq!(
            route["detail"],
            "Unavailable until a later release supplies route activation."
        );
        let action = super::security::xid(&orm, "stock", "inventory_addons_action").await;
        for item in json["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["key"].as_str().unwrap().starts_with("stock.module_"))
        {
            assert_eq!(item["kind"], "link");
            assert_eq!(item["target"]["id"], action);
        }
        let error = write_values(
            &env,
            &mut tx,
            company_scope(a),
            "1".into(),
            vec![(
                "stock.group_stock_multi_locations".into(),
                Value::Bool(true),
            )],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Setting stock.group_stock_multi_locations is unavailable or access is denied."
        );
        let current_groups = tx
            .query(
                "SELECT gid::text || ':' || hid::text FROM res_groups_implied_rel ORDER BY gid,hid",
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>();
        assert_eq!(current_groups, groups);
        let current_modules = tx
            .query(
                "SELECT name || ':' || state FROM ir_module_module ORDER BY name",
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>();
        assert_eq!(current_modules, modules);
        let stock_keys = description
            .declarations
            .iter()
            .filter(|d| d.key.starts_with("stock."))
            .map(|d| d.key.clone())
            .collect();
        let values = read_values(&env, &mut tx, company_scope(a), stock_keys)
            .await
            .unwrap();
        let global_description = describe(&env, &mut tx, SettingsScope::Global {})
            .await
            .unwrap();
        let global_values = read_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            vec!["stock.barcode_separator".into()],
        )
        .await
        .unwrap();
        std::fs::create_dir_all("logs").unwrap();
        std::fs::write(
            "logs/inventory-settings-fixture.json",
            serde_json::to_vec(&serde_json::json!({
                "company": {"description": description, "values": values},
                "global": {"description": global_description, "values": global_values}
            }))
            .unwrap(),
        )
        .unwrap();
        tx.rollback().await.unwrap();
    });
}

async fn date_writer(orm: &Orm, company: i64, month: &str, day: i64) -> erp_sdk::Result<()> {
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    let env = tx.env().with_company(company)?;
    let result = write_values(
        &env,
        &mut tx,
        company_scope(company),
        "0".into(),
        vec![
            ("stock.annual_inventory_month".into(), Value::str(month)),
            ("stock.annual_inventory_day".into(), Value::Int(day)),
        ],
    )
    .await;
    match result {
        Ok(_) => tx.commit().await,
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

#[test]
fn inventory_settings_concurrent_date_writes_keep_one_complete_pair() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let company = super::security::xid(&orm, "base", "main_company").await;
        let (april, february) = tokio::join!(
            date_writer(&orm, company, "4", 30),
            date_writer(&orm, company, "2", 28)
        );
        let expected = match (april, february) {
            (Ok(()), Err(error)) => {
                assert_eq!(error.kind(), ErrorKind::Conflict);
                assert_eq!(error.message(), "Settings changed; reload before saving.");
                ("4", 30)
            }
            (Err(error), Ok(())) => {
                assert_eq!(error.kind(), ErrorKind::Conflict);
                assert_eq!(error.message(), "Settings changed; reload before saving.");
                ("2", 28)
            }
            other => panic!("expected one writer and one revision conflict: {other:?}"),
        };
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        let values = read_values(
            &env,
            &mut tx,
            company_scope(company),
            vec![
                "stock.annual_inventory_month".into(),
                "stock.annual_inventory_day".into(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(values.revision, "1");
        assert_eq!(
            serde_json::to_value(&values.values[0].value).unwrap(),
            serde_json::json!(expected.0)
        );
        assert_eq!(
            serde_json::to_value(&values.values[1].value).unwrap(),
            serde_json::json!(expected.1)
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn inventory_settings_upgrade_preserves_scoped_values_and_actions() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let company = super::security::xid(&orm, "base", "main_company").await;
        let action = super::security::xid(&orm, "stock", "inventory_addons_action").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        write_values(
            &env,
            &mut tx,
            company_scope(company),
            "0".into(),
            vec![("stock.picking_policy".into(), Value::str("one"))],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let system = super::system(&db);
        for _ in 0..2 {
            let upgraded = system
                .button_immediate_upgrade(
                    &system.system_env(Context::new()),
                    &[ModuleName::new("stock").unwrap()],
                )
                .await
                .unwrap();
            assert!(upgraded.post_commit.is_clean());
            assert_eq!(
                super::security::xid(&upgraded.orm, "stock", "inventory_addons_action").await,
                action
            );
            let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
            let env = tx.env().with_company(company).unwrap();
            let values = read_values(
                &env,
                &mut tx,
                company_scope(company),
                vec!["stock.picking_policy".into()],
            )
            .await
            .unwrap();
            assert_eq!(values.revision, "1");
            assert_eq!(
                serde_json::to_value(&values.values[0].value).unwrap(),
                serde_json::json!("one")
            );
            tx.rollback().await.unwrap();
        }
    });
}

#[test]
fn stock_unit_policy_is_in_system_parameters_not_on_the_settings_page() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env();
        let description = describe(&env, &mut tx, SettingsScope::Global {})
            .await
            .unwrap();
        let json = serde_json::to_value(&description).unwrap();
        assert!(
            !json.to_string().contains("stock.quant_"),
            "no policy key is described on the Settings page"
        );
        let rows = tx
            .query(
                "SELECT key, value FROM ir_config_parameter WHERE key LIKE 'stock.quant\\_%' ORDER BY key",
                &[],
            )
            .await
            .unwrap();
        let stored: BTreeMap<String, String> =
            rows.iter().map(|row| (row.get(0), row.get(1))).collect();
        assert_eq!(
            stored,
            BTreeMap::from([
                (
                    "stock.quant_idle_transaction_timeout_ms".to_owned(),
                    "60000".to_owned()
                ),
                ("stock.quant_lock_timeout_ms".into(), "1000".into()),
                ("stock.quant_max_attempts".into(), "5".into()),
                ("stock.quant_retry_base_ms".into(), "50".into()),
                ("stock.quant_retry_cap_ms".into(), "2000".into()),
                ("stock.quant_tcp_keepalives_count".into(), "3".into()),
                ("stock.quant_tcp_keepalives_idle_s".into(), "30".into()),
                ("stock.quant_tcp_keepalives_interval_s".into(), "10".into()),
            ])
        );
        tx.rollback().await.unwrap();
        drop(tx);
        // Even the superuser gets the unknown-key refusal from the Settings service.
        for key in stored.keys() {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let env = tx.env();
            let read = read_values(&env, &mut tx, SettingsScope::Global {}, vec![key.clone()])
                .await
                .unwrap_err();
            let write = write_values(
                &env,
                &mut tx,
                SettingsScope::Global {},
                "0".into(),
                vec![(key.clone(), Value::Int(20))],
            )
            .await
            .unwrap_err();
            for error in [read, write] {
                assert_eq!(error.kind(), ErrorKind::Access);
                assert_eq!(
                    error.message(),
                    format!("Setting {key} is unavailable or access is denied.")
                );
            }
            tx.rollback().await.unwrap();
        }
    });
}
