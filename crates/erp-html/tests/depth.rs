//! Depth boundaries of the bounded parse (step 5).
//!
//! The parse starts from a generated `html` root at depth 1, so `n` nested user elements reach
//! depth `n + 1`. With the ceiling of 256, 255 nested elements are accepted and 256 are refused.

use erp_html::{DefaultPolicy, HtmlSanitizeError, Limits, SanitizeMode, SanitizePolicy, sanitize};

fn run(input: &str, limits: &Limits) -> Result<String, HtmlSanitizeError> {
    let policy = SanitizePolicy::Default(DefaultPolicy::new());
    sanitize(input, &policy, limits, SanitizeMode::Clean)
        .map(|r| r.into_output().as_str().to_owned())
}

fn nested(n: usize) -> String {
    format!("{}sentinel{}", "<div>".repeat(n), "</div>".repeat(n))
}

/// Count `needle` in `haystack` by scanning, not with `str::matches`.
fn count(haystack: &str, needle: &str) -> usize {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    let mut found = 0;
    let mut i = 0;
    while i + n.len() <= h.len() {
        if &h[i..i + n.len()] == n {
            found += 1;
            i += n.len();
        } else {
            i += 1;
        }
    }
    found
}

fn depth_error(limit: u64) -> HtmlSanitizeError {
    HtmlSanitizeError::DepthExceeded { limit }
}

const DEPTH_MESSAGE_256: &str = "element nesting exceeds the depth limit of 256";

#[test]
fn n_254_and_255_succeed_at_the_ceiling() {
    for n in [254usize, 255] {
        let out = run(&nested(n), &Limits::ceiling()).expect("within the depth limit");
        assert_eq!(
            count(&out, "<div>"),
            n,
            "opening div tags in the output, n={n}"
        );
        assert_eq!(count(&out, "</div>"), n);
        assert_eq!(count(&out, "sentinel"), 1);
        assert_eq!(out, nested(n));
    }
}

/// Leading text makes the cleaner wrap the output in a `<p>`, one level deeper than the input, and
/// the output must itself be acceptable input (`sanitize(y) == y`). So text-led
/// input accepts 254 nested user elements and refuses 255 and more; input that does not start with
/// text keeps the 255/256/257 contract.
#[test]
fn leading_text_wrapper_lowers_the_accepted_depth_by_one() {
    let text_nested = |n: usize| "x<b>".repeat(n);
    let out = run(&text_nested(254), &Limits::ceiling()).expect("within the depth limit");
    assert_eq!(count(&out, "<b>"), 254);
    assert!(out.starts_with("<p>x<b>"));
    assert_eq!(
        run(&out, &Limits::ceiling()).expect("the output is acceptable input"),
        out,
        "idempotent at the text-led boundary"
    );
    for n in [255usize, 256, 257] {
        let err = run(&text_nested(n), &Limits::ceiling()).expect_err("over the depth limit");
        assert_eq!(err, depth_error(256), "n={n}");
        assert_eq!(err.to_string(), DEPTH_MESSAGE_256);
    }
    // Not text-led: 255 stays accepted and its output is stable.
    let out = run(&nested(255), &Limits::ceiling()).expect("within the depth limit");
    assert_eq!(run(&out, &Limits::ceiling()).expect("stable"), out);
    for n in [256usize, 257] {
        assert_eq!(
            run(&nested(n), &Limits::ceiling()).expect_err("over the depth limit"),
            depth_error(256),
            "n={n}"
        );
    }
    let limits = Limits::ceiling().with_depth(8).expect("below the ceiling");
    let out = run(&text_nested(6), &limits).expect("within the lowered limit");
    assert_eq!(run(&out, &limits).expect("stable"), out);
    for n in [7usize, 8] {
        assert_eq!(
            run(&text_nested(n), &limits).expect_err("over the lowered limit"),
            depth_error(8),
            "n={n}"
        );
    }
}

#[test]
fn leading_text_wrapper_check_is_conservative_for_block_children() {
    let text_div = |n: usize| format!("x{}", "<div>".repeat(n));
    // The wrapper would not deepen this output (the divs follow the paragraph), but the check
    // measures the parsed tree, so 255 is refused and 254 is accepted. Fail closed.
    let out = run(&text_div(254), &Limits::ceiling()).expect("within the depth limit");
    assert_eq!(run(&out, &Limits::ceiling()).expect("stable"), out);
    let err = run(&text_div(255), &Limits::ceiling()).expect_err("conservative refusal");
    assert_eq!(err, depth_error(256));
    assert_eq!(err.to_string(), DEPTH_MESSAGE_256);
}

#[test]
fn n_256_and_257_are_refused_at_the_ceiling() {
    for n in [256usize, 257] {
        let err = run(&nested(n), &Limits::ceiling()).expect_err("over the depth limit");
        assert_eq!(err, depth_error(256), "n={n}");
        assert_eq!(err.to_string(), DEPTH_MESSAGE_256);
    }
}

#[test]
fn lowered_depth_of_8_accepts_6_and_7_and_refuses_8() {
    let limits = Limits::ceiling()
        .with_depth(8)
        .expect("8 is below the ceiling");
    for n in [6usize, 7] {
        let out = run(&nested(n), &limits).expect("within the depth limit");
        assert_eq!(count(&out, "<div>"), n);
        assert_eq!(count(&out, "sentinel"), 1);
    }
    let err = run(&nested(8), &limits).expect_err("over the depth limit");
    assert_eq!(err, depth_error(8));
    assert_eq!(
        err.to_string(),
        "element nesting exceeds the depth limit of 8"
    );
}

/// Implicit elements count: `<table><tr><td>` opens `table`, `tbody`, `tr` and `td`, so four
/// levels per nesting although the input writes three tags. The limit is reached sooner than the
/// tag count suggests, and the result is a refusal, never a truncated tree (fails closed).
#[test]
fn implicit_table_elements_reach_the_limit_sooner() {
    let table = |levels: usize| format!("{}x", "<table><tr><td>".repeat(levels));
    // 63 levels: 1 + 63 * 4 = 253 deep, accepted.
    let out = run(&table(63), &Limits::ceiling()).expect("within the depth limit");
    assert!(out.contains('x'));
    assert_eq!(
        count(&out, "<tbody>"),
        63,
        "the tbody elements are implicit in the input"
    );
    // 64 levels: 192 written tags, yet 1 + 64 * 4 = 257 deep.
    let input = table(64);
    assert_eq!(count(&input, "<"), 192);
    let err = run(&input, &Limits::ceiling()).expect_err("implicit elements reach depth 257");
    assert_eq!(err, depth_error(256));
    assert_eq!(err.to_string(), DEPTH_MESSAGE_256);
}
