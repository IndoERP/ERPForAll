// SPDX-License-Identifier: MIT OR Apache-2.0
//! Inventory settings through the real installer and durable settings service.
use super::*;
use erp_addon_base::ir_config_setting::{describe, read_values, toggle_group, write_values};
use erp_sdk::Value;
use erp_sdk::views::wire::GroupToggleState;
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
fn delivery_policy_save_updates_only_changed_active_types_in_the_selected_company() {
    use erp_orm::{Transaction, UserId};
    use serde_json::json;
    use std::sync::Arc;

    const POLICY: &str = "stock.picking_policy";
    const HORIZON: &str = "stock.horizon_days";

    // xmin also detects an UPDATE that stored the original value back to the same row.
    async fn policies(tx: &mut Transaction, company: i64) -> Vec<(i64, i64, bool, String, String)> {
        tx.flush().await.unwrap();
        tx.query(
            "SELECT id, warehouse_id, active, move_type, xmin::text FROM stock_picking_type \
             WHERE company_id=$1 ORDER BY id",
            &[&company],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
        .collect()
    }

    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let roles = super::security::roles(&installed).await;
        let company = super::security::xid(&installed, "base", "main_company").await;
        let scope = company_scope(company);
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let env = setup.env().with_company(company).unwrap();
        let currency = env
            .records("res.company")
            .unwrap()
            .browse([company])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        let other = env
            .records("res.company")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Other shipping company".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let warehouse = env
            .records("stock.warehouse")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Second shipping warehouse".into()))
                        .set_cell("code", Cell::Str("SHIP2".into()))
                        .set_cell("company_id", Cell::Id(company)),
                ],
            )
            .await
            .unwrap();
        let Cell::Id(already_one) = warehouse.cell(&mut setup, "out_type_id").await.unwrap() else {
            panic!("the second warehouse has no delivery type");
        };
        env.records("stock.picking.type")
            .unwrap()
            .browse([already_one])
            .unwrap()
            .write(
                &mut setup,
                RawVals::new().set_cell("move_type", Cell::Str("one".into())),
            )
            .await
            .unwrap();
        setup.commit().await.unwrap();
        drop(setup);

        let orm = super::security::secure(&db, &installed).await;
        let admin = UserId::new(roles[0]).unwrap();
        let manager = UserId::new(roles[3]).unwrap();
        let change = |policy| vec![(POLICY.into(), Value::str(policy))];
        let mut denied = orm.begin(manager).await.unwrap();
        let error = write_values(
            &denied.env(),
            &mut denied,
            scope.clone(),
            "0".into(),
            change("one"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Setting stock.picking_policy is unavailable or access is denied."
        );
        denied.rollback().await.unwrap();
        drop(denied);

        let mut tx = orm.begin(admin).await.unwrap();
        let env = tx.env();
        let error = write_values(
            &env,
            &mut tx,
            company_scope(other),
            "0".into(),
            change("one"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Choose a company available to your account."
        );
        let before = policies(&mut tx, company).await;
        let foreign = policies(&mut tx, other).await;
        assert!(foreign.iter().any(|row| row.2 && row.3 == "direct"));
        assert!(before.iter().any(|row| !row.2 && row.3 == "direct"));
        assert_eq!(
            before
                .iter()
                .filter(|row| row.2)
                .map(|row| row.1)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            2
        );
        let blocked = before
            .iter()
            .find(|row| row.2 && row.3 == "direct")
            .unwrap()
            .0;
        let saved = write_values(&env, &mut tx, scope.clone(), "0".into(), change("direct"))
            .await
            .unwrap();
        assert_eq!(saved.revision, "1");
        assert_eq!(policies(&mut tx, company).await, before);
        tx.commit().await.unwrap();
        drop(tx);

        let mut lock = installed.begin(SUPERUSER_ID).await.unwrap();
        lock.env()
            .records("stock.picking.type")
            .unwrap()
            .browse([blocked])
            .unwrap()
            .lock_for_update(&mut lock, true)
            .await
            .unwrap();
        let mut tx = orm.begin(admin).await.unwrap();
        let env = tx.env();
        let mut changes = change("one");
        changes.push((HORIZON.into(), Value::Int(49)));
        let error = write_values(&env, &mut tx, scope.clone(), "1".into(), changes)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Lock);
        assert_eq!(error.message(), "Cannot grab a lock on records");
        let saved = read_values(
            &env,
            &mut tx,
            scope.clone(),
            vec![POLICY.into(), HORIZON.into()],
        )
        .await
        .unwrap();
        assert_eq!(saved.revision, "1");
        let values: BTreeMap<_, _> = saved
            .values
            .iter()
            .map(|entry| (entry.key.as_str(), json!(&entry.value)))
            .collect();
        assert_eq!(
            values,
            BTreeMap::from([(HORIZON, json!(365)), (POLICY, json!("direct"))])
        );
        let audit = tx
            .query(
                "SELECT setting_key, new_value FROM ir_config_setting_audit a \
             JOIN ir_config_setting_scope s ON s.id=a.scope_id WHERE s.company_id=$1",
                &[&company],
            )
            .await
            .unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].get::<_, String>(0), POLICY);
        assert_eq!(audit[0].get::<_, String>(1), "\"direct\"");
        assert_eq!(policies(&mut tx, company).await, before);
        tx.commit().await.unwrap();
        drop(tx);
        lock.rollback().await.unwrap();
        drop(lock);

        // A customized record rule must refuse an incomplete update instead of silently
        // retaining the previous policy on the hidden row.
        let model = super::security::xid(&installed, "stock", "model_stock_picking_type").await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let access = setup.env().records("ir.access").unwrap();
        let restriction = access
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Hide one operation type".into()))
                        .set_cell("model_id", Cell::Id(model))
                        .set_cell("operation", Cell::Str("r".into()))
                        .set_cell(
                            "domain",
                            Cell::Str(format!("[('id', '!=', {blocked})]").into()),
                        ),
                ],
            )
            .await
            .unwrap();
        setup.commit().await.unwrap();
        drop(setup);
        let restricted = super::security::secure(&db, &installed).await;
        let mut tx = restricted.begin(admin).await.unwrap();
        let env = tx.env();
        let error = write_values(&env, &mut tx, scope.clone(), "1".into(), change("one"))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "You cannot change the delivery policy because some affected operation types are not accessible."
        );
        assert_eq!(policies(&mut tx, company).await, before);
        tx.commit().await.unwrap();
        drop(tx);
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        setup
            .env()
            .records("ir.access")
            .unwrap()
            .browse(restriction.ids())
            .unwrap()
            .unlink(&mut setup)
            .await
            .unwrap();
        setup.commit().await.unwrap();
        drop(setup);

        let orm = super::security::secure(&db, &installed).await;
        let mut tx = orm.begin(admin).await.unwrap();
        let env = tx.env();
        let saved: Value = env
            .records("ir.config.setting")
            .unwrap()
            .call(
                &mut tx,
                "erp_write_values",
                (
                    Value::Json(Arc::new(json!(scope))),
                    "1".to_owned(),
                    vec![Value::Json(Arc::new(
                        json!({ "key": POLICY, "value": "one" }),
                    ))],
                ),
            )
            .await
            .unwrap();
        let Value::Json(saved) = saved else {
            panic!("Settings must return its typed JSON result");
        };
        assert_eq!(saved["revision"], "2");
        for (old, new) in before.iter().zip(policies(&mut tx, company).await) {
            if old.2 && old.3 == "direct" {
                assert_eq!(
                    (new.0, new.1, new.2, new.3.as_str()),
                    (old.0, old.1, true, "one")
                );
                assert_ne!(new.4, old.4);
            } else {
                assert_eq!(&new, old);
            }
        }
        assert_eq!(policies(&mut tx, other).await, foreign);
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        // Explicit owner B takes precedence over the environment's current company A.
        let env = tx.env().with_company(company).unwrap();
        let types = env.records("stock.picking.type").unwrap();
        let value = types.default_get(&mut tx, &["move_type"]).await.unwrap();
        assert_eq!(value["move_type"], Value::str("one"));
        let values = |name: &str, owner| {
            RawVals::new()
                .set_cell("name", Cell::Str(name.into()))
                .set_cell("company_id", Cell::Id(owner))
        };
        let created = types
            .create(
                &mut tx,
                vec![
                    values("Company policy", company),
                    values("Explicit policy", company)
                        .set_cell("move_type", Cell::Str("direct".into())),
                    values("Other company default", other),
                ],
            )
            .await
            .unwrap();
        let policies = created.cells(&mut tx, "move_type").await.unwrap();
        assert_eq!(
            policies,
            [
                Cell::Str("one".into()),
                Cell::Str("direct".into()),
                Cell::Str("direct".into())
            ]
        );

        // Generated types also use their warehouse's owner, even in another active company.
        let env = env.with_company(other).unwrap();
        let warehouse = env
            .records("stock.warehouse")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("New policy warehouse".into()))
                        .set_cell("code", Cell::Str("POL3".into()))
                        .set_cell("company_id", Cell::Id(company)),
                ],
            )
            .await
            .unwrap();
        let rows = tx
            .query(
                "SELECT move_type FROM stock_picking_type WHERE warehouse_id=$1 ORDER BY id",
                &[&warehouse.ids()[0]],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 8);
        assert!(rows.iter().all(|row| row.get::<_, String>(0) == "one"));
        tx.commit().await.unwrap();
    });
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
            ("stock.replenish_on_order", serde_json::json!(false)),
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
        // Inventory managers read company values but cannot save them and do not see admin module and group
        // toggles.
        let mut tx = orm.begin(uid).await.unwrap();
        let env = tx.env().with_company(company).unwrap();
        let manager_desc = describe(&env, &mut tx, company_scope(company))
            .await
            .unwrap();
        let manager_json = serde_json::to_value(&manager_desc).unwrap();
        for key in [
            "stock.module_product_expiry",
            "stock.module_stock_sms",
            "stock.module_delivery",
            "stock.module_stock_dropshipping",
            "stock.group_stock_multi_locations",
            "stock.group_stock_adv_location",
            "stock.group_stock_production_lot",
            "stock.group_stock_tracking_lot",
            "stock.group_stock_tracking_owner",
            "stock.group_warning_stock",
        ] {
            assert!(
                !manager_json["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|i| i["key"] == key),
                "non-admin stock manager must not see {key}"
            );
        }
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
        // The feature groups that gate menus, view elements or fields are toggles, off on a fresh install; no group
        // row ids travel.
        for (key, label, group) in [
            (
                "stock.group_stock_multi_locations",
                "Multiple locations",
                "group_stock_multi_locations",
            ),
            (
                "stock.group_stock_adv_location",
                "Advanced routes",
                "group_adv_location",
            ),
            (
                "stock.group_stock_production_lot",
                "Company-wide lot tracking",
                "group_production_lot",
            ),
            (
                "stock.group_stock_tracking_lot",
                "Packages",
                "group_tracking_lot",
            ),
            (
                "stock.group_stock_tracking_owner",
                "Consignment",
                "group_tracking_owner",
            ),
            (
                "stock.group_warning_stock",
                "Partner stock warnings",
                "group_warning_stock",
            ),
        ] {
            let item = json["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["key"] == key)
                .expect(key);
            assert_eq!(item["kind"], "group", "{key}");
            assert_eq!(item["label"], label, "{key}");
            assert_eq!(item["group"], format!("stock.{group}"), "{key}");
            assert_eq!(item["state"], "disabled", "{key}");
            assert!(item.get("group_id").is_none(), "{key}");
        }
        // A module toggle reads what the install route says: offered addons (linked into this binary) are not
        // installed yet, other addons of the directory are not available.
        let catalogue = erp_sdk::settings::addon_catalogue().await.unwrap().unwrap();
        for (key, module, menu) in [
            (
                "stock.backorder_policies",
                "stock",
                "menu_stock_picking_type_config",
            ),
            ("stock.reordering_rules", "stock", "menu_stock_orderpoint"),
            ("stock.lot_serial_register", "stock", "menu_stock_lot"),
            ("stock.lot_user_access", "base", "settings_users_menu"),
        ] {
            let item = json["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["key"] == key)
                .expect(key);
            assert_eq!(item["app"], "stock");
            assert_eq!(item["kind"], "link");
            assert_eq!(item["target"]["kind"], "menu");
            assert_eq!(
                item["target"]["id"],
                super::security::xid(&orm, module, menu).await
            );
        }
        // stock.replenish_on_order is now a live boolean company setting rather than a deferred info item.
        assert!(
            !json["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["key"] == "stock.replenish_on_order"),
            "stock.replenish_on_order should be a declared setting, not a deferred panel info item"
        );
        for (key, label, addon) in [
            (
                "stock.module_product_expiry",
                "Expiry dates",
                "product_expiry",
            ),
            ("stock.module_stock_sms", "Text delivery", "stock_sms"),
            ("stock.module_delivery", "Carriers", "delivery"),
            (
                "stock.module_stock_dropshipping",
                "Direct supplier delivery",
                "stock_dropshipping",
            ),
            (
                "stock.module_stock_landed_costs",
                "Landed costs",
                "stock_landed_costs",
            ),
            ("stock.module_stock_barcode", "Scanning", "stock_barcode"),
            (
                "stock.module_quality_control",
                "Quality checks",
                "quality_control",
            ),
            ("stock.module_stock_fleet", "Transport", "stock_fleet"),
        ] {
            let item = json["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["key"] == key)
                .unwrap();
            assert_eq!(item["kind"], "module");
            assert_eq!(item["label"], label);
            assert_eq!(item["addon"], addon);
            let expected = if catalogue.offered.contains(addon) {
                "not_installed"
            } else {
                assert!(catalogue.refused.contains(addon), "{addon}");
                "not_available"
            };
            assert_eq!(item["state"], expected, "{key}");
            let error = write_values(
                &env,
                &mut tx,
                company_scope(a),
                "1".into(),
                vec![(key.into(), Value::Bool(true))],
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Access);
        }
        // All inventory features are now shipped as live settings or installable modules (no deferred info rows).
        assert!(
            !json["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["app"] == "stock" && i["kind"] == "info"),
            "no deferred stock info items remain"
        );
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

/// Whether `user` holds `group`, asked through the membership check of a freshly built policy-enforcing ORM.
async fn holds(db: &TestDb, installed: &Orm, user: i64, group: &str) -> bool {
    let orm = super::security::secure(db, installed).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let held = tx
        .user_has_group(&env, erp_orm::UserId::new(user).unwrap(), group)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    held
}

// The Inventory group toggles through the Settings service: only Settings administrators may switch them, on
// grants the feature to every internal user, off takes it from users who held it only that way, and off is
// refused while another group still grants it.
#[test]
fn inventory_group_toggle_turns_a_feature_on_and_off_for_every_internal_user() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let actors = super::security::roles(&installed).await;
        let (admin, internal, direct, manager) = (actors[0], actors[1], actors[2], actors[3]);
        let company = super::security::xid(&installed, "base", "main_company").await;
        let packages = super::security::xid(&installed, "stock", "group_tracking_lot").await;
        let locations =
            super::security::xid(&installed, "stock", "group_stock_multi_locations").await;
        const PACKAGES: &str = "stock.group_stock_tracking_lot";
        const LOCATIONS: &str = "stock.group_stock_multi_locations";
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        tx.env()
            .records("res.users")
            .unwrap()
            .browse([direct])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands(
                    "group_ids",
                    [erp_orm::Command::link(
                        erp_fields::RecId::from_real(packages).unwrap(),
                    )],
                ),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let toggle = async |uid: i64, key: &str, enabled: bool| {
            let orm = super::security::secure(&db, &installed).await;
            let mut tx = orm.begin(erp_orm::UserId::new(uid).unwrap()).await.unwrap();
            let env = tx.env().with_company(company).unwrap();
            let result = toggle_group(&env, &mut tx, key, enabled).await;
            if result.is_ok() {
                tx.commit().await.unwrap();
            } else {
                tx.rollback().await.unwrap();
            }
            result
        };
        assert!(!holds(&db, &installed, internal, "stock.group_tracking_lot").await);
        assert!(holds(&db, &installed, direct, "stock.group_tracking_lot").await);

        // Only Settings administrators; only declared group toggles.
        let refused = toggle(manager, PACKAGES, true).await.unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Access);
        assert_eq!(
            refused.message(),
            "Only Settings administrators may turn a feature on or off for every user."
        );
        let refused = toggle(admin, "stock.picking_policy", true)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Validation);
        assert_eq!(
            refused.message(),
            "Setting stock.picking_policy is not a feature toggle."
        );
        assert!(!holds(&db, &installed, internal, "stock.group_tracking_lot").await);

        // On: every internal user has the feature, and the page says so.
        assert_eq!(
            toggle(admin, PACKAGES, true).await.unwrap(),
            GroupToggleState::Enabled
        );
        assert!(holds(&db, &installed, internal, "stock.group_tracking_lot").await);
        let orm = super::security::secure(&db, &installed).await;
        let mut tx = orm
            .begin(erp_orm::UserId::new(admin).unwrap())
            .await
            .unwrap();
        let env = tx.env().with_company(company).unwrap();
        let described = serde_json::to_value(
            describe(&env, &mut tx, company_scope(company))
                .await
                .unwrap(),
        )
        .unwrap();
        let row = described["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == PACKAGES)
            .unwrap()
            .clone();
        assert_eq!(row["state"], "enabled");
        tx.rollback().await.unwrap();
        drop(tx);
        drop(orm);

        // Off: the user who held it only through the internal-user group loses it; the directly granted user keeps it.
        assert_eq!(
            toggle(admin, PACKAGES, false).await.unwrap(),
            GroupToggleState::Disabled
        );
        assert!(!holds(&db, &installed, internal, "stock.group_tracking_lot").await);
        assert!(holds(&db, &installed, direct, "stock.group_tracking_lot").await);
        // The same answer again changes nothing.
        assert_eq!(
            toggle(admin, PACKAGES, false).await.unwrap(),
            GroupToggleState::Disabled
        );

        // Each change wrote one Settings audit row in the global scope, by the administrator.
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        let audit = tx
            .query(
                "SELECT a.old_value || '>' || a.new_value || '@' || a.actor FROM ir_config_setting_audit a \
                 JOIN ir_config_setting_scope s ON s.id = a.scope_id \
                 WHERE a.setting_key = $1 AND s.identity = 'global' ORDER BY a.id",
                &[&PACKAGES],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>();
        assert_eq!(
            audit,
            [
                format!("false>true@user:{admin}"),
                format!("true>false@user:{admin}")
            ]
        );
        // Another group now grants the feature too (Multiple locations implies Packages).
        tx.env()
            .records("res.groups")
            .unwrap()
            .browse([locations])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands(
                    "implied_ids",
                    [erp_orm::Command::link(
                        erp_fields::RecId::from_real(packages).unwrap(),
                    )],
                ),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        assert_eq!(
            toggle(admin, LOCATIONS, true).await.unwrap(),
            GroupToggleState::Enabled
        );
        assert!(holds(&db, &installed, internal, "stock.group_tracking_lot").await);
        // Off for Packages would not be true while Multiple locations is on: refused, naming the group.
        let refused = toggle(admin, PACKAGES, false).await.unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Validation);
        assert_eq!(
            refused.message(),
            "Packages stays on for every internal user through the group stock.group_stock_multi_locations; \
             change that group instead."
        );
        assert!(holds(&db, &installed, internal, "stock.group_tracking_lot").await);
    });
}

// A change an administrator makes in the Groups form (here: Packages made to imply a group that is not a light
// feature group) leaves the Settings page and module loads working: only that row is unavailable, with the
// declaration rule's reason, and only its switch refuses.
#[test]
fn inventory_group_toggle_made_heavy_in_the_groups_form_is_unavailable_on_its_row_only() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let actors = super::security::roles(&installed).await;
        let (admin, internal) = (actors[0], actors[1]);
        let company = super::security::xid(&installed, "base", "main_company").await;
        let packages = super::security::xid(&installed, "stock", "group_tracking_lot").await;
        let stock_user = super::security::xid(&installed, "stock", "group_stock_user").await;
        const PACKAGES: &str = "stock.group_stock_tracking_lot";
        const RULE: &str = "Setting item stock.group_stock_tracking_lot cannot toggle the group \
                            stock.group_tracking_lot: it is or implies a group that is not a light feature group.";
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        tx.env()
            .records("res.groups")
            .unwrap()
            .browse([packages])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands(
                    "implied_ids",
                    [erp_orm::Command::link(
                        erp_fields::RecId::from_real(stock_user).unwrap(),
                    )],
                ),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let orm = super::security::secure(&db, &installed).await;
        let mut tx = orm
            .begin(erp_orm::UserId::new(admin).unwrap())
            .await
            .unwrap();
        let env = tx.env().with_company(company).unwrap();
        // The registration check that every module load runs still passes.
        erp_addon_base::ir_config_setting::validate_declarations(&env, &mut tx)
            .await
            .unwrap();
        let described = serde_json::to_value(
            describe(&env, &mut tx, company_scope(company))
                .await
                .unwrap(),
        )
        .unwrap();
        let row = |key: &str| {
            described["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["key"] == key)
                .expect(key)
                .clone()
        };
        let packages_row = row(PACKAGES);
        assert_eq!(packages_row["state"], "not_available");
        assert_eq!(packages_row["reason"], RULE);
        let other = row("stock.group_stock_tracking_owner");
        assert_eq!(other["state"], "disabled");
        assert!(other["reason"].is_null());
        // The switch of that row refuses with the same reason; nothing changes.
        let refused = toggle_group(&env, &mut tx, PACKAGES, true)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Validation);
        assert_eq!(refused.message(), RULE);
        // A key no declaration can have is refused before it is repeated.
        let long = format!("stock.{}", "x".repeat(130));
        let refused = toggle_group(&env, &mut tx, &long, true).await.unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Validation);
        assert_eq!(
            refused.message(),
            "A setting key is at most 128 bytes and has no control characters."
        );
        let refused = toggle_group(&env, &mut tx, "stock.a\nb", true)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Validation);
        assert_eq!(
            refused.message(),
            "A setting key is at most 128 bytes and has no control characters."
        );
        // Another row still switches.
        assert_eq!(
            toggle_group(&env, &mut tx, "stock.group_stock_tracking_owner", true)
                .await
                .unwrap(),
            GroupToggleState::Enabled
        );
        tx.commit().await.unwrap();
        drop(tx);
        assert!(!holds(&db, &installed, internal, "stock.group_tracking_lot").await);
        assert!(holds(&db, &installed, internal, "stock.group_tracking_owner").await);
    });
}

/// The described row of `key` as the Settings administrator `admin` sees the company page.
async fn described_row(
    db: &TestDb,
    installed: &Orm,
    admin: i64,
    company: i64,
    key: &str,
) -> serde_json::Value {
    let orm = super::security::secure(db, installed).await;
    let mut tx = orm
        .begin(erp_orm::UserId::new(admin).unwrap())
        .await
        .unwrap();
    let env = tx.env().with_company(company).unwrap();
    let described = serde_json::to_value(
        describe(&env, &mut tx, company_scope(company))
            .await
            .unwrap(),
    )
    .unwrap();
    tx.rollback().await.unwrap();
    described["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == key)
        .expect(key)
        .clone()
}

// A row that is on and whose group is later made to imply a group that is not light says so, refuses to switch
// on, and can still be switched off (removing the implication grants nothing).
#[test]
fn inventory_group_toggle_that_is_on_and_made_heavy_can_still_be_switched_off() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let actors = super::security::roles(&installed).await;
        let (admin, internal) = (actors[0], actors[1]);
        let company = super::security::xid(&installed, "base", "main_company").await;
        let packages = super::security::xid(&installed, "stock", "group_tracking_lot").await;
        let stock_user = super::security::xid(&installed, "stock", "group_stock_user").await;
        const PACKAGES: &str = "stock.group_stock_tracking_lot";
        const RULE: &str = "Setting item stock.group_stock_tracking_lot cannot toggle the group \
                            stock.group_tracking_lot: it is or implies a group that is not a light feature group.";
        let orm = super::security::secure(&db, &installed).await;
        let switch = async |enabled: bool| {
            let mut tx = orm
                .begin(erp_orm::UserId::new(admin).unwrap())
                .await
                .unwrap();
            let env = tx.env().with_company(company).unwrap();
            let result = toggle_group(&env, &mut tx, PACKAGES, enabled).await;
            if result.is_ok() {
                tx.commit().await.unwrap();
            } else {
                tx.rollback().await.unwrap();
            }
            result
        };
        assert_eq!(switch(true).await.unwrap(), GroupToggleState::Enabled);
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        tx.env()
            .records("res.groups")
            .unwrap()
            .browse([packages])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands(
                    "implied_ids",
                    [erp_orm::Command::link(
                        erp_fields::RecId::from_real(stock_user).unwrap(),
                    )],
                ),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let row = described_row(&db, &installed, admin, company, PACKAGES).await;
        assert_eq!(row["state"], "not_available");
        assert_eq!(
            row["reason"],
            format!("{RULE} It is on for every internal user.")
        );
        assert_eq!(row["on_for_all"], true);
        let refused = switch(true).await.unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Validation);
        assert_eq!(
            refused.message(),
            format!("{RULE} It is on for every internal user.")
        );
        assert_eq!(switch(false).await.unwrap(), GroupToggleState::Disabled);
        assert!(!holds(&db, &installed, internal, "stock.group_tracking_lot").await);
        // Off, the row no longer says it is on.
        let row = described_row(&db, &installed, admin, company, PACKAGES).await;
        assert_eq!(row["reason"], RULE);
        assert_eq!(row["on_for_all"], false);
        db.cleanup().await.unwrap();
    });
}

// A group deleted in the Groups form (its external id gone) shows its row as not available with the reason; the
// page and the other rows keep working.
#[test]
fn inventory_group_toggle_whose_group_was_deleted_is_unavailable_on_its_row_only() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let actors = super::security::roles(&installed).await;
        let company = super::security::xid(&installed, "base", "main_company").await;
        let packages = super::security::xid(&installed, "stock", "group_tracking_lot").await;
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        tx.execute(
            "DELETE FROM ir_model_data WHERE model = 'res.groups' AND res_id = $1",
            &[&packages],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let row = described_row(
            &db,
            &installed,
            actors[0],
            company,
            "stock.group_stock_tracking_lot",
        )
        .await;
        assert_eq!(row["state"], "not_available");
        assert_eq!(
            row["reason"],
            "Setting item stock.group_stock_tracking_lot cannot toggle the group stock.group_tracking_lot: \
             it does not exist."
        );
        let other = described_row(
            &db,
            &installed,
            actors[0],
            company,
            "stock.group_stock_tracking_owner",
        )
        .await;
        assert_eq!(other["state"], "disabled");
        db.cleanup().await.unwrap();
    });
}

// The group that keeps a feature on is named by its display name when it has no external id.
#[test]
fn inventory_group_toggle_off_refusal_names_an_implying_group_without_external_id_by_display_name()
{
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let actors = super::security::roles(&installed).await;
        let company = super::security::xid(&installed, "base", "main_company").await;
        let packages = super::security::xid(&installed, "stock", "group_tracking_lot").await;
        let locations =
            super::security::xid(&installed, "stock", "group_stock_multi_locations").await;
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        tx.env()
            .records("res.groups")
            .unwrap()
            .browse([locations])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands(
                    "implied_ids",
                    [erp_orm::Command::link(
                        erp_fields::RecId::from_real(packages).unwrap(),
                    )],
                ),
            )
            .await
            .unwrap();
        // The internal-user group implies Multiple locations, which implies Packages; then Multiple locations
        // loses its external id.
        let internal_group = super::security::xid(&installed, "base", "group_user").await;
        tx.env()
            .records("res.groups")
            .unwrap()
            .browse([internal_group])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands(
                    "implied_ids",
                    [erp_orm::Command::link(
                        erp_fields::RecId::from_real(locations).unwrap(),
                    )],
                ),
            )
            .await
            .unwrap();
        tx.execute(
            "DELETE FROM ir_model_data WHERE model = 'res.groups' AND res_id = $1",
            &[&locations],
        )
        .await
        .unwrap();
        let display = erp_addon_base::group_graph::display_names(&mut tx, &[locations])
            .await
            .unwrap()
            .remove(0)
            .1;
        tx.commit().await.unwrap();
        drop(tx);
        let orm = super::security::secure(&db, &installed).await;
        let mut tx = orm
            .begin(erp_orm::UserId::new(actors[0]).unwrap())
            .await
            .unwrap();
        let env = tx.env().with_company(company).unwrap();
        let refused = toggle_group(&env, &mut tx, "stock.group_stock_tracking_lot", false)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Validation);
        assert_eq!(
            refused.message(),
            format!(
                "Packages stays on for every internal user through the group {display}; change that group instead."
            )
        );
        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

// The group switch through the real JSON-2 dispatch: a stock manager (no Settings administrator) is refused with
// Access before anything changes.
#[test]
fn inventory_group_switch_through_rpc_refuses_a_stock_manager() {
    run(async {
        use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let installed = install(&db).await;
        let actors = super::security::roles(&installed).await;
        let orm = super::security::secure(&db, &installed).await;
        let auth = Auth::new(
            orm.clone(),
            erp_security::auth::AuthConfig::DEFAULT,
            HashPool::global(),
        )
        .await
        .unwrap();
        let service = erp_rpc::RpcService::new(
            orm.clone(),
            super::security::Host,
            erp_rpc::RpcConfig::default(),
        );
        // The stock manager of the role fixture.
        let login = "stock-security-3";
        let logged = auth
            .authenticate(LoginRequest::new(
                login,
                format!("{login}-fixture-password"),
                ClientAddr::InProcess("stock-settings-test"),
            ))
            .await
            .unwrap();
        let mut begun = auth
            .begin_session(logged.session().secret(), CredentialOptions::new())
            .await
            .unwrap();
        let principal = begun.tx.env().principal().clone();
        begun.tx.rollback().await.unwrap();
        drop(begun);
        let response = service
            .call(erp_rpc::RpcCall::json2(
                "/json/2/ir.config.setting/erp_toggle_group",
                serde_json::to_vec(&serde_json::json!({
                    "key": "stock.group_stock_tracking_lot",
                    "enabled": true
                }))
                .unwrap(),
                erp_rpc::Caller::Principal(principal),
            ))
            .await;
        let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(response.status(), 403, "{body}");
        assert_eq!(body["name"], "erp.exceptions.AccessError");
        assert_eq!(
            body["message"],
            "Only Settings administrators may turn a feature on or off for every user."
        );
        assert!(!holds(&db, &installed, actors[1], "stock.group_tracking_lot").await);
        db.cleanup().await.unwrap();
    });
}
