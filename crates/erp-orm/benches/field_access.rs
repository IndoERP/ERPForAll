//! Cost of the field access checks on the read path (engine note §7): one
//! `Records::cell` on a cached value of a single in-memory record, as the superuser (the check is
//! skipped before the policy is asked) and as a user principal under `StaticPolicy::AllowAll` (a
//! principal binding, the policy's mask, one field lookup), and `fields_get` of a model for both.
//! No statement runs; the setup asserts it. Needs `DATABASE_URL` (the transaction holds a pooled
//! connection; nothing is sent). Results.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "benchmark setup")]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use erp_fields::Cell;
use erp_orm::{Orm, RawVals, Records, SUPERUSER_ID, StaticPolicy, Transaction};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "bench",
    depends: &["base"],
    exact_only: false,
};

static PARTNER: ModelDecl = ModelDecl::new("bench", "bench.partner").fields(
    const {
        &[
            FieldDecl::char("name"),
            FieldDecl::integer("number"),
            FieldDecl::boolean("active"),
            FieldDecl::char("email").groups("base.group_system"),
        ]
    },
);

fn transaction(rt: &tokio::runtime::Runtime) -> Transaction {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let registry = Arc::new(
        RegistryBuilder::new([&ADDON])
            .with_decls([&PARTNER])
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

fn record(rt: &tokio::runtime::Runtime, tx: &mut Transaction) -> Records {
    let model = tx.env().records("bench.partner").expect("model");
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str(Arc::from("partner 000001")));
    vals.insert("number".into(), Cell::Int(7));
    rt.block_on(tx.new_record(&model, vals, None, None))
        .expect("new record")
}

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut tx = transaction(&rt);
    let su = record(&rt, &mut tx);
    let user = su.with_user(2);
    let before = tx.query_count();
    rt.block_on(su.cell(&mut tx, "name")).expect("warm");
    let mut group = c.benchmark_group("field_access");
    group.bench_function("cell_superuser", |b| {
        b.iter(|| rt.block_on(su.cell(&mut tx, black_box("name"))).unwrap())
    });
    group.bench_function("cell_user", |b| {
        b.iter(|| rt.block_on(user.cell(&mut tx, black_box("name"))).unwrap())
    });
    group.bench_function("fields_get_superuser", |b| {
        b.iter(|| {
            rt.block_on(black_box(&su).fields_get(&mut tx, None, None))
                .unwrap()
        })
    });
    group.bench_function("fields_get_user", |b| {
        b.iter(|| {
            rt.block_on(black_box(&user).fields_get(&mut tx, None, None))
                .unwrap()
        })
    });
    assert_eq!(tx.query_count(), before, "field checks must not query");
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
