//! Pure scoped-domain admission. These tests grant no registry or category authority.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_expr::{parse, wire::ValidatedAst};
use erp_views::{
    DescriptionBudget, DescriptionBuilder, FieldReference, LimitKind, ReferenceVisitor, SchemaNode,
    SchemaProperty, ViewError, ViewSchema, expression_preflight,
};
use serde_json::{Value, json};

static SCOPED: SchemaNode = SchemaNode::ScopedDomain;
static TEXT: SchemaNode = SchemaNode::Text { max_bytes: 16 };
static PAIR: SchemaNode = SchemaNode::Array {
    item: &SCOPED,
    max_items: 2,
};
fn ast(source: &str) -> ValidatedAst {
    ValidatedAst::from_expr(&parse(source).unwrap()).unwrap()
}
fn document(ast: &ValidatedAst) -> Value {
    serde_json::from_slice(&ast.to_json().unwrap()).unwrap()
}
fn invalid(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.into())
}
fn refs(index: &erp_views::SchemaReferences<'_>) -> Vec<String> {
    index
        .fields()
        .map(|r| match r {
            FieldReference::RootPath(p) => format!("root:{p}"),
            FieldReference::ModelPath { model, path } => format!("{model}:{path}"),
            _ => panic!("unexpected reference"),
        })
        .collect()
}

#[test]
fn both_visitors_keep_same_spelled_criterion_and_rhs_in_distinct_scopes() {
    let ast =
        ast("[('category_id', '=', category_id), ('owner.name', 'in', [category_id, 'literal'])]");
    let schema = ViewSchema::new(1, &SCOPED).unwrap();
    let budget = DescriptionBudget::new();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|b| b.scoped_domain("demo.tag", &ast))
        .unwrap();
    let json = serde_json::to_value(&payload).unwrap();
    assert_eq!(json, json!({"model":"demo.tag","ast":document(&ast)}));
    let expected = [
        "root:category_id",
        "demo.tag:category_id",
        "demo.tag:owner.name",
    ];
    let visitor = ReferenceVisitor::new(schema);
    assert_eq!(
        refs(&visitor.payload_references(&payload, &budget).unwrap()),
        expected
    );
    assert_eq!(
        refs(
            &visitor
                .references(&json, &DescriptionBudget::new())
                .unwrap()
        ),
        expected
    );
    drop(payload);
    budget.reserve(32 * 1024 * 1024).unwrap();
}

#[test]
fn scoped_constructor_refuses_wrong_slots_and_invalid_models_stickily() {
    let ast = ast("[]");
    for model in ["", "bad\nmodel", &"a".repeat(4097)] {
        let schema = ViewSchema::new(1, &SCOPED).unwrap();
        let budget = DescriptionBudget::new();
        let err = DescriptionBuilder::new(&schema, &budget)
            .build(|b| b.scoped_domain(model, &ast))
            .unwrap_err();
        assert_eq!(
            err,
            invalid("Description scoped domain has an invalid model")
        );
        assert_eq!(
            err.to_string(),
            "Description scoped domain has an invalid model"
        );
        assert!(budget.check().is_err());
    }
    let schema = ViewSchema::new(1, &TEXT).unwrap();
    let budget = DescriptionBudget::new();
    assert_eq!(
        DescriptionBuilder::new(&schema, &budget)
            .build(|b| b.scoped_domain("demo.tag", &ast))
            .unwrap_err(),
        invalid("Description value does not match its scoped domain schema slot")
    );
    assert!(budget.check().is_err());
}

#[test]
fn json_shape_model_and_ast_refusals_are_closed_sanitized_and_sticky() {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, &SCOPED).unwrap());
    let valid = document(&ast("[]"));
    for (payload, message) in [
        (
            json!("secret-input"),
            "Description scoped domain requires exactly model and ast",
        ),
        (
            json!({"model":"demo.tag","ast":valid,"extra":"secret-input"}),
            "Description scoped domain requires exactly model and ast",
        ),
        (
            json!({"ast":valid}),
            "Description scoped domain requires exactly model and ast",
        ),
        (
            json!({"model":7,"ast":valid}),
            "Description scoped domain has an invalid model",
        ),
        (
            json!({"model":"bad\nmodel","ast":valid}),
            "Description scoped domain has an invalid model",
        ),
        (
            json!({"model":"demo.tag","ast":"secret-input"}),
            "Description scoped domain has an invalid wire document",
        ),
        (
            json!({"model":"demo.tag","ast":{"schema":99,"nodes":[]}}),
            "Description scoped domain has an invalid wire document",
        ),
    ] {
        let budget = DescriptionBudget::new();
        assert_eq!(
            visitor.references(&payload, &budget).unwrap_err(),
            invalid(message)
        );
        assert!(budget.check().is_err());
    }
}

#[test]
fn malformed_and_quantified_domains_never_publish_partial_references() {
    let schema = ViewSchema::new(1, &SCOPED).unwrap();
    let visitor = ReferenceVisitor::new(schema.clone());
    for source in [
        "['|', ('a', '=', 1)]",
        "[('a', '=')]",
        "[('a', 'secret-op', 1)]",
        "[('a', 'any', [('hidden', '=', 1)])]",
        "[('a', 'not any', [])]",
        "[('a', 'any!', [])]",
        "[('a', 'not any!', [])]",
        "[('', '=', 1)]",
    ] {
        let ast = ast(source);
        let budget = DescriptionBudget::new();
        let error = DescriptionBuilder::new(&schema, &budget)
            .build(|b| b.scoped_domain("demo.tag", &ast))
            .unwrap_err();
        assert_eq!(
            error,
            invalid(
                "Description scoped domain has an unsupported domain structure or operator; additional syntax requires a later release"
            )
        );
        assert!(budget.check().is_err());
        let budget = DescriptionBudget::new();
        let json = json!({"model":"demo.tag","ast":document(&ast)});
        assert_eq!(visitor.references(&json, &budget).unwrap_err(), error);
        assert!(budget.check().is_err());
    }
}

#[test]
fn generic_rhs_expressions_preserve_references_without_panel_dependency_authorization() {
    let ast = ast("[('id', '=', first + second), ('name', '=', data.member)]");
    let schema = ViewSchema::new(1, &SCOPED).unwrap();
    let budget = DescriptionBudget::new();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|b| b.scoped_domain("demo.tag", &ast))
        .unwrap();
    assert_eq!(
        refs(
            &ReferenceVisitor::new(schema)
                .payload_references(&payload, &budget)
                .unwrap()
        ),
        [
            "root:data.member",
            "root:first",
            "root:second",
            "demo.tag:id",
            "demo.tag:name"
        ]
    );
}

#[test]
fn scoped_storage_and_expression_bytes_are_reserved_before_copying_and_shared() {
    let ast = ast("[]");
    let schema = ViewSchema::new(1, &SCOPED).unwrap();
    let reserve = expression_preflight(ast.as_expr()).unwrap().reserve_bytes() + "demo.tag".len();
    let budget = DescriptionBudget::with_limit(reserve).unwrap();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|b| b.scoped_domain("demo.tag", &ast))
        .unwrap();
    drop(payload);
    budget.reserve(reserve).unwrap();
    let budget = DescriptionBudget::with_limit(reserve - 1).unwrap();
    assert_eq!(
        DescriptionBuilder::new(&schema, &budget)
            .build(|b| b.scoped_domain("demo.tag", &ast))
            .unwrap_err(),
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionBuildBytes,
            max: reserve - 1,
            actual: Some(reserve)
        }
    );
    assert!(budget.check().is_err());
    let pair = ViewSchema::new(1, &PAIR).unwrap();
    let total = 2 * ast.encoded_len();
    for bytes in [total, total - 1] {
        let budget = DescriptionBudget::with_limits(32 * 1024 * 1024, 1000, bytes).unwrap();
        let result = DescriptionBuilder::new(&pair, &budget)
            .build(|b| b.array(2, |_, b| b.scoped_domain("demo.tag", &ast)));
        if bytes == total {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err(),
                ViewError::LimitExceeded {
                    limit: LimitKind::DescriptionExpressionBytes,
                    max: bytes,
                    actual: Some(total)
                }
            );
            assert!(budget.check().is_err());
        }
    }
    // Empty list admission visits one node in structural validation and one in reference traversal.
    for work in [2, 1] {
        let budget = DescriptionBudget::with_limits(32 * 1024 * 1024, work, total).unwrap();
        let result =
            DescriptionBuilder::new(&schema, &budget).build(|b| b.scoped_domain("demo.tag", &ast));
        if work == 2 {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err(),
                ViewError::LimitExceeded {
                    limit: LimitKind::DescriptionExpressionWork,
                    max: work,
                    actual: Some(2)
                }
            );
            assert!(budget.check().is_err());
        }
    }
}

#[test]
fn caught_failure_prevents_serializing_an_earlier_scoped_payload() {
    let schema = ViewSchema::new(1, &SCOPED).unwrap();
    let budget = DescriptionBudget::new();
    let ast = ast("[]");
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|b| b.scoped_domain("demo.tag", &ast))
        .unwrap();
    DescriptionBuilder::new(&schema, &budget)
        .build(|b| {
            let _caught = b.scoped_domain("", &ast);
            b.scoped_domain("demo.tag", &ast)
        })
        .unwrap_err();
    assert!(serde_json::to_vec(&payload).is_err());
}

#[test]
fn schema_depth_counts_the_embedded_ast() {
    static HOLDER: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("domain", &SCOPED)]);
    let schema = ViewSchema::new(1, &HOLDER).unwrap();
    let budget = DescriptionBudget::new();
    let ast = ast("[('name', '=', value)]");
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|b| b.object(|o| o.set("domain", |b| b.scoped_domain("demo.tag", &ast))))
        .unwrap();
    assert_eq!(
        refs(
            &ReferenceVisitor::new(schema)
                .payload_references(&payload, &budget)
                .unwrap()
        ),
        ["root:value", "demo.tag:name"]
    );
}

#[test]
fn execution_identifier_spellings_are_root_dependencies_in_scoped_domains() {
    let ast = ast("[('uid', '=', uid), ('name', 'in', [context, active_id, parent])]");
    let schema = ViewSchema::new(1, &SCOPED).unwrap();
    let budget = DescriptionBudget::new();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|b| b.scoped_domain("demo.tag", &ast))
        .unwrap();
    let visitor = ReferenceVisitor::new(schema);
    let expected = [
        "root:active_id",
        "root:context",
        "root:parent",
        "root:uid",
        "demo.tag:name",
        "demo.tag:uid",
    ];
    assert_eq!(
        refs(&visitor.payload_references(&payload, &budget).unwrap()),
        expected
    );
    let json = serde_json::to_value(&payload).unwrap();
    assert_eq!(
        refs(
            &visitor
                .references(&json, &DescriptionBudget::new())
                .unwrap()
        ),
        expected
    );
}

#[test]
fn strict_panel_grammar_is_separate_from_the_generic_scoped_domain() {
    for source in [
        "[]",
        "()",
        "[('name','=','x')]",
        "['!',('id','in',[1,2])]",
        "['|', ('a', '=', category), ('b', '=', False), ('c', '=', None)]",
    ] {
        erp_views::validate_panel_domain(&ast(source)).unwrap();
    }
    for source in [
        "[('id', '=', first + second)]",
        "[('name', '=', data.member)]",
        "[('id', '=', context.get('x'))]",
        "[('id', '=', data[0])]",
        "[('id', '=', {'k':1})]",
        "[('id', '=', b'abc')]",
        "[('id', '=', [x for x in y])]",
        "[[('id', '=', 1)]]",
        "[('id', '=', 1)],",
        "['!', 'name']",
        "['&', ('id', '=', 1)]",
        "[('id', '=', -category)]",
        "[('id', '=', --1)]",
    ] {
        assert_eq!(
            erp_views::validate_panel_domain(&ast(source)).unwrap_err(),
            invalid(
                "Search-panel domain syntax is unsupported in version 1; additional syntax requires a later release"
            ),
            "{source}"
        );
    }
    // A name is a dependency even when the ordinary evaluator treats it as an execution global.
    erp_views::validate_panel_domain(&ast("[('id', '=', uid)]")).unwrap();
}

#[test]
fn panel_numeric_literals_admit_one_sign_but_no_arithmetic() {
    for source in [
        "[('id', '=', -1)]",
        "[('id', '=', +2)]",
        "[('weight', 'in', [-1.5, +2.5])]",
    ] {
        erp_views::validate_panel_domain(&ast(source)).unwrap();
    }
    for source in [
        "[('id', '=', -category)]",
        "[('id', '=', --1)]",
        "[('id', '=', -(1+2))]",
    ] {
        assert_eq!(
            erp_views::validate_panel_domain(&ast(source)).unwrap_err(),
            invalid(
                "Search-panel domain syntax is unsupported in version 1; additional syntax requires a later release"
            )
        );
    }
}

#[test]
fn model_utf8_byte_limit_and_invalid_ast_edges_are_admitted_independently() {
    let admitted = ast("[]");
    let schema = ViewSchema::new(1, &SCOPED).unwrap();
    for model in ["a".repeat(4096), "é".repeat(2048)] {
        let budget = DescriptionBudget::new();
        let payload = DescriptionBuilder::new(&schema, &budget)
            .build(|b| b.scoped_domain(&model, &admitted))
            .unwrap();
        let json = serde_json::to_value(&payload).unwrap();
        ReferenceVisitor::new(schema.clone())
            .references(&json, &DescriptionBudget::new())
            .unwrap();
        assert_eq!(json["model"].as_str().unwrap().len(), 4096);
    }
    let mut wire = document(&ast("[('name', '=', category)]"));
    wire["root"] = json!(4294967295u32);
    let payload = json!({"model":"demo.tag","ast":wire});
    let budget = DescriptionBudget::new();
    assert_eq!(
        ReferenceVisitor::new(schema)
            .references(&payload, &budget)
            .unwrap_err(),
        invalid("Description scoped domain has an invalid wire document")
    );
    assert_eq!(
        budget.check().unwrap_err(),
        invalid("Description remains refused after an earlier construction failure")
    );
}

#[test]
fn raw_and_constructed_visitors_share_expression_byte_admission_with_the_builder() {
    let admitted = ast("[]");
    let schema = ViewSchema::new(1, &SCOPED).unwrap();
    let encoded = admitted.encoded_len();
    let visitor = ReferenceVisitor::new(schema.clone());
    let raw = json!({"model":"demo.tag","ast":document(&admitted)});
    let budget = DescriptionBudget::with_limits(32 * 1024 * 1024, 1000, 2 * encoded).unwrap();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|b| b.scoped_domain("demo.tag", &admitted))
        .unwrap();
    visitor.payload_references(&payload, &budget).unwrap();
    let error = visitor.references(&raw, &budget).unwrap_err();
    assert_eq!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionBytes,
            max: 2 * encoded,
            actual: Some(3 * encoded)
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "aggregate expression bytes per request limit exceeded: maximum {}, reached {}",
            2 * encoded,
            3 * encoded
        )
    );
    assert_eq!(
        budget.check().unwrap_err(),
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionBytes,
            max: 2 * encoded,
            actual: None
        }
    );
}

#[test]
fn combined_payload_and_ast_depth_admits_exact_limit_and_refuses_one_more() {
    // Match description_expression.rs's deep-schema fixture stack in debug builds.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(combined_payload_and_ast_depth_body)
        .unwrap()
        .join()
        .unwrap();
}
fn combined_payload_and_ast_depth_body() {
    use erp_expr::{Expr, Literal};
    use erp_views::{DescriptionValue, DescriptionValueBuilder, ViewResult};
    fn build_nested(
        builder: DescriptionValueBuilder<'_>,
        admitted: &ValidatedAst,
        arrays: usize,
    ) -> ViewResult<DescriptionValue> {
        if arrays == 0 {
            builder.scoped_domain("demo.tag", admitted)
        } else {
            builder.array(1, |_, child| build_nested(child, admitted, arrays - 1))
        }
    }
    let mut node = &SCOPED;
    for _ in 0..32 {
        node = Box::leak(Box::new(SchemaNode::Array {
            item: node,
            max_items: 1,
        }));
    }
    let schema = ViewSchema::new(1, node).unwrap();
    let visitor = ReferenceVisitor::new(schema.clone());
    for nesting in [91, 92] {
        let mut rhs = Expr::Name("value".into());
        for _ in 0..nesting {
            rhs = Expr::List(vec![rhs]);
        }
        let expr = Expr::List(vec![Expr::Tuple(vec![
            Expr::Literal(Literal::Str("name".into())),
            Expr::Literal(Literal::Str("=".into())),
            rhs,
        ])]);
        let admitted = ValidatedAst::from_expr(&expr).unwrap();
        let budget = DescriptionBudget::new();
        let built = DescriptionBuilder::new(&schema, &budget)
            .build(|root| build_nested(root, &admitted, 32));
        let mut raw = json!({"model":"demo.tag","ast":document(&admitted)});
        for _ in 0..32 {
            raw = Value::Array(vec![raw]);
        }
        let raw_budget = DescriptionBudget::new();
        let raw_refs = visitor.references(&raw, &raw_budget);
        if nesting == 91 {
            let payload = built.unwrap();
            assert_eq!(refs(&raw_refs.unwrap()), ["root:value", "demo.tag:name"]);
            assert_eq!(
                refs(&visitor.payload_references(&payload, &budget).unwrap()),
                ["root:value", "demo.tag:name"]
            );
        } else {
            let expected = invalid("Description payload exceeds its depth limit");
            assert_eq!(built.unwrap_err(), expected);
            assert_eq!(raw_refs.unwrap_err(), expected);
            assert_eq!(
                budget.check().unwrap_err(),
                invalid("Description remains refused after an earlier construction failure")
            );
            assert_eq!(
                raw_budget.check().unwrap_err(),
                invalid("Description remains refused after an earlier construction failure")
            );
        }
    }
}

#[test]
fn panel_domain_document_references_validate_grammar_under_the_shared_budget() {
    let admitted = ast("[('category_id', '=', category_id)]");
    let document = erp_expr::wire::WireDocument::from_expr(admitted.as_expr()).unwrap();
    let budget = DescriptionBudget::new();
    let found = erp_views::panel_domain_references("demo.tag", &document, &budget).unwrap();
    assert_eq!(refs(&found), ["root:category_id", "demo.tag:category_id"]);
    let unsupported = ast("[('id', '=', value + 1)]");
    let document = erp_expr::wire::WireDocument::from_expr(unsupported.as_expr()).unwrap();
    let error = erp_views::panel_domain_references("demo.tag", &document, &budget).unwrap_err();
    assert_eq!(
        error,
        invalid(
            "Search-panel domain syntax is unsupported in version 1; additional syntax requires a later release"
        )
    );
    assert_eq!(
        budget.check().unwrap_err(),
        invalid("Description remains refused after an earlier construction failure")
    );
}

#[test]
fn host_reservation_growth_never_raises_or_revives_the_request_limit() {
    let budget = DescriptionBudget::with_limit(8).unwrap();
    let mut reservation = budget.reserve(3).unwrap();
    reservation.grow(5).unwrap();
    budget.check().unwrap();
    let expected = ViewError::LimitExceeded {
        limit: LimitKind::DescriptionBuildBytes,
        max: 8,
        actual: Some(9),
    };
    let error = reservation.grow(1).unwrap_err();
    assert_eq!(error, expected);
    assert_eq!(error.to_string(), expected.to_string());
    drop(reservation);
    let sticky = ViewError::LimitExceeded {
        limit: LimitKind::DescriptionBuildBytes,
        max: 8,
        actual: None,
    };
    assert_eq!(budget.reserve(1).unwrap_err(), sticky);
    assert_eq!(budget.check().unwrap_err().to_string(), sticky.to_string());

    let budget = DescriptionBudget::with_limit(8).unwrap();
    let mut reservation = budget.reserve(1).unwrap();
    assert_eq!(reservation.grow(usize::MAX).unwrap_err(), sticky);
    drop(reservation);
    assert_eq!(budget.check().unwrap_err(), sticky);
}
