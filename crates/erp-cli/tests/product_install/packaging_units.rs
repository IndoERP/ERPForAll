// SPDX-License-Identifier: MIT OR Apache-2.0
//! Packaging with installed metadata and real group policy, without unit-test feature probes.
use super::*;
use erp_fields::{Decimal, Value};
use erp_sdk::{Domain, Operator};

#[test]
fn installed_packaging_helpers_use_global_feature_state() {
    big_stack(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
            .await
            .unwrap();
        let orm = Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .resolver(Resolver::for_registry(installed.registry()).unwrap())
            .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
            .impls(erp_modules::base_models::IMPLS.iter().copied())
            .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .unwrap();
        let main = xid(&orm, "base", "main_company").await;
        let root = xid(&orm, "uom", "product_uom_unit").await;
        let manager_group = xid(&orm, "product", "group_product_manager").await;
        let feature = xid(&orm, "uom", "group_uom").await;
        let system_group = xid(&orm, "base", "group_system").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env().with_company(main).unwrap();
        let units = env.records("uom.uom").unwrap();
        let mut extras = Vec::new();
        for (name, factor) in [("Template carton", 6), ("Variant carton", 12)] {
            extras.push(
                units
                    .create(
                        &mut tx,
                        vec![
                            RawVals::new()
                                .set_cell("name", Cell::Str(name.into()))
                                .set_cell("relative_uom_id", Cell::Id(root))
                                .set_cell("relative_factor", Cell::Decimal(Decimal::from(factor))),
                        ],
                    )
                    .await
                    .unwrap()
                    .ids()[0],
            );
        }
        let t = env
            .records("product.template")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Installed packaging".into()))
                        .set_cell("company_id", Cell::Id(main))
                        .set_cell(
                            "uom_ids",
                            Cell::Ids(vec![RecId::from_real(extras[0]).unwrap()].into()),
                        ),
                ],
            )
            .await
            .unwrap();
        let p = env
            .records("product.product")
            .unwrap()
            .search(
                &mut tx,
                &Domain::condition("product_tmpl_id", Operator::Eq, t.ids()[0]).unwrap(),
            )
            .await
            .unwrap();
        p.write(
            &mut tx,
            RawVals::new().set_cell(
                "extra_uom_ids",
                Cell::Ids(vec![RecId::from_real(extras[1]).unwrap()].into()),
            ),
        )
        .await
        .unwrap();
        // The base user invariant requires a regular administrator before a manager.
        env.records("res.users")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Packaging administrator".into()))
                        .set_cell("login", Cell::Str("packaging-administrator".into()))
                        .set_cell("company_id", Cell::Id(main))
                        .set_commands(
                            "company_ids",
                            [Command::set([RecId::from_real(main).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set([RecId::from_real(system_group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        let manager = env
            .records("res.users")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Packaging manager".into()))
                        .set_cell("login", Cell::Str("packaging-manager".into()))
                        .set_cell("company_id", Cell::Id(main))
                        .set_commands(
                            "company_ids",
                            [Command::set([RecId::from_real(main).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set([
                                RecId::from_real(manager_group).unwrap(),
                                RecId::from_real(feature).unwrap(),
                            ])],
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let (tid, pid) = (t.ids()[0], p.ids()[0]);
        tx.commit().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(UserId::new(manager).unwrap()).await.unwrap();
        let p = tx
            .env()
            .records("product.product")
            .unwrap()
            .browse([pid])
            .unwrap();
        let disabled: erp_orm::Records = p.call(&mut tx, "_get_available_uoms", ()).await.unwrap();
        assert_eq!(
            disabled.ids(),
            vec![root],
            "caller membership must not enable the global feature"
        );
        let raw: Value = p
            .call(&mut tx, "action_open_packaging_barcodes", ())
            .await
            .unwrap();
        let normalized: Value = p
            .call(&mut tx, "erp_normalize_action_result", (1_u64, raw))
            .await
            .unwrap();
        let Value::Json(envelope) = normalized else {
            panic!("normalization envelope")
        };
        assert_eq!(envelope["result"]["model"], "product.uom");
        // A unit reached through a current-window action carries caller selection metadata.
        let unit = tx
            .env()
            .with_context([
                ("active_model", Value::str("uom.uom")),
                ("active_id", Value::Int(root)),
                ("active_ids", Value::list([Value::Int(root)])),
                ("product_ids", Value::list([Value::Int(pid)])),
            ])
            .records("uom.uom")
            .unwrap()
            .browse([root])
            .unwrap();
        let raw: Value = unit
            .call(&mut tx, "action_open_packaging_barcodes", ())
            .await
            .unwrap();
        let normalized: Value = unit
            .call(&mut tx, "erp_normalize_action_result", (1_u64, raw))
            .await
            .unwrap();
        let Value::Json(envelope) = normalized else {
            panic!("unit normalization envelope")
        };
        assert_eq!(envelope["result"]["model"], "product.uom");
        assert_eq!(envelope["result"]["context"]["default_uom_id"], root);
        assert!(envelope["result"]["context"].get("active_id").is_none());
        assert!(envelope["result"]["context"].get("active_ids").is_none());
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.env()
            .records("res.users")
            .unwrap()
            .browse([i64::from(SUPERUSER_ID)])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands(
                    "group_ids",
                    [Command::link(RecId::from_real(feature).unwrap())],
                ),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(UserId::new(manager).unwrap()).await.unwrap();
        let t = tx
            .env()
            .records("product.template")
            .unwrap()
            .browse([tid])
            .unwrap();
        let p = tx
            .env()
            .records("product.product")
            .unwrap()
            .browse([pid])
            .unwrap();
        let template_units: erp_orm::Records =
            t.call(&mut tx, "_get_available_uoms", ()).await.unwrap();
        let variant_units: erp_orm::Records =
            p.call(&mut tx, "_get_available_uoms", ()).await.unwrap();
        assert_eq!(template_units.ids(), vec![root, extras[0]]);
        assert_eq!(variant_units.ids(), vec![root, extras[0], extras[1]]);
        let multiple: bool = p.call(&mut tx, "_has_multiple_uoms", ()).await.unwrap();
        assert!(multiple);
        tx.rollback().await.unwrap();
        drop(tx);
        drop(orm);
        drop(installed);
        db.cleanup().await.unwrap();
    });
}
