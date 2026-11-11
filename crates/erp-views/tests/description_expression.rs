//! Expression slots: the reservation is derived from the admitted source and charged before the
//! wire document is built (WP1, plan §2). The allocation side of the same
//! contract is `alloc_expression_preflight.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_expr::{parse, wire::ValidatedAst};
use erp_views::{
    DescriptionBudget, DescriptionBuilder, DescriptionEncoder, ExpressionKind, FieldReference,
    LimitKind, ReferenceScope, ReferenceVisitor, SchemaNode, SchemaProperty, SchemaReferences,
    ViewError, ViewLimits, ViewSchema, expression_preflight,
};
use serde_json::{Value, json};

static DOMAIN: SchemaNode = SchemaNode::Expression {
    kind: ExpressionKind::Domain,
    scope: ReferenceScope::Root,
    parent_model: None,
};

fn admitted(source: &str) -> ValidatedAst {
    ValidatedAst::from_expr(&parse(source).unwrap()).unwrap()
}

/// VD-08, expression reservation boundary and one beyond. No `SPEC-` tag yet: the
/// numbered VDESC cases are promoted by WP0 (plan §2.4), which tags this test.
#[test]
fn expression_refusal_happens_before_any_wire_allocation() {
    let ast = admitted("[('name', '=', 'x'), ('qty', 'in', [1, 2, 3])]");
    let reserve = expression_preflight(ast.as_expr()).unwrap().reserve_bytes();
    assert!(reserve > 1, "the preflight must be a real, nonzero size");
    let schema = ViewSchema::new(1, &DOMAIN).unwrap();

    // One byte short: the exact preflight value is what the refusal reports.
    let budget = DescriptionBudget::with_limit(reserve - 1).unwrap();
    let error = DescriptionBuilder::new(&schema, &budget)
        .build(|root| root.expression(&ast))
        .unwrap_err();
    assert_eq!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionBuildBytes,
            max: reserve - 1,
            actual: Some(reserve),
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "simultaneously retained description bytes limit exceeded: maximum {}, reached {reserve}",
            reserve - 1
        )
    );
    // The refusal is sticky: a caught failure cannot be followed by a published payload.
    assert_eq!(
        budget.check().unwrap_err(),
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionBuildBytes,
            max: reserve - 1,
            actual: None,
        }
    );

    // Exactly the reservation: admitted, and everything is released once the payload is dropped.
    let budget = DescriptionBudget::with_limit(reserve).unwrap();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|root| root.expression(&ast))
        .unwrap();
    drop(payload);
    budget.check().unwrap();
    budget.reserve(reserve).unwrap();
}

// ---- WP2: references derived from the actual expression, in every visitor --------------------

fn expression_node(
    kind: ExpressionKind,
    scope: ReferenceScope,
    parent_model: Option<&'static str>,
) -> &'static SchemaNode {
    Box::leak(Box::new(SchemaNode::Expression {
        kind,
        scope,
        parent_model,
    }))
}
fn invalid(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}
fn describe(references: &SchemaReferences<'_>) -> Vec<String> {
    references
        .fields()
        .map(|field| match field {
            FieldReference::RootPath(path) => format!("root:{path}"),
            FieldReference::ModelPath { model, path } => format!("{model}:{path}"),
            _ => panic!("unexpected reference kind"),
        })
        .collect()
}
fn wire_value(ast: &ValidatedAst) -> Value {
    serde_json::from_slice(&ast.to_json().unwrap()).unwrap()
}
/// The untrusted-JSON visitor: decodes the embedded wire document itself.
fn raw_references(node: &'static SchemaNode, source: &str) -> Result<Vec<String>, ViewError> {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, node).unwrap());
    let budget = DescriptionBudget::new();
    let payload = wire_value(&admitted(source));
    Ok(describe(&visitor.references(&payload, &budget)?))
}
/// The constructed-payload visitor: builds through the schema-bound builder, then walks again.
fn built_references(node: &'static SchemaNode, source: &str) -> Result<Vec<String>, ViewError> {
    let schema = ViewSchema::new(1, node).unwrap();
    let budget = DescriptionBudget::new();
    let ast = admitted(source);
    let payload = DescriptionBuilder::new(&schema, &budget).build(|root| root.expression(&ast))?;
    let visitor = ReferenceVisitor::new(schema);
    Ok(describe(&visitor.payload_references(&payload, &budget)?))
}

struct Case {
    name: &'static str,
    node: &'static SchemaNode,
    source: &'static str,
    expected: &'static [&'static str],
}

fn agreeing_cases() -> Vec<Case> {
    let value = expression_node(ExpressionKind::Value, ReferenceScope::Root, None);
    let domain = expression_node(ExpressionKind::Domain, ReferenceScope::Root, None);
    let context = expression_node(ExpressionKind::Context, ReferenceScope::Root, None);
    let model_value = expression_node(
        ExpressionKind::Value,
        ReferenceScope::Model("sale.order"),
        None,
    );
    let parented = expression_node(
        ExpressionKind::Value,
        ReferenceScope::Root,
        Some("res.partner"),
    );
    vec![
        Case {
            name: "terminal path",
            node: value,
            source: "company_id.partner_id.email",
            expected: &["root:company_id.partner_id.email"],
        },
        Case {
            name: "index position",
            node: value,
            source: "context[internal_memo]",
            expected: &["root:internal_memo"],
        },
        Case {
            name: "slice positions",
            node: value,
            source: "items[low:high:step]",
            expected: &["root:high", "root:items", "root:low", "root:step"],
        },
        Case {
            name: "conditional branches",
            node: value,
            source: "a if b else c",
            expected: &["root:a", "root:b", "root:c"],
        },
        Case {
            name: "execution identifiers are not fields",
            node: value,
            source: "uid + user.id + context.get_x",
            expected: &[],
        },
        Case {
            name: "domain keeps literal values as data",
            node: domain,
            source: "[('partner_id.name', '=', 'name')]",
            expected: &["root:partner_id.name"],
        },
        Case {
            name: "domain value expressions are walked",
            node: domain,
            source: "[('state', 'in', ['draft', other_field])]",
            expected: &["root:other_field", "root:state"],
        },
        Case {
            name: "domain logical prefix operators",
            node: domain,
            source: "['|', ('a', '=', 1), ('b', '!=', 2)]",
            expected: &["root:a", "root:b"],
        },
        Case {
            name: "context triples stay literal data",
            node: context,
            source: "{'choices': ['name', '=', 'literal'], 'group_by': ['priority', 'create_date:month']}",
            expected: &["root:create_date", "root:priority"],
        },
        Case {
            name: "context group_by string",
            node: context,
            source: "{'group_by': 'stage_id:quarter'}",
            expected: &["root:stage_id"],
        },
        Case {
            name: "explicit model scope",
            node: model_value,
            source: "partner_id.name",
            expected: &["sale.order:partner_id.name"],
        },
        Case {
            name: "explicit parent scope",
            node: parented,
            source: "parent.partner_id.name + local",
            expected: &["root:local", "res.partner:partner_id.name"],
        },
        Case {
            name: "bare parent is an execution identifier",
            node: parented,
            source: "parent",
            expected: &[],
        },
    ]
}

/// derive the same references independently of the builder)
/// explicit model and parent scope)
#[test]
fn raw_and_constructed_visitors_derive_the_same_references_from_the_expression() {
    for case in agreeing_cases() {
        let expected: Vec<String> = case.expected.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            raw_references(case.node, case.source).unwrap(),
            expected,
            "raw: {}",
            case.name
        );
        assert_eq!(
            built_references(case.node, case.source).unwrap(),
            expected,
            "constructed: {}",
            case.name
        );
    }
}

#[test]
fn parent_paths_need_a_declared_parent_model() {
    let message = "Description expression uses parent.<path> without a declared parent model";
    let value = expression_node(ExpressionKind::Value, ReferenceScope::Root, None);
    assert_eq!(
        raw_references(value, "parent.name").unwrap_err(),
        invalid(message)
    );
    assert_eq!(
        built_references(value, "parent.name").unwrap_err(),
        invalid(message)
    );
}

#[test]
fn unsupported_operators_comprehensions_and_dynamic_bases_refuse_in_every_visitor() {
    let domain = expression_node(ExpressionKind::Domain, ReferenceScope::Root, None);
    let value = expression_node(ExpressionKind::Value, ReferenceScope::Root, None);
    let cases: [(&'static SchemaNode, &str, &str); 3] = [
        (
            domain,
            "[('a', 'bogus', 1)]",
            "Unsupported search domain operator 'bogus'. Additional operators require a later release.",
        ),
        (
            value,
            "[x for x in y]",
            "Unsupported search expression traversal requires a later release.",
        ),
        (
            value,
            "f(x).secret",
            "Unsupported dynamic search field attribute traversal. Additional expression traversal requires a later release.",
        ),
    ];
    for (node, source, message) in cases {
        assert_eq!(
            raw_references(node, source).unwrap_err(),
            invalid(message),
            "raw {source}"
        );
        assert_eq!(
            built_references(node, source).unwrap_err(),
            invalid(message),
            "built {source}"
        );
    }
}

#[test]
fn context_group_by_with_an_unsupported_granularity_names_it() {
    let context = expression_node(ExpressionKind::Context, ReferenceScope::Root, None);
    let message = "Description group_by granularity 'decade' is unsupported; expected day, week, month, quarter or year";
    assert_eq!(
        raw_references(context, "{'group_by': 'stage_id:decade'}").unwrap_err(),
        invalid(message)
    );
    assert_eq!(
        built_references(context, "{'group_by': ['a', 'stage_id:decade']}").unwrap_err(),
        invalid(message)
    );
}

#[test]
fn a_failed_construction_walk_is_sticky_for_the_request() {
    let schema = ViewSchema::new(
        1,
        expression_node(ExpressionKind::Value, ReferenceScope::Root, None),
    )
    .unwrap();
    let budget = DescriptionBudget::new();
    let bad = admitted("f(x).secret");
    let error = DescriptionBuilder::new(&schema, &budget)
        .build(|root| {
            // A caller that catches the refusal and carries on still cannot publish.
            let _caught = root.expression(&bad);
            root.expression(&admitted("fine"))
        })
        .unwrap_err();
    assert_eq!(
        error,
        invalid("Description remains refused after an earlier construction failure")
    );
}

#[test]
fn raw_expression_slots_refuse_values_that_are_not_wire_documents() {
    let node = expression_node(ExpressionKind::Value, ReferenceScope::Root, None);
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, node).unwrap());
    let budget = DescriptionBudget::new();
    for payload in [
        json!("name"),
        json!(7),
        json!(null),
        json!([]),
        json!({"schema":1}),
    ] {
        assert_eq!(
            visitor.references(&payload, &budget).unwrap_err(),
            invalid("Description expression requires a versioned wire document"),
            "{payload}"
        );
    }
    let error = visitor
        .references(
            &json!({"schema":2, "nodes":[{"kind":"name","name":"a"}]}),
            &budget,
        )
        .unwrap_err();
    assert_eq!(
        error,
        invalid(
            "Description expression is not a valid wire document: InvalidAst at $.schema: unsupported schema version 2; reload or upgrade required"
        )
    );
}

#[test]
fn expression_values_cannot_cross_schemas_slots_or_budgets() {
    static TEXT: SchemaNode = SchemaNode::Text { max_bytes: 64 };
    let text = ViewSchema::new(1, &TEXT).unwrap();
    let ast = admitted("[('a', '=', 1)]");
    let error = DescriptionBuilder::new(&text, &DescriptionBudget::new())
        .build(|root| root.expression(&ast))
        .unwrap_err();
    assert_eq!(
        error,
        invalid("Description value does not match its expression schema slot")
    );
    // A value made under another budget cannot be stored in this request's object.
    static HOLDER: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("e", &DOMAIN)]);
    let schema = ViewSchema::new(1, &HOLDER).unwrap();
    let other = DescriptionBudget::new();
    let mut foreign = None;
    let inner = ViewSchema::new(1, &DOMAIN).unwrap();
    DescriptionBuilder::new(&inner, &other)
        .build(|root| {
            foreign = Some(root.expression(&ast)?);
            root.expression(&ast)
        })
        .unwrap();
    let error = DescriptionBuilder::new(&schema, &DescriptionBudget::new())
        .build(|root| root.object(|object| object.set("e", |_| Ok(foreign.take().unwrap()))))
        .unwrap_err();
    assert_eq!(
        error,
        invalid("Description value budget identity does not match its request")
    );
}

// ---- shared expression work and aggregate bytes ----------------------------------------------

static VALUE: SchemaNode = SchemaNode::Expression {
    kind: ExpressionKind::Value,
    scope: ReferenceScope::Root,
    parent_model: None,
};
static PAIR: SchemaNode = SchemaNode::Array {
    item: &VALUE,
    max_items: 4,
};

fn budget_with(work: usize, bytes: usize) -> DescriptionBudget {
    DescriptionBudget::with_limits(32 * 1024 * 1024, work, bytes).unwrap()
}
fn build_pair(
    budget: &DescriptionBudget,
    first: &ValidatedAst,
    second: &ValidatedAst,
) -> Result<(), ViewError> {
    let schema = ViewSchema::new(1, &PAIR).unwrap();
    DescriptionBuilder::new(&schema, budget)
        .build(|root| {
            root.array(2, |index, value| {
                value.expression(if index == 0 { first } else { second })
            })
        })
        .map(drop)
}

/// charged before the document is allocated, sticky when the caller catches the refusal)
#[test]
fn expression_work_is_shared_across_values_and_sticky() {
    let ast = admitted("x + y"); // 3 nodes
    let nodes = expression_preflight(ast.as_expr()).unwrap().nodes();
    assert_eq!(nodes, 3);
    // Exactly the work of both values passes ...
    build_pair(&budget_with(2 * nodes, 16 * 1024 * 1024), &ast, &ast).unwrap();
    // ... one step fewer refuses on the first step past the limit, naming it.
    let budget = budget_with(2 * nodes - 1, 16 * 1024 * 1024);
    let error = build_pair(&budget, &ast, &ast).unwrap_err();
    assert_eq!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionWork,
            max: 2 * nodes - 1,
            actual: Some(2 * nodes),
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "expression work steps per request limit exceeded: maximum {}, reached {}",
            2 * nodes - 1,
            2 * nodes
        )
    );
    // The caught failure stays sticky for the whole request.
    assert_eq!(
        budget.check().unwrap_err(),
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionWork,
            max: 2 * nodes - 1,
            actual: None,
        }
    );
    // One value alone at the limit passes; the same budget cannot be reset by a second build.
    let single = budget_with(nodes, 16 * 1024 * 1024);
    let schema = ViewSchema::new(1, &VALUE).unwrap();
    DescriptionBuilder::new(&schema, &single)
        .build(|root| root.expression(&ast))
        .unwrap();
    let error = DescriptionBuilder::new(&schema, &single)
        .build(|root| root.expression(&ast))
        .unwrap_err();
    assert!(matches!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionWork,
            ..
        }
    ));
}

#[test]
fn aggregate_expression_bytes_are_shared_across_values() {
    let first = admitted("x + y");
    let second = admitted("name_of_a_longer_field");
    let encoded = |ast: &ValidatedAst| ast.to_json().unwrap().len();
    let total = encoded(&first) + encoded(&second);
    build_pair(&budget_with(1_000_000, total), &first, &second).unwrap();
    let budget = budget_with(1_000_000, total - 1);
    let error = build_pair(&budget, &first, &second).unwrap_err();
    assert_eq!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionBytes,
            max: total - 1,
            actual: Some(total),
        }
    );
    assert_eq!(
        budget.check().unwrap_err(),
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionBytes,
            max: total - 1,
            actual: None,
        }
    );
}

#[test]
fn the_raw_visitor_charges_work_and_bytes_to_the_same_request_budget() {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, &PAIR).unwrap());
    let ast = admitted("x + y");
    let one = wire_value(&ast);
    let payload = Value::Array(vec![one.clone(), one]);
    let encoded = ast.to_json().unwrap().len();
    // Decoding charges each node once and walking charges each node once: 3 + 3 per value.
    visitor
        .references(&payload, &budget_with(12, 2 * encoded))
        .unwrap();
    let budget = budget_with(11, 2 * encoded);
    let error = visitor.references(&payload, &budget).unwrap_err();
    assert_eq!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionWork,
            max: 11,
            actual: Some(12),
        }
    );
    let budget = budget_with(12, 2 * encoded - 1);
    let error = visitor.references(&payload, &budget).unwrap_err();
    assert_eq!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionBytes,
            max: 2 * encoded - 1,
            actual: Some(2 * encoded),
        }
    );
    // A refusal caught by the caller still refuses every later use of the request.
    assert!(visitor.references(&payload, &budget).is_err());
}

#[test]
fn expression_limits_are_lower_only_and_never_zero() {
    for (work, bytes) in [(0, 1), (1, 0), (1_000_001, 1), (1, 16 * 1024 * 1024 + 1)] {
        let error = DescriptionBudget::with_limits(1, work, bytes).unwrap_err();
        assert!(
            matches!(error, ViewError::InvalidLimit { .. }),
            "{work} {bytes}"
        );
    }
    assert_eq!(
        ViewLimits::DEFAULT.get(LimitKind::DescriptionExpressionWork),
        1_000_000
    );
    assert_eq!(
        ViewLimits::DEFAULT.get(LimitKind::DescriptionExpressionBytes),
        16 * 1024 * 1024
    );
    assert!(
        ViewLimits::DEFAULT
            .lowered(LimitKind::DescriptionExpressionWork, 1_000_001)
            .is_err()
    );
}

// ---- payload depth includes the expression's logical height -----------------------------------

macro_rules! wrap {
    ($name:ident, $inner:ident) => {
        static $name: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("e", &$inner)]);
    };
}
wrap!(W1, VALUE);
wrap!(W2, W1);
wrap!(W3, W2);
wrap!(W4, W3);
wrap!(W5, W4);
wrap!(W6, W5);
wrap!(W7, W6);
wrap!(W8, W7);
wrap!(W9, W8);
wrap!(W10, W9);
wrap!(W11, W10);
wrap!(W12, W11);
wrap!(W13, W12);
wrap!(W14, W13);
wrap!(W15, W14);
wrap!(W16, W15);
wrap!(W17, W16);
wrap!(W18, W17);
wrap!(W19, W18);
wrap!(W20, W19);
wrap!(W21, W20);
wrap!(W22, W21);
wrap!(W23, W22);
wrap!(W24, W23);
wrap!(W25, W24);
wrap!(W26, W25);
wrap!(W27, W26);
wrap!(W28, W27);

/// Depth-128 payloads recurse past the 2 MiB default test-thread stack in an unoptimized build
/// (observed as a stack overflow in CI run ci-f26f78c2763d). The depth contract is exercised on a
/// thread with an explicit stack; the release-build frame cost is not measured here.
fn on_large_stack(body: fn()) {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap();
}

/// height, at the limit and one beyond, in both visitors)
#[test]
fn payload_depth_counts_the_expression_height() {
    on_large_stack(payload_depth_counts_the_expression_height_body);
}
fn payload_depth_counts_the_expression_height_body() {
    // Slot depth is 29 (28 objects, root = 1); the expression's deepest node sits `height` below it.
    let schema = ViewSchema::new(1, &W28).unwrap();
    let nest = |value: Value| (0..28).fold(value, |inner, _| json!({ "e": inner }));
    let unary = |negations: usize| admitted(&format!("{}x", "-".repeat(negations)));
    let message = "Description payload exceeds its depth limit";
    let visitor = ReferenceVisitor::new(schema.clone());
    let build = |ast: &ValidatedAst| {
        let budget = DescriptionBudget::new();
        let result = DescriptionBuilder::new(&schema, &budget)
            .build(|root| nested_expression(root, 28, ast));
        (budget, result)
    };
    // 98 negations over a name: height 99, deepest node at depth 29 + 99 = 128.
    let at_limit = unary(98);
    visitor
        .references(&nest(wire_value(&at_limit)), &DescriptionBudget::new())
        .unwrap();
    let (budget, payload) = build(&at_limit);
    visitor
        .payload_references(&payload.unwrap(), &budget)
        .unwrap();
    // One negation more reaches depth 129.
    let beyond = unary(99);
    assert_eq!(
        visitor
            .references(&nest(wire_value(&beyond)), &DescriptionBudget::new())
            .unwrap_err(),
        invalid(message)
    );
    assert_eq!(build(&beyond).1.unwrap_err(), invalid(message));
}
#[test]
fn payload_depth_counts_every_attribute_level() {
    on_large_stack(payload_depth_counts_every_attribute_level_body);
}
fn payload_depth_counts_every_attribute_level_body() {
    let schema = ViewSchema::new(1, &W28).unwrap();
    let nest = |value: Value| (0..28).fold(value, |inner, _| json!({ "e": inner }));
    let visitor = ReferenceVisitor::new(schema.clone());
    for (attributes, accepted) in [(98, true), (99, false)] {
        let ast = admitted(&format!("x{}", ".x".repeat(attributes)));
        let raw_value = nest(wire_value(&ast));
        let raw = visitor.references(&raw_value, &DescriptionBudget::new());
        let budget = DescriptionBudget::new();
        let built = DescriptionBuilder::new(&schema, &budget)
            .build(|root| nested_expression(root, 28, &ast));
        if accepted {
            assert_eq!(describe(&raw.unwrap()).len(), 1);
            let payload = built.unwrap();
            assert_eq!(
                describe(&visitor.payload_references(&payload, &budget).unwrap()).len(),
                1
            );
        } else {
            for error in [
                raw.expect_err("raw traversal must refuse depth 129"),
                built.expect_err("construction must refuse depth 129"),
            ] {
                assert_eq!(
                    error,
                    invalid("Description payload exceeds its depth limit")
                );
                assert_eq!(
                    error.to_string(),
                    "Description payload exceeds its depth limit"
                );
            }
        }
    }
}

fn nested_expression(
    value: erp_views::DescriptionValueBuilder<'_>,
    levels: usize,
    ast: &ValidatedAst,
) -> Result<erp_views::DescriptionValue, ViewError> {
    if levels == 0 {
        return value.expression(ast);
    }
    value.object(|o| o.set("e", |v| nested_expression(v, levels - 1, ast)))
}

// ---- the embedded wire document survives encoding ---------------------------------------------

#[test]
fn the_encoded_payload_round_trips_the_wire_document_and_its_references() {
    let source = "[('partner_id.name', '=', name), ('qty', 'in', [1, 2, 3])]";
    let ast = admitted(source);
    let schema = ViewSchema::new(1, &DOMAIN).unwrap();
    let budget = DescriptionBudget::new();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|root| root.expression(&ast))
        .unwrap();
    let encoded = DescriptionEncoder::new(&budget, &ViewLimits::DEFAULT)
        .encode(&payload)
        .unwrap();
    let decoded = ValidatedAst::from_json(encoded.as_json_bytes()).unwrap();
    assert_eq!(decoded.as_expr(), ast.as_expr());
    // The untrusted visitor reaches the same references from the encoded bytes alone.
    let visitor = ReferenceVisitor::new(schema);
    let from_bytes: Value = serde_json::from_slice(encoded.as_json_bytes()).unwrap();
    let raw = describe(&visitor.references(&from_bytes, &budget).unwrap());
    let built = describe(&visitor.payload_references(&payload, &budget).unwrap());
    assert_eq!(raw, built);
    assert_eq!(raw, ["root:name", "root:partner_id.name", "root:qty"]);
}
