//! Allocation and memory bound of a windowed read (`Records::stream`, design notes
//! , §3.6, §9.3 and `perf-stream-window.md` §2; standards §5b):
//! 5 columns of `bench.order.line` (two of them text) for 20,000 and 40,000 lines through a SQL
//! cursor, every window consumed and let go.
//!
//! What a whole-result read keeps grows with the rows (`search_with` retains 164.5 B per row in the
//! cache, `perf-stream.md` §2); a stream must not: each window's cache entries are truncated before
//! the next one and its buffers are reused, so
//! - the peak is the same at both sizes, within one window's bytes;
//! - nothing is left per row when the stream ends (end bytes equal);
//! - the blocks grow only by the interim per-row term (two text `Arc<str>` until PERF.STRINGS, and
//!   the driver's per-batch term, PERF.STREAM-RAW-BATCH) and a fixed term per window (the driver's
//!   unnamed `FETCH` statement and the read path's per-level allocations, see
//!   [`BLOCKS_PER_WINDOW`]); the per-window term is separated from the per-row one by a third run
//!   of 40,000 lines in windows twice as large.
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

use erp_orm::{Load, StreamOptions};
use erp_sdk::Domain;
use erp_testkit::alloc::{self, AllocStats};
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    a_stream_holds_one_window_whatever_the_result_size,
    windowed_read_semantics,
);

const FIELDS: [&str; 5] = ["code", "order_id", "product_name", "qty", "price_unit"];
const SMALL: usize = 20_000;
const LARGE: usize = 40_000;
const WINDOW: usize = 2_000;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// What one measured stream did.
struct Run {
    stats: AllocStats,
    statements: u64,
    rows: usize,
    windows: usize,
    renewed: u64,
    /// The driver's per-batch blocks: the batches it delivered for it, and how many parked.
    batches: DriverBlocks,
}

/// Blocks of `b` over `a` without the driver's per-batch blocks.
fn growth(a: &Run, b: &Run) -> f64 {
    b.stats.total_blocks as f64 - a.stats.total_blocks as f64 - b.batches.more_than(&a.batches)
}

/// The lines with an id up to `last`: a domain with a literal, so the `DECLARE` is planned per
/// call (no prepared statement to warm).
fn up_to(last: i64) -> Domain {
    Domain::from_json(&erp_sdk::serde_json::json!([["id", "<=", last]])).unwrap()
}

/// One measured stream of the first `n` lines (by id) in windows of `window`, in a fresh
/// transaction warmed by a small stream first.
fn measure(
    rt: &tokio::runtime::Runtime,
    h: &erp_testkit::Harness,
    ids: &[i64],
    n: usize,
    window: usize,
) -> Run {
    let mut tx = rt.block_on(h.begin()).unwrap();
    let lines = tx.env().records("bench.order.line").unwrap();
    let opts = StreamOptions::new()
        .fixed_window(window)
        .unwrap()
        .order("id")
        .load(Load::Raw);
    rt.block_on(async {
        tx.open().await.unwrap();
        let mut warm = lines
            .stream(&mut tx, &up_to(ids[10]), &FIELDS, &opts)
            .await
            .unwrap();
        while warm.next(&mut tx).await.unwrap().is_some() {}
        tx.invalidate_all().await.unwrap();
        // the warm stream's `CLOSE` rides this statement, not the measured stream's flight
        tx.query("SELECT 1", &[]).await.unwrap();
    });
    let domain = up_to(ids[n - 1]);
    let before = tx.query_count();
    let batches_before = tx.driver_response_batches().expect("connection");
    let (stats, outcome) = alloc::measure(|| {
        rt.block_on(async {
            let mut stream = lines.stream(&mut tx, &domain, &FIELDS, &opts).await?;
            let (mut rows, mut windows, mut total) = (0usize, 0usize, 0.0f64);
            while let Some(window) = stream.next(&mut tx).await? {
                windows += 1;
                rows += window.len();
                for i in window.indices() {
                    total += window.f64(i, "qty")?.unwrap_or(0.0);
                }
            }
            assert!(total > 0.0);
            Ok::<_, erp_sdk::Error>((rows, windows, stream.buffers_renewed()))
        })
    })
    .expect("measure");
    let (rows, windows, renewed) = outcome.unwrap();
    let statements = tx.query_count() - before;
    let batches = tx
        .driver_response_batches()
        .expect("connection")
        .since(batches_before)
        .expect("one connection: the counts only grow");
    let batches = DriverBlocks::new(rt.handle(), batches).expect("the driver's counts of a window");
    rt.block_on(tx.rollback()).unwrap();
    Run {
        stats,
        statements,
        rows,
        windows,
        renewed,
        batches,
    }
}

fn a_stream_holds_one_window_whatever_the_result_size() {
    let rt = runtime();
    let h = rt.block_on(bench_fixture::harness());
    rt.block_on(bench_fixture::dataset(&h, LARGE / 10, 10, 100, 1));
    let ids: Vec<i64> = rt.block_on(async {
        let mut tx = h.begin().await.unwrap();
        let found = tx
            .env()
            .records("bench.order.line")
            .unwrap()
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &erp_sdk::SearchOptions::new().order("id"),
            )
            .await
            .unwrap()
            .ids();
        tx.rollback().await.unwrap();
        found
    });
    assert_eq!(ids.len(), LARGE);
    let small = measure(&rt, &h, &ids, SMALL, WINDOW);
    let large = measure(&rt, &h, &ids, LARGE, WINDOW);
    let wide = measure(&rt, &h, &ids, LARGE, 2 * WINDOW);
    for (run, n, w) in [
        (&small, SMALL, WINDOW),
        (&large, LARGE, WINDOW),
        (&wide, LARGE, 2 * WINDOW),
    ] {
        assert_eq!(run.rows, n);
        assert_eq!(run.windows, n / w);
        // perf-stream §9.3: `DECLARE`, ⌊n / w⌋ + 1 `FETCH`es (the last one empty); the warm
        // stream sent the transaction's `SET LOCAL` (review DB 6: once per transaction)
        assert_eq!(run.statements, 2 + (n / w) as u64, "n = {n}, w = {w}");
        // the consumer keeps no window, so every window after the first reuses the buffers
        assert_eq!(run.renewed, 0, "n = {n}, w = {w}");
    }
    let windows = (large.windows - wide.windows) as f64;
    // How many batches the rows arrive in, and how many park the driver, depends on the host's
    // load, not on the stream (lesson): each growth is taken without the driver's
    // per-batch blocks, counted.
    let per_window_blocks = growth(&wide, &large) / windows;
    let rows = (LARGE - SMALL) as f64;
    let added_windows = (large.windows - small.windows) as f64;
    let per_row_blocks = (growth(&small, &large) - added_windows * per_window_blocks) / rows;
    let peak_growth = large.stats.peak_bytes as f64 - small.stats.peak_bytes as f64;
    let end_growth = large.stats.end_bytes as f64 - small.stats.end_bytes as f64;
    println!(
        "stream 5 columns, window {WINDOW}: {SMALL} lines {:?} in {}; {LARGE} lines {:?} in {}; \
         {LARGE} lines in windows of {} {:?} in {}; without the driver's per-batch blocks: per \
         row {per_row_blocks:.3} blocks, per window {per_window_blocks:.1} blocks; peak growth \
         {peak_growth:.0} B, end growth {end_growth:.0} B; statements {} / {} / {}",
        small.stats,
        small.batches,
        large.stats,
        large.batches,
        2 * WINDOW,
        wide.stats,
        wide.batches,
        small.statements,
        large.statements,
        wide.statements
    );
    // the peak does not grow with the result: within one window's bytes (est. ~350 B per row in
    // the window: cache, window batch, received bytes; perf-stream-window §2)
    assert!(
        peak_growth.abs() <= WINDOW_BYTES,
        "peak grew by {peak_growth:.0} B from {SMALL} to {LARGE} lines"
    );
    // nothing is left per row once the stream ends
    assert!(
        end_growth.abs() <= END_SLACK,
        "{end_growth:.0} B more left after {LARGE} lines than after {SMALL}"
    );
    assert!(
        per_row_blocks <= BLOCKS_PER_ROW,
        "{per_row_blocks:.3} blocks per row"
    );
    assert!(
        per_window_blocks <= BLOCKS_PER_WINDOW,
        "{per_window_blocks:.1} blocks per window"
    );
}

/// One window's bytes at [`WINDOW`] rows (est. 350 B per row, perf-stream-window §2).
const WINDOW_BYTES: f64 = (WINDOW * 350) as f64;
/// End bytes may differ by allocator-independent bookkeeping only (statement texts, the cursor
/// list), never by a per-row amount: 20,000 rows × 1 B would already exceed it.
const END_SLACK: f64 = 4_096.0;
/// Blocks per row without the driver's per-batch blocks: two text `Arc<str>` (PERF.STRINGS removes
/// them) and the driver's per-8 KiB blocks (read buffer and shared header, ~80 B per row here:
/// 0.02; PERF.STREAM-RAW-BATCH).
const BLOCKS_PER_ROW: f64 = 2.08;
/// None in the window's own buffers
/// (cache columns cut with their capacity kept, the batch recycled: `buffers_renewed` is 0) and
/// none per row; at 2,000 rows a window this is 0.03 blocks per row.
const BLOCKS_PER_WINDOW: f64 = 62.0;

fn windowed_read_semantics() {
    let rt = runtime();
    rt.block_on(async {
        let h = bench_fixture::harness().await;
        bench_fixture::dataset(&h, LARGE / 10, 10, 100, 1).await;
        let mut setup = h.begin().await.unwrap();
        let ids = setup
            .env()
            .records("bench.order.line")
            .unwrap()
            .search_with(
                &mut setup,
                &Domain::TRUE,
                &erp_sdk::SearchOptions::new().order("id"),
            )
            .await
            .unwrap()
            .ids();
        assert_eq!(ids.len(), LARGE);
        setup.rollback().await.unwrap();
        drop(setup);
        for (n, width) in [(SMALL, WINDOW), (LARGE, WINDOW), (LARGE, 2 * WINDOW)] {
            let mut tx = h.begin().await.unwrap();
            tx.open().await.unwrap();
            let lines = tx.env().records("bench.order.line").unwrap();
            let opts = StreamOptions::new()
                .fixed_window(width)
                .unwrap()
                .order("id")
                .load(Load::Raw);
            let before = tx.query_count();
            let mut stream = lines
                .stream(&mut tx, &up_to(ids[n - 1]), &FIELDS, &opts)
                .await
                .unwrap();
            let (mut rows, mut windows, mut total) = (0usize, 0usize, 0.0f64);
            while let Some(window) = stream.next(&mut tx).await.unwrap() {
                rows += window.len();
                windows += 1;
                for i in window.indices() {
                    total += window
                        .f64(i, "qty")
                        .unwrap()
                        .expect("fixture qty is present");
                }
            }
            assert!(total > 0.0);
            assert_eq!(rows, n);
            assert_eq!(windows, n / width);
            assert_eq!(stream.buffers_renewed(), 0);
            // Fresh transaction: SET LOCAL + DECLARE + data FETCHes + final empty FETCH.
            // Unlike the original measurement, no earlier stream consumed SET LOCAL.
            assert_eq!(tx.query_count() - before, 3 + (n / width) as u64);
            drop(stream);
            tx.rollback().await.unwrap();
        }
        h.cleanup().await.unwrap();
    });
}
