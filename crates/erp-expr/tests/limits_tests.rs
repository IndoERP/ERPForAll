//! Resource limits and the exploit strings of the reviews
//! (rounds 1 and 2). Every case failed before this
//! round's fixes (stack overflow, multi-gigabyte allocation, panic, or wrong result).

// Test helpers outside `#[test]` functions fail loudly too (clippy.toml only covers the tests).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use erp_expr::{EvalContext, EvalLimits, ExprValue, eval, eval_ast, parse, parse_with_limits};

/// Bytes of one value slot, used in the size messages.
fn slot() -> usize {
    ExprValue::Int(0).deep_size()
}

fn per_value_msg(bytes: usize) -> String {
    format!("LimitExceeded: value of {bytes} bytes exceeds the per-value limit of 1048576 bytes")
}

const DEPTH_MSG: &str = "LimitExceeded: expression nesting depth limit of 100 exceeded";

/// A context holding `s`, a 100 KB string, as in the review's amplification strings.
fn ctx_with_s() -> EvalContext {
    EvalContext::standard().with_var("s", big_string(100_000))
}

// ---------------------------------------------------------------------------------------------
// Round 2, blocking 1: memory amplification past the budget
// ---------------------------------------------------------------------------------------------

#[test]
fn repeat_of_a_list_of_big_strings_is_charged_by_deep_size() {
    // [('a'*10**6)]*10**6: one 1 MB string, a million references.
    let per_copy = slot() + 1_000_000;
    assert_err(
        "[('a'*10**6)]*10**6",
        "LimitExceeded",
        &per_value_msg(per_copy * 1_000_000 + slot()),
    );
}

#[test]
fn format_with_many_fields_stops_at_the_value_limit() {
    assert_err_in(
        &ctx_with_s(),
        "('{0}'*20000).format(s)",
        "LimitExceeded",
        "LimitExceeded: string exceeds the per-value limit of 1048576 bytes",
    );
}

#[test]
fn replace_computes_the_result_size_before_building() {
    assert_err_in(
        &ctx_with_s(),
        "s.replace('a', s)",
        "LimitExceeded",
        &per_value_msg(100_000 * 100_000),
    );
}

#[test]
fn printf_with_many_big_arguments_is_rejected_at_the_tuple_repeat() {
    let s_bytes = slot() + 100_000;
    assert_err_in(
        &ctx_with_s(),
        "'%s'*30000 % ((s,)*30000)",
        "LimitExceeded",
        &per_value_msg(s_bytes * 30_000 + slot()),
    );
}

#[test]
fn printf_output_is_capped_even_when_arguments_are_small() {
    // Arguments of a few bytes, 2 MB of padded output.
    assert_err(
        "'%1000s'*2000 % (('a',)*2000)",
        "LimitExceeded",
        "LimitExceeded: string exceeds the per-value limit of 1048576 bytes",
    );
}

#[test]
fn total_budget_is_charged_across_values() {
    // 20 lookups of a 900 KB string: each value is under 1 MiB, the total passes 16 MiB.
    let ctx = EvalContext::standard().with_var("s", big_string(900_000));
    let src = vec!["len(s)"; 20].join(" + ");
    assert_err_in(
        &ctx,
        &src,
        "LimitExceeded",
        "LimitExceeded: evaluation memory budget of 16777216 bytes exceeded",
    );
}

// ---------------------------------------------------------------------------------------------
// Round 2, blocking 2: slice step overflow
// ---------------------------------------------------------------------------------------------

#[test]
fn slice_with_extreme_step_does_not_overflow() {
    assert_eq!(repr("[1,2,3][1::9223372036854775807]"), "[2]");
    assert_eq!(repr("[1,2,3][::-9223372036854775807]"), "[3]");
    assert_eq!(
        repr("'abc'[-9223372036854775807:9223372036854775807:4611686018427387904]"),
        "'a'"
    );
    assert_eq!(repr("[1,2,3][::-9223372036854775807-1]"), "[3]");
    assert_err(
        "[1,2,3][::0]",
        "ValueError",
        "ValueError: slice step cannot be zero",
    );
}

// ---------------------------------------------------------------------------------------------
// Round 2, blocking 3: parser depth (nested f-strings, left-associative chains, comprehensions)
// ---------------------------------------------------------------------------------------------

fn nested_fstring(levels: usize) -> String {
    format!("{}1{}", "f'{".repeat(levels), "}'".repeat(levels))
}

#[test]
fn nested_fstrings_continue_the_parser_depth() {
    assert_eq!(ok(&nested_fstring(20)), ExprValue::str("1"));
    assert_err(&nested_fstring(200), "LimitExceeded", DEPTH_MSG);
}

#[test]
fn left_associative_chains_count_towards_the_tree_height() {
    let long_sum = vec!["1"; 10_000].join("+");
    assert_err(&long_sum, "LimitExceeded", DEPTH_MSG);
    let attrs = format!("a{}", ".b".repeat(10_000));
    assert_err(&attrs, "LimitExceeded", DEPTH_MSG);
    let calls = format!("f{}", "()".repeat(10_000));
    assert_err(&calls, "LimitExceeded", DEPTH_MSG);
    let subscripts = format!("x{}", "[0]".repeat(10_000));
    assert_err(&subscripts, "LimitExceeded", DEPTH_MSG);
    // Just under the limit: 99 terms make a tree of height 99, which evaluates.
    assert_eq!(ok(&vec!["1"; 99].join("+")), ExprValue::Int(99));
}

#[test]
fn comprehension_clauses_count_towards_the_tree_height() {
    let src = format!("[1 {}]", "for x in [1] ".repeat(1_000));
    assert_err(&src, "LimitExceeded", DEPTH_MSG);
    let ok_src = format!("[1 {}]", "for x in [1] ".repeat(20));
    assert_eq!(repr(&ok_src), "[1]");
}

#[test]
fn deep_parentheses_and_prefix_operators_are_rejected() {
    // Round 1 strings: ((((… and ----x.
    let parens = format!("{}1{}", "(".repeat(10_000), ")".repeat(10_000));
    assert_err(&parens, "LimitExceeded", DEPTH_MSG);
    assert_err(
        &format!("{}1", "-".repeat(10_000)),
        "LimitExceeded",
        DEPTH_MSG,
    );
    assert_err(
        &format!("{}1", "not ".repeat(10_000)),
        "LimitExceeded",
        DEPTH_MSG,
    );
    assert_err(
        &format!("{}1{}", "[".repeat(10_000), "]".repeat(10_000)),
        "LimitExceeded",
        DEPTH_MSG,
    );
}

// ---------------------------------------------------------------------------------------------
// Round 2, blocking 4: deeply nested runtime values
// ---------------------------------------------------------------------------------------------

#[test]
fn walrus_loops_cannot_build_values_deeper_than_the_limit() {
    // The review's string: `[0]*100000` alone is already past the per-value limit.
    assert_err(
        "(a:=[], [a:=[a] for _ in [0]*100000])",
        "LimitExceeded",
        &per_value_msg(slot() * 100_000 + slot()),
    );
    // Iterating a string instead reaches the nesting check.
    assert_err(
        "(a:=[], [a:=[a] for _ in 'x'*100000])",
        "LimitExceeded",
        "LimitExceeded: value nesting depth limit of 100 exceeded",
    );
    // At the limit the value exists and can be compared, printed and dropped.
    // 97 rounds: `a` is 98 deep, the comprehension's list 99, the enclosing tuple 100.
    let v = ok("(a:=[], [a:=[a] for _ in [0]*97], a)[2]");
    assert_eq!(v.depth(), 98);
    assert_eq!(
        ok("(a:=[], [a:=[a] for _ in [0]*97], a == a)[2]"),
        ExprValue::Bool(true)
    );
    assert_err(
        "(a:=[], [a:=[a] for _ in [0]*98], a)",
        "LimitExceeded",
        "LimitExceeded: value nesting depth limit of 100 exceeded",
    );
}

// ---------------------------------------------------------------------------------------------
// Round 2, blocking 5: unchecked arithmetic
// ---------------------------------------------------------------------------------------------

const INT_OVERFLOW: &str = "OverflowError: integer result does not fit in 64 bits";

#[test]
fn integer_overflow_is_an_error_in_every_profile() {
    for src in [
        "1<<63",
        "3<<62",
        "abs(-9223372036854775807-1)",
        "-(-9223372036854775807-1)",
        "9223372036854775807 + 1",
        "(-9223372036854775807-1) - 1",
        "4611686018427387904 * 2",
        "(-9223372036854775807-1) // -1",
        "2**63",
        "2**4294967297",
        "(-3)**41",
        "int(1e19)",
        "len('a') + 9223372036854775807",
    ] {
        assert_err(src, "OverflowError", INT_OVERFLOW);
    }
    assert_err(
        "-1<<64",
        "OverflowError",
        "OverflowError: shift count 64 is not below 64",
    );
    assert_eq!(ok("(-9223372036854775807-1) % -1"), ExprValue::Int(0));
    assert_eq!(ok("(-2)**63"), ExprValue::Int(i64::MIN));
    assert_eq!(ok("(-1)**(2**40)"), ExprValue::Int(1));
    assert_eq!(ok("1<<62"), ExprValue::Int(1 << 62));
    assert_eq!(ok("-1<<63"), ExprValue::Int(i64::MIN));
}

#[test]
fn date_and_duration_arithmetic_is_checked() {
    let days = "OverflowError: days=1000000000000000000; must have magnitude <= 999999999";
    assert_err("timedelta(days=10**18)", "OverflowError", days);
    assert_err(
        "timedelta(weeks=9223372036854775807)",
        "OverflowError",
        "OverflowError: days=64563604257983430649; must have magnitude <= 999999999",
    );
    assert_err(
        "timedelta(hours=1e300)",
        "OverflowError",
        "OverflowError: timedelta value out of range",
    );
    let range = "OverflowError: date value out of range";
    assert_err(
        "datetime(2024, 1, 1) + timedelta(days=999999999)",
        "OverflowError",
        range,
    );
    assert_err(
        "datetime(2024, 1, 1).date() + timedelta(days=999999999)",
        "OverflowError",
        range,
    );
    assert_err(
        "datetime(2024, 1, 1) - timedelta(days=999999999)",
        "OverflowError",
        range,
    );
    assert_err(
        "relativedelta(years=9223372036854775807) + relativedelta(years=1)",
        "OverflowError",
        range,
    );
    assert_err(
        "-relativedelta(years=-9223372036854775807-1)",
        "OverflowError",
        range,
    );
    assert_err(
        "relativedelta(microseconds=-9223372036854775807-1)",
        "OverflowError",
        range,
    );
    assert_err(
        "datetime(2024, 1, 1) + relativedelta(years=2**62)",
        "ValueError",
        "ValueError: year 4611686018427389928 is out of range",
    );
    assert_err(
        "datetime(2024, 1, 1) + relativedelta(days=9223372036854775807)",
        "OverflowError",
        "OverflowError: days=9223372036854775807; must have magnitude <= 999999999",
    );
    assert_err(
        "datetime(2024, 1, 1) + relativedelta(weekday=0) + relativedelta(days=-999999999)",
        "OverflowError",
        range,
    );
}

// ---------------------------------------------------------------------------------------------
// Round 2, blocking 6: printf-style formatting
// ---------------------------------------------------------------------------------------------

#[test]
fn printf_decodes_characters_and_applies_width_and_precision() {
    assert_eq!(ok("'%s é %s' % ('ü', 'ñ')"), ExprValue::str("ü é ñ"));
    assert_eq!(ok("'é%5sé' % 'ü'"), ExprValue::str("é    üé"));
    assert_eq!(
        ok("'%5.2f|%-6.1f|' % (3.14159, 2.25)"),
        ExprValue::str(" 3.14|2.2   |")
    );
    assert_eq!(ok("'%.3s' % 'héllo'"), ExprValue::str("hél"));
    assert_eq!(
        ok("'%(name)s:%(n)05.1f' % {'name': 'ü', 'n': 2.5}"),
        ExprValue::str("ü:002.5")
    );
    assert_eq!(ok("'%10.10f' % 1"), ExprValue::str("1.0000000000"));
    assert_eq!(ok("'%1000d' % 1").as_str().map(str::len), Some(1000));
}

#[test]
fn printf_rejects_unknown_conversions_leftovers_and_huge_widths() {
    assert_err(
        "'%q' % 1",
        "ValueError",
        "ValueError: unsupported format character 'q' (0x71) at index 1",
    );
    assert_err(
        "'é %y' % 1",
        "ValueError",
        "ValueError: unsupported format character 'y' (0x79) at index 3",
    );
    assert_err(
        "'%s' % (1, 2)",
        "TypeError",
        "TypeError: not all arguments converted during string formatting",
    );
    let width = "LimitExceeded: format width or precision 999999999 exceeds the limit of 1000";
    assert_err("'%0999999999d' % 1", "LimitExceeded", width);
    assert_err("'%.999999999f' % 1", "LimitExceeded", width);
    assert_err("'%*d' % (999999999, 1)", "LimitExceeded", width);
    assert_err("f'{1:0999999999d}'", "LimitExceeded", width);
    assert_err("'{:999999999}'.format(1)", "LimitExceeded", width);
    assert_err(
        "'%1001d' % 1",
        "LimitExceeded",
        "LimitExceeded: format width or precision 1001 exceeds the limit of 1000",
    );
}

// ---------------------------------------------------------------------------------------------
// Round 2, blocking 8 and lead decisions: limits cannot be raised, counters reset per evaluation
// ---------------------------------------------------------------------------------------------

#[test]
fn limits_can_only_be_lowered() {
    let raised = EvalLimits::default()
        .with_max_source_bytes(usize::MAX)
        .with_max_depth(10_000)
        .with_max_steps(usize::MAX)
        .with_max_value_bytes(usize::MAX)
        .with_max_total_bytes(usize::MAX)
        .with_max_format_width(usize::MAX);
    assert_eq!(raised, EvalLimits::CEILING);
    assert_eq!(raised.max_depth(), 100);
    assert_eq!(raised.max_steps(), 1_000_000);
    assert_eq!(raised.max_source_bytes(), 64 * 1024);
    assert_eq!(raised.max_value_bytes(), 1024 * 1024);
    assert_eq!(raised.max_total_bytes(), 16 * 1024 * 1024);
    assert_eq!(raised.max_format_width(), 1_000);

    let lowered = EvalLimits::default().with_max_depth(5).with_max_steps(10);
    assert_eq!(lowered.max_depth(), 5);
    assert_eq!(lowered.max_steps(), 10);
    // Lowering again never raises a previously lowered limit.
    assert_eq!(lowered.min(EvalLimits::default()).max_depth(), 5);

    let ctx = EvalContext::standard().with_limits(lowered);
    assert_err_in(
        &ctx,
        "((((((1))))))",
        "LimitExceeded",
        "LimitExceeded: expression nesting depth limit of 5 exceeded",
    );
    assert_err_in(
        &ctx,
        "[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]",
        "LimitExceeded",
        "LimitExceeded: evaluation step limit of 10 exceeded",
    );
}

#[test]
fn source_size_limit() {
    let src = format!("'{}'", "a".repeat(64 * 1024));
    assert_err(
        &src,
        "LimitExceeded",
        "LimitExceeded: expression source of 65538 bytes exceeds the limit of 65536 bytes",
    );
}

#[test]
fn counters_reset_for_every_evaluation() {
    // Steps: each evaluation uses about 600 of 1000 steps.
    let ctx = EvalContext::standard().with_limits(EvalLimits::default().with_max_steps(1_000));
    let src = format!("len([{}])", vec!["1"; 600].join(", "));
    for _ in 0..3 {
        assert_eq!(ok_in(&ctx, &src), ExprValue::Int(600));
    }
    // Memory: each evaluation charges about 10.8 MB of 16 MiB.
    let ctx = EvalContext::standard().with_var("s", big_string(900_000));
    let src = ["len(s)"; 12].join(" + ");
    for _ in 0..3 {
        assert_eq!(ok_in(&ctx, &src), ExprValue::Int(10_800_000));
    }
    // The same parsed tree evaluated twice also starts from zero.
    let ast = parse(&src).expect("parses");
    for _ in 0..2 {
        assert_eq!(
            eval_ast(&ast, &ctx).expect("evaluates"),
            ExprValue::Int(10_800_000)
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Round 1 strings
// ---------------------------------------------------------------------------------------------

#[test]
fn round_one_allocation_strings_are_rejected() {
    assert_err("'a'*10**9", "LimitExceeded", &per_value_msg(1_000_000_000));
    assert_err(
        "[0]*10**10",
        "LimitExceeded",
        &per_value_msg(slot() * 10_000_000_000 + slot()),
    );
    assert_err(
        "('a'*1000)*1000*1000",
        "LimitExceeded",
        &per_value_msg(1_000_000_000),
    );
    assert_err(
        "[0]*10**6*10**6",
        "LimitExceeded",
        &per_value_msg(slot() * 1_000_000 + slot()),
    );
}

// ---------------------------------------------------------------------------------------------
// Small-stack thread: evaluate and drop the deepest accepted inputs, and the rejected ones
// ---------------------------------------------------------------------------------------------

/// Stack of the test thread. Measured worst cases (`stack_probe.rs`): every input at or past the
/// depth limit runs within 768 KiB unoptimized and 128 KiB in a release build.
const SMALL_STACK: usize = if cfg!(debug_assertions) {
    1024 * 1024
} else {
    256 * 1024
};

#[test]
fn deep_inputs_evaluate_and_drop_on_a_small_stack() {
    let handle = std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(|| {
            let ctx = EvalContext::standard();
            let accepted = [
                format!("{}1{}", "(".repeat(99), ")".repeat(99)),
                format!("{}1", "-".repeat(98)),
                format!("{}True", "not ".repeat(98)),
                vec!["1"; 99].join("+"),
                vec!["1"; 99].join("**"),
                format!("{}1{}", "[".repeat(98), "]".repeat(98)),
                format!("x{}", "[0]".repeat(97)),
                nested_fstring(30),
                "(a:=[], [a:=[a] for _ in [0]*97], str(a), a == a, a)".to_string(),
            ];
            for src in &accepted {
                let ast = parse(src).unwrap_or_else(|e| panic!("{src:.40}: {e}"));
                let ctx = ctx.clone().with_var("x", nested_list(98));
                match eval_ast(&ast, &ctx) {
                    Ok(v) => {
                        assert!(!v.repr().is_empty());
                        drop(v);
                    }
                    Err(e) => panic!("{src:.40}: {e}"),
                }
                drop(ast);
            }
            let rejected = [
                format!("{}1{}", "(".repeat(5_000), ")".repeat(5_000)),
                vec!["1"; 5_000].join("+"),
                nested_fstring(5_000),
                format!("[1 {}]", "for x in [1] ".repeat(5_000)),
                "(a:=[], [a:=[a] for _ in [0]*100000])".to_string(),
                "(a:=[], [a:=[a] for _ in 'x'*100000])".to_string(),
                BOUND_METHOD_CHAIN.to_string(),
            ];
            for src in &rejected {
                let e = eval(src, &ctx).expect_err("rejected");
                assert_eq!(e.class(), "LimitExceeded", "{src:.40}");
            }
            // Parsing with a lower limit on the same thread.
            let limits = EvalLimits::default().with_max_depth(30);
            let deep = format!("{}1{}", "(".repeat(35), ")".repeat(35));
            assert_eq!(
                parse_with_limits(&deep, limits)
                    .expect_err("too deep")
                    .to_string(),
                "LimitExceeded: expression nesting depth limit of 30 exceeded"
            );
        })
        .expect("spawn test thread");
    handle
        .join()
        .expect("the small-stack thread must not overflow");
}

/// A host value nested `depth` levels deep: `[[[...[0]...]]]`.
fn nested_list(depth: usize) -> ExprValue {
    let mut v = ExprValue::Int(0);
    for _ in 0..depth {
        v = ExprValue::list(vec![v]);
    }
    v
}

#[test]
fn host_values_deeper_than_the_limit_are_rejected_on_use() {
    let ctx = EvalContext::standard().with_var("deep", nested_list(150));
    assert_err_in(
        &ctx,
        "deep",
        "LimitExceeded",
        "LimitExceeded: value nesting depth limit of 100 exceeded",
    );
    // Not touching it is fine.
    assert_eq!(ok_in(&ctx, "1"), ExprValue::Int(1));
}

// ---------------------------------------------------------------------------------------------
// Security verification of round 3 (round 4 findings)
// ---------------------------------------------------------------------------------------------

/// Bound methods keep their receiver: a chain of `{'f': a.get}` dicts nests through them.
const BOUND_METHOD_CHAIN: &str = "(a:={}, [0 for _ in 'x'*100000 if not (a:={'f': a.get})])";

#[test]
fn bound_methods_count_towards_value_depth() {
    // Before the fix the chain grew without limit and dropping it overflowed the stack. The
    // small-stack thread test also evaluates and drops it.
    assert_err(
        BOUND_METHOD_CHAIN,
        "LimitExceeded",
        "LimitExceeded: value nesting depth limit of 100 exceeded",
    );
    // A bound method is one level deeper than its receiver and carries its size.
    let m = ok("{'k': [1]}.get");
    assert_eq!(m.depth(), 3);
    assert_eq!(m.deep_size(), ok("{'k': [1]}").deep_size() + slot());
    assert_eq!(ok("{'k': [1]}.get('k')").repr(), "[1]");
}

#[test]
fn strip_with_a_large_character_set_is_linear() {
    // Before the fix: about 10**12 character comparisons, uncharged.
    assert_eq!(ok("('a'*999999).strip('b'*999999+'a')"), ExprValue::str(""));
    assert_eq!(ok("'xxaxx'.strip('xy')"), ExprValue::str("a"));
}

#[test]
fn printf_mapping_fields_borrow_the_value() {
    // 200,000 mapped fields of a 1 MB value: before the fix each field copied the value
    // (200 GB of uncharged copies); now each field borrows it and costs one step.
    let v = ok("('%(a)%'*200000) % {'a': 'x'*999999}");
    assert_eq!(v.as_str().map(str::len), Some(200_000));
    assert_eq!(ok("'%(a)s/%(a)s' % {'a': 'é'}"), ExprValue::str("é/é"));
}

#[test]
fn bytes_membership_is_charged_by_its_scan_cost() {
    assert_err(
        "(b'a'*1000 + b'b') in (b'a'*1000000)",
        "LimitExceeded",
        "LimitExceeded: evaluation step limit of 1000000 exceeded",
    );
    assert_eq!(ok("b'ab' in b'xxab'"), ExprValue::Bool(true));
    assert_eq!(ok("b'' in b'x'"), ExprValue::Bool(true));
}

#[test]
fn many_local_names_resolve_correctly() {
    let binds: Vec<String> = (0..2000).map(|i| format!("(v{i} := {i})")).collect();
    let src = format!("[{}, v0 + v1999 + v1000]", binds.join(", "));
    let ExprValue::List(items) = ok(&src) else {
        panic!("expected a list");
    };
    assert_eq!(items.items().last(), Some(&ExprValue::Int(2999)));
    let targets: Vec<String> = (0..300).map(|i| format!("t{i}")).collect();
    let row: Vec<String> = (0..300).map(|i| i.to_string()).collect();
    let src = format!(
        "[t0 + t299 for ({}) in [({})]]",
        targets.join(", "),
        row.join(", ")
    );
    assert_eq!(repr(&src), "[299]");
}

#[test]
fn error_messages_carry_truncated_reprs() {
    assert_err(
        "{'a': 1}['x'*1000]",
        "KeyError",
        &format!("KeyError: '{}...", "x".repeat(199)),
    );
    assert_err(
        "int('9'*5000 + 'x')",
        "ValueError",
        &format!(
            "ValueError: invalid literal for int() with base 10: '{}...",
            "9".repeat(199)
        ),
    );
    assert_err("{'a': 1}['b']", "KeyError", "KeyError: 'b'");
}
