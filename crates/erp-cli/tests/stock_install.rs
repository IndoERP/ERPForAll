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

        let removals = strings(
            &orm,
            "SELECT d.name || '=' || r.method || '/' || (r.name->>'en_US') FROM ir_model_data d \
             JOIN product_removal r ON r.id = d.res_id \
             WHERE d.module = 'stock' AND d.model = 'product.removal' ORDER BY d.name",
        )
        .await;
        assert_eq!(
            removals,
            [
                "removal_closest=closest/Closest Location",
                "removal_fifo=fifo/First In First Out (FIFO)",
                "removal_least_packages=least_packages/Least Packages",
                "removal_lifo=lifo/Last In First Out (LIFO)",
            ]
        );

        let locations = strings(
            &orm,
            "SELECT d.name || '=' || l.usage || '/' || l.complete_name || '/' || l.active::text \
             || '/' || coalesce(l.company_id::text, 'shared') || '/' || count(p.id)::text \
             FROM ir_model_data d JOIN stock_location l ON l.id = d.res_id \
             LEFT JOIN stock_location_policy p ON p.location_id = l.id AND NOT p.forbid_negative \
             WHERE d.module = 'stock' AND d.model = 'stock.location' \
             GROUP BY d.name, l.id ORDER BY d.name",
        )
        .await;
        assert_eq!(
            locations,
            [
                "stock_location_customers=customer/Customers/true/shared/1",
                "stock_location_inter_company=transit/Inter-company transit/false/shared/1",
                "stock_location_suppliers=supplier/Vendors/true/shared/1",
            ]
        );
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

        let routes = strings(
            &orm,
            "SELECT d.name || '=' || (r.name->>'en_US') || '/' || r.active::text || '/' || \
             r.sequence::text || '/' || r.product_selectable::text || '/' || \
             r.product_categ_selectable::text || '/' || coalesce(r.company_id::text, 'shared') \
             FROM ir_model_data d JOIN stock_route r ON r.id = d.res_id \
             WHERE d.module = 'stock' AND d.model = 'stock.route' ORDER BY d.name",
        )
        .await;
        assert_eq!(
            routes,
            ["route_warehouse0_mto=Replenish on Order (MTO)/false/5/true/true/shared"]
        );

        let versions = strings(
            &orm,
            "SELECT d.name || '=' || v.generation::text FROM stock_topology_version v \
             LEFT JOIN ir_model_data d ON d.res_id = v.id AND d.model = 'stock.topology.version'",
        )
        .await;
        assert_eq!(versions, ["stock_topology_version=0"]);

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
