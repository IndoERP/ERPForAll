//! Property tests: integer arithmetic against an `i128` reference of Python's semantics, nesting of
//! every construct around the depth limit, float `repr` round trips, and fuzzed sources. None of
//! them may panic; every failure must be a typed error.

// Test helpers outside `#[test]` functions fail loudly too (clippy.toml only covers the tests).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use erp_expr::{EvalContext, EvalLimits, ExprError, ExprValue, eval};
use proptest::prelude::*;

fn lit(i: i64) -> String {
    if i == i64::MIN {
        "(-9223372036854775807-1)".to_string()
    } else if i < 0 {
        format!("({i})")
    } else {
        i.to_string()
    }
}

fn fit(r: i128) -> Result<i64, &'static str> {
    i64::try_from(r).map_err(|_| "OverflowError")
}

/// Python's result for `a <op> b` with 64-bit checked integers.
fn reference(op: &str, a: i64, b: i64) -> Result<i64, &'static str> {
    let (x, y) = (i128::from(a), i128::from(b));
    let floor_div = |x: i128, y: i128| {
        let q = x / y;
        if x % y != 0 && ((x < 0) != (y < 0)) {
            q - 1
        } else {
            q
        }
    };
    match op {
        "+" => fit(x + y),
        "-" => fit(x - y),
        "*" => fit(x * y),
        "//" => {
            if b == 0 {
                Err("ZeroDivisionError")
            } else {
                fit(floor_div(x, y))
            }
        }
        "%" => {
            if b == 0 {
                Err("ZeroDivisionError")
            } else {
                fit(x - y * floor_div(x, y))
            }
        }
        "**" => {
            let mut acc: i128 = 1;
            for _ in 0..b.min(200) {
                acc *= x;
                if acc.abs() > i128::from(i64::MAX) + 1 {
                    return Err("OverflowError");
                }
            }
            if b > 200 && x.abs() > 1 {
                return Err("OverflowError");
            }
            if b > 200 && x == -1 {
                return Ok(if b % 2 == 0 { 1 } else { -1 });
            }
            fit(acc)
        }
        "<<" | ">>" => {
            if b < 0 {
                Err("ValueError")
            } else if b >= 64 {
                Err("OverflowError")
            } else if op == "<<" {
                fit(x << b)
            } else {
                Ok(a >> b)
            }
        }
        "&" => Ok(a & b),
        "|" => Ok(a | b),
        "^" => Ok(a ^ b),
        _ => unreachable!("unknown operator {op}"),
    }
}

fn interesting_i64() -> impl Strategy<Value = i64> {
    prop_oneof![
        any::<i64>(),
        -300i64..300,
        Just(i64::MIN),
        Just(i64::MAX),
        Just(i64::MIN + 1),
        Just(i64::MAX - 1),
        Just(0),
        Just(1),
        Just(-1),
        (-62i64..63).prop_map(|e| 1i64 << e.unsigned_abs().min(62)),
    ]
}

fn small_exponent() -> impl Strategy<Value = i64> {
    prop_oneof![-5i64..70, interesting_i64()]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn integer_binary_ops_match_python(
        a in interesting_i64(),
        b in interesting_i64(),
        e in small_exponent(),
        op in prop::sample::select(vec!["+", "-", "*", "//", "%", "**", "<<", ">>", "&", "|", "^"]),
    ) {
        let rhs = if matches!(op, "**" | "<<" | ">>") { e } else { b };
        let src = format!("{} {op} {}", lit(a), lit(rhs));
        let got = eval(&src, &EvalContext::standard());
        if op == "**" && rhs < 0 {
            // Negative exponents produce floats, or ZeroDivisionError for a zero base.
            match got {
                Ok(ExprValue::Float(f)) => prop_assert!(a != 0 && (f - (a as f64).powf(rhs as f64)).abs() <= f.abs() * 1e-12),
                Err(ExprError::ZeroDivision { .. }) => prop_assert_eq!(a, 0),
                other => prop_assert!(false, "{src}: unexpected {other:?}"),
            }
        } else {
            match (reference(op, a, rhs), got) {
                (Ok(want), Ok(v)) => prop_assert_eq!(v, ExprValue::Int(want), "{}", src),
                (Err(class), Err(e)) => prop_assert_eq!(e.class(), class, "{}: {}", src, e),
                (want, got) => prop_assert!(false, "{src}: want {want:?}, got {got:?}"),
            }
        }
    }

    #[test]
    fn unary_ops_and_abs_match_python(a in interesting_i64()) {
        let ctx = EvalContext::standard();
        let neg = eval(&format!("-{}", lit(a)), &ctx);
        match a.checked_neg() {
            Some(n) => prop_assert_eq!(neg.ok(), Some(ExprValue::Int(n))),
            None => prop_assert_eq!(neg.map_err(|e| e.class()), Err("OverflowError")),
        }
        let abs = eval(&format!("abs({})", lit(a)), &ctx);
        match a.checked_abs() {
            Some(n) => prop_assert_eq!(abs.ok(), Some(ExprValue::Int(n))),
            None => prop_assert_eq!(abs.map_err(|e| e.class()), Err("OverflowError")),
        }
        prop_assert_eq!(eval(&format!("~{}", lit(a)), &ctx).ok(), Some(ExprValue::Int(!a)));
        prop_assert_eq!(eval(&format!("int(str({}))", lit(a)), &ctx).ok(), Some(ExprValue::Int(a)));
    }

    #[test]
    fn float_floor_division_and_modulo_are_consistent(
        a in -1e6f64..1e6,
        b in prop_oneof![-1e3f64..-1e-3, 1e-3f64..1e3],
    ) {
        let ctx = EvalContext::standard().with_var("a", ExprValue::Float(a)).with_var("b", ExprValue::Float(b));
        let q = eval("a // b", &ctx).ok().and_then(|v| v.as_f64());
        let r = eval("a % b", &ctx).ok().and_then(|v| v.as_f64());
        let (Some(q), Some(r)) = (q, r) else { return Err(TestCaseError::fail("float // or % failed")) };
        prop_assert_eq!(q, q.floor());
        prop_assert!(r == 0.0 || (r < 0.0) == (b < 0.0), "remainder sign follows the divisor");
        prop_assert!((q * b + r - a).abs() <= 1e-6 * a.abs().max(1.0));
    }

    #[test]
    fn float_repr_round_trips(bits in any::<u64>()) {
        let x = f64::from_bits(bits);
        prop_assume!(x.is_finite());
        let ctx = EvalContext::standard().with_var("x", ExprValue::Float(x));
        let text = match eval("repr(x)", &ctx) {
            Ok(ExprValue::Str(s)) => s,
            other => return Err(TestCaseError::fail(format!("repr failed: {other:?}"))),
        };
        match eval(&text, &ctx) {
            Ok(ExprValue::Float(y)) => prop_assert_eq!(y.to_bits(), x.to_bits(), "{}", text),
            other => prop_assert!(false, "{text}: {other:?}"),
        }
    }
}

/// Wraps `inner` in `depth` levels of one construct.
fn nest(kind: usize, depth: usize) -> String {
    let mut s = "1".to_string();
    for _ in 0..depth {
        s = match kind {
            0 => format!("({s})"),
            1 => format!("-{s}"),
            2 => format!("not {s}"),
            3 => format!("[{s}]"),
            4 => format!("({s},)"),
            5 => format!("abs({s})"),
            6 => format!("{s} + 1"),
            7 => format!("1 ** {s}"),
            8 => format!("{{'k': {s}}}"),
            9 => format!("[{s}][0]"),
            10 => format!("{s} if 1 else 0"),
            11 => format!("[{s} for _ in [1]]"),
            12 => format!("str({s}).upper()"),
            13 => format!("f'{{{s}}}'"),
            _ => format!("({s} or 0)"),
        };
    }
    s
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn nesting_is_accepted_below_and_rejected_above_the_limit(kind in 0usize..15, depth in 0usize..260) {
        let r = eval(&nest(kind, depth), &EvalContext::standard());
        match r {
            Ok(_) => prop_assert!(depth <= 100, "kind {kind} depth {depth} must be rejected"),
            Err(ExprError::LimitExceeded { message }) => {
                prop_assert!(depth > 24, "kind {kind} depth {depth} rejected too early: {message}");
                prop_assert!(message.contains("depth limit of 100"), "{}", message);
            }
            Err(other) => prop_assert!(false, "kind {kind} depth {depth}: {other}"),
        }
    }

    #[test]
    fn lowered_depth_limits_hold_for_every_construct(kind in 0usize..15, limit in 1usize..40) {
        let ctx = EvalContext::standard().with_limits(EvalLimits::default().with_max_depth(limit));
        match eval(&nest(kind, limit + 1), &ctx) {
            Err(ExprError::LimitExceeded { message }) => prop_assert!(message.contains(&format!("limit of {limit}")), "{}", message),
            other => prop_assert!(false, "kind {kind} limit {limit}: {other:?}"),
        }
    }

    #[test]
    fn fuzzed_token_soup_never_panics(tokens in prop::collection::vec(prop::sample::select(vec![
        "(", ")", "[", "]", "{", "}", ",", ":", ".", "1", "0", "-1", "2.5", "'a'", "'é'", "f'{", "}'",
        "x", "s", "+", "-", "*", "**", "/", "//", "%", "<<", ">>", "&", "|", "^", "~", "not", "and",
        "or", "in", "is", "if", "else", "for", ":=", "==", "<", "len", "str", "Command", "[0]",
        "*10**6", "'%s'", "%", "lambda", "\\", "\n", "'''", "b'x'", "datetime", "relativedelta",
        "timedelta", "(days=", "999999999", "9223372036854775807",
    ]), 0..120)) {
        let src = tokens.join(" ");
        let ctx = EvalContext::standard()
            .with_var("x", ExprValue::list(vec![ExprValue::Int(1), ExprValue::str("é")]))
            .with_var("s", ExprValue::str("a".repeat(1000)));
        let _ = eval(&src, &ctx);
    }

    #[test]
    fn arbitrary_text_never_panics(src in "\\PC{0,300}") {
        let _ = eval(&src, &EvalContext::standard());
    }
}
