//! The evaluation-step budget at the XPath and apply level: what each unit of work costs, and
//! that a charge never lets work run past the limit. The per-path hostile combinations are in
//! `tests/work_limits.rs`.

use super::arch;
use crate::apply::apply_inheritance_specs;
use crate::arch::Caps;
use crate::error::{LimitKind, ViewError};
use crate::limits::{Budget, ViewLimits};
use crate::xpath::XPath;

fn budget_of(max: usize) -> (ViewLimits, Budget) {
    let limits = ViewLimits::default()
        .lowered(LimitKind::EvaluationSteps, max)
        .expect("valid budget");
    let budget = Budget::new(&limits);
    (limits, budget)
}

fn exceeded(max: usize) -> ViewError {
    ViewError::LimitExceeded {
        limit: LimitKind::EvaluationSteps,
        max,
        actual: Some(max + 1),
    }
}

/// `//a[@x='2']` over four nodes: the mark table (1), then per node one visit and one tag
/// comparison, and for each `<a>` the predicate (1), the scan of its one attribute name (1) and
/// the value comparison (1): 1 + 2 + 5 + 2 + 5 = 15.
#[test]
fn xpath_charges_visits_tags_attribute_scans_and_values() {
    let source = arch(r#"<form><a x="1"/><b/><a x="2"/></form>"#);
    let xpath = XPath::parse("//a[@x='2']", &ViewLimits::default()).expect("valid");
    let (_, mut enough) = budget_of(15);
    assert_eq!(xpath.all(&source, &mut enough).map(|ids| ids.len()), Ok(1));
    assert_eq!(enough.spent(), 15);
    let (_, mut short) = budget_of(14);
    let err = xpath.all(&source, &mut short).unwrap_err();
    assert_eq!(err, exceeded(14));
    assert_eq!(
        err.to_string(),
        "evaluation steps per combination limit exceeded: maximum 14, reached 15"
    );
    // `first` stops at the first match, here the last node: the same 15.
    let (_, mut first) = budget_of(15);
    assert!(matches!(xpath.first(&source, &mut first), Ok(Some(_))));
    assert_eq!(first.spent(), 15);
}

/// Every step of a multi-step path walks the tree and has its own mark table; compound
/// predicates charge each test node they evaluate.
#[test]
fn multi_step_paths_and_compound_predicates_are_charged() {
    let source = arch("<form><g><a/></g><g><a/></g></form>");
    // Two tables (2); step 1 visits and compares 5 nodes (10); step 2 visits 5 and compares the
    // two children of a marked <g> (7).
    let xpath = XPath::parse("//g/a", &ViewLimits::default()).expect("valid");
    let (_, mut budget) = budget_of(100);
    assert_eq!(xpath.all(&source, &mut budget).map(|ids| ids.len()), Ok(2));
    assert_eq!(budget.spent(), 19);
    // One table; per node a visit, the `or`, and both attribute tests (no attributes to scan).
    let or = XPath::parse("//*[@x or @y]", &ViewLimits::default()).expect("valid");
    let (_, mut budget) = budget_of(100);
    assert_eq!(or.all(&source, &mut budget).map(|ids| ids.len()), Ok(0));
    assert_eq!(budget.spent(), 21);
}

/// A mark table is charged by the arena's slot count, removed and detached nodes included.
#[test]
fn mark_tables_are_charged_by_arena_slots() {
    let mut source = arch("<form><a/></form>");
    for _ in 0..1_000 {
        source.alloc_sentinel().expect("within caps");
    }
    let xpath = XPath::parse("//a", &ViewLimits::default()).expect("valid");
    let (_, mut budget) = budget_of(1_000);
    assert_eq!(xpath.all(&source, &mut budget).map(|ids| ids.len()), Ok(1));
    // Table of 1,002 slots: 1 + 1,002 / 16 = 63; then form and a: a visit and a comparison each.
    assert_eq!(budget.spent(), 67);
}

/// Long values cost one step per 16 bytes compared; differing lengths cost one step.
#[test]
fn string_comparisons_are_charged_by_length() {
    let (_, mut budget) = budget_of(1_000);
    let long = "x".repeat(160);
    assert_eq!(budget.str_eq(&long, &long), Ok(true));
    assert_eq!(budget.spent(), 11);
    assert_eq!(budget.str_eq(&long, "x"), Ok(false));
    assert_eq!(budget.spent(), 12);
}

/// A charge of several steps that crosses the limit fails as a whole, names the first step past
/// the limit, and leaves the counter pinned so every later charge fails too.
#[test]
fn exhausted_budget_stays_exhausted() {
    let (_, mut budget) = budget_of(10);
    assert_eq!(budget.charge_n(4), Ok(()));
    assert_eq!(budget.charge_n(7), Err(exceeded(10)));
    assert_eq!(budget.spent(), 10);
    assert_eq!(budget.charge(), Err(exceeded(10)));
    assert_eq!(budget.charge_n(0), Ok(()));
    assert_eq!(budget.charge_n(usize::MAX), Err(exceeded(10)));
}

/// Applying `<b position="inside"><c/></b>`: locating `<b>` visits and compares form, a, b (6
/// steps), the depth check walks form, a, b, c (4).
#[test]
fn depth_check_after_a_spec_is_charged() {
    let (limits, mut budget) = budget_of(10);
    let mut source = arch("<form><a/><b/></form>");
    source.set_caps(Caps::combined(&limits));
    let spec = arch(r#"<b position="inside"><c/></b>"#);
    let applied =
        apply_inheritance_specs(source.clone(), &spec, &limits, &mut budget).map(|a| a.to_xml());
    assert_eq!(applied, Ok("<form><a/><b><c/></b></form>".to_owned()));
    assert_eq!(budget.spent(), 10);
    let (limits, mut short) = budget_of(9);
    assert_eq!(
        apply_inheritance_specs(source, &spec, &limits, &mut short).map(|a| a.to_xml()),
        Err(exceeded(9))
    );
}

/// `position="attributes"` charges the attribute scan, every token of the current value and of
/// `remove`/`add`, and their bytes.
#[test]
fn attribute_edits_are_charged_by_tokens() {
    let (limits, mut budget) = budget_of(1_000);
    let mut source = arch(r#"<form><f class="a b c"/></form>"#);
    source.set_caps(Caps::combined(&limits));
    let spec = arch(
        r#"<f position="attributes"><attribute name="class" separator=" " remove="b" add="d"/></f>"#,
    );
    let applied = apply_inheritance_specs(source, &spec, &limits, &mut budget).map(|a| a.to_xml());
    assert_eq!(applied, Ok(r#"<form><f class="a c d"/></form>"#.to_owned()));
    // Locate: form and f, a visit and a tag comparison each (4; `position` is not a selector).
    // Edit: scan for `class` (1), bytes of "a b c" + "d" + "b" (1), one remove token (1), three
    // current tokens (3), one add token (1), the set (1 + one attribute). Depth check: 2.
    assert_eq!(budget.spent(), 4 + 1 + 1 + 1 + 3 + 1 + 2 + 2);
}
