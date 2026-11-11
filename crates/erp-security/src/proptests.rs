//! Fuzz smoke (DN §8.3) as property tests: rule strings into the compile pipeline (errors, never
//! panics, bounded), group implication graphs with cycles, and `access` chains with cycles. The
//! repository has no cargo-fuzz setup; these run in every CI.

use std::collections::BTreeSet;

use erp_domain::Operation;
use erp_orm::{AccessPolicy, GroupId};
use proptest::prelude::*;

use crate::groups::GroupGraph;
use crate::rows::{GroupRow, Implication, PolicyRows};
use crate::rule;
use crate::testing::{
    CATEG, CHILD, G1, SOME_OBJ, USER, bind, model, principal, registry, rule as rule_row, snapshot,
};

/// Fragments of rule strings: names, operators, values, symbols and broken syntax.
fn fragment() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "[",
        "]",
        "(",
        ")",
        ",",
        "'&'",
        "'|'",
        "'!'",
        "'val'",
        "'name'",
        "'id'",
        "'categ_id'",
        "'categ_id.name'",
        "'partner_id'",
        "'nope'",
        "'='",
        "'!='",
        "'in'",
        "'not in'",
        "'>'",
        "'any'",
        "'not any'",
        "'any!'",
        "'access'",
        "'child_of'",
        "'read'",
        "'write'",
        "'blabla'",
        "1",
        "-1",
        "0",
        "None",
        "True",
        "False",
        "'x'",
        "[1, 2]",
        "()",
        "user",
        "user.id",
        "user.ids",
        "user.partner_id.id",
        "user.partner_id",
        "user.employee_ids.ids",
        "user.name",
        "user.env['test_access_right.obj_categ'].search([])",
        ".ids",
        "user.has_group('base.group_user')",
        "company_ids",
        "company_id",
        "company_ids + [False]",
        "time.strftime('%Y-%m-%d')",
        "datetime.now()",
        "+",
        "-",
        "*",
        " if ",
        " else ",
        "lambda",
        "{",
        "}",
        ":",
        "'é経'",
        "\r\n",
        "\\",
        "#",
        "\"",
        "'",
        "9223372036854775807",
        "-9223372036854775808",
        "1e400",
    ])
}

/// A rule string built from fragments (often malformed, sometimes valid).
fn rule_text() -> impl Strategy<Value = String> {
    prop::collection::vec(fragment(), 0..40).prop_map(|parts| parts.concat())
}

/// A well-formed condition list over the fixture's fields and symbols.
fn condition() -> impl Strategy<Value = String> {
    let field = prop::sample::select(vec![
        "val",
        "name",
        "id",
        "categ_id",
        "categ_id.name",
        "user_id",
        "company_id",
        "partner_id",
        "tag_ids",
    ]);
    let op = prop::sample::select(vec![
        "=", "!=", "in", "not in", ">", "<", "child_of", "any", "access",
    ]);
    let value = prop::sample::select(vec![
        "1",
        "False",
        "[1, 2]",
        "'x'",
        "user.id",
        "company_ids",
        "company_ids + [False]",
        "user.partner_id.id",
        "[]",
        "[('name', '=', 'x')]",
        "'read'",
        "'write'",
    ]);
    (field, op, value).prop_map(|(f, o, v)| format!("('{f}', '{o}', {v})"))
}

fn domain_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            condition(),
            Just("'&'".to_owned()),
            Just("'|'".to_owned()),
            Just("'!'".to_owned()),
        ],
        0..12,
    )
    .prop_map(|items| format!("[{}]", items.join(", ")))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    // the results are discarded on purpose: these properties are "returns, never panics"
    #[test]
    fn arbitrary_text_compiles_or_errors(text in "\\PC{0,200}") {
        let _ = rule::compile(registry(), model(SOME_OBJ), Some(&text));
        let _ = rule::validate(registry(), SOME_OBJ, Some(&text));
    }

    #[test]
    fn fragments_compile_or_error(text in rule_text()) {
        if let Ok(compiled) = rule::compile(registry(), model(SOME_OBJ), Some(&text)) {
            // whatever compiled reads only fields of the registry
            for (m, f) in &compiled.reads {
                prop_assert!(registry().model_by_id(*m).fields.len() > f.index());
            }
        }
    }

    #[test]
    fn well_formed_domains_compile_or_error(text in domain_text()) {
        let _ = rule::compile(registry(), model(SOME_OBJ), Some(&text));
    }

    /// The closure equals a naive transitive closure, and implied-by is its inverse, whatever
    /// cycles the graph has.
    #[test]
    fn group_closure_matches_a_naive_closure(
        n in 1usize..12,
        edges in prop::collection::vec((0usize..12, 0usize..12), 0..40),
    ) {
        let edges: BTreeSet<(usize, usize)> =
            edges.into_iter().filter(|(a, b)| *a < n && *b < n).collect();
        let id = |i: usize| GroupId::new(i64::try_from(i).unwrap() + 1).unwrap();
        let rows = PolicyRows::builder(0)
            .groups((0..n).map(|i| GroupRow::new(id(i), "g").unwrap()))
            .implications(edges.iter().map(|(a, b)| Implication::new(id(*a), id(*b))))
            .build()
            .unwrap();
        let graph = GroupGraph::build(&rows).unwrap();
        // naive: reach[i][j] by repeated relaxation
        let mut reach = vec![vec![false; n]; n];
        for (i, row) in reach.iter_mut().enumerate() {
            row[i] = true;
        }
        for _ in 0..n {
            for &(a, b) in &edges {
                for row in reach.iter_mut() {
                    if row[a] {
                        row[b] = true;
                    }
                }
            }
        }
        for (i, row) in reach.iter().enumerate() {
            let expected: Vec<GroupId> = (0..n).filter(|j| row[*j]).map(id).collect();
            let closure = graph.closure(&[id(i)]).unwrap();
            prop_assert_eq!(closure.ids(), expected.as_slice());
            let by: Vec<GroupId> = (0..n).filter(|j| reach[*j][i]).map(id).collect();
            prop_assert_eq!(graph.implied_by(id(i)).unwrap(), by);
        }
    }

    /// Random `access` chains (with cycles) compile, and deciding them for a principal either
    /// gives a predicate or a cycle/rule error: never a panic or an endless recursion.
    #[test]
    fn access_chains_terminate(
        chains in prop::collection::vec(
            (0usize..3, 0usize..4, 0usize..3, 0usize..4, prop::bool::ANY),
            0..8,
        ),
    ) {
        let models = [SOME_OBJ, CATEG, CHILD];
        let ops = ["r", "u", "c", "d"];
        let names = ["read", "write", "create", "unlink"];
        // the field of each model whose access condition reaches each target
        let link = |from: usize, to: usize| -> Option<&'static str> {
            match (from, to) {
                (0, 0) => Some("parent_id"),
                (0, 1) => Some("categ_id"),
                (2, 0) => Some("some_id"),
                (_, _) if from == to => Some("id"),
                _ => None,
            }
        };
        let mut rules = Vec::new();
        for (i, (from, op, to, target_op, group)) in chains.iter().enumerate() {
            let Some(field) = link(*from, *to) else { continue };
            let domain = format!("[('{field}', 'access', '{}')]", names[*target_op]);
            let group = group.then_some(G1);
            rules.push(rule_row(
                i64::try_from(i).unwrap() + 100,
                "chain",
                models[*from],
                ops[*op],
                group,
                Some(&domain),
            ));
        }
        let policy = snapshot(rules);
        let p = principal(&policy, 7, &[USER, G1]);
        let b = bind(&policy, &p);
        for m in models {
            for op in [Operation::Read, Operation::Write, Operation::Create, Operation::Unlink] {
                let _ = policy.access_domain(&b, model(m), op);
                let _ = policy.allowing_groups(model(m), op);
            }
        }
    }
}
