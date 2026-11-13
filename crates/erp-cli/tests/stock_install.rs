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

use std::path::Path;

use erp_fields::{Cell, Decimal, ErrorKind};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, Orm, RawVals, SUPERUSER_ID};
use erp_testkit::TestDb;

#[path = "support/install_defaults.rs"]
mod install_defaults;

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
            "The operation cannot be completed: Multiple capacity rules for one product."
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
