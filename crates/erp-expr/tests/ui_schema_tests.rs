//! Exercise the actual derive graph independently of the shell drift tests.
#![cfg(feature = "schema-gen")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../examples/support/schema.rs"]
mod schema;

use ts_rs::TS;

#[test]
fn nested_test_schema_uses_the_same_ast_wire_root() {
    let ast =
        erp_expr::wire::WireDocument::from_expr(&erp_expr::parse("9007199254740993").unwrap())
            .unwrap();
    let nested = schema::TestOnlyResponse {
        description: schema::TestOnlyDescription {
            expressions: vec![ast],
        },
    };
    let value = serde_json::to_value(nested).unwrap();
    assert_eq!(value["description"]["expressions"][0]["schema"], 1);
    assert!(value["description"]["expressions"][0]["nodes"].is_array());
    assert!(schema::TestOnlyDescription::decl().contains("Array<WireDocument>"));
    let dependencies = schema::TestOnlyResponse::dependencies();
    assert!(
        schema::TestOnlyDescription::dependencies()
            .iter()
            .any(|dependency| dependency.ts_name == "WireDocument")
    );
    assert!(
        dependencies
            .iter()
            .any(|dependency| dependency.ts_name == "TestOnlyDescription")
    );
}

fn nested_expression(source: &str) -> schema::TestOnlyResponse {
    schema::TestOnlyResponse {
        description: schema::TestOnlyDescription {
            expressions: vec![
                erp_expr::wire::WireDocument::from_expr(&erp_expr::parse(source).unwrap()).unwrap(),
            ],
        },
    }
}

#[test]
fn producer_accepts_exact_aggregate_boundaries_including_wrapper_bytes() {
    let response = nested_expression("1 + 2");
    let expected = serde_json::to_vec(&response).unwrap();
    let description_bytes = serde_json::to_vec(&response.description).unwrap().len();
    let limits = schema::DescriptionLimits::default()
        .with_max_description_bytes(description_bytes)
        .unwrap()
        .with_max_response_bytes(expected.len())
        .unwrap()
        .with_max_expressions(1)
        .unwrap()
        .with_max_nodes(3)
        .unwrap();
    assert_eq!(response.to_json_with_limits(limits).unwrap(), expected);
}

#[test]
fn producer_refuses_each_aggregate_boundary_without_partial_publication() {
    use schema::{DescriptionLimits, SchemaErrorKind};
    let response = nested_expression("1 + 2");
    let response_bytes = serde_json::to_vec(&response).unwrap().len();
    let description_bytes = serde_json::to_vec(&response.description).unwrap().len();
    let cases = [
        (
            DescriptionLimits::default()
                .with_max_description_bytes(description_bytes - 1)
                .unwrap(),
            format!(
                "DescriptionLimit at $.description: description byte limit exceeded ({})",
                description_bytes - 1
            ),
        ),
        (
            DescriptionLimits::default()
                .with_max_response_bytes(response_bytes - 1)
                .unwrap(),
            format!(
                "DescriptionLimit at $: response byte limit exceeded ({})",
                response_bytes - 1
            ),
        ),
        (
            DescriptionLimits::default().with_max_nodes(2).unwrap(),
            "DescriptionLimit at $.description.expressions: aggregate node limit exceeded (2)"
                .to_owned(),
        ),
    ];
    for (limits, message) in cases {
        let error = response.to_json_with_limits(limits).unwrap_err();
        assert_eq!(error.kind(), SchemaErrorKind::DescriptionLimit);
        assert_eq!(error.to_string(), message);
    }
    let mut two_expressions = nested_expression("1");
    two_expressions
        .description
        .expressions
        .push(erp_expr::wire::WireDocument::from_expr(&erp_expr::parse("2").unwrap()).unwrap());
    let error = two_expressions
        .to_json_with_limits(
            DescriptionLimits::default()
                .with_max_expressions(1)
                .unwrap(),
        )
        .unwrap_err();
    assert_eq!(error.kind(), SchemaErrorKind::DescriptionLimit);
    assert_eq!(
        error.to_string(),
        "DescriptionLimit at $.description.expressions: expression count limit exceeded (1)"
    );
}

#[test]
fn producer_validates_each_ast_before_nesting_it() {
    let mut response = nested_expression("1");
    response.description.expressions[0].schema = 9;
    let error = response
        .to_json_with_limits(schema::DescriptionLimits::default())
        .unwrap_err();
    assert_eq!(error.kind(), schema::SchemaErrorKind::InvalidAst);
    assert_eq!(
        error.to_string(),
        "InvalidAst at $.description.expressions[0]: InvalidAst at $.schema: unsupported schema version 9; reload or upgrade required"
    );
}

#[test]
fn producer_limits_can_only_be_lowered_and_empty_description_is_allowed() {
    use schema::{DescriptionLimits, SchemaErrorKind};
    let cases = [
        (
            DescriptionLimits::default().with_max_description_bytes(usize::MAX),
            "max_description_bytes",
            33554432,
        ),
        (
            DescriptionLimits::default().with_max_response_bytes(usize::MAX),
            "max_response_bytes",
            67108864,
        ),
        (
            DescriptionLimits::default().with_max_expressions(usize::MAX),
            "max_expressions",
            1024,
        ),
        (
            DescriptionLimits::default().with_max_nodes(usize::MAX),
            "max_nodes",
            262144,
        ),
    ];
    for (result, field, maximum) in cases {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), SchemaErrorKind::InvalidDescriptionLimit);
        assert_eq!(
            error.to_string(),
            format!(
                "InvalidDescriptionLimit at $.limits.{field}: expected 1..={maximum}, got {}",
                usize::MAX
            )
        );
    }
    for (result, field, maximum) in [
        (
            DescriptionLimits::default().with_max_description_bytes(0),
            "max_description_bytes",
            33554432,
        ),
        (
            DescriptionLimits::default().with_max_response_bytes(0),
            "max_response_bytes",
            67108864,
        ),
        (
            DescriptionLimits::default().with_max_expressions(0),
            "max_expressions",
            1024,
        ),
        (
            DescriptionLimits::default().with_max_nodes(0),
            "max_nodes",
            262144,
        ),
    ] {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), SchemaErrorKind::InvalidDescriptionLimit);
        assert_eq!(
            error.to_string(),
            format!("InvalidDescriptionLimit at $.limits.{field}: expected 1..={maximum}, got 0")
        );
    }
    let empty = schema::TestOnlyResponse {
        description: schema::TestOnlyDescription {
            expressions: Vec::new(),
        },
    };
    let output = empty
        .to_json_with_limits(DescriptionLimits::default())
        .unwrap();
    assert_eq!(output, serde_json::to_vec(&empty).unwrap());
}
