//! Allocation budget of building a result batch: a
//! 5-key batch (id, text, 3 floats) gathered from prepared values the way the read path gathers
//! cache columns. One test in its own binary, run on the main thread by `erp_alloc_budget::harness`
//! (`erp_alloc_budget`: one profiler, all threads counted).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_alloc_budget::{self as alloc, AllocStats, Budget};
use erp_fields::rows::{Column, ColumnData, Key, Layout, Rows, RowsBuilder};
use erp_fields::{Value, ValueMap};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    building_a_batch_costs_no_allocation_per_row,
    building_a_batch_preserves_values,
);

const KEYS: [&str; 5] = ["id", "code", "qty", "price_unit", "price_subtotal"];

/// The cache's columns: text shared by `Arc`, numbers by value.
struct Prepared {
    ids: Vec<i64>,
    codes: Vec<Option<Arc<str>>>,
    numbers: [Vec<f64>; 3],
}

fn prepared(n: usize) -> Prepared {
    Prepared {
        ids: (1..=n as i64).collect(),
        codes: (0..n)
            .map(|i| Some(Arc::from(format!("L{i:08}"))))
            .collect(),
        numbers: [
            (0..n).map(|i| i as f64).collect(),
            (0..n).map(|i| i as f64 * 1.5).collect(),
            (0..n).map(|i| i as f64 * 2.5).collect(),
        ],
    }
}

/// One presized vector per key, scalars copied and text `Arc`s cloned, then the batch.
fn build(p: &Prepared) -> Rows {
    let n = p.ids.len();
    let layout = Layout::for_model("bench.order.line", KEYS.map(Key::from)).unwrap();
    let mut builder = RowsBuilder::new(layout, n).unwrap();
    let mut ids = Vec::with_capacity(n);
    ids.extend_from_slice(&p.ids);
    builder
        .column("id", Column::new(ColumnData::Int(ids)))
        .unwrap();
    let mut codes = Vec::with_capacity(n);
    codes.extend(p.codes.iter().cloned());
    builder
        .column("code", Column::new(ColumnData::Str(codes)))
        .unwrap();
    for (key, values) in KEYS[2..].iter().zip(&p.numbers) {
        let mut column = Vec::with_capacity(n);
        column.extend_from_slice(values);
        builder
            .column(key, Column::new(ColumnData::Float(column)))
            .unwrap();
    }
    builder.finish().unwrap()
}

/// Today's shape for the same data: one map per row with owned keys.
fn maps(p: &Prepared) -> Vec<ValueMap> {
    (0..p.ids.len())
        .map(|i| {
            let mut row = ValueMap::with_capacity(5);
            row.insert("id".to_owned(), Value::Int(p.ids[i]));
            row.insert(
                "code".to_owned(),
                p.codes[i].clone().map_or(Value::False, Value::Str),
            );
            for (key, values) in KEYS[2..].iter().zip(&p.numbers) {
                row.insert((*key).to_owned(), Value::Float(values[i]));
            }
            row
        })
        .collect()
}

fn measure_build(n: usize) -> AllocStats {
    let p = prepared(n);
    let (stats, rows) = alloc::measure(|| build(&p)).expect("measure");
    assert_eq!(rows.len(), n);
    assert_eq!(rows.str(n - 1, "code").unwrap(), p.codes[n - 1].as_deref());
    stats
}

fn building_a_batch_costs_no_allocation_per_row() {
    let small = measure_build(10_000);
    let large = measure_build(20_000);
    // marginal cost of one row from two measurements (the fixed part cancels out)
    let per_row_blocks = (large.total_blocks - small.total_blocks) / 10_000;
    let per_row_bytes = (large.total_bytes - small.total_bytes) / 10_000;
    println!("build: {small:?} / {large:?}; per row {per_row_blocks} blocks, {per_row_bytes} B");
    assert_eq!(per_row_blocks, 0, "{small:?} {large:?}");
    // 8 (id) + 16 (text handle) + 3 × 8 (floats) = 48 B of values per row
    assert!(per_row_bytes <= 64, "{per_row_bytes} B per row");
    // budget of perf-rows §10: 0 blocks and ≤ 64 B per row, ≤ 16 blocks fixed (layout, column
    // vectors, the batch)
    let budget = Budget::per_row(20_000, (0, 64), Budget::new(16, 4_096)).expect("budget");
    large.assert_within(&budget);
    // the budget can fail: today's maps for the same rows are far over it
    let p = prepared(20_000);
    let (old, kept) = alloc::measure(|| maps(&p)).expect("measure");
    assert_eq!(kept.len(), 20_000);
    println!("maps: {old:?}");
    assert!(old.check(&budget).is_err(), "{old:?}");
}

fn building_a_batch_preserves_values() {
    for n in [10_000, 20_000] {
        let p = prepared(n);
        let rows = build(&p);
        assert_eq!(rows.len(), n);
        assert_eq!(rows.str(n - 1, "code").unwrap(), p.codes[n - 1].as_deref());
        assert_eq!(rows.get(0, "id").unwrap(), Value::Int(p.ids[0]));
        for (key, values) in KEYS[2..].iter().zip(&p.numbers) {
            assert_eq!(rows.f64(n - 1, key).unwrap(), Some(values[n - 1]));
        }
    }
    assert_eq!(maps(&prepared(20_000)).len(), 20_000);
}
