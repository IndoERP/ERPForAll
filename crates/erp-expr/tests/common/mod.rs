//! Helpers shared by the erp-expr integration tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use erp_expr::{EvalContext, ExprError, ExprValue, eval};
use time::{Date, Month, PrimitiveDateTime, Time};

/// The fixed instant the test clock returns: 2026-10-15 12:34:56.
pub fn fixed_now() -> PrimitiveDateTime {
    PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 15).expect("valid date"),
        Time::from_hms(12, 34, 56).expect("valid time"),
    )
}

pub fn ctx() -> EvalContext {
    EvalContext::standard()
}

pub fn ok_in(ctx: &EvalContext, src: &str) -> ExprValue {
    eval(src, ctx).unwrap_or_else(|e| panic!("`{src}` failed: {e}"))
}

pub fn ok(src: &str) -> ExprValue {
    ok_in(&ctx(), src)
}

/// `repr()` of the result, the easiest form to compare with Python.
pub fn repr(src: &str) -> String {
    ok(src).repr()
}

pub fn err_in(ctx: &EvalContext, src: &str) -> ExprError {
    match eval(src, ctx) {
        Ok(v) => panic!("`{src}` should fail, evaluated to {v:?}"),
        Err(e) => e,
    }
}

pub fn err(src: &str) -> ExprError {
    err_in(&ctx(), src)
}

/// Asserts the error class and the full message (`Display` is `"<Class>: <message>"`).
pub fn assert_err_in(ctx: &EvalContext, src: &str, class: &str, expected: &str) {
    let e = err_in(ctx, src);
    assert_eq!(e.class(), class, "`{src}`: wrong error class in {e}");
    assert_eq!(e.to_string(), expected, "`{src}`: wrong message");
}

pub fn assert_err(src: &str, class: &str, expected: &str) {
    assert_err_in(&ctx(), src, class, expected);
}

/// A string of `n` ASCII letters.
pub fn big_string(n: usize) -> ExprValue {
    ExprValue::str("a".repeat(n))
}
