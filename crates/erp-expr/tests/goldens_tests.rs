//! CPython 3.12 evaluation goldens.

// Test helpers outside `#[test]` functions fail loudly too (clippy.toml only covers the tests).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "data/cpython_goldens.rs"]
mod goldens;

use erp_expr::{EvalContext, eval};

#[test]
fn cpython_goldens_match() {
    let ctx = EvalContext::standard();
    let mut failures = Vec::new();
    for &(expr, evaluates, expected) in goldens::GOLDENS {
        let got = match eval(expr, &ctx) {
            Ok(v) if evaluates => v.repr(),
            Ok(v) => format!("<evaluated to {}>", v.repr()),
            Err(e) if !evaluates => e.to_string(),
            Err(e) => format!("<error {e}>"),
        };
        if got != expected {
            failures.push(format!(
                "{expr}\n    expected: {expected}\n    got:      {got}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} goldens differ:\n{}",
        failures.len(),
        goldens::GOLDENS.len(),
        failures.join("\n")
    );
    assert_eq!(goldens::GOLDENS.len(), 482, "golden table size is pinned");
}
