// SPDX-License-Identifier: MIT OR Apache-2.0
//! Format-1 vectors independent of the Rust implementation. No database assurance is claimed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_orm::audit::seal::{
    BatchDigest, CanonicalValue as V, CodecError, RowDigest, SealCursor, SealIdentity, SealInput,
};
use serde_json::Value;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn bytes(value: &Value) -> Vec<u8> {
    let s = value.as_str().unwrap();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn value(v: &Value) -> V {
    match v["tag"].as_str().unwrap() {
        "missing" => V::missing(),
        "null" => V::null(),
        "bool" => V::boolean(v["value"].as_bool().unwrap()),
        "int" => V::integer(v["value"].as_str().unwrap().parse().unwrap()),
        "xid" => V::xid(v["value"].as_str().unwrap().parse().unwrap()),
        "numeric" => V::numeric(v["value"].as_str().unwrap()).unwrap(),
        "text" => V::text(v["value"].as_str().unwrap()).unwrap(),
        "timestamp" => V::timestamp_micros(v["value"].as_str().unwrap().parse().unwrap()),
        "uuid" => V::uuid(bytes(&v["value"]).try_into().unwrap()),
        "bytes" => V::bytes(&bytes(&v["value"])).unwrap(),
        "array" => V::array(
            &v["value"]
                .as_array()
                .unwrap()
                .iter()
                .map(value)
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        "object" => V::object(
            &v["value"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p[0].as_str().unwrap(), value(&p[1])))
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        other => panic!("unknown fixture tag {other}"),
    }
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/audit-seal-v1.json")).unwrap()
}
fn rows(f: &Value) -> Vec<RowDigest> {
    f["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let columns: Vec<_> = r["columns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p[0].as_str().unwrap(), value(&p[1])))
                .collect();
            let digest = RowDigest::schema1(&columns).unwrap();
            assert_eq!(hex(digest.as_bytes()), r["sha256"].as_str().unwrap());
            digest
        })
        .collect()
}

#[test]
fn independent_format_one_vectors() {
    let f = fixture();
    for case in f["values"].as_array().unwrap() {
        assert_eq!(
            hex(value(&case["input"]).as_bytes()),
            case["hex"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
    let rows = rows(&f);
    let batch = BatchDigest::new(&rows).unwrap();
    assert_eq!(hex(batch.as_bytes()), f["batch_sha256"].as_str().unwrap());
    for seal in f["seals"].as_array().unwrap() {
        let cursor = |v: &Value| {
            SealCursor::new(
                v[0].as_str().unwrap().parse().unwrap(),
                v[1].as_i64().unwrap(),
            )
            .unwrap()
        };
        let identity = SealIdentity::new(
            bytes(&seal["incarnation"]).try_into().unwrap(),
            seal["epoch"].as_u64().unwrap(),
            seal["batch_no"].as_u64().unwrap(),
        )
        .unwrap();
        let input = SealInput::new(
            identity,
            cursor(&seal["lower"]),
            cursor(&seal["upper"]),
            batch,
            bytes(&seal["predecessor"]).try_into().unwrap(),
        )
        .unwrap();
        assert_eq!(hex(&input.hash()), seal["sha256"].as_str().unwrap());
    }
}

#[test]
fn decimal_equivalence_and_precision() {
    for inputs in [
        ["0", "-0.000e-100", "0e100"],
        ["123", "00123.000", "1.2300e2"],
        ["-0.00123", "-123e-5", "-0.00123000"],
    ] {
        let expected = V::numeric(inputs[0]).unwrap();
        for input in inputs {
            assert_eq!(V::numeric(input).unwrap(), expected);
        }
    }
    assert_ne!(
        V::numeric("9007199254740992.1").unwrap(),
        V::numeric("9007199254740992.2").unwrap()
    );
    assert_ne!(V::numeric("1e-400").unwrap(), V::numeric("0").unwrap());
    assert_eq!(
        hex(V::numeric("1e-4").unwrap().as_bytes()),
        "0400000006302e30303031"
    );
    assert_ne!(V::integer(1), V::numeric("1").unwrap());
    // Admission applies to canonical output, not fractional zeros discarded by normalization.
    assert_eq!(
        V::numeric("1.000e-4093").unwrap(),
        V::numeric("1e-4093").unwrap()
    );
}

fn refusal<T: std::fmt::Debug>(result: Result<T, CodecError>, expected: CodecError, message: &str) {
    let error = result.unwrap_err();
    assert_eq!(error, expected);
    assert_eq!(error.to_string(), message);
}

#[test]
fn rejects_invalid_numbers_and_expansion() {
    for input in [
        "",
        "+1",
        "NaN",
        "Infinity",
        "1.",
        ".1",
        "1e",
        "1e+",
        "1 2",
        "1e1.1",
        "１",
        "secret-canary",
    ] {
        refusal(
            V::numeric(input),
            CodecError::InvalidNumber,
            "audit seal codec: invalid decimal number",
        );
    }
    for input in ["1e9999999999999999999999999999", "1e4096", "1e-4096"] {
        refusal(
            V::numeric(input),
            CodecError::Limit("numeric bytes"),
            "audit seal codec: numeric bytes limit exceeded",
        );
    }
}

#[test]
fn structural_boundaries_and_duplicates() {
    let one = V::object(&[("é", V::integer(1)), ("a", V::integer(2))]).unwrap();
    assert_eq!(
        one,
        V::object(&[("a", V::integer(2)), ("é", V::integer(1))]).unwrap()
    );
    refusal(
        V::object(&[
            ("secret-canary", V::null()),
            ("secret-canary", V::missing()),
        ]),
        CodecError::DuplicateName,
        "audit seal codec: duplicate name",
    );
    assert_ne!(V::null(), V::missing());
    assert_ne!(V::text("é").unwrap(), V::text("e\u{301}").unwrap());
    let mut nested = V::null();
    for _ in 0..16 {
        nested = V::array(&[nested]).unwrap();
    }
    refusal(
        V::array(&[nested]),
        CodecError::Limit("depth"),
        "audit seal codec: depth limit exceeded",
    );
    refusal(
        V::bytes(&vec![0; 8 * 1024 * 1024]),
        CodecError::Limit("encoded bytes"),
        "audit seal codec: encoded bytes limit exceeded",
    );
    refusal(
        V::array(&vec![V::null(); 65_537]),
        CodecError::Limit("items"),
        "audit seal codec: items limit exceeded",
    );
}

#[test]
fn persisted_column_coverage_and_order() {
    let f = fixture();
    let mut columns: Vec<_> = f["rows"][0]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p[0].as_str().unwrap(), value(&p[1])))
        .collect();
    let expected = RowDigest::schema1(&columns).unwrap();
    columns.reverse();
    assert_eq!(expected, RowDigest::schema1(&columns).unwrap());
    for i in 0..columns.len() {
        let mut altered = columns.clone();
        altered[i].1 = V::text("changed").unwrap();
        assert_ne!(
            expected,
            RowDigest::schema1(&altered).unwrap(),
            "{}",
            columns[i].0
        );
    }
    let mut missing = columns.clone();
    missing.pop();
    refusal(
        RowDigest::schema1(&missing),
        CodecError::ColumnSet,
        "audit seal codec: columns do not match audit schema 1",
    );
    let mut unknown = columns.clone();
    unknown[0].0 = "unknown";
    refusal(
        RowDigest::schema1(&unknown),
        CodecError::ColumnSet,
        "audit seal codec: columns do not match audit schema 1",
    );
    columns[0].0 = columns[1].0;
    refusal(
        RowDigest::schema1(&columns),
        CodecError::DuplicateName,
        "audit seal codec: duplicate name",
    );
}

#[test]
fn batch_order_and_seal_boundaries() {
    let r = rows(&fixture());
    let batch = BatchDigest::new(&r).unwrap();
    let reversed = BatchDigest::new(&r.iter().copied().rev().collect::<Vec<_>>()).unwrap();
    assert_ne!(batch, reversed);
    assert_ne!(batch, BatchDigest::new(&r[..1]).unwrap());
    refusal(
        BatchDigest::new(&[]),
        CodecError::EmptyBatch,
        "audit seal codec: empty batch",
    );
    refusal(
        SealCursor::new(1, -1),
        CodecError::InvalidCursor,
        "audit seal codec: invalid cursor",
    );
    for (epoch, number) in [(0, 1), (1, 0), (u64::MAX, 1), (1, u64::MAX)] {
        refusal(
            SealIdentity::new([1; 16], epoch, number),
            CodecError::InvalidIdentity,
            "audit seal codec: invalid seal identity",
        );
    }
    refusal(
        SealIdentity::new([0; 16], 1, 1),
        CodecError::InvalidIdentity,
        "audit seal codec: invalid seal identity",
    );
    let identity = SealIdentity::new([1; 16], 1, 1).unwrap();
    let lower = SealCursor::new(0, 0).unwrap();
    let upper = SealCursor::new(2, 1).unwrap();
    refusal(
        SealInput::new(identity, upper, lower, batch, [0; 32]),
        CodecError::InvalidCursor,
        "audit seal codec: invalid cursor",
    );
    refusal(
        SealInput::new(identity, lower, upper, batch, [1; 32]),
        CodecError::InvalidPredecessor,
        "audit seal codec: invalid predecessor hash",
    );
    let later = SealIdentity::new([1; 16], 2, 1).unwrap();
    refusal(
        SealInput::new(later, lower, upper, batch, [0; 32]),
        CodecError::InvalidPredecessor,
        "audit seal codec: invalid predecessor hash",
    );
    let hash = SealInput::new(identity, lower, upper, batch, [0; 32])
        .unwrap()
        .hash();
    assert_ne!(
        hash,
        SealInput::new(
            SealIdentity::new([2; 16], 1, 1).unwrap(),
            lower,
            upper,
            batch,
            [0; 32]
        )
        .unwrap()
        .hash()
    );
    assert_ne!(
        hash,
        SealInput::new(
            identity,
            lower,
            SealCursor::new(3, 1).unwrap(),
            batch,
            [0; 32]
        )
        .unwrap()
        .hash()
    );
    assert_ne!(
        hash,
        SealInput::new(identity, lower, upper, reversed, [0; 32])
            .unwrap()
            .hash()
    );
}

#[test]
fn exact_resource_bounds_and_safe_diagnostics() {
    let large = V::bytes(&vec![0; 8 * 1024 * 1024 - 5]).unwrap();
    assert_eq!(large.as_bytes().len(), 8 * 1024 * 1024);
    refusal(
        V::array(std::slice::from_ref(&large)),
        CodecError::Limit("encoded bytes"),
        "audit seal codec: encoded bytes limit exceeded",
    );
    refusal(
        V::object(&[("a", large)]),
        CodecError::Limit("encoded bytes"),
        "audit seal codec: encoded bytes limit exceeded",
    );
    assert_eq!(
        V::numeric(&"9".repeat(4096)).unwrap().as_bytes().len(),
        4101
    );
    refusal(
        V::numeric(&"9".repeat(4097)),
        CodecError::Limit("numeric bytes"),
        "audit seal codec: numeric bytes limit exceeded",
    );
    assert!(V::array(&vec![V::null(); 65_536]).is_ok());
    let digest = rows(&fixture())[0];
    assert_eq!(
        BatchDigest::new(&vec![digest; 65_536]).unwrap().row_count(),
        65_536
    );
    refusal(
        BatchDigest::new(&vec![digest; 65_537]),
        CodecError::Limit("items"),
        "audit seal codec: items limit exceeded",
    );
    let sensitive = V::object(&[(
        "private-column-canary",
        V::text("private-value-canary").unwrap(),
    )])
    .unwrap();
    let debug = format!("{sensitive:?}");
    assert!(!debug.contains("canary"));
    assert_eq!(debug, "CanonicalValue { encoded_bytes: 55, depth: 1, .. }");
    for (xid, id) in [(0, 1), (1, 0), (0, -1)] {
        refusal(
            SealCursor::new(xid, id),
            CodecError::InvalidCursor,
            "audit seal codec: invalid cursor",
        );
    }
    assert!(SealCursor::new(u64::MAX, i64::MAX).is_ok());
}
