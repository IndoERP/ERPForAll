//! Allocation budget of `web_read` with the BENCH-002 specification (
//! §12; standards §5b): 1,000 and 2,000 orders of 10 lines and two
//! of 50 tags each, read in a transaction whose cache was dropped, as BENCH-002 reads. The
//! difference of the two runs is the cost of 10,000 lines (and 1,000 orders); it must stay
//! within the per-line budget, so a map or a copy per row coming back fails here. The statements
//! are the same for both sizes and pinned.
//!
//! One test in its own binary, run on the main thread by `erp_alloc_budget::harness`
//! (`erp_alloc_budget`: one profiler, every thread counted). The runtime is current-thread and
//! built outside the measured closure, so only the read's own allocations (and its connection's)
//! count.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

mod bench_fixture;

use erp_testkit::alloc::{self, AllocStats, Budget};
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    web_read_bench_002_allocates_within_the_per_line_budget,
    web_read_semantics,
);

const LINES: usize = 10;
const SMALL: usize = 1_000;
const LARGE: usize = 2_000;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// One measured `web_read` of `ids` (cache dropped first, prepared statements warm), with the
/// statements it sent.
fn measure(
    rt: &tokio::runtime::Runtime,
    h: &erp_testkit::Harness,
    ids: &[i64],
) -> (AllocStats, u64, DriverBlocks) {
    let mut tx = rt.block_on(h.begin_as(2)).unwrap();
    rt.block_on(async {
        tx.open().await.unwrap();
        bench_fixture::orders(&tx, &ids[..1])
            .web_read(&mut tx, bench_fixture::spec_002())
            .await
            .unwrap();
        tx.invalidate_all().await.unwrap();
    });
    let records = bench_fixture::orders(&tx, ids);
    let spec = bench_fixture::spec_002();
    let before = tx.query_count();
    let batches_before = tx.driver_response_batches().expect("connection");
    let (stats, rows) =
        alloc::measure(|| rt.block_on(records.web_read(&mut tx, spec))).expect("measure");
    let rows = rows.unwrap();
    assert_eq!(rows.len(), ids.len());
    let statements = tx.query_count() - before;
    let batches = tx
        .driver_response_batches()
        .expect("connection")
        .since(batches_before)
        .expect("one connection: the counts only grow");
    let batches = DriverBlocks::new(rt.handle(), batches).expect("the driver's counts of a window");
    drop(rows);
    rt.block_on(tx.rollback()).unwrap();
    (stats, statements, batches)
}

fn web_read_bench_002_allocates_within_the_per_line_budget() {
    let rt = runtime();
    let h = rt.block_on(bench_fixture::harness());
    let ids = rt.block_on(bench_fixture::dataset(&h, LARGE, LINES, 100, 50));
    let (small, small_statements, small_batches) = measure(&rt, &h, &ids[..SMALL]);
    let (large, large_statements, large_batches) = measure(&rt, &h, &ids);
    let lines = ((LARGE - SMALL) * LINES) as u64;
    let per_line_blocks = (large.total_blocks - small.total_blocks) as f64 / lines as f64;
    // the driver's per-batch blocks of the added batches, counted: how many batches the rows
    // arrive in, and how many park the driver, depends on the host's load (lesson)
    let per_line_net = per_line_blocks - large_batches.more_than(&small_batches) / lines as f64;
    let per_line_bytes = (large.total_bytes - small.total_bytes) as f64 / lines as f64;
    let per_line_retained = (large.end_bytes as f64 - small.end_bytes as f64) / lines as f64;
    let transient = |s: &AllocStats| s.peak_bytes as f64 - s.end_bytes as f64;
    let per_line_transient = (transient(&large) - transient(&small)) / lines as f64;
    println!(
        "web_read BENCH-002: {SMALL} orders {small:?} in {small_batches}; {LARGE} orders \
         {large:?} in {large_batches}; per line {per_line_blocks:.3} blocks \
         ({per_line_net:.3} without the driver's per-batch blocks), \
         {per_line_bytes:.1} B \
         requested, {per_line_retained:.1} B retained, {per_line_transient:.1} B transient; \
         statements {small_statements} / {large_statements}"
    );
    // perf-stream §2, §9.1: rows are decoded as they arrive, so nothing of the fetch is live at
    // the peak beyond the cache (490.6 B per line while the driver's rows were collected). The
    // 47.4 B per line measured with PERF.STREAM-CORE is the record layer's scratch at the peak
    // (dhat at the global peak, 2k orders): the read's settle scope (a seen-set and an id vector
    // over the 20k lines, ~29 B), `Records::related`'s two id `Arc`s for the lines (16 B) and the
    // read's slot vector (4 B), owned by PERF.READ-002. Budget: the measurement plus ~25 %.
    assert!(
        per_line_transient <= 60.0,
        "{per_line_transient:.1} B transient per line"
    );
    // the same statements whatever the size (one chunk of orders)
    assert_eq!(small_statements, large_statements);
    assert_eq!(large_statements, STATEMENTS);
    // (perf-stream §9.2): per line ≤ 2.56 blocks without the driver's per-batch
    // blocks (2.6 with them until lesson, of which 0.044 per line were the driver's batches
    // at idle; 2.548 measured with them counted; 3.88 with PERF.STREAM-CORE, of which
    // 1.3 were the driver's row ranges, one per fetched row; what stays: two `Arc<str>` per line
    // until PERF.STRINGS and the per-owner x2many value until PERF.READ-002); ≤ 2,750 B requested (2,275–2,291 B measured
    // with PERF.STREAM-CORE, perf-rows §12.4.1, plus ~20 %; 2,797 B while the driver's rows were
    // collected) and ≤ 440 B
    // retained (the cache included: the transaction still holds it), over the §10 ceilings of
    // 1,200 B and 350 B (driver row ranges, text values and cache columns: PERF.STREAM-RAW,
    // PERF.STRINGS); the fixed part (statements, layouts, flights) ≤ 2,000 blocks. `d864e9c6`'s
    // maps: 33.3 blocks, 5,709 B, 1,206 B retained per line.
    assert!(
        per_line_net <= 2.56,
        "{per_line_net:.3} blocks per line without the driver's per-batch blocks"
    );
    assert!(per_line_bytes <= 2_750.0, "{per_line_bytes:.1} B per line");
    assert!(
        per_line_retained <= 440.0,
        "{per_line_retained:.1} B retained per line"
    );
    let budget = Budget::per_row(
        (SMALL * LINES) as u64,
        (4, 2_750),
        Budget::new(2_000, 1 << 20),
    )
    .expect("budget");
    small.assert_within(&budget);
}

/// Statements of the BENCH-002 `web_read` of one chunk of orders: orders, their lines, the tag
/// links, then partners and tags in one flight (measured on `d864e9c6`, unchanged since).
const STATEMENTS: u64 = 5;

fn web_read_semantics() {
    let rt = runtime();
    rt.block_on(async {
        let h = bench_fixture::harness().await;
        let ids = bench_fixture::dataset(&h, LARGE, LINES, 100, 50).await;
        for n in [SMALL, LARGE] {
            let mut tx = h.begin_as(2).await.unwrap();
            tx.open().await.unwrap();
            let records = bench_fixture::orders(&tx, &ids[..n]);
            let before = tx.query_count();
            let rows = records
                .web_read(&mut tx, bench_fixture::spec_002())
                .await
                .unwrap();
            assert_eq!(rows.len(), n);
            assert_eq!(tx.query_count() - before, STATEMENTS);
            tx.rollback().await.unwrap();
        }
        h.cleanup().await.unwrap();
    });
}
