//! Unit tests of the budget, the arena sink and the bounded pipeline (
//! steps 4 to 7). Counter values are read back from our own budget after a full run (parse, clean,
//! serialise) at the ceilings, never typed in as literals; each limit is then checked at `n` and `n - 1`.

use std::sync::Arc;

use html5ever::budget::{Budget, BudgetExceeded, BudgetKind};
use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElemName, NodeOrText, TreeSink};

use crate::budget::{HtmlBudget, map_budget_error};
use crate::dom::{Dom, DomSink, NodeId, NodeKind};
use crate::parse::parse_fragment_bounded;
use crate::{DefaultPolicy, HtmlSanitizeError, Limits, SanitizeMode, SanitizePolicy, sanitize};

fn parse_with(input: &str, limits: &Limits) -> (Result<Dom, HtmlSanitizeError>, Arc<HtmlBudget>) {
    let budget = HtmlBudget::new(limits);
    let dom = parse_fragment_bounded(input, &budget);
    (dom, budget)
}

/// What a parse of `input` used at the ceilings.
struct Measured {
    nodes: u64,
    work: u64,
    bytes: u64,
    depth: u64,
    /// The work of the second pass alone.
    second_pass_work: u64,
}

fn measure(input: &str) -> Measured {
    let limits = Limits::ceiling();
    let budget = HtmlBudget::new(&limits);
    let policy = SanitizePolicy::Default(DefaultPolicy::new());
    crate::run_pipeline(input, &policy, &limits, &budget)
        .expect("the input is sanitised at the ceilings");
    Measured {
        nodes: budget.nodes(),
        work: budget.work(),
        bytes: budget.peak_bytes(),
        depth: budget.max_depth(),
        second_pass_work: budget.last_pass_work(),
    }
}

fn run(input: &str, limits: &Limits) -> Result<String, HtmlSanitizeError> {
    let policy = SanitizePolicy::Default(DefaultPolicy::new());
    sanitize(input, &policy, limits, SanitizeMode::Clean)
        .map(|r| r.into_output().as_str().to_owned())
}

const LIMIT_INPUTS: [&str; 4] = [
    "<p>a</p><p>b</p>",
    "<b>x</b><!--c-->y",
    "<ul><li>a<li>b</ul>",
    "<p a=1 a=2 b=3 class=\"k\">text &amp; more</p>",
];

#[test]
fn node_limit_admits_exactly_the_measured_count() {
    for html in LIMIT_INPUTS {
        let n = measure(html).nodes;
        assert!(n >= 2, "{html:?}");
        let at = Limits::ceiling().with_nodes(n).expect("n is positive");
        assert!(run(html, &at).is_ok(), "{html:?} with exactly {n} nodes");
        let below = Limits::ceiling()
            .with_nodes(n - 1)
            .expect("n - 1 is positive");
        let err = run(html, &below).expect_err("one node fewer is refused");
        assert_eq!(
            err,
            HtmlSanitizeError::NodeLimit { limit: n - 1 },
            "{html:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("document exceeds the node limit of {}", n - 1)
        );
    }
}

#[test]
fn working_byte_limit_admits_exactly_the_measured_peak() {
    for html in LIMIT_INPUTS {
        let bytes = measure(html).bytes;
        assert!(bytes >= 2, "{html:?}");
        let at = Limits::ceiling()
            .with_working_bytes(bytes)
            .expect("positive");
        assert!(
            run(html, &at).is_ok(),
            "{html:?} with exactly {bytes} bytes"
        );
        let below = Limits::ceiling()
            .with_working_bytes(bytes - 1)
            .expect("positive");
        let err = run(html, &below).expect_err("one byte fewer is refused");
        assert_eq!(
            err,
            HtmlSanitizeError::MemoryLimit { limit: bytes - 1 },
            "{html:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "sanitising exceeds the working memory limit of {} bytes",
                bytes - 1
            )
        );
    }
}

#[test]
fn work_limit_admits_exactly_the_measured_units() {
    for html in LIMIT_INPUTS {
        // The smallest limit that admits the call: the total, and twice the second pass so that
        // the output can be sanitised again (`HtmlBudget::admit_repeat_work`).
        let measured = measure(html);
        let work = measured.work.max(measured.second_pass_work * 2);
        assert!(work >= 2, "{html:?}");
        let at = Limits::ceiling().with_work_units(work).expect("positive");
        assert!(run(html, &at).is_ok(), "{html:?} with exactly {work} units");
        let below = Limits::ceiling()
            .with_work_units(work - 1)
            .expect("positive");
        let err = run(html, &below).expect_err("one unit fewer is refused");
        assert_eq!(
            err,
            HtmlSanitizeError::WorkLimit { limit: work - 1 },
            "{html:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("sanitising exceeds the work limit of {} units", work - 1)
        );
    }
}

#[test]
fn work_counts_more_than_the_input_length() {
    // Attribute comparisons and tree operations add to the input bytes charged up front.
    let html = "<p a=1 a=2 b=3>x</p>";
    assert!(measure(html).work > html.len() as u64);
}

/// The context element and the generated `html` root are the two nodes of an empty parse; each
/// text, comment and element then adds one.
#[test]
fn text_comment_and_element_each_raise_the_node_count() {
    // One parse only: the full pipeline parses the output a second time (the reparse gate).
    let nodes = |input: &str| {
        let (dom, budget) = parse_with(input, &Limits::ceiling());
        dom.expect("parses");
        budget.nodes()
    };
    let base = nodes("");
    assert_eq!(base, 2, "context element plus generated root");
    assert_eq!(nodes("x"), base + 1, "text");
    assert_eq!(nodes("<!--c-->"), base + 1, "comment");
    assert_eq!(nodes("<p></p>"), base + 1, "element");
    assert_eq!(nodes("a<!--c-->b"), base + 3, "text, comment, text");
    assert_eq!(nodes("ab"), base + 1, "adjacent text is one node");
}

#[test]
fn doctype_raises_the_node_count() {
    let budget = HtmlBudget::new(&Limits::ceiling());
    let sink = DomSink::new(Arc::clone(&budget));
    let before = budget.nodes();
    sink.append_doctype_to_document("html".into(), StrTendril::new(), StrTendril::new());
    assert_eq!(budget.nodes(), before + 1);
    let dom = sink.finish();
    let children = dom.children(dom.document());
    assert_eq!(children.len(), 1);
    assert!(matches!(dom.kind(children[0]), Some(NodeKind::Doctype)));

    // At a node limit of zero remaining the doctype is refused, not stored.
    let limits = Limits::ceiling().with_nodes(1).expect("positive");
    let budget = HtmlBudget::new(&limits);
    assert!(budget.admit_node().is_ok());
    let sink = DomSink::new(Arc::clone(&budget));
    sink.append_doctype_to_document("html".into(), StrTendril::new(), StrTendril::new());
    assert_eq!(
        budget.failure(),
        Some(HtmlSanitizeError::NodeLimit { limit: 1 })
    );
    let dom = sink.finish();
    assert!(dom.children(dom.document()).is_empty());
}

#[test]
fn a_node_limit_of_one_is_spent_on_the_context_element() {
    let limits = Limits::ceiling().with_nodes(1).expect("positive");
    let (dom, _) = parse_with("", &limits);
    assert_eq!(
        dom.expect_err("the root is refused"),
        HtmlSanitizeError::NodeLimit { limit: 1 }
    );
}

#[test]
fn a_working_byte_limit_of_one_is_a_typed_refusal() {
    let limits = Limits::ceiling().with_working_bytes(1).expect("positive");
    let (dom, _) = parse_with("<p>x</p>", &limits);
    assert_eq!(
        dom.expect_err("memory is refused"),
        HtmlSanitizeError::MemoryLimit { limit: 1 }
    );
}

/// Depth values the fork reports: the generated `html` root is depth 1, so `n` nested elements
/// reach `n + 1`, and the first refused push asks for `n + 1` against the limit of 256.
#[test]
fn observed_depths_at_the_ceiling() {
    let nested = |n: usize| format!("{}x{}", "<div>".repeat(n), "</div>".repeat(n));
    for n in [254usize, 255] {
        assert_eq!(measure(&nested(n)).depth, n as u64 + 1, "n={n}");
    }
    for n in [256usize, 257] {
        let (dom, budget) = parse_with(&nested(n), &Limits::ceiling());
        assert_eq!(
            dom.expect_err("refused"),
            HtmlSanitizeError::DepthExceeded { limit: 256 }
        );
        let refusal = budget.tripped().expect("sticky refusal");
        assert_eq!(refusal.kind, BudgetKind::Depth);
        assert_eq!((refusal.requested, refusal.limit), (257, 256), "n={n}");
        assert_eq!(budget.max_depth(), 256, "the deepest admitted depth, n={n}");
    }
}

const SWEEP_INPUTS: [&str; 12] = [
    "<table><tr><td><table><tr><td>x</td></tr></table></td></tr></table>",
    "<b><i><p>x</b>y</i>z",
    "<a href=u><div>x</a>y",
    "<select><option>a<option>b</select>",
    "<template><p>x</p><template>y</template></template>",
    "<svg><foreignObject><p>x</p></foreignObject></svg><math><mi>y</mi></math>",
    "a &amp; b &#65; &#x41; &#",
    "<p>&#",
    "<!--c--><p>x<!--d--></p><!DOCTYPE html>",
    "plain text only",
    "<table> x <b>y</b></table>",
    "<p a=1 a=2 b=3 class=k>x</p><ul><li>a<li>b</ul>",
];

/// Refuse the k-th admission call for every k: the parse ends with a typed limit error, never a
/// panic and never a success, whatever the parser was in the middle of (adoption agency, table
/// foster parenting, character references, templates, foreign content).
#[test]
fn refusing_the_kth_admission_is_always_a_typed_error() {
    for html in SWEEP_INPUTS {
        let budget = HtmlBudget::new(&Limits::ceiling());
        parse_fragment_bounded(html, &budget).expect("parses without a refusal");
        let total = budget.calls();
        assert!(total >= 10, "{html:?} made only {total} admission calls");
        for k in 1..=total {
            let budget = HtmlBudget::new(&Limits::ceiling());
            budget.refuse_call(k);
            let err = parse_fragment_bounded(html, &budget)
                .expect_err("a refused admission fails the parse");
            assert!(
                matches!(
                    err,
                    HtmlSanitizeError::DepthExceeded { .. }
                        | HtmlSanitizeError::NodeLimit { .. }
                        | HtmlSanitizeError::WorkLimit { .. }
                        | HtmlSanitizeError::MemoryLimit { .. }
                ),
                "{html:?} call {k} of {total}: {err:?}"
            );
        }
    }
}

#[test]
fn accounting_faults_are_internal_not_limits() {
    let err = map_budget_error(BudgetExceeded::new(BudgetKind::Accounting, 11, 10));
    assert_eq!(
        err,
        HtmlSanitizeError::Internal {
            reason: "budget accounting fault"
        }
    );
    assert_eq!(
        err.to_string(),
        "internal sanitiser error: budget accounting fault"
    );

    // A release larger than the live charge is recorded, refuses further charges and surfaces as
    // the same internal error (no panic in any build profile).
    let budget = HtmlBudget::new(&Limits::ceiling());
    assert!(budget.admit_bytes(10).is_ok());
    budget.release_bytes(11);
    assert_eq!(budget.failure(), Some(err));
    assert!(budget.admit_bytes(1).is_err());
}

#[test]
fn limit_kinds_map_to_their_own_errors() {
    let e = |kind| map_budget_error(BudgetExceeded::new(kind, 5, 4));
    assert_eq!(
        e(BudgetKind::Depth),
        HtmlSanitizeError::DepthExceeded { limit: 4 }
    );
    assert_eq!(
        e(BudgetKind::Nodes),
        HtmlSanitizeError::NodeLimit { limit: 4 }
    );
    assert_eq!(
        e(BudgetKind::Work),
        HtmlSanitizeError::WorkLimit { limit: 4 }
    );
    assert_eq!(
        e(BudgetKind::WorkingBytes),
        HtmlSanitizeError::MemoryLimit { limit: 4 }
    );
}

#[test]
fn the_refusal_is_sticky_and_counters_do_not_move_after_it() {
    let limits = Limits::ceiling().with_nodes(2).expect("positive");
    let budget = HtmlBudget::new(&limits);
    assert!(budget.admit_node().is_ok());
    assert!(budget.admit_node().is_ok());
    let first = budget.admit_node().expect_err("third node");
    assert_eq!(first.kind, BudgetKind::Nodes);
    // Everything is refused now, with the same first refusal, and nothing is counted.
    assert_eq!(budget.admit_bytes(1), Err(first));
    assert_eq!(budget.admit_work(1), Err(first));
    assert_eq!(budget.admit_depth(1), Err(first));
    assert_eq!(budget.admit_node(), Err(first));
    assert_eq!(budget.nodes(), 2);
    assert_eq!(budget.tripped(), Some(first));
}

#[test]
fn counters_saturate_instead_of_wrapping() {
    let budget = HtmlBudget::new(&Limits::ceiling());
    let err = budget
        .admit_work(u64::MAX)
        .expect_err("far above the limit");
    assert_eq!(err.kind, BudgetKind::Work);
    assert_eq!(err.requested, u64::MAX);
    let budget = HtmlBudget::new(&Limits::ceiling());
    let err = budget
        .admit_bytes(usize::MAX)
        .expect_err("far above the limit");
    assert_eq!(err.kind, BudgetKind::WorkingBytes);
}

#[test]
fn parse_errors_are_counted_not_stored() {
    let html = "</x>".repeat(2000);
    let (dom, _) = parse_with(&html, &Limits::ceiling());
    let dom = dom.expect("stray end tags parse");
    assert!(dom.parse_errors() >= 2000);
}

/// After a refusal the parser still calls the sink with the document stand-in handle. The sink
/// answers for it, forwards nothing, and does not mistake it for a fault.
#[test]
fn sink_after_refusal_answers_stand_ins_and_stores_nothing() {
    let limits = Limits::ceiling().with_nodes(1).expect("positive");
    let budget = HtmlBudget::new(&limits);
    let sink = DomSink::new(Arc::clone(&budget));
    assert!(budget.admit_node().is_ok());
    assert!(
        budget.admit_node().is_err(),
        "the second node trips the budget"
    );

    let doc = sink.get_document();
    let name = sink.elem_name(&doc);
    assert!(name.ns().is_empty());
    assert!(name.local_name().is_empty());
    assert!(sink.same_node(&sink.get_template_contents(&doc), &doc));
    sink.append(&doc, NodeOrText::AppendText("x".into()));
    sink.append_before_sibling(&doc, NodeOrText::AppendText("y".into()));
    sink.append_doctype_to_document("html".into(), StrTendril::new(), StrTendril::new());
    sink.remove_from_parent(&doc);
    sink.reparent_children(&doc, &doc);
    assert_eq!(
        budget.failure(),
        Some(HtmlSanitizeError::NodeLimit { limit: 1 }),
        "no internal fault was recorded"
    );
    let dom = sink.finish();
    assert!(dom.children(dom.document()).is_empty());
}

/// A handle that names no node never panics; it is recorded as an internal fault.
#[test]
fn sink_invalid_handles_are_an_internal_fault_not_a_panic() {
    type Call = fn(&DomSink, &NodeId);
    let calls: [(&str, Call); 7] = [
        ("elem_name", |s, bad| {
            let _ = s.elem_name(bad);
        }),
        ("get_template_contents", |s, bad| {
            let _ = s.get_template_contents(bad);
        }),
        ("append", |s, bad| {
            s.append(bad, NodeOrText::AppendText("x".into()));
        }),
        ("append_node", |s, bad| {
            s.append(&s.get_document(), NodeOrText::AppendNode(*bad));
        }),
        ("append_before_sibling", |s, bad| {
            s.append_before_sibling(bad, NodeOrText::AppendText("x".into()));
        }),
        ("remove_from_parent", |s, bad| s.remove_from_parent(bad)),
        ("reparent_children", |s, bad| {
            s.reparent_children(bad, &s.get_document());
        }),
    ];
    for (name, call) in calls {
        let budget = HtmlBudget::new(&Limits::ceiling());
        let sink = DomSink::new(Arc::clone(&budget));
        call(&sink, &NodeId::from_raw(9999));
        assert!(
            matches!(budget.failure(), Some(HtmlSanitizeError::Internal { .. })),
            "{name}: {:?}",
            budget.failure()
        );
    }
}

#[test]
fn document_children_hold_the_generated_root() {
    let (dom, _) = parse_with("<p>a</p>b", &Limits::ceiling());
    let dom = dom.expect("parses");
    let root = dom.children(dom.document());
    assert_eq!(root.len(), 1);
    match dom.kind(root[0]) {
        Some(NodeKind::Element { name, .. }) => assert_eq!(&*name.local, "html"),
        other => panic!("expected the html root, got {other:?}"),
    }
    assert_eq!(dom.children(root[0]).len(), 2, "p and the text");
}
