//! Measurement of copy_data allocations and its narrow SQL read floor (standards §5b).
//! The floor reads the same source columns; it does not perform access checks, build copy
//! dictionaries, traverse relations, or create records, and is not full copy parity.
//! The exact driver-adjusted block ceiling is pinned to the host-stamped initial measurement.
//! Timing runs separately from the allocation profiler (dhat backtraces distort latency).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    reason = "tests"
)]

use std::time::Instant;

use erp_fields::Value;
use erp_registry::{FieldDecl, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::alloc;
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    copy_data_allocation_measurement,
    copy_data_semantics,
);

const ROWS: usize = 1_000;
static ADDON: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_copy_alloc",
    depends: &["base"],
    exact_only: false,
};
const FLOOR: &str =
    "SELECT id, name, code FROM test_copy_alloc_item WHERE id = ANY($1) ORDER BY id";

/// Separate observed ceilings: Linux's focused evidence and macOS Actions run 39986040595.
/// The one-block platform difference has not been attributed to a particular allocation.
/// Unknown platforms must measure and pin their own ceiling before this test can pass.
fn measured_block_ceiling() -> f64 {
    let measured = if cfg!(target_os = "linux") {
        Some(7_255.0)
    } else if cfg!(target_os = "macos") {
        Some(7_256.0)
    } else {
        None
    };
    measured.unwrap_or_else(|| {
        panic!(
            "no measured copy_data block ceiling for {}: measure alloc_copy here and pin it",
            std::env::consts::OS
        )
    })
}

fn copy_data_allocation_measurement() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(async {
        Harness::builder([&base_users::ADDON, &ADDON])
            .with_decls([ModelDecl::new("test_copy_alloc", "test_copy_alloc.item")
                .log_access(false)
                .fields(const { &[FieldDecl::char("name"), FieldDecl::char("code")] })])
            .build()
            .await
            .unwrap()
    });
    let mut tx = rt.block_on(h.begin()).unwrap();
    rt.block_on(tx.batch("INSERT INTO test_copy_alloc_item (name, code) SELECT 'Name ' || g, 'Code ' || g FROM generate_series(1, 1000) AS g"))
        .unwrap();
    let ids: Vec<i64> = rt
        .block_on(tx.query("SELECT id FROM test_copy_alloc_item ORDER BY id", &[]))
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(ids.len(), ROWS);
    let records = tx
        .env()
        .records("test_copy_alloc.item")
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap();
    // Warm dispatch, statement plans, and connection buffers; invalidate source cells so the
    // measured operation still pays its own reads. Both timed paths use the same transaction.
    let warm = rt.block_on(records.copy_data(&mut tx, None)).unwrap();
    assert_eq!(warm.len(), ROWS);
    drop(warm);
    let warm_floor = rt.block_on(tx.query(FLOOR, &[&ids])).unwrap();
    assert_eq!(warm_floor.len(), ROWS);
    drop(warm_floor);
    rt.block_on(tx.invalidate_all()).unwrap();

    let queries_before = tx.query_count();
    let batches_before = tx.driver_response_batches().unwrap();
    let (stats, copied) = alloc::measure(|| rt.block_on(records.copy_data(&mut tx, None))).unwrap();
    let copied = copied.unwrap();
    let copy_queries = tx.query_count() - queries_before;
    let batches = DriverBlocks::new(
        rt.handle(),
        tx.driver_response_batches()
            .unwrap()
            .since(batches_before)
            .unwrap(),
    )
    .unwrap();
    let net_blocks = stats.total_blocks as f64 - batches.blocks();
    assert_eq!(copied.len(), ROWS);
    for (index, result) in copied.iter().enumerate() {
        let values = result.as_ref().expect("every source record is copied");
        assert_eq!(
            values.get("name"),
            Some(&Value::str(format!("Name {}", index + 1)))
        );
        assert_eq!(
            values.get("code"),
            Some(&Value::str(format!("Code {}", index + 1)))
        );
        assert_eq!(values.len(), 2);
    }
    println!(
        "copy_data rows={ROWS} stats={stats:?} driver={batches} net_blocks={net_blocks} queries={copy_queries}"
    );
    // Linux raw evidence:
    let block_ceiling = measured_block_ceiling();
    assert!(
        net_blocks <= block_ceiling,
        "copy_data allocated {net_blocks} non-driver blocks (ceiling {block_ceiling}): {stats:?}"
    );
    assert!(
        copy_queries <= 1,
        "copy_data used {copy_queries} statements"
    );
    drop(copied);

    rt.block_on(tx.invalidate_all()).unwrap();
    let start = Instant::now();
    let timed_copy = rt.block_on(records.copy_data(&mut tx, None)).unwrap();
    let copy_elapsed = start.elapsed();
    assert_eq!(timed_copy.len(), ROWS);
    drop(timed_copy);

    let floor_queries_before = tx.query_count();
    let start = Instant::now();
    let floor_rows = rt.block_on(tx.query(FLOOR, &[&ids])).unwrap();
    let floor_elapsed = start.elapsed();
    assert_eq!(floor_rows.len(), ROWS);
    for (index, row) in floor_rows.iter().enumerate() {
        assert_eq!(row.get::<_, i64>(0), ids[index]);
        assert_eq!(row.get::<_, String>(1), format!("Name {}", index + 1));
        assert_eq!(row.get::<_, String>(2), format!("Code {}", index + 1));
    }
    println!(
        "narrow_SQL_read_floor rows={ROWS} queries={} elapsed_ns={} copy_to_floor_ratio={:.6}",
        tx.query_count() - floor_queries_before,
        floor_elapsed.as_nanos(),
        copy_elapsed.as_secs_f64() / floor_elapsed.as_secs_f64()
    );
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    rt.block_on(h.cleanup()).unwrap();
}

fn copy_data_semantics() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(async {
        Harness::builder([&base_users::ADDON, &ADDON])
            .with_decls([ModelDecl::new("test_copy_alloc", "test_copy_alloc.item")
                .log_access(false)
                .fields(const { &[FieldDecl::char("name"), FieldDecl::char("code")] })])
            .build()
            .await
            .unwrap()
    });
    let mut tx = rt.block_on(h.begin()).unwrap();
    rt.block_on(tx.batch("INSERT INTO test_copy_alloc_item (name, code) SELECT 'Name ' || g, 'Code ' || g FROM generate_series(1, 1000) AS g"))
        .unwrap();
    let ids: Vec<i64> = rt
        .block_on(tx.query("SELECT id FROM test_copy_alloc_item ORDER BY id", &[]))
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(ids.len(), ROWS);
    let records = tx
        .env()
        .records("test_copy_alloc.item")
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap();
    rt.block_on(tx.invalidate_all()).unwrap();
    let queries_before = tx.query_count();
    let copied = rt.block_on(records.copy_data(&mut tx, None)).unwrap();
    let copy_queries = tx.query_count() - queries_before;
    assert_eq!(copied.len(), ROWS);
    for (index, result) in copied.iter().enumerate() {
        let values = result.as_ref().expect("every source record is copied");
        assert_eq!(
            values.get("name"),
            Some(&Value::str(format!("Name {}", index + 1)))
        );
        assert_eq!(
            values.get("code"),
            Some(&Value::str(format!("Code {}", index + 1)))
        );
        assert_eq!(values.len(), 2);
    }
    assert!(
        copy_queries <= 1,
        "copy_data used {copy_queries} statements"
    );
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    rt.block_on(h.cleanup()).unwrap();
}
