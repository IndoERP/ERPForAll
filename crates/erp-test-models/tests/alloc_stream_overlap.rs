//! Presence before decode (perf-audit W9): a fetch
//! whose statement also returns cells already cached must not decode them. Text cells cost one
//! `Arc<str>` each when decoded, so a fetch of `code` and `product_name` for 20,000 lines where
//! `code` is already cached for half of them allocates one block less per skipped cell than the
//! same fetch with nothing cached in `code`. Both runs start with the same slots (the first half
//! of the lines cached in another field), so the slot map and the columns grow alike.
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

use erp_sdk::Records;
use erp_testkit::alloc::{self, AllocStats};
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    a_fetch_does_not_decode_cells_already_cached,
    overlapping_fetch_semantics,
);

const LINES: usize = 20_000;
const FETCHED: [&str; 2] = ["code", "product_name"];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Fetch [`FETCHED`] for every line after `cached` was fetched for the first half (cache dropped
/// first, prepared statements warm); the statements of the measured fetch.
fn measure(
    rt: &tokio::runtime::Runtime,
    h: &erp_testkit::Harness,
    ids: &[i64],
    cached: &str,
) -> (AllocStats, u64, DriverBlocks) {
    let mut tx = rt.block_on(h.begin()).unwrap();
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
            .fetch(&mut tx, Some(&FETCHED[..]))
            .await
            .unwrap();
        tx.invalidate_all().await.unwrap();
        lines(&tx, &ids[..ids.len() / 2])
            .fetch(&mut tx, Some(&[cached][..]))
            .await
            .unwrap();
    });
    let records = lines(&tx, ids);
    let before = tx.query_count();
    let batches_before = tx.driver_response_batches().expect("connection");
    let (stats, done) = alloc::measure(|| rt.block_on(records.fetch(&mut tx, Some(&FETCHED[..]))))
        .expect("measure");
    done.unwrap();
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

fn a_fetch_does_not_decode_cells_already_cached() {
    let rt = runtime();
    let h = rt.block_on(bench_fixture::harness());
    rt.block_on(bench_fixture::dataset(&h, LINES / 10, 10, 100, 1));
    let ids: Vec<i64> = rt.block_on(async {
        let mut tx = h.begin().await.unwrap();
        let rows = tx
            .query("SELECT id FROM bench_order_line ORDER BY id", &[])
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        rows.iter().map(|r| r.get::<_, i64>(0)).collect()
    });
    assert_eq!(ids.len(), LINES);
    // the same slots in both runs: the first half cached in a float field, or in `code`
    let (clean, clean_statements, clean_batches) = measure(&rt, &h, &ids, "qty");
    let (overlap, overlap_statements, overlap_batches) = measure(&rt, &h, &ids, "code");
    let skipped = (LINES / 2) as u64;
    // both runs receive the same rows, but not in the same number of batches, nor with the same
    // number parked, when the host is loaded: the driver's per-batch blocks the clean run has
    // more, counted, are taken out (lesson)
    let saved_blocks = clean.total_blocks as f64
        - overlap.total_blocks as f64
        - clean_batches.more_than(&overlap_batches);
    let saved_bytes = clean.total_bytes as i64 - overlap.total_bytes as i64;
    let bound = skipped as f64 * 0.95;
    println!(
        "fetch {FETCHED:?} of {LINES} lines: nothing cached in code {clean:?} in {clean_batches}; \
         code cached for {skipped} {overlap:?} in {overlap_batches}; saved {saved_blocks} blocks \
         without the driver's per-batch blocks, {saved_bytes} B; statements {clean_statements} / \
         {overlap_statements}"
    );
    // the same statements: `code` is still missing for half of the lines, so it is selected for
    // every line (two chunks of 10,000 ids)
    assert_eq!(clean_statements, overlap_statements);
    assert_eq!(overlap_statements, 2);
    // one `Arc<str>` per cached `code` cell is not made; decoding them and dropping the value
    // (the behaviour before PERF.STREAM-CORE) saves nothing. The 5 % slack covers column growth
    // steps that differ between the two runs (a handful of blocks).
    assert!(
        saved_blocks >= bound,
        "{saved_blocks} blocks saved for {skipped} cached cells"
    );
}

fn overlapping_fetch_semantics() {
    let rt = runtime();
    rt.block_on(async {
        let h = bench_fixture::harness().await;
        bench_fixture::dataset(&h, LINES / 10, 10, 100, 1).await;
        let mut setup = h.begin().await.unwrap();
        let ids: Vec<i64> = setup
            .query("SELECT id FROM bench_order_line ORDER BY id", &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(ids.len(), LINES);
        setup.rollback().await.unwrap();
        drop(setup);
        for cached in ["qty", "code"] {
            let mut tx = h.begin().await.unwrap();
            tx.open().await.unwrap();
            let lines = tx.env().records("bench.order.line").unwrap();
            lines
                .browse(ids[..ids.len() / 2].iter().copied())
                .unwrap()
                .fetch(&mut tx, Some(&[cached][..]))
                .await
                .unwrap();
            let records = lines.browse(ids.iter().copied()).unwrap();
            let before = tx.query_count();
            records.fetch(&mut tx, Some(&FETCHED[..])).await.unwrap();
            assert_eq!(tx.query_count() - before, 2);
            tx.rollback().await.unwrap();
        }
        h.cleanup().await.unwrap();
    });
}
