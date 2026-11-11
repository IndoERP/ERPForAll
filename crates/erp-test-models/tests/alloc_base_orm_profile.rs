//! Ignored explanatory profile of the existing BASE assignment ratchet, not a timing test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "explicit diagnostic probe"
)]

mod base_orm_cost;

use base_orm_cost::{Case, Fixture};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

erp_testkit::alloc::harness!(
    #[ignore = "cross-platform allocation attribution; run explicitly with --ignored"]
    profile_company_assignment
);

fn profile_company_assignment() {
    let directory = std::path::PathBuf::from(
        std::env::var_os("ERP_BASE_PROFILE_DIR").expect("explicit profile directory required"),
    );
    assert!(directory.is_dir(), "profile directory must already exist");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fixture = rt.block_on(Fixture::new(2)).unwrap();
    let case = Case::AssignInactive;
    let floor = false;
    let mut warm = rt.block_on(fixture.harness.begin()).unwrap();
    rt.block_on(warm.open()).unwrap();
    let id = rt
        .block_on(async {
            if floor {
                fixture.floor(&mut warm, case).await
            } else {
                fixture.orm(&mut warm, case).await
            }
        })
        .unwrap();
    rt.block_on(fixture.verify(&mut warm, case, id)).unwrap();
    rt.block_on(warm.rollback()).unwrap();

    for repeat in 0..3 {
        let output = directory.join(format!("assignment-{repeat}.json"));
        assert!(!output.exists(), "refusing to overwrite a retained profile");
        let mut tx = rt.block_on(fixture.harness.begin()).unwrap();
        rt.block_on(tx.open()).unwrap();
        let queries = tx.query_count();
        // Same complete operation+flush window as alloc_base_orm; backtraces enabled.
        // No alloc::measure nesting: this binary owns its profiler directly.
        let profiler = dhat::Profiler::builder().file_name(&output).build();
        let id = rt
            .block_on(async {
                if floor {
                    fixture.floor(&mut tx, case).await
                } else {
                    fixture.orm(&mut tx, case).await
                }
            })
            .unwrap();
        let stats = dhat::HeapStats::get();
        drop(profiler);
        let queries = tx.query_count().checked_sub(queries).unwrap();
        assert_eq!(queries, 2, "assignment query count");
        println!(
            "{}",
            serde_json::json!({
                "case": case.name(), "repeat": repeat, "profile": output,
                "query_count": queries, "total_blocks": stats.total_blocks,
                "total_bytes": stats.total_bytes, "peak_live_bytes": stats.max_bytes,
                "retained_blocks": stats.curr_blocks, "retained_bytes": stats.curr_bytes,
                "window": "complete operation+flush; setup/open/verification/rollback excluded; one untimed warmup",
            })
        );
        rt.block_on(fixture.verify(&mut tx, case, id)).unwrap();
        rt.block_on(tx.rollback()).unwrap();
    }
    rt.block_on(fixture.harness.cleanup()).unwrap();
}
