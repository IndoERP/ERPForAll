//! Heap ratchet for the new pure locale formatter; no record cache or served RPC is claimed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "measurement test"
)]

use erp_testkit::alloc;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    locale_format_allocations,
    locale_format_semantics,
);

// Fixed positive cents and international grouping: a deliberately narrow handwritten floor.
fn floor(cents: u64) -> String {
    let integer = cents / 100;
    format!(
        "{},{:03},{:03}.{:02}",
        integer / 1_000_000,
        (integer / 1_000) % 1_000,
        integer % 1_000,
        cents % 100
    )
}

fn locale_format_allocations() {
    const ROWS: u32 = 10_000;
    let (actual_stats, outputs) = alloc::measure(|| {
        (0..ROWS)
            .map(|offset| {
                let value = 123_456_789.5 + f64::from(offset);
                erp_addon_base::format_float(value, 2, "[3,0]", ",", ".").unwrap()
            })
            .collect::<Vec<_>>()
    })
    .unwrap();
    let (floor_stats, floor_outputs) = alloc::measure(|| {
        (0..ROWS)
            .map(|offset| floor(12_345_678_950 + u64::from(offset) * 100))
            .collect::<Vec<_>>()
    })
    .unwrap();
    assert_eq!(outputs, floor_outputs);
    println!("rows={ROWS} actual={actual_stats:?} floor={floor_stats:?}");
    assert!(actual_stats.total_blocks <= 110_001, "{actual_stats:?}");
    assert!(actual_stats.total_bytes <= 2_290_000, "{actual_stats:?}");
    assert!(actual_stats.peak_bytes <= 460_144, "{actual_stats:?}");
}

fn locale_format_semantics() {
    // Every original input remains checked, without profiling either formatter.
    for offset in 0..10_000u32 {
        let value = 123_456_789.5 + f64::from(offset);
        let integer = 123_456_789 + u64::from(offset);
        let expected = format!(
            "{},{:03},{:03}.50",
            integer / 1_000_000,
            (integer / 1_000) % 1_000,
            integer % 1_000
        );
        assert_eq!(
            erp_addon_base::format_float(value, 2, "[3,0]", ",", ".").unwrap(),
            expected
        );
    }
}
