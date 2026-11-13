// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema of the stock topology tables: one test per model. Each reads the
//! columns, constraints and indexes the registry created from `pg_catalog`, violates every
//! declared constraint once through the ORM (kind, full message and constraint name), and checks
//! the creation defaults. Default records are checked by `crates/erp-cli/tests/stock_install.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

const PREFIX: &str = "The operation cannot be completed: ";

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
    ])
    .build()
    .await
    .unwrap()
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

/// Column name to "is nullable" of a table.
async fn columns(tx: &mut Transaction, table: &str) -> BTreeMap<String, bool> {
    tx.query(
        "SELECT column_name::text, is_nullable = 'YES' FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1",
        &[&table],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get::<_, String>(0), row.get::<_, bool>(1)))
    .collect()
}

/// Constraint name to definition, for unique and check constraints of a table.
async fn constraints(tx: &mut Transaction, table: &str) -> BTreeMap<String, String> {
    tx.query(
        "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = $1::text::regclass AND contype IN ('u', 'c')",
        &[&table],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
    .collect()
}

/// Index names of a table.
async fn indexes(tx: &mut Transaction, table: &str) -> BTreeSet<String> {
    tx.query(
        "SELECT indexname::text FROM pg_indexes WHERE schemaname = current_schema() AND tablename = $1",
        &[&table],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<_, String>(0))
    .collect()
}

/// Every name of `required` is a column, and the ones in `not_null` refuse NULL.
async fn assert_columns(tx: &mut Transaction, table: &str, required: &[&str], not_null: &[&str]) {
    let found = columns(tx, table).await;
    for name in required {
        assert!(
            found.contains_key(*name),
            "{table}.{name} is missing: {found:?}"
        );
    }
    for name in not_null {
        assert_eq!(
            found.get(*name),
            Some(&false),
            "{table}.{name} must be NOT NULL"
        );
    }
}

/// The constraint exists with this definition.
async fn assert_constraint(tx: &mut Transaction, table: &str, name: &str, definition: &str) {
    let found = constraints(tx, table).await;
    assert_eq!(
        found.get(name).map(String::as_str),
        Some(definition),
        "{table}: {found:?}"
    );
}

async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> Result<Records> {
    let model = model.to_owned();
    tx.savepoint(async move |tx| tx.env().records(&model)?.create(tx, vec![vals]).await)
        .await
}

async fn write(tx: &mut Transaction, record: &Records, vals: RawVals) -> Result<()> {
    let record = record.clone();
    tx.savepoint(async move |tx| record.write(tx, vals).await)
        .await
}

/// A refused write: Validation, the declared message and the constraint's database name.
fn assert_violation(error: &Error, message: &str, constraint: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(error.message(), format!("{PREFIX}{message}"));
    assert_eq!(error.constraint(), Some(constraint));
}

/// A new company row; the harness loads no data, so the first call also inserts the currency
/// fixture.
async fn company(tx: &mut Transaction, name: &str) -> i64 {
    let existing = tx
        .query("SELECT id FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap();
    let currency = match existing.first() {
        Some(row) => row.get(0),
        None => erp_testkit::fixtures::seed_currency_in(tx).await.unwrap(),
    };
    // raw root row with its own parent path: an ORM company create reads external ids, and this
    // harness has no `ir.model.data`
    tx.query(
        "WITH n AS (SELECT nextval(pg_get_serial_sequence('res_company', 'id')) AS id) \
         INSERT INTO res_company (id, name, currency_id, parent_path) \
         SELECT id, $1, $2, id::text || '/' FROM n RETURNING id::int8",
        &[&name, &currency],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn location_in(
    tx: &mut Transaction,
    name: &str,
    usage: &str,
    parent: Option<i64>,
    company: Option<i64>,
) -> Records {
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("usage", text(usage));
    if let Some(parent) = parent {
        vals = vals.set_cell("location_id", Cell::Id(parent));
    }
    if let Some(company) = company {
        vals = vals.set_cell("company_id", Cell::Id(company));
    }
    create(tx, "stock.location", vals).await.unwrap()
}

async fn location(tx: &mut Transaction, name: &str, usage: &str, parent: Option<i64>) -> Records {
    location_in(tx, name, usage, parent, None).await
}

/// A warehouse with its two required locations, in `company`.
async fn warehouse(tx: &mut Transaction, company: i64, name: &str, code: &str) -> Records {
    let view = location_in(tx, code, "view", None, Some(company)).await;
    let stock = location_in(tx, "Stock", "internal", Some(view.ids()[0]), Some(company)).await;
    create(
        tx,
        "stock.warehouse",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("code", text(code))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("view_location_id", Cell::Id(view.ids()[0]))
            .set_cell("lot_stock_id", Cell::Id(stock.ids()[0])),
    )
    .await
    .unwrap()
}

/// once, gets exactly one policy row and loses it with the location; the raw-insert trigger, the
/// verifier and the usage-change refusals come with later slices)
#[tokio::test]
async fn location_table_complete_name_and_policy_row() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_location",
        &[
            "name",
            "complete_name",
            "active",
            "usage",
            "location_id",
            "parent_path",
            "company_id",
            "replenish_location",
            "removal_strategy_id",
            "barcode",
            "cyclic_inventory_frequency",
            "last_inventory_date",
            "storage_category_id",
            "generated_warehouse_id",
            "generated_addon",
            "generated_role",
        ],
        &["name", "usage"],
    )
    .await;
    let found = indexes(&mut tx, "stock_location").await;
    for name in [
        "stock_location_parent_path_id_idx",
        "stock_location__parent_path_c_index",
        "stock_location__usage_index",
        "stock_location__location_id_index",
        "stock_location__company_id_index",
        "stock_location__storage_category_id_index",
    ] {
        assert!(found.contains(name), "index {name} missing: {found:?}");
    }

    let zone = location(&mut tx, "Zone A", "internal", None).await;
    let row = location(&mut tx, "Row 1", "internal", Some(zone.ids()[0])).await;
    let view = location(&mut tx, "WH", "view", Some(zone.ids()[0])).await;
    assert_eq!(
        row.cell(&mut tx, "complete_name").await.unwrap(),
        text("Zone A/Row 1")
    );
    assert_eq!(
        view.cell(&mut tx, "complete_name").await.unwrap(),
        text("WH")
    );
    let shelf = location(&mut tx, "Shelf", "internal", Some(view.ids()[0])).await;
    assert_eq!(
        shelf.cell(&mut tx, "complete_name").await.unwrap(),
        text("WH/Shelf")
    );
    // renaming the parent recomputes the stored child name
    zone.write(&mut tx, RawVals::new().set_cell("name", text("Zone B")))
        .await
        .unwrap();
    assert_eq!(
        row.cell(&mut tx, "complete_name").await.unwrap(),
        text("Zone B/Row 1")
    );
    assert_eq!(
        row.cell(&mut tx, "usage").await.unwrap(),
        text("internal"),
        "usage default"
    );
    assert_eq!(
        zone.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );

    // one policy row per location, also for a batch create
    let batch = tx
        .env()
        .records("stock.location")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new().set_cell("name", text("Bin 1")),
                RawVals::new().set_cell("name", text("Bin 2")),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let rows = tx
        .query(
            "SELECT l.id, count(p.id), bool_or(p.forbid_negative) FROM stock_location l \
             LEFT JOIN stock_location_policy p ON p.location_id = l.id GROUP BY l.id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 6, "six locations created");
    for row in &rows {
        assert_eq!(
            row.get::<_, i64>(1),
            1,
            "location {} policy rows",
            row.get::<_, i64>(0)
        );
        assert!(!row.get::<_, bool>(2), "forbid_negative defaults to false");
    }
    assert_eq!(batch.ids().len(), 2);
    // a second policy row for a location is refused; deleting the location deletes its row
    let error = create(
        &mut tx,
        "stock.location.policy",
        RawVals::new().set_cell("location_id", Cell::Id(shelf.ids()[0])),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "A location has one stock policy.",
        "stock_location_policy_location_uniq",
    );
    shelf.unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();
    let left: i64 = tx
        .query(
            "SELECT count(*) FROM stock_location_policy WHERE location_id = $1",
            &[&shelf.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(left, 0);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// amendment B3: switching the negative-stock guard on is refused, naming the slice
/// that delivers it, on create and on write; nothing is stored.
#[tokio::test]
async fn policy_row_refuses_forbid_negative_until_its_slice() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let loc = location(&mut tx, "Guarded", "internal", None).await;
    let id = loc.ids()[0];
    let policy = tx
        .env()
        .records("stock.location.policy")
        .unwrap()
        .search(
            &mut tx,
            &Domain::condition("location_id", Operator::Eq, id).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(policy.ids().len(), 1);
    let expected = format!(
        "Location {id} cannot forbid negative stock yet: the negative-stock guard is not \
         supported until a later release."
    );
    let error = write(
        &mut tx,
        &policy,
        RawVals::new().set_cell("forbid_negative", Cell::Bool(true)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), expected);
    let source = std::error::Error::source(&error).expect("typed source");
    assert_eq!(
        source.downcast_ref::<erp_addon_stock::StockError>(),
        Some(&erp_addon_stock::StockError::ForbidNegativeUnavailable { location_id: id })
    );
    assert_eq!(
        policy.cell(&mut tx, "forbid_negative").await.unwrap(),
        Cell::Bool(false)
    );
    // a fresh row created with the flag set is refused too
    policy.unlink(&mut tx).await.unwrap();
    let error = create(
        &mut tx,
        "stock.location.policy",
        RawVals::new()
            .set_cell("location_id", Cell::Id(id))
            .set_cell("forbid_negative", Cell::Bool(true)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), expected);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn location_barcode_and_inventory_frequency_constraints() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let main = company(&mut tx, "Main company").await;
    let other = company(&mut tx, "Second company").await;
    assert_constraint(
        &mut tx,
        "stock_location",
        "stock_location_barcode_company_uniq",
        "UNIQUE (barcode, company_id)",
    )
    .await;
    assert_constraint(
        &mut tx,
        "stock_location",
        "stock_location_inventory_freq_nonneg",
        "CHECK ((cyclic_inventory_frequency >= 0))",
    )
    .await;
    let with = |name: &str, barcode: Option<&str>, company: Option<i64>| {
        let mut vals = RawVals::new().set_cell("name", text(name));
        if let Some(barcode) = barcode {
            vals = vals.set_cell("barcode", text(barcode));
        }
        if let Some(company) = company {
            vals = vals.set_cell("company_id", Cell::Id(company));
        }
        vals
    };
    create(
        &mut tx,
        "stock.location",
        with("L1", Some("LOC123"), Some(main)),
    )
    .await
    .unwrap();
    let error = create(
        &mut tx,
        "stock.location",
        with("L2", Some("LOC123"), Some(main)),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "The barcode for a location must be unique per company!",
        "stock_location_barcode_company_uniq",
    );
    // the same barcode in another company, and shared locations without barcode, do not collide
    create(
        &mut tx,
        "stock.location",
        with("L3", Some("LOC123"), Some(other)),
    )
    .await
    .unwrap();
    create(&mut tx, "stock.location", with("S1", None, None))
        .await
        .unwrap();
    create(&mut tx, "stock.location", with("S2", None, None))
        .await
        .unwrap();

    let loc = create(&mut tx, "stock.location", with("Counted", None, Some(main)))
        .await
        .unwrap();
    assert_eq!(
        loc.cell(&mut tx, "cyclic_inventory_frequency")
            .await
            .unwrap(),
        Cell::Int(0)
    );
    let error = write(
        &mut tx,
        &loc,
        RawVals::new().set_cell("cyclic_inventory_frequency", Cell::Int(-1)),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "The inventory frequency (days) for a location must be non-negative",
        "stock_location_inventory_freq_nonneg",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn warehouse_table_and_unique_name_and_code_per_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_warehouse",
        &[
            "name",
            "active",
            "company_id",
            "partner_id",
            "view_location_id",
            "lot_stock_id",
            "code",
            "reception_steps",
            "delivery_steps",
            "wh_input_stock_loc_id",
            "wh_qc_stock_loc_id",
            "wh_output_stock_loc_id",
            "wh_pack_stock_loc_id",
            "mto_pull_id",
            "pick_type_id",
            "pack_type_id",
            "out_type_id",
            "in_type_id",
            "int_type_id",
            "qc_type_id",
            "store_type_id",
            "xdock_type_id",
            "reception_route_id",
            "delivery_route_id",
            "sequence",
        ],
        &[
            "name",
            "company_id",
            "view_location_id",
            "lot_stock_id",
            "code",
            "reception_steps",
            "delivery_steps",
        ],
    )
    .await;
    assert_columns(
        &mut tx,
        "stock_route_warehouse",
        &["route_id", "warehouse_id"],
        &[],
    )
    .await;
    assert_columns(
        &mut tx,
        "stock_wh_resupply_table",
        &["supplied_wh_id", "supplier_wh_id"],
        &[],
    )
    .await;
    let main = company(&mut tx, "Main company").await;
    let other = company(&mut tx, "Second company").await;
    let wh = warehouse(&mut tx, main, "Main WH", "WH").await;
    assert_eq!(
        wh.cell(&mut tx, "reception_steps").await.unwrap(),
        text("one_step")
    );
    assert_eq!(
        wh.cell(&mut tx, "delivery_steps").await.unwrap(),
        text("ship_only")
    );
    assert_eq!(wh.cell(&mut tx, "sequence").await.unwrap(), Cell::Int(10));
    assert_eq!(wh.cell(&mut tx, "active").await.unwrap(), Cell::Bool(true));

    let view = location_in(&mut tx, "Second", "view", None, Some(main)).await;
    let stock = location_in(
        &mut tx,
        "Stock 2",
        "internal",
        Some(view.ids()[0]),
        Some(main),
    )
    .await;
    let vals = |name: &str, code: &str, company: i64| {
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("code", text(code))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("view_location_id", Cell::Id(view.ids()[0]))
            .set_cell("lot_stock_id", Cell::Id(stock.ids()[0]))
    };
    let error = create(&mut tx, "stock.warehouse", vals("Main WH", "WH2", main))
        .await
        .unwrap_err();
    assert_violation(
        &error,
        "The name of the warehouse must be unique per company!",
        "stock_warehouse_warehouse_name_uniq",
    );
    let error = create(&mut tx, "stock.warehouse", vals("Other WH", "WH", main))
        .await
        .unwrap_err();
    assert_violation(
        &error,
        "The short name of the warehouse must be unique per company!",
        "stock_warehouse_warehouse_code_uniq",
    );
    // renaming into the existing name is refused as well
    let second = create(&mut tx, "stock.warehouse", vals("Other WH", "WH2", main))
        .await
        .unwrap();
    let error = write(
        &mut tx,
        &second,
        RawVals::new().set_cell("name", text("Main WH")),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "The name of the warehouse must be unique per company!",
        "stock_warehouse_warehouse_name_uniq",
    );
    // the same name and code in another company are allowed
    let elsewhere = warehouse(&mut tx, other, "Main WH", "WH").await;
    assert_eq!(elsewhere.ids().len(), 1);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The operation type table, its creation defaults and the generated-role identity of topology
/// ADR D9 (all three parts or none; one row per warehouse, addon and role).
#[tokio::test]
async fn picking_type_table_defaults_and_generated_role_identity() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_picking_type",
        &[
            "name",
            "color",
            "sequence",
            "sequence_id",
            "default_location_src_id",
            "default_location_dest_id",
            "allocated_location_id",
            "code",
            "return_picking_type_id",
            "warehouse_id",
            "active",
            "use_create_lots",
            "use_existing_lots",
            "print_label",
            "reservation_method",
            "reservation_days_before",
            "reservation_days_before_priority",
            "barcode",
            "company_id",
            "create_backorder",
            "move_type",
            "generated_warehouse_id",
            "generated_addon",
            "generated_role",
        ],
        &[
            "name",
            "default_location_src_id",
            "default_location_dest_id",
            "code",
            "company_id",
            "reservation_method",
            "create_backorder",
            "move_type",
        ],
    )
    .await;
    assert_columns(
        &mut tx,
        "picking_type_favorite_user_rel",
        &["picking_type_id", "user_id"],
        &[],
    )
    .await;
    let found = indexes(&mut tx, "stock_picking_type").await;
    for name in [
        "stock_picking_type__company_id_index",
        "stock_picking_type__warehouse_id_index",
        "stock_picking_type__return_picking_type_id_index",
    ] {
        assert!(found.contains(name), "index {name} missing: {found:?}");
    }
    let main = company(&mut tx, "Main company").await;
    let wh = warehouse(&mut tx, main, "Main WH", "WH").await;
    let stock: i64 = match wh.cell(&mut tx, "lot_stock_id").await.unwrap() {
        Cell::Id(id) => id,
        other => panic!("lot_stock_id {other:?}"),
    };
    let base = || {
        RawVals::new()
            .set_cell("name", text("Receipts"))
            .set_cell("company_id", Cell::Id(main))
            .set_cell("default_location_src_id", Cell::Id(stock))
            .set_cell("default_location_dest_id", Cell::Id(stock))
    };
    let receipts = create(&mut tx, "stock.picking.type", base()).await.unwrap();
    for (field, value) in [
        ("code", text("incoming")),
        ("reservation_method", text("at_confirm")),
        ("create_backorder", text("ask")),
        ("move_type", text("direct")),
        ("use_create_lots", Cell::Bool(true)),
        ("use_existing_lots", Cell::Bool(true)),
        ("active", Cell::Bool(true)),
    ] {
        assert_eq!(
            receipts.cell(&mut tx, field).await.unwrap(),
            value,
            "{field}"
        );
    }
    let role = |addon: Option<&str>, role: Option<&str>| {
        let mut vals = base().set_cell("generated_warehouse_id", Cell::Id(wh.ids()[0]));
        if let Some(addon) = addon {
            vals = vals.set_cell("generated_addon", text(addon));
        }
        if let Some(role) = role {
            vals = vals.set_cell("generated_role", text(role));
        }
        vals
    };
    let error = create(&mut tx, "stock.picking.type", role(Some("stock"), None))
        .await
        .unwrap_err();
    assert_violation(
        &error,
        "A generated operation type names its warehouse, addon and role together.",
        "stock_picking_type_generated_role_complete",
    );
    create(
        &mut tx,
        "stock.picking.type",
        role(Some("stock"), Some("in_type")),
    )
    .await
    .unwrap();
    let error = create(
        &mut tx,
        "stock.picking.type",
        role(Some("stock"), Some("in_type")),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "A warehouse has one operation type per addon role.",
        "stock_picking_type_generated_role_uniq",
    );
    // another addon may own a role of the same name
    create(
        &mut tx,
        "stock.picking.type",
        role(Some("stock_barcode"), Some("in_type")),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn route_table_selectability_and_creation_defaults() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_route",
        &[
            "name",
            "active",
            "sequence",
            "product_selectable",
            "product_categ_selectable",
            "warehouse_selectable",
            "package_type_selectable",
            "supplied_wh_id",
            "supplier_wh_id",
            "company_id",
            "generated_warehouse_id",
            "generated_addon",
            "generated_role",
        ],
        &["name"],
    )
    .await;
    for (table, cols) in [
        ("stock_route_product", ["route_id", "product_id"]),
        ("stock_route_categ", ["route_id", "categ_id"]),
        (
            "stock_package_type_stock_route_rel",
            ["stock_route_id", "stock_package_type_id"],
        ),
    ] {
        assert_columns(&mut tx, table, &cols, &[]).await;
    }
    let plain = create(
        &mut tx,
        "stock.route",
        RawVals::new().set_cell("name", text("Plain")),
    )
    .await
    .unwrap();
    assert_eq!(
        plain.cell(&mut tx, "product_selectable").await.unwrap(),
        Cell::Bool(true)
    );
    for flag in [
        "product_categ_selectable",
        "warehouse_selectable",
        "package_type_selectable",
    ] {
        assert_eq!(
            plain.cell(&mut tx, flag).await.unwrap(),
            Cell::Bool(false),
            "{flag}"
        );
    }
    let all = create(
        &mut tx,
        "stock.route",
        RawVals::new()
            .set_cell("name", text("Everywhere"))
            .set_cell("product_selectable", Cell::Bool(true))
            .set_cell("product_categ_selectable", Cell::Bool(true))
            .set_cell("warehouse_selectable", Cell::Bool(true))
            .set_cell("package_type_selectable", Cell::Bool(true)),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    for flag in [
        "product_selectable",
        "product_categ_selectable",
        "warehouse_selectable",
        "package_type_selectable",
        "active",
    ] {
        assert_eq!(
            all.cell(&mut tx, flag).await.unwrap(),
            Cell::Bool(true),
            "{flag}"
        );
    }
    assert_eq!(all.cell(&mut tx, "sequence").await.unwrap(), Cell::Int(0));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The rule table: defaults, the stored route sequence and the cascade from its route.
#[tokio::test]
async fn rule_table_defaults_route_sequence_and_cascade() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_rule",
        &[
            "name",
            "active",
            "action",
            "sequence",
            "company_id",
            "location_dest_id",
            "location_src_id",
            "location_dest_from_rule",
            "route_id",
            "route_sequence",
            "procure_method",
            "picking_type_id",
            "delay",
            "partner_address_id",
            "propagate_cancel",
            "propagate_carrier",
            "warehouse_id",
            "auto",
            "push_domain",
            "generated_warehouse_id",
            "generated_addon",
            "generated_role",
        ],
        &[
            "name",
            "action",
            "location_dest_id",
            "route_id",
            "procure_method",
            "picking_type_id",
            "auto",
        ],
    )
    .await;
    let main = company(&mut tx, "Main company").await;
    let wh = warehouse(&mut tx, main, "Main WH", "WH").await;
    let Cell::Id(stock) = wh.cell(&mut tx, "lot_stock_id").await.unwrap() else {
        panic!("lot_stock_id")
    };
    let route = create(
        &mut tx,
        "stock.route",
        RawVals::new()
            .set_cell("name", text("Route"))
            .set_cell("sequence", Cell::Int(7)),
    )
    .await
    .unwrap();
    let ptype = create(
        &mut tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text("Internal"))
            .set_cell("code", text("internal"))
            .set_cell("company_id", Cell::Id(main))
            .set_cell("default_location_src_id", Cell::Id(stock))
            .set_cell("default_location_dest_id", Cell::Id(stock)),
    )
    .await
    .unwrap();
    let rule = create(
        &mut tx,
        "stock.rule",
        RawVals::new()
            .set_cell("name", text("Pull"))
            // a shared rule may not point at company locations and types (company check)
            .set_cell("company_id", Cell::Id(main))
            .set_cell("route_id", Cell::Id(route.ids()[0]))
            .set_cell("location_dest_id", Cell::Id(stock))
            .set_cell("picking_type_id", Cell::Id(ptype.ids()[0])),
    )
    .await
    .unwrap();
    for (field, value) in [
        ("action", text("pull")),
        ("procure_method", text("make_to_stock")),
        ("auto", text("manual")),
        ("sequence", Cell::Int(20)),
        ("delay", Cell::Int(0)),
        ("route_sequence", Cell::Int(7)),
        ("active", Cell::Bool(true)),
        ("propagate_cancel", Cell::Bool(false)),
    ] {
        assert_eq!(rule.cell(&mut tx, field).await.unwrap(), value, "{field}");
    }
    route
        .write(&mut tx, RawVals::new().set_cell("sequence", Cell::Int(3)))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let stored: i32 = tx
        .query(
            "SELECT route_sequence FROM stock_rule WHERE id = $1",
            &[&rule.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, 3, "route_sequence is stored and follows the route");
    route.unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();
    let left: i64 = tx
        .query(
            "SELECT count(*) FROM stock_rule WHERE id = $1",
            &[&rule.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(left, 0, "deleting the route deletes its rules");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn storage_category_table_weight_check_and_policies() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_storage_category",
        &["name", "max_weight", "allow_new_product", "company_id"],
        &["name", "allow_new_product"],
    )
    .await;
    let category = create(
        &mut tx,
        "stock.storage.category",
        RawVals::new().set_cell("name", text("Pallets")),
    )
    .await
    .unwrap();
    assert_eq!(
        category.cell(&mut tx, "allow_new_product").await.unwrap(),
        text("mixed")
    );
    for policy in ["empty", "same", "mixed"] {
        category
            .write(
                &mut tx,
                RawVals::new().set_cell("allow_new_product", text(policy)),
            )
            .await
            .unwrap();
        assert_eq!(
            category.cell(&mut tx, "allow_new_product").await.unwrap(),
            text(policy)
        );
    }
    category
        .write(
            &mut tx,
            RawVals::new().set_cell("max_weight", Cell::Decimal(Decimal::ZERO)),
        )
        .await
        .unwrap();
    let error = write(
        &mut tx,
        &category,
        RawVals::new().set_cell("max_weight", Cell::Decimal(Decimal::from(-5))),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "Max weight should be a positive number.",
        "stock_storage_category_positive_max_weight",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// product row is asserted in `crates/erp-cli/tests/stock_install.rs`, where products have units)
#[tokio::test]
async fn storage_capacity_table_quantity_and_uniqueness() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_storage_category_capacity",
        &[
            "storage_category_id",
            "product_id",
            "package_type_id",
            "quantity",
        ],
        &["storage_category_id", "quantity"],
    )
    .await;
    assert_constraint(
        &mut tx,
        "stock_storage_category_capacity",
        "stock_storage_category_capacity_unique_product",
        "UNIQUE (product_id, storage_category_id)",
    )
    .await;
    let category = create(
        &mut tx,
        "stock.storage.category",
        RawVals::new().set_cell("name", text("Shelves")),
    )
    .await
    .unwrap()
    .ids()[0];
    let package = create(
        &mut tx,
        "stock.package.type",
        RawVals::new().set_cell("name", text("Box")),
    )
    .await
    .unwrap()
    .ids()[0];
    let capacity = |quantity: i32| {
        RawVals::new()
            .set_cell("storage_category_id", Cell::Id(category))
            .set_cell("package_type_id", Cell::Id(package))
            .set_cell("quantity", Cell::Decimal(Decimal::from(quantity)))
    };
    for quantity in [0, -1] {
        let error = create(
            &mut tx,
            "stock.storage.category.capacity",
            capacity(quantity),
        )
        .await
        .unwrap_err();
        assert_violation(
            &error,
            "Quantity should be a positive number.",
            "stock_storage_category_capacity_positive_quantity",
        );
    }
    create(&mut tx, "stock.storage.category.capacity", capacity(4))
        .await
        .unwrap();
    // the refusal names the package-type constraint: rows without product never collide on the
    // product one (NULLs are distinct)
    let error = create(&mut tx, "stock.storage.category.capacity", capacity(2))
        .await
        .unwrap_err();
    assert_violation(
        &error,
        "Multiple capacity rules for one package type.",
        "stock_storage_category_capacity_unique_package_type",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn package_type_table_dimension_checks_and_unique_barcode() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_package_type",
        &[
            "name",
            "sequence",
            "sequence_id",
            "height",
            "width",
            "packaging_length",
            "base_weight",
            "max_weight",
            "barcode",
            "company_id",
            "package_use",
        ],
        &["name", "package_use"],
    )
    .await;
    let boxed = create(
        &mut tx,
        "stock.package.type",
        RawVals::new()
            .set_cell("name", text("Box"))
            .set_cell("barcode", text("PACK-BOX-10")),
    )
    .await
    .unwrap();
    assert_eq!(
        boxed.cell(&mut tx, "package_use").await.unwrap(),
        text("disposable")
    );
    assert_eq!(boxed.cell(&mut tx, "sequence").await.unwrap(), Cell::Int(1));
    for (field, message, constraint) in [
        (
            "height",
            "Height must be positive",
            "stock_package_type_positive_height",
        ),
        (
            "width",
            "Width must be positive",
            "stock_package_type_positive_width",
        ),
        (
            "packaging_length",
            "Length must be positive",
            "stock_package_type_positive_length",
        ),
        (
            "max_weight",
            "Max Weight must be positive",
            "stock_package_type_positive_max_weight",
        ),
    ] {
        boxed
            .write(
                &mut tx,
                RawVals::new().set_cell(field, Cell::Decimal(Decimal::ZERO)),
            )
            .await
            .unwrap();
        let error = write(
            &mut tx,
            &boxed,
            RawVals::new().set_cell(field, Cell::Decimal(Decimal::from(-1))),
        )
        .await
        .unwrap_err();
        assert_violation(&error, message, constraint);
    }
    let error = create(
        &mut tx,
        "stock.package.type",
        RawVals::new()
            .set_cell("name", text("Other box"))
            .set_cell("barcode", text("PACK-BOX-10")),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "A barcode can only be assigned to one package type!",
        "stock_package_type_barcode_uniq",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The removal strategy table, and the topology version table: one row, nonnegative generation.
#[tokio::test]
async fn removal_and_topology_version_tables() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "product_removal",
        &["name", "method"],
        &["name", "method"],
    )
    .await;

    assert_columns(
        &mut tx,
        "stock_topology_version",
        &["singleton", "generation"],
        &["singleton", "generation"],
    )
    .await;
    let version = create(&mut tx, "stock.topology.version", RawVals::new())
        .await
        .unwrap();
    assert_eq!(
        version.cell(&mut tx, "generation").await.unwrap(),
        Cell::Int(0)
    );
    let error = create(&mut tx, "stock.topology.version", RawVals::new())
        .await
        .unwrap_err();
    assert_violation(
        &error,
        "The stock topology version is a single row.",
        "stock_topology_version_singleton_uniq",
    );
    let error = write(
        &mut tx,
        &version,
        RawVals::new().set_cell("singleton", Cell::Bool(false)),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "The stock topology version is a single row.",
        "stock_topology_version_singleton_true",
    );
    let error = write(
        &mut tx,
        &version,
        RawVals::new().set_cell("generation", Cell::Int(-1)),
    )
    .await
    .unwrap_err();
    assert_violation(
        &error,
        "The stock topology generation cannot be negative.",
        "stock_topology_version_generation_nonneg",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A create refused by the company check: a user error that names the company crossover.
async fn assert_crossover(tx: &mut Transaction, model: &str, vals: RawVals, what: &str) {
    let error = create(tx, model, vals).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError, "{what}: {error:?}");
    assert!(
        error.message().contains("company"),
        "{what}: {}",
        error.message()
    );
}

/// Cross-company links on the topology models are refused: a warehouse naming another company's
/// location, an operation type naming another company's location, a child location under another
/// company's parent, and a capacity row naming a package type of another company than its category. A shared (company-less) target is accepted.
/// (S2, S5)
#[tokio::test]
async fn cross_company_links_are_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "Company A").await;
    let b = company(&mut tx, "Company B").await;
    warehouse(&mut tx, a, "WH A", "WA").await;
    let b_view = location_in(&mut tx, "BV", "view", None, Some(b))
        .await
        .ids()[0];
    let b_stock = location_in(&mut tx, "BS", "internal", Some(b_view), Some(b))
        .await
        .ids()[0];
    // warehouse of A with a location of B
    assert_crossover(
        &mut tx,
        "stock.warehouse",
        RawVals::new()
            .set_cell("name", text("WH X"))
            .set_cell("code", text("WX"))
            .set_cell("company_id", Cell::Id(a))
            .set_cell("view_location_id", Cell::Id(b_view))
            .set_cell("lot_stock_id", Cell::Id(b_stock)),
        "warehouse locations",
    )
    .await;
    // operation type of A with a location of B
    assert_crossover(
        &mut tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text("Receipts"))
            .set_cell("company_id", Cell::Id(a))
            .set_cell("default_location_src_id", Cell::Id(b_stock))
            .set_cell("default_location_dest_id", Cell::Id(b_stock)),
        "operation type location",
    )
    .await;
    // child location of A under a parent of B (the parent_of rule accepts only the company's own
    // or a parent company's location), while a shared parent is accepted
    assert_crossover(
        &mut tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Child"))
            .set_cell("usage", text("internal"))
            .set_cell("location_id", Cell::Id(b_view))
            .set_cell("company_id", Cell::Id(a)),
        "child location",
    )
    .await;
    let shared = location(&mut tx, "Shared", "view", None).await.ids()[0];
    create(
        &mut tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Under shared"))
            .set_cell("usage", text("internal"))
            .set_cell("location_id", Cell::Id(shared))
            .set_cell("company_id", Cell::Id(a)),
    )
    .await
    .unwrap();
    // capacity rows: the category's company decides
    let category = create(
        &mut tx,
        "stock.storage.category",
        RawVals::new()
            .set_cell("name", text("Shelves A"))
            .set_cell("company_id", Cell::Id(a)),
    )
    .await
    .unwrap()
    .ids()[0];
    let package_b = create(
        &mut tx,
        "stock.package.type",
        RawVals::new()
            .set_cell("name", text("Box B"))
            .set_cell("company_id", Cell::Id(b)),
    )
    .await
    .unwrap()
    .ids()[0];
    let capacity = |field: &'static str, target: i64| {
        RawVals::new()
            .set_cell("storage_category_id", Cell::Id(category))
            .set_cell(field, Cell::Id(target))
            .set_cell("quantity", Cell::Decimal(Decimal::from(3)))
    };
    assert_crossover(
        &mut tx,
        "stock.storage.category.capacity",
        capacity("package_type_id", package_b),
        "capacity package type",
    )
    .await;
    // the same category's own company is accepted
    let package_a = create(
        &mut tx,
        "stock.package.type",
        RawVals::new()
            .set_cell("name", text("Box A"))
            .set_cell("company_id", Cell::Id(a)),
    )
    .await
    .unwrap()
    .ids()[0];
    create(
        &mut tx,
        "stock.storage.category.capacity",
        capacity("package_type_id", package_a),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
