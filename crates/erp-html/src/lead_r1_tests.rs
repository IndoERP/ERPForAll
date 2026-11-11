//! Regression tests for the lead review round 1 (F1, F2, F3, N2): output re-admission under lowered
//! limits, work across the two passes, `var()` inside a resource function, a node appended to itself.

use std::sync::Arc;

use html5ever::tree_builder::{NodeOrText, TreeSink};
use html5ever::{QualName, local_name, ns};

use crate::budget::HtmlBudget;
use crate::dom::DomSink;
use crate::{DefaultPolicy, HtmlSanitizeError, Limits, SanitizeMode, SanitizePolicy, sanitize};

fn run(input: &str, limits: &Limits) -> Result<String, HtmlSanitizeError> {
    sanitize(
        input,
        &SanitizePolicy::Default(DefaultPolicy::new()),
        limits,
        SanitizeMode::Clean,
    )
    .map(|r| r.into_output().as_str().to_owned())
}

fn raw(style: &str) -> String {
    run(&format!("<p style=\"{style}\">x</p>"), &Limits::ceiling()).expect("sanitises")
}

#[test]
fn f1_output_longer_than_the_input_limit_is_refused() {
    let limits = Limits::ceiling().with_input_bytes(1000).expect("limit");
    let x = "&".repeat(900);
    assert!(
        matches!(
            run(&x, &limits),
            Err(HtmlSanitizeError::OutputTooLarge { limit: 1000 })
        ),
        "the 900-byte input must be refused as OutputTooLarge"
    );
}

#[test]
fn f1_a_successful_output_is_accepted_again_under_the_same_limits() {
    let limits = Limits::ceiling().with_input_bytes(1000).expect("limit");
    for n in [1, 10, 100, 150, 190, 200, 250, 400, 900] {
        for unit in ["&", "<", "a&b", "<p>&</p>"] {
            let x = unit.repeat(n);
            if x.len() > 1000 {
                continue;
            }
            if let Ok(y) = run(&x, &limits) {
                assert!(
                    run(&y, &limits).is_ok_and(|z| z == y),
                    "n={n} unit={unit:?}"
                );
            }
        }
    }
}

#[test]
fn f2_work_limit_that_admits_x_also_admits_its_output() {
    for x in [
        "&".repeat(900),
        "<".repeat(300),
        "<p style=\"color:red\">a &amp; b</p>".repeat(20),
    ] {
        // Smallest work limit under which sanitise(x) succeeds (success is monotone in the limit).
        let ok = |w: u64| {
            let limits = Limits::ceiling().with_work_units(w).expect("limit");
            run(&x, &limits)
        };
        let (mut lo, mut hi) = (1_u64, 50_000_000_u64);
        assert!(ok(hi).is_ok());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if ok(mid).is_ok() {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        for w in [lo, lo + 1, lo + 7] {
            let limits = Limits::ceiling().with_work_units(w).expect("limit");
            let y = run(&x, &limits).expect("x succeeds at its threshold");
            assert!(run(&y, &limits).is_ok_and(|z| z == y), "work_units={w}");
        }
    }
}

#[test]
fn f2_threshold_for_a_growing_input_is_the_same_for_its_output() {
    // The output of this input is larger than the input, so its pass costs more work.
    let x = "&".repeat(900);
    let y = run(&x, &Limits::ceiling()).expect("sanitises");
    assert!(y.len() > x.len());
    let min_ok = |s: &str| {
        let (mut lo, mut hi) = (1_u64, 50_000_000_u64);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let limits = Limits::ceiling().with_work_units(mid).expect("limit");
            if run(s, &limits).is_ok() {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    };
    assert_eq!(min_ok(&x), min_ok(&y));
}

/// A resource function whose argument is substituted by the browser is refused.
#[test]
fn f3_var_env_attr_inside_a_resource_function_drop_the_declaration() {
    for decl in [
        "background-image:src(var(--u))",
        "background-image:image(var(--u))",
        "background-image:url(var(--u))",
        "background-image:src(env(x))",
        "background-image:image(env(x))",
        "background-image:src(attr(data-u))",
        "background-image:image(attr(data-u))",
        "background-image:src( VAR (--u))",
        "background-image:src(if(media(width>0):&quot;file://h/s&quot;;else:&quot;a&quot;))",
        "background-image:image(inherit(--u))",
        "background-image:src(&quot;)&quot;var(--u))",
    ] {
        // `&quot;` is a double quote once the attribute value is decoded.
        let out = raw(&format!("color:red;--u:'file://h/s';{decl}"));
        assert!(out.contains("color:red"), "{decl}: {out}");
        assert!(
            !out.contains("src(") && !out.contains("image(") && !out.contains("url("),
            "{decl}: {out}"
        );
    }
}

#[test]
fn f3_harmless_var_declarations_are_kept() {
    let out = raw("color:var(--c);width:calc(1px + env(safe-area-inset-top))");
    assert!(out.contains("color:var(--c)"), "{out}");
    assert!(out.contains("env(safe-area-inset-top)"), "{out}");
    let out = raw("background-image:url(https://example.com/a.png)");
    assert!(out.contains("url(https://example.com/a.png)"), "{out}");
}

/// A node appended to itself is an internal fault, not a loop until a limit.
#[test]
fn n2_appending_a_childless_node_to_itself_is_an_internal_fault() {
    let budget = HtmlBudget::new(&Limits::ceiling());
    let sink = DomSink::new(Arc::clone(&budget));
    let node = sink.create_element(
        QualName::new(None, ns!(html), local_name!("div")),
        Vec::new(),
        html5ever::tree_builder::ElementFlags::default(),
    );
    sink.append(&node, NodeOrText::AppendNode(node));
    assert!(
        matches!(budget.failure(), Some(HtmlSanitizeError::Internal { .. })),
        "{:?}",
        budget.failure()
    );
}
