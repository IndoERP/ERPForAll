// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company topology links, derived defaults and invalidation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
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

fn text(s: &str) -> Cell {
    Cell::Str(s.into())
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

/// A warehouse with its two required locations, in `company`.
async fn warehouse(tx: &mut Transaction, company: i64, name: &str, code: &str) -> Records {
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

#[tokio::test]
async fn company_lookups_follow_topology_and_rollback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "Links A").await;
    let b = company(&mut tx, "Links B").await;
    let companies = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([a, b])
        .unwrap();
    assert_eq!(
        companies.cells(&mut tx, "scrap_location_id").await.unwrap(),
        vec![Cell::Null, Cell::Null]
    );
    let shared = location_in(&mut tx, "Shared inventory", "inventory", None, None).await;
    let first = location_in(&mut tx, "First inventory", "inventory", None, Some(a)).await;
    let second = location_in(&mut tx, "Second inventory", "inventory", None, Some(a)).await;
    let foreign = location_in(&mut tx, "Other inventory", "inventory", None, Some(b)).await;
    assert_eq!(
        companies.cells(&mut tx, "scrap_location_id").await.unwrap(),
        vec![Cell::Id(first.ids()[0]), Cell::Id(foreign.ids()[0])]
    );
    write(
        &mut tx,
        &first,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    assert_eq!(
        companies
            .clone()
            .browse([a])
            .unwrap()
            .cell(&mut tx, "scrap_location_id")
            .await
            .unwrap(),
        Cell::Id(second.ids()[0])
    );
    assert_eq!(
        shared.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        companies
            .clone()
            .browse([a])
            .unwrap()
            .cell(&mut tx, "default_stock_location_id")
            .await
            .unwrap(),
        Cell::Null
    );
    let w1 = warehouse(&mut tx, a, "First warehouse", "W1").await;
    let w2 = warehouse(&mut tx, a, "Second warehouse", "W2").await;
    write(
        &mut tx,
        &w1,
        RawVals::new().set_cell("sequence", Cell::Int(20)),
    )
    .await
    .unwrap();
    write(
        &mut tx,
        &w2,
        RawVals::new().set_cell("sequence", Cell::Int(10)),
    )
    .await
    .unwrap();
    let stock2 = w2.cell(&mut tx, "lot_stock_id").await.unwrap();
    let only_a = companies.clone().browse([a]).unwrap();
    assert_eq!(
        only_a
            .cell(&mut tx, "default_stock_location_id")
            .await
            .unwrap(),
        stock2
    );
    let w = w2.clone();
    let c = only_a.clone();
    let result: Result<()> = tx
        .savepoint(async move |tx| {
            w.write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                .await?;
            assert_eq!(
                c.cell(tx, "default_stock_location_id").await?,
                w1.cell(tx, "lot_stock_id").await?
            );
            Err(Error::user("rollback topology test"))
        })
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), "rollback topology test");
    assert_eq!(
        only_a
            .cell(&mut tx, "default_stock_location_id")
            .await
            .unwrap(),
        stock2
    );
    second.unlink(&mut tx).await.unwrap();
    assert_eq!(
        only_a.cell(&mut tx, "scrap_location_id").await.unwrap(),
        Cell::Null
    );
    write(
        &mut tx,
        &first,
        RawVals::new().set_cell("active", Cell::Bool(true)),
    )
    .await
    .unwrap();
    assert_eq!(
        only_a.cell(&mut tx, "scrap_location_id").await.unwrap(),
        Cell::Id(first.ids()[0])
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn derived_company_links_refuse_assignments() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "Derived company").await;
    let c = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([a])
        .unwrap();
    for field in ["scrap_location_id", "default_stock_location_id"] {
        let error = write(
            &mut tx,
            &c,
            RawVals::new()
                .set_cell(field, Cell::Null)
                .set_cell("name", text("must not persist")),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "Company scrap and default stock locations are derived and cannot be assigned."
        );
        assert_eq!(
            c.cell(&mut tx, "name").await.unwrap(),
            text("Derived company")
        );
        let error = create(
            &mut tx,
            "res.company",
            RawVals::new().set_cell(field, Cell::Null),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "Company scrap and default stock locations are derived and cannot be assigned."
        );
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn company_transit_rejects_unrelated_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "Transit A").await;
    let b = company(&mut tx, "Transit B").await;
    let la = location_in(&mut tx, "A transit", "transit", None, Some(a)).await;
    let lb = location_in(&mut tx, "B transit", "transit", None, Some(b)).await;
    let c = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([a])
        .unwrap();
    write(
        &mut tx,
        &c,
        RawVals::new().set_cell("internal_transit_location_id", Cell::Id(la.ids()[0])),
    )
    .await
    .unwrap();
    let error = write(
        &mut tx,
        &c,
        RawVals::new().set_cell("internal_transit_location_id", Cell::Id(lb.ids()[0])),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "These values link records of different companies:\n- The record is company “Transit A”, and “Internal transit location” (internal_transit_location_id: 'B transit') points to a record of another company.\nChoose records of the same company, or records shared by all companies."
    );
    assert_eq!(
        c.cell(&mut tx, "internal_transit_location_id")
            .await
            .unwrap(),
        Cell::Id(la.ids()[0])
    );
    let referenced = la.clone();
    let error = tx
        .savepoint(async move |tx| referenced.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The database refused the change: Other records still point to the record you want to delete.\n\nReferring model: 'Company' (res.company)\nConstraint: 'Internal transit location' (internal_transit_location_id)\nArchive the record instead of deleting it."
    );
    assert_eq!(
        c.cell(&mut tx, "internal_transit_location_id")
            .await
            .unwrap(),
        Cell::Id(la.ids()[0])
    );
    tx.rollback().await.unwrap();
}
