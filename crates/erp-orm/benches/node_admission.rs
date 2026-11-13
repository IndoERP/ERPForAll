//! Cost of request admission under the node registry (SCALE.NODES, design §6.4): one
//! `NodeAdmission::is_serving` (three atomic loads and one read of the membership clock, a
//! monotonic clock that counts suspend) on a ready handle, alone and with seven other threads
//! reading a clone of the handle, next to `std::time::Instant::now` (the clock that does not
//! count suspend) under the same two loads, and `NodeAdmission::readiness`. The handle is
//! detached (no database, no heartbeat task): the hot path is exactly the one a request task
//! runs. Needs the `bench-seams` feature for the detached constructors. Results.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "benchmark setup")]

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use erp_orm::node::{
    AddonContent, AddonsDigest, BuildIdentity, NodeAdmission, NodeIdentity, NodeRole, Readiness,
};

fn admission() -> NodeAdmission {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let build = BuildIdentity::__from_build(
        "erp-bench-node",
        "1.0.0",
        false,
        Some("0123456789abcdef0123456789abcdef01234567"),
        Some("0"),
    );
    let addons = AddonsDigest::builder()
        .addon("base", "1.0.0", AddonContent::Embedded)
        .unwrap()
        .build()
        .unwrap();
    let identity = rt
        .block_on(
            NodeIdentity::builder(NodeRole::App, "bench-1", "host-1", build, addons)
                .create_detached_for_benches(),
        )
        .unwrap();
    let admission =
        NodeAdmission::detached_for_benches(Arc::new(identity), "bench", Duration::from_secs(3600));
    assert!(admission.is_serving());
    assert_eq!(admission.readiness(), Readiness::Ready);
    admission
}

fn bench(c: &mut Criterion) {
    let admission = admission();
    let mut group = c.benchmark_group("node_admission");
    group.bench_function("std_instant_now", |b| b.iter(|| black_box(Instant::now())));
    group.bench_function("is_serving", |b| {
        b.iter(|| black_box(black_box(&admission).is_serving()));
    });
    group.bench_function("readiness", |b| {
        b.iter(|| black_box(black_box(&admission).readiness()));
    });
    // eight request tasks on eight threads: the timed thread reads while seven others read in
    // a loop (the atomics are read-only, so their cache line stays shared; what is left is the
    // clock's own cost under concurrent reads)
    with_readers(
        || black_box(Instant::now()),
        || {
            group.bench_function("std_instant_now_with_7_concurrent_readers", |b| {
                b.iter(|| black_box(Instant::now()));
            });
        },
    );
    let shared = admission.clone();
    with_readers(
        move || shared.is_serving(),
        || {
            group.bench_function("is_serving_with_7_concurrent_readers", |b| {
                b.iter(|| black_box(black_box(&admission).is_serving()));
            });
        },
    );
    group.finish();
}

/// Run `timed` while seven threads call `read` in a loop.
fn with_readers<R, T>(read: R, timed: impl FnOnce())
where
    R: Fn() -> T + Clone + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..7)
        .map(|_| {
            let read = read.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut reads = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    black_box(read());
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    timed();
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        assert!(reader.join().unwrap() > 0);
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
