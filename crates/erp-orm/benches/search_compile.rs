//! Cost of turning a client domain into a search query through the real ORM path
//! (`Records::search_query`: access, active test, FULL optimization, planning, the search memo),
//! with the memo on and off. Each iteration parses the JSON domain, as a JSON-2
//! request must. No statement runs: the query is built, not executed. Needs `DATABASE_URL` (a
//! connection is opened for the transaction; nothing is sent). Results;
//! the relational cases (paths, `any`, many2one order).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "benchmark setup")]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use erp_domain::Domain;
use erp_orm::{Orm, OrmConfig, SUPERUSER_ID, SearchOptions, StaticPolicy, Transaction};
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
            FieldDecl::char("email"),
            FieldDecl::integer("number"),
            FieldDecl::boolean("active"),
            FieldDecl::many2one("country_id", "bench.country"),
            FieldDecl::many2many("category_ids", "bench.category"),
            FieldDecl::many2one("parent_id", "bench.partner"),
            FieldDecl::one2many("child_ids", "bench.partner", "parent_id"),
        ]
    },
);

static COUNTRY: ModelDecl = ModelDecl::new("bench", "bench.country")
    .fields(const { &[FieldDecl::char("name"), FieldDecl::char("code")] })
    .order("name");

static CATEGORY: ModelDecl =
    ModelDecl::new("bench", "bench.category").fields(const { &[FieldDecl::char("name")] });

fn transaction(rt: &tokio::runtime::Runtime, memo_size: usize) -> Transaction {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let registry = Arc::new(
        RegistryBuilder::new([&ADDON])
            .with_decls([&PARTNER, &COUNTRY, &CATEGORY])
            .build()
            .expect("registry"),
    );
    let config = url
        .parse::<tokio_postgres::Config>()
        .expect("DATABASE_URL config");
    let pool = erp_orm::pool::create_pool(config, 4).expect("pool");
    let orm = Orm::builder(registry, pool)
        .policy(StaticPolicy::AllowAll)
        .config(OrmConfig {
            search_memo_size: memo_size,
            ..OrmConfig::default()
        })
        .build()
        .expect("orm");
    rt.block_on(orm.begin(SUPERUSER_ID)).expect("transaction")
}

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let ids: Vec<i64> = (1..=500).collect();
    let cases = [
        (
            "list_view",
            serde_json::json!([
                ["name", "ilike", "acme"],
                ["number", ">", 5],
                ["email", "!=", false]
            ]),
        ),
        ("id_list_500", serde_json::json!([["id", "in", ids]])),
        (
            "m2o_path",
            serde_json::json!([["country_id.code", "=", "ID"], ["name", "ilike", "acme"]]),
        ),
        (
            "x2many_paths",
            serde_json::json!([
                ["category_ids.name", "ilike", "vip"],
                ["child_ids.country_id.code", "!=", "ID"]
            ]),
        ),
    ];
    for (memo, size) in [("memo_on", 4_096), ("memo_off", 0)] {
        let mut tx = transaction(&rt, size);
        let partners = tx.env().records("bench.partner").expect("model");
        let opts = SearchOptions::new();
        let by_country = SearchOptions::new().order("country_id, name");
        c.bench_function(&format!("search_query/order_m2o/{memo}"), |b| {
            b.iter(|| {
                let domain = Domain::from_json(black_box(&cases[0].1)).expect("parse");
                rt.block_on(partners.search_query(&mut tx, &domain, &by_country))
                    .expect("query")
            })
        });
        for (name, json) in &cases {
            c.bench_function(&format!("search_query/{name}/{memo}"), |b| {
                b.iter(|| {
                    let domain = Domain::from_json(black_box(json)).expect("parse");
                    rt.block_on(partners.search_query(&mut tx, &domain, &opts))
                        .expect("query")
                })
            });
        }
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
