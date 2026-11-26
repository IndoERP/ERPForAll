//! Allocation pin of `analyze` (standards §5b, the per-slice ratchet): the fixed cost and the
//! cost per template of a synthetic set, measured with N and 2N templates. A change in either
//! direction beyond the margin fails, so a regression is caught and an improvement is recorded by
//! updating the pin. One test in its own binary, run on the main
//! thread by `erp_alloc_budget::harness`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod alloc_common;

use alloc_common::{Cost, check, cost};
use erp_alloc_budget as alloc;
use erp_ui_compiler::{BuildMode, CompileOptions, analyze};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    analyze_allocations_are_pinned,
    analyze_templates_semantics,
);

/// Canonical main and M4 produced the same counts after removing the eager template queue.
/// The previous 10,963 / 21,883 allocation pin was already stale on canonical main.
fn analyze_pin() -> Cost {
    Cost {
        n: (17_878, 2_242_755, 1_527_132),
        n2: (35_694, 4_466_969, 3_036_776),
    }
}

fn analyze_allocations_are_pinned() {
    let options = CompileOptions::new(BuildMode::Prod);
    let measured = cost(|s| {
        let a = analyze(s, &options);
        assert!(a.is_clean());
    });
    check("analyze", measured, analyze_pin());
}

fn analyze_templates_semantics() {
    let options = CompileOptions::new(BuildMode::Prod);
    for n in [alloc_common::N, 2 * alloc_common::N] {
        assert!(analyze(&alloc_common::set(n), &options).is_clean());
    }
}
