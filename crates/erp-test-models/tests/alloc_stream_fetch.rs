//! Allocation budget of a search with fetched columns (`SearchQuery::execute`, design note
//! ; standards §5b): 5 columns of `bench.order.line` (two of
//! them text) for 20,000 and 40,000 lines, one statement each, in a transaction whose cache was
//! dropped. The rows are decoded into the cache as the driver receives them, so what is live
//! beyond the answer at the peak (`peak_bytes − end_bytes`, the transient) must not grow with the
//! row count beyond the cache's own growth: ≤ 40 B per added row. Collecting the rows first (the
//! driver's `Vec<Row>`, their column ranges and the received bytes) held ~347 B per row.
//!
//! One test in its own binary, run on the main thread by `erp_alloc_budget::harness`
//! (`erp_alloc_budget`: one profiler, every thread counted). The runtime is current-thread and
//! built outside the measured closure.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

mod bench_fixture;

use erp_sdk::{Domain, SearchOptions};
use erp_testkit::alloc::{self, AllocStats};
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    search_with_fetched_columns_decodes_rows_as_they_arrive,
    fetched_columns_semantics,
);

const FIELDS: [&str; 5] = ["code", "order_id", "product_name", "qty", "price_unit"];
const SMALL: usize = 20_000;
const LARGE: usize = 40_000;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// One measured search of the first `n` lines with the 5 columns fetched (cache dropped first,
/// prepared statements warm), with the statements it sent.
fn measure(
    rt: &tokio::runtime::Runtime,
    h: &erp_testkit::Harness,
    n: usize,
) -> (AllocStats, u64, DriverBlocks) {
    let mut tx = rt.block_on(h.begin()).unwrap();
    let lines = tx.env().records("bench.order.line").unwrap();
    let opts = |limit: usize| {
        SearchOptions::new()
            .limit(limit as u64)
            .order("id")
            .fetch_fields(FIELDS)
    };
    rt.block_on(async {
        tx.open().await.unwrap();
        lines
            .search_with(&mut tx, &Domain::TRUE, &opts(1))
            .await
            .unwrap();
        tx.invalidate_all().await.unwrap();
    });
    let opts = opts(n);
    let before = tx.query_count();
    let batches_before = tx.driver_response_batches().expect("connection");
    let (stats, found) =
        alloc::measure(|| rt.block_on(lines.search_with(&mut tx, &Domain::TRUE, &opts)))
            .expect("measure");
    assert_eq!(found.unwrap().len(), n);
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

fn search_with_fetched_columns_decodes_rows_as_they_arrive() {
    let rt = runtime();
    let h = rt.block_on(bench_fixture::harness());
    rt.block_on(bench_fixture::dataset(&h, LARGE / 10, 10, 100, 1));
    let (small, small_statements, small_batches) = measure(&rt, &h, SMALL);
    let (large, large_statements, large_batches) = measure(&rt, &h, LARGE);
    let rows = (LARGE - SMALL) as f64;
    let per_row_blocks = (large.total_blocks - small.total_blocks) as f64 / rows;
    // the driver's per-batch blocks of the added batches, counted: how many batches the rows
    // arrive in, and how many park the driver, depends on the host's load (lesson)
    let per_row_net = per_row_blocks - large_batches.more_than(&small_batches) / rows;
    let per_row_bytes = (large.total_bytes - small.total_bytes) as f64 / rows;
    let transient = |s: &AllocStats| s.peak_bytes as f64 - s.end_bytes as f64;
    let per_row_transient = (transient(&large) - transient(&small)) / rows;
    let per_row_retained = (large.end_bytes as f64 - small.end_bytes as f64) / rows;
    println!(
        "search_with 5 columns: {SMALL} lines {small:?} in {small_batches}; {LARGE} lines \
         {large:?} in {large_batches}; per row {per_row_blocks:.3} blocks \
         ({per_row_net:.3} without the driver's per-batch blocks), \
         {per_row_bytes:.1} B \
         requested, {per_row_transient:.1} B transient, {per_row_retained:.1} B retained; \
         statements {small_statements} / {large_statements}"
    );
    // one statement whatever the size
    assert_eq!(small_statements, 1);
    assert_eq!(large_statements, 1);
    // perf-stream §9.1: the transient grows by the cache's growth only (today's collected rows:
    // 346.8 B per row, `5dbed30e` capture)
    assert!(
        per_row_transient <= 40.0,
        "{per_row_transient:.1} B transient per added row"
    );
    // (perf-stream §9.2): the driver's row range is gone (PERF.STREAM-RAW); two text
    // `Arc<str>` per row stay until PERF.STRINGS
    assert!(
        per_row_net <= BLOCKS_PER_ROW,
        "{per_row_net:.3} blocks per row without the driver's per-batch blocks"
    );
    assert!(
        per_row_bytes <= BYTES_PER_ROW,
        "{per_row_bytes:.1} B requested per row"
    );
}

/// Blocks per fetched row without the driver's per-batch blocks: ≤ 2.18 (2.2 with
/// them until lesson, of which 0.021 per row were the driver's batches at idle; 2.021
/// measured with them counted). 3.034–3.043 with PERF.STREAM-CORE (perf-rows §12.4.1: one driver row range and two
/// text `Arc<str>`); the raw-row read removes the range.
const BLOCKS_PER_ROW: f64 = 2.18;
/// Requested bytes per fetched row: 357.0 B measured with the raw-row read plus 20 % (
/// 493–523 B with PERF.STREAM-CORE, the driver's read buffer varying between runs; 896.2 B before).
const BYTES_PER_ROW: f64 = 430.0;

fn fetched_columns_semantics() {
    let rt = runtime();
    rt.block_on(async {
        let h = bench_fixture::harness().await;
        bench_fixture::dataset(&h, LARGE / 10, 10, 100, 1).await;
        for n in [SMALL, LARGE] {
            let mut tx = h.begin().await.unwrap();
            tx.open().await.unwrap();
            let lines = tx.env().records("bench.order.line").unwrap();
            let opts = SearchOptions::new()
                .limit(n as u64)
                .order("id")
                .fetch_fields(FIELDS);
            let before = tx.query_count();
            let found = lines
                .search_with(&mut tx, &Domain::TRUE, &opts)
                .await
                .unwrap();
            assert_eq!(found.len(), n);
            assert_eq!(tx.query_count() - before, 1);
            tx.rollback().await.unwrap();
        }
        h.cleanup().await.unwrap();
    });
}
