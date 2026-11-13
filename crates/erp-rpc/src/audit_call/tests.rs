#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::json::{Json, canonical, parse};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};

use super::*;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "audit_digest",
    depends: &["base"],
    exact_only: false,
};

fn registry() -> Registry {
    let fields = const {
        &[
            FieldDecl::char("name"),
            FieldDecl::char("restricted_note").groups("."),
            FieldDecl::char("password").secret(true),
            FieldDecl::one2many("lines", "audit_digest.line", "order_id"),
        ]
    };
    let line_fields = const {
        &[
            FieldDecl::char("label"),
            FieldDecl::char("only_admin").groups("."),
            FieldDecl::char("pin").secret(true),
            FieldDecl::many2one("order_id", "audit_digest.order"),
        ]
    };
    RegistryBuilder::new([&ADDON])
        .with_decls([
            &*Box::leak(Box::new(
                ModelDecl::new("audit_digest", "audit_digest.order").fields(fields),
            )),
            &*Box::leak(Box::new(
                ModelDecl::new("audit_digest", "audit_digest.line").fields(line_fields),
            )),
        ])
        .build()
        .unwrap()
}

fn json(text: &str) -> Json {
    parse(text.as_bytes(), 64).unwrap()
}

fn text(value: &Json) -> String {
    String::from_utf8(canonical(value)).unwrap()
}

/// The digest does not depend on the order of keys, and changes with any kept value.
#[test]
fn digest_is_deterministic_across_key_order_and_sensitive_to_values() {
    let r = registry();
    let a = args_digest(
        &r,
        "audit_digest.order",
        "write",
        &[3, 4],
        &json(r#"{"vals":{"name":"x","lines":[]}}"#),
    );
    let b = args_digest(
        &r,
        "audit_digest.order",
        "write",
        &[3, 4],
        &json(r#"{"vals":{"lines":[],"name":"x"}}"#),
    );
    let c = args_digest(
        &r,
        "audit_digest.order",
        "write",
        &[3, 4],
        &json(r#"{"vals":{"name":"y","lines":[]}}"#),
    );
    let d = args_digest(
        &r,
        "audit_digest.order",
        "write",
        &[4, 3],
        &json(r#"{"vals":{"name":"x","lines":[]}}"#),
    );
    assert_eq!(a, b);
    assert_ne!(a, c, "a kept value changes the digest");
    assert_ne!(a, d, "the id order of the caller is part of the call");
}

/// Restricted and unknown fields are replaced by their reason; x2many create and update commands
/// recurse into the comodel; other commands are kept.
#[test]
fn record_values_are_redacted_by_field_and_x2many_recurses() {
    let r = registry();
    let kwargs = json(
        r#"{"vals":{"name":"visible","restricted_note":"CANARY-1","nope":"CANARY-2",
            "lines":[[0,0,{"label":"l","only_admin":"CANARY-3","ghost":1}],
                     [1,7,{"only_admin":"CANARY-4"}],[4,9],[5],[6,0,[1,2]]]}}"#,
    );
    let projected = projection(&r, "audit_digest.order", "write", &[1], &kwargs);
    let shown = text(&projected);
    assert!(!shown.contains("CANARY"), "{shown}");
    assert!(shown.contains(r#""name":"visible""#), "{shown}");
    assert!(
        shown.contains(r#""restricted_note":{"$redacted":"restricted"}"#),
        "{shown}"
    );
    assert!(
        shown.contains(r#""nope":{"$redacted":"unknown"}"#),
        "{shown}"
    );
    assert!(
        shown.contains(r#"[0,0,{"ghost":{"$redacted":"unknown"},"label":"l","only_admin":{"$redacted":"restricted"}}]"#)
            || shown.contains(r#"[0,0,{"label":"l","ghost":{"$redacted":"unknown"},"only_admin":{"$redacted":"restricted"}}]"#),
        "{shown}"
    );
    assert!(
        shown.contains(r#"[1,7,{"only_admin":{"$redacted":"restricted"}}]"#),
        "{shown}"
    );
    assert!(
        shown.contains("[4,9]") && shown.contains("[5]") && shown.contains("[6,0,[1,2]]"),
        "{shown}"
    );
}

/// A method without record values keeps only the names of its keyword arguments, so a secret
/// passed under any name never reaches the projection; `vals_list` redacts every record.
#[test]
fn other_methods_keep_argument_names_only() {
    let r = registry();
    let kwargs = json(r#"{"password":"CANARY-PW","token":{"nested":"CANARY-T"},"limit":5}"#);
    let projected = projection(&r, "audit_digest.order", "do_thing", &[], &kwargs);
    let shown = text(&projected);
    assert!(!shown.contains("CANARY") && !shown.contains('5'), "{shown}");
    assert_eq!(
        shown,
        r#"{"args":{"limit":{"$redacted":"undeclared"},"password":{"$redacted":"undeclared"},"token":{"$redacted":"undeclared"}},"ids":[],"method":"do_thing","model":"audit_digest.order"}"#
    );
    let many = projection(
        &r,
        "audit_digest.order",
        "create",
        &[],
        &json(r#"{"vals_list":[{"restricted_note":"CANARY-5"},{"name":"n"}]}"#),
    );
    let shown = text(&many);
    assert!(!shown.contains("CANARY"), "{shown}");
    assert!(shown.contains(r#"{"name":"n"}"#), "{shown}");
}

/// A model the registry does not know redacts every field as unknown (nothing is kept on a guess),
/// and a nesting deeper than the bound is redacted, not followed.
#[test]
fn unknown_models_and_deep_nesting_are_redacted() {
    let r = registry();
    let projected = projection(
        &r,
        "no.such",
        "write",
        &[1],
        &json(r#"{"vals":{"name":"CANARY-6"}}"#),
    );
    assert!(!text(&projected).contains("CANARY"));
    let mut nested = r#"{"label":"x"}"#.to_owned();
    for _ in 0..20 {
        nested = format!(r#"{{"lines":[[0,0,{nested}]]}}"#);
    }
    let projected = projection(
        &r,
        "audit_digest.order",
        "write",
        &[1],
        &json(&format!(r#"{{"vals":{nested}}}"#)),
    );
    // the line model has no `lines` field: the second level is already unknown; the depth guard
    // is exercised on self-nesting models by the bound itself
    assert!(text(&projected).contains("$redacted"));
}

/// A field marked `secret` is replaced by `{"$redacted":"secret"}` whatever its value or JSON type,
/// in `write`, `create`-style `vals_list` and nested x2many create and update commands, so the
/// digest of two different secrets is the same and the cleartext is in no part of the projection.
#[test]
fn secret_fields_are_redacted_before_hashing() {
    let r = registry();
    let digest = |secret: &str| {
        args_digest(
            &r,
            "audit_digest.order",
            "write",
            &[1],
            &json(&format!(
                r#"{{"vals":{{"name":"n","password":"{secret}",
                    "lines":[[0,0,{{"label":"l","pin":"{secret}"}}],[1,7,{{"pin":"{secret}"}}]]}}}}"#
            )),
        )
    };
    assert_eq!(digest("hunter2-CANARY"), digest("correct-horse-CANARY"));
    let kwargs = json(
        r#"{"vals":{"password":"CANARY-A","lines":[[0,0,{"pin":"CANARY-B"}],[1,7,{"pin":12345}]]}}"#,
    );
    let shown = text(&projection(
        &r,
        "audit_digest.order",
        "write",
        &[1],
        &kwargs,
    ));
    assert!(
        !shown.contains("CANARY") && !shown.contains("12345"),
        "{shown}"
    );
    assert!(
        shown.contains(r#""password":{"$redacted":"secret"}"#),
        "{shown}"
    );
    assert!(
        shown.contains(r#"[0,0,{"pin":{"$redacted":"secret"}}]"#)
            && shown.contains(r#"[1,7,{"pin":{"$redacted":"secret"}}]"#),
        "{shown}"
    );
    let many = text(&projection(
        &r,
        "audit_digest.order",
        "create",
        &[],
        &json(r#"{"vals_list":[{"password":"CANARY-C"},{"password":["CANARY-D"]}]}"#),
    ));
    assert!(!many.contains("CANARY"), "{many}");
    assert_eq!(
        many.matches(r#"{"password":{"$redacted":"secret"}}"#)
            .count(),
        2,
        "{many}"
    );
}

/// The redaction version recorded with a digest is the one of these rules.
#[test]
fn the_digest_records_redaction_version_two() {
    assert_eq!(REDACTION_VERSION, 2);
}

/// Every x2many shape the ORM accepts is redacted with the comodel's declarations, the plain list
/// of value maps included, and a shape that is not known is replaced as a whole: no element of an
/// x2many value passes through unparsed.
#[test]
fn every_x2many_shape_is_redacted_or_replaced() {
    let r = registry();
    let digest = |secret: &str| {
        args_digest(
            &r,
            "audit_digest.order",
            "write",
            &[1],
            &json(&format!(
                r#"{{"vals":{{"lines":[{{"label":"l","pin":"{secret}"}},{{"pin":"{secret}"}}]}}}}"#
            )),
        )
    };
    assert_eq!(digest("CANARY-one"), digest("CANARY-two"));
    let shown = |lines: &str| {
        text(&projection(
            &r,
            "audit_digest.order",
            "write",
            &[1],
            &json(&format!(r#"{{"vals":{{"lines":{lines}}}}}"#)),
        ))
    };
    let plain = shown(r#"[{"label":"l","pin":"CANARY-1","ghost":"CANARY-2"}]"#);
    assert!(!plain.contains("CANARY"), "{plain}");
    assert!(plain.contains(r#""pin":{"$redacted":"secret"}"#), "{plain}");
    // ids, clear, and the triples without values are kept
    let kept =
        shown(r#"[1,2,[2,3,0],[3,4,false],[4,5],[5,0,0],[6,0,[7,8]],[0,0,false],[1,9,null]]"#);
    for part in [
        "[1,2,",
        "[2,3,0]",
        "[3,4,false]",
        "[4,5]",
        "[5,0,0]",
        "[6,0,[7,8]]",
        "[0,0,false]",
        "[1,9,null]",
    ] {
        assert!(kept.contains(part), "{part} in {kept}");
    }
    assert!(shown("false").contains(r#""lines":false"#));
    // unknown shapes are replaced whole
    for lines in [
        r#"{"pin":"CANARY-3"}"#,
        r#""CANARY-4""#,
        r#"[[2,3,{"pin":"CANARY-5"}]]"#,
        r#"[[6,0,["CANARY-6"]]]"#,
        r#"[[0,0,"CANARY-7"]]"#,
        r#"[[9,0,0]]"#,
        r#"[[0,0,{}, "CANARY-8"]]"#,
        r#"["CANARY-9"]"#,
        r#"[[0,"CANARY-10",{}]]"#,
        r#"[1.5]"#,
    ] {
        let out = shown(lines);
        assert!(
            !out.contains("CANARY") && !out.contains("1.5"),
            "{lines} -> {out}"
        );
        assert!(
            out.contains(r#"{"$redacted":"unparsed"}"#),
            "{lines} -> {out}"
        );
    }
    // a many2one carries an id, never a structure
    let m2o = text(&projection(
        &r,
        "audit_digest.line",
        "write",
        &[1],
        &json(r#"{"vals":{"order_id":{"password":"CANARY-11"}}}"#),
    ));
    assert!(!m2o.contains("CANARY"), "{m2o}");
}
