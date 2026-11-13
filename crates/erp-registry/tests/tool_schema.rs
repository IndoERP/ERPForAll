// SPDX-License-Identifier: MIT OR Apache-2.0
//! The portable tool-schema subset: one schema per parameter shape and one accept/refuse pair per
//! validator keyword (AI1.REG-SCHEMA, R1, R2, R5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeSet;

use erp_fields::json::{Json, canonical, parse};
use erp_registry::tool_schema::{
    ObjectSchema, SchemaExportError, StringPattern, ToolSchema, params_schema, shape_schema,
    validate,
};
use erp_registry::{ArgShape, ParamSpec};

fn text(schema: &ToolSchema) -> String {
    String::from_utf8(canonical(&schema.to_json())).unwrap()
}

fn json(text: &str) -> Json {
    parse(text.as_bytes(), 16).unwrap()
}

fn refused(schema: &ToolSchema, value: &str) -> (String, String, String) {
    let v = validate(schema, &json(value)).expect_err(value);
    (
        v.path().to_owned(),
        v.expected().to_owned(),
        v.got().to_owned(),
    )
}

fn string() -> ToolSchema {
    ToolSchema::String {
        values: None,
        max_length: None,
        pattern: None,
    }
}

#[test]
fn every_closed_shape_has_its_schema() {
    for (shape, expected) in [
        (ArgShape::Boolean, r#"{"type":"boolean"}"#),
        (ArgShape::Integer { min: None }, r#"{"type":"integer"}"#),
        (
            ArgShape::Integer { min: Some(0) },
            r#"{"minimum":0,"type":"integer"}"#,
        ),
        (ArgShape::Number, r#"{"type":"number"}"#),
        (ArgShape::String, r#"{"type":"string"}"#),
        (
            ArgShape::Enum(&["write", "read", "read"]),
            r#"{"enum":["read","write"],"type":"string"}"#,
        ),
        (
            ArgShape::Array(&ArgShape::String),
            r#"{"items":{"type":"string"},"type":"array"}"#,
        ),
        (
            ArgShape::OrNull(&ArgShape::Boolean),
            r#"{"anyOf":[{"type":"boolean"},{"type":"null"}]}"#,
        ),
        (
            ArgShape::OrNull(&ArgShape::OrNull(&ArgShape::Boolean)),
            r#"{"anyOf":[{"type":"boolean"},{"type":"null"}]}"#,
        ),
    ] {
        assert_eq!(text(&shape_schema(&shape).unwrap()), expected, "{shape:?}");
    }
    let domain = text(&shape_schema(&ArgShape::Domain).unwrap());
    assert!(
        domain
            .starts_with(r#"{"items":{"anyOf":[{"enum":["!","&","|"],"type":"string"},{"items":"#)
    );
    assert!(domain.contains(r#""maxItems":3,"minItems":3"#), "{domain}");
    assert_eq!(
        shape_schema(&ArgShape::Array(&ArgShape::OrNull(&ArgShape::Open(
            "an object"
        )))),
        Err("an object")
    );
}

#[test]
fn params_schema_lists_parameters_and_names_open_ones() {
    const PARAMS: &[ParamSpec] = &[
        ParamSpec::with_shape("name", true, "a string", ArgShape::String),
        ParamSpec::with_shape(
            "limit",
            false,
            "a non-negative integer",
            ArgShape::OrNull(&ArgShape::Integer { min: Some(0) }),
        ),
    ];
    assert_eq!(
        text(&params_schema("m.x", "go", PARAMS).unwrap()),
        r#"{"additionalProperties":false,"properties":{"limit":{"anyOf":[{"minimum":0,"type":"integer"},{"type":"null"}]},"name":{"type":"string"}},"required":["name"],"type":"object"}"#
    );
    assert_eq!(
        text(&params_schema("m.x", "go", &[]).unwrap()),
        r#"{"additionalProperties":false,"properties":{},"required":[],"type":"object"}"#
    );
    let open = [
        ParamSpec::with_shape("name", true, "a string", ArgShape::String),
        ParamSpec::new("vals", true, "an object"),
    ];
    let error = params_schema("m.x", "go", &open).unwrap_err();
    assert_eq!(
        error,
        SchemaExportError::Unsupported {
            model: "m.x".into(),
            method: "go".into(),
            param: "vals",
            reason: "no shape declared"
        }
    );
    assert_eq!(
        error.to_string(),
        "m.x.go: parameter 'vals' has no closed schema (no shape declared)"
    );
    let twice = [
        ParamSpec::with_shape("a", true, "a string", ArgShape::String),
        ParamSpec::with_shape("a", false, "a string", ArgShape::String),
    ];
    assert_eq!(
        params_schema("m.x", "go", &twice).unwrap_err().to_string(),
        "m.x.go: parameter 'a' is declared twice"
    );
}

#[test]
fn validator_types() {
    for (schema, ok, bad, expected, got) in [
        (ToolSchema::Boolean, "true", "1", "a boolean", "integer"),
        (ToolSchema::Null, "null", "false", "null", "boolean"),
        (string(), "\"x\"", "null", "a string", "null"),
        (
            ToolSchema::Integer {
                minimum: None,
                maximum: None,
            },
            "-9223372036854775808",
            "1.0",
            "an integer",
            "number",
        ),
        (
            ToolSchema::Integer {
                minimum: None,
                maximum: None,
            },
            "0",
            "9223372036854775808",
            "an integer",
            "an integer outside the 64-bit range",
        ),
        (ToolSchema::Number, "1.5", "\"1\"", "a number", "string"),
        (
            ToolSchema::Number,
            "9007199254740992",
            "9007199254740993",
            "a number (integers up to 2^53 in magnitude)",
            "9007199254740993",
        ),
    ] {
        validate(&schema, &json(ok)).unwrap();
        assert_eq!(
            refused(&schema, bad),
            ("$".to_owned(), expected.to_owned(), got.to_owned()),
            "{schema:?} {bad}"
        );
    }
}

#[test]
fn validator_keywords() {
    let bounded = ToolSchema::Integer {
        minimum: Some(-1),
        maximum: Some(1),
    };
    validate(&bounded, &json("-1")).unwrap();
    validate(&bounded, &json("1")).unwrap();
    assert_eq!(refused(&bounded, "-2").1, "an integer >= -1");
    assert_eq!(refused(&bounded, "2").1, "an integer <= 1");

    let short = ToolSchema::String {
        values: None,
        max_length: Some(2),
        pattern: None,
    };
    validate(&short, &json("\"éé\"")).unwrap();
    assert_eq!(
        refused(&short, "\"ééé\""),
        (
            "$".into(),
            "at most 2 characters".into(),
            "3 characters".into()
        )
    );

    let closed = ToolSchema::String {
        values: Some(BTreeSet::from(["a".into(), "b".into()])),
        max_length: None,
        pattern: None,
    };
    validate(&closed, &json("\"b\"")).unwrap();
    assert_eq!(refused(&closed, "\"c\"").1, "one of \"a\", \"b\"");
    let long = format!("\"{}\"", "x".repeat(100));
    assert_eq!(
        refused(&closed, &long).2,
        format!("\"{}…\"", "x".repeat(64))
    );

    for (pattern, ok, bad) in [
        (StringPattern::Date, "2026-11-12", "2026-1-12"),
        (
            StringPattern::Datetime,
            "2026-11-12 08:30:00",
            "2026-11-12T08:30:00",
        ),
        (
            StringPattern::Uuid,
            "123e4567-E89B-12d3-a456-426614174000",
            "123e4567-e89b-12d3-a456-42661417400g",
        ),
    ] {
        let schema = ToolSchema::String {
            values: None,
            max_length: None,
            pattern: Some(pattern),
        };
        validate(&schema, &json(&format!("\"{ok}\""))).unwrap();
        assert!(refused(&schema, &format!("\"{bad}\"")).1.starts_with('a'));
        assert!(text(&schema).contains(&format!("\"pattern\":{:?}", pattern.regex())));
        // a multi-byte character never matches a position of the template
        assert!(!pattern.matches(&ok.replacen('1', "١", 1)));
    }

    let list = ToolSchema::Array {
        items: Box::new(ToolSchema::Boolean),
        min_items: Some(1),
        max_items: Some(2),
    };
    validate(&list, &json("[true]")).unwrap();
    assert_eq!(
        refused(&list, "[]"),
        ("$".into(), "at least 1 items".into(), "0 items".into())
    );
    assert_eq!(refused(&list, "[true,true,true]").1, "at most 2 items");
    assert_eq!(
        refused(&list, "[true,0]"),
        ("$[1]".into(), "a boolean".into(), "integer".into())
    );

    let nullable = ToolSchema::Boolean.or_null();
    validate(&nullable, &json("null")).unwrap();
    validate(&nullable, &json("false")).unwrap();
    assert_eq!(refused(&nullable, "\"x\"").1, "a boolean or null");

    let mut object = ObjectSchema::new();
    assert!(object.insert("a", ToolSchema::Boolean, Some("Flag"), true));
    assert!(object.insert("b c", string().or_null(), None, false));
    assert!(
        !object.insert("a", ToolSchema::Null, None, false),
        "a repeat is refused"
    );
    let object = ToolSchema::Object(object);
    assert_eq!(
        text(&object),
        r#"{"additionalProperties":false,"properties":{"a":{"description":"Flag","type":"boolean"},"b c":{"anyOf":[{"type":"string"},{"type":"null"}]}},"required":["a"],"type":"object"}"#
    );
    validate(&object, &json(r#"{"a": true, "b c": null}"#)).unwrap();
    assert_eq!(
        refused(&object, r#"{"b c": "x"}"#),
        ("$.a".into(), "a value (required)".into(), "nothing".into())
    );
    assert_eq!(
        refused(&object, r#"{"a": true, "b c": 1}"#),
        (
            "$[\"b c\"]".into(),
            "a string or null".into(),
            "integer".into()
        )
    );
    assert_eq!(
        refused(&object, r#"{"a": true, "zé": 1}"#),
        (
            "$[\"zé\"]".into(),
            "no such property".into(),
            "integer".into()
        )
    );
    assert_eq!(refused(&object, "[]").1, "an object");
    // an unknown key of plain letters is cut in the path like any other text (review S4)
    let long_key = "k".repeat(10_000);
    let (path, expected, _) = refused(&object, &format!(r#"{{"a": true, "{long_key}": 1}}"#));
    assert_eq!(path, format!("$[\"{}…\"]", "k".repeat(64)));
    assert_eq!(expected, "no such property");
}

/// The walk follows the schema: a value nested far deeper than the schema is refused at the
/// schema's depth, without recursing into it.
#[test]
fn deep_values_stop_at_the_schema_depth() {
    let deep = format!("{}{}", "[".repeat(500), "]".repeat(500));
    let value = parse(deep.as_bytes(), 1_000).unwrap();
    let schema = shape_schema(&ArgShape::Array(&ArgShape::Boolean)).unwrap();
    assert_eq!(
        validate(&schema, &value).unwrap_err().to_string(),
        "$[0]: expected a boolean, got array"
    );
}
