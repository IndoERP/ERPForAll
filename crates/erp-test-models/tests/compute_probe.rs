//! Timing probe for the scalar compute engine (not a parity case, not a benchmark of record).
//! `ERP_PROBE_N` sets the number of records (default 100,000).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::time::Instant;

use erp_test_models::test_compute::{self as tc, PerfBase};
use erp_testkit::Harness;

#[tokio::test]
#[ignore = "timing probe; run explicitly with --run-ignored only"]
async fn probe_scalar_recompute() {
    let n: i32 = std::env::var("ERP_PROBE_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let h = Harness::builder([&tc::ADDON]).build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();

    let t = Instant::now();
    let vals = (0..n)
        .map(|i| model.vals().set(PerfBase::VALUE, i))
        .collect();
    let recs = model.create(&mut tx, vals).await.unwrap();
    tx.flush().await.unwrap();
    let create = t.elapsed();
    let q_create = tx.query_count();

    let t = Instant::now();
    recs.set(&mut tx, PerfBase::VALUE, 7).await.unwrap();
    let write = t.elapsed();
    // pull recompute (no SQL), then the flush (the UPDATE)
    let q0 = tx.query_count();
    let t = Instant::now();
    recs.mapped(&mut tx, PerfBase::VALUE_PC).await.unwrap();
    let recompute = t.elapsed();
    let t = Instant::now();
    tx.flush().await.unwrap();
    let flush = t.elapsed();
    let q_flush = tx.query_count() - q0;
    eprintln!("probe n={n}: recompute (pull, in memory) {recompute:?}");

    // The plan of the flush's UPDATE, with fresh statistics (same statement shape as the flush).
    tx.batch("ANALYZE test_performance_base").await.unwrap();
    let ids: Vec<i64> = recs.ids();
    let values: Vec<Option<i32>> = vec![Some(9); ids.len()];
    let pcs: Vec<Option<f64>> = vec![Some(0.09); ids.len()];
    let uids: Vec<Option<i64>> = vec![Some(1); ids.len()];
    let explain = "EXPLAIN (ANALYZE, BUFFERS) UPDATE \"test_performance_base\" SET \
                   \"value\" = \"__v\".\"value\", \"value_pc\" = \"__v\".\"value_pc\", \
                   \"write_uid\" = \"__v\".\"write_uid\", \
                   \"write_date\" = (now() AT TIME ZONE 'UTC') \
                   FROM unnest($1::int8[], $2::int4[], $3::float8[], $4::int8[]) \
                   AS \"__v\"(\"id\", \"value\", \"value_pc\", \"write_uid\") \
                   WHERE \"test_performance_base\".\"id\" = \"__v\".\"id\"";
    let plan = tx
        .query(explain, &[&ids, &values, &pcs, &uids])
        .await
        .unwrap();
    for row in &plan {
        eprintln!("plan| {}", row.get::<_, String>(0));
    }
    // back to the probed values (the raw UPDATE bypassed the cache)
    tx.invalidate_all().await.unwrap();
    recs.set(&mut tx, PerfBase::VALUE, 7).await.unwrap();

    tx.commit().await.unwrap();
    let t = Instant::now();
    let values = recs.mapped(&mut tx, PerfBase::VALUE_PC).await.unwrap();
    let read = t.elapsed();
    assert!(values.iter().all(|v| (*v - 0.07).abs() < 1e-12));
    eprintln!(
        "probe n={n}: create+precompute {create:?} ({q_create} statements); write {write:?}; \
         flush (UPDATE) {flush:?} ({q_flush} statements); read back {read:?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
