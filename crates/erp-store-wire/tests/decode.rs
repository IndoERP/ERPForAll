//! Strict decoder tests: the Rust half of the decode contract. The refusal vocabulary is owned by
//! the TypeScript cases 017 to 024; here the decoder is checked for the header half of
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{assert_err, cursor_104, schema, writing};
use erp_store_wire::{
    AnswerBuilder, Decoder, FieldValue, FrameBuilder, Limit, Outcome, RecordRef, SchemaGeneration,
    StoreSchema, WireLimits,
};

const ASOF_READ: &str = r#"{"own":null,"xip":["100","103"],"xmax":"104","xmin":"100"}"#;
const ASOF_WRITE: &str = r#"{"own":"104","xip":["100","103"],"xmax":"104","xmin":"100"}"#;
const T: &str = "test.thread";

/// (model, id, [(field, value, written)]) as read back from a decoded record.
type Upsert = (String, u64, Vec<(String, FieldValue, bool)>);

fn decoder() -> Decoder {
    Decoder::new(WireLimits::default()).with_schema(schema())
}

fn answer(records: &str, extra: &str) -> String {
    format!(r#"{{"asof":{ASOF_READ},"records":{records},"schema":"g1","wire":1{extra}}}"#)
}

fn frame(records: &str) -> String {
    format!(
        r#"{{"asof":{ASOF_WRITE},"cursor":{{"id":"5512","xid":"104"}},"records":{records},"schema":"g1","wire":1}}"#
    )
}

fn decoded(outcome: Outcome<erp_store_wire::DecodedPayload>) -> erp_store_wire::DecodedPayload {
    match outcome {
        Outcome::Decoded(payload) => payload,
        other => panic!("expected a decoded payload, got {other:?}"),
    }
}

#[test]
fn decoder_refuses_a_payload_missing_the_header() {
    let d = decoder();
    let no_wire =
        r#"{"asof":{"own":null,"xip":[],"xmax":"2","xmin":"1"},"records":{},"schema":"g1"}"#;
    assert_err(
        d.decode_answer(no_wire.as_bytes()),
        "malformed",
        Some("wire"),
        "wire: malformed payload: missing member",
    );
    let no_schema = r#"{"asof":{"own":null,"xip":[],"xmax":"2","xmin":"1"},"records":{},"wire":1}"#;
    assert_err(
        d.decode_answer(no_schema.as_bytes()),
        "malformed",
        Some("schema"),
        "schema: malformed payload: missing member",
    );
    let text_wire = answer("{}", "").replace(r#""wire":1"#, r#""wire":"1""#);
    assert_err(
        d.decode_answer(text_wire.as_bytes()),
        "malformed",
        Some("wire"),
        "wire: malformed payload: expected an integer",
    );
    // A foreign major is refused before any content error.
    let major2 = r#"{"wire":2,"records":42}"#;
    let err = d.decode_answer(major2.as_bytes()).expect_err("refused");
    assert_eq!((err.code(), err.path()), ("wire_major_unsupported", None));
    assert_eq!(err.to_string(), "unsupported wire major 2");
}

#[test]
fn needs_schema_until_the_generation_is_known() {
    let bytes = answer(r#"{"test.thread":[{"id":7,"seq":1}]}"#, "");
    let none = Decoder::new(WireLimits::default());
    let generation = SchemaGeneration::new("g1").expect("token");
    assert_eq!(
        none.decode_answer(bytes.as_bytes()).expect("not an error"),
        Outcome::NeedsSchema {
            generation: generation.clone()
        }
    );
    // A held schema of another generation yields no operations either.
    let mut other = StoreSchema::builder(SchemaGeneration::new("g0").expect("token"));
    other
        .field(
            T,
            "seq",
            erp_store_wire::FieldSpec::new(erp_store_wire::FieldKind::Int),
        )
        .expect("ok");
    let mut held = Decoder::new(WireLimits::default()).with_schema(other.build().expect("ok"));
    assert_eq!(
        held.decode_answer(bytes.as_bytes()).expect("not an error"),
        Outcome::NeedsSchema { generation }
    );
    // After the g1 schema is supplied the same bytes decode.
    held.set_schema(schema());
    let payload = decoded(held.decode_answer(bytes.as_bytes()).expect("decodes"));
    assert_eq!(payload.upserts().len(), 1);
}

#[test]
fn answer_round_trip_is_canonical_and_deletes_come_last() {
    let schema = schema();
    let mut b = AnswerBuilder::new(&schema, writing());
    b.set(T, 9, "title", FieldValue::String("nine".to_owned()))
        .expect("set");
    b.set(T, 7, "messages", FieldValue::X2ManyReplace(vec![5, 1]))
        .expect("set");
    b.set(
        T,
        7,
        "owner",
        FieldValue::Reference(Some(RecordRef::new("test.partner", 3).expect("ref"))),
    )
    .expect("set");
    b.set(
        "test.message",
        5,
        "text",
        FieldValue::String("hi".to_owned()),
    )
    .expect("set");
    b.delete("test.message", 2).expect("delete");
    b.delete(T, 8).expect("delete");
    b.mark_written(T, 7, "messages").expect("marker");
    let bytes = b.encode().expect("encodes");
    let payload = decoded(decoder().decode_answer(&bytes).expect("decodes"));
    assert_eq!(payload.generation().as_str(), "g1");
    assert_eq!(payload.asof(), &writing());
    assert_eq!(payload.cursor(), None);
    let upserts: Vec<Upsert> = payload
        .upserts()
        .iter()
        .map(|r| {
            (
                r.model().as_str().to_owned(),
                r.id(),
                r.fields()
                    .iter()
                    .map(|f| (f.name().as_str().to_owned(), f.value().clone(), f.written()))
                    .collect(),
            )
        })
        .collect();
    let thread = "test.thread".to_owned();
    assert_eq!(
        upserts,
        vec![
            (
                "test.message".to_owned(),
                5,
                vec![(
                    "text".to_owned(),
                    FieldValue::String("hi".to_owned()),
                    false
                )]
            ),
            (
                thread.clone(),
                7,
                vec![
                    (
                        "messages".to_owned(),
                        FieldValue::X2ManyReplace(vec![1, 5]),
                        true
                    ),
                    (
                        "owner".to_owned(),
                        FieldValue::Reference(Some(
                            RecordRef::new("test.partner", 3).expect("ref")
                        )),
                        false
                    ),
                ]
            ),
            (
                thread.clone(),
                9,
                vec![(
                    "title".to_owned(),
                    FieldValue::String("nine".to_owned()),
                    false
                )]
            ),
        ]
    );
    let deletes: Vec<(String, u64)> = payload
        .deletes()
        .iter()
        .map(|r| (r.model().as_str().to_owned(), r.id()))
        .collect();
    assert_eq!(deletes, vec![("test.message".to_owned(), 2), (thread, 8)]);
}

#[test]
fn frame_round_trip_keeps_cursor_and_deltas() {
    let schema = schema();
    let mut b = FrameBuilder::new(&schema, cursor_104(), writing()).expect("frame");
    b.set(T, 7, "unread", FieldValue::Inc(2)).expect("set");
    b.set(
        T,
        7,
        "messages",
        FieldValue::X2ManyDelta {
            add: vec![3],
            remove: vec![1],
        },
    )
    .expect("set");
    let payload = decoded(
        decoder()
            .decode_frame(&b.encode().expect("encodes"))
            .expect("decodes"),
    );
    assert_eq!(payload.cursor(), Some(&cursor_104()));
    let fields: Vec<FieldValue> = payload.upserts()[0]
        .fields()
        .iter()
        .map(|f| f.value().clone())
        .collect();
    assert_eq!(
        fields,
        vec![
            FieldValue::X2ManyDelta {
                add: vec![3],
                remove: vec![1]
            },
            FieldValue::Inc(2)
        ]
    );
    // A frame is not an answer and an answer is not a frame.
    assert_eq!(
        decoder()
            .decode_answer(&b.encode().expect("encodes"))
            .expect_err("refused")
            .code(),
        "malformed"
    );
    let plain = answer("{}", "");
    assert_eq!(
        decoder()
            .decode_frame(plain.as_bytes())
            .expect_err("refused")
            .code(),
        "malformed"
    );
    // A frame whose descriptor own differs from its cursor is refused.
    let bad = frame("{}").replace(r#""own":"104""#, r#""own":"105""#);
    assert_err(
        decoder().decode_frame(bad.as_bytes()),
        "bad_descriptor",
        Some("asof.own"),
        "asof.own: bad descriptor: own xid must equal the cursor xid",
    );
}

#[test]
fn content_refusals_name_code_and_path() {
    let d = decoder();
    let rec = |entry: &str| answer(&format!(r#"{{"test.thread":[{entry}]}}"#), "");
    let frec = |entry: &str| frame(&format!(r#"{{"test.thread":[{entry}]}}"#));
    let at = |field: &str| format!("records.test.thread[0].{field}");
    let cases: Vec<(String, bool, &str, String, String)> = vec![
        (
            answer(r#"{"nowhere":[]}"#, ""),
            false,
            "unknown_model",
            "records.nowhere".into(),
            "records.nowhere: unknown model nowhere".into(),
        ),
        (
            rec(r#"{"id":7,"nope":1}"#),
            false,
            "unknown_field",
            at("nope"),
            format!("{}: unknown field nope", at("nope")),
        ),
        (
            rec(r#"{"id":7,"seq":{"foo":1}}"#),
            false,
            "unknown_operation",
            at("seq"),
            format!("{}: unknown operation foo", at("seq")),
        ),
        (
            frec(r#"{"id":7,"seq":{"inc":1}}"#),
            true,
            "operation_not_allowed",
            at("seq"),
            format!(
                "{}: operation not allowed: inc is only allowed on a counter field",
                at("seq")
            ),
        ),
        (
            frec(r#"{"id":7,"messages":[1]}"#),
            true,
            "operation_not_allowed",
            at("messages"),
            format!(
                "{}: operation not allowed: x2many replace is not allowed in a frame",
                at("messages")
            ),
        ),
        (
            frec(r#"{"id":7,"stats":"x"}"#),
            true,
            "operation_not_allowed",
            at("stats"),
            format!(
                "{}: operation not allowed: answer_only field is not allowed in a frame",
                at("stats")
            ),
        ),
        (
            rec(r#"{"id":7,"unread":{"inc":1}}"#),
            false,
            "operation_not_allowed",
            at("unread"),
            format!(
                "{}: operation not allowed: inc is only allowed in a bus frame",
                at("unread")
            ),
        ),
        (
            rec(r#"{"id":7,"seq":"x"}"#),
            false,
            "type_mismatch",
            at("seq"),
            format!("{}: expected int", at("seq")),
        ),
        (
            rec(r#"{"id":7,"seq":1.5}"#),
            false,
            "type_mismatch",
            at("seq"),
            format!("{}: expected int", at("seq")),
        ),
        (
            rec(r#"{"id":7,"delete":true,"seq":1}"#),
            false,
            "delete_with_fields",
            "records.test.thread[0]".into(),
            "records.test.thread[0]: delete marker together with fields".into(),
        ),
        (
            rec(r#"{"id":0}"#),
            false,
            "bad_id",
            "records.test.thread[0].id".into(),
            "records.test.thread[0].id: bad record id 0".into(),
        ),
        (
            rec(r#"{"seq":1}"#),
            false,
            "malformed",
            "records.test.thread[0].id".into(),
            "records.test.thread[0].id: malformed payload: missing member".into(),
        ),
        (
            rec(r#"{"id":1,"seq":1},{"id":1,"seq":2}"#),
            false,
            "duplicate_id",
            "records.test.thread[1].id".into(),
            "records.test.thread[1].id: duplicate id 1".into(),
        ),
    ];
    for (bytes, is_frame, code, path, message) in cases {
        let result = if is_frame {
            d.decode_frame(bytes.as_bytes())
        } else {
            d.decode_answer(bytes.as_bytes())
        };
        assert_err(result, code, Some(&path), &message);
    }
    // No partial application: one bad record yields an error, not the good ones.
    let mixed = answer(
        r#"{"test.message":[{"id":1,"text":"ok"}],"test.thread":[{"id":7,"nope":1}]}"#,
        "",
    );
    assert_eq!(
        d.decode_answer(mixed.as_bytes())
            .expect_err("refused")
            .code(),
        "unknown_field"
    );
    // Duplicate keys in one object are refused, never last-one-wins.
    let dup = rec(r#"{"id":7,"seq":1,"seq":2}"#);
    assert_eq!(
        d.decode_answer(dup.as_bytes()).expect_err("refused").code(),
        "malformed"
    );
}

#[test]
fn written_markers_are_validated() {
    let d = decoder();
    let records = r#"{"test.thread":[{"id":7,"seq":1,"unread":3}]}"#;
    let with = |own: bool, written: &str| {
        let asof = if own { ASOF_WRITE } else { ASOF_READ };
        format!(
            r#"{{"asof":{asof},"records":{records},"schema":"g1","wire":1,"written":{written}}}"#
        )
    };
    let cases: [(bool, &str, &str, &str); 5] = [
        (
            false,
            r#"{"test.thread":{"7":["seq"]}}"#,
            "written",
            "answer has no own xid",
        ),
        (
            true,
            r#"{"test.message":{"1":["text"]}}"#,
            "written.test.message",
            "model is not in the payload",
        ),
        (
            true,
            r#"{"test.thread":{"8":["seq"]}}"#,
            "written.test.thread.8",
            "record is not in the payload",
        ),
        (
            true,
            r#"{"test.thread":{"7":["title"]}}"#,
            "written.test.thread.7[0]",
            "field is not set by the entry",
        ),
        (
            true,
            r#"{"test.thread":{"7":["unread"]}}"#,
            "written.test.thread.7[0]",
            "counter fields cannot be written markers",
        ),
    ];
    for (own, written, path, reason) in cases {
        assert_err(
            d.decode_answer(with(own, written).as_bytes()),
            "bad_written",
            Some(path),
            &format!("{path}: bad written marker: {reason}"),
        );
    }
    let ok = decoded(
        d.decode_answer(with(true, r#"{"test.thread":{"7":["seq"]}}"#).as_bytes())
            .expect("valid"),
    );
    let flags: Vec<(String, bool)> = ok.upserts()[0]
        .fields()
        .iter()
        .map(|f| (f.name().as_str().to_owned(), f.written()))
        .collect();
    assert_eq!(
        flags,
        vec![("seq".to_owned(), true), ("unread".to_owned(), false)]
    );
}

#[test]
fn limits_refuse_by_name() {
    let low = |limit, value| {
        Decoder::new(WireLimits::default().lowered(limit, value).expect("lower"))
            .with_schema(schema())
    };
    let two_records = answer(r#"{"test.thread":[{"id":1,"seq":1},{"id":2,"seq":1}]}"#, "");
    let cases: Vec<(Decoder, String, &str, &str, u64)> = vec![
        (
            low(Limit::Bytes, 20),
            two_records.clone(),
            "limit_exceeded",
            "",
            20,
        ),
        (
            low(Limit::Records, 1),
            two_records.clone(),
            "limit_exceeded",
            "records",
            1,
        ),
        (
            low(Limit::Models, 1),
            answer(r#"{"test.thread":[],"test.message":[]}"#, ""),
            "limit_exceeded",
            "records",
            1,
        ),
        (
            low(Limit::IdsPerList, 1),
            answer(r#"{"test.thread":[{"id":1,"messages":[1,2]}]}"#, ""),
            "limit_exceeded",
            "records.test.thread[0].messages",
            1,
        ),
        (
            low(Limit::Xip, 1),
            answer("{}", ""),
            "limit_exceeded",
            "asof.xip",
            1,
        ),
        (
            low(Limit::StringBytes, 2),
            answer(r#"{"test.thread":[{"id":1,"title":"abc"}]}"#, ""),
            "limit_exceeded",
            "records.test.thread[0].title",
            2,
        ),
        (
            low(Limit::Depth, 2),
            answer("{}", ""),
            "limit_exceeded",
            "",
            2,
        ),
    ];
    for (d, bytes, code, path, maximum) in cases {
        let err = d.decode_answer(bytes.as_bytes()).expect_err("refused");
        assert_eq!((err.code(), err.path()), (code, Some(path)), "{err}");
        assert!(
            matches!(&err, erp_store_wire::WireError::LimitExceeded { maximum: m, .. } if *m == maximum),
            "{err}"
        );
    }
    assert_eq!(
        low(Limit::Bytes, 20)
            .decode_answer(two_records.as_bytes())
            .expect_err("refused")
            .to_string(),
        ": limit bytes exceeded (maximum 20)"
    );
    // The JSON parser's own wording is not stable across versions: assert code, path and prefix.
    let err = Decoder::new(WireLimits::default())
        .decode_answer(b"{")
        .expect_err("refused");
    assert_eq!((err.code(), err.path()), ("malformed", Some("")));
    assert!(
        err.to_string().starts_with(": malformed payload: "),
        "{err}"
    );
}

/// `float_roundtrip` feature; the crate's own reader parses the number text)
#[test]
fn float_decimals_are_read_back_exactly() {
    // Hard rounding cases: halfway points and neighbours, the subnormal edge, the largest finite
    // value, and the decimal expansion of 1 + 2^-53 with a digit past the midpoint.
    let lexemes = [
        "1.00000000000000011102230246251565404236316680908203125",
        "1.00000000000000011102230246251565404236316680908203126",
        "1.00000000000000011102230246251565404236316680908203124",
        "9007199254740993.0",
        "9007199254740993.000000000000000000001",
        "2.2250738585072014e-308",
        "2.2250738585072011e-308",
        "5e-324",
        "1.7976931348623157e308",
        "0.1",
        "-0.3e-5",
        "123456789.12345678901234567890",
        "18446744073709551617",
        "3",
        "-0.0",
    ];
    let d = decoder();
    for lexeme in lexemes {
        let bytes = answer(
            &format!(r#"{{"test.thread":[{{"id":7,"ratio":{lexeme}}}]}}"#),
            "",
        );
        let payload = decoded(d.decode_answer(bytes.as_bytes()).expect("decodes"));
        let values: Vec<FieldValue> = payload.upserts()[0]
            .fields()
            .iter()
            .map(|f| f.value().clone())
            .collect();
        let [FieldValue::Float(got)] = values.as_slice() else {
            panic!("expected one float for {lexeme}");
        };
        let expected: f64 = lexeme.parse().expect("rust parses it");
        assert_eq!(got.to_bits(), expected.to_bits(), "{lexeme}");
    }
    // A decimal out of the f64 range is refused, never turned into a default.
    let bytes = answer(r#"{"test.thread":[{"id":7,"ratio":1e999}]}"#, "");
    assert!(d.decode_answer(bytes.as_bytes()).is_err());
}
