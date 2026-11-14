// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company ownership guards and relational write regressions for stock configuration.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

const IMMUTABLE: &str = "The company of this record cannot be changed once it exists. Archive it and create a new record for the other company.";

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

async fn location(tx: &mut Transaction, name: &str, usage: &str, parent: Option<i64>) -> Records {
    location_in(tx, name, usage, parent, None).await
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

async fn picking_type(tx: &mut Transaction, company: i64, name: &str) -> Records {
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

async fn ownership(record: &Records, tx: &mut Transaction, company: i64, sequence: i32) {
    assert_eq!(
        record.cell(tx, "company_id").await.unwrap(),
        Cell::Id(company)
    );
    assert_eq!(
        record.cell(tx, "sequence").await.unwrap(),
        Cell::Int(sequence)
    );
    tx.flush().await.unwrap();
    let table = if record.spec().name == "stock.warehouse" {
        "stock_warehouse"
    } else {
        "stock_picking_type"
    };
    let rows = tx
        .query(
            &format!("SELECT company_id::int8, sequence::int8 FROM {table} WHERE id = $1"),
            &[&record.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), company);
    assert_eq!(rows[0].get::<_, i64>(1), i64::from(sequence));
}

async fn immutable_case(model: &str) {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let b = company(&mut tx, "B").await;
    let row = if model == "stock.warehouse" {
        warehouse(&mut tx, a, "First", "FIRST").await
    } else {
        picking_type(&mut tx, a, "First").await
    };
    let initial = match row.cell(&mut tx, "sequence").await.unwrap() {
        Cell::Int(n) => n,
        other => panic!("{other:?}"),
    };
    for active in [true, false] {
        write(
            &mut tx,
            &row,
            RawVals::new().set_cell("active", Cell::Bool(active)),
        )
        .await
        .unwrap();
        for value in [Cell::Id(b), Cell::Null] {
            let error = write(
                &mut tx,
                &row,
                RawVals::new()
                    .set_cell("company_id", value)
                    .set_cell("sequence", Cell::Int(91)),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
            assert_eq!(error.message(), IMMUTABLE);
            ownership(&row, &mut tx, a, initial).await;
        }
    }
    // Both unchanged ownership and writes omitting it retain ordinary base behavior.
    write(
        &mut tx,
        &row,
        RawVals::new()
            .set_cell("company_id", Cell::Id(a))
            .set_cell("sequence", Cell::Int(31)),
    )
    .await
    .unwrap();
    ownership(&row, &mut tx, a, 31).await;
    write(
        &mut tx,
        &row,
        RawVals::new().set_cell("sequence", Cell::Int(32)),
    )
    .await
    .unwrap();
    ownership(&row, &mut tx, a, 32).await;
    // Malformed native cells must not be interpreted as absent ownership.
    for malformed in [Cell::Str("wrong".into()), Cell::Int(1), Cell::Id(0)] {
        let error = write(
            &mut tx,
            &row,
            RawVals::new().set_cell("company_id", malformed),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
        assert_eq!(
            error.message(),
            format!(
                "Invalid value for {model}.company_id: expected a positive company id or null."
            )
        );
        ownership(&row, &mut tx, a, 32).await;
    }
    tx.env()
        .records(model)
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("company_id", Cell::Id(b)))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn warehouse_company_is_immutable() {
    immutable_case("stock.warehouse").await;
}

#[tokio::test]
async fn operation_type_company_is_immutable() {
    immutable_case("stock.picking.type").await;
}

#[tokio::test]
async fn ownership_batches_are_atomic() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let b = company(&mut tx, "B").await;
    for model in ["stock.warehouse", "stock.picking.type"] {
        let mut rows = Vec::new();
        for (c, name) in [(a, "First"), (a, "Second"), (b, "Third")] {
            rows.push(if model == "stock.warehouse" {
                warehouse(&mut tx, c, name, name).await
            } else {
                picking_type(&mut tx, c, name).await
            });
        }
        let same = rows[0].union(&rows[1]).unwrap();
        write(
            &mut tx,
            &same,
            RawVals::new()
                .set_cell("company_id", Cell::Id(a))
                .set_cell("sequence", Cell::Int(21)),
        )
        .await
        .unwrap();
        let all = same.union(&rows[2]).unwrap();
        write(
            &mut tx,
            &all,
            RawVals::new().set_cell("sequence", Cell::Int(22)),
        )
        .await
        .unwrap();
        for batch in [&all, &rows[2].union(&same).unwrap()] {
            let error = write(
                &mut tx,
                batch,
                RawVals::new()
                    .set_cell("company_id", Cell::Id(a))
                    .set_cell("sequence", Cell::Int(99)),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UserError);
            assert_eq!(error.message(), IMMUTABLE);
            for (row, company) in rows.iter().zip([a, a, b]) {
                ownership(row, &mut tx, company, 22).await;
            }
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_topology_links_are_refused_on_write() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let b = company(&mut tx, "B").await;
    let wa = warehouse(&mut tx, a, "A", "A").await;
    let wb = warehouse(&mut tx, b, "B", "B").await;
    let pa = picking_type(&mut tx, a, "A").await;
    let pb = picking_type(&mut tx, b, "B").await;
    let vb = location_in(&mut tx, "Foreign", "view", None, Some(b)).await;
    for (row, field, label, target, target_name) in [
        (&wa, "view_location_id", "View Location", &vb, "Foreign"),
        (&wa, "pick_type_id", "Pick Type", &pb, "B"),
        (&pa, "warehouse_id", "Warehouse", &wb, "B"),
    ] {
        let before = row.cell(&mut tx, field).await.unwrap();
        let error = write(
            &mut tx,
            row,
            RawVals::new().set_cell(field, Cell::Id(target.ids()[0])),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
        assert_eq!(
            error.message(),
            format!(
                "These values link records of different companies:\n- “A” is in company “A”, and “{label}” ({field}: '{target_name}') points to a record of another company.\nChoose records of the same company, or records shared by all companies."
            )
        );
        assert_eq!(row.cell(&mut tx, field).await.unwrap(), before);
    }
    write(
        &mut tx,
        &wa,
        RawVals::new().set_cell("pick_type_id", Cell::Id(pa.ids()[0])),
    )
    .await
    .unwrap();
    write(
        &mut tx,
        &pa,
        RawVals::new().set_cell("warehouse_id", Cell::Id(wa.ids()[0])),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Field-write denial must precede both ownership comparison and malformed-value diagnostics.
#[tokio::test]
async fn field_write_denial_precedes_ownership_guard() {
    let policy = erp_testkit::FieldPolicy::new()
        .deny("stock.warehouse", "sequence", FieldOp::Write)
        .deny("stock.picking.type", "sequence", FieldOp::Write);
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
    ])
    .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .policy(policy)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let b = company(&mut tx, "B").await;
    let rows = [
        warehouse(&mut tx, a, "A", "A").await,
        picking_type(&mut tx, a, "A").await,
    ];
    for row in rows {
        // This policy explicitly needs no database principal resolution; model/record access
        // is allowed and only sequence writing is denied to this non-superuser fixture.
        let caller = row.with_user(7);
        for value in [Cell::Id(a), Cell::Id(b), Cell::Str("malformed".into())] {
            let error = write(
                &mut tx,
                &caller,
                RawVals::new()
                    .set_cell("company_id", value)
                    .set_cell("sequence", Cell::Int(99)),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(
                error.message(),
                format!("Writing {}.sequence is not allowed.", row.spec().name)
            );
            ownership(&row, &mut tx, a, 10).await;
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
