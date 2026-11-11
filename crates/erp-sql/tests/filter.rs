//! SQL shapes of scalar conditions (sql-shape cases compare structure,/§5).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_domain::{Domain, Operator, Value};
use erp_fields::{Date, Decimal, ErrorKind};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};
use erp_sql::filter::{Unaccent, WhereContext, compile_where, plan_where};
use erp_sql::order::compile_order;
use erp_sql::param::{Param, Scalar, ScalarKind};

static T: AddonDescriptor = AddonDescriptor {
    name: "t",
    depends: &["base"],
    exact_only: false,
};

static M: ModelDecl = ModelDecl::new("t", "t.m").table("t").fields(
    const {
        &[
            FieldDecl::integer("number"),
            FieldDecl::char("name"),
            FieldDecl::char("code").required(true),
            FieldDecl::date("date"),
            FieldDecl::datetime("moment"),
            FieldDecl::boolean("flag"),
            FieldDecl::boolean("req_flag").required(true),
            FieldDecl::float("x"),
            FieldDecl::float_digits("amount", erp_fields::Digits::Unrounded),
            FieldDecl::selection("state", &[("c", "C"), ("b", "B"), ("a", "A")]),
            FieldDecl::json("doc"),
            FieldDecl::many2one("partner_id", "t.partner"),
            FieldDecl::char("upper").compute("_compute_upper"),
        ]
    },
);

static P: ModelDecl = ModelDecl::new("t", "t.partner")
    .order("name")
    .fields(const { &[FieldDecl::char("name")] });

fn registry() -> Registry {
    RegistryBuilder::new([&T])
        .with_decls([&M, &P])
        .build()
        .expect("registry")
}

fn sql(domain: &Domain, unaccent: bool) -> (String, Vec<Param>) {
    let registry = registry();
    let model = registry.model("t.m").expect("model");
    let unaccent = if unaccent {
        Unaccent::Present
    } else {
        Unaccent::Off
    };
    let filter = compile_where(
        domain,
        &WhereContext {
            registry: &registry,
            model,
            unaccent,
            lang: None,
        },
    )
    .expect("compile");
    (filter.sql, filter.params)
}

/// SQL compared structurally: whitespace runs collapsed, no space inside parentheses.
fn norm(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("( ", "(")
        .replace(" )", ")")
}

macro_rules! assert_sql {
    ($actual:expr, $expected:expr $(, $($msg:tt)*)?) => {
        assert_eq!(norm(&$actual), norm(&$expected) $(, $($msg)*)?)
    };
}

fn cond(field: &str, op: Operator, value: impl Into<Value>) -> Domain {
    Domain::condition(field, op, value).expect("condition")
}

fn ints(values: &[i64]) -> Param {
    Param::Array(
        ScalarKind::Int8,
        values.iter().map(|v| Scalar::Int8(*v)).collect(),
    )
}

fn text(s: &str) -> Param {
    Param::Scalar(Scalar::Text(s.into()))
}

#[test]
fn spec_field_044_in_with_falsy_values() {
    let (s, p) = sql(
        &cond(
            "number",
            Operator::In,
            Value::list([Value::Int(42), Value::FALSE]),
        ),
        false,
    );
    assert_sql!(
        s,
        "(\"t\".\"number\" = ANY($1::int8[]) OR \"t\".\"number\" IS NULL)"
    );
    assert_eq!(p, [ints(&[42, 0])]);

    let (s, p) = sql(&cond("name", Operator::In, Value::list([""])), false);
    assert_sql!(
        s,
        "(\"t\".\"name\" = ANY($1::text[]) OR \"t\".\"name\" IS NULL)"
    );
    assert_eq!(
        p,
        [Param::Array(
            ScalarKind::Text,
            vec![Scalar::Text("".into())]
        )]
    );

    let (s, p) = sql(&cond("date", Operator::In, Value::list([false])), false);
    assert_eq!((s.as_str(), p.len()), ("\"t\".\"date\" IS NULL", 0));
    let (s, _) = sql(&cond("date", Operator::NotIn, Value::list([false])), false);
    assert_sql!(s, "\"t\".\"date\" IS NOT NULL");

    let (s, p) = sql(&cond("number", Operator::NotIn, Value::list([42])), false);
    assert_sql!(
        s,
        "(\"t\".\"number\" <> ALL($1::int8[]) OR \"t\".\"number\" IS NULL)"
    );
    assert_eq!(p, [ints(&[42])]);

    // `id` and required columns are NOT NULL: no IS NULL branch, and `in [False]` is FALSE.
    let (s, _) = sql(&cond("id", Operator::In, Value::list([false])), false);
    assert_sql!(s, "FALSE");
    let (s, _) = sql(&cond("id", Operator::NotIn, Value::list([3])), false);
    assert_sql!(s, "\"t\".\"id\" <> ALL($1::int8[])");
    let (s, _) = sql(&cond("code", Operator::NotIn, Value::list(["a"])), false);
    assert_sql!(s, "\"t\".\"code\" <> ALL($1::text[])");
    // `''` is the char null value: "not set" also excludes empty strings
    let (s, p) = sql(&cond("code", Operator::NotIn, Value::list([false])), false);
    assert_sql!(s, "\"t\".\"code\" <> ALL($1::text[])");
    assert_eq!(
        p,
        [Param::Array(
            ScalarKind::Text,
            vec![Scalar::Text("".into())]
        )]
    );
    let (s, _) = sql(&cond("id", Operator::NotIn, Value::list([false])), false);
    assert_sql!(s, "TRUE");

    // 0 in the set already means "or NULL" for integers.
    let (s, _) = sql(&cond("number", Operator::In, Value::list([0])), false);
    assert_sql!(
        s,
        "(\"t\".\"number\" = ANY($1::int8[]) OR \"t\".\"number\" IS NULL)"
    );
    let (s, _) = sql(&cond("number", Operator::NotIn, Value::list([0])), false);
    assert_sql!(s, "\"t\".\"number\" <> ALL($1::int8[])");
}

#[test]
fn spec_field_045_inequalities_with_falsy_values() {
    let (s, p) = sql(&cond("number", Operator::Lt, 1), false);
    assert_sql!(
        s,
        "(\"t\".\"number\" < $1::int8 OR \"t\".\"number\" IS NULL)"
    );
    assert_eq!(p, [Param::Scalar(Scalar::Int8(1))]);
    let (s, _) = sql(&cond("number", Operator::Gt, 1), false);
    assert_sql!(s, "\"t\".\"number\" > $1::int8");
    let (s, _) = sql(&cond("number", Operator::Ge, 0), false);
    assert_sql!(
        s,
        "(\"t\".\"number\" >= $1::int8 OR \"t\".\"number\" IS NULL)"
    );
    let (s, p) = sql(&cond("date", Operator::Lt, "2020-01-01"), false);
    assert_sql!(s, "\"t\".\"date\" < $1::date");
    assert_eq!(
        p,
        [Param::Scalar(Scalar::Date(
            Date::from_ymd(2020, 1, 1).expect("date")
        ))]
    );
    let (s, _) = sql(&cond("name", Operator::Lt, "b"), false);
    assert_sql!(s, "(\"t\".\"name\" < $1::text OR \"t\".\"name\" IS NULL)");
    let (s, _) = sql(&cond("name", Operator::Gt, "b"), false);
    assert_sql!(s, "\"t\".\"name\" > $1::text");
    let (_, p) = sql(&cond("amount", Operator::Gt, 1.5), false);
    assert_eq!(
        p,
        [Param::Scalar(Scalar::Numeric(
            "1.5".parse::<Decimal>().expect("decimal")
        ))]
    );
}

#[test]
fn spec_field_046_like_family() {
    let (s, p) = sql(&cond("name", Operator::Like, "ab"), false);
    assert_sql!(s, "\"t\".\"name\" LIKE $1::text");
    assert_eq!(p, [text("%ab%")]);
    let (s, p) = sql(&cond("name", Operator::EqLike, "a_%"), false);
    assert_sql!(s, "\"t\".\"name\" LIKE $1::text");
    assert_eq!(p, [text("a_%")]);
    let (s, p) = sql(&cond("name", Operator::ILike, "Ab"), true);
    assert_sql!(s, "unaccent(\"t\".\"name\") ILIKE unaccent($1::text)");
    assert_eq!(p, [text("%Ab%")]);
    let (s, _) = sql(&cond("name", Operator::NotILike, "ab"), true);
    assert_sql!(
        s,
        "(unaccent(\"t\".\"name\") NOT ILIKE unaccent($1::text) OR \"t\".\"name\" IS NULL)"
    );
    let (s, _) = sql(&cond("name", Operator::NotILike, "ab"), false);
    assert_sql!(
        s,
        "(\"t\".\"name\" NOT ILIKE $1::text OR \"t\".\"name\" IS NULL)"
    );
    let (s, p) = sql(&cond("number", Operator::Like, "4"), false);
    assert_sql!(s, "\"t\".\"number\"::text LIKE $1::text");
    assert_eq!(p, [text("%4%")]);
    let (s, _) = sql(&cond("code", Operator::NotLike, "x"), false);
    assert_sql!(
        s,
        "\"t\".\"code\" NOT LIKE $1::text",
        "required: no IS NULL"
    );
}

#[test]
fn spec_field_047_rejected_conditions() {
    let registry = registry();
    let model = registry.model("t.m").expect("model");
    let ctx = WhereContext {
        registry: &registry,
        model,
        unaccent: Unaccent::Off,
        lang: None,
    };
    let err = compile_where(&cond("name", Operator::ChildOf, 1), &ctx).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Invalid operator 'child_of' for SQL in domain term ('name', 'child_of', 1)"
    );
    let err = compile_where(&cond("name", Operator::AnyBypass, 5), &ctx).unwrap_err();
    assert_eq!(
        err.message(),
        "condition_to_sql() operator 'any!' accepts SQL or Query, got 5"
    );
    let err = compile_where(&cond("name", Operator::Eq, "x"), &ctx).unwrap_err();
    assert!(err.message().starts_with("Invalid operator '='"), "{err}");
    // non-stored field: no SQL representation
    let err = compile_where(&cond("upper", Operator::In, Value::list(["X"])), &ctx).unwrap_err();
    assert!(
        err.message()
            .starts_with("Field has no SQL representation because it is not stored"),
        "{err}"
    );
    // property conditions arrive later, with a clear message
    let err = compile_where(
        &cond("date.month_number", Operator::In, Value::list([2])),
        &ctx,
    )
    .unwrap_err();
    assert!(err.message().contains("not supported until"), "{err}");
    // a literal that does not convert: Programming for Rust-built domains, Validation for input
    let err = compile_where(&cond("number", Operator::In, Value::list(["abc"])), &ctx).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    let external =
        Domain::from_json(&serde_json::json!([["number", "in", ["abc"]]])).expect("parse");
    let err = compile_where(&external, &ctx).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(err.message().contains("t.m.number"), "{err}");
}

#[test]
fn spec_field_018_boolean_sql_shape() {
    let (s, p) = sql(&cond("flag", Operator::In, Value::list([true])), false);
    assert_eq!((s.as_str(), p.len()), ("\"t\".\"flag\" IS TRUE", 0));
    let (s, _) = sql(&cond("flag", Operator::NotIn, Value::list([true])), false);
    assert_sql!(s, "\"t\".\"flag\" IS NOT TRUE");
    let (s, _) = sql(
        &cond("flag", Operator::In, Value::list([true, false])),
        false,
    );
    assert_sql!(s, "TRUE");
    let (s, _) = sql(
        &cond("flag", Operator::NotIn, Value::list([true, false])),
        false,
    );
    assert_sql!(s, "FALSE");
}

/// surviving NOT as `(…) IS NOT TRUE`.
#[test]
fn spec_dom_036_boolean_logic_shape() {
    let d = Domain::and([
        cond("flag", Operator::In, Value::list([true])),
        cond("name", Operator::Like, "foo"),
        Domain::or([
            cond("partner_id", Operator::In, Value::list([1])),
            cond("name", Operator::NotIn, Value::list(["john@example.com"])),
        ]),
    ]);
    let (s, p) = sql(&d, false);
    assert_sql!(
        s,
        "(\"t\".\"flag\" IS TRUE AND \"t\".\"name\" LIKE $1::text AND (\"t\".\"partner_id\" = \
         ANY($2::int8[]) OR (\"t\".\"name\" <> ALL($3::text[]) OR \"t\".\"name\" IS NULL)))"
    );
    assert_eq!(p.len(), 3);
}

/// `id` orders by the raw column;: unknown field.
#[test]
fn spec_dom_056_086_087_order_shapes() {
    let registry = registry();
    let model = registry.model("t.m").expect("model");
    let order = compile_order(&registry, model, "flag, req_flag desc, state", None).expect("order");
    assert_sql!(
        order.sql,
        "COALESCE(\"t\".\"flag\", FALSE), \"t\".\"req_flag\" DESC, \
         array_position(ARRAY['c', 'b', 'a']::text[], \"t\".\"state\"::text), \"t\".\"id\""
    );
    let order =
        compile_order(&registry, model, "partner_id.id desc nulls last", None).expect("order");
    assert_sql!(
        order.sql,
        "\"t\".\"partner_id\" DESC NULLS LAST, \"t\".\"id\"",
        "`id` appended as tiebreaker"
    );
    let order = compile_order(&registry, model, "number desc, id desc", None).expect("order");
    assert_sql!(order.sql, "\"t\".\"number\" DESC, \"t\".\"id\" DESC");
    // a comodel ordered by `name`: joined and ordered by its own order
    let order = compile_order(&registry, model, "partner_id", None).expect("order");
    assert_sql!(order.sql, "\"t__partner_id\".\"name\", \"t\".\"id\"");
    assert_eq!(
        order.joins,
        [(
            "t__partner_id".to_owned(),
            " LEFT JOIN \"t_partner\" AS \"t__partner_id\" ON (\"t\".\"partner_id\" = \
             \"t__partner_id\".\"id\")"
                .to_owned()
        )]
    );
    let err = compile_order(&registry, model, "partner_id.name", None).unwrap_err();
    assert_eq!(
        err.message(),
        "Cannot order by comodel fields in 'partner_id.name'"
    );
    let err = compile_order(&registry, model, "zzz", None).unwrap_err();
    assert_eq!(err.message(), "Invalid field 'zzz' on model 't.m'");
    assert_eq!(err.kind(), ErrorKind::Programming);
    let err = compile_order(&registry, model, "name; drop", None).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    // default: the model's _order ('id')
    assert_eq!(
        compile_order(&registry, model, "", None)
            .expect("order")
            .sql,
        "\"t\".\"id\"",
        "the model's _order is `id`: no tiebreaker added"
    );
}

#[test]
fn spec_field_050_sql_for_non_stored_field() {
    let registry = registry();
    let model = registry.model("t.m").expect("model");
    let err = compile_order(&registry, model, "upper", None).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Cannot convert t.m.upper to SQL because it is not stored"
    );
}

/// (memo key, S2 review item 2): the shape key holds no literal, so `1`, `1.0` and
/// `True` share a key (and a text) while their parameters stay their own; every value-dependent
/// decision that changes the text is part of the key.
#[test]
fn shape_keys_lift_literals_but_keep_decisions() {
    let registry = registry();
    let model = registry.model("t.m").expect("model");
    let ctx = WhereContext {
        registry: &registry,
        model,
        unaccent: Unaccent::Off,
        lang: None,
    };
    let plan = |d: Domain| plan_where(&d, &ctx).expect("plan");
    let int = plan(cond("number", Operator::Lt, Value::Int(7)));
    let float = plan(cond("number", Operator::Lt, Value::Float(7.0)));
    let truthy = plan(cond("number", Operator::Lt, Value::Bool(true)));
    assert_eq!(int.key, float.key);
    assert_eq!(int.key, truthy.key);
    assert_eq!(int.params, [Param::Scalar(Scalar::Int8(7))]);
    assert_eq!(truthy.params, [Param::Scalar(Scalar::Int8(1))]);
    // `number < 1` accepts NULL rows (0 < 1), `number < -1` does not: different texts
    let lt_one = plan(cond("number", Operator::Lt, 1));
    let lt_minus = plan(cond("number", Operator::Lt, -1));
    assert_ne!(lt_one.key, lt_minus.key);
    // `False` in a set adds an IS NULL branch
    let plain = plan(cond("number", Operator::In, Value::list([5])));
    let with_false = plan(cond(
        "number",
        Operator::In,
        Value::list([Value::Int(5), Value::FALSE]),
    ));
    assert_ne!(plain.key, with_false.key);
    // the number of values never matters: one array parameter
    let many = plan(cond("number", Operator::In, Value::list([5, 6, 7, 8])));
    assert_eq!(plain.key, many.key);
    // the field and the parameter type are part of the key
    assert_ne!(
        plan(cond("x", Operator::Lt, 7)).key,
        plan(cond("number", Operator::Lt, 7)).key
    );
    assert_ne!(
        plan(cond("name", Operator::In, Value::list(["5"]))).key,
        plan(cond("number", Operator::In, Value::list([5]))).key
    );
}

static REL_TAG: ModelDecl = ModelDecl::new("t", "t.tag").fields(
    const {
        &[
            FieldDecl::char("name"),
            FieldDecl::many2one("m_id", "t.x2m"),
        ]
    },
);

static REL_X2M: ModelDecl = ModelDecl::new("t", "t.x2m").table("t_x2m").fields(
    const {
        &[
            FieldDecl::many2many("tag_ids", "t.tag"),
            FieldDecl::one2many("child_ids", "t.tag", "m_id"),
            FieldDecl::char("non_stored").compute("_compute_non_stored"),
        ]
    },
);

#[test]
fn spec_rel_077_x2many_cannot_be_used_as_sql_column() {
    let reg = RegistryBuilder::new([&T])
        .with_decls([&REL_X2M, &REL_TAG])
        .build()
        .unwrap();

    let model = reg.model("t.x2m").unwrap();
    let ctx = WhereContext {
        registry: &reg,
        model,
        unaccent: Unaccent::Off,
        lang: None,
    };

    // 1. Calling SQL filter compilation on a many2many field fails
    let domain_m2m = cond("tag_ids", Operator::In, vec![1]);
    let err = compile_where(&domain_m2m, &ctx).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message()
            .starts_with("Cannot generate SQL for multi-relational field t.x2m.tag_ids"),
        "{err}"
    );

    // 2. Calling SQL filter compilation on a one2many field fails
    let domain_o2m = cond("child_ids", Operator::In, vec![1]);
    let err_o2m = compile_where(&domain_o2m, &ctx).unwrap_err();
    assert_eq!(err_o2m.kind(), ErrorKind::Programming);
    assert!(
        err_o2m
            .message()
            .starts_with("Cannot generate SQL for multi-relational field t.x2m.child_ids"),
        "{err_o2m}"
    );

    // 3. Calling SQL filter compilation on a non-stored field fails
    let domain_non_stored = cond("non_stored", Operator::In, vec!["foo"]);
    let err2 = compile_where(&domain_non_stored, &ctx).unwrap_err();
    assert_eq!(err2.kind(), ErrorKind::Programming);
    assert!(
        err2.message()
            .starts_with("Field has no SQL representation because it is not stored"),
        "{err2}"
    );
}

/// 5: `blind_id_list` rewrites what the compiler renders for a many2one `in` an id
/// list (the one2many inverse read's filter), so the textual coupling is pinned here.
#[test]
fn the_rendered_id_list_of_a_many2one_in_is_what_blind_id_list_rewrites() {
    let (s, params) = sql(&cond("partner_id", Operator::In, vec![3_i64, 1, 2]), false);
    assert_eq!(params.len(), 1, "{params:?}");
    assert_eq!(
        erp_sql::select::blind_id_list(&s).as_deref(),
        Some("\"t\".\"partner_id\" = ANY((SELECT $1::int8[])::int8[])"),
        "{s}"
    );
}
