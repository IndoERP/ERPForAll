// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
use erp_addon_barcodes::engine::Nomenclature;
use serde_json::{Value, json};

fn parse(rules: Value, conversion: &str, code: &str) -> Value {
    let source = json!({"version":1,"conversion":conversion,"rules":rules}).to_string();
    let engine = Nomenclature::from_json(&source).unwrap();
    serde_json::from_str(&engine.parse_json(code).unwrap()).unwrap()
}
fn rule(id: i64, sequence: i32, encoding: &str, kind: &str, pattern: &str, alias: &str) -> Value {
    json!({"id":id,"sequence":sequence,"encoding":encoding,"kind":kind,"pattern":pattern,"alias":alias})
}
#[test]
fn encodings_and_conversion_policies() {
    for (encoding, valid, invalid) in [
        ("ean8", "12345670", "12345678"),
        ("ean13", "1020034051259", "1020034051250"),
        ("upca", "036000291452", "036000291453"),
    ] {
        let rules = json!([rule(1, 0, encoding, "product", ".*", "0")]);
        assert_eq!(parse(rules.clone(), "none", valid)["status"], "matched");
        for code in [invalid, "", "1", "１２３４５６７０"] {
            assert_eq!(parse(rules.clone(), "none", code)["status"], "no_match");
        }
    }
    let ean = json!([rule(1, 0, "ean13", "product", ".*", "0")]);
    let upc = json!([rule(1, 0, "upca", "product", ".*", "0")]);
    for mode in ["none", "ean2upc", "upc2ean", "always"] {
        let forward = parse(ean.clone(), mode, "036000291452");
        assert_eq!(
            forward["status"],
            if matches!(mode, "upc2ean" | "always") {
                "matched"
            } else {
                "no_match"
            }
        );
        let reverse = parse(upc.clone(), mode, "0036000291452");
        assert_eq!(
            reverse["status"],
            if matches!(mode, "ean2upc" | "always") {
                "matched"
            } else {
                "no_match"
            }
        );
    }
    assert_eq!(
        parse(ean, "always", "036000291452")["items"][0]["code"],
        "0036000291452"
    );
}
#[test]
fn exact_numeric_spans_and_base_checksums() {
    let r = parse(
        json!([rule(1, 0, "ean13", "weight", "1........{NND}.", "0")]),
        "none",
        "1020034051259",
    );
    assert_eq!(r["items"][0]["value"], "12.5");
    assert_eq!(r["items"][0]["base_code"], "1020034050009");
    let r = parse(
        json!([rule(1, 0, "any", "weight", "AB+{NDD}", "0")]),
        "none",
        "ABBB125tail",
    );
    assert_eq!(r["items"][0]["value"], "1.25");
    assert_eq!(r["items"][0]["base_code"], "ABBB000tail");
    for (pattern, code, expected) in [
        ("{DD}", "05", "0.05"),
        ("{NN}", "05", "5"),
        ("{ND}", "00", "0"),
    ] {
        assert_eq!(
            parse(
                json!([rule(1, 0, "any", "product", pattern, "0")]),
                "none",
                code
            )["items"][0]["value"],
            expected
        );
    }
    let u = parse(
        json!([rule(1, 0, "upca", "product", "......{NNDDD}.", "0")]),
        "none",
        "036000291452",
    );
    assert_eq!(u["items"][0]["value"], "29.145");
    assert_eq!(u["items"][0]["base_code"], "036000000009");
    let r = parse(
        json!([rule(1, 0, "ean8", "product", "66{NN}....", "0")]),
        "none",
        "66012344",
    );
    assert_eq!(r["items"][0]["base_code"], "66002345");
}
#[test]
fn ordering_aliases_and_prefix_matching() {
    let rules = json!([
        rule(3, 90, "any", "product", ".*", "0"),
        rule(2, 2, "any", "lot", "Y", "0"),
        rule(1, 1, "any", "alias", "X", "Y42")
    ]);
    let r = parse(rules, "none", "X99");
    assert_eq!(r["items"][0]["type"], "lot");
    assert_eq!(r["code"], "Y42");
    assert_eq!(r["original_code"], "X99");
    let r = parse(
        json!([
            rule(2, 0, "any", "lot", "A|B", "0"),
            rule(1, 0, "any", "product", ".*", "0")
        ]),
        "none",
        "B",
    );
    assert_eq!(r["items"][0]["type"], "product");
    assert_eq!(
        parse(json!([rule(1, 0, "any", "lot", "A|B", "0")]), "none", "xB")["status"],
        "no_match"
    );
    let r = parse(json!([rule(1, 0, "any", "alias", "X", "X")]), "none", "X");
    assert_eq!(r["status"], "no_match");
    assert_eq!(r["code"], "X");
    assert_eq!(
        parse(
            json!([rule(1, 0, "any", "product", r"A\{B\}\.\\", "0")]),
            "none",
            r"A{B}.\"
        )["status"],
        "matched"
    );
}
#[test]
fn rejects_bad_configuration_and_resource_exhaustion() {
    for (pattern, message) in [
        (
            "{}",
            "Numeric pattern must contain N digits followed by D digits.",
        ),
        (
            "{DN}",
            "Numeric pattern must contain N digits followed by D digits.",
        ),
        ("{N}{D}", "A pattern may contain only one numeric token."),
        ("*", "Pattern is not supported by the bounded regex engine."),
        (
            "(?=A)",
            "Pattern is not supported by the bounded regex engine.",
        ),
        (
            "{NNNNNNNNNNNNNNNNNNN}",
            "Numeric pattern exceeds 18 digits.",
        ),
    ] {
        let source=json!({"version":1,"conversion":"none","rules":[rule(1,0,"any","product",pattern,"0")]}).to_string();
        let e = Nomenclature::from_json(&source).unwrap_err();
        assert_eq!(e.kind(), "pattern");
        assert_eq!(e.to_string(), message);
    }
    let e =
        Nomenclature::from_json(r#"{"version":1,"conversion":"none","rules":[],"unknown":true}"#)
            .unwrap_err();
    assert_eq!(e.kind(), "configuration");
    assert_eq!(e.to_string(), "Invalid barcode snapshot JSON.");
    let source=json!({"version":1,"conversion":"none","rules":[rule(1,0,"any","product",".*","0"),rule(1,1,"any","product",".*","0")]}).to_string();
    let e = Nomenclature::from_json(&source).unwrap_err();
    assert_eq!(e.kind(), "configuration");
    assert_eq!(e.to_string(), "Rule IDs must be positive and unique.");
    let n = Nomenclature::from_json(r#"{"version":1,"conversion":"none","rules":[]}"#).unwrap();
    let e = n.parse_json(&"x".repeat(4097)).unwrap_err();
    assert_eq!(e.kind(), "limit");
    assert_eq!(e.to_string(), "Barcode exceeds 4096 bytes.");
    assert_eq!(
        parse(
            json!([rule(1, 0, "any", "product", ".*", "0")]),
            "none",
            &"x".repeat(4096)
        )["status"],
        "matched"
    );
}

#[test]
fn every_rule_set_limit_is_explicit() {
    fn error(source: String, kind: &str, message: &str) {
        let e = Nomenclature::from_json(&source).unwrap_err();
        assert_eq!(e.kind(), kind);
        assert_eq!(e.to_string(), message);
    }
    error(
        " ".repeat(1048577),
        "limit",
        "Snapshot exceeds 1048576 bytes.",
    );
    let mut rules: Vec<Value> = (1..=256)
        .map(|id| rule(id, 0, "any", "product", ".*", "0"))
        .collect();
    let source = json!({"version":1,"conversion":"none","rules":rules}).to_string();
    assert!(Nomenclature::from_json(&source).is_ok());
    rules.push(rule(257, 0, "any", "product", ".*", "0"));
    error(
        json!({"version":1,"conversion":"none","rules":rules}).to_string(),
        "limit",
        "Nomenclature exceeds 256 rules.",
    );
    for (pattern, alias, message) in [
        ("A".repeat(1025), "0".into(), "Pattern exceeds 1024 bytes."),
        (".*".into(), "x".repeat(4097), "Alias exceeds 4096 bytes."),
    ] {
        error(json!({"version":1,"conversion":"none","rules":[rule(1,0,"any","product",&pattern,&alias)]}).to_string(),"limit",message);
    }
    let rules: Vec<Value> = (1..=9)
        .map(|id| rule(id, 0, "any", "product", &"A".repeat(1024), "0"))
        .collect();
    error(
        json!({"version":1,"conversion":"none","rules":rules}).to_string(),
        "limit",
        "Rule patterns exceed 8192 bytes.",
    );
    error(json!({"version":1,"conversion":"none","rules":[rule(1,0,"any","product",&format!("{}A{}","(".repeat(40),")".repeat(40)),"0")]}).to_string(),"pattern","Pattern is not supported by the bounded regex engine.");
}
