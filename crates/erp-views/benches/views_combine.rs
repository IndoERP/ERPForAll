//! combined `res.partner` form with 40 inheriting views, < 5 ms cold.
//!
//! "Cold" means from
//! the raw arch strings: catalogue indexing, parsing every arch and applying every spec, nothing
//! cached.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "benchmark setup")]

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use erp_views::ViewCatalogue;
use partner_form::fixture;
use std::hint::black_box;

#[path = "../tests/support/partner_form.rs"]
mod partner_form;

fn bench(c: &mut Criterion) {
    let views = fixture();
    // Fail the bench run loudly if the fixture does not combine.
    let combined = ViewCatalogue::new(views.clone())
        .and_then(|catalogue| catalogue.combine(1))
        .expect("fixture combines");
    eprintln!(
        "fixture: base {} bytes, combined {} bytes, {} views applied",
        views[0].arch().len(),
        combined.to_xml().len(),
        combined.applied_view_ids().len()
    );

    c.bench_function("res_partner_40_extensions/cold", |b| {
        b.iter_batched(
            || views.clone(),
            |views| {
                let catalogue = ViewCatalogue::new(views).expect("fixture combines");
                black_box(catalogue.combine(1).expect("fixture combines"))
            },
            BatchSize::SmallInput,
        );
    });

    let catalogue = ViewCatalogue::new(views.clone()).expect("fixture combines");
    c.bench_function("res_partner_40_extensions/combine", |b| {
        b.iter(|| black_box(catalogue.combine(black_box(1)).expect("fixture combines")));
    });

    let base_only = ViewCatalogue::new(vec![views[0].clone()]).expect("fixture combines");
    c.bench_function("res_partner_base/parse", |b| {
        b.iter(|| black_box(base_only.combine(black_box(1)).expect("fixture combines")));
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
