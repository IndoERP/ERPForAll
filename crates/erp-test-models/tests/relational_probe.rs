//! Timing probe for computes through relations (slice S4b; not a parity case, not a benchmark of
//! record — BENCH-005 is measured).
//! `ERP_PROBE_ORDERS` sets the number of orders (default 10,000), each with 10
//! lines.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::time::Instant;

use erp_sdk::{Cell, Command, Domain, RawVals, Value};

#[tokio::test]
#[ignore = "timing probe; run explicitly with --run-ignored only"]
async fn probe_relational_recompute() {
    let orders_n: usize = std::env::var("ERP_PROBE_ORDERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = tx.env().records("test_dep.order").unwrap();
    let vals: Vec<RawVals> = (0..orders_n)
        .map(|o| {
            RawVals::new()
                .set_cell("name", Cell::Str(format!("o{o}").into()))
                .set_commands(
                    "line_ids",
                    (0..10).map(|l| {
                        Command::create(
                            RawVals::new()
                                .set_cell("product", Cell::Str(format!("p{l}").into()))
                                .set_cell("price_unit", Cell::Float(1.0)),
                        )
                    }),
                )
        })
        .collect();
    let t = Instant::now();
    let q = tx.query_count();
    let created = orders.create(&mut tx, vals).await.unwrap();
    tx.flush().await.unwrap();
    let create = (t.elapsed(), tx.query_count() - q);
    tx.commit().await.unwrap();

    // a new request: nothing cached; every line's price changes
    let lines_model = tx.env().records("test_dep.line").unwrap();
    let lines = lines_model.search(&mut tx, &Domain::TRUE).await.unwrap();
    let q = tx.query_count();
    let t = Instant::now();
    lines
        .write_values(&mut tx, &support::map(&[("price_unit", Value::Float(2.0))]))
        .await
        .unwrap();
    let write = (t.elapsed(), tx.query_count() - q);
    let q = tx.query_count();
    let t = Instant::now();
    tx.flush().await.unwrap();
    let flush = (t.elapsed(), tx.query_count() - q);
    tx.commit().await.unwrap();

    // one line per order changes (the traversal reads each line's order)
    let some: Vec<i64> = lines.ids().into_iter().step_by(10).collect();
    let some = lines_model.browse(some).unwrap();
    let q = tx.query_count();
    let t = Instant::now();
    some.write_values(&mut tx, &support::map(&[("qty", Value::Float(3.0))]))
        .await
        .unwrap();
    let t1 = Instant::now();
    some.values(&mut tx, "price_subtotal").await.unwrap();
    let pull_lines = t1.elapsed();
    let t1 = Instant::now();
    created.values(&mut tx, "amount_total").await.unwrap();
    let pull_orders = t1.elapsed();
    let t1 = Instant::now();
    tx.flush().await.unwrap();
    let flush_partial = t1.elapsed();
    eprintln!(
        "probe partial: subtotals {pull_lines:?}, totals {pull_orders:?}, flush {flush_partial:?}"
    );
    let partial = (t.elapsed(), tx.query_count() - q);
    tx.commit().await.unwrap();

    let totals = created.values(&mut tx, "amount_total").await.unwrap();
    assert!(
        totals.iter().all(|v| *v == Value::Float(24.0)),
        "{:?}",
        &totals[..2]
    );
    eprintln!(
        "probe orders={orders_n} lines={}: create {:?} ({} statements); write {:?} ({}); flush \
         (subtotals + totals) {:?} ({}); one line per order + flush {:?} ({})",
        orders_n * 10,
        create.0,
        create.1,
        write.0,
        write.1,
        flush.0,
        flush.1,
        partial.0,
        partial.1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
