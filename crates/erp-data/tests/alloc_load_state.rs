//! Allocation pin of the load-state context values (`erp_data::load::load_state`, review A-R2 of
//! ): an upgrade whose `ir.ui.view` windows alternate with windows of another model, so
//! every view window sees one more loaded name. Rebuilding the list per window allocated
//! `O(n²)` bytes; the shared chunks allocate `O(n log n)`. A ratchet: the budget is the value
//! measured when the fix merged and may only go down. One test in its own binary, run on the main
//! thread by `erp_alloc_budget::harness`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "tests"
)]

use std::sync::Arc;

use erp_alloc_budget::{self as alloc, Budget};
use erp_data::load::{LoadSession, NoFiles, SessionOptions};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    interleaved_windows_stay_within_the_pin,
    interleaved_windows_preserve_loaded_names,
);

/// The
/// list rebuilt per window allocated about one block per loaded name per window: some 2,000,000
/// blocks here.
const PIN: Budget = Budget::new(PIN_BLOCKS, PIN_BYTES);
const PIN_BLOCKS: u64 = 30_018;
const PIN_BYTES: u64 = 2_440_026;

/// Windows of the measured upgrade.
const WINDOWS: usize = 2_000;

fn session() -> LoadSession {
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    session.begin_operation(["m", "later"]).unwrap();
    session
}

/// One loaded name and the values of the next view window, `names.len()` times.
fn run(session: &mut LoadSession, names: &[String]) -> usize {
    let mut chunks = 0;
    for name in names {
        let (pending, loaded) = session.load_state_after_for_tests("m", name).unwrap();
        drop((pending, loaded));
        chunks = chunks.max(session.load_state_chunks_for_tests());
    }
    chunks
}

fn interleaved_windows_stay_within_the_pin() {
    let names: Vec<String> = (0..WINDOWS).map(|i| format!("view_{i:05}")).collect();
    // warm: lazily initialised statics are not part of the pin
    let mut warm = session();
    run(&mut warm, &names[..10]);
    drop(warm);
    let mut session = session();
    let (stats, chunks) = alloc::measure(|| run(&mut session, &names)).expect("measure");
    // log2(2000) + 1
    assert!(chunks <= 11, "{chunks} chunks");
    println!("load state over {WINDOWS} interleaved windows: {stats:?}");
    stats.assert_within(&PIN);
}

fn interleaved_windows_preserve_loaded_names() {
    let names: Vec<String> = (0..WINDOWS).map(|i| format!("view_{i:05}")).collect();
    let mut session = session();
    run(&mut session, &names);
    let mut loaded = session.loaded_names("m");
    loaded.sort_unstable();
    assert_eq!(loaded, names.iter().map(String::as_str).collect::<Vec<_>>());
}
