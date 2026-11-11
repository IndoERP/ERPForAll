//! Allocation budget of `Records::read_rows` (design note, §12;
//! standards §5b): 5 fields of 10,000 and 20,000 order lines in a transaction whose cache was
//! dropped. The difference of the two runs is the cost of 10,000 rows; it must stay within the
//! per-row budget, which `read`'s dictionaries (one map with owned keys per row) exceed: measured
//! here too, so the budget is shown to fail on a map per row.
//!
//! One test in its own binary, run on the main thread by `erp_alloc_budget::harness`
//! (`erp_alloc_budget`: one profiler, every thread counted).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

mod bench_fixture;

use erp_orm::Load;
use erp_sdk::Records;
use erp_testkit::alloc::{self, AllocStats, Budget};
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    read_rows_allocates_within_the_per_row_budget,
    read_rows_semantics,
);

/// blocks per row without the driver's per-batch blocks (see the assertion).
const PER_ROW_BLOCKS: f64 = 1.08;
const FIELDS: [&str; 5] = ["code", "order_id", "qty", "price_unit", "price_subtotal"];
const SMALL: usize = 10_000;
const LARGE: usize = 20_000;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Batch,
    Maps,
}

fn measure(
    rt: &tokio::runtime::Runtime,
    h: &erp_testkit::Harness,
    ids: &[i64],
    shape: Shape,
) -> (AllocStats, u64, DriverBlocks) {
    let mut tx = rt.block_on(h.begin_as(2)).unwrap();
    let lines = |tx: &erp_sdk::Transaction, ids: &[i64]| -> Records {
        tx.env()
            .records("bench.order.line")
            .unwrap()
            .browse(ids.iter().copied())
            .unwrap()
    };
    rt.block_on(async {
        tx.open().await.unwrap();
        lines(&tx, &ids[..1])
            .read_rows(&mut tx, &FIELDS, Load::Raw)
            .await
            .unwrap();
        tx.invalidate_all().await.unwrap();
    });
    let records = lines(&tx, ids);
    let before = tx.query_count();
    let batches_before = tx.driver_response_batches().expect("connection");
    let stats = match shape {
        Shape::Batch => {
            let (stats, rows) =
                alloc::measure(|| rt.block_on(records.read_rows(&mut tx, &FIELDS, Load::Raw)))
                    .expect("measure");
            assert_eq!(rows.unwrap().len(), ids.len());
            stats
        }
        Shape::Maps => {
            let (stats, rows) =
                alloc::measure(|| rt.block_on(records.read_with(&mut tx, &FIELDS, Load::Raw)))
                    .expect("measure");
            assert_eq!(rows.unwrap().len(), ids.len());
            stats
        }
    };
    let statements = tx.query_count() - before;
    let batches = tx
        .driver_response_batches()
        .expect("connection")
        .since(batches_before)
        .expect("one connection: the counts only grow");
    let batches = DriverBlocks::new(rt.handle(), batches).expect("the driver's counts of a window");
    rt.block_on(tx.rollback()).unwrap();
    (stats, statements, batches)
}

fn read_rows_allocates_within_the_per_row_budget() {
    let rt = runtime();
    let h = rt.block_on(bench_fixture::harness());
    rt.block_on(bench_fixture::dataset(&h, LARGE / 10, 10, 100, 1));
    let ids: Vec<i64> = rt.block_on(async {
        let mut tx = h.begin().await.unwrap();
        let rows = tx
            .query("SELECT id FROM bench_order_line ORDER BY id", &[])
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        rows.iter().map(|r| r.get::<_, i64>(0)).collect()
    });
    assert_eq!(ids.len(), LARGE);
    let (small, small_statements, small_batches) = measure(&rt, &h, &ids[..SMALL], Shape::Batch);
    let (large, large_statements, large_batches) = measure(&rt, &h, &ids, Shape::Batch);
    let rows = (LARGE - SMALL) as f64;
    let per_row_blocks = (large.total_blocks - small.total_blocks) as f64 / rows;
    // the driver's per-batch blocks of the added batches, counted: how many batches the rows
    // arrive in, and how many park the driver, depends on the host's load (lesson)
    let per_row_net = per_row_blocks - large_batches.more_than(&small_batches) / rows;
    let per_row_bytes = (large.total_bytes - small.total_bytes) as f64 / rows;
    let transient = |s: &AllocStats| s.peak_bytes as f64 - s.end_bytes as f64;
    let per_row_transient = (transient(&large) - transient(&small)) / rows;
    println!(
        "read_rows: {SMALL} rows {small:?} in {small_batches}; {LARGE} rows {large:?} in \
         {large_batches}; per row {per_row_blocks:.3} blocks ({per_row_net:.3} without \
         the driver's per-batch blocks), {per_row_bytes:.1} B \
         requested, {per_row_transient:.1} B transient; statements {small_statements} / \
         {large_statements}"
    );
    // perf-stream §9.1: rows are decoded as they arrive, so what is live beyond the result and
    // the cache at the peak grows by the cache's own growth only
    assert!(
        per_row_transient <= 40.0,
        "{per_row_transient:.1} B transient per row"
    );
    // per row ≤ 1.08 blocks without the driver's per-batch blocks (1.1 with them
    // until lesson, of which 0.02 per row were the driver's batches at idle; 1.022 measured with them counted;
    // 2.04 with PERF.STREAM-CORE: the driver's row range, now gone, and one `Arc<str>` of the
    // fetch until PERF.STRINGS; the batch itself allocates per column, not per row) and ≤ 780 B
    // requested (565.4–650.7 B measured with PERF.STREAM-CORE, perf-rows §12.4.1, plus margin; within
    // the §10 ceiling of 700 B + margin; 1,130.7 B while the driver's rows were collected), ≤ 500
    // blocks fixed
    assert!(
        per_row_net <= PER_ROW_BLOCKS,
        "{per_row_net:.3} blocks per row without the driver's per-batch blocks"
    );
    assert!(per_row_bytes <= 780.0, "{per_row_bytes:.1} B per row");
    let budget =
        Budget::per_row(SMALL as u64, (2, 780), Budget::new(500, 1 << 20)).expect("budget");
    small.assert_within(&budget);
    // one statement per chunk of 10,000 ids (`prefetch_chunk`), whatever the shape
    assert_eq!(small_statements, 1);
    assert_eq!(large_statements, 2);
    // the budget can fail: `read`'s dictionaries for the same rows are over it
    let (maps, maps_statements, _) = measure(&rt, &h, &ids[..SMALL], Shape::Maps);
    println!("read (maps): {SMALL} rows {maps:?}");
    assert_eq!(maps_statements, small_statements);
    assert!(maps.check(&budget).is_err(), "{maps:?}");
}

fn read_rows_semantics() {
    let rt = runtime();
    rt.block_on(async {
        let h = bench_fixture::harness().await;
        bench_fixture::dataset(&h, LARGE / 10, 10, 100, 1).await;
        let mut setup = h.begin().await.unwrap();
        let ids: Vec<i64> = setup
            .query("SELECT id FROM bench_order_line ORDER BY id", &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(ids.len(), LARGE);
        setup.rollback().await.unwrap();
        drop(setup);
        for (n, shape, queries) in [
            (SMALL, Shape::Batch, 1),
            (LARGE, Shape::Batch, 2),
            (SMALL, Shape::Maps, 1),
        ] {
            let mut tx = h.begin_as(2).await.unwrap();
            tx.open().await.unwrap();
            let records = tx
                .env()
                .records("bench.order.line")
                .unwrap()
                .browse(ids[..n].iter().copied())
                .unwrap();
            let before = tx.query_count();
            let count = match shape {
                Shape::Batch => records
                    .read_rows(&mut tx, &FIELDS, Load::Raw)
                    .await
                    .unwrap()
                    .len(),
                Shape::Maps => records
                    .read_with(&mut tx, &FIELDS, Load::Raw)
                    .await
                    .unwrap()
                    .len(),
            };
            assert_eq!(count, n);
            assert_eq!(tx.query_count() - before, queries);
            tx.rollback().await.unwrap();
        }
        h.cleanup().await.unwrap();
    });
}
