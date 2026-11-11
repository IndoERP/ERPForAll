//! Allocation budget of reading a result batch (design note):
//! `get`, `str` and `f64` over every row and key allocate nothing. One test in its own binary,
//! run on the main thread by `erp_alloc_budget::harness` (`erp_alloc_budget`: one profiler, all
//! threads counted).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_alloc_budget::{self as alloc, AllocStats, Budget};
use erp_fields::Value;
use erp_fields::rows::{Column, ColumnData, Key, Layout, Rows, RowsBuilder};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    reading_a_batch_allocates_nothing,
    reading_a_batch_preserves_values,
);

const KEYS: [&str; 5] = ["id", "code", "qty", "price_unit", "price_subtotal"];

fn rows(n: usize) -> Rows {
    let layout = Layout::for_model("bench.order.line", KEYS.map(Key::from)).unwrap();
    let mut b = RowsBuilder::new(layout, n).unwrap();
    b.column("id", Column::new(ColumnData::Int((1..=n as i64).collect())))
        .unwrap();
    b.column(
        "code",
        Column::new(ColumnData::Str(
            (0..n).map(|i| Some(Arc::from(format!("L{i}")))).collect(),
        )),
    )
    .unwrap();
    for key in &KEYS[2..] {
        b.column(
            key,
            Column::new(ColumnData::Float((0..n).map(|i| i as f64).collect())),
        )
        .unwrap();
    }
    b.finish().unwrap()
}

/// Reads every key of every row through `get` and the typed reads; returns a checksum.
fn read_all(rows: &Rows) -> f64 {
    let mut sum = 0.0;
    for i in rows.indices() {
        for key in KEYS {
            match rows.get(i, key).unwrap() {
                Value::Int(v) => sum += v as f64,
                Value::Float(v) => sum += v,
                Value::Str(s) => sum += s.len() as f64,
                other => panic!("unexpected {other:?}"),
            }
        }
        sum += rows.str(i, "code").unwrap().map_or(0, str::len) as f64;
        for key in &KEYS[2..] {
            sum += rows.f64(i, key).unwrap().unwrap_or_default();
        }
    }
    sum
}

fn measure_read(n: usize) -> AllocStats {
    let rows = rows(n);
    let (stats, sum) = alloc::measure(|| read_all(&rows)).expect("measure");
    assert!(sum > 0.0);
    stats
}

fn reading_a_batch_allocates_nothing() {
    let small = measure_read(10_000);
    let large = measure_read(20_000);
    println!("view: {small:?} / {large:?}");
    let budget = Budget::new(0, 0);
    small.assert_within(&budget);
    large.assert_within(&budget);
    // the budget can fail: converting rows to maps allocates per row
    let rows = rows(1_000);
    let (maps, kept) = alloc::measure(|| {
        rows.iter()
            .map(|row| row.to_map().unwrap())
            .collect::<Vec<_>>()
    })
    .expect("measure");
    assert_eq!(kept.len(), 1_000);
    assert!(maps.check(&budget).is_err(), "{maps:?}");
}

fn reading_a_batch_preserves_values() {
    for n in [10_000, 20_000] {
        assert!(read_all(&rows(n)) > 0.0);
    }
    let rows = rows(1_000);
    let maps: Vec<_> = rows.iter().map(|row| row.to_map().unwrap()).collect();
    assert_eq!(maps.len(), 1_000);
    assert_eq!(maps[999].get("id"), Some(&Value::Int(1000)));
}
