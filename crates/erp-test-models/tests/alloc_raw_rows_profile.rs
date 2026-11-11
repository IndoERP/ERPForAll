//! Heap profile of `alloc_raw_rows`'s fetch (probe, not a check): the same
//! fixed-width search of 40,000 `bench.order.line` rows under a whole-process dhat profiler with
//! backtraces, written to `$ERP_PROFILE_OUT` (default `dhat-raw-rows.json` in the working
//! directory). "Per-batch term" attributes it; rerun after a
//! driver or ORM change to the fetch path:
//!
//! `cargo nextest run -p erp-test-models --test alloc_raw_rows_profile --run-ignored only`
//!
//! Its own binary (dhat allows one profiler per process), no other test in it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "tests"
)]

mod bench_fixture;

use erp_sdk::{Domain, SearchOptions};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

erp_testkit::alloc::harness!(
    #[ignore = "heap profile probe; run explicitly with --run-ignored only"]
    profile_fixed_width_fetch
);

const FIELDS: [&str; 4] = ["order_id", "qty", "price_unit", "price_subtotal"];
const ROWS: usize = 40_000;

fn profile_fixed_width_fetch() {
    let out = std::env::var("ERP_PROFILE_OUT").unwrap_or_else(|_| "dhat-raw-rows.json".into());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(bench_fixture::harness());
    rt.block_on(bench_fixture::dataset(&h, ROWS / 10, 10, 100, 1));
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
    let opts = opts(ROWS);
    let profiler = dhat::Profiler::builder().file_name(&out).build();
    let found = rt
        .block_on(lines.search_with(&mut tx, &Domain::TRUE, &opts))
        .unwrap();
    let stats = dhat::HeapStats::get();
    drop(profiler);
    assert_eq!(found.len(), ROWS);
    println!("profile of {ROWS} rows written to {out}: {stats:?}");
    rt.block_on(tx.rollback()).unwrap();
}
