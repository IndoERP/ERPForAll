//! BASIC optimization (SPEC-DOM §4.4–4.6).

// Test crate: helpers outside `#[test]` functions may unwrap and panic too (clippy.toml intent).
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use common::*;
use erp_domain::{
    ConditionError, Domain, ErrorKind, ErrorReason, ModelContext, Operator, OptimizationLevel,
    Value,
};
use serde_json::json;

const MIXED: &str = "test_orm.mixed";
const MESSAGE: &str = "test_orm.message";
const DISCUSSION: &str = "test_orm.discussion";
const PARTNER: &str = "test_orm.partner";

fn number_domain() -> Domain {
    c("numeric", ">", 5)
}

/// Types without a falsy value (many2one, datetime, …) get `in [False]`; the others rely on SQL.
#[test]
fn spec_dom_007_complement_of_inequalities() {
    assert_eq!(opt(MIXED, &!c("integer", ">", 2)), c("integer", "<=", 2));
    assert_eq!(opt(MIXED, &!c("float", "<", 1)), c("float", ">=", 1));
    assert_eq!(opt(MIXED, &!c("char", "<", "")), c("char", ">=", ""));
    assert_eq!(
        opt(PARTNER, &!c("parent_id", ">", 2)),
        cond("parent_id", Operator::In, set([false])) | cond("parent_id", Operator::Le, 2)
    );
    assert_eq!(
        opt(MIXED, &!c("moment", ">", datetime(2024, 1, 1, 10, 0, 0))),
        cond("moment", Operator::In, set([false]))
            | cond("moment", Operator::Lt, datetime(2024, 1, 1, 10, 0, 1))
    );
    // `id` has no falsy value either, but `False` is then dropped because ids are never NULL.
    assert_eq!(opt(MIXED, &!c("id", ">", 5)), c("id", "<=", 5));
    // De Morgan through optimization.
    assert_eq!(
        opt(MIXED, &!(c("integer", ">", 2) & c("float", "<", 1))),
        c("float", ">=", 1) | c("integer", "<=", 2)
    );
}

#[test]
fn spec_dom_008_equality_becomes_in_set() {
    assert_eq!(
        opt(MIXED, &c("numeric", "=", 5)),
        cond("numeric", Operator::In, set([5]))
    );
    let range = opt(MIXED, &c("id", "in", list(0..5)));
    assert_eq!(range, cond("id", Operator::In, set(0..5)));
    let ids = opt(MIXED, &c("id", "in", list([9, 99])));
    assert!(matches!(ids.as_condition().unwrap().value(), Value::Set(_)));
    let again = opt(MIXED, &ids);
    assert_eq!(again, ids, "idempotent");
    assert_eq!(again.level(), OptimizationLevel::Basic);

    assert_eq!(
        opt(MIXED, &c("id", "in", list(Vec::<i64>::new()))),
        Domain::FALSE
    );
    assert_eq!(
        opt(MIXED, &c("id", "not in", list(Vec::<i64>::new()))),
        Domain::TRUE
    );
    // `= []` means "not set".
    assert_eq!(
        opt(MIXED, &c("char", "=", list(Vec::<i64>::new()))),
        cond("char", Operator::In, set([false]))
    );
    assert_eq!(
        opt(MIXED, &c("char", "!=", list(["a", "b"]))),
        cond("char", Operator::NotIn, set(["a", "b"]))
    );
    // A scalar given to `in` is wrapped; falsy scalars are an empty set.
    assert_eq!(
        opt(MIXED, &c("integer", "in", 3)),
        cond("integer", Operator::In, set([3]))
    );
    assert_eq!(opt(MIXED, &c("integer", "in", false)), Domain::FALSE);
    assert_eq!(opt(MIXED, &c("integer", "not in", false)), Domain::TRUE);
    // Sets drop duplicates (Python equality: 1 == 1.0).
    assert_eq!(
        opt(
            MIXED,
            &c(
                "numeric",
                "in",
                list([Value::Int(1), Value::Float(1.0), Value::Int(2)])
            )
        ),
        cond("numeric", Operator::In, set([1, 2]))
    );
    // Constants are already optimized.
    assert_eq!(opt(MIXED, &Domain::TRUE), Domain::TRUE);
    assert_eq!(opt(MIXED, &Domain::FALSE), Domain::FALSE);
}

#[test]
fn spec_dom_009_false_dropped_for_not_null_fields() {
    assert_eq!(
        opt(MIXED, &c("id", "in", list([Value::Int(3), Value::FALSE]))),
        cond("id", Operator::In, set([3]))
    );
    assert_eq!(
        opt(
            "test_orm.business",
            &c("partner_id", "in", list([Value::Int(7), Value::FALSE]))
        ),
        cond("partner_id", Operator::In, set([7]))
    );
    // Types with a falsy value keep False, even when required.
    assert_eq!(
        opt(
            "test_orm.business",
            &c("name", "in", list([Value::from("x"), Value::FALSE]))
        ),
        cond("name", Operator::In, set([Value::from("x"), Value::FALSE]))
    );
    // A model holding new records keeps False.
    let with_new = model(MIXED).with_new_records(true);
    assert_eq!(
        c("id", "in", list([Value::Int(3), Value::FALSE]))
            .optimize(&with_new)
            .unwrap(),
        cond("id", Operator::In, set([Value::Int(3), Value::FALSE]))
    );
    // Only False is dropped, not 0.
    assert_eq!(
        opt(MIXED, &c("id", "in", list([Value::Int(0), Value::FALSE]))),
        cond("id", Operator::In, set([0])),
    );
    assert_eq!(opt(MIXED, &c("id", "=", false)), Domain::FALSE);
}

#[test]
fn spec_dom_010_eq_if_set() {
    assert_eq!(
        opt(MIXED, &c("numeric", "=?", 5)),
        opt(MIXED, &c("numeric", "=", 5))
    );
    assert_eq!(opt(MIXED, &c("numeric", "=?", 0)), Domain::TRUE);
    assert_eq!(opt(MIXED, &c("char", "=?", "")), Domain::TRUE);
    assert_eq!(opt(MIXED, &c("char", "=?", false)), Domain::TRUE);
}

#[test]
fn spec_dom_011_path_decomposition() {
    assert_eq!(
        opt(MIXED, &c("currency_id.id", ">", 5)),
        cond("currency_id", Operator::Any, c("id", ">", 5))
    );
    assert_eq!(
        opt(MIXED, &!c("currency_id.id", ">", 5)),
        cond("currency_id", Operator::NotAny, c("id", ">", 5))
    );
    // Longer paths nest.
    assert_eq!(
        opt(PARTNER, &c("parent_id.country_id.name", "=", "BE")),
        cond(
            "parent_id",
            Operator::Any,
            cond(
                "country_id",
                Operator::Any,
                cond("name", Operator::In, set(["BE"]))
            )
        )
    );
    // On a non-relational field the suffix is a property and is left to the field.
    assert_eq!(
        opt(MIXED, &c("date.month_number", "=", 2)),
        cond("date.month_number", Operator::In, set([2]))
    );
}

#[test]
fn spec_dom_012_like_value_normalization() {
    let like_ok = c("name", "like", "ok");
    assert_eq!(opt(MESSAGE, &like_ok), like_ok, "idempotent");
    assert_eq!(
        opt(MESSAGE, &c("name", "like", "")),
        Domain::TRUE,
        "matches anything"
    );
    assert_eq!(opt(MESSAGE, &c("name", "ilike", "")), Domain::TRUE);
    assert_eq!(
        opt(MESSAGE, &c("name", "not like", "")),
        Domain::FALSE,
        "matches nothing"
    );
    assert_eq!(opt(MESSAGE, &c("name", "not ilike", "")), Domain::FALSE);
    assert_eq!(
        opt(MESSAGE, &c("name", "=like", "")),
        opt(MESSAGE, &c("name", "=", false)),
        "matches the empty string only"
    );
    assert_eq!(
        opt(MESSAGE, &c("name", "not =ilike", "")),
        opt(MESSAGE, &c("name", "!=", false))
    );
    assert_eq!(
        opt(MESSAGE, &c("name", "like", 5)),
        c("name", "like", "5"),
        "converted with str()"
    );
    assert_eq!(
        opt(MESSAGE, &c("name", "ilike", 1.5)),
        c("name", "ilike", "1.5")
    );
    assert_eq!(
        opt(MESSAGE, &c("name", "ilike", true)),
        c("name", "ilike", "True")
    );

    let err = opt_err(MESSAGE, &c("color", "=like", 4));
    assert_eq!(
        err.condition_reason(),
        Some(&ConditionError::PatternNotString)
    );
    assert!(
        err.to_string()
            .starts_with("The pattern to match must be a string")
    );
    assert_eq!(err.python_class(), "TypeError");

    // Relational fields.
    assert_eq!(
        opt(MESSAGE, &c("discussion", "like", "")),
        cond("discussion", Operator::NotIn, set([false])),
        "matches anything in relation"
    );
    let like = opt(MESSAGE, &c("discussion", "like", "ok"));
    let condition = like.as_condition().unwrap();
    assert_eq!(condition.operator(), Operator::Any);
    let Value::Domain(sub) = condition.value() else {
        panic!("{like}")
    };
    assert_eq!(sub.as_condition().unwrap().field_expr(), "display_name");
    assert_eq!(
        opt(MESSAGE, &c("discussion", "not like", "ok"))
            .as_condition()
            .unwrap()
            .operator(),
        Operator::NotAny,
        "always the positive operator on display_name"
    );

    let err = opt_err(MESSAGE, &c("name", "like", sql()));
    assert_eq!(err.condition_reason(), Some(&ConditionError::LikeWithSql));
    assert_eq!(err.python_class(), "TypeError");
}

#[test]
fn spec_dom_013_relational_fields_compared_to_strings() {
    assert_eq!(
        opt(PARTNER, &c("country_id", "like", "ok")),
        cond("country_id", Operator::Any, c("display_name", "like", "ok"))
    );
    assert_eq!(
        opt(PARTNER, &c("country_id", "not like", "ok")),
        cond(
            "country_id",
            Operator::NotAny,
            c("display_name", "like", "ok")
        )
    );
    assert_eq!(
        opt(PARTNER, &c("country_id", "!=", "Belgium")),
        cond(
            "country_id",
            Operator::NotAny,
            cond("display_name", Operator::In, set(["Belgium"]))
        )
    );
    // Strings search the name, other members stay ids; canonical order puts `in` first.
    assert_eq!(
        opt(
            PARTNER,
            &c(
                "category_id",
                "in",
                list([Value::from("sellers"), Value::Int(4)])
            )
        ),
        cond("category_id", Operator::In, set([4]))
            | cond(
                "category_id",
                Operator::Any,
                cond("display_name", Operator::In, set(["sellers"]))
            )
    );
    // Negative: AND of `not any` and `not in`.
    assert_eq!(
        opt(
            PARTNER,
            &c(
                "category_id",
                "not in",
                list([Value::from("sellers"), Value::Int(4)])
            )
        ),
        cond("category_id", Operator::NotIn, set([4]))
            & cond(
                "category_id",
                Operator::NotAny,
                cond("display_name", Operator::In, set(["sellers"]))
            )
    );
    let err = opt_err(PARTNER, &c("country_id", ">=", "Par"));
    assert_eq!(
        err.condition_reason(),
        Some(&ConditionError::RelationalInequalityWithString)
    );
    assert!(
        err.to_string()
            .starts_with("Inequality not supported for relational field using a string")
    );
    assert_eq!(err.python_class(), "TypeError");
    // Inequalities with ids are fine.
    assert_eq!(
        opt(PARTNER, &c("country_id", ">=", 3)),
        c("country_id", ">=", 3)
    );
}

#[test]
fn spec_dom_014_boolean_normalization() {
    let is_important = cond("important", Operator::In, set([true]));
    assert_eq!(opt(MESSAGE, &is_important), is_important, "idempotent");
    assert_eq!(opt(MESSAGE, &c("important", "=", true)), is_important);
    assert_eq!(
        opt(MESSAGE, &c("important", "=", false)),
        cond("important", Operator::NotIn, set([true]))
    );
    assert_eq!(
        opt(MESSAGE, &c("important", "!=", false)),
        cond("important", Operator::In, set([true]))
    );
    // The tautology stays at BASIC (it would remove `active` from the domain).
    assert_eq!(
        opt(MESSAGE, &c("important", "not in", list([true, false])))
            .to_json()
            .unwrap(),
        json!([["important", "not in", [true, false]]])
    );
    assert_eq!(
        opt(MESSAGE, &c("active", "in", list([true, false])))
            .to_json()
            .unwrap(),
        json!([["active", "in", [true, false]]])
    );
    // Non-boolean members are parsed (str2bool for strings, truthiness otherwise).
    assert_eq!(
        opt(MESSAGE, &c("important", "=", "x")),
        cond("important", Operator::NotIn, set([true]))
    );
    assert_eq!(
        opt(MESSAGE, &c("important", "=", "true")),
        cond("important", Operator::In, set([true]))
    );
    assert_eq!(
        opt(MESSAGE, &c("important", "in", list([1]))),
        cond("important", Operator::In, set([true]))
    );
    let err = opt_err(MESSAGE, &c("important", ">", true));
    assert!(matches!(
        err.condition_reason(),
        Some(ConditionError::BooleanNotCollection { .. })
    ));
    assert_eq!(
        err.to_string(),
        "Cannot compare 'important' to <class 'bool'> which is not a collection of length 1 \
         in condition ('important', '>', True)"
    );
    assert_eq!(err.python_class(), "ValueError");
}

#[test]
fn spec_dom_020_any_value_normalization() {
    let q = query();
    let with_query = cond("currency_id", Operator::AnyBypass, q.clone());
    assert_eq!(
        opt(MIXED, &with_query),
        with_query,
        "idempotent with a query value"
    );
    assert_eq!(
        opt(MIXED, &cond("currency_id", Operator::Any, Domain::FALSE)),
        Domain::FALSE
    );
    assert_eq!(
        opt(MIXED, &cond("currency_id", Operator::NotAny, Domain::FALSE)),
        Domain::TRUE
    );
    assert_eq!(
        opt(
            MIXED,
            &cond(
                "currency_id",
                Operator::Any,
                c("id", "not in", list(Vec::<i64>::new()))
            )
        ),
        cond("currency_id", Operator::Any, Domain::TRUE)
    );
    let once = opt(
        MIXED,
        &cond("currency_id", Operator::Any, c("id", "in", list([1]))),
    );
    assert_eq!(opt(MIXED, &once), once, "idempotent");

    assert_eq!(
        opt(MIXED, &cond("id", Operator::Any, number_domain())),
        number_domain()
    );
    assert_eq!(
        opt(MIXED, &cond("id", Operator::NotAny, number_domain())),
        opt(MIXED, &!number_domain())
    );
    assert_eq!(opt(MIXED, &!number_domain()), c("numeric", "<=", 5));

    let err = opt_err(MIXED, &cond("numeric", Operator::Any, c("id", ">", 0)));
    assert_eq!(
        err.condition_reason(),
        Some(&ConditionError::AnyOnNonRelational)
    );
    assert!(
        err.to_string()
            .starts_with("Cannot use 'any' with non-relational fields")
    );
    assert_eq!(err.python_class(), "ValueError");

    // A query value forces the bypass form.
    assert_eq!(
        opt(DISCUSSION, &cond("moderator", Operator::Any, q.clone())),
        cond("moderator", Operator::AnyBypass, q.clone())
    );
    assert_eq!(
        opt(DISCUSSION, &cond("moderator", Operator::NotAny, q.clone())),
        cond("moderator", Operator::NotAnyBypass, q.clone())
    );
    // `in` with a query or domain value switches to `any`.
    assert_eq!(
        opt(DISCUSSION, &c("moderator", "in", q.clone())),
        cond("moderator", Operator::AnyBypass, q)
    );
    // List values are parsed (not as internal lists: `any!` is rejected inside).
    assert_eq!(
        opt(
            DISCUSSION,
            &json(json!([["moderator", "any", [["login", "=", "x"]]]]))
        ),
        cond(
            "moderator",
            Operator::Any,
            cond("login", Operator::In, set(["x"]))
        )
    );
    let nested_bypass =
        Domain::condition("moderator", Operator::Any, jv(json!([["id", "any!", []]]))).unwrap();
    let err = nested_bypass.optimize(&model(DISCUSSION)).unwrap_err();
    assert!(matches!(err.reason(), ErrorReason::InvalidItem(_)));
    // Built in Rust code: a programming error, even though the bad item is in a list value.
    assert_eq!(err.kind(), ErrorKind::Programming);
    // Sub-domains are checked against the comodel.
    assert!(matches!(
        opt_err(
            MIXED,
            &cond("many2one_id", Operator::Any, c("nope", "=", 1))
        )
        .condition_reason(),
        Some(ConditionError::InvalidField { .. })
    ));
}

#[test]
fn spec_dom_025_canonical_child_order() {
    let domain = Domain::and([
        c("numeric", "=", 5),
        c("date", "like", "2024"),
        c("date", "!=", false),
        c("numeric", "<", 99),
        c("html", "like", "ok"),
    ]);
    assert_eq!(
        opt(MIXED, &domain),
        Domain::and([
            cond("date", Operator::NotIn, set([false])),
            c("date", "like", "2024"),
            c("html", "like", "ok"),
            cond("numeric", Operator::In, set([5])),
            c("numeric", "<", 99),
        ])
    );
}

#[test]
fn spec_dom_026_set_merges_for_scalar_fields() {
    fn domain(op: Operator, values: &[i64]) -> Domain {
        if values.is_empty() {
            return Domain::constant(op == Operator::NotIn);
        }
        cond("numeric", op, set(values.iter().copied()))
    }
    fn or(a: &[i64], b: &[i64]) -> Vec<i64> {
        let mut out = a.to_vec();
        out.extend(b.iter().filter(|v| !a.contains(v)));
        out
    }
    fn and(a: &[i64], b: &[i64]) -> Vec<i64> {
        a.iter().copied().filter(|v| b.contains(v)).collect()
    }
    fn minus(a: &[i64], b: &[i64]) -> Vec<i64> {
        a.iter().copied().filter(|v| !b.contains(v)).collect()
    }
    use Operator::{In, NotIn};
    let sets: [&[i64]; 4] = [&[1, 2, 3], &[3, 4, 5], &[9, 10], &[]];
    let mut pairs = Vec::new();
    for i in 0..sets.len() {
        for j in i + 1..sets.len() {
            pairs.push((sets[i], sets[j]));
            pairs.push((sets[sets.len() - 1 - i], sets[sets.len() - 1 - j]));
        }
    }
    for (a, b) in pairs {
        let ctx = format!("{a:?} {b:?}");
        assert_eq!(
            opt(MIXED, &(domain(In, a) | domain(In, b))),
            domain(In, &or(a, b)),
            "in | in {ctx}"
        );
        assert_eq!(
            opt(MIXED, &(domain(In, a) & domain(In, b))),
            domain(In, &and(a, b)),
            "in & in {ctx}"
        );
        assert_eq!(
            opt(MIXED, &(domain(NotIn, a) | domain(NotIn, b))),
            domain(NotIn, &and(a, b)),
            "not in | not in {ctx}"
        );
        assert_eq!(
            opt(MIXED, &(domain(NotIn, a) & domain(NotIn, b))),
            domain(NotIn, &or(a, b)),
            "not in & not in {ctx}"
        );
        assert_eq!(
            opt(MIXED, &(domain(In, a) | domain(NotIn, b))),
            domain(NotIn, &minus(b, a)),
            "in | not in {ctx}"
        );
        assert_eq!(
            opt(MIXED, &(domain(In, a) & domain(NotIn, b))),
            domain(In, &minus(a, b)),
            "in & not in {ctx}"
        );
    }
    let (s123, s345, s910): (&[i64], &[i64], &[i64]) = (&[1, 2, 3], &[3, 4, 5], &[9, 10]);
    assert_eq!(
        opt(
            MIXED,
            &(domain(In, s123) | domain(NotIn, s910) | domain(In, s345))
        ),
        domain(NotIn, s910)
    );
    assert_eq!(
        opt(
            MIXED,
            &(domain(In, s123) & domain(NotIn, s345) & domain(In, &[1]))
        ),
        domain(In, &[1])
    );
    assert_eq!(
        opt(MIXED, &!(domain(In, s123) | domain(In, s345))),
        domain(NotIn, &or(s123, s345))
    );
    assert_eq!(
        opt(MIXED, &!(domain(In, s123) & domain(In, s345))),
        domain(NotIn, &and(s123, s345))
    );
    let merged = opt(
        MIXED,
        &(c("numeric", "in", list([1])) | c("numeric", "in", list([2]))),
    );
    assert!(matches!(
        merged.as_condition().unwrap().value(),
        Value::Set(_)
    ));
}

#[test]
fn spec_dom_027_relational_merge_rules() {
    // in / not in
    for (field, many2one) in [
        ("moderator", true),
        ("messages", false),
        ("categories", false),
    ] {
        let d1 = opt(DISCUSSION, &c(field, "in", list([1])));
        let d2 = opt(DISCUSSION, &c(field, "in", list([1, 2])));
        let not_d1 = !&d1;
        let not_d2 = !&d2;
        if many2one {
            assert_eq!(opt(DISCUSSION, &(&d1 & &d2)), d1, "{field}");
            assert_eq!(opt(DISCUSSION, &(&not_d1 | &not_d2)), not_d1, "{field}");
        } else {
            assert_eq!(opt(DISCUSSION, &(&d1 & &d2)), &d1 & &d2, "{field}");
            assert_eq!(
                opt(DISCUSSION, &(&not_d1 | &not_d2)),
                &not_d1 | &not_d2,
                "{field}"
            );
        }
        assert_eq!(opt(DISCUSSION, &(&d1 | &d2)), d2, "{field}");
        assert_eq!(opt(DISCUSSION, &(&not_d1 & &not_d2)), not_d2, "{field}");
    }

    // any / not any
    for (field, comodel, right) in [
        ("moderator", "res.users", c("login", "like", "one")),
        (
            "categories",
            "test_orm.category",
            c("name", "like", "these"),
        ),
        ("messages", MESSAGE, c("name", "like", "hello")),
    ] {
        let many2one = field == "moderator";
        let left = opt(comodel, &c("id", ">", 5));
        let right = opt(comodel, &right);
        let any = |d: &Domain| cond(field, Operator::Any, d.clone());
        let not_any = |d: &Domain| cond(field, Operator::NotAny, d.clone());
        assert_eq!(
            opt(DISCUSSION, &(any(&left) | any(&right))),
            any(&(&left | &right)),
            "{field}"
        );
        assert_eq!(
            opt(DISCUSSION, &(any(&left) & any(&right))),
            if many2one {
                any(&(&left & &right))
            } else {
                any(&left) & any(&right)
            },
            "{field}"
        );
        let q = query();
        assert_eq!(
            opt(
                DISCUSSION,
                &(any(&left) | cond(field, Operator::Any, q.clone()) | any(&right))
            ),
            any(&(&left | &right)) | cond(field, Operator::AnyBypass, q),
            "{field}: a query value is not merged"
        );
        assert_eq!(
            opt(DISCUSSION, &(not_any(&left) | not_any(&right))),
            if many2one {
                not_any(&(&left & &right))
            } else {
                not_any(&left) | not_any(&right)
            },
            "{field}"
        );
        assert_eq!(
            opt(DISCUSSION, &(not_any(&left) & not_any(&right))),
            not_any(&(&left | &right)),
            "{field}"
        );
        assert_eq!(
            opt(DISCUSSION, &(any(&left) | not_any(&right))),
            any(&left) | not_any(&right),
            "{field}: any and not any are not merged"
        );
    }
}

#[test]
fn spec_dom_028_duplicates_removed() {
    assert_eq!(
        opt(MIXED, &(number_domain() & number_domain())),
        number_domain()
    );
    assert_eq!(
        opt(MIXED, &(number_domain() | number_domain())),
        number_domain()
    );
}

#[test]
fn spec_dom_031_invalid_field_and_operator() {
    let err = opt_err("test_orm.country", &c("abcdefg", "in", list(["foo"])));
    assert!(err.to_string().starts_with("Invalid field"), "{err}");
    assert_eq!(
        err.to_string(),
        "Invalid field test_orm.country.abcdefg in condition ('abcdefg', 'in', ['foo'])"
    );
    assert_eq!(err.python_class(), "ValueError");
    let err = opt_err("test_orm.country", &c("does_not_exist", "=", "foo"));
    assert!(err.to_string().contains("'does_not_exist'"), "{err}");
    let err = opt_err(MIXED, &c("xxx_inexisting", "=", false));
    assert!(matches!(
        err.condition_reason(),
        Some(ConditionError::InvalidField { .. })
    ));
    // Unknown fields inside paths and sub-domains are reported against the comodel.
    let err = opt_err(PARTNER, &c("country_id.nope", "=", 1));
    assert!(
        err.to_string()
            .starts_with("Invalid field test_orm.country.nope"),
        "{err}"
    );
    let err = Domain::condition_str("create_date", ">>", "foo").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Invalid operator in condition ('create_date', '>>', 'foo')"
    );
    let err = opt_err(MIXED, &c(".name", "=", 1));
    assert!(matches!(
        err.reason(),
        ErrorReason::InvalidFieldExpression(_)
    ));
}

/// A domain optimized for one context is checked again for another: the cached level is keyed by
/// the context (model, timezone, new records, schema version).
#[test]
fn optimization_level_is_keyed_by_context() {
    let optimized = opt(MIXED, &c("numeric", ">", 5));
    assert_eq!(optimized.level(), OptimizationLevel::Basic);
    // `numeric` does not exist on partners: the cached level must not skip that check.
    let err = opt_err(PARTNER, &optimized);
    assert!(matches!(
        err.condition_reason(),
        Some(ConditionError::InvalidField { .. })
    ));
    // Timezone matters for datetime fields: a date value converted for one timezone is not
    // re-used for another key.
    let brussels = Brussels;
    let utc_ctx = model(MIXED);
    let brussels_ctx = model(MIXED).with_tz(&brussels);
    assert_ne!(utc_ctx.cache_key(), brussels_ctx.cache_key());
    assert_eq!(utc_ctx.cache_key(), model(MIXED).cache_key());
    assert_ne!(
        model(MIXED).cache_key(),
        model(MIXED).with_new_records(true).cache_key()
    );
    assert_eq!(utc_ctx.cache_key().timezone, "UTC");
    assert_eq!(brussels_ctx.cache_key().timezone, "Europe/Brussels");
    let ctx: &ModelContext<'_> = &brussels_ctx;
    let in_brussels = c("moment", ">=", "2024-07-02").optimize(ctx).unwrap();
    assert_eq!(
        in_brussels,
        cond("moment", Operator::Ge, datetime(2024, 7, 1, 22, 0, 0))
    );
    // Same context again: the cached level is used and the result is unchanged.
    assert_eq!(in_brussels.optimize(ctx).unwrap(), in_brussels);
}

#[test]
fn spec_dom_067_basic_is_idempotent_and_defers_later_levels() {
    // Relative dates are resolved at DYNAMIC_VALUES, the active tautology at FULL.
    let relative = c("date", ">", "today");
    assert_eq!(opt(MIXED, &relative), relative);
    let tautology = c("active", "in", list([true, false]));
    assert_eq!(
        opt(MESSAGE, &tautology),
        cond("active", Operator::In, set([true, false]))
    );
    for (model_name, domain) in [
        (MIXED, c("numeric", ">", 5)),
        (
            MIXED,
            json(json!([
                "|",
                ["numeric", "=", 5],
                "!",
                ["moment", ">", "2024-01-01"]
            ])),
        ),
        (
            PARTNER,
            json(json!([
                ["parent_id.name", "ilike", "x"],
                ["category_id", "!=", "a"]
            ])),
        ),
        (
            MESSAGE,
            json(json!([
                "!",
                "&",
                ["discussion.name", "=", "d"],
                ["important", "=", false]
            ])),
        ),
    ] {
        let once = opt(model_name, &domain);
        let twice = opt(model_name, &once);
        assert_eq!(twice, once, "{domain}");
        assert_eq!(once.level(), OptimizationLevel::Basic, "{domain}");
        assert_eq!(twice.level(), OptimizationLevel::Basic, "{domain}");
    }
    // An unoptimized input is left untouched (domains are immutable).
    let input = c("numeric", "=", 5);
    let _ = opt(MIXED, &input);
    assert_eq!(input.level(), OptimizationLevel::None);
    assert_eq!(input.as_condition().unwrap().operator(), Operator::Eq);
}

#[test]
fn spec_dom_069_path_conditions_merge_into_one_any() {
    let check = |domain: serde_json::Value, expected: serde_json::Value| {
        let anyfied = opt(PARTNER, &json(domain.clone()));
        let expected = json(expected).map_conditions(|condition| opt(PARTNER, condition));
        assert_eq!(anyfied, expected, "for initial domain {domain}");
    };
    check(json!([[1, "=", 1]]), json!([[1, "=", 1]]));
    check(
        json!([["name", "=", "Jack"]]),
        json!([["name", "=", "Jack"]]),
    );
    check(
        json!([["business_id.name", "=", "SGC"]]),
        json!([["business_id", "any", [["name", "=", "SGC"]]]]),
    );
    check(
        json!([["child_ids.name", "=", "Jack"]]),
        json!([["child_ids", "any", [["name", "=", "Jack"]]]]),
    );
    let three = |field: &str, op: &str| {
        let names = if field == "business_id" {
            ["SGC", "NID", "Free Jaffa Nation"]
        } else {
            ["Jack", "Sam", "Daniel"]
        };
        let mut items = vec![json!(op), json!(op)];
        items.extend(
            names
                .iter()
                .map(|n| json!([format!("{field}.name"), "=", n])),
        );
        let sub: Vec<serde_json::Value> = std::iter::repeat_n(json!(op), 2)
            .chain(names.iter().map(|n| json!(["name", "=", n])))
            .collect();
        (items, sub, names)
    };
    // many2one AND / OR -> one `any`.
    for op in ["&", "|"] {
        let (items, sub, _) = three("business_id", op);
        check(json!(items), json!([["business_id", "any", sub]]));
    }
    // one2many AND stays three `any`; OR -> one `any`.
    let (items, _, names) = three("child_ids", "&");
    let mut expected = vec![json!("&"), json!("&")];
    expected.extend(
        names
            .iter()
            .map(|n| json!(["child_ids", "any", [["name", "=", n]]])),
    );
    check(json!(items), json!(expected));
    let (items, sub, _) = three("child_ids", "|");
    check(json!(items), json!([["child_ids", "any", sub]]));

    // Negations.
    check(
        json!(["!", ["name", "=", "Jack"]]),
        json!([["name", "!=", "Jack"]]),
    );
    check(
        json!(["!", ["business_id.name", "=", "SGC"]]),
        json!([["business_id", "not any", [["name", "=", "SGC"]]]]),
    );
    check(
        json!(["!", ["child_ids.name", "=", "Jack"]]),
        json!([["child_ids", "not any", [["name", "=", "Jack"]]]]),
    );
    for op in ["&", "|"] {
        let (items, sub, _) = three("business_id", op);
        let mut negated = vec![json!("!")];
        negated.extend(items);
        check(json!(negated), json!([["business_id", "not any", sub]]));
    }
    let (items, _, names) = three("child_ids", "&");
    let mut negated = vec![json!("!")];
    negated.extend(items);
    let mut expected = vec![json!("|"), json!("|")];
    expected.extend(
        names
            .iter()
            .map(|n| json!(["child_ids", "not any", [["name", "=", n]]])),
    );
    check(json!(negated), json!(expected));
    let (items, sub, _) = three("child_ids", "|");
    let mut negated = vec![json!("!")];
    negated.extend(items);
    check(json!(negated), json!([["child_ids", "not any", sub]]));
}
