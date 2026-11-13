// SPDX-License-Identifier: MIT OR Apache-2.0
//! Upgrading `stock` onto a database without the tracking tables (S5,
//! ): what `button_immediate_upgrade` does when the
//! manifest version is unchanged, and when it is higher than the installed one.
//!
//! These are current-schema lifecycle fixtures, NOT previous-binary evidence. Each installs the
//! current `stock`, then drops the five tracking tables (`stock_serial_position`, `stock_quant`,
//! `stock_lot_name_scope`, `stock_lot`, `stock_package`) to stand in for a database that had stock
//! before, after the three move line tables that reference
//! them (such a database had none of those either). Unlike such a database it keeps the tracking models' `ir_model`,
//! field, access and `ir_model_data` rows, which the earlier stock never wrote. A higher manifest
//! version is presented by rewinding the installed version marker (`latest_version`) below the
//! manifest's, not by a second manifest: the engine compares only those two values. A changed
//! artifact is presented by giving the installed receipt and `identity_previous` one digest that
//! is not the current files', as a release upgrade over an older release does
//! (`scripts/human-activate.py`, `--identity-previous`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::collections::BTreeMap;
use std::path::Path;

use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem, ModuleSystemBuilder};
use erp_orm::Context;
use erp_sdk::ErrorKind;
use erp_testkit::TestDb;

/// The tracking tables and the move line tables that reference them, dropped in an order their
/// foreign keys allow.
const TRACKING_TABLES: [&str; 8] = [
    "stock_move_line_correction",
    "stock_move_line_serial",
    "stock_move_line",
    "stock_serial_position",
    "stock_quant",
    "stock_lot_name_scope",
    "stock_lot",
    "stock_package",
];

/// The constraints S1, `table.constraint`.
const S1_CONSTRAINTS: [&str; 3] = [
    "stock_lot.stock_lot_id_product",
    "stock_quant.stock_quant_lot_product",
    "stock_serial_position.stock_serial_position_lot_product",
];

/// A digest no addon folder has: the receipt of the "earlier" stock artifact.
const EARLIER_STOCK: &str =
    "addon-v1:0000000000000000000000000000000000000000000000000000000000000001";

fn addons() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../addons")
        .canonicalize()
        .unwrap()
}

fn current_identities() -> BTreeMap<ModuleName, String> {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons());
    index
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
        .collect()
}

fn builder(db: &TestDb) -> ModuleSystemBuilder {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons());
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(current_identities())
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

fn stock() -> Vec<ModuleName> {
    vec![ModuleName::new("stock").unwrap()]
}

/// Install `stock`, drop the tracking tables and set the installed version marker to `installed`.
async fn stock_without_tracking_tables(db: &TestDb, installed: &str) {
    let system = builder(db).build().unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    assert!(initialized.post_commit.is_clean());
    let installed_outcome = system
        .button_immediate_install(&system.system_env(Context::new()), &stock())
        .await
        .unwrap();
    assert!(installed_outcome.post_commit.is_clean());
    drop(installed_outcome);
    let client = db.client().await.unwrap();
    for table in TRACKING_TABLES {
        client
            .batch_execute(&format!("DROP TABLE {table}"))
            .await
            .unwrap_or_else(|e| panic!("drop {table}: {e:?}"));
    }
    client
        .execute(
            "UPDATE ir_module_module SET latest_version = $1 WHERE name = 'stock'",
            &[&installed],
        )
        .await
        .unwrap();
    assert_eq!(tables(db).await, Vec::<String>::new());
}

/// Give the installed `stock` receipt the earlier artifact's digest, and return the previous
/// release identities a release upgrade passes (`identity_previous`).
async fn earlier_stock_artifact(db: &TestDb) -> BTreeMap<ModuleName, String> {
    let client = db.client().await.unwrap();
    let changed = client
        .execute(
            "UPDATE installed_addon_identity SET content_hash = $1 WHERE name = 'stock'",
            &[&EARLIER_STOCK],
        )
        .await
        .unwrap();
    assert_eq!(changed, 1);
    let mut previous = current_identities();
    previous.insert(ModuleName::new("stock").unwrap(), EARLIER_STOCK.to_owned());
    previous
}

/// The tracking tables that exist, in `TRACKING_TABLES` order.
async fn tables(db: &TestDb) -> Vec<String> {
    let client = db.client().await.unwrap();
    let mut out = Vec::new();
    for table in TRACKING_TABLES {
        let row = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
            .await
            .unwrap();
        if row.get::<_, bool>(0) {
            out.push(table.to_owned());
        }
    }
    out
}

/// The S1 constraints that exist, `table.constraint`.
async fn s1_constraints(db: &TestDb) -> Vec<String> {
    let client = db.client().await.unwrap();
    client
        .query(
            "SELECT c.relname::text || '.' || k.conname::text FROM pg_constraint k \
             JOIN pg_class c ON c.oid = k.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = current_schema() AND k.conname::text = ANY($1) \
             ORDER BY array_position($1, k.conname::text)",
            &[&S1_CONSTRAINTS
                .iter()
                .map(|c| c.split_once('.').unwrap().1)
                .collect::<Vec<_>>()],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

async fn installed_version(db: &TestDb) -> String {
    db.client()
        .await
        .unwrap()
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name = 'stock'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// The module state of `stock`.
async fn module_state(db: &TestDb) -> String {
    db.client()
        .await
        .unwrap()
        .query_one(
            "SELECT state FROM ir_module_module WHERE name = 'stock'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// Upgrade `stock` and print what happened (the S5 record reads this line). A refusal is returned
/// as its kind and message, so a case asserts both.
async fn upgrade(system: &ModuleSystem, case: &str) -> Result<(), (ErrorKind, String)> {
    let outcome = system
        .button_immediate_upgrade(&system.system_env(Context::new()), &stock())
        .await;
    let result = match outcome {
        Ok(outcome) => {
            assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
            Ok(())
        }
        Err(erp_modules::Error::Orm(error)) => Err((error.kind(), error.message().to_owned())),
        // a refused schema step names the module and step around the ORM error
        Err(erp_modules::Error::StepFailed { source, .. }) => match *source {
            erp_modules::Error::Orm(error) => Err((error.kind(), error.message().to_owned())),
            other => panic!("expected an ORM error in the step, got {other:?}"),
        },
        Err(other) => panic!("expected an ORM error, got {other:?}"),
    };
    eprintln!("stock_tracking_upgrade case={case} result={result:?}");
    result
}

fn all_tables() -> Vec<String> {
    TRACKING_TABLES.iter().map(|t| (*t).to_owned()).collect()
}

fn all_s1() -> Vec<String> {
    S1_CONSTRAINTS.iter().map(|c| (*c).to_owned()).collect()
}

/// Case A: the same files and the same manifest version as the installed stock. An explicit
/// upgrade synchronizes the schema whatever the versions say, so the tables and the S1
/// constraints are created.
#[test]
fn upgrade_with_same_version_and_files_creates_the_tracking_tables() {
    run(async {
        let db = TestDb::create().await.unwrap();
        stock_without_tracking_tables(&db, "1.0.0").await;
        let system = builder(&db).build().unwrap();
        assert_eq!(upgrade(&system, "A same version, same files").await, Ok(()));
        assert_eq!(tables(&db).await, all_tables());
        assert_eq!(s1_constraints(&db).await, all_s1());
        assert_eq!(installed_version(&db).await, "1.0.0");
        db.cleanup().await.unwrap();
    });
}

/// Case A2: changed stock files under the same manifest version, upgraded the way a release is
/// (installed receipt of the earlier artifact, `identity_previous`).
#[test]
fn release_upgrade_with_same_version_and_changed_files_creates_the_tracking_tables() {
    run(async {
        let db = TestDb::create().await.unwrap();
        stock_without_tracking_tables(&db, "1.0.0").await;
        let previous = earlier_stock_artifact(&db).await;
        let system = builder(&db).identity_previous(previous).build().unwrap();
        assert_eq!(
            upgrade(&system, "A2 same version, changed files").await,
            Ok(())
        );
        assert_eq!(tables(&db).await, all_tables());
        assert_eq!(s1_constraints(&db).await, all_s1());
        assert_eq!(installed_version(&db).await, "1.0.0");
        db.cleanup().await.unwrap();
    });
}

/// Case B: changed stock files and a manifest version above the installed one (marker rewound to
/// `0.9.0`), upgraded the way a release is.
#[test]
fn release_upgrade_with_higher_version_creates_the_tracking_tables() {
    run(async {
        let db = TestDb::create().await.unwrap();
        stock_without_tracking_tables(&db, "0.9.0").await;
        let previous = earlier_stock_artifact(&db).await;
        let system = builder(&db).identity_previous(previous).build().unwrap();
        assert_eq!(upgrade(&system, "B higher version").await, Ok(()));
        assert_eq!(tables(&db).await, all_tables());
        assert_eq!(s1_constraints(&db).await, all_s1());
        assert_eq!(installed_version(&db).await, "1.0.0");
        db.cleanup().await.unwrap();
    });
}

/// Case C: changed stock files upgraded without `identity_previous` (not the release route), with
/// the same version and with a higher one. The installed receipt is not the current files', so the
/// upgrade is refused whatever the versions, and the tables, version marker and module state stay as
/// they were.
#[test]
fn plain_upgrade_with_changed_files_is_refused_whatever_the_version() {
    run(async {
        for installed in ["1.0.0", "0.9.0"] {
            let db = TestDb::create().await.unwrap();
            stock_without_tracking_tables(&db, installed).await;
            earlier_stock_artifact(&db).await;
            let system = builder(&db).build().unwrap();
            assert_eq!(
                upgrade(&system, &format!("C changed files, installed {installed}")).await,
                Err((
                    ErrorKind::Validation,
                    "Addon 'stock' installed artifact identity does not match available content"
                        .to_owned()
                ))
            );
            assert_eq!(tables(&db).await, Vec::<String>::new());
            assert_eq!(installed_version(&db).await, installed);
            assert_eq!(module_state(&db).await, "installed");
            db.cleanup().await.unwrap();
        }
    });
}

/// Install `stock`, put one product, one lot of it, one quant and one serial position in the
/// tracking tables, then drop the S1 constraints: a database from `main` before they existed, with
/// rows. With `mismatch` the quant and the position name the first product's lot under the second
/// product (`main` had no guard to stop that).
async fn stock_with_rows_and_no_s1_constraints(db: &TestDb, mismatch: bool) {
    let system = builder(db).build().unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    assert!(initialized.post_commit.is_clean());
    let installed = system
        .button_immediate_install(&system.system_env(Context::new()), &stock())
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());
    let orm = installed.orm;
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let mut products = Vec::new();
    for name in ["Row Product A", "Row Product B"] {
        let template = tx
            .env()
            .records("product.template")
            .unwrap()
            .create(
                &mut tx,
                vec![erp_orm::RawVals::new().set_cell("name", erp_fields::Cell::Str(name.into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let row = tx
            .query(
                "SELECT id FROM product_product WHERE product_tmpl_id = $1",
                &[&template],
            )
            .await
            .unwrap();
        products.push(row[0].get::<_, i64>(0));
    }
    let lot = tx
        .env()
        .records("stock.lot")
        .unwrap()
        .create(
            &mut tx,
            vec![
                erp_orm::RawVals::new()
                    .set_cell("name", erp_fields::Cell::Str("LOT-ROWS".into()))
                    .set_cell("product_id", erp_fields::Cell::Id(products[0])),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
    let row_product = if mismatch { products[1] } else { products[0] };
    let client = db.client().await.unwrap();
    for constraint in [
        "ALTER TABLE stock_quant DROP CONSTRAINT stock_quant_lot_product",
        "ALTER TABLE stock_serial_position DROP CONSTRAINT stock_serial_position_lot_product",
        // the move line tables hold keys on it too; the upgrade puts them back
        "ALTER TABLE stock_lot DROP CONSTRAINT stock_lot_id_product CASCADE",
    ] {
        client.batch_execute(constraint).await.unwrap();
    }
    let company: i64 = client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'main_company'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let location: i64 = client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module = 'stock' AND name = 'stock_location_customers'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute(
            "INSERT INTO stock_quant (company_id, product_id, location_id, lot_id, quantity, \
             reserved_quantity, in_date) VALUES ($1, $2, $3, $4, 1, 0, now())",
            &[&company, &row_product, &location, &lot],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO stock_serial_position (company_id, product_id, lot_id, location_id, quantity) \
             VALUES ($1, $2, $3, $4, 1)",
            &[&company, &row_product, &lot, &location],
        )
        .await
        .unwrap();
    assert_eq!(s1_constraints(db).await, Vec::<String>::new());
}

/// every row names its own product's lot; S1)
#[test]
fn upgrade_with_rows_that_match_adds_the_lot_product_keys() {
    run(async {
        let db = TestDb::create().await.unwrap();
        stock_with_rows_and_no_s1_constraints(&db, false).await;
        let system = builder(&db).build().unwrap();
        assert_eq!(upgrade(&system, "R1 matching rows").await, Ok(()));
        assert_eq!(s1_constraints(&db).await, all_s1());
        db.cleanup().await.unwrap();
    });
}

/// naming the key, and adds none; S1)
#[test]
fn upgrade_with_a_row_naming_another_products_lot_is_refused_by_name() {
    run(async {
        let db = TestDb::create().await.unwrap();
        stock_with_rows_and_no_s1_constraints(&db, true).await;
        let system = builder(&db).build().unwrap();
        let (kind, message) = upgrade(&system, "R2 mismatching rows").await.unwrap_err();
        assert_eq!(kind, ErrorKind::Validation, "{message}");
        // the database error mapper words every foreign-key violation as a blocked delete (the same
        // wording `tracking_schema` pins); the constraint it names is what identifies the refusal
        assert!(
            message.contains("'Quants' (stock.quant)")
                && message.contains("'stock_quant_lot_product'"),
            "{message}"
        );
        assert_eq!(s1_constraints(&db).await, Vec::<String>::new());
        assert_eq!(module_state(&db).await, "installed");
        db.cleanup().await.unwrap();
    });
}

/// Install the current `stock` and return its system, for the cases that put an older definition back
/// by SQL and upgrade (S8: the three definition changes of that slice).
async fn installed_stock(db: &TestDb) {
    let system = builder(db).build().unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    assert!(initialized.post_commit.is_clean());
    let installed = system
        .button_immediate_install(&system.system_env(Context::new()), &stock())
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());
}

async fn one_text(db: &TestDb, sql: &str) -> Option<String> {
    db.client()
        .await
        .unwrap()
        .query(sql, &[])
        .await
        .unwrap()
        .first()
        .map(|r| r.get(0))
}

const ORDERPOINT_KEY: &str = "stock_warehouse_orderpoint_product_location_check";

/// UNIQUE constraint of the same name with the declared key form, and adds the four finite checks)
#[test]
fn upgrade_replaces_a_same_named_constraint_with_the_key_form() {
    run(async {
        let db = TestDb::create().await.unwrap();
        installed_stock(&db).await;
        let client = db.client().await.unwrap();
        // the schema before: a raw UNIQUE and no finite checks
        client
            .batch_execute(&format!(
                "ALTER TABLE stock_warehouse_orderpoint DROP CONSTRAINT {ORDERPOINT_KEY}; \
                 ALTER TABLE stock_warehouse_orderpoint ADD CONSTRAINT {ORDERPOINT_KEY} \
                   UNIQUE (product_id, location_id, company_id); \
                 COMMENT ON CONSTRAINT {ORDERPOINT_KEY} ON stock_warehouse_orderpoint \
                   IS 'UNIQUE (product_id, location_id, company_id)'; \
                 ALTER TABLE stock_warehouse_orderpoint \
                   DROP CONSTRAINT stock_warehouse_orderpoint_finite_product_min_qty, \
                   DROP CONSTRAINT stock_warehouse_orderpoint_finite_product_max_qty, \
                   DROP CONSTRAINT stock_warehouse_orderpoint_finite_daily_demand, \
                   DROP CONSTRAINT stock_warehouse_orderpoint_finite_qty_to_order_manual"
            ))
            .await
            .unwrap();
        let def = format!(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{ORDERPOINT_KEY}'"
        );
        assert_eq!(
            one_text(&db, &def).await.as_deref(),
            Some("UNIQUE (product_id, location_id, company_id)")
        );
        let system = builder(&db).build().unwrap();
        assert_eq!(upgrade(&system, "S8 orderpoint key").await, Ok(()));
        assert_eq!(
            one_text(&db, &def).await.as_deref(),
            Some("UNIQUE NULLS NOT DISTINCT (product_id, location_id, company_id)")
        );
        let finite = one_text(
            &db,
            "SELECT count(*)::text FROM pg_constraint WHERE conname LIKE 'stock_warehouse_orderpoint_finite_%'",
        )
        .await;
        assert_eq!(finite.as_deref(), Some("4"));
        db.cleanup().await.unwrap();
    });
}

#[test]
fn upgrade_turns_the_owner_links_into_restrict() {
    run(async {
        let db = TestDb::create().await.unwrap();
        installed_stock(&db).await;
        let client = db.client().await.unwrap();
        let links = [
            ("stock_picking", "owner_id"),
            ("stock_move", "restrict_partner_id"),
        ];
        for (table, column) in links {
            client
                .batch_execute(&format!(
                    "ALTER TABLE {table} DROP CONSTRAINT {table}_{column}_fkey; \
                     ALTER TABLE {table} ADD CONSTRAINT {table}_{column}_fkey \
                       FOREIGN KEY ({column}) REFERENCES res_partner (id) ON DELETE SET NULL"
                ))
                .await
                .unwrap();
        }
        let action = |table: &'static str, column: &'static str| {
            format!(
                "SELECT confdeltype::text FROM pg_constraint WHERE conname = '{table}_{column}_fkey'"
            )
        };
        for (table, column) in links {
            assert_eq!(
                one_text(&db, &action(table, column)).await.as_deref(),
                Some("n")
            );
        }
        let system = builder(&db).build().unwrap();
        assert_eq!(upgrade(&system, "S8 owner links").await, Ok(()));
        for (table, column) in links {
            assert_eq!(
                one_text(&db, &action(table, column)).await.as_deref(),
                Some("r"),
                "{table}.{column}"
            );
        }
        db.cleanup().await.unwrap();
    });
}

/// declares in place)
#[test]
fn upgrade_keeps_the_picking_company_index_the_model_no_longer_declares() {
    run(async {
        let db = TestDb::create().await.unwrap();
        installed_stock(&db).await;
        let client = db.client().await.unwrap();
        client
            .batch_execute(
                "CREATE INDEX stock_picking__company_id_index ON stock_picking (company_id)",
            )
            .await
            .unwrap();
        let system = builder(&db).build().unwrap();
        assert_eq!(upgrade(&system, "S8 company index").await, Ok(()));
        let left = one_text(
            &db,
            "SELECT indexname FROM pg_indexes WHERE schemaname = current_schema() \
             AND indexname = 'stock_picking__company_id_index'",
        )
        .await;
        // schema sync never drops an index the registry no longer declares: an upgraded database keeps
        // the redundant index (harmless, one more write cost); a new database never has it
        assert_eq!(left.as_deref(), Some("stock_picking__company_id_index"));
        db.cleanup().await.unwrap();
    });
}
