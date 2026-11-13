//! Cost of the in-memory recordset operations on cached values: `filtered_domain`,
//! `sorted` (one and two terms), `mapped`, `filtered` on a field, and `grouped`, over 1,000, 10,000
//! and 100,000 records. The records are new (in-memory) records, so every value is in the cache
//! and no statement runs; the setup asserts it. Needs `DATABASE_URL` (the transaction holds a pooled
//! connection; nothing is sent). Results.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "benchmark setup")]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use erp_domain::Domain;
use erp_fields::{Cell, Value};
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
        ]
    },
);

const SIZES: [usize; 3] = [1_000, 10_000, 100_000];

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

/// `n` new records with pseudo-random names and numbers (a fixed permutation, so runs compare).
fn records(rt: &tokio::runtime::Runtime, tx: &mut Transaction, n: usize) -> Records {
    let model = tx.env().records("bench.partner").expect("model");
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let k = (i * 7919) % n;
        let mut vals = RawVals::new();
        vals.insert(
            "name".into(),
            Cell::Str(Arc::from(format!("partner {k:06}"))),
        );
        vals.insert("number".into(), Cell::Int(i32::try_from(k % 1000).unwrap()));
        vals.insert("active".into(), Cell::Bool(!k.is_multiple_of(3)));
        let rec = rt
            .block_on(tx.new_record(&model, vals, None, None))
            .expect("new record");
        ids.push(rec.rec_ids()[0]);
    }
    model.browse_ids(ids)
}

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut tx = transaction(&rt);
    let domain = Domain::from_json(&serde_json::json!([
        "|",
        ["number", ">", 900],
        "&",
        ["name", "ilike", "7"],
        ["active", "=", true]
    ]))
    .expect("domain");
    let mut group = c.benchmark_group("rset_ops");
    for n in SIZES {
        let rs = records(&rt, &mut tx, n);
        let before = tx.query_count();
        // warm every column once; nothing may hit the database
        rt.block_on(rs.values(&mut tx, "name")).expect("mapped");
        rt.block_on(rs.values(&mut tx, "number")).expect("mapped");
        rt.block_on(rs.values(&mut tx, "active")).expect("mapped");
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("filtered_domain", n), &rs, |b, rs| {
            b.iter(|| {
                rt.block_on(rs.filtered_domain(&mut tx, black_box(&domain)))
                    .unwrap()
            })
        });
        group.bench_with_input(BenchmarkId::new("sorted_name", n), &rs, |b, rs| {
            b.iter(|| {
                rt.block_on(rs.sorted(&mut tx, Some("name"), false))
                    .unwrap()
            })
        });
        group.bench_with_input(
            BenchmarkId::new("sorted_number_desc_name", n),
            &rs,
            |b, rs| {
                b.iter(|| {
                    rt.block_on(rs.sorted(&mut tx, Some("number desc, name"), false))
                        .unwrap()
                })
            },
        );
        group.bench_with_input(BenchmarkId::new("mapped_name", n), &rs, |b, rs| {
            b.iter(|| rt.block_on(rs.values(&mut tx, "name")).unwrap())
        });
        group.bench_with_input(BenchmarkId::new("filtered_active", n), &rs, |b, rs| {
            b.iter(|| {
                rt.block_on(rs.filtered_values(&mut tx, "active", Value::is_truthy))
                    .unwrap()
            })
        });
        group.bench_with_input(BenchmarkId::new("grouped_number", n), &rs, |b, rs| {
            b.iter(|| rt.block_on(rs.grouped(&mut tx, "number")).unwrap())
        });
        assert_eq!(
            tx.query_count(),
            before,
            "in-memory operations must not query"
        );
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
