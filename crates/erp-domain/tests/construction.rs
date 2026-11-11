//! Construction, list syntax, negation and traversal (SPEC-DOM §4.1, §4.3).

// Test crate: helpers outside `#[test]` functions may unwrap and panic too (clippy.toml intent).
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::*;
use erp_domain::{
    ConditionError, Domain, DomainKind, ErrorKind, ErrorReason, MAX_DEPTH, Operator, Value,
    ValueKind,
};
use serde_json::json;

fn a() -> Domain {
    c("a", "=", 1)
}

fn b() -> Domain {
    c("b", "=", 2)
}

#[test]
fn spec_dom_001_list_syntax_implicit_and() {
    // Implicit AND between top-level items.
    assert_eq!(
        json(json!([["a", "=", 1], ["b", "=", 2]])),
        json(json!(["&", ["a", "=", 1], ["b", "=", 2]]))
    );
    assert_eq!(
        json(json!(["|", ["a", "=", 1], ["b", "=", 2], ["c", "=", 3]])),
        json(json!([
            "&",
            "|",
            ["a", "=", 1],
            ["b", "=", 2],
            ["c", "=", 3]
        ]))
    );
    assert_eq!(
        json(json!([["a", "=", 1], "|", ["b", "=", 2], ["c", "=", 3]])),
        json(json!([
            "&",
            ["a", "=", 1],
            "|",
            ["b", "=", 2],
            ["c", "=", 3]
        ]))
    );
    assert_eq!(
        json(json!(["&", [1, "=", 1], ["a", "=", 1]])),
        json(json!(["&", [1, "=", 1], ["a", "=", 1]]))
    );
    let items = json!([
        ["a", "=", 1],
        ["b.z", "=", 2],
        "|",
        "|",
        ["c", "=", 3],
        "!",
        ["d", "=", 4],
        ["e", "=", 5],
        ["f", "=", 6]
    ]);
    let mut prefixed = vec![json!("&"), json!("&"), json!("&")];
    prefixed.extend(items.as_array().unwrap().iter().cloned());
    assert_eq!(json(items.clone()), json(json!(prefixed)));
}

#[test]
fn spec_dom_001_iterated_back_to_prefix_list() {
    let normalize = |v: serde_json::Value| json(v).to_json().unwrap();
    assert_eq!(
        normalize(json!([["a", "=", 1], ["b", "=", 2]])),
        json!(["&", ["a", "=", 1], ["b", "=", 2]])
    );
    assert_eq!(
        normalize(json!(["|", ["a", "=", 1], ["b", "=", 2]])),
        json!(["|", ["a", "=", 1], ["b", "=", 2]])
    );
    assert_eq!(
        normalize(json!(["|", ["a", "=", 1], ["b", "=", 2], ["c", "=", 3]])),
        json!(["&", "|", ["a", "=", 1], ["b", "=", 2], ["c", "=", 3]])
    );
    assert_eq!(
        normalize(json!([["a", "=", 1], "|", ["b", "=", 2], ["c", "=", 3]])),
        json!(["&", ["a", "=", 1], "|", ["b", "=", 2], ["c", "=", 3]])
    );
    let items = json!([
        ["a", "=", 1],
        ["b.z", "=", 2],
        "|",
        "|",
        ["c", "=", 3],
        ["d", "=", 4],
        ["e", "=", 5],
        ["f", "=", 6]
    ]);
    let mut prefixed = vec![json!("&"), json!("&"), json!("&")];
    prefixed.extend(items.as_array().unwrap().iter().cloned());
    assert_eq!(normalize(items), json!(prefixed));

    // Rewrite rules when making a list: sets and sub-domains become lists.
    let any = cond("foo", Operator::Any, c("bar", "=", "baz"));
    assert_eq!(
        any.to_json().unwrap(),
        json!([["foo", "any", [["bar", "=", "baz"]]]])
    );
    let any_and = cond("foo", Operator::Any, c("bar", "=", "baz") & a());
    assert_eq!(
        any_and.to_json().unwrap(),
        json!([["foo", "any", ["&", ["bar", "=", "baz"], ["a", "=", 1]]]])
    );
    assert_eq!(
        c("foo", "in", list([5])).to_json().unwrap(),
        json!([["foo", "in", [5]]])
    );
    assert_eq!(
        cond("foo", Operator::In, set([5, 6])).to_list(),
        vec![list([Value::from("foo"), "in".into(), list([5, 6])])]
    );
    assert_eq!(
        (a() | b()).to_string(),
        "['|', ('a', '=', 1), ('b', '=', 2)]"
    );
}

#[test]
fn spec_dom_001_typed_builder_matches_list_syntax() {
    let built = Domain::field("a").unwrap().eq(1) & Domain::field("b").unwrap().eq(2);
    assert_eq!(built, json(json!([["a", "=", 1], ["b", "=", 2]])));
    let any = Domain::field("partner_id")
        .unwrap()
        .any(Domain::field("name").unwrap().ilike("acme"));
    assert_eq!(
        any.to_json().unwrap(),
        json!([["partner_id", "any", [["name", "ilike", "acme"]]]])
    );
    let not_in = Domain::field("state")
        .unwrap()
        .not_in(vec!["done", "cancel"]);
    assert_eq!(
        not_in,
        json(json!([["state", "not in", ["done", "cancel"]]]))
    );
    let err = Domain::field("").unwrap_err();
    assert!(matches!(
        err.reason(),
        ErrorReason::InvalidFieldExpression(_)
    ));
    assert_eq!(err.kind(), ErrorKind::Programming);
}

#[test]
fn spec_dom_002_malformed_lists() {
    for malformed in [
        json!(["&"]),
        json!(["&", ["a", "=", 1]]),
        json!([["a", "=", 1], "&", ["b", "=", 2]]),
        json!([["a", "=", 1], "!"]),
    ] {
        let err = Domain::from_json(&malformed).unwrap_err();
        assert!(
            matches!(err.reason(), ErrorReason::Malformed(_)),
            "{malformed}: {err}"
        );
        assert!(err.to_string().starts_with("Domain() malformed domain"));
        assert_eq!(err.python_class(), "ValueError");
    }
    for invalid in [
        json!([["foo", "any!", []]]),
        json!([["foo", "not any!", []]]),
        json!([42]),
    ] {
        let err = Domain::from_json(&invalid).unwrap_err();
        assert!(
            matches!(err.reason(), ErrorReason::InvalidItem(_)),
            "{invalid}: {err}"
        );
        assert!(
            err.to_string()
                .starts_with("Domain() invalid item in domain: ")
        );
        assert_eq!(err.python_class(), "ValueError");
    }
    assert_eq!(
        Domain::from_json(&json!([42])).unwrap_err().to_string(),
        "Domain() invalid item in domain: 42"
    );
    // Internal parsing accepts the bypass operators and parses their list values.
    for op in ["any!", "not any!"] {
        let items = jv(json!([["foo", op, []]]));
        let Value::List(items) = items else { panic!() };
        let domain = Domain::from_list_internal(&items).unwrap();
        let condition = domain.as_condition().unwrap();
        assert!(matches!(condition.value(), Value::Domain(d) if d.is_true()));
        // The three-argument form always accepts them.
        assert!(Domain::condition_str("foo", op, Value::list(Vec::<Value>::new())).is_ok());
    }
    // Nested internal sub-domains are parsed recursively.
    let Value::List(items) = jv(json!([[
        "foo",
        "any",
        [["bar", "not any!", [["x", "=", 1]]]]
    ]])) else {
        panic!()
    };
    let domain = Domain::from_list_internal(&items).unwrap();
    let Value::Domain(sub) = domain.as_condition().unwrap().value() else {
        panic!()
    };
    assert!(matches!(
        sub.as_condition().unwrap().value(),
        Value::Domain(_)
    ));
    // A 3-item list whose first item is not a field name nor a constant leaf.
    let err = Domain::from_json(&json!([[2, "=", 1]])).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::InvalidArguments(_)));
    assert_eq!(err.python_class(), "TypeError");
    // Something that is not a domain at all.
    let err = Domain::from_json(&json!(42)).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::InvalidArgumentType(_)));
    assert_eq!(err.python_class(), "TypeError");
    // JSON objects are not domain values.
    assert!(Domain::from_json(&json!([["a", "=", {"x": 1}]])).is_err());
}

/// stack (ERPForAll guard, see `MAX_DEPTH`).
#[test]
fn spec_dom_002_depth_guard() {
    // ['&', c0, '|', c1, '&', c2, …, last]: each alternation adds one level.
    let build = |depth: usize| {
        let mut items = Vec::new();
        for i in 0..depth {
            items.push(Value::from(if i % 2 == 0 { "&" } else { "|" }));
            let field = if i % 3 == 0 { "integer" } else { "char" };
            items.push(list([
                Value::from(field),
                "=".into(),
                Value::from(i as i64),
            ]));
        }
        items.push(list([Value::from("float"), ">".into(), 1.into()]));
        items
    };
    let deep = Domain::from_list(&build(MAX_DEPTH - 1)).unwrap();
    // The deepest accepted domain optimizes, and negates, without exhausting the stack.
    let optimized = opt("test_orm.mixed", &deep);
    let negated = opt("test_orm.mixed", &!&optimized);
    assert!(!negated.is_true());
    let err = Domain::from_list(&build(MAX_DEPTH + 5)).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::TooDeep(_)));
    assert_eq!(err.python_class(), "RecursionError");
}

#[test]
fn spec_dom_003_constants_and_truthiness() {
    assert_eq!(
        Domain::from_value(&Value::Bool(true)).unwrap(),
        Domain::TRUE
    );
    assert_eq!(json(json!([])), Domain::TRUE);
    assert_eq!(json(json!(true)), Domain::TRUE);
    assert_eq!(
        Domain::from_value(&Value::Bool(false)).unwrap(),
        Domain::FALSE
    );
    assert_eq!(json(json!([[0, "=", 1]])), Domain::FALSE);
    assert_eq!(json(json!([[1, "=", 1]])), Domain::TRUE);
    assert_eq!(
        json(json!([[true, "=", 1]])),
        Domain::TRUE,
        "Python: True == 1"
    );
    assert_eq!(Domain::TRUE.to_json().unwrap(), json!([[1, "=", 1]]));
    assert_eq!(Domain::FALSE.to_json().unwrap(), json!([[0, "=", 1]]));

    assert_eq!(!Domain::TRUE, Domain::FALSE);
    assert_eq!(!Domain::FALSE, Domain::TRUE);
    for (items, expected) in [
        (json!(["!", "!", [1, "=", 1]]), Domain::TRUE),
        (json!(["!", "!", [0, "=", 1]]), Domain::FALSE),
        (json!(["!", "!", "!", "!", [1, "=", 1]]), Domain::TRUE),
        (json!(["!", "!", "!", "!", [0, "=", 1]]), Domain::FALSE),
        (json!(["!", [1, "=", 1]]), Domain::FALSE),
        (json!(["!", [0, "=", 1]]), Domain::TRUE),
        (json!(["!", "!", "!", [1, "=", 1]]), Domain::FALSE),
        (json!(["!", "!", "!", [0, "=", 1]]), Domain::TRUE),
    ] {
        assert_eq!(json(items.clone()), expected, "{items}");
    }

    let simple = c("foo", "=", "bar");
    let and_domain = &simple & &c("bar", "=", "baz");
    let neg_domain = !c("foo.x", ">", "bar");
    for (domain, is_true, is_false) in [
        (&simple, false, false),
        (&Domain::TRUE, true, false),
        (&Domain::FALSE, false, true),
        (&and_domain, false, false),
        (&neg_domain, false, false),
    ] {
        assert_eq!(domain.is_true(), is_true, "{domain}");
        assert_eq!(domain.is_false(), is_false, "{domain}");
    }
    // Domain(Domain) is the same domain.
    assert_eq!(
        Domain::from_value(&Value::Domain(simple.clone())).unwrap(),
        simple
    );
}

#[test]
fn spec_dom_004_and_or_flattening_and_absorption() {
    let t = Domain::TRUE;
    let f = Domain::FALSE;
    let normal = c("foo", "=", "bar");
    assert_eq!(Domain::or([]), f);
    assert_eq!(Domain::and([]), t);
    assert_eq!(Domain::or([f.clone()]), f);
    assert_eq!(Domain::or([f.clone(), f.clone()]), f);
    assert_eq!(&f | &f, f);
    assert_eq!(Domain::or([f.clone(), normal.clone()]), normal);
    assert_eq!(&f | &normal, normal);
    assert_eq!(Domain::or([Domain::and([t.clone()]), normal.clone()]), t);
    assert_eq!(Domain::and([t.clone()]), t);
    assert_eq!(Domain::and([t.clone(), t.clone()]), t);
    assert_eq!(&t & &t, t);
    assert_eq!(Domain::and([t.clone(), normal.clone()]), normal);
    assert_eq!(&t & &normal, normal);
    assert_eq!(Domain::and([Domain::or([f.clone()]), normal.clone()]), f);
    assert_eq!(Domain::and([json(json!([])), normal.clone()]), normal);
    assert_eq!(Domain::or([json(json!([])), normal.clone()]), t);

    let c1 = c("foo", "=", "bar");
    let c2 = c("foo", "=", "baz");
    let c3 = c("foo", "=", "abc");
    let c4 = c("foo", "=", "foo");
    let all = Domain::and([c1.clone(), c2.clone(), c3.clone(), c4.clone()]);
    let any = Domain::or([c1.clone(), c2.clone(), c3.clone(), c4.clone()]);
    assert_eq!(c1.clone() & c2.clone() & c3.clone() & c4.clone(), all);
    assert_eq!((c1.clone() & c2.clone()) & (c3.clone() & c4.clone()), all);
    assert_eq!(c1.clone() | c2.clone() | c3.clone() | c4.clone(), any);
    assert_eq!((c1.clone() | c2.clone()) | (c3.clone() | c4.clone()), any);
    assert_eq!(&all & &t, all);
    assert_eq!(&all & &f, f);
    assert_eq!(&all | &t, t);
    assert_eq!(&all | &f, all);
    assert_eq!(&t & &all, all);
    assert_eq!(&f & &all, f);
    assert_eq!(&t | &all, t);
    assert_eq!(&f | &all, all);

    let d1 = json(json!([["foo", "=", 1], ["bar", "=", 1]]));
    let d2 = json(json!(["&", ["foo", "=", 2], ["bar", "=", 2]]));
    assert_eq!(
        Domain::or([d1, d2]),
        json(json!([
            "|",
            "&",
            ["foo", "=", 1],
            ["bar", "=", 1],
            "&",
            ["foo", "=", 2],
            ["bar", "=", 2]
        ]))
    );
}

#[test]
fn spec_dom_005_construction_time_condition_checks() {
    let err = Domain::condition_str("foo", "xxx", "bar").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Invalid operator in condition ('foo', 'xxx', 'bar')"
    );
    assert_eq!(
        err.condition_reason(),
        Some(&ConditionError::InvalidOperator)
    );
    assert_eq!(err.python_class(), "ValueError");
    for op in ["<>", ">>", "==", "IN", ""] {
        assert!(Domain::condition_str("foo", op, 1).is_err(), "{op}");
    }
    // Same through list syntax.
    let err = Domain::from_json(&json!([["create_date", ">>", "foo"]])).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Invalid operator in condition ('create_date', '>>', 'foo')"
    );

    let err = Domain::condition_str("", "=", 1).unwrap_err();
    assert!(
        err.to_string().starts_with("Empty field name in condition"),
        "{err}"
    );
    assert_eq!(err.python_class(), "TypeError");
    assert!(Domain::condition("", Operator::Eq, 1).is_err());
    assert!(Domain::from_json(&json!([["", "=", 1]])).is_err());

    // None (JSON null) becomes False.
    let none = Domain::condition("a", Operator::Eq, None::<i64>).unwrap();
    assert_eq!(none.as_condition().unwrap().value(), &Value::FALSE);
    let null = json(json!([["a", "=", null]]));
    assert_eq!(null.as_condition().unwrap().value().kind(), ValueKind::Bool);
    assert_eq!(null, none);

    // A new-record reference becomes `in []` (negative operators: `not in []`).
    assert_eq!(
        Domain::condition("a", Operator::Eq, Value::NewId)
            .unwrap()
            .to_json()
            .unwrap(),
        json!([["a", "in", []]])
    );
    assert_eq!(
        Domain::condition("a", Operator::Ne, Value::NewId)
            .unwrap()
            .to_json()
            .unwrap(),
        json!([["a", "not in", []]])
    );

    // Values are kept as given until optimization.
    let dom = c("a", "=", 1);
    let condition = dom.as_condition().unwrap();
    assert_eq!(
        (
            condition.field_expr(),
            condition.operator(),
            condition.value()
        ),
        ("a", Operator::Eq, &Value::Int(1))
    );
    assert_eq!(
        c("a", "=", list([1, 2])).as_condition().unwrap().value(),
        &list([1, 2])
    );
    assert_eq!(
        c("a", "in", 5).as_condition().unwrap().value(),
        &Value::Int(5)
    );
    assert_eq!(
        c("a", "=", Vec::<Value>::new())
            .as_condition()
            .unwrap()
            .value(),
        &list(Vec::<Value>::new())
    );
    assert_eq!(
        c("a", "in", Domain::TRUE)
            .as_condition()
            .unwrap()
            .operator(),
        Operator::In
    );
    assert_eq!(
        json(json!([["a", "any", [["x", ">", 1]]]]))
            .as_condition()
            .unwrap()
            .value()
            .kind(),
        ValueKind::List
    );
}

#[test]
fn spec_dom_006_negation_at_construction() {
    let c1 = c("foo", "=", "bar");
    let c2 = c("foo", "=", "baz");
    assert_eq!(!&c1, c("foo", "!=", "bar"));
    assert_eq!(!!c1.clone(), c1);
    assert_eq!(!(&c1 & &c2), !&c1 | !&c2);
    assert_eq!(!(&c1 | &c2), !&c1 & !&c2);

    // Paths keep a NOT node.
    let c3 = c("foo.bar", "=", "baz");
    let c3_neg = !&c3;
    assert_eq!(c3_neg.to_list().first(), Some(&Value::from("!")));
    assert_eq!(
        json(json!(["!", ["a.b", "=", 4]])).to_json().unwrap(),
        json!(["!", ["a.b", "=", 4]])
    );
    assert_eq!(!(&c3_neg | &c2), &c3 & &!&c2);

    // Inequalities keep a NOT node until optimization.
    let gt = c("x", ">", 5);
    assert!(matches!((!&gt).kind(), DomainKind::Not(_)));
    assert_eq!(!!gt.clone(), gt);
    assert_eq!(!(&gt & &gt), !&gt | !&gt);
    assert_eq!(!(&gt | &gt), !&gt & !&gt);

    assert_eq!(
        json(json!([
            "!",
            "&",
            ["user_id", "=", 4],
            ["partner_id", "in", [1, 2]]
        ])),
        json(json!([
            "|",
            ["user_id", "!=", 4],
            ["partner_id", "not in", [1, 2]]
        ]))
    );

    // Negating an OR of 1000 ANDs.
    let pos = Domain::and([c("a", "in", Vec::<Value>::new()), c("d", "!=", 3)]);
    let neg = Domain::or([c("a", "not in", Vec::<Value>::new()), c("d", "=", 3)]);
    let source = !Domain::or(vec![pos; 1000]);
    let expected = Domain::and(vec![neg; 1000]);
    assert_eq!(source, expected);

    // Every invertible operator.
    for (op, inverse) in [
        ("in", "not in"),
        ("=", "!="),
        ("like", "not like"),
        ("ilike", "not ilike"),
        ("=like", "not =like"),
        ("=ilike", "not =ilike"),
        ("any", "not any"),
        ("any!", "not any!"),
    ] {
        assert_eq!(!c("f", op, 1), c("f", inverse, 1));
        assert_eq!(!c("f", inverse, 1), c("f", op, 1));
    }
    for op in [
        "<",
        ">",
        "<=",
        ">=",
        "=?",
        "child_of",
        "parent_of",
        "access",
    ] {
        assert!(
            matches!((!c("f", op, 1)).kind(), DomainKind::Not(_)),
            "{op}"
        );
    }
}

#[test]
fn spec_dom_082_iter_conditions() {
    let simple = c("foo", "=", "bar");
    let conditions: Vec<&Domain> = simple.iter_conditions().collect();
    assert_eq!(conditions.len(), 1);
    assert!(std::ptr::eq(conditions[0], &simple));

    assert_eq!(Domain::TRUE.iter_conditions().count(), 0);
    let with_true = &simple & &Domain::TRUE;
    assert_eq!(
        with_true.iter_conditions().cloned().collect::<Vec<_>>(),
        vec![simple.clone()]
    );
    let x = c("x", "=", "y");
    let both = &simple & &x;
    assert_eq!(
        both.iter_conditions().cloned().collect::<Vec<_>>(),
        vec![simple.clone(), x]
    );

    let any_domain = cond("foo", Operator::Any, c("bar", "=", "baz"));
    assert_eq!(
        any_domain.iter_conditions().cloned().collect::<Vec<_>>(),
        vec![any_domain.clone()]
    );
    let custom = Domain::custom(Arc::new(FakeCustom));
    assert_eq!(custom.iter_conditions().count(), 0);
    assert!(!custom.is_condition());
    // Conditions under NOT are visited.
    assert_eq!((!c("a.b", "=", 1)).iter_conditions().count(), 1);
}

#[test]
fn spec_dom_082_is_condition() {
    let dom = c("a", ">=", 1);
    assert!(dom.is_condition());
    assert!(dom.is_condition_matching(Some("a"), &[Operator::Ge], &[]));
    assert!(dom.is_condition_matching(None, &[Operator::Ge, Operator::In], &[]));
    assert!(dom.is_condition_matching(None, &[], &[ValueKind::Int]));
    assert!(dom.is_condition_matching(None, &[], &[ValueKind::Int, ValueKind::Float]));
    assert!(!dom.is_condition_matching(Some("b"), &[Operator::Ge], &[]));
    assert!(!dom.is_condition_matching(None, &[Operator::Eq], &[]));
    assert!(!dom.is_condition_matching(None, &[], &[ValueKind::Str]));
    assert!(!Domain::TRUE.is_condition());

    let both = &dom & &c("b", "=", 1);
    assert!(!both.is_condition());
    assert!(both.iter_conditions().all(Domain::is_condition));
}

#[test]
fn spec_dom_082_map_conditions() {
    let a1 = c("a", "=", 1);
    let b2 = c("b", "=", 2);
    let c3 = c("c", "=", 3);
    let replace = |search: &Domain, by: &Domain| {
        let (search, by) = (search.clone(), by.clone());
        move |condition: &Domain| {
            if *condition == search {
                by.clone()
            } else {
                condition.clone()
            }
        }
    };
    assert_eq!(
        a1.map_conditions(replace(&a1, &b2)),
        b2,
        "simple replacement"
    );
    assert_eq!(a1.map_conditions(replace(&b2, &a1)), a1, "no match");
    assert_eq!(
        Domain::TRUE.map_conditions(replace(&Domain::TRUE, &Domain::FALSE)),
        Domain::TRUE,
        "constants are not conditions"
    );
    let and_domain = &a1 & &b2;
    assert_eq!(and_domain.map_conditions(replace(&c3, &a1)), and_domain);
    assert_eq!(and_domain.map_conditions(replace(&b2, &a1)), &a1 & &a1);
    assert_eq!(and_domain.map_conditions(replace(&b2, &Domain::TRUE)), a1);
    assert_eq!(
        and_domain.map_conditions(replace(&b2, &Domain::FALSE)),
        Domain::FALSE
    );
    assert_eq!(
        (&and_domain | &c3).map_conditions(replace(&b2, &c3)),
        &(&a1 & &c3) | &c3,
        "replace inside different n-ary nodes"
    );
    let any = cond("foo", Operator::Any, a1.clone());
    assert_eq!(
        any.map_conditions(replace(&a1, &b2)),
        any,
        "`any` is not entered"
    );
    // NOT nodes are re-negated after mapping.
    let not_path = !c("x.y", "=", 1);
    assert_eq!(not_path.map_conditions(|_| c("z", "=", 1)), c("z", "!=", 1));
}

/// Structural equality and hashing support memoization.
#[test]
fn structural_equality_and_hash() {
    use std::collections::HashSet;
    let d1 = json(json!(["|", ["a", "in", [1, 2]], ["b", "=", "x"]]));
    let d2 = json(json!(["|", ["a", "in", [1, 2]], ["b", "=", "x"]]));
    let mut seen = HashSet::new();
    seen.insert(d1.clone());
    assert!(seen.contains(&d2));
    // Sets compare without order.
    assert_eq!(
        cond("a", Operator::In, set([1, 2])),
        cond("a", Operator::In, set([2, 1]))
    );
    let mut hashes = HashSet::new();
    hashes.insert(cond("a", Operator::In, set([1, 2])));
    assert!(hashes.contains(&cond("a", Operator::In, set([2, 1]))));
    // Values themselves follow Python equality (used for set membership and merges) ...
    assert_eq!(set([1]), set([1.0]));
    assert_eq!(set([Value::Int(1)]), set([Value::Bool(true)]));
    assert_eq!(
        set([Value::Int(0), Value::FALSE]),
        set([0]),
        "0 == False in Python"
    );
    assert_ne!(cond("a", Operator::Eq, 1), cond("a", Operator::Eq, true));
    assert_ne!(
        cond("a", Operator::In, set([1])),
        cond("a", Operator::In, set([1.0]))
    );
    assert_ne!(
        cond("a", Operator::In, set([1])),
        cond("a", Operator::In, list([1]))
    );
    // Custom nodes compare by identity.
    let custom = Domain::custom(Arc::new(FakeCustom));
    assert_eq!(custom, custom.clone());
    assert_ne!(custom, Domain::custom(Arc::new(FakeCustom)));
}
