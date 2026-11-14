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
async fn partner_properties_are_company_local() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "Property A").await;
    let b = company(&mut tx, "Property B").await;
    let la = location_in(&mut tx, "A customer", "customer", None, Some(a)).await;
    let lb = location_in(&mut tx, "B customer", "customer", None, Some(b)).await;
    let p: i64 = tx
        .query(
            "INSERT INTO res_partner (name) VALUES ('Topology contact') RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let pa = tx
        .env()
        .with_company(a)
        .unwrap()
        .records("res.partner")
        .unwrap()
        .browse([p])
        .unwrap();
    let pb = tx
        .env()
        .with_company(b)
        .unwrap()
        .records("res.partner")
        .unwrap()
        .browse([p])
        .unwrap();
    write(
        &mut tx,
        &pa,
        RawVals::new().set_cell("property_stock_customer", Cell::Id(la.ids()[0])),
    )
    .await
    .unwrap();
    write(
        &mut tx,
        &pb,
        RawVals::new().set_cell("property_stock_customer", Cell::Id(lb.ids()[0])),
    )
    .await
    .unwrap();
    assert_eq!(
        pa.cell(&mut tx, "property_stock_customer").await.unwrap(),
        Cell::Id(la.ids()[0])
    );
    assert_eq!(
        pb.cell(&mut tx, "property_stock_customer").await.unwrap(),
        Cell::Id(lb.ids()[0])
    );
    write(
        &mut tx,
        &pa,
        RawVals::new().set_cell("property_stock_customer", Cell::Null),
    )
    .await
    .unwrap();
    assert_eq!(
        pa.cell(&mut tx, "property_stock_customer").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        pb.cell(&mut tx, "property_stock_customer").await.unwrap(),
        Cell::Id(lb.ids()[0])
    );
    let p = pb.clone();
    let result: Result<()> = tx
        .savepoint(async move |tx| {
            p.write(
                tx,
                RawVals::new().set_cell("property_stock_customer", Cell::Null),
            )
            .await?;
            assert_eq!(p.cell(tx, "property_stock_customer").await?, Cell::Null);
            Err(Error::user("rollback property test"))
        })
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), "rollback property test");
    assert_eq!(
        pb.cell(&mut tx, "property_stock_customer").await.unwrap(),
        Cell::Id(lb.ids()[0])
    );
    lb.unlink(&mut tx).await.unwrap();
    assert_eq!(
        pb.cell(&mut tx, "property_stock_customer").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        pa.cell(&mut tx, "property_stock_customer").await.unwrap(),
        Cell::Null
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn warehouse_preference_keeps_separate_company_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "Preference A").await;
    let b = company(&mut tx, "Preference B").await;
    let wa = warehouse(&mut tx, a, "A warehouse", "PA").await;
    let wb = warehouse(&mut tx, b, "B warehouse", "PB").await;
    let u: i64 = tx.query("INSERT INTO res_users (name, login, active) VALUES ('Preference user', 'topology-preference', true) RETURNING id::int8", &[]).await.unwrap()[0].get(0);
    let ua = tx
        .env()
        .with_company(a)
        .unwrap()
        .records("res.users")
        .unwrap()
        .browse([u])
        .unwrap();
    let ub = tx
        .env()
        .with_company(b)
        .unwrap()
        .records("res.users")
        .unwrap()
        .browse([u])
        .unwrap();
    write(
        &mut tx,
        &ua,
        RawVals::new().set_cell("property_warehouse_id", Cell::Id(wa.ids()[0])),
    )
    .await
    .unwrap();
    write(
        &mut tx,
        &ub,
        RawVals::new().set_cell("property_warehouse_id", Cell::Id(wb.ids()[0])),
    )
    .await
    .unwrap();
    assert_eq!(
        ua.cell(&mut tx, "property_warehouse_id").await.unwrap(),
        Cell::Id(wa.ids()[0])
    );
    assert_eq!(
        ub.cell(&mut tx, "property_warehouse_id").await.unwrap(),
        Cell::Id(wb.ids()[0])
    );
    write(
        &mut tx,
        &ua,
        RawVals::new().set_cell("property_warehouse_id", Cell::Null),
    )
    .await
    .unwrap();
    assert_eq!(
        ua.cell(&mut tx, "property_warehouse_id").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        ub.cell(&mut tx, "property_warehouse_id").await.unwrap(),
        Cell::Id(wb.ids()[0])
    );
    wb.unlink(&mut tx).await.unwrap();
    assert_eq!(
        ub.cell(&mut tx, "property_warehouse_id").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        ua.cell(&mut tx, "property_warehouse_id").await.unwrap(),
        Cell::Null
    );
    tx.rollback().await.unwrap();
}
