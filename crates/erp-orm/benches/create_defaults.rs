//! Micro-benchmark for default resolution on create batches.
//!
//! Measures the CPU and allocation cost of resolving missing field defaults
//! over batches of 10, 100, and 1,000 records across:
//! - Fast path: static scalar defaults resolved once per batch and cloned.
//! - Slow path (context): fallback path on the same model triggered by context default keys.
//! - Slow path (fn): fallback path with per-record function default.
//!
//! Needs `DATABASE_URL` (holds a pooled connection; no statements are sent).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "benchmark setup")]

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use erp_fields::{Cell, Value};
use erp_orm::{Orm, RawVals, SUPERUSER_ID, StaticPolicy, Transaction};
use erp_registry::{
    AddonDescriptor, DefaultContext, FieldDecl, ModelDecl, RegistryBuilder, StaticValue,
};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "bench",
    depends: &["base"],
    exact_only: false,
};

static ITEM: ModelDecl = ModelDecl::new("bench", "bench.default_item").fields(
    const {
        &[
            FieldDecl::char("name"),
            FieldDecl::boolean("active").default(StaticValue::Bool(true)),
            FieldDecl::char("state").default(StaticValue::Str("draft")),
            FieldDecl::integer("priority").default(StaticValue::Int(10)),
            FieldDecl::char("description"),
            FieldDecl::integer("count"),
        ]
    },
);

static SEQ: AtomicI64 = AtomicI64::new(0);

fn bench_seq(_ctx: &DefaultContext) -> Value {
    Value::Int(SEQ.fetch_add(1, Ordering::Relaxed))
}

static FN_ITEM: ModelDecl = ModelDecl::new("bench", "bench.default_fn_item").fields(
    const {
        &[
            FieldDecl::char("name"),
            FieldDecl::boolean("active").default(StaticValue::Bool(true)),
            FieldDecl::integer("seq").default_fn(bench_seq),
        ]
    },
);

const SIZES: [usize; 3] = [10, 100, 1_000];

fn transaction(rt: &tokio::runtime::Runtime) -> Transaction {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let registry = Arc::new(
        RegistryBuilder::new([&ADDON])
            .with_decls([&ITEM, &FN_ITEM])
            .build()
            .expect("registry"),
    );
    let config = url
        .parse::<tokio_postgres::Config>()
        .expect("DATABASE_URL config");
    let pool = erp_orm::pool::create_pool(config, 4).expect("pool");
    let orm = Orm::builder(registry, pool)
        .policy(StaticPolicy::AllowAll)
        .build()
        .expect("orm");
    rt.block_on(orm.begin(SUPERUSER_ID)).expect("transaction")
}

fn make_batch(n: usize) -> Vec<RawVals> {
    (0..n)
        .map(|i| {
            let mut vals = RawVals::new();
            vals.insert("name".into(), Cell::Str(Arc::from(format!("item_{i}"))));
            vals
        })
        .collect()
}

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut tx = transaction(&rt);
    let static_model = tx.env().records("bench.default_item").expect("model");
    let context_model = static_model.with_context([("default_ctx", Value::str("fallback"))]);
    let fn_model = tx.env().records("bench.default_fn_item").expect("model");
    let mut group = c.benchmark_group("create_defaults");

    for n in SIZES {
        let template = make_batch(n);
        group.throughput(Throughput::Elements(n as u64));

        // 1. Fast path: static scalar defaults resolved once per batch and cloned
        let before = tx.query_count();
        group.bench_with_input(
            BenchmarkId::new("fast_static", n),
            &template,
            |b, template| {
                b.iter(|| {
                    let mut batch = template.clone();
                    rt.block_on(async {
                        erp_orm::__bench::add_missing_defaults_batch(
                            &mut tx,
                            &static_model,
                            &mut batch,
                        )
                        .await
                        .unwrap();
                    });
                    black_box(batch)
                })
            },
        );
        assert_eq!(
            tx.query_count(),
            before,
            "fast path default resolution must not query database"
        );

        // 2. Slow path: fallback path on the same model triggered by context default key
        let before = tx.query_count();
        group.bench_with_input(
            BenchmarkId::new("slow_fallback_context", n),
            &template,
            |b, template| {
                b.iter(|| {
                    let mut batch = template.clone();
                    rt.block_on(async {
                        erp_orm::__bench::add_missing_defaults_batch(
                            &mut tx,
                            &context_model,
                            &mut batch,
                        )
                        .await
                        .unwrap();
                    });
                    black_box(batch)
                })
            },
        );
        assert_eq!(
            tx.query_count(),
            before,
            "slow context default resolution must not query database"
        );

        // 3. Slow path: fallback path on model with dynamic function default
        let before = tx.query_count();
        group.bench_with_input(
            BenchmarkId::new("slow_fn_default", n),
            &template,
            |b, template| {
                b.iter(|| {
                    let mut batch = template.clone();
                    rt.block_on(async {
                        erp_orm::__bench::add_missing_defaults_batch(
                            &mut tx, &fn_model, &mut batch,
                        )
                        .await
                        .unwrap();
                    });
                    black_box(batch)
                })
            },
        );
        assert_eq!(
            tx.query_count(),
            before,
            "slow fn default resolution must not query database"
        );
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
