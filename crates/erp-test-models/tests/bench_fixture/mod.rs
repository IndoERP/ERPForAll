//! The benchmark models (`erp_test_models::bench`, BENCH-002's `web_read`) on a fresh database,
//! for the result-batch tests (`web_read_rows.rs`, `alloc_web_read.rs`, `alloc_read.rs`).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers"
)]

use std::collections::BTreeMap;

use erp_orm::{Spec, SpecItem};
use erp_sdk::{Records, Transaction, Value};
use erp_test_models::{base_users, bench};
use erp_testkit::Harness;

/// `base_users` + the bench mirror addon.
pub async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &bench::ADDON])
        .build()
        .await
        .expect("harness")
}

/// The BENCH-002 specification (`bench/workloads.json` `bench_002_specification`).
pub fn spec_002() -> Spec {
    Spec::new()
        .field("code")
        .field("name")
        .field("state")
        .field("date_order")
        .field("amount_total")
        .field("partner_name")
        .with("partner_id", SpecItem::fields(Spec::new().field("code")))
        .with(
            "line_ids",
            SpecItem::fields(
                Spec::new()
                    .field("code")
                    .field("qty")
                    .field("price_unit")
                    .field("price_subtotal"),
            ),
        )
        .with("tag_ids", SpecItem::fields(Spec::new().field("code")))
}

pub fn vals(pairs: Vec<(&str, Value)>) -> BTreeMap<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// `[(6, 0, ids)]`.
pub fn set_ids(ids: &[i64]) -> Value {
    Value::list([Value::list([
        Value::Int(6),
        Value::Int(0),
        Value::list(ids.iter().map(|i| Value::Int(*i))),
    ])])
}

/// `orders` orders over `partners` partners and `tags` tags, `lines` lines each, the i-th order
/// with tags `i % tags` and `(i + 1) % tags` (one tag when `tags` is 1), committed. Returns the
/// order ids in creation order.
pub async fn dataset(
    h: &Harness,
    orders: usize,
    lines: usize,
    partners: usize,
    tags: usize,
) -> Vec<i64> {
    let mut tx = h.begin().await.unwrap();
    let partner_rows: Vec<_> = (0..partners)
        .map(|i| {
            vals(vec![
                ("code", Value::str(format!("P{i:06}"))),
                ("name", Value::str(format!("Partner {i}"))),
                ("country", Value::str("id")),
            ])
        })
        .collect();
    let partner_ids = create(&mut tx, "bench.partner", &partner_rows).await;
    let tag_rows: Vec<_> = (0..tags)
        .map(|i| {
            vals(vec![
                ("code", Value::str(format!("T{i:03}"))),
                ("name", Value::str(format!("Tag {i:03}"))),
            ])
        })
        .collect();
    let tag_ids = create(&mut tx, "bench.tag", &tag_rows).await;
    let mut order_ids = Vec::with_capacity(orders);
    for chunk in (0..orders).collect::<Vec<_>>().chunks(500) {
        let rows: Vec<_> = chunk
            .iter()
            .map(|i| {
                let mut wanted = vec![tag_ids[i % tags]];
                if tags > 1 {
                    wanted.push(tag_ids[(i + 1) % tags]);
                }
                vals(vec![
                    ("code", Value::str(format!("O{i:08}"))),
                    ("name", Value::str(format!("SO{i:08}"))),
                    ("partner_id", Value::Int(partner_ids[i % partners])),
                    ("tag_ids", set_ids(&wanted)),
                ])
            })
            .collect();
        let made = create(&mut tx, "bench.order", &rows).await;
        let line_rows: Vec<_> = made
            .iter()
            .zip(chunk)
            .flat_map(|(order, i)| {
                (0..lines).map(move |l| {
                    vals(vec![
                        ("code", Value::str(format!("L{i:08}-{l:02}"))),
                        ("order_id", Value::Int(*order)),
                        ("product_name", Value::str(format!("Product {l}"))),
                        ("qty", Value::Float((l + 1) as f64)),
                        ("price_unit", Value::Float(1.25 * (l as f64 + 1.0))),
                    ])
                })
            })
            .collect();
        create(&mut tx, "bench.order.line", &line_rows).await;
        order_ids.extend(made);
    }
    tx.commit().await.unwrap();
    order_ids
}

pub async fn create(
    tx: &mut Transaction,
    model: &str,
    rows: &[BTreeMap<String, Value>],
) -> Vec<i64> {
    let made = tx
        .env()
        .records(model)
        .unwrap()
        .create_values(tx, rows)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    made.ids()
}

pub fn orders(tx: &Transaction, ids: &[i64]) -> Records {
    tx.env()
        .records("bench.order")
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}
