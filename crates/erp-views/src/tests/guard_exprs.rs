//! Tests for the expression attribute walk (GA-3 WP-A,
//! §23.1, §23.3). The walk is pure: a `StaticLookup` stands in for the registry.

use super::guard_capture::{ROOT, StaticLookup};
use crate::exprs::{AttrKind, ExprProblem, ExprRef, Site, walk};
use crate::guard::{GuardLookup, echo};
use crate::validate::ArchIssue;

const LINE: &str = "res.line";
const PRODUCT: &str = "res.product";

fn lookup() -> StaticLookup {
    StaticLookup::new(3)
        .field(ROOT, "name", None, None)
        .field(ROOT, "state", None, None)
        .field(ROOT, "amount", None, None)
        .field(ROOT, "secret", Some("base.group_system"), None)
        .field(ROOT, "line_ids", None, Some(LINE))
        .field(ROOT, "parent_id", None, Some(ROOT))
        .field(LINE, "product_id", None, Some(PRODUCT))
        .field(LINE, "qty", None, None)
        .field(LINE, "cost", None, None)
        .field(PRODUCT, "name", None, None)
}

fn site(kind: AttrKind, model: &'static str) -> Site<'static> {
    Site {
        kind,
        model,
        parent: None,
        comodel: None,
    }
}

fn in_sub_view(kind: AttrKind) -> Site<'static> {
    Site {
        kind,
        model: LINE,
        parent: Some(ROOT),
        comodel: None,
    }
}

fn run(site: &Site<'_>, value: &str) -> Result<Vec<(String, String)>, ExprProblem> {
    let lookup = lookup();
    let source: &dyn GuardLookup = &lookup;
    walk(site, value, source).map(|refs| {
        refs.into_iter()
            .map(|ExprRef { model, path }| (model.into_string(), path.into_string()))
            .collect()
    })
}

fn refs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(m, p)| ((*m).to_owned(), (*p).to_owned()))
        .collect()
}

fn problem(site: &Site<'_>, value: &str) -> ExprProblem {
    run(site, value).expect_err("the attribute is refused")
}

#[test]
fn root_names_resolve_against_the_node_model() {
    let got = run(
        &site(AttrKind::Expression, ROOT),
        "state == 'draft' and not name",
    )
    .expect("walks");
    assert_eq!(got, refs(&[(ROOT, "state"), (ROOT, "name")]));
}

#[test]
fn a_repeated_name_is_one_reference() {
    let got = run(
        &site(AttrKind::Expression, ROOT),
        "state and state or state",
    )
    .expect("walks");
    assert_eq!(got, refs(&[(ROOT, "state")]));
}

#[test]
fn provided_names_are_not_references() {
    let got = run(
        &site(AttrKind::Expression, ROOT),
        "context.get('x') or uid or bool(today) or len(allowed_company_ids)",
    )
    .expect("walks");
    assert_eq!(got, refs(&[]));
}

#[test]
fn attribute_access_reads_the_root_name() {
    let got = run(&site(AttrKind::Expression, ROOT), "line_ids.qty > 1").expect("walks");
    assert_eq!(got, refs(&[(ROOT, "line_ids")]));
    let got = run(
        &site(AttrKind::Expression, ROOT),
        "name.lower() == str(state) or relativedelta(days=amount)",
    )
    .expect("walks");
    assert_eq!(
        got,
        refs(&[(ROOT, "name"), (ROOT, "state"), (ROOT, "amount")])
    );
}

#[test]
fn parent_reads_the_enclosing_model_in_a_sub_view() {
    let got = run(
        &in_sub_view(AttrKind::Expression),
        "parent.state == 'draft' and qty > 1 and parent.name.upper()",
    )
    .expect("walks");
    assert_eq!(got, refs(&[(ROOT, "state"), (LINE, "qty"), (ROOT, "name")]));
}

#[test]
fn parent_outside_a_sub_view_is_refused() {
    let p = problem(&site(AttrKind::Expression, ROOT), "parent.state");
    assert_eq!(p, ExprProblem::ParentOutsideSubView);
    assert_eq!(
        p.to_string(),
        "'parent' is only available inside the sub-view of a relational field"
    );
}

#[test]
fn parent_field_must_exist_on_the_enclosing_model() {
    let p = problem(&in_sub_view(AttrKind::Expression), "parent.qty");
    assert_eq!(
        p,
        ExprProblem::UnknownField {
            model: ROOT.to_owned(),
            field: "qty".to_owned()
        }
    );
    assert_eq!(
        p.to_string(),
        "Field \"qty\" does not exist in model \"res.partner\""
    );
}

#[test]
fn constant_fast_path_reads_nothing() {
    for value in ["0", "1", "True", "False", "TRUE", "false", "", "   "] {
        let got = run(&site(AttrKind::Expression, ROOT), value).expect("walks");
        assert_eq!(got, refs(&[]), "{value:?}");
    }
}

#[test]
fn unknown_name_is_refused() {
    let p = problem(&site(AttrKind::Expression, ROOT), "state and nope");
    assert_eq!(
        p,
        ExprProblem::UnknownField {
            model: ROOT.to_owned(),
            field: "nope".to_owned()
        }
    );
}

#[test]
fn unparseable_source_is_a_syntax_problem() {
    let source = "state ==";
    let expected = erp_expr::parse_with_limits(source, erp_expr::EvalLimits::default())
        .expect_err("does not parse")
        .to_string();
    let p = problem(&site(AttrKind::Expression, ROOT), source);
    assert_eq!(p, ExprProblem::Syntax(expected.clone()));
    assert_eq!(p.to_string(), expected);
}

#[test]
fn oversized_source_is_refused() {
    let source = format!("state or {}name", "name or ".repeat(9_000));
    assert!(source.len() > 64 * 1024);
    let expected = erp_expr::parse_with_limits(&source, erp_expr::EvalLimits::default())
        .expect_err("is over the bound")
        .to_string();
    let p = problem(&site(AttrKind::Expression, ROOT), &source);
    assert_eq!(p, ExprProblem::Syntax(expected));
}

#[test]
fn unsupported_constructs_are_refused() {
    let s = site(AttrKind::Expression, ROOT);
    let cases = [
        ("(x := state)", "an assignment expression (:=)"),
        ("[n for n in name]", "a comprehension"),
        ("f'{state}'", "an f-string"),
        ("bool(*name)", "a starred expression"),
    ];
    for (source, what) in cases {
        let p = problem(&s, source);
        assert_eq!(p, ExprProblem::Unsupported(what), "{source}");
        assert_eq!(
            p.to_string(),
            format!("{what} is not supported in a view expression")
        );
    }
}

#[test]
fn field_names_the_plan_cannot_hold_are_refused() {
    let s = site(AttrKind::FilterDomain, ROOT);
    let p = problem(&s, "[('\u{e9}t\u{e9}', '=', 1)]");
    assert_eq!(p, ExprProblem::BadName("\u{e9}t\u{e9}".to_owned()));
    assert_eq!(
        p.to_string(),
        "'\u{e9}t\u{e9}' is not a field name (ASCII letters, digits and '_', at most 128 bytes)"
    );
    let long = "n".repeat(129);
    let p = problem(&s, &format!("[('{long}', '=', 1)]"));
    assert_eq!(p, ExprProblem::BadName(echo(&long)));
}

#[test]
fn field_domain_splits_left_and_right() {
    let s = Site {
        kind: AttrKind::FieldDomain,
        model: ROOT,
        parent: None,
        comodel: Some(LINE),
    };
    let got = run(
        &s,
        "[('qty', '>', amount), ('product_id.name', 'ilike', name), '|', ('cost', '=', 0), ('cost', '=', state)]",
    )
    .expect("walks");
    assert_eq!(
        got,
        refs(&[
            (LINE, "qty"),
            (ROOT, "amount"),
            (LINE, "product_id.name"),
            (ROOT, "name"),
            (LINE, "cost"),
            (ROOT, "state"),
        ])
    );
}

#[test]
fn field_domain_right_side_reads_parent_in_a_sub_view() {
    let s = Site {
        kind: AttrKind::FieldDomain,
        model: LINE,
        parent: Some(ROOT),
        comodel: Some(PRODUCT),
    };
    let got = run(&s, "[('name', '=', parent.state)]").expect("walks");
    assert_eq!(got, refs(&[(PRODUCT, "name"), (ROOT, "state")]));
}

#[test]
fn domain_on_a_non_relational_field_is_refused() {
    let p = problem(&site(AttrKind::FieldDomain, ROOT), "[('name', '=', 1)]");
    assert_eq!(p, ExprProblem::NonRelationalDomain);
    assert_eq!(
        p.to_string(),
        "a domain on a field that is not relational makes no sense"
    );
}

#[test]
fn filter_domain_reads_the_view_model() {
    let got = run(
        &site(AttrKind::FilterDomain, ROOT),
        "[('state', '=', 'draft'), ('line_ids.qty', '>', amount)]",
    )
    .expect("walks");
    assert_eq!(
        got,
        refs(&[(ROOT, "state"), (ROOT, "line_ids.qty"), (ROOT, "amount")])
    );
}

#[test]
fn filter_domain_ignores_self() {
    let got = run(
        &site(AttrKind::FilterDomain, ROOT),
        "['|', ('name', 'ilike', self), ('state', '=', self)]",
    )
    .expect("walks");
    assert_eq!(got, refs(&[(ROOT, "name"), (ROOT, "state")]));
}

#[test]
fn domain_structures_follow_the_reference_rules() {
    let s = site(AttrKind::FilterDomain, ROOT);
    assert_eq!(run(&s, "state").expect("name"), refs(&[(ROOT, "state")]));
    assert_eq!(
        run(&s, "[('name', '=', 1)] if amount else [('state', '=', 1)]").expect("if"),
        refs(&[(ROOT, "amount"), (ROOT, "name"), (ROOT, "state")])
    );
    assert_eq!(
        run(&s, "amount and [('name', '=', 1)]").expect("and"),
        refs(&[(ROOT, "amount"), (ROOT, "name")])
    );
    assert_eq!(
        run(&s, "[('name', '=', 1)] + [('state', '=', 1)]").expect("sum"),
        refs(&[(ROOT, "name"), (ROOT, "state")])
    );
    // the deprecated leaf forms: a numeric left side, and a python expression compared with 1
    assert_eq!(
        run(&s, "[(1, '=', 1), (state, '=', 1)]").expect("legacy leaves"),
        refs(&[(ROOT, "state")])
    );
}

#[test]
fn malformed_domains_are_refused() {
    let s = site(AttrKind::FilterDomain, ROOT);
    let cases = [
        ("name()", "the domain is not a list of leaves"),
        (
            "[('name', '=')]",
            "a domain leaf must have three elements (field, operator, value)",
        ),
        (
            "[state]",
            "a domain item must be an operator, True, False or a leaf",
        ),
        (
            "['x']",
            "a domain item must be an operator, True, False or a leaf",
        ),
        (
            "[(name, '=', 2)]",
            "the left side of a domain leaf must be a field name",
        ),
    ];
    for (source, message) in cases {
        let p = problem(&s, source);
        assert_eq!(p, ExprProblem::NotADomain(message.to_owned()), "{source}");
        assert_eq!(p.to_string(), message);
    }
}

#[test]
fn left_paths_are_resolved_hop_by_hop() {
    let s = site(AttrKind::FilterDomain, ROOT);
    let eight = format!("{}state", "parent_id.".repeat(7));
    assert_eq!(
        run(&s, &format!("[('{eight}', '=', 1)]")).expect("8 hops"),
        refs(&[(ROOT, &eight)])
    );
    let nine = format!("{}state", "parent_id.".repeat(8));
    let p = problem(&s, &format!("[('{nine}', '=', 1)]"));
    assert_eq!(
        p,
        ExprProblem::TooManyHops {
            path: echo(&nine),
            hops: 9,
            max: 8
        }
    );
    assert_eq!(
        p.to_string(),
        format!(
            "the path '{}' has 9 hops: at most 8 are allowed",
            echo(&nine)
        )
    );

    let p = problem(&s, "[('name.x', '=', 1)]");
    assert_eq!(
        p,
        ExprProblem::NotRelationalHop {
            model: ROOT.to_owned(),
            field: "name".to_owned(),
            path: "name.x".to_owned()
        }
    );
    assert_eq!(
        p.to_string(),
        "field \"name\" of model \"res.partner\" is not relational, so the path 'name.x' cannot continue through it"
    );

    let p = problem(&s, "[('line_ids.nope', '=', 1)]");
    assert_eq!(
        p,
        ExprProblem::UnknownField {
            model: LINE.to_owned(),
            field: "nope".to_owned()
        }
    );
}

#[test]
fn left_path_starting_at_parent_reads_nothing() {
    let got = run(
        &site(AttrKind::FilterDomain, ROOT),
        "[('parent.x', '=', 1)]",
    )
    .expect("walks");
    assert_eq!(got, refs(&[]));
}

#[test]
fn default_order_reads_plain_field_names() {
    let list = site(AttrKind::ListOrder, ROOT);
    assert_eq!(
        run(&list, "name, state DESC , amount asc").expect("walks"),
        refs(&[(ROOT, "name"), (ROOT, "state"), (ROOT, "amount")])
    );
    let p = problem(&list, "nope");
    assert_eq!(
        p,
        ExprProblem::UnknownField {
            model: ROOT.to_owned(),
            field: "nope".to_owned()
        }
    );
    for (source, message) in [
        ("name sideways", "'sideways' is not asc or desc"),
        (
            "name asc extra",
            "'name asc extra' is not a field name with an optional direction",
        ),
        ("name,,state", "an empty term in the order"),
    ] {
        let p = problem(&list, source);
        assert_eq!(p, ExprProblem::Order(message.to_owned()), "{source}");
        assert_eq!(p.to_string(), message);
    }
}

#[test]
fn pivot_order_is_one_measure() {
    let pivot = site(AttrKind::PivotOrder, ROOT);
    assert_eq!(run(&pivot, "__count desc").expect("count"), refs(&[]));
    assert_eq!(
        run(&pivot, "amount").expect("field"),
        refs(&[(ROOT, "amount")])
    );
    let p = problem(&pivot, "amount, name");
    assert_eq!(
        p,
        ExprProblem::Order("a pivot order is one measure and an optional direction".to_owned())
    );
    // the row count is a pivot measure only
    let list = site(AttrKind::ListOrder, ROOT);
    assert_eq!(
        problem(&list, "__count"),
        ExprProblem::UnknownField {
            model: ROOT.to_owned(),
            field: "__count".to_owned()
        }
    );
}

#[test]
fn context_reads_names_in_values() {
    let got = run(
        &site(AttrKind::Context, ROOT),
        "{'default_state': state, 'k': [amount, context.get('z')]}",
    )
    .expect("walks");
    assert_eq!(got, refs(&[(ROOT, "state"), (ROOT, "amount")]));
}

#[test]
fn context_group_by_is_read_as_field_names() {
    let s = site(AttrKind::Context, ROOT);
    assert_eq!(
        run(&s, "{'group_by': 'name'}").expect("walks"),
        refs(&[(ROOT, "name")])
    );
    assert_eq!(
        run(&s, "{'group_by': ['state', 'amount:sum']}").expect("walks"),
        refs(&[(ROOT, "state"), (ROOT, "amount")])
    );
    assert_eq!(
        run(&s, "{'group_by': ('name',)}").expect("walks"),
        refs(&[(ROOT, "name")])
    );
    assert_eq!(
        run(&s, "relativedelta(group_by='state:month')").expect("walks"),
        refs(&[(ROOT, "state")])
    );
    // a restricted field is read like any other: the guard sees it
    assert_eq!(
        run(&s, "{'group_by': 'secret'}").expect("walks"),
        refs(&[(ROOT, "secret")])
    );
}

#[test]
fn context_group_by_of_another_type_is_refused() {
    let s = site(AttrKind::Context, ROOT);
    for source in [
        "{'group_by': name}",
        "{'group_by': 1}",
        "{'group_by': ['name', amount]}",
        "{'group_by': 'name' if state else 'amount'}",
        "relativedelta(group_by=name)",
    ] {
        let p = problem(&s, source);
        assert_eq!(
            p,
            ExprProblem::Unsupported(
                "a 'group_by' value that is not a string or a list of strings"
            ),
            "{source}"
        );
        assert_eq!(
            p.to_string(),
            "a 'group_by' value that is not a string or a list of strings is not supported in a view expression"
        );
    }
    let p = problem(&s, "{'group_by': 'nope'}");
    assert_eq!(
        p,
        ExprProblem::UnknownField {
            model: ROOT.to_owned(),
            field: "nope".to_owned()
        }
    );
    let p = problem(&s, "{name: 1}");
    assert_eq!(
        p,
        ExprProblem::Unsupported("a context key that is not a string literal")
    );
}

#[test]
fn group_by_outside_a_context_is_not_read() {
    let got = run(&site(AttrKind::Expression, ROOT), "{'group_by': 'name'}").expect("walks");
    assert_eq!(got, refs(&[]));
}

#[test]
fn filter_date_attributes_are_field_names() {
    for name in ["date", "end_date"] {
        assert_eq!(
            AttrKind::of("filter", name),
            Some(AttrKind::FieldName),
            "{name}"
        );
        assert_eq!(AttrKind::of("field", name), None, "{name}");
    }
    let s = site(AttrKind::FieldName, ROOT);
    assert_eq!(run(&s, "amount").expect("walks"), refs(&[(ROOT, "amount")]));
    assert_eq!(run(&s, "secret").expect("walks"), refs(&[(ROOT, "secret")]));
    assert_eq!(
        problem(&s, "nope"),
        ExprProblem::UnknownField {
            model: ROOT.to_owned(),
            field: "nope".to_owned()
        }
    );
    assert_eq!(problem(&s, "a.b"), ExprProblem::BadName("a.b".to_owned()));
}

#[test]
fn only_the_listed_attributes_are_read() {
    for name in [
        "invisible",
        "readonly",
        "required",
        "column_invisible",
        "decoration-danger",
    ] {
        assert_eq!(
            AttrKind::of("field", name),
            Some(AttrKind::Expression),
            "{name}"
        );
        assert_eq!(
            AttrKind::of("button", name),
            Some(AttrKind::Expression),
            "{name}"
        );
    }
    for tag in ["field", "filter", "button"] {
        assert_eq!(
            AttrKind::of(tag, "context"),
            Some(AttrKind::Context),
            "{tag}"
        );
    }
    assert_eq!(AttrKind::of("field", "domain"), Some(AttrKind::FieldDomain));
    assert_eq!(
        AttrKind::of("filter", "domain"),
        Some(AttrKind::FilterDomain)
    );
    assert_eq!(
        AttrKind::of("field", "filter_domain"),
        Some(AttrKind::FilterDomain)
    );
    assert_eq!(
        AttrKind::of("list", "default_order"),
        Some(AttrKind::ListOrder)
    );
    assert_eq!(
        AttrKind::of("pivot", "default_order"),
        Some(AttrKind::PivotOrder)
    );
    for (tag, name) in [
        ("div", "t-if"),
        ("div", "t-elif"),
        ("div", "t-att-class"),
        ("field", "options"),
        ("field", "widget"),
        ("button", "domain"),
        ("form", "default_order"),
        ("filter", "filter_domain"),
        ("calendar", "date_start"),
        ("field", "date"),
    ] {
        assert_eq!(AttrKind::of(tag, name), None, "{tag} {name}");
    }
}

#[test]
fn problems_become_arch_issues_with_the_origin() {
    let arch = super::arch("<form>\n<field name=\"name\" invisible=\"nope\"/></form>");
    let origin = arch.origin(arch.root_id());

    let unknown = ExprProblem::UnknownField {
        model: ROOT.to_owned(),
        field: "nope".to_owned(),
    }
    .into_issue("invisible", "nope", origin);
    assert_eq!(
        unknown,
        ArchIssue::UnknownFieldInExpression {
            attribute: "invisible".to_owned(),
            field: "nope".to_owned(),
            model: ROOT.to_owned(),
            origin,
        }
    );
    assert_eq!(
        unknown.to_string(),
        "Field \"nope\" used in the invisible attribute does not exist in model \"res.partner\""
    );
    assert_eq!(unknown.origin(), origin);

    let invalid = ExprProblem::ParentOutsideSubView.into_issue("invisible", "parent.x", origin);
    assert_eq!(
        invalid,
        ArchIssue::InvalidExpression {
            attribute: "invisible".to_owned(),
            value: "parent.x".to_owned(),
            problem: "'parent' is only available inside the sub-view of a relational field"
                .to_owned(),
            origin,
        }
    );
    assert_eq!(
        invalid.to_string(),
        "Invalid expression in the invisible attribute 'parent.x': 'parent' is only available inside the sub-view of a relational field"
    );
}

#[test]
fn a_bare_parent_is_refused() {
    for source in ["parent", "parent['qty']", "len(parent)", "qty or parent"] {
        let p = problem(&in_sub_view(AttrKind::Expression), source);
        assert_eq!(
            p,
            ExprProblem::Unsupported("'parent' other than as `parent.<field>`"),
            "{source}"
        );
        assert_eq!(
            p.to_string(),
            "'parent' other than as `parent.<field>` is not supported in a view expression"
        );
    }
}

#[test]
fn a_parent_path_on_a_model_with_a_parent_field_is_refused() {
    let s = site(AttrKind::FilterDomain, ROOT);
    assert_eq!(
        run(&s, "[('parent.x', '=', 1)]").expect("skipped as in the reference ERP"),
        refs(&[])
    );
    let lookup = lookup().field(ROOT, "parent", None, None);
    let source: &dyn GuardLookup = &lookup;
    let p = walk(&s, "[('parent.x', '=', 1)]", source).expect_err("refused");
    assert_eq!(
        p,
        ExprProblem::Unsupported(
            "a domain path starting with 'parent' on a model that has a field named 'parent'"
        )
    );
    assert_eq!(
        p.to_string(),
        "a domain path starting with 'parent' on a model that has a field named 'parent' is not supported in a view expression"
    );
}

#[test]
fn a_very_dotted_path_gives_the_hop_error() {
    let s = site(AttrKind::FilterDomain, ROOT);
    let long = "a.".repeat(5000) + "a";
    let p = problem(&s, &format!("[('{long}', '=', 1)]"));
    assert_eq!(
        p,
        ExprProblem::TooManyHops {
            path: echo(&long),
            hops: 9,
            max: 8,
        }
    );
    assert_eq!(
        p.to_string(),
        format!(
            "the path '{}' has 9 hops: at most 8 are allowed",
            echo(&long)
        )
    );
}

const ANY_NOT_A_LIST: &str =
    "the right side of 'any', 'not any', 'any!' or 'not any!' must be a literal list of leaves";

#[test]
fn a_domain_list_under_any_is_read_on_the_comodel() {
    let s = site(AttrKind::FilterDomain, ROOT);
    for op in ["any", "not any", "any!", "not any!"] {
        let got = run(
            &s,
            &format!("[('line_ids', '{op}', [('cost', '>', 1), ('product_id.name', '=', 'x')])]"),
        )
        .expect("walks");
        assert_eq!(
            got,
            refs(&[
                (ROOT, "line_ids"),
                (ROOT, "line_ids.cost"),
                (ROOT, "line_ids.product_id.name"),
            ]),
            "{op}"
        );
    }
    // nested any, and operators inside the nested domain are checked too
    let got = run(
        &s,
        "[('line_ids', 'any', [('product_id', 'any', [('name', '=', 'x')])])]",
    )
    .expect("walks");
    assert_eq!(
        got,
        refs(&[
            (ROOT, "line_ids"),
            (ROOT, "line_ids.product_id"),
            (ROOT, "line_ids.product_id.name"),
        ])
    );
    // an unknown field in the nested domain is refused on the comodel
    assert_eq!(
        problem(&s, "[('line_ids', 'any', [('nope', '=', 1)])]"),
        ExprProblem::UnknownField {
            model: LINE.to_owned(),
            field: "nope".to_owned()
        }
    );
    // the nested domain's right side is read on the node's model
    let got = run(&s, "[('line_ids', 'any', [('qty', '=', amount)])]").expect("walks");
    assert_eq!(
        got,
        refs(&[(ROOT, "line_ids"), (ROOT, "line_ids.qty"), (ROOT, "amount")])
    );
}

#[test]
fn any_with_a_right_side_that_is_not_a_literal_list_is_refused() {
    let s = site(AttrKind::FilterDomain, ROOT);
    for op in ["any", "not any", "any!", "not any!"] {
        for right in [
            "[('cost', '>', 1)] + []",
            "[('cost', '>', 1)] if amount else []",
            "amount",
            "[('cost', '>', 1)] or []",
            "1",
        ] {
            let p = problem(&s, &format!("[('line_ids', '{op}', {right})]"));
            assert_eq!(
                p,
                ExprProblem::NotADomain(ANY_NOT_A_LIST.to_owned()),
                "{op} {right}"
            );
            assert_eq!(p.to_string(), ANY_NOT_A_LIST);
        }
    }
    // `any` needs a relational field on the left
    let p = problem(&s, "[('amount', 'any', [('cost', '>', 1)])]");
    assert_eq!(
        p,
        ExprProblem::NotADomain(
            "the left side of 'any', 'not any', 'any!' or 'not any!' must be a relational field"
                .to_owned()
        )
    );
}

#[test]
fn a_computed_operator_is_refused() {
    let s = site(AttrKind::FilterDomain, ROOT);
    let message = "the operator of a domain leaf must be a string literal";
    for source in [
        "[('name', state, 'x')]",
        "[('name', '=' + 'x', 'x')]",
        "[('line_ids', state, [('cost', '>', 1)])]",
        "[('name', 1, 'x')]",
    ] {
        let p = problem(&s, source);
        assert_eq!(p, ExprProblem::NotADomain(message.to_owned()), "{source}");
        assert_eq!(p.to_string(), message);
    }
    // the deprecated `(expression, '=', 1)` leaf has its operator checked as well
    assert_eq!(
        problem(&s, "[(amount, state, 1)]"),
        ExprProblem::NotADomain(message.to_owned())
    );
}

#[test]
fn a_provided_name_that_is_a_field_is_refused() {
    let lookup = lookup()
        .field(ROOT, "today", None, None)
        .field(LINE, "uid", None, None);
    let source: &dyn GuardLookup = &lookup;
    let s = site(AttrKind::Expression, ROOT);
    let p = walk(&s, "today == 1", source).expect_err("refused");
    assert_eq!(
        p,
        ExprProblem::ShadowedName {
            name: "today".to_owned(),
            model: ROOT.to_owned()
        }
    );
    assert_eq!(
        p.to_string(),
        "'today' is a name the client provides and also a field of model \"res.partner\": the view cannot say which one it reads"
    );
    // a provided name that is not a field of the node's own model still reads nothing
    assert_eq!(
        walk(&s, "uid or context.get('x')", source).expect("walks"),
        vec![]
    );
    // it resolves on the sub-view's model there
    let p = walk(&in_sub_view(AttrKind::Expression), "uid", source).expect_err("refused");
    assert_eq!(
        p,
        ExprProblem::ShadowedName {
            name: "uid".to_owned(),
            model: LINE.to_owned()
        }
    );
}

#[test]
fn parent_parent_on_a_model_with_a_parent_field_is_refused() {
    let lookup = lookup().field(ROOT, "parent", None, None);
    let source: &dyn GuardLookup = &lookup;
    let p = walk(
        &in_sub_view(AttrKind::Expression),
        "parent.parent.state",
        source,
    )
    .expect_err("refused");
    assert_eq!(
        p,
        ExprProblem::Unsupported("'parent.parent' on a model that has a field named 'parent'")
    );
    assert_eq!(
        p.to_string(),
        "'parent.parent' on a model that has a field named 'parent' is not supported in a view expression"
    );
}
