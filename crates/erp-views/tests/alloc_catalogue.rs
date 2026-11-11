//! Allocation pin of the record layer's catalogue (standards §5b):
//! the 41 rows of the `res.partner` fixture, as the `ir.ui.view` layer fetches them,
//! turned into descriptors, indexed and combined. A ratchet: the budget is the value measured
//! when merged and may only go down. One test in its own binary, run on the main thread
//! by `erp_alloc_budget::harness`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

use erp_alloc_budget::{self as alloc, Budget};
use erp_views::record::{ViewMode, ViewRow, validation_targets};
use erp_views::{Inheritance, ViewCatalogue};

#[path = "support/partner_form.rs"]
mod partner_form;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    catalogue_from_rows_and_combine_within_the_pin,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    write_check_of_an_extension_within_the_pin,
    catalogue_from_rows_and_combine_semantics,
    write_check_of_an_extension_semantics,
);

const PIN: Budget = Budget::new(PIN_BLOCKS, PIN_BYTES);
const PIN_BLOCKS: u64 = 3_107;
const PIN_BYTES: u64 = 424_624;

/// The fixture as fetched rows: the strings are owned by the rows, as the record layer reads them.
fn rows() -> Vec<ViewRow> {
    partner_form::fixture()
        .into_iter()
        .map(|v| {
            let (mode, parent) = match v.inheritance() {
                Inheritance::Root => (ViewMode::Primary, None),
                Inheritance::Primary { parent } => (ViewMode::Primary, Some(parent)),
                Inheritance::Extension { parent } => (ViewMode::Extension, Some(parent)),
                other => panic!("unexpected inheritance {other:?}"),
            };
            ViewRow::new(v.id(), mode)
                .name(v.name())
                .model(Some(v.model().to_owned()))
                .arch(v.arch())
                .inherit_id(parent)
                .priority(v.priority())
                .active(v.active())
        })
        .collect()
}

fn build_and_combine(rows: Vec<ViewRow>) -> usize {
    let views = rows
        .into_iter()
        .map(ViewRow::into_descriptor)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let catalogue = ViewCatalogue::new(views).unwrap();
    catalogue.combine(1).unwrap().applied_view_ids().len()
}

fn catalogue_from_rows_and_combine_within_the_pin() {
    // warm: lazily initialised statics of the engine are not part of the pin
    assert_eq!(build_and_combine(rows()), 41);
    let rows = rows();
    assert_eq!(rows.len(), 41);
    let (stats, applied) = alloc::measure(|| build_and_combine(rows)).expect("measure");
    assert_eq!(applied, 41);
    println!("catalogue from 41 rows + combine: {stats:?}");
    stats.assert_within(&PIN);
}

/// The combinations a write of one extension of the fixture sends to the engine (review R-4): its
/// targets, then `combine_many` of the validated and combined views.
// Lowered from 3,857 blocks / 504,694 bytes after the indexed, byte-accounted cache.
const WRITE_PIN: Budget = Budget::new(WRITE_PIN_BLOCKS, WRITE_PIN_BYTES);
const WRITE_PIN_BLOCKS: u64 = 3_853;
const WRITE_PIN_BYTES: u64 = 497_522;

fn write_check(catalogue: &ViewCatalogue, written: i64) -> usize {
    let targets = validation_targets(catalogue, &[written]).unwrap();
    let mut all = targets.validated;
    all.extend(targets.combined);
    catalogue.combine_many(&all).unwrap().len()
}

fn write_check_of_an_extension_within_the_pin() {
    let views = rows()
        .into_iter()
        .map(ViewRow::into_descriptor)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let catalogue = ViewCatalogue::new(views).unwrap();
    let written = 41;
    assert!(matches!(
        catalogue.get(written).unwrap().inheritance(),
        Inheritance::Extension { .. }
    ));
    // warm, as above
    let combinations = write_check(&catalogue, written);
    let (stats, measured) = alloc::measure(|| write_check(&catalogue, written)).expect("measure");
    assert_eq!(measured, combinations);
    println!("write check of extension {written}: {combinations} combination(s), {stats:?}");
    stats.assert_within(&WRITE_PIN);
}

fn catalogue_from_rows_and_combine_semantics() {
    let rows = rows();
    assert_eq!(rows.len(), 41);
    assert_eq!(build_and_combine(rows), 41);
}

fn write_check_of_an_extension_semantics() {
    let views = rows()
        .into_iter()
        .map(ViewRow::into_descriptor)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let catalogue = ViewCatalogue::new(views).unwrap();
    assert!(matches!(
        catalogue.get(41).unwrap().inheritance(),
        Inheritance::Extension { .. }
    ));
    assert_eq!(write_check(&catalogue, 41), 1);
}
