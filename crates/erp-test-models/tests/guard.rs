//! guard tests for link-time registration with `linkme`.
//!
//! 1. Every crate here is built with `unsafe_code = "forbid"` (workspace lints) and `erp-registry` /
//!    `erp-orm` declare `linkme` slices that macro output fills: compiling at all is the check.
//! 2. The exact number of collected elements of the two-addon fixture (`guard_a`, `guard_b`). CI runs
//!    this under `dev`; `cargo test --release` and `--profile bench` (thin LTO) are run for the
//!    report.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_sdk::__private::{METHOD_IMPLS, MODEL_DECLS};
use erp_test_models::guard_a::{One, OneMethods};
use erp_test_models::guard_b::OneExtMethods;
use erp_test_models::{guard_a, guard_b};
use erp_testkit::Harness;

#[test]
fn adr_0005_guard_collected_element_counts() {
    let in_guard = |addon: &str| addon == "guard_a" || addon == "guard_b";
    let mut decls: Vec<_> = MODEL_DECLS
        .iter()
        .filter(|d| in_guard(d.addon))
        .map(|d| (d.addon, d.name))
        .collect();
    decls.sort_unstable();
    assert_eq!(
        decls,
        [
            ("guard_a", "guard.one"),
            ("guard_a", "guard.two"),
            ("guard_b", "guard.one")
        ]
    );
    let mut impls: Vec<_> = METHOD_IMPLS
        .iter()
        .filter(|i| in_guard(i.addon))
        .map(|i| (i.addon, i.method))
        .collect();
    impls.sort_unstable();
    assert_eq!(
        impls,
        [
            ("guard_a", "ping"),
            ("guard_b", "ping"),
            ("guard_b", "pong")
        ]
    );
    // core implementations are registered by the dispatch builder, not through the slice
    assert!(!METHOD_IMPLS.iter().any(|i| i.addon == "base"));
}

#[tokio::test]
async fn adr_0005_guard_dispatch_through_linked_entries() {
    let h = Harness::builder([&guard_a::ADDON, &guard_b::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let one = tx.env().model::<One>().unwrap();
    assert_eq!(one.ping(&mut tx).await.unwrap(), 2);
    assert_eq!(one.pong(&mut tx, 41).await.unwrap(), 41);
    drop(tx);
    h.cleanup().await.unwrap();
}
