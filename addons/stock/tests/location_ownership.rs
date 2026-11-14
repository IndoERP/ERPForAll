// SPDX-License-Identifier: MIT OR Apache-2.0
//! Nearest-warehouse ownership follows location and warehouse topology changes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::{DomainPolicy, Harness, HarnessBuilder, Otherwise};

fn builder() -> HarnessBuilder {
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
}

async fn harness() -> Harness {
    builder().build().await.unwrap()
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

async fn owner(tx: &mut Transaction, location: &Records) -> Option<i64> {
    match location.cell(tx, "warehouse_id").await.unwrap() {
        Cell::Id(id) => Some(id),
        Cell::Null | Cell::Bool(false) => None,
        value => panic!("invalid warehouse ownership cell: {value:?}"),
    }
}

async fn root(tx: &mut Transaction, warehouse: &Records, field: &str) -> Records {
    let Cell::Id(id) = warehouse.cell(tx, field).await.unwrap() else {
        panic!("warehouse has no {field}");
    };
    tx.env()
        .records("stock.location")
        .unwrap()
        .browse(vec![id])
        .unwrap()
}

#[tokio::test]
async fn nearest_warehouse_follows_nested_root_reparent_without_manual_recompute() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "Company").await;
    let outer = warehouse(&mut tx, c, "Outer", "OUT").await;
    let inner = warehouse(&mut tx, c, "Inner", "IN").await;
    write(
        &mut tx,
        &outer,
        RawVals::new().set_cell("sequence", Cell::Int(100)),
    )
    .await
    .unwrap();
    write(
        &mut tx,
        &inner,
        RawVals::new().set_cell("sequence", Cell::Int(1)),
    )
    .await
    .unwrap();
    let inner_root = root(&mut tx, &inner, "view_location_id").await;
    let inner_stock = root(&mut tx, &inner, "lot_stock_id").await;
    let outer_stock = root(&mut tx, &outer, "lot_stock_id").await;
    write(
        &mut tx,
        &inner_root,
        RawVals::new().set_cell("location_id", Cell::Id(outer_stock.ids()[0])),
    )
    .await
    .unwrap();
    assert_eq!(owner(&mut tx, &inner_stock).await, Some(inner.ids()[0]));
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut fresh = h.begin().await.unwrap();
    assert_eq!(owner(&mut fresh, &inner_stock).await, Some(inner.ids()[0]));
    write(
        &mut fresh,
        &inner,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    assert_eq!(owner(&mut fresh, &inner_stock).await, Some(outer.ids()[0]));
    write(
        &mut fresh,
        &inner,
        RawVals::new().set_cell("active", Cell::Bool(true)),
    )
    .await
    .unwrap();
    assert_eq!(owner(&mut fresh, &inner_stock).await, Some(inner.ids()[0]));
    drop(fresh);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn warehouse_display_sequence_does_not_change_nearest_ownership() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "Company").await;
    let outer = warehouse(&mut tx, c, "Outer", "OUT").await;
    let inner = warehouse(&mut tx, c, "Inner", "IN").await;
    let inner_root = root(&mut tx, &inner, "view_location_id").await;
    let inner_stock = root(&mut tx, &inner, "lot_stock_id").await;
    let outer_stock = root(&mut tx, &outer, "lot_stock_id").await;
    write(
        &mut tx,
        &inner_root,
        RawVals::new().set_cell("location_id", Cell::Id(outer_stock.ids()[0])),
    )
    .await
    .unwrap();
    for (a, b) in [(100, 1), (1, 100)] {
        write(
            &mut tx,
            &outer,
            RawVals::new().set_cell("sequence", Cell::Int(a)),
        )
        .await
        .unwrap();
        write(
            &mut tx,
            &inner,
            RawVals::new().set_cell("sequence", Cell::Int(b)),
        )
        .await
        .unwrap();
        tx.flush().await.unwrap();
        assert_eq!(owner(&mut tx, &inner_stock).await, Some(inner.ids()[0]));
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn warehouse_creation_updates_existing_locations_and_new_children() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "Company").await;
    let wh = warehouse(&mut tx, c, "Warehouse", "WH").await;
    let view = root(&mut tx, &wh, "view_location_id").await;
    let stock = root(&mut tx, &wh, "lot_stock_id").await;
    let bin = location(&mut tx, "Bin", "internal", Some(stock.ids()[0])).await;
    let domain = Domain::condition("warehouse_id", Operator::Eq, wh.ids()[0]).unwrap();
    let mut found = tx
        .env()
        .records("stock.location")
        .unwrap()
        .search(&mut tx, &domain)
        .await
        .unwrap()
        .ids();
    found.sort_unstable();
    let mut expected = vec![view.ids()[0], stock.ids()[0], bin.ids()[0]];
    expected.sort_unstable();
    assert_eq!(
        found, expected,
        "search settles pending ownership in this transaction"
    );
    for loc in [&view, &stock, &bin] {
        assert_eq!(owner(&mut tx, loc).await, Some(wh.ids()[0]));
    }
    tx.flush().await.unwrap();
    let rows = tx
        .query(
            "SELECT warehouse_id::int8 FROM stock_location WHERE id = ANY($1) ORDER BY id",
            &[&vec![view.ids()[0], stock.ids()[0], bin.ids()[0]]],
        )
        .await
        .unwrap();
    assert!(
        rows.iter()
            .all(|r| r.get::<_, Option<i64>>(0) == Some(wh.ids()[0]))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn subtree_move_changes_ownership_and_rollback_restores_it() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "Company").await;
    let a = warehouse(&mut tx, c, "A", "A").await;
    // The first warehouse root is 1; advance the location fixture so the second root
    // is 11. These IDs share a text prefix but are different path components.
    for index in 0..8 {
        location(&mut tx, &format!("Unrelated gap {index}"), "internal", None).await;
    }
    let b = warehouse(&mut tx, c, "B", "B").await;
    let a_view = root(&mut tx, &a, "view_location_id").await;
    let b_view = root(&mut tx, &b, "view_location_id").await;
    assert!(
        b_view.ids()[0]
            .to_string()
            .starts_with(&a_view.ids()[0].to_string())
    );
    let a_stock = root(&mut tx, &a, "lot_stock_id").await;
    let b_stock = root(&mut tx, &b, "lot_stock_id").await;
    let branch = location(&mut tx, "Branch", "internal", Some(a_stock.ids()[0])).await;
    let leaf = location(&mut tx, "Leaf", "internal", Some(branch.ids()[0])).await;
    let unrelated = location(&mut tx, "Unrelated", "internal", Some(a_stock.ids()[0])).await;
    assert_eq!(owner(&mut tx, &leaf).await, Some(a.ids()[0]));
    assert_eq!(owner(&mut tx, &unrelated).await, Some(a.ids()[0]));
    tx.flush().await.unwrap();
    let original = leaf.cell(&mut tx, "parent_path").await.unwrap();
    let untouched_id = unrelated.ids()[0];
    let untouched: String = tx
        .query(
            "SELECT ctid::text FROM stock_location WHERE id=$1",
            &[&untouched_id],
        )
        .await
        .unwrap()[0]
        .get(0);
    let moved = branch.clone();
    let read_leaf = leaf.clone();
    let read_unrelated = unrelated.clone();
    let target = b_stock.ids()[0];
    let expected = b.ids()[0];
    let original_owner = a.ids()[0];
    let result: Result<()> = tx
        .savepoint(async move |tx| {
            moved
                .write(tx, RawVals::new().set_cell("location_id", Cell::Id(target)))
                .await?;
            assert_eq!(owner(tx, &read_leaf).await, Some(expected));
            assert_eq!(owner(tx, &read_unrelated).await, Some(original_owner));
            Err(Error::user("rollback ownership move"))
        })
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), "rollback ownership move");
    assert_eq!(owner(&mut tx, &leaf).await, Some(a.ids()[0]));
    assert_eq!(leaf.cell(&mut tx, "parent_path").await.unwrap(), original);
    assert_eq!(owner(&mut tx, &unrelated).await, Some(a.ids()[0]));
    write(
        &mut tx,
        &branch,
        RawVals::new().set_cell("location_id", Cell::Null),
    )
    .await
    .unwrap();
    assert_eq!(owner(&mut tx, &leaf).await, None);
    tx.flush().await.unwrap();
    assert!(tx.is_cached(&unrelated, "warehouse_id").unwrap());
    let after: String = tx
        .query(
            "SELECT ctid::text FROM stock_location WHERE id=$1",
            &[&untouched_id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(after, untouched, "unrelated subtree row was rewritten");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn warehouse_root_activity_and_deletion_refresh_descendants() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "Company").await;
    let wh = warehouse(&mut tx, c, "Warehouse", "WH").await;
    let old_root = root(&mut tx, &wh, "view_location_id").await;
    let old_stock = root(&mut tx, &wh, "lot_stock_id").await;
    let new_root = location(&mut tx, "New root", "view", None).await;
    let new_leaf = location(&mut tx, "New leaf", "internal", Some(new_root.ids()[0])).await;
    assert_eq!(owner(&mut tx, &old_stock).await, Some(wh.ids()[0]));
    assert_eq!(owner(&mut tx, &new_leaf).await, None);
    write(
        &mut tx,
        &wh,
        RawVals::new().set_cell("view_location_id", Cell::Id(new_root.ids()[0])),
    )
    .await
    .unwrap();
    assert_eq!(owner(&mut tx, &old_root).await, None);
    assert_eq!(owner(&mut tx, &old_stock).await, None);
    assert_eq!(owner(&mut tx, &new_leaf).await, Some(wh.ids()[0]));
    write(
        &mut tx,
        &wh,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    assert_eq!(owner(&mut tx, &new_leaf).await, None);
    write(
        &mut tx,
        &wh,
        RawVals::new().set_cell("active", Cell::Bool(true)),
    )
    .await
    .unwrap();
    assert_eq!(owner(&mut tx, &new_leaf).await, Some(wh.ids()[0]));
    wh.unlink(&mut tx).await.unwrap();
    assert_eq!(owner(&mut tx, &new_leaf).await, None);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ownership_is_derived_and_parent_cycles_leave_ownership_unchanged() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "Company").await;
    let wh = warehouse(&mut tx, c, "Warehouse", "WH").await;
    let stock = root(&mut tx, &wh, "lot_stock_id").await;
    let leaf = location(&mut tx, "Leaf", "internal", Some(stock.ids()[0])).await;
    assert_eq!(owner(&mut tx, &leaf).await, Some(wh.ids()[0]));
    let error = write(
        &mut tx,
        &leaf,
        RawVals::new().set_cell("warehouse_id", Cell::Null),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Location warehouse ownership is derived from its hierarchy and cannot be assigned."
    );
    let error = write(
        &mut tx,
        &stock,
        RawVals::new().set_cell("location_id", Cell::Id(leaf.ids()[0])),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(
        error
            .message()
            .contains("a record cannot sit under itself or under one of its own children")
    );
    assert_eq!(owner(&mut tx, &leaf).await, Some(wh.ids()[0]));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caller_cannot_read_warehouse_but_cannot_change_derived_ownership() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .deny("stock.warehouse", erp_sdk::Operation::Read)
        .build()
        .unwrap();
    let h = builder().policy(policy).build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "Company").await;
    let wh = warehouse(&mut tx, c, "Hidden warehouse", "HID").await;
    let stock = root(&mut tx, &wh, "lot_stock_id").await;
    let detached = location(&mut tx, "Étagère 架", "internal", None).await;
    tx.query("INSERT INTO res_users (id, name, login, active, company_id) VALUES (7, 'Reader', 'reader', true, $1)", &[&c]).await.unwrap();
    tx.query(
        "INSERT INTO res_company_users_rel (user_id, cid) VALUES (7, $1)",
        &[&c],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(7).await.unwrap();
    let record = tx
        .env()
        .records("stock.location")
        .unwrap()
        .browse(detached.ids())
        .unwrap();
    let forbidden = tx
        .env()
        .records("stock.warehouse")
        .unwrap()
        .browse(wh.ids())
        .unwrap();
    let error = forbidden.cell(&mut tx, "name").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you read 'Warehouse' (stock.warehouse) records."
    );
    record
        .write(
            &mut tx,
            RawVals::new().set_cell("location_id", Cell::Id(stock.ids()[0])),
        )
        .await
        .unwrap();
    assert_eq!(owner(&mut tx, &record).await, Some(wh.ids()[0]));
    let archived_context = record.with_context([("active_test", Value::Bool(false))]);
    assert_eq!(owner(&mut tx, &archived_context).await, Some(wh.ids()[0]));
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(owner(&mut tx, &detached).await, Some(wh.ids()[0]));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn direct_tree_creation_flushes_before_any_ownership_read() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let locations = tx.env().records("stock.location").unwrap();
    let view = locations
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Direct root"))
                    .set_cell("usage", text("view")),
            ],
        )
        .await
        .unwrap();
    let leaf = locations
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Direct leaf"))
                    .set_cell("usage", text("internal"))
                    .set_cell("location_id", Cell::Id(view.ids()[0])),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        leaf.cell(&mut tx, "complete_name").await.unwrap(),
        text("Direct root/Direct leaf")
    );
    assert_eq!(owner(&mut tx, &leaf).await, None);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn derived_fields_refuse_batch_create_and_inverse_commands() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .env()
        .records("stock.location")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new().set_cell("name", text("Valid first row")),
                RawVals::new()
                    .set_cell("name", text("Invalid second row"))
                    .set_cell("warehouse_id", Cell::Null),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Location warehouse ownership is derived from its hierarchy and cannot be assigned."
    );
    assert_eq!(
        std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<erp_addon_stock::StockError>(),
        Some(&erp_addon_stock::StockError::LocationOwnershipDerived)
    );
    let count = tx
        .query("SELECT count(*)::int8 FROM stock_location", &[])
        .await
        .unwrap();
    assert_eq!(count[0].get::<_, i64>(0), 0);
    let record = location(&mut tx, "Standalone", "internal", None).await;
    let error = write(
        &mut tx,
        &record,
        RawVals::new().set_commands("warehouse_view_ids", [erp_sdk::Command::Clear]),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Location warehouse ownership is derived from its hierarchy and cannot be assigned."
    );
    assert_eq!(owner(&mut tx, &record).await, None);
    drop(tx);
    h.cleanup().await.unwrap();
}
