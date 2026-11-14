// SPDX-License-Identifier: MIT OR Apache-2.0
//! The `stock` addon installed into a real database: the default topology
//! records load from the manifest, each default location has its policy row, the topology version
//! row exists once, and a capacity row per product is unique. Table-level checks per model are in
//! `addons/stock/tests/schema.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use erp_addon_base::ADDON;
use std::path::Path;

use erp_fields::{Cell, Decimal, ErrorKind};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, Orm, RawVals, SUPERUSER_ID};
use erp_testkit::TestDb;

#[path = "support/install_defaults.rs"]
mod install_defaults;

#[path = "support/stock_access.rs"]
mod stock_access;

#[path = "stock_install/settings.rs"]
mod settings;

#[path = "stock_install/quant_unit.rs"]
mod quant_unit;

#[path = "stock_install/security.rs"]
mod security;

#[path = "stock_install/upgrade_from_main.rs"]
mod upgrade_from_main;

#[path = "stock_install/policy_migration_utc.rs"]
mod policy_migration_utc;

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

async fn install(db: &TestDb) -> Orm {
    let system = system(db);
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
    installed.orm
}

async fn strings(orm: &Orm, sql: &str) -> Vec<String> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    rows.iter().map(|row| row.get::<_, String>(0)).collect()
}

/// one topology version row at generation 0; generated warehouse roles, a second install pass and
/// extension retirement come with the warehouse and route slices)
/// the raw-insert trigger, the verifier and the usage-change refusals come later)
#[test]
fn stock_installs_its_default_topology_records() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;

        for (sql, expected) in install_defaults::STOCK {
            assert_eq!(strings(&orm, sql).await, *expected, "{sql}");
        }
        let unguarded = strings(
            &orm,
            "SELECT l.id::text FROM stock_location l \
             WHERE NOT EXISTS (SELECT 1 FROM stock_location_policy p WHERE p.location_id = l.id)",
        )
        .await;
        assert!(
            unguarded.is_empty(),
            "locations without policy row: {unguarded:?}"
        );

        for table in [
            "stock_warehouse",
            "stock_picking_type",
            "stock_rule",
            "stock_storage_category",
            "stock_package_type",
            "stock_warehouse_orderpoint",
            "stock_putaway_rule",
        ] {
            let count = strings(&orm, &format!("SELECT count(*)::text FROM {table}")).await;
            assert_eq!(count, ["0"], "{table} has no default records");
        }
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_installs_the_orderpoint_sequence() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let rows = strings(
            &orm,
            "SELECT d.name || '=' || s.name || '/' || s.code || '/' || s.prefix || '/' || s.padding::text \
             || '/' || s.number_next::text || '/' || s.number_increment::text || '/' \
             || s.implementation || '/' || coalesce(s.company_id::text, 'shared') \
             FROM ir_model_data d JOIN ir_sequence s ON s.id = d.res_id \
             WHERE d.module = 'stock' AND d.model = 'ir.sequence' ORDER BY d.name",
        )
        .await;
        assert_eq!(
            rows,
            ["sequence_mrp_op=Stock orderpoint/stock.orderpoint/OP//5/1/1/standard/shared"]
        );
        // the sequence works: one draw by code gives prefix plus the zero-padded first number, both
        // read from the row (`OP/00001`)
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let row = tx
            .query(
                "SELECT prefix, padding FROM ir_sequence WHERE code = 'stock.orderpoint'",
                &[],
            )
            .await
            .unwrap();
        let prefix: String = row[0].get(0);
        let padding = usize::try_from(row[0].get::<_, i32>(1)).unwrap();
        let env = tx.env();
        let drawn = erp_orm::sequence::next_by_code(&mut tx, &env, "stock.orderpoint", None)
            .await
            .unwrap()
            .expect("the stock.orderpoint sequence is drawn");
        assert_eq!(drawn, format!("{prefix}{:0>padding$}", 1));
        assert_eq!(drawn, "OP/00001");
        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

/// definition is asserted in `addons/stock/tests/schema.rs`)
#[test]
fn storage_capacity_refuses_a_second_row_for_one_product() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let category = tx
            .env()
            .records("stock.storage.category")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Shelves".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let product = tx
            .env()
            .records("product.product")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Bolt".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let capacity = move |quantity: i32| {
            RawVals::new()
                .set_cell("storage_category_id", Cell::Id(category))
                .set_cell("product_id", Cell::Id(product))
                .set_cell("quantity", Cell::Decimal(Decimal::from(quantity)))
        };
        let capacities = tx.env().records("stock.storage.category.capacity").unwrap();
        capacities
            .create(&mut tx, vec![capacity(10)])
            .await
            .unwrap();
        let error = tx
            .savepoint(async move |tx| capacities.create(tx, vec![capacity(5)]).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
        assert_eq!(
            error.message(),
            "The database refused the change: A storage category takes one capacity rule per product."
        );
        assert_eq!(
            error.constraint(),
            Some("stock_storage_category_capacity_unique_product")
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

/// `addons/stock/tests/schema.rs`)
///
/// A capacity row of a category of one company refuses a product restricted to another company and
/// accepts one of its own (S2).
#[test]
fn storage_capacity_refuses_a_product_of_another_company() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let rows = tx
            .query(
                "SELECT id, currency_id FROM res_company ORDER BY id LIMIT 1",
                &[],
            )
            .await
            .unwrap();
        let a: i64 = rows[0].get(0);
        let currency: i64 = rows[0].get(1);
        let b = tx
            .env()
            .with_company(a)
            .unwrap()
            .records("res.company")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Other".into()))
                        .set_cell("currency_id", Cell::Id(currency)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let category = tx
            .env()
            .records("stock.storage.category")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Shelves A".into()))
                        .set_cell("company_id", Cell::Id(a)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let mut products = Vec::new();
        for (company, name) in [(a, "Bolt A"), (b, "Bolt B")] {
            products.push(
                tx.env()
                    .records("product.product")
                    .unwrap()
                    .create(
                        &mut tx,
                        vec![
                            RawVals::new()
                                .set_cell("name", Cell::Str(name.into()))
                                .set_cell("company_id", Cell::Id(company)),
                        ],
                    )
                    .await
                    .unwrap()
                    .ids()[0],
            );
        }
        let capacity = move |product: i64| {
            RawVals::new()
                .set_cell("storage_category_id", Cell::Id(category))
                .set_cell("product_id", Cell::Id(product))
                .set_cell("quantity", Cell::Decimal(Decimal::from(3)))
        };
        let capacities = tx.env().records("stock.storage.category.capacity").unwrap();
        let foreign = products[1];
        let refused = capacities.clone();
        let error = tx
            .savepoint(async move |tx| refused.create(tx, vec![capacity(foreign)]).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
        assert!(error.message().contains("company"), "{}", error.message());
        capacities
            .create(&mut tx, vec![capacity(products[0])])
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

/// Installed defaults feed operation-type configuration; the sequence remains administrator-editable.
#[test]
fn operation_types_use_installed_locations_and_standard_sequences() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company: i64=tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='main_company'",&[]).await.unwrap()[0].get(0);
        let stock = tx
            .env()
            .records("stock.location")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Installed stock".into()))
                        .set_cell("company_id", Cell::Id(company)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let warehouse = tx
            .env()
            .records("stock.warehouse")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Installed warehouse".into()))
                        .set_cell("code", Cell::Str("IW".into()))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("view_location_id", Cell::Id(stock))
                        .set_cell("lot_stock_id", Cell::Id(stock)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let operation = tx
            .env()
            .records("stock.picking.type")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Installed receipt".into()))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("warehouse_id", Cell::Id(warehouse))
                        .set_cell("sequence_code", Cell::Str("IW/IN/".into())),
                ],
            )
            .await
            .unwrap();
        let vendors: i64=tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name='stock_location_suppliers'",&[]).await.unwrap()[0].get(0);
        assert_eq!(
            operation
                .cell(&mut tx, "default_location_src_id")
                .await
                .unwrap(),
            Cell::Id(vendors)
        );
        assert_eq!(
            operation
                .cell(&mut tx, "default_location_dest_id")
                .await
                .unwrap(),
            Cell::Id(stock)
        );
        assert_eq!(
            operation
                .cell(&mut tx, "reservation_days_before")
                .await
                .unwrap(),
            Cell::Int(0)
        );
        let Cell::Id(sequence) = operation.cell(&mut tx, "sequence_id").await.unwrap() else {
            panic!("missing sequence")
        };
        let sequence = tx
            .env()
            .records("ir.sequence")
            .unwrap()
            .browse([sequence])
            .unwrap();
        assert_eq!(
            sequence.cell(&mut tx, "implementation").await.unwrap(),
            Cell::Str("standard".into())
        );
        assert_eq!(
            sequence.cell(&mut tx, "prefix").await.unwrap(),
            Cell::Str("IW/IN/".into())
        );
        let id = operation.ids()[0];
        let error = tx
            .savepoint(async |tx| {
                tx.execute(
                    "UPDATE stock_picking_type SET reservation_days_before=366 WHERE id=$1",
                    &[&id],
                )
                .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.constraint(),
            Some("stock_picking_type_reservation_days_bounds")
        );
        assert_eq!(
            error.message(),
            "The database refused the change: stock.picking.type reservation lead times must be between 0 and 365 days."
        );
        let error=tx.savepoint(async |tx| tx.execute("UPDATE stock_picking_type SET reservation_method='by_date',reservation_days_before_priority=366 WHERE id=$1",&[&id]).await).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.constraint(),
            Some("stock_picking_type_reservation_days_bounds")
        );
        assert_eq!(
            error.message(),
            "The database refused the change: stock.picking.type reservation lead times must be between 0 and 365 days."
        );
        let error = tx
            .savepoint(async |tx| {
                tx.execute(
                    "UPDATE stock_picking_type SET reservation_days_before=1 WHERE id=$1",
                    &[&id],
                )
                .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.constraint(),
            Some("stock_picking_type_reservation_days_method")
        );
        assert_eq!(
            error.message(),
            "The database refused the change: stock.picking.type reservation lead times must be zero unless reservation_method is by_date."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

/// Current-binary lifecycle fixture with the old nullable columns restored. This is not a prior-binary rehearsal.
#[test]
fn operation_type_upgrade_backfills_missing_days_and_preserves_configuration() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company: i64=tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='main_company'",&[]).await.unwrap()[0].get(0);
        let location: i64=tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name='stock_location_suppliers'",&[]).await.unwrap()[0].get(0);
        let mut ids = Vec::new();
        for name in ["Legacy missing days", "Preserved priority days"] {
            let created = tx
                .env()
                .records("stock.picking.type")
                .unwrap()
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("company_id", Cell::Id(company))
                            .set_cell("default_location_src_id", Cell::Id(location))
                            .set_cell("default_location_dest_id", Cell::Id(location)),
                    ],
                )
                .await
                .unwrap();
            ids.push(created.ids()[0]);
        }
        tx.flush().await.unwrap();
        tx.batch("ALTER TABLE stock_picking_type DROP CONSTRAINT stock_picking_type_reservation_days_bounds; ALTER TABLE stock_picking_type DROP CONSTRAINT stock_picking_type_reservation_days_method; ALTER TABLE stock_picking_type ALTER COLUMN reservation_days_before DROP NOT NULL; ALTER TABLE stock_picking_type ALTER COLUMN reservation_days_before_priority DROP NOT NULL;").await.unwrap();
        tx.execute("UPDATE stock_picking_type SET reservation_days_before=NULL,reservation_days_before_priority=NULL WHERE id=$1",&[&ids[0]]).await.unwrap();
        tx.execute("UPDATE stock_picking_type SET reservation_method='by_date',reservation_days_before=365,reservation_days_before_priority=7 WHERE id=$1",&[&ids[1]]).await.unwrap();
        tx.execute(
            "UPDATE ir_module_module SET latest_version='1.0.0' WHERE name='stock'",
            &[],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        drop(orm);
        let sys = system(&db);
        let upgraded = sys
            .button_immediate_upgrade(
                &sys.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap();
        assert!(upgraded.post_commit.is_clean());
        let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
        let rows=tx.query("SELECT reservation_days_before::int8,reservation_days_before_priority::int8 FROM stock_picking_type WHERE id=ANY($1) ORDER BY id",&[&ids]).await.unwrap();
        assert_eq!((rows[0].get::<_, i64>(0), rows[0].get::<_, i64>(1)), (0, 0));
        assert_eq!(
            (rows[1].get::<_, i64>(0), rows[1].get::<_, i64>(1)),
            (365, 7)
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let repeated = sys
            .button_immediate_upgrade(
                &sys.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap();
        assert!(repeated.post_commit.is_clean());
        db.cleanup().await.unwrap();
    });
}

/// an explicit upgrade adds and backfills the ownership column for an existing tree.
/// The fixture removes only the new derived column to represent the pre-LOC location schema;
/// it is not a claim that an older executable was run.
#[test]
fn stock_upgrade_backfills_location_warehouse_ownership() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company: i64 = tx
            .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
            .await
            .unwrap()[0]
            .get(0);
        let env = tx.env();
        let locations = env.records("stock.location").unwrap();
        let view = locations
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Upgrade root".into()))
                        .set_cell("usage", Cell::Str("view".into())),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let leaf = locations
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Upgrade leaf".into()))
                        .set_cell("location_id", Cell::Id(view)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let warehouse = env
            .records("stock.warehouse")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Upgrade warehouse".into()))
                        .set_cell("code", Cell::Str("UPG".into()))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("view_location_id", Cell::Id(view))
                        .set_cell("lot_stock_id", Cell::Id(leaf)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.flush().await.unwrap();
        assert_eq!(
            locations
                .browse(vec![leaf])
                .unwrap()
                .cell(&mut tx, "warehouse_id")
                .await
                .unwrap(),
            Cell::Id(warehouse)
        );
        tx.query("ALTER TABLE stock_location DROP COLUMN warehouse_id", &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let system = system(&db);
        let upgraded = system
            .button_immediate_upgrade(
                &system.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap();
        assert!(
            upgraded.post_commit.is_clean(),
            "{:?}",
            upgraded.post_commit
        );
        let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
        let rows = tx
            .query(
                "SELECT warehouse_id::int8 FROM stock_location WHERE id = ANY($1) ORDER BY id",
                &[&vec![view, leaf]],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.get::<_, Option<i64>>(0), Some(warehouse));
        }
        let orphan = tx.query("SELECT count(*)::int8 FROM stock_location l WHERE NOT EXISTS (SELECT FROM stock_location_policy p WHERE p.location_id=l.id)", &[]).await.unwrap();
        assert_eq!(orphan[0].get::<_, i64>(0), 0);
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}
