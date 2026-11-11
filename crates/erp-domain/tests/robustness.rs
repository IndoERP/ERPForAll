//! Input limits, error provenance and complexity guarantees for untrusted domains.

// Test crate: helpers outside `#[test]` functions may unwrap and panic too (clippy.toml intent).
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use common::*;
use erp_domain::{
    ConditionError, Domain, DomainKind, ErrorKind, ErrorReason, MAX_DEPTH, MAX_ITEMS, Operator,
    Value,
};
use serde_json::json;

fn hash(domain: &Domain) -> u64 {
    let mut hasher = DefaultHasher::new();
    domain.hash(&mut hasher);
    hasher.finish()
}

/// External input gives `Validation` errors; Rust code gives `Programming` errors (orchestrator
/// decision of 2026-10-15, ADR pending).
#[test]
fn error_kind_follows_provenance() {
    // Parsing external input.
    for bad in [
        json!(["&"]),
        json!([["foo", "xxx", 1]]),
        json!([["", "=", 1]]),
        json!([42]),
        json!(42),
        json!([["a", "=", {"x": 1}]]),
    ] {
        let err = Domain::from_json(&bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{bad}: {err}");
    }
    let Value::List(items) = jv(json!([["a", "any!", []]])) else {
        panic!()
    };
    assert_eq!(
        Domain::from_list(&items).unwrap_err().kind(),
        ErrorKind::Validation
    );
    assert_eq!(
        Domain::from_value(&Value::Int(3)).unwrap_err().kind(),
        ErrorKind::Validation
    );
    assert_eq!(
        Value::from_json(&json!({"a": 1})).unwrap_err().kind(),
        ErrorKind::Validation
    );

    // Rust code.
    assert_eq!(
        Domain::condition_str("foo", "xxx", 1).unwrap_err().kind(),
        ErrorKind::Programming
    );
    assert_eq!(
        Domain::condition("", Operator::Eq, 1).unwrap_err().kind(),
        ErrorKind::Programming
    );
    let typed = c("nope", "=", 1);
    assert!(!typed.is_external());
    assert_eq!(opt_err(MIXED, &typed).kind(), ErrorKind::Programming);

    // A domain parsed from external input stays tagged through optimization errors, and through
    // combinations with domains built in Rust.
    let parsed = json(json!([["nope", "=", 1]]));
    assert!(parsed.is_external());
    let err = opt_err(MIXED, &parsed);
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(matches!(
        err.condition_reason(),
        Some(ConditionError::InvalidField { .. })
    ));
    let combined = &c("integer", "=", 1) & &parsed;
    assert!(combined.is_external());
    assert_eq!(opt_err(MIXED, &combined).kind(), ErrorKind::Validation);
    let owned = c("integer", "=", 1) | parsed.clone();
    assert!(owned.is_external());
    assert!((!&parsed).is_external());
    assert!(Domain::and([c("integer", "=", 1), parsed.clone()]).is_external());
    // The optimized result keeps the tag.
    let ok = json(json!([["integer", "=", 1]]));
    assert!(opt(MIXED, &ok).is_external());
    // The tag is an annotation: it does not change equality.
    assert_eq!(ok, c("integer", "=", 1));
}

/// A top-level JSON `null` is not a domain (a missing domain defaults to TRUE at the caller);
/// `null` as a condition value still means False.
#[test]
fn top_level_null_is_rejected() {
    let err = Domain::from_json(&json!(null)).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::InvalidArgumentType(_)));
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        json(json!([["a", "=", null]]))
            .as_condition()
            .unwrap()
            .value(),
        &Value::FALSE
    );
}

/// JSON integers beyond 64 bits are rejected, never turned into a lossy float.
#[test]
fn large_json_integers_are_rejected() {
    let max: serde_json::Value = serde_json::from_str("9223372036854775807").unwrap();
    assert_eq!(Value::from_json(&max).unwrap(), Value::Int(i64::MAX));
    let min: serde_json::Value = serde_json::from_str("-9223372036854775808").unwrap();
    assert_eq!(Value::from_json(&min).unwrap(), Value::Int(i64::MIN));
    let too_big: serde_json::Value = serde_json::from_str("18446744073709551615").unwrap();
    let err = Value::from_json(&too_big).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::UnsupportedValue(_)));
    let in_domain: serde_json::Value =
        serde_json::from_str(r#"[["id", "in", [1, 18446744073709551615]]]"#).unwrap();
    assert!(Domain::from_json(&in_domain).is_err());
    // Numbers written with a fraction stay floats.
    let float: serde_json::Value = serde_json::from_str("1.5").unwrap();
    assert_eq!(
        Value::from_json(&float).unwrap().kind(),
        erp_domain::ValueKind::Float
    );
}

/// Nested JSON arrays deeper than MAX_DEPTH are rejected while converting values, without
/// relying on serde_json's own parser limit (values can be built in memory).
#[test]
fn value_nesting_depth_is_limited() {
    let nest = |depth: usize| {
        let mut v = json!(1);
        for _ in 0..depth {
            v = serde_json::Value::Array(vec![v]);
        }
        v
    };
    assert!(Value::from_json(&nest(MAX_DEPTH)).is_ok());
    let err = Value::from_json(&nest(MAX_DEPTH + 1)).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::TooDeep(_)));
    let deep_condition = json!([["a", "in", nest(MAX_DEPTH + 10)]]);
    let err = Domain::from_json(&deep_condition).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::TooDeep(_)));
}

/// Error messages show at most about 200 characters of a value.
#[test]
fn error_reprs_are_truncated() {
    let long = "x".repeat(10_000);
    let err = Domain::condition_str("foo", "xxx", long.as_str()).unwrap_err();
    let message = err.to_string();
    assert!(message.len() < 300, "{} bytes", message.len());
    assert!(message.ends_with('…'), "{message}");
    assert!(message.starts_with("Invalid operator in condition ('foo', 'xxx', 'xxx"));

    let many: Vec<serde_json::Value> = (0..10_000).map(|i| json!(["a", "=", i])).collect();
    let mut malformed = vec![json!("&"); 20_000];
    malformed.extend(many);
    let err = Domain::from_json(&serde_json::Value::Array(malformed)).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::Malformed(_)));
    assert!(err.to_string().len() < 300);

    let ids: Vec<i64> = (0..100_000).collect();
    let err = opt_err(MIXED, &c("nope", "in", list(ids)));
    assert!(err.to_string().len() < 300);
    // Short values are rendered in full.
    assert_eq!(
        Domain::condition_str("foo", "xxx", "bar")
            .unwrap_err()
            .to_string(),
        "Invalid operator in condition ('foo', 'xxx', 'bar')"
    );
}

/// At most MAX_ITEMS list items.
#[test]
fn item_count_is_limited() {
    let leaf = list([Value::Int(1), "=".into(), Value::Int(1)]);
    assert!(
        Domain::from_list(&vec![leaf.clone(); MAX_ITEMS])
            .unwrap()
            .is_true()
    );
    let err = Domain::from_list(&vec![leaf; MAX_ITEMS + 1]).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::TooManyItems(_)));
    assert_eq!(err.kind(), ErrorKind::Validation);
    // Items inside internally parsed `any` sub-domains count too.
    let inner = vec![list([Value::Int(1), "=".into(), Value::Int(1)]); MAX_ITEMS];
    let outer = vec![list([Value::from("f"), "any".into(), Value::List(inner)])];
    assert!(matches!(
        Domain::from_list_internal(&outer).unwrap_err().reason(),
        ErrorReason::TooManyItems(_)
    ));
}

fn chain(op: &str, leaves: usize, right_nested: bool) -> Vec<Value> {
    let leaf = |i: usize| list([Value::from("integer"), "=".into(), Value::Int(i as i64)]);
    let mut items = Vec::with_capacity(2 * leaves);
    if right_nested {
        // ['|', A0, '|', A1, …, A(n-1)]
        for i in 0..leaves - 1 {
            items.push(Value::from(op));
            items.push(leaf(i));
        }
        items.push(leaf(leaves - 1));
    } else {
        // ['|', '|', …, A0, A1, …]
        items.extend(std::iter::repeat_n(Value::from(op), leaves - 1));
        items.extend((0..leaves).map(leaf));
    }
    items
}

/// Long prefix chains parse into one flat node, left- and right-nested alike.
#[test]
fn long_chains_parse_flat() {
    for right_nested in [false, true] {
        let domain = Domain::from_list(&chain("|", 50_000, right_nested)).unwrap();
        let DomainKind::Or(children) = domain.kind() else {
            panic!("expected an OR")
        };
        assert_eq!(children.len(), 50_000);
        assert_eq!(children[0], c("integer", "=", 0));
        assert_eq!(children[49_999], c("integer", "=", 49_999));
        assert_eq!(domain.to_list().len(), 99_999);
    }
    // Same result as combining the leaves in Rust.
    let small = Domain::from_list(&chain("&", 5, true)).unwrap();
    assert_eq!(small, Domain::and((0..5).map(|i| c("integer", "=", i))));
    // Mixed connectives keep their structure.
    let mixed = json(json!([
        "|",
        "&",
        ["a", "=", 1],
        ["b", "=", 2],
        "&",
        ["c", "=", 3],
        "|",
        ["d", "=", 4],
        ["e", "=", 5]
    ]));
    assert_eq!(
        mixed,
        (c("a", "=", 1) & c("b", "=", 2)) | (c("c", "=", 3) & (c("d", "=", 4) | c("e", "=", 5)))
    );
}

/// Parsing a 50k-leaf chain stays well under 100 ms (release builds; the debug build only checks
/// the shape above).
#[test]
#[ignore = "DEFERRED_PERFORMANCE: milestone-end release-only diagnostic"]
fn long_chain_parse_time_budget() {
    for right_nested in [false, true] {
        let items = chain("|", 50_000, right_nested);
        let start = Instant::now();
        let domain = Domain::from_list(&items).unwrap();
        let elapsed = start.elapsed();
        assert!(matches!(domain.kind(), DomainKind::Or(_)));
        assert!(
            elapsed < Duration::from_millis(100),
            "right_nested={right_nested}: {elapsed:?}"
        );
    }
    // Building the same chain with owned `|` reuses the child list.
    let start = Instant::now();
    let mut built = Domain::FALSE;
    for i in 0..50_000 {
        built = built | c("integer", "=", i);
    }
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "{:?}",
        start.elapsed()
    );
    assert!(matches!(built.kind(), DomainKind::Or(children) if children.len() == 50_000));
}

/// Owned `&`/`|` append to a child list they own; the result equals the iterator forms.
#[test]
fn owned_combination_reuses_children() {
    let mut built = Domain::TRUE;
    for i in 0..20_000 {
        built = built & c("integer", "=", i);
    }
    let expected = Domain::and((0..20_000).map(|i| c("integer", "=", i)));
    assert_eq!(built, expected);
    assert_eq!(hash(&built), hash(&expected));
    // A shared operand is copied, not modified.
    let shared = c("a", "=", 1) & c("b", "=", 2);
    let extended = shared.clone() & c("c", "=", 3);
    assert_eq!(shared, c("a", "=", 1) & c("b", "=", 2));
    assert_eq!(
        extended,
        Domain::and([c("a", "=", 1), c("b", "=", 2), c("c", "=", 3)])
    );
    // Prepending works too.
    let front = c("z", "=", 0) & Domain::and([c("a", "=", 1), c("b", "=", 2)]);
    assert_eq!(
        front,
        Domain::and([c("z", "=", 0), c("a", "=", 1), c("b", "=", 2)])
    );
}

/// Hashing is O(1) after construction: a 100k-id set hashes as fast as a small one.
#[test]
#[ignore = "DEFERRED_PERFORMANCE: milestone end"]
fn hashes_are_cached() {
    let ids: Vec<i64> = (0..100_000).collect();
    let big = opt(MIXED, &c("id", "in", list(ids.clone())));
    let again = opt(MIXED, &c("id", "in", list(ids.into_iter().rev())));
    assert_eq!(big, again, "sets compare without order");
    assert_eq!(hash(&big), hash(&again));
    let start = Instant::now();
    for _ in 0..10_000 {
        std::hint::black_box(hash(&big));
    }
    // 10k hashes of a 100k-element set would take minutes without the cache.
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

const MIXED: &str = "test_orm.mixed";

#[test]
fn owned_or_chain_preserves_shape() {
    let mut built = Domain::FALSE;
    for i in 0..50_000 {
        built = built | c("integer", "=", i);
    }
    assert!(matches!(built.kind(), DomainKind::Or(children) if children.len() == 50_000));
}

#[test]
fn unordered_id_sets_have_equal_hashes() {
    let ids: Vec<i64> = (0..100_000).collect();
    let big = opt(MIXED, &c("id", "in", list(ids.clone())));
    let again = opt(MIXED, &c("id", "in", list(ids.into_iter().rev())));
    assert_eq!(big, again, "sets compare without order");
    assert_eq!(hash(&big), hash(&again));
}

// Native list parsing has its own diagnostic contract, independent of external input.
#[test]
fn native_list_parser_accepts_internal_operators_without_external_origin() {
    for operator in ["any!", "not any!"] {
        let items = vec![list(["f".into(), operator.into(), Value::List(vec![])])];
        let parsed = Domain::from_list_native(&items).unwrap();
        assert!(!parsed.is_external());
        assert_eq!(parsed.as_condition().unwrap().operator().as_str(), operator);
        let error = Domain::from_list(&items).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.to_string(),
            format!("Domain() invalid item in domain: ('f', '{operator}', [])")
        );
    }
    let error = Domain::from_list_native(&[Value::Int(42)]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.to_string(), "Domain() invalid item in domain: 42");
}

#[test]
fn native_list_parser_bounds_quantifier_descent_and_counts_nested_items() {
    fn nested(levels: usize) -> Vec<Value> {
        let mut items = vec![];
        for _ in 0..levels {
            items = vec![list(["f".into(), "any".into(), Value::List(items)])];
        }
        items
    }
    assert!(Domain::from_list_native(&nested(MAX_DEPTH)).is_ok());
    // The malformed deepest token must never be reached: refuse before descending.
    let mut too_deep = vec![Value::Int(42)];
    for _ in 0..=MAX_DEPTH {
        too_deep = vec![list(["f".into(), "any".into(), Value::List(too_deep)])];
    }
    for (error, kind) in [
        (
            Domain::from_list_native(&too_deep).unwrap_err(),
            ErrorKind::Programming,
        ),
        (
            Domain::from_list_internal(&too_deep).unwrap_err(),
            ErrorKind::Validation,
        ),
    ] {
        assert_eq!(error.kind(), kind);
        assert_eq!(error.to_string(), "Domain() nesting deeper than 256 levels");
        assert!(matches!(error.reason(), ErrorReason::TooDeep(256)));
    }
    let leaf = list([Value::Int(1), "=".into(), Value::Int(1)]);
    assert!(
        Domain::from_list_native(&vec![leaf.clone(); MAX_ITEMS])
            .unwrap()
            .is_true()
    );
    let outer = vec![list([
        "f".into(),
        "any".into(),
        Value::List(vec![leaf; MAX_ITEMS]),
    ])];
    let error = Domain::from_list_native(&outer).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.to_string(), "Domain() has more than 250000 items");
}

#[test]
fn native_list_parser_bounds_nonquantifier_values_before_cloning() {
    let nested = |levels: usize| {
        let mut value = Value::Int(1);
        for _ in 0..levels {
            value = Value::List(vec![value]);
        }
        vec![list(["id".into(), "in".into(), value])]
    };
    assert!(Domain::from_list_native(&nested(MAX_DEPTH)).is_ok());
    let error = Domain::from_list_native(&nested(MAX_DEPTH + 1)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.to_string(), "Domain() nesting deeper than 256 levels");
}

/// Amendment 1: `thread_access` is a declared access-rule operator expanded by
/// `erp-security`, not a domain operator. Every parser entry refuses it with the ordinary
/// unknown-operator error, whatever the trust of the caller, so a client domain can never carry it.
/// External entries give `Validation`, native Rust entries `Programming`.
#[test]
fn every_parser_entry_refuses_a_declared_rule_operator() {
    const MESSAGE: &str = "Invalid operator in condition ('res_id', 'thread_access', 'read')";
    let triple = || json!(["res_id", "thread_access", "read"]);
    let items_of = |value: serde_json::Value| match jv(value) {
        Value::List(items) => items,
        other => panic!("a list, got {other:?}"),
    };
    let flat = items_of(json!([triple()]));
    let nested = items_of(json!(["|", ["a", "=", 1], triple()]));
    let quantified = items_of(json!([["partner_id", "any!", [triple()]]]));

    let cases = [
        ("from_list", Domain::from_list(&flat), ErrorKind::Validation),
        (
            "from_list_internal",
            Domain::from_list_internal(&flat),
            ErrorKind::Validation,
        ),
        (
            "from_list_native",
            Domain::from_list_native(&flat),
            ErrorKind::Programming,
        ),
        (
            "from_value",
            Domain::from_value(&Value::List(flat.clone())),
            ErrorKind::Validation,
        ),
        (
            "from_json",
            Domain::from_json(&json!([triple()])),
            ErrorKind::Validation,
        ),
        (
            "condition_str",
            Domain::condition_str("res_id", "thread_access", "read"),
            ErrorKind::Programming,
        ),
        (
            "from_list (under a connective)",
            Domain::from_list(&nested),
            ErrorKind::Validation,
        ),
        (
            "from_list_internal (under a quantifier)",
            Domain::from_list_internal(&quantified),
            ErrorKind::Validation,
        ),
        (
            "from_list_native (under a quantifier)",
            Domain::from_list_native(&quantified),
            ErrorKind::Programming,
        ),
    ];
    for (entry, result, kind) in cases {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), kind, "{entry}: {error}");
        assert_eq!(error.to_string(), MESSAGE, "{entry}");
    }
}
