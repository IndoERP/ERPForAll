#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use erp_expr::wire::{MAX_ENCODED_BYTES, ValidatedAst, WireErrorKind};
use erp_expr::{EvalContext, Expr, Literal, eval_ast, parse};

fn reject(json: &str, path: &str, message: &str) {
    let error = ValidatedAst::from_json(json.as_bytes()).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(error.path(), path);
    assert_eq!(
        error.to_string(),
        format!("InvalidAst at {path}: {message}")
    );
}

#[test]
fn roundtrips_all_expression_shapes() {
    let sources = [
        "None",
        "True",
        "9223372036854775807",
        "1.25",
        "'é😀'",
        "b'abc'",
        "a",
        "-x",
        "a+b",
        "a and b",
        "1 < x <= 3",
        "a if b else c",
        "x.y",
        "x[1]",
        "x[::2]",
        "f(1,*a,k=2,**b)",
        "[1,2]",
        "(1,2)",
        "{1:2}",
        "(x := 2)",
        "f'hello {x!r:>4}'",
        "[x for x in y if x]",
        "(x for x in y)",
    ];
    for source in sources {
        let expr = parse(source).unwrap();
        let wire = ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap();
        let decoded = ValidatedAst::from_json(&wire).unwrap();
        assert_eq!(decoded.as_expr(), &expr, "{source}");
    }
}

/// Canonical length is retained at every admission boundary, rather than re-encoding in a caller.
#[test]
fn admitted_encoded_length_matches_each_constructor_and_scalar_encoding() {
    let sources = [
        "None",
        "True",
        "9223372036854775807",
        "1.25",
        "'é😀'",
        "b'abc'",
        "a",
        "-x",
        "a+b",
        "a and b",
        "1 < x <= 3",
        "a if b else c",
        "x.y",
        "x[1]",
        "x[::2]",
        "f(1,*a,k=2,**b)",
        "[1,2]",
        "(1,2)",
        "{1:2}",
        "(x := 2)",
        "f'hello {x!r:>4}'",
        "[x for x in y if x]",
        "(x for x in y)",
    ];
    for source in sources {
        let expr = parse(source).unwrap();
        assert_each_admitted_length(&expr);
    }
    // A NaN payload and signed zero must retain their exact bit encoding too.
    for bits in [0, 1_u64 << 63, 0x7ff8000000001234] {
        assert_each_admitted_length(&Expr::Literal(Literal::Float(f64::from_bits(bits))));
    }
    assert_each_admitted_length(&Expr::Literal(Literal::Int(i64::MIN)));
    assert_each_admitted_length(&Expr::Literal(Literal::Str(
        "\"\\\n\r\t\u{0}é😀".to_owned(),
    )));
}

fn assert_each_admitted_length(expr: &Expr) {
    let from_expr = ValidatedAst::from_expr(expr).unwrap();
    let bytes = from_expr.to_json().unwrap();
    assert_eq!(from_expr.encoded_len(), bytes.len());
    let from_json = ValidatedAst::from_json(&bytes).unwrap();
    assert_eq!(from_json.encoded_len(), from_json.to_json().unwrap().len());
    let from_document =
        ValidatedAst::try_from(erp_expr::wire::WireDocument::from_expr(expr).unwrap()).unwrap();
    assert_eq!(
        from_document.encoded_len(),
        from_document.to_json().unwrap().len()
    );
}

#[test]
fn admitted_encoded_length_counts_normalized_document_not_input_bytes() {
    let input = br#"{  "nodes" : [ { "name" : "\u00e9", "kind" : "name" } ], "schema" : 1e0 }"#;
    let ast = ValidatedAst::from_json(input).unwrap();
    let canonical = ast.to_json().unwrap();
    assert_ne!(input.len(), canonical.len());
    assert_eq!(ast.encoded_len(), canonical.len());
    assert_eq!(ast.as_expr(), &Expr::Name("é".to_owned()));
}

#[test]
fn preserves_float_bits_and_i64_boundaries() {
    for bits in [
        0,
        1_u64 << 63,
        0x7ff0000000000000,
        0xfff0000000000000,
        0x7ff8000000001234,
    ] {
        let expr = Expr::Literal(Literal::Float(f64::from_bits(bits)));
        let wire = ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap();
        let decoded = ValidatedAst::from_json(&wire).unwrap();
        match decoded.as_expr() {
            Expr::Literal(Literal::Float(value)) => assert_eq!(value.to_bits(), bits),
            other => panic!("{other:?}"),
        }
    }
    for value in [i64::MIN, 0, i64::MAX] {
        let expr = Expr::Literal(Literal::Int(value));
        let wire = ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap();
        assert_eq!(ValidatedAst::from_json(&wire).unwrap().as_expr(), &expr);
    }
}

#[test]
fn rejects_graph_and_scalar_invalidities() {
    reject(
        r#"{"schema":2,"nodes":[]}"#,
        "$.schema",
        "unsupported schema version 2; reload or upgrade required",
    );
    reject(
        r#"{"schema":1,"nodes":[]}"#,
        "$.nodes",
        "expected 1..=65536 nodes",
    );
    reject(
        r#"{"schema":1,"nodes":[{"kind":"starred","value":0}]}"#,
        "$.nodes[0]",
        "child index 0 must point strictly backwards",
    );
    reject(
        r#"{"schema":1,"nodes":[{"kind":"name","name":"x"},{"kind":"list","items":[0,0]}]}"#,
        "$.nodes[1]",
        "child index 0 has more than one incoming edge",
    );
    reject(
        r#"{"schema":1,"nodes":[{"kind":"name","name":"unused"},{"kind":"name","name":"root"}]}"#,
        "$.nodes[0]",
        "nonroot node must have exactly one incoming edge",
    );
    reject(
        r#"{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"int","value":"01"}}]}"#,
        "$.nodes[0].value.value",
        "expected canonical i64 decimal string",
    );
    reject(
        r#"{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"float","value":"7FF0000000000000"}}]}"#,
        "$.nodes[0].value.value",
        "expected 16 lowercase hexadecimal float bits",
    );
}

#[test]
fn strict_json_rejects_duplicates_unknown_fields_and_surrogates() {
    for json in [
        r#"{"schema":1,"schema":1,"nodes":[]}"#,
        r#"{"schema":1,"nodes":[],"extra":0}"#,
        r#"{"schema":1,"nodes":[{"kind":"name","name":"x","name":"y"}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"name","name":"\ud800"}]}"#,
    ] {
        let error = ValidatedAst::from_json(json.as_bytes()).unwrap_err();
        assert_eq!(error.kind(), WireErrorKind::InvalidAst);
        assert_eq!(error.path(), "$");
        // serde line/column diagnostics are version-specific; full text includes its source verbatim.
        assert_eq!(
            error.to_string(),
            format!(
                "InvalidAst at $: invalid JSON: {}",
                std::error::Error::source(&error).unwrap()
            )
        );
    }
}

#[test]
fn bounds_bytes_and_depth_before_deserialization() {
    let error = ValidatedAst::from_json(&vec![b' '; MAX_ENCODED_BYTES + 1]).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(
        error.to_string(),
        "InvalidAst at $: encoded JSON exceeds 16777216 bytes"
    );
    reject(&"[".repeat(17), "$", "JSON nesting exceeds 16");
}

#[test]
fn source_and_decoded_evaluation_agree() {
    let expr = parse("[x * 2 for x in [1,2,3] if x > 1]").unwrap();
    let decoded =
        ValidatedAst::from_json(&ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap())
            .unwrap();
    assert_eq!(
        eval_ast(decoded.as_expr(), &EvalContext::standard()).unwrap(),
        eval_ast(&expr, &EvalContext::standard()).unwrap()
    );
}

#[test]
fn logical_height_matches_parser_chain_boundary() {
    let source = std::iter::repeat_n("1", 100).collect::<Vec<_>>().join("+");
    let expr = parse(&source).unwrap();
    ValidatedAst::from_expr(&expr).unwrap();
    let deeper = Expr::Starred(Box::new(expr));
    let error = ValidatedAst::from_expr(&deeper).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(
        error.to_string(),
        "InvalidAst at $.nodes: logical AST depth exceeds 100"
    );
}

#[path = "data/cpython_goldens.rs"]
mod goldens;

#[test]
fn unchanged_goldens_survive_wire_roundtrip() {
    for &(source, succeeds, expected) in goldens::GOLDENS {
        let expr = parse(source).unwrap();
        let decoded =
            ValidatedAst::from_json(&ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap())
                .unwrap();
        let result = eval_ast(decoded.as_expr(), &EvalContext::standard());
        let actual = match result {
            Ok(value) => {
                assert!(succeeds, "{source}");
                value.repr()
            }
            Err(error) => {
                assert!(!succeeds, "{source}");
                error.to_string()
            }
        };
        assert_eq!(actual, expected, "{source}");
    }
}

#[test]
fn comprehension_clause_height_counts_each_clause() {
    let source = format!(
        "[x {}]",
        std::iter::repeat_n("for x in y", 98)
            .collect::<Vec<_>>()
            .join(" ")
    );
    let expr = parse(&source).unwrap();
    let bytes = ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap();
    assert_eq!(ValidatedAst::from_json(&bytes).unwrap().as_expr(), &expr);
    let Expr::ListComp {
        elt,
        mut generators,
    } = expr
    else {
        panic!("expected comprehension")
    };
    generators.push(generators[0].clone());
    let error = ValidatedAst::from_expr(&Expr::ListComp { elt, generators }).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(
        error.to_string(),
        "InvalidAst at $.nodes: logical AST depth exceeds 100"
    );
}

#[test]
fn node_limit_is_inclusive() {
    let expr = Expr::List(vec![Expr::Literal(Literal::None); 65_535]);
    let encoded = ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap();
    ValidatedAst::from_json(&encoded).unwrap();
    let excessive = Expr::List(vec![Expr::Literal(Literal::None); 65_536]);
    let error = ValidatedAst::from_expr(&excessive).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(
        error.to_string(),
        "InvalidAst at $.nodes: expected 1..=65536 nodes"
    );
}

#[test]
fn lexer_nonfinite_float_is_preserved_by_wire() {
    let expr = parse("1e9999").unwrap();
    let Expr::Literal(Literal::Float(value)) = &expr else {
        panic!("expected float")
    };
    assert_eq!(value.to_bits(), f64::INFINITY.to_bits());
    let bytes = ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap();
    let decoded = ValidatedAst::from_json(&bytes).unwrap();
    let result = eval_ast(decoded.as_expr(), &EvalContext::standard()).unwrap();
    assert_eq!(result.repr(), "inf");
}

#[test]
fn validates_shape_invariants_before_executable_construction() {
    reject(
        r#"{"schema":1,"nodes":[{"kind":"bool_op","op":"and","values":[]}]}"#,
        "$.nodes[0].values",
        "boolean operation requires at least two values",
    );
    reject(
        r#"{"schema":1,"nodes":[{"kind":"compare","left":0,"ops":["eq"],"comparators":[]}]}"#,
        "$.nodes[0].comparators",
        "comparison requires matching nonempty operators and comparators",
    );
    reject(
        r#"{"schema":1,"nodes":[{"kind":"list_comp","elt":0,"generators":[]}]}"#,
        "$.nodes[0].generators",
        "comprehension requires at least one clause",
    );
    reject(
        r#"{"schema":1,"nodes":[{"kind":"formatted_value","value":0,"conversion":"rs","format_spec":null}]}"#,
        "$.nodes[0].conversion",
        "expected one Unicode scalar or null",
    );
}

#[test]
fn every_operator_and_slice_shape_roundtrips() {
    for source in [
        "+x",
        "-x",
        "not x",
        "~x",
        "x+y",
        "x-y",
        "x*y",
        "x/y",
        "x//y",
        "x%y",
        "x**y",
        "x&y",
        "x|y",
        "x^y",
        "x<<y",
        "x>>y",
        "x and y",
        "x or y",
        "x==y",
        "x!=y",
        "x<y",
        "x<=y",
        "x>y",
        "x>=y",
        "x in y",
        "x not in y",
        "x is y",
        "x is not y",
        "x[:]",
        "x[1:]",
        "x[:2]",
        "x[1:2:3]",
    ] {
        let expr = parse(source).unwrap();
        let json = ValidatedAst::from_expr(&expr).unwrap().to_json().unwrap();
        assert_eq!(
            ValidatedAst::from_json(&json).unwrap().as_expr(),
            &expr,
            "{source}"
        );
    }
}

#[test]
fn nullable_fields_are_required_and_bad_scalars_fail_strictly() {
    for json in [
        r#"{"schema":1,"nodes":[{"kind":"name","name":"f"},{"kind":"literal","value":{"kind":"none"}},{"kind":"call","func":0,"args":[],"keywords":[{"value":1}]}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"name","name":"x"},{"kind":"subscript","value":0,"slice":{"kind":"slice","lower":null,"upper":null}}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"int","value":1}}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"bytes","value":[256]}}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"name","name":"x"},{"kind":"starred","value":0.5}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"none","extra":1}}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"name","kind":"name","name":"x"}]}"#,
    ] {
        let error = ValidatedAst::from_json(json.as_bytes()).unwrap_err();
        assert_eq!(error.kind(), WireErrorKind::InvalidAst);
        assert_eq!(error.path(), "$");
        assert_eq!(
            error.to_string(),
            format!(
                "InvalidAst at $: invalid JSON: {}",
                std::error::Error::source(&error).unwrap()
            )
        );
    }
}

#[test]
fn integral_json_numbers_match_browser_number_semantics() {
    let decoded = ValidatedAst::from_json(br#"{"schema":1.0,"nodes":[{"kind":"literal","value":{"kind":"bytes","value":[0.0,2.55e2]}},{"kind":"starred","value":0e0}]}"#).unwrap();
    assert_eq!(
        decoded.as_expr(),
        &Expr::Starred(Box::new(Expr::Literal(Literal::Bytes(vec![0, 255]))))
    );
}

#[test]
fn bounded_raw_node_decode_rejects_before_materializing_full_input() {
    let mut raw = String::from("{\"schema\":1,\"nodes\":[");
    for index in 0..65_537 {
        if index != 0 {
            raw.push(',');
        }
        raw.push_str(r#"{"kind":"literal","value":{"kind":"none"}}"#);
    }
    raw.push_str("]}");
    let error = ValidatedAst::from_json(raw.as_bytes()).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(error.path(), "$");
    assert_eq!(
        error.to_string(),
        format!(
            "InvalidAst at $: invalid JSON: AST node array exceeds 65536 entries at line 1 column {}",
            raw.rfind(",{").unwrap() + 1
        )
    );
}

#[test]
fn bounded_raw_node_decode_stops_before_overflow_payload() {
    let prefix = format!(
        "{{\"schema\":1,\"nodes\":[{}",
        std::iter::repeat_n(r#"{"kind":"literal","value":{"kind":"none"}}"#, 65_536)
            .collect::<Vec<_>>()
            .join(",")
    );
    // The raw visitor accepts exactly the cap. Graph validation is a separate layer.
    let exact: erp_expr::wire::WireDocument =
        serde_json::from_str(&format!("{prefix}]}}")).unwrap();
    assert_eq!(exact.nodes.len(), 65_536);
    // A byte outside u8 would fail if the excess node's payload decoder were entered.
    let overflow =
        format!(r#"{prefix},{{"kind":"literal","value":{{"kind":"bytes","value":[256]}}}}]}}"#);
    let error = ValidatedAst::from_json(overflow.as_bytes()).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(error.path(), "$");
    assert_eq!(
        error.to_string(),
        format!(
            "InvalidAst at $: invalid JSON: AST node array exceeds 65536 entries at line 1 column {}",
            prefix.len() + 1
        )
    );
}

#[test]
fn near_source_limit_ast_fits_transport() {
    let source = format!("[{}0]", "0,".repeat(32_766));
    assert_eq!(source.len(), 65_535);
    let expr = parse(&source).unwrap();
    let bytes = erp_expr::wire::encode(&expr).unwrap();
    assert_eq!(ValidatedAst::from_json(&bytes).unwrap().as_expr(), &expr);
}

#[test]
fn raw_tree_height_rejects_one_over_for_chains_and_comprehensions() {
    let mut nodes = vec![serde_json::json!({"kind":"name","name":"x"})];
    for index in 1..100 {
        nodes.push(serde_json::json!({"kind":"starred","value":index - 1}));
    }
    let bytes = serde_json::to_vec(&serde_json::json!({"schema":1,"nodes":nodes})).unwrap();
    ValidatedAst::from_json(&bytes).unwrap();
    nodes.push(serde_json::json!({"kind":"starred","value":99}));
    let raw = serde_json::to_string(&serde_json::json!({"schema":1,"nodes":nodes})).unwrap();
    reject(&raw, "$.nodes", "logical AST depth exceeds 100");

    for clauses in [98, 99] {
        let mut nodes = vec![serde_json::json!({"kind":"name","name":"x"}); 1 + 2 * clauses];
        let generators: Vec<_> = (0..clauses)
            .map(|index| serde_json::json!({"target":1 + 2 * index,"iter":2 + 2 * index,"ifs":[]}))
            .collect();
        nodes.push(serde_json::json!({"kind":"list_comp","elt":0,"generators":generators}));
        let raw = serde_json::to_string(&serde_json::json!({"schema":1,"nodes":nodes})).unwrap();
        if clauses == 98 {
            ValidatedAst::from_json(raw.as_bytes()).unwrap();
        } else {
            reject(&raw, "$.nodes", "logical AST depth exceeds 100");
        }
    }
}

#[test]
fn encoded_byte_cap_is_inclusive_and_escaped_text_is_charged() {
    let overhead = erp_expr::wire::encode(&Expr::Literal(Literal::Str(String::new())))
        .unwrap()
        .len();
    let exact = Expr::Literal(Literal::Str("a".repeat(MAX_ENCODED_BYTES - overhead)));
    let bytes = erp_expr::wire::encode(&exact).unwrap();
    assert_eq!(bytes.len(), MAX_ENCODED_BYTES);
    ValidatedAst::from_json(&bytes).unwrap();
    let excessive = Expr::Literal(Literal::Str("a".repeat(MAX_ENCODED_BYTES - overhead + 1)));
    let error = erp_expr::wire::encode(&excessive).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(
        error.to_string(),
        "InvalidAst at $: encoded JSON exceeds 16777216 bytes"
    );
    let escaped = Expr::Literal(Literal::Str("\0".repeat(MAX_ENCODED_BYTES / 6 + 1)));
    let error = erp_expr::wire::encode(&escaped).unwrap_err();
    assert_eq!(error.kind(), WireErrorKind::InvalidAst);
    assert_eq!(
        error.to_string(),
        "InvalidAst at $: encoded JSON exceeds 16777216 bytes"
    );
}

#[test]
fn canonical_integer_validation_rejects_overflow_and_alternate_spellings() {
    for value in [
        "",
        "-0",
        "+1",
        "00",
        "-01",
        "1.0",
        "1e0",
        " 1",
        "1 ",
        "9223372036854775808",
        "-9223372036854775809",
    ] {
        let raw = serde_json::to_string(&serde_json::json!({"schema":1,"nodes":[{"kind":"literal","value":{"kind":"int","value":value}}]})).unwrap();
        reject(
            &raw,
            "$.nodes[0].value.value",
            "expected canonical i64 decimal string",
        );
    }
}

#[test]
fn numeric_tokens_match_browser_rounding_before_integer_validation() {
    #[derive(serde::Deserialize)]
    struct TokenCase {
        token: String,
        rounded: Option<u32>,
    }
    let cases: Vec<TokenCase> =
        serde_json::from_str(include_str!("data/wire_number_tokens.json")).unwrap();
    for case in cases {
        let raw = format!(
            r#"{{"schema":1,"nodes":[{{"kind":"name","name":"x"}},{{"kind":"starred","value":{}}}]}}"#,
            case.token
        );
        let result = ValidatedAst::from_json(raw.as_bytes());
        match case.rounded {
            Some(0) => assert_eq!(
                result.unwrap().as_expr(),
                &Expr::Starred(Box::new(Expr::Name("x".into()))),
                "{}",
                case.token
            ),
            Some(index) => {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), WireErrorKind::InvalidAst);
                assert_eq!(
                    error.to_string(),
                    format!(
                        "InvalidAst at $.nodes[1]: child index {index} must point strictly backwards"
                    ),
                    "{}",
                    case.token
                );
            }
            None => {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), WireErrorKind::InvalidAst);
                assert_eq!(error.path(), "$", "{}", case.token);
                // JSON syntax/range diagnostics are supplied by serde; the wrapper must preserve
                // the complete provider diagnostic and its source rather than swallowing either.
                assert_eq!(
                    error.to_string(),
                    format!(
                        "InvalidAst at $: invalid JSON: {}",
                        std::error::Error::source(&error).unwrap()
                    ),
                    "{}",
                    case.token
                );
            }
        }
    }
}

#[test]
fn numeric_admission_keeps_original_json_grammar_and_types() {
    for token in [
        "01", "-01", "+1", "1.", "1e", "1e+", ".0", "0x0", "1_0", "0 0",
    ] {
        let raw = format!(
            r#"{{"schema":1,"nodes":[{{"kind":"name","name":"x"}},{{"kind":"starred","value":{token}}}]}}"#
        );
        let error = ValidatedAst::from_json(raw.as_bytes()).unwrap_err();
        assert_eq!(error.kind(), WireErrorKind::InvalidAst, "{token}");
        assert_eq!(error.path(), "$", "{token}");
        // Provider syntax text includes token-dependent positions; preserve it completely.
        assert_eq!(
            error.to_string(),
            format!(
                "InvalidAst at $: invalid JSON: {}",
                std::error::Error::source(&error).unwrap()
            )
        );
    }
    for raw in [
        r#"{"schema":1,"nodes":[{"kind":"name","name":0e0}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"bool","value":1e0}}]}"#,
        r#"{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"int","value":1e0}}]}"#,
    ] {
        let error = ValidatedAst::from_json(raw.as_bytes()).unwrap_err();
        assert_eq!(error.kind(), WireErrorKind::InvalidAst);
        assert_eq!(error.path(), "$");
        assert_eq!(
            error.to_string(),
            format!(
                "InvalidAst at $: invalid JSON: {}",
                std::error::Error::source(&error).unwrap()
            )
        );
    }
    let expression = Expr::Literal(Literal::Str("é😀 0.9999999999999999 \\\"1e9\\\" \\".into()));
    let encoded = erp_expr::wire::encode(&expression).unwrap();
    assert_eq!(
        ValidatedAst::from_json(&encoded).unwrap().as_expr(),
        &expression
    );
}

#[test]
fn numeric_admission_rounds_all_integer_slots_without_global_parser_feature() {
    let raw = r#"{"schema":1.0000000000000001,"nodes":[{"kind":"literal","value":{"kind":"bytes","value":[-0,1e-9999,254.99999999999999999]}}]}"#;
    assert_eq!(
        ValidatedAst::from_json(raw.as_bytes()).unwrap().as_expr(),
        &Expr::Literal(Literal::Bytes(vec![0, 0, 255]))
    );
    reject(
        r#"{"schema":1e9,"nodes":[]}"#,
        "$.schema",
        "unsupported schema version 1000000000; reload or upgrade required",
    );
}

#[test]
fn zero_exponent_tokens_use_actual_raw_and_validated_numeric_slots() {
    use erp_expr::wire::{WireDocument, WireNode};

    let raw: WireDocument = serde_json::from_str(
        r#"{"schema":1e0,"nodes":[{"kind":"list","items":[0e0,4294967295E0]}]}"#,
    )
    .unwrap();
    assert_eq!(raw.schema, 1);
    let [WireNode::List { items }] = raw.nodes.as_slice() else {
        panic!("expected raw list node");
    };
    assert_eq!(items, &[0, u32::MAX]);
    // This raw maximum index is not a valid graph. Admission is checked separately
    // on an actual two-node postorder tree, never claimed from raw DTO success.
    let admitted = ValidatedAst::from_json(
        br#"{"schema":1E0,"nodes":[{"kind":"name","name":"x"},{"kind":"list","items":[0e0]}]}"#,
    )
    .unwrap();
    assert_eq!(admitted.as_expr(), &parse("[x]").unwrap());
}
