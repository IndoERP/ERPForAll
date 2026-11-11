//! Allocation regression for R2: the actual production parser must reject a long stored
//! update path without allocating a segment vector proportional to the input.
//! classification: an untrusted-input admission check (a refusal must not scale with
//! the input), not a product performance ratchet, so it stays in ordinary CI.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_addon_base::server_action::{MAX_PATH_STEPS, RunLimit, ServerActionError};
use erp_testkit::alloc::{self, Budget};

#[path = "../../../addons/base/src/server_action/path.rs"]
mod path;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    path_step_boundary,
    path_step_limit,
    path_step_far_over_limit
);

fn path_step_boundary() {
    let input = vec!["parent_id"; MAX_PATH_STEPS].join(".");
    let (stats, result) = alloc::measure(|| path::steps(&input).map(Iterator::count)).unwrap();
    assert_eq!(result.unwrap(), MAX_PATH_STEPS);
    println!("boundary: {stats:?}");
    stats.assert_within(&Budget::new(0, 0));
}

fn assert_limit(input: &str) {
    let (stats, result) = alloc::measure(|| path::steps(input).map(Iterator::count)).unwrap();
    let error = result.unwrap_err();
    assert_eq!(
        error,
        ServerActionError::Limit {
            kind: RunLimit::PathSteps
        }
    );
    assert_eq!(
        error.to_string(),
        "The server action has more than 16 steps in an update path; the run is refused."
    );
    println!("input_bytes={} {stats:?}", input.len());
    stats.assert_within(&Budget::new(0, 0));
}

fn path_step_limit() {
    assert_limit(&vec!["parent_id"; MAX_PATH_STEPS + 1].join("."));
}

fn path_step_far_over_limit() {
    // Input construction happens before the measured parser window.
    assert_limit(&".".repeat(1_000_000));
}
