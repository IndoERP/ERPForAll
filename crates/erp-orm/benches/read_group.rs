//! Grouped reads and `web_read_group` (slice S5): wall time and statements per call on 100,000
//! lines and 1,000 partners, built inside one transaction that is rolled back at the end (the
//! tables never outlive the run). Needs `DATABASE_URL`. Results:.
//!
//! - `read_group_m2o_sum`: `_read_group([], ['partner_id'], ['value:sum', '__count'])`.
//! - `read_group_month_partner_ordered`: `['date:month', 'partner_id']` ordered by the partners'
//!   own order (the comodel join and its `GROUP BY` columns).
//! - `web_read_group_open_10`: ten first-level groups opened with their records (one statement for
//!   the groups, one for every opened group's records).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "benchmark setup")]

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use erp_domain::Domain;
use erp_orm::{
    FormattedReadGroup, Orm, ReadGroup, SUPERUSER_ID, SearchOptions, Spec, StaticPolicy,
    Transaction, WebReadGroup,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "bench",
    depends: &["base"],
    exact_only: false,
};

static PARTNER: ModelDecl = ModelDecl::new("bench", "bench.partner")
    .order("name")
    .fields(const { &[FieldDecl::char("name")] });

static LINE: ModelDecl = ModelDecl::new("bench", "bench.line").fields(
    const {
        &[
            FieldDecl::many2one("partner_id", "bench.partner"),
            FieldDecl::integer("value"),
            FieldDecl::date("date"),
        ]
    },
);

fn transaction(rt: &tokio::runtime::Runtime) -> Transaction {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let registry = Arc::new(
        RegistryBuilder::new([&ADDON])
            .with_decls([&PARTNER, &LINE])
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
    let mut tx = rt.block_on(orm.begin(SUPERUSER_ID)).expect("transaction");
    rt.block_on(async {
        tx.init_models(&[]).await?;
        tx.batch(
            "INSERT INTO bench_partner (name) SELECT 'partner ' || lpad(((i * 7919) % 1000)::text, \
             4, '0') FROM generate_series(1, 1000) i; INSERT INTO bench_line (partner_id, value, \
             date) SELECT 1 + (i % 1000), i % 97, DATE '2024-01-01' + (i % 365) FROM \
             generate_series(1, 100000) i; ANALYZE bench_partner; ANALYZE bench_line",
        )
        .await?;
        tx.invalidate_all().await
    })
    .expect("data");
    tx
}

async fn per_group(
    tx: &mut Transaction,
    lines: &erp_orm::Records,
    spec: &Spec,
) -> Vec<erp_fields::Value> {
    let groups = lines
        .formatted_read_group(
            tx,
            FormattedReadGroup::new(Domain::TRUE, ["partner_id"]).aggregates(["__count"]),
        )
        .await
        .unwrap();
    let mut all = Vec::new();
    for g in groups.groups.iter().take(10) {
        let extra = g.get("__extra_domain").unwrap();
        let domain = erp_orm::web::domain_from_value(extra).unwrap();
        let found = lines
            .search_with(
                tx,
                &domain,
                &SearchOptions::new()
                    .limit(80)
                    .fetch_fields(["value", "date"]),
            )
            .await
            .unwrap();
        all.extend(found.rec_ids().iter().copied());
    }
    lines
        .browse_ids(all)
        .web_read(tx, spec.clone())
        .await
        .unwrap()
}

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut tx = transaction(&rt);
    let lines = tx.env().records("bench.line").unwrap();
    let mut group = c.benchmark_group("read_group");
    let report = |name: &str, tx: &Transaction, before: u64| {
        eprintln!(
            "{name}: {} statement(s) per call",
            tx.query_count() - before
        );
    };

    let before = tx.query_count();
    rt.block_on(
        lines._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE)
                .groupby(["partner_id"])
                .aggregates(["value:sum", "__count"]),
        ),
    )
    .unwrap();
    report("read_group_m2o_sum", &tx, before);
    group.bench_function("read_group_m2o_sum", |b| {
        b.iter(|| {
            rt.block_on(
                lines._read_group(
                    &mut tx,
                    ReadGroup::new(Domain::TRUE)
                        .groupby(["partner_id"])
                        .aggregates(["value:sum", "__count"]),
                ),
            )
            .unwrap()
        })
    });

    let month = || {
        ReadGroup::new(Domain::TRUE)
            .groupby(["date:month", "partner_id"])
            .aggregates(["value:sum"])
            .order("date:month, partner_id")
    };
    let before = tx.query_count();
    rt.block_on(lines._read_group(&mut tx, month())).unwrap();
    report("read_group_month_partner_ordered", &tx, before);
    group.bench_function("read_group_month_partner_ordered", |b| {
        b.iter(|| rt.block_on(lines._read_group(&mut tx, month())).unwrap())
    });

    let spec = Spec::new().field("value").field("date");
    let open = || {
        let mut args = WebReadGroup::new(Domain::TRUE, ["partner_id"]);
        args.limit = Some(10);
        args.auto_unfold = true;
        args.unfold_read_specification = Some(Spec::new().field("value").field("date"));
        args
    };
    rt.block_on(tx.invalidate_all()).unwrap();
    let before = tx.query_count();
    rt.block_on(lines.web_read_group(&mut tx, open())).unwrap();
    report("web_read_group_open_10", &tx, before);
    group.bench_function("web_read_group_open_10", |b| {
        b.iter(|| {
            rt.block_on(async {
                tx.invalidate_all().await.unwrap();
                lines.web_read_group(&mut tx, open()).await.unwrap()
            })
        })
    });

    rt.block_on(tx.invalidate_all()).unwrap();
    let before = tx.query_count();
    rt.block_on(per_group(&mut tx, &lines, &spec));
    report("per_group_searches_10", &tx, before);
    group.bench_function("per_group_searches_10", |b| {
        b.iter(|| {
            rt.block_on(async {
                tx.invalidate_all().await.unwrap();
                per_group(&mut tx, &lines, &spec).await
            })
        })
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
