//! The driver's allocations per fetched row (and D7, perf-stream §9.2; G6
//! item 5): a search of `bench.order.line` fetching only fixed-width columns (`id`, `order_id`,
//! `qty`, `price_unit`, `price_subtotal`: int8 and float8, no text), for 20,000 and 40,000 lines,
//! one statement each, cache dropped first. No value of these columns owns heap memory and the
//! cache columns are reserved for the known row count, so what the added rows allocate is the
//! fetch path's own cost per row: the driver's range vector (`Row::new`) before the raw-row read,
//! nothing after it, plus what the driver allocates per received batch, which grows with the
//! received bytes, not the rows ("Context", lead decision 1).
//!
//! The driver's terms, every part named from a dhat profile of the 40k run
//! (`alloc_raw_rows_profile.rs`): per received batch, the
//! futures-channel mpsc node carrying it to the stream (1) and the parked-sender node of the
//! response channel (`channel(1)`) when the connection task finds it full (0 or 1, about every
//! second batch); per 8 KiB received, the new read buffer (1) and the `Bytes` shared header when a
//! batch is first split off it (1). On an idle host a batch is 8 KiB, and the lead decision on ADR
//! 0055 (2026-10-20) bounded the sum at 4 blocks per 8 KiB received (3.56 measured).
//! So each
//! run is now measured against the batches the driver counted, delivered and parked
//! (`Transaction::driver_response_batches`, [`erp_testkit::driver::DriverBlocks`]):
//!
//! - D7, per run: what is left after 2 blocks per 8 KiB received and the per-batch blocks is the
//!   fixed part of a search: within [`FIXED_BLOCKS_LEAST`]..=[`FIXED_BLOCKS`] at either size. One
//!   more block per batch, or one per 300 rows, breaks the upper side; a driver that no longer
//!   allocates its read buffers breaks the lower one.
//! - D5, between the runs: ≤ [`PER_ROW_BLOCKS`] per added row once the driver's terms are taken
//!   out (one per row before the raw-row read: the row term is 0), the two runs' per-batch blocks
//!   each counted, since they need not be batched alike.
//!
//! The term is an open G6 item (follow-up PERF.STREAM-RAW-BATCH). Printed: both, the blocks per
//! batch of each run, and the blocks per received KiB that the lead decision asks to report.
//!
//! One test in its own binary, run on the main thread by `erp_alloc_budget::harness` (one profiler,
//! every thread counted). The runtime is current-thread and built outside the measured closure.
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
    fixed_width_fetch_allocates_nothing_per_row_in_the_driver,
    fixed_width_fetch_semantics,
);

const FIELDS: [&str; 4] = ["order_id", "qty", "price_unit", "price_subtotal"];
const SMALL: usize = 20_000;
const LARGE: usize = 40_000;
/// Bytes of one `DataRow` message of these rows: tag 1, length 4, field count 2, and per field a
/// 4-byte length and 8 bytes of value (5 fields, none NULL: checked below).
const WIRE_BYTES_PER_ROW: f64 = (1 + 4 + 2 + 5 * (4 + 8)) as f64;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// What one measured search did.
struct Run {
    stats: AllocStats,
    statements: u64,
    /// The driver's per-batch blocks: the batches it delivered for it, and how many parked.
    batches: DriverBlocks,
}

/// One measured search of the first `n` lines with the fixed-width columns fetched (cache dropped
/// first, prepared statements warm), with the statements it sent and the batches it was read in.
fn measure(rt: &tokio::runtime::Runtime, h: &erp_testkit::Harness, n: usize) -> Run {
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
    Run {
        stats,
        statements,
        batches,
    }
}

fn fixed_width_fetch_allocates_nothing_per_row_in_the_driver() {
    let rt = runtime();
    let h = rt.block_on(bench_fixture::harness());
    rt.block_on(bench_fixture::dataset(&h, LARGE / 10, 10, 100, 1));
    // the wire size above holds only without NULLs
    let nulls: i64 = rt.block_on(async {
        let mut tx = h.begin().await.unwrap();
        let n = tx
            .query(
                "SELECT count(*) FROM bench_order_line WHERE order_id IS NULL OR qty IS NULL \
                 OR price_unit IS NULL OR price_subtotal IS NULL",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.rollback().await.unwrap();
        n
    });
    assert_eq!(nulls, 0);
    let small = measure(&rt, &h, SMALL);
    let large = measure(&rt, &h, LARGE);
    let rows = (LARGE - SMALL) as f64;
    let blocks = large.stats.total_blocks as f64 - small.stats.total_blocks as f64;
    let received = rows * WIRE_BYTES_PER_ROW;
    let per_row_blocks = blocks / rows;
    let per_kib = blocks / (received / 1024.0);
    let per_8_kib = blocks / (received / 8192.0);
    let per_batch = |run: &Run| run.stats.total_blocks as f64 / run.batches.delivered() as f64;
    // D7 per run: what is left after the per-8 KiB blocks and the per-batch blocks
    let rest = |run: &Run, n: usize| {
        run.stats.total_blocks as f64 - 2.0 * read_buffers(n) - run.batches.blocks()
    };
    let (rest_small, rest_large) = (rest(&small, SMALL), rest(&large, LARGE));
    // D5 between the runs: the per-batch blocks each run's counts give
    let per_row_net = (blocks
        - large.batches.more_than(&small.batches)
        - 2.0 * (read_buffers(LARGE) - read_buffers(SMALL)))
        / rows;
    println!(
        "fixed-width fetch: {SMALL} lines {:?} in {} ({:.3} blocks per batch, {rest_small:.0} left \
         after the driver's terms); {LARGE} lines {:?} in {} ({:.3} per batch, {rest_large:.0} \
         left); added {rows} rows, {received} B received: {per_row_blocks:.4} blocks per row \
         ({per_row_net:.4} without the driver's terms), {per_kib:.4} blocks per received KiB \
         ({per_8_kib:.3} per 8 KiB); statements {} / {}",
        small.stats,
        small.batches,
        per_batch(&small),
        large.stats,
        large.batches,
        per_batch(&large),
        small.statements,
        large.statements
    );
    // one statement whatever the size
    assert_eq!(small.statements, 1);
    assert_eq!(large.statements, 1);
    // as decided, per batch counted: mpsc node 1, parked-sender node 1 when the
    // delivery parked; per 8 KiB received: read buffer 1, `Bytes` shared header 1
    for (rest, n) in [(rest_small, SMALL), (rest_large, LARGE)] {
        assert!(
            (FIXED_BLOCKS_LEAST..=FIXED_BLOCKS).contains(&rest),
            "{rest:.0} blocks left at {n} lines after the driver's per-batch and per-8 KiB \
             blocks: outside {FIXED_BLOCKS_LEAST}..={FIXED_BLOCKS}"
        );
    }
    // the row term is 0 (before the raw read: one range vector per row)
    assert!(
        per_row_net <= PER_ROW_BLOCKS,
        "{per_row_net:.4} blocks per added row without the driver's terms"
    );
}

/// Read buffers of 8 KiB the driver fills to receive `n` rows.
fn read_buffers(n: usize) -> f64 {
    (n as f64 * WIRE_BYTES_PER_ROW / 8192.0).ceil()
}

/// One more block per 2,000 rows breaks it at 40,000 lines.
const FIXED_BLOCKS: f64 = 110.0;
/// The least a run may leave (D7, lower side; review rust-2): a driver that stopped allocating
/// its read buffers (or a model that took out more than the driver allocates) leaves far less,
/// about −240 and −230.
const FIXED_BLOCKS_LEAST: f64 = 80.0;
const PER_ROW_BLOCKS: f64 = 0.002;

fn fixed_width_fetch_semantics() {
    let rt = runtime();
    rt.block_on(async {
        let h = bench_fixture::harness().await;
        bench_fixture::dataset(&h, LARGE / 10, 10, 100, 1).await;
        let mut setup = h.begin().await.unwrap();
        let nulls: i64 = setup
            .query(
                "SELECT count(*) FROM bench_order_line WHERE order_id IS NULL OR qty IS NULL \
             OR price_unit IS NULL OR price_subtotal IS NULL",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(nulls, 0);
        setup.rollback().await.unwrap();
        drop(setup);
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
