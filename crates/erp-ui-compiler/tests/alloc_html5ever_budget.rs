//! Envelope measurement of the bounded html5ever fork (plan section 5): for every
//! corpus entry, the bytes the parser charged to its budget against the heap peak dhat observed
//! around the whole parse (tree included). Prints one JSON line per entry; the ratio
//! `peak_bytes / charged_peak` over the corpus is the measured `K`, the empty input gives `C`.
//! The invariant `peak_bytes <= K * charged_peak + C` is checked by `scripts/html-budget-envelope.sh`
//! on every run. A refusal on an entry is reported as such, never as a measurement. Run only on
//! the build host (`scripts/html-budget-envelope.sh`); product latency is not measured.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "measurement harness"
)]

mod html_adversarial;

use erp_alloc_budget as alloc;
use erp_ui_compiler::html_budget::{ParseBudget, ParseError, ParseLimits, parse_document_bounded};
use markup5ever_rcdom::RcDom;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    envelope_empty,
    envelope_attribute_floods,
    envelope_nesting,
    envelope_char_ref_bombs,
    envelope_unclosed,
    envelope_multibyte,
);

/// Generous limits so no corpus entry trips: this measures charged versus real bytes, not refusal.
fn limits() -> ParseLimits {
    ParseLimits::new(1 << 32, 1 << 40, 1 << 20, 1 << 32).unwrap()
}

fn run(family: &str) {
    let entries = html_adversarial::all()
        .into_iter()
        .find(|(f, _)| *f == family)
        .unwrap_or_else(|| panic!("unknown corpus family {family}"))
        .1;
    for e in entries {
        let input_bytes: u64 = e.chunks.iter().map(|c| c.len() as u64).sum();
        let chunks: Vec<&str> = e.chunks.iter().map(String::as_str).collect();
        let budget = ParseBudget::new(limits());
        let (stats, result) = alloc::measure(|| {
            let r = parse_document_bounded(RcDom::default(), &chunks, 1 << 30, &budget);
            r.map(drop)
        })
        .unwrap();
        let outcome = match result {
            Ok(()) => "parsed".to_owned(),
            Err(ParseError::Budget(b)) => format!("refused:{:?}", b.kind()),
            Err(other) => panic!("{}: {other}", e.name),
        };
        println!(
            "{{\"family\":\"{family}\",\"entry\":\"{}\",\"input_bytes\":{input_bytes},\"charged_peak\":{},\"peak_bytes\":{},\"total_bytes\":{},\"outcome\":\"{outcome}\"}}",
            e.name,
            budget.peak_bytes(),
            stats.peak_bytes,
            stats.total_bytes,
        );
    }
}

fn envelope_empty() {
    run("empty");
}
fn envelope_attribute_floods() {
    run("attribute_floods");
}
fn envelope_nesting() {
    run("nesting");
}
fn envelope_char_ref_bombs() {
    run("char_ref_bombs");
}
fn envelope_unclosed() {
    run("unclosed");
}
fn envelope_multibyte() {
    run("multibyte");
}
