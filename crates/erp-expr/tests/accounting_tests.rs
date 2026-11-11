// SPDX-License-Identifier: MIT OR Apache-2.0

//! Portable expression logical byte accounting and charging ledger tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_expr::{Budget, EvalContext, EvalLimits, ExprValue, RelativeDelta, eval};

#[test]
fn test_normative_scalar_logical_bytes() {
    assert_eq!(ExprValue::None.deep_size(), 32);
    assert_eq!(ExprValue::Bool(true).deep_size(), 32);
    assert_eq!(ExprValue::Bool(false).deep_size(), 32);
    assert_eq!(ExprValue::Int(0).deep_size(), 32);
    assert_eq!(ExprValue::Int(i64::MAX).deep_size(), 32);
    assert_eq!(ExprValue::Int(i64::MIN).deep_size(), 32);
    assert_eq!(ExprValue::Float(0.0).deep_size(), 32);
    assert_eq!(ExprValue::Float(1.2345).deep_size(), 32);
}

#[test]
fn test_normative_string_and_bytes_logical_bytes() {
    assert_eq!(ExprValue::Str("".into()).deep_size(), 32);
    assert_eq!(ExprValue::Str("hello".into()).deep_size(), 37);
    assert_eq!(
        ExprValue::Str("héllo".into()).deep_size(),
        32 + "héllo".len()
    ); // 32 + 6 = 38 UTF-8 bytes
    assert_eq!(ExprValue::Bytes(vec![]).deep_size(), 32);
    assert_eq!(ExprValue::Bytes(vec![0x00, 0xff, 0x42]).deep_size(), 35);
}

#[test]
fn test_normative_relative_delta_logical_bytes() {
    let rd = RelativeDelta::default();
    // 32-byte slot + 144-byte fixed payload = 176 bytes
    assert_eq!(ExprValue::RelativeDelta(Box::new(rd)).deep_size(), 176);
}

#[test]
fn test_normative_sequence_logical_bytes() {
    // List/Tuple: 32 + sum(child deep bytes)
    let empty_list = ExprValue::list(vec![]);
    assert_eq!(empty_list.deep_size(), 32);

    let int_list = ExprValue::list(vec![
        ExprValue::Int(1),
        ExprValue::Int(2),
        ExprValue::Int(3),
    ]);
    // 32 + 3 * 32 = 128
    assert_eq!(int_list.deep_size(), 128);

    let mixed_tuple = ExprValue::tuple(vec![
        ExprValue::Int(1),
        ExprValue::Str("abc".into()), // 32 + 3 = 35
    ]);
    // 32 + 32 + 35 = 99
    assert_eq!(mixed_tuple.deep_size(), 99);
}

#[test]
fn test_normative_dict_logical_bytes() {
    // Dict: 32 + sum(key deep bytes + val deep bytes + 16)
    let empty_dict = ExprValue::dict(vec![]).unwrap();
    assert_eq!(empty_dict.deep_size(), 32);

    let dict = ExprValue::dict(vec![
        (ExprValue::Str("a".into()), ExprValue::Int(1)),
        (ExprValue::Str("b".into()), ExprValue::Int(2)),
    ])
    .unwrap();
    // Key "a": 32 + 1 = 33
    // Val 1: 32
    // Entry 1 overhead: 16 -> entry 1 total = 33 + 32 + 16 = 81
    // Key "b": 32 + 1 = 33
    // Val 2: 32
    // Entry 2 overhead: 16 -> entry 2 total = 81
    // Container base: 32
    // Total = 32 + 81 + 81 = 194
    assert_eq!(dict.deep_size(), 194);
}

#[test]
fn test_charging_events_ledger() {
    let mut budget = Budget::new(EvalLimits::CEILING);
    assert_eq!(budget.bytes_used(), 0);
    assert_eq!(budget.steps_used(), 0);

    // Sequence literal reservation: 32 * (n + 1)
    // 3 elements: 32 * 4 = 128
    budget.alloc(32 * 4).unwrap();
    assert_eq!(budget.bytes_used(), 128);

    // Dict literal reservation: 32 + n * 80
    // 2 entries: 32 + 2 * 80 = 192
    budget.alloc(32 + 2 * 80).unwrap();
    assert_eq!(budget.bytes_used(), 128 + 192);

    // Admission of a string value "test" (36 bytes, 1 node)
    let val = ExprValue::Str("test".into());
    budget.admit(&val).unwrap();
    assert_eq!(budget.bytes_used(), 320 + 36);
    assert_eq!(budget.steps_used(), 1);
}

#[test]
fn test_exact_memory_budget_limit_trip() {
    let limits = EvalLimits::default().with_max_total_bytes(100);
    let ctx = EvalContext::standard().with_limits(limits);

    // Literal str "a" * 70:
    // "a" has deep size 33.
    // 'a' * 70 produces 32 + 70 = 102 bytes, exceeding 100 bytes budget!
    let err = eval("'a' * 70", &ctx).unwrap_err();
    assert_eq!(err.class(), "LimitExceeded");
    assert!(
        err.to_string()
            .contains("evaluation memory budget of 100 bytes exceeded"),
        "{err}"
    );
}
