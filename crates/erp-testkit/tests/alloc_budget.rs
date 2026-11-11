//! Allocation budget of a small real operation: parsing a JSON domain (`erp_domain::Domain`). One
//! test in its own binary, run on the main thread by `erp_alloc_budget::harness` (module docs of
//! `erp_testkit::alloc`: one profiler, all threads counted).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_testkit::alloc::{self, AllocStats, Budget};
use serde_json::json;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    parsing_a_domain_stays_within_its_allocation_budget,
    parsing_domains_succeeds,
);

fn domain_of(leaves: usize) -> serde_json::Value {
    let mut items: Vec<serde_json::Value> = Vec::new();
    for _ in 0..leaves {
        items.push(json!(["name", "=", "x"]));
    }
    serde_json::Value::Array(items)
}

fn parse(json: &serde_json::Value) -> AllocStats {
    let (stats, domain) = alloc::measure(|| erp_domain::Domain::from_json(json)).expect("measure");
    domain.expect("domain");
    stats
}

/// The allocation budget of building a domain: a fixed part and a part per condition, the way a
/// bulk path states its budget per row.
fn parsing_a_domain_stays_within_its_allocation_budget() {
    let small = parse(&domain_of(1));
    let ten = parse(&domain_of(10));
    let twenty = parse(&domain_of(20));
    // The cost grows with the number of conditions, so the counter sees the work.
    assert!(
        small.total_blocks < ten.total_blocks && ten.total_blocks < twenty.total_blocks,
        "{small:?} {ten:?} {twenty:?}"
    );
    // Marginal cost of one condition, from two measurements (the fixed part cancels out).
    let per_condition_blocks = (twenty.total_blocks - ten.total_blocks) / 10;
    let per_condition_bytes = (twenty.total_bytes - ten.total_bytes) / 10;
    assert!(
        per_condition_blocks <= 8,
        "{per_condition_blocks} blocks per condition"
    );
    assert!(
        per_condition_bytes <= 1_100,
        "{per_condition_bytes} bytes per condition"
    );
    // The whole domain, as a budget stated per condition plus a fixed part (measured: 153 blocks,
    // 20,452 bytes for 20 conditions).
    let budget = Budget::per_row(20, (8, 1_100), Budget::new(8, 128)).expect("budget");
    twenty.assert_within(&budget);
    // The budget can fail: a tighter one is refused with the measured figures.
    let tight = Budget::per_row(20, (5, 800), Budget::new(8, 128)).expect("budget");
    let over = twenty
        .check(&tight)
        .expect_err("a budget below the measured cost must fail");
    assert_eq!(over.stats, twenty);
    assert_eq!(over.budget, tight);
}

fn parsing_domains_succeeds() {
    for leaves in [1, 10, 20] {
        let input = domain_of(leaves);
        let domain = erp_domain::Domain::from_json(&input).expect("domain");
        let serialized = domain.to_json().expect("domain JSON");
        let reparsed = erp_domain::Domain::from_json(&serialized).expect("roundtrip domain");
        assert_eq!(reparsed, domain);
    }
}
