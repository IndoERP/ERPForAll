//! A binary without the counting allocator gets an error from `measure`, not a count of zero.
//! No `#[global_allocator]` here on purpose.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_alloc_budget::{self as alloc, AllocError};

alloc::harness!(measure_without_the_global_allocator_is_an_error);

fn measure_without_the_global_allocator_is_an_error() {
    let outcome = alloc::measure(|| vec![0u8; 1024]);
    assert_eq!(
        outcome.map(|(s, _)| s),
        Err(AllocError::AllocatorNotInstalled)
    );
}
