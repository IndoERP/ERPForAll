// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fixture builders shared by route lifecycle and draft-onchange tests.
#![allow(dead_code, reason = "shared test fixture subsets")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::prelude::*;
use erp_testkit::Harness;

pub async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
    ])
    .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .build()
    .await
    .unwrap()
}

pub fn text(s: &str) -> Cell {
    Cell::Str(s.into())
}

pub async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> Result<Records> {
    let model = model.to_owned();
    tx.savepoint(async move |tx| tx.env().records(&model)?.create(tx, vec![vals]).await)
        .await
}

pub async fn write(tx: &mut Transaction, record: &Records, vals: RawVals) -> Result<()> {
    let record = record.clone();
    tx.savepoint(async move |tx| record.write(tx, vals).await)
        .await
}

pub async fn company(tx: &mut Transaction, name: &str) -> i64 {
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

pub async fn location_in(
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

pub async fn location(
    tx: &mut Transaction,
    name: &str,
    usage: &str,
    parent: Option<i64>,
) -> Records {
    location_in(tx, name, usage, parent, None).await
}

/// A warehouse with its two required locations, in `company`.
pub async fn warehouse(tx: &mut Transaction, company: i64, name: &str, code: &str) -> Records {
    let view = location_in(tx, code, "view", None, None).await;
    let stock = location_in(tx, "Stock", "internal", Some(view.ids()[0]), None).await;
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

pub async fn picking_type(tx: &mut Transaction, company: i64, name: &str) -> Records {
    let shared = location(tx, "Shared", "internal", None).await.ids()[0];
    create(
        tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("sequence", Cell::Int(10))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("default_location_src_id", Cell::Id(shared))
            .set_cell("default_location_dest_id", Cell::Id(shared)),
    )
    .await
    .unwrap()
}
