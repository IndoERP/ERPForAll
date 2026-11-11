//! Common-source golden bridge and independently labelled generated differential corpus.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../examples/support/fixtures.rs"]
mod fixtures;
use fixtures::goldens as original;

use erp_expr::{EvalContext, eval};

#[test]
fn golden_bridge_preserves_every_original_row_and_parsed_ast() {
    let rows = fixtures::golden_rows().unwrap();
    assert_eq!(rows.len(), 482, "golden table size is pinned");
    assert_eq!(rows.len(), original::GOLDENS.len());
    let transport = serde_json::to_value(&rows).unwrap();
    for (ordinal, (row, &(source, evaluates, expected))) in
        rows.iter().zip(original::GOLDENS).enumerate()
    {
        assert_eq!(row.ordinal, ordinal);
        assert_eq!(row.source, source);
        assert_eq!(row.evaluates, evaluates);
        assert_eq!(row.expected, expected);
        assert_eq!(transport[ordinal]["ordinal"], ordinal);
        assert_eq!(transport[ordinal]["source"], source);
        assert_eq!(transport[ordinal]["evaluates"], evaluates);
        assert_eq!(transport[ordinal]["expected"], expected);
        let encoded = serde_json::to_vec(&row.ast).unwrap();
        let decoded = erp_expr::wire::ValidatedAst::from_json(&encoded).unwrap();
        let actual = erp_expr::eval_ast(decoded.as_expr(), &EvalContext::standard());
        match actual {
            Ok(value) => {
                assert!(evaluates, "row {ordinal} should raise {expected}");
                assert_eq!(value.repr(), expected, "row {ordinal}");
            }
            Err(error) => {
                assert!(!evaluates, "row {ordinal} should return {expected}");
                assert_eq!(error.to_string(), expected, "row {ordinal}");
            }
        }
    }
}

#[test]
fn changing_bridged_expectation_cannot_change_the_actual_result() {
    let mut rows = fixtures::golden_rows().unwrap();
    let row = &mut rows[0];
    row.expected = "deliberately mutated expectation";
    let actual = match eval(row.source, &EvalContext::standard()) {
        Ok(value) => value.repr(),
        Err(error) => error.to_string(),
    };
    assert_ne!(actual, row.expected);
    assert_eq!(actual, original::GOLDENS[0].2);
}

#[test]
fn generated_differential_is_bounded_deterministic_and_keeps_parse_errors() {
    let left = fixtures::differential_corpus().unwrap();
    let right = fixtures::differential_corpus().unwrap();
    assert_eq!(
        serde_json::to_vec(&left).unwrap(),
        serde_json::to_vec(&right).unwrap()
    );
    let exported = serde_json::to_value(&left).unwrap();
    let families = ["core", "containers", "format", "dates", "errors", "parser"];
    assert_eq!(left.rows.len(), families.len() * 1000);
    for family in families {
        let rows: Vec<_> = exported["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["family"] == family)
            .collect();
        assert_eq!(rows.len(), 1000, "family {family}");
        for (case, row) in rows.iter().enumerate() {
            assert_eq!(row["case"], case);
            assert_eq!(row["seed"], 0x5549_5801_u32);
        }
    }
    assert_eq!(
        left.context,
        "EvalContext::standard; no host vars; no clock"
    );
    let mut parsed = 0;
    let mut rejected = 0;
    for (ordinal, row) in left.rows.iter().enumerate() {
        assert_eq!(row.ordinal, ordinal);
        assert!(row.source.len() <= 256);
        match &row.outcome {
            fixtures::DifferentialOutcome::Parsed { ast, result } => {
                parsed += 1;
                if row.family == "containers" {
                    assert!(
                        matches!(result, fixtures::EvalOutcome::Ok { .. }),
                        "container operation must execute, not fail at name lookup: {}",
                        row.source
                    );
                }
                let decoded =
                    erp_expr::wire::ValidatedAst::from_json(&serde_json::to_vec(ast).unwrap())
                        .unwrap();
                let actual = fixtures::EvalOutcome::from_result(erp_expr::eval_ast(
                    decoded.as_expr(),
                    &EvalContext::standard(),
                ));
                assert_eq!(&actual, result);
            }
            fixtures::DifferentialOutcome::ParseError { class, message } => {
                rejected += 1;
                let error = erp_expr::parse(&row.source).unwrap_err();
                assert_eq!(*class, error.class());
                assert_eq!(message, &error.to_string());
            }
        }
    }
    assert!(parsed > 0);
    assert!(rejected > 0);
}

#[test]
fn unknown_presentation_type_keeps_type_specific_diagnostic() {
    for (source, expected) in [
        (
            "'{:q}'.format(1)",
            "ValueError: Unknown format code 'q' for object of type 'int'",
        ),
        (
            "'{:q}'.format('a')",
            "ValueError: Unknown format code 'q' for object of type 'str'",
        ),
        (
            "'{:q}'.format(1.0)",
            "ValueError: Unknown format code 'q' for object of type 'float'",
        ),
        (
            "'{:qq}'.format(1)",
            "ValueError: Invalid format specifier 'qq' for object of type 'int'",
        ),
    ] {
        let error = eval(source, &EvalContext::standard()).unwrap_err();
        assert_eq!(error.class(), "ValueError");
        assert_eq!(error.to_string(), expected);
    }
}
