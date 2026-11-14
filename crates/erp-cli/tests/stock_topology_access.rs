// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installed topology permissions let employees read public topology, but deny management.
//! System helpers remain private; administrators remain subject to ownership guards.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use std::path::Path;
use std::sync::Arc;

use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID, UserId};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

fn system(db: &TestDb) -> ModuleSystem {
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
    ModuleSystem::builder(db.pool().clone(), index)
        .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_barcodes::ADDON,
            &erp_addon_barcodes_gs1_nomenclature::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_stock::ADDON,
        ])
        .build()
        .unwrap()
}

/// Run `body` on a thread with a large stack: a full install chain exceeds the default one.
fn run(body: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(body);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[path = "support/stock_access.rs"]
mod stock_access;
use stock_access::refusal;

#[test]
fn internal_user_reads_topology_but_cannot_manage_it() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let system = system(&db);
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        assert!(initialized.post_commit.is_clean());
        let installed = system
            .button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap();
        assert!(
            installed.post_commit.is_clean(),
            "{:?}",
            installed.post_commit
        );
        let installed = installed.orm;
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        let mut ids = Vec::new();
        for name in ["main_company", "group_system", "group_user"] {
            let rows = tx
                .query(
                    "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = $1",
                    &[&name],
                )
                .await
                .unwrap();
            ids.push(rows[0].get::<_, i64>(0));
        }
        let company = ids[0];
        // the base rules want an administrator before an ordinary user is created
        let mut users = Vec::new();
        for (login, group) in [("stock-admin", ids[1]), ("stock-clerk", ids[2])] {
            let created = tx
                .env()
                .records("res.users")
                .unwrap()
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(login.into()))
                            .set_cell("login", Cell::Str(login.into()))
                            .set_cell(
                                "password",
                                Cell::Str(format!("{login}-fixture-password").into()),
                            )
                            .set_cell("company_id", Cell::Id(company))
                            .set_commands(
                                "company_ids",
                                [Command::Set(vec![RecId::from_real(company).unwrap()])],
                            )
                            .set_commands(
                                "group_ids",
                                [Command::Set(vec![RecId::from_real(group).unwrap()])],
                            ),
                    ],
                )
                .await
                .unwrap();
            users.push(created.ids()[0]);
        }
        let user = users[1];
        // Shared required links isolate ownership validation from link consistency.
        let shared = tx
            .env()
            .records("stock.location")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Shared fixture location".into()))
                        .set_cell("usage", Cell::Str("internal".into())),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let mut topology = Vec::new();
        for (model, description, values) in [
            (
                "stock.warehouse",
                "Warehouse",
                RawVals::new()
                    .set_cell("code", Cell::Str("GUARD".into()))
                    .set_cell("view_location_id", Cell::Id(shared))
                    .set_cell("lot_stock_id", Cell::Id(shared)),
            ),
            (
                "stock.picking.type",
                "Picking Type",
                RawVals::new()
                    .set_cell("default_location_src_id", Cell::Id(shared))
                    .set_cell("default_location_dest_id", Cell::Id(shared)),
            ),
        ] {
            let created = tx
                .env()
                .records(model)
                .unwrap()
                .create(
                    &mut tx,
                    vec![
                        values
                            .set_cell("name", Cell::Str("Ownership fixture".into()))
                            .set_cell("company_id", Cell::Id(company)),
                    ],
                )
                .await
                .unwrap();
            topology.push((model, description, created.ids()[0]));
        }
        tx.install_audit_schema().await.unwrap();
        // Route defaults survive the real stock installer; no warehouse-generated rule is invented.
        let defaults = tx.query(
            "SELECT r.active, r.sequence, r.product_categ_selectable FROM stock_route r JOIN ir_model_data d ON d.res_id=r.id AND d.model='stock.route' WHERE d.module='stock' AND d.name='route_warehouse0_mto'", &[]).await.unwrap();
        assert_eq!(defaults.len(), 1);
        assert!(!defaults[0].get::<_, bool>(0));
        assert_eq!(defaults[0].get::<_, i32>(1), 5);
        assert!(defaults[0].get::<_, bool>(2));
        let version = tx
            .query("SELECT count(*)::int8 FROM stock_topology_version", &[])
            .await
            .unwrap();
        assert_eq!(version[0].get::<_, i64>(0), 1);
        let route = tx
            .env()
            .records("stock.route")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Route lifecycle fixture".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let rule = tx
            .env()
            .records("stock.rule")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Rule lifecycle fixture".into()))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("route_id", Cell::Id(route))
                        .set_cell("location_dest_id", Cell::Id(shared))
                        .set_cell("picking_type_id", Cell::Id(topology[1].2)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.commit().await.unwrap();
        drop(tx);
        // the compiled policy of the installed access data, as the server builds it
        let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
            .await
            .unwrap();
        let orm = Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
            .generation_attachment(erp_orm::GenerationAttachment::new(
                Arc::clone(installed.registry()),
                installed
                    .env(SUPERUSER_ID, Context::new())
                    .generation_attachment::<erp_views::PublishedViewTypes>()
                    .unwrap(),
            ))
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .resolver(Resolver::for_registry(installed.registry()).unwrap())
            .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
            .impls(erp_modules::base_models::IMPLS.iter().copied())
            .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .unwrap();
        let mut tx = orm.begin(UserId::new(user).unwrap()).await.unwrap();
        for (model, description) in [
            ("stock.location", "Inventory Locations"),
            ("stock.location.policy", "Location Stock Policy"),
            ("product.removal", "Removal Strategy"),
            ("stock.warehouse", "Warehouse"),
            ("stock.picking.type", "Picking Type"),
            ("stock.route", "Inventory Routes"),
            ("stock.rule", "Stock Rule"),
            ("stock.storage.category", "Storage Category"),
            (
                "stock.storage.category.capacity",
                "Storage Category Capacity",
            ),
            ("stock.package.type", "Stock package type"),
            ("stock.topology.version", "Stock Topology Version"),
        ] {
            let records = tx.env().records(model).unwrap();
            let read = records
                .search_count(&mut tx, &erp_sdk::Domain::constant(true), None)
                .await;
            if matches!(model, "stock.package.type" | "stock.topology.version") {
                let error = read.unwrap_err();
                assert_eq!(error.kind(), erp_fields::ErrorKind::Access, "{model}");
                assert_eq!(
                    error.message(),
                    refusal(&orm, "access", "r", description, model).await
                );
            } else {
                read.unwrap();
            }
            let error = records
                .create(&mut tx, vec![RawVals::new()])
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Access, "{model}");
            assert_eq!(
                error.message(),
                refusal(&orm, "create", "c", description, model).await
            );
            if model == "stock.location.policy" {
                let error = records
                    .create(
                        &mut tx,
                        vec![
                            RawVals::new()
                                .set_cell("location_id", Cell::Id(shared))
                                .set_cell("forbid_negative", Cell::Bool(true)),
                        ],
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
                assert_eq!(
                    error.message(),
                    refusal(&orm, "create", "c", description, model).await
                );
            }
        }
        for (model, id, description) in [
            ("stock.route", route, "Inventory Routes"),
            ("stock.rule", rule, "Stock Rule"),
        ] {
            let row = tx.env().records(model).unwrap().browse([id]).unwrap();
            let error = row
                .write(
                    &mut tx,
                    RawVals::new().set_cell("active", Cell::Bool(false)),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
            assert_eq!(
                error.message(),
                refusal(&orm, "modify", "u", description, model).await
            );
        }
        // Derived ownership is readable, but employees cannot overwrite it.
        let location = tx
            .env()
            .records("stock.location")
            .unwrap()
            .browse(vec![shared])
            .unwrap();
        assert_eq!(
            location.cell(&mut tx, "warehouse_id").await.unwrap(),
            Cell::Id(topology[0].2)
        );
        let error = location
            .write(&mut tx, RawVals::new().set_cell("warehouse_id", Cell::Null))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
        assert_eq!(
            error.message(),
            refusal(&orm, "modify", "u", "Inventory Locations", "stock.location").await
        );
        // A denied caller must receive the base Access error before ownership or type checks.
        for &(model, description, id) in &topology {
            let record = tx.env().records(model).unwrap().browse(vec![id]).unwrap();
            for value in [Cell::Null, Cell::Str("malformed".into()), Cell::Id(company)] {
                let error = record
                    .write(&mut tx, RawVals::new().set_cell("company_id", value))
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
                assert_eq!(
                    error.message(),
                    refusal(&orm, "modify", "u", description, model).await
                );
            }
        }
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(UserId::new(users[0]).unwrap()).await.unwrap();
        let route_record = tx
            .env()
            .records("stock.route")
            .unwrap()
            .browse([route])
            .unwrap();
        let rule_record = tx
            .env()
            .records("stock.rule")
            .unwrap()
            .browse([rule])
            .unwrap();
        for active in [false, true] {
            route_record
                .write(
                    &mut tx,
                    RawVals::new().set_cell("active", Cell::Bool(active)),
                )
                .await
                .unwrap();
            assert_eq!(
                rule_record.cell(&mut tx, "active").await.unwrap(),
                Cell::Bool(active)
            );
        }
        for &(model, _, id) in &topology {
            let record = tx.env().records(model).unwrap().browse(vec![id]).unwrap();
            record
                .write(
                    &mut tx,
                    RawVals::new()
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("sequence", Cell::Int(37)),
                )
                .await
                .unwrap();
            let error = record
                .write(
                    &mut tx,
                    RawVals::new()
                        .set_cell("company_id", Cell::Null)
                        .set_cell("sequence", Cell::Int(99)),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::UserError);
            assert_eq!(
                error.message(),
                "The company of this record cannot be changed once it exists. Archive it and create a new record for the other company."
            );
            assert_eq!(
                record.cell(&mut tx, "company_id").await.unwrap(),
                Cell::Id(company)
            );
            assert_eq!(
                record.cell(&mut tx, "sequence").await.unwrap(),
                Cell::Int(37)
            );
        }
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}
