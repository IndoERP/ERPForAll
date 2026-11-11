//! Allocation cost of `onchange_batch`'s specification per values entry (review J2-WEB round 3,
//! N8; standards §5b): the batch runs one `onchange` per entry through the method chain, which
//! takes the specification by value, so a specification that is deep-copied per entry costs its
//! whole size once per entry; the onchange diff then copies the item of every changed field.
//!
//! One test in its own binary, run on the main thread by `erp_alloc_budget::harness` (one profiler,
//! every thread counted). The runtime is current-thread and built outside the measured closure.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    reason = "tests"
)]

use erp_orm::{Spec, SpecItem};
use erp_sdk::{Value, ValueMap};
use erp_test_models::{base_users, test_web};
use erp_testkit::Harness;
use erp_testkit::alloc::{self, AllocStats};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    onchange_batch_does_not_copy_the_spec_per_entry,
    onchange_batch_semantics,
);

const SMALL: usize = 10;
const LARGE: usize = 20;
/// Keys of the padding context: every one is a `String` a deep copy allocates.
const PADDING: usize = 256;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn spec(padded: bool) -> Spec {
    let mut body = SpecItem::default();
    if padded {
        body.context = Some(
            (0..PADDING)
                .map(|i| (format!("padding_key_{i}"), Value::Int(1)))
                .collect(),
        );
    }
    Spec::new().field("name").with("body", body).field("size")
}

fn entries(n: usize) -> Vec<ValueMap> {
    (0..n)
        .map(|i| {
            [("body".to_owned(), Value::str(format!("b{i}")))]
                .into_iter()
                .collect()
        })
        .collect()
}

fn measure(rt: &tokio::runtime::Runtime, h: &Harness, n: usize, padded: bool) -> AllocStats {
    let mut tx = rt.block_on(h.begin()).unwrap();
    let messages = tx.env().records("test_web.message").unwrap();
    // warm: statements prepared, access memoized
    rt.block_on(messages.onchange_batch(&mut tx, entries(1), Vec::new(), spec(padded)))
        .unwrap();
    let (values, spec) = (entries(n), spec(padded));
    let (stats, out) =
        alloc::measure(|| rt.block_on(messages.onchange_batch(&mut tx, values, Vec::new(), spec)))
            .expect("measure");
    assert_eq!(out.unwrap().len(), n);
    rt.block_on(tx.rollback()).unwrap();
    stats
}

fn onchange_batch_does_not_copy_the_spec_per_entry() {
    let rt = runtime();
    let h = rt
        .block_on(Harness::builder([&base_users::ADDON, &test_web::ADDON]).build())
        .expect("harness");
    let per_entry = |padded: bool| {
        let small = measure(&rt, &h, SMALL, padded);
        let large = measure(&rt, &h, LARGE, padded);
        (large.total_blocks as f64 - small.total_blocks as f64) / (LARGE - SMALL) as f64
    };
    let plain = per_entry(false);
    let padded = per_entry(true);
    println!(
        "onchange_batch per entry: {plain:.1} blocks with the plain spec, {padded:.1} with a \
         {PADDING}-key context"
    );
    // a deep copy costs at least one block per padding key (48f85ee8: two copies per entry, 514
    // blocks); a shared spec costs nothing more per entry. The margin absorbs the driver's
    // per-batch blocks (lesson).
    assert!(
        plain <= 320.0,
        "{plain:.1} blocks per entry with the plain spec"
    );
    assert!(
        padded - plain < 16.0,
        "the padded spec costs {:.1} more blocks per entry",
        padded - plain
    );
    rt.block_on(h.cleanup()).unwrap();
}

fn onchange_batch_semantics() {
    let rt = runtime();
    let h = rt
        .block_on(Harness::builder([&base_users::ADDON, &test_web::ADDON]).build())
        .unwrap();
    rt.block_on(async {
        for n in [SMALL, LARGE] {
            for padded in [false, true] {
                let mut tx = h.begin().await.unwrap();
                let records = tx.env().records("test_web.message").unwrap();
                let result = records
                    .onchange_batch(&mut tx, entries(n), Vec::new(), spec(padded))
                    .await
                    .unwrap();
                assert_eq!(result.len(), n);
                tx.rollback().await.unwrap();
            }
        }
        h.cleanup().await.unwrap();
    });
}
