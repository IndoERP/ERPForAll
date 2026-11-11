//! `measure` while a profiler started elsewhere runs gives an error, not a panic.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_alloc_budget::{self as alloc, AllocError};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(measure_next_to_a_foreign_profiler_is_an_error);

fn measure_next_to_a_foreign_profiler_is_an_error() {
    let _foreign = dhat::Profiler::builder().testing().build();
    let outcome = alloc::measure(|| vec![0u8; 16]);
    assert_eq!(
        outcome.map(|(s, _)| s),
        Err(AllocError::ProfilerAlreadyRunning)
    );
    // The foreign profiler is still intact and usable.
    let kept = std::hint::black_box(Box::new(7u64));
    let stats = dhat::HeapStats::get();
    assert!(stats.total_blocks > 0);
    drop(kept);
}
