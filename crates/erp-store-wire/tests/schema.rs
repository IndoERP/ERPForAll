//! Store schema construction, canonical encoding and strict decoding.
//! Feeds through the fixtures; the case itself is owned by the TypeScript side.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{assert_err, schema};
use erp_store_wire::{
    Decoder, FieldKind, FieldSpec, Limit, SchemaGeneration, StoreSchema, SyncClass, WireLimits,
};

fn builder() -> erp_store_wire::SchemaBuilder {
    StoreSchema::builder(SchemaGeneration::new("g1").expect("token"))
}

#[test]
fn schema_round_trips_and_encodes_canonically() {
    let schema = schema();
    let bytes = schema.encode().expect("encodes");
    assert_eq!(bytes, schema.encode().expect("encodes"), "byte identical");
    let decoded = Decoder::new(WireLimits::default())
        .decode_schema(&bytes)
        .expect("decodes");
    assert_eq!(decoded, schema);
    assert_eq!(decoded.generation().as_str(), "g1");
    // Accessors come from the builder input.
    let field = decoded
        .model("test.thread")
        .and_then(|m| m.field("author"))
        .expect("field");
    assert_eq!(field.kind(), FieldKind::Many2One);
    assert_eq!(field.target().map(|t| t.as_str()), Some("test.partner"));
    assert_eq!(field.sync(), SyncClass::State);
    let score = decoded
        .model("test.partner")
        .and_then(|m| m.field("score"))
        .expect("field");
    assert_eq!((score.sync(), score.cap()), (SyncClass::Counter, Some(99)));
    let stats = decoded
        .model("test.thread")
        .and_then(|m| m.field("stats"))
        .expect("field");
    assert_eq!(stats.sync(), SyncClass::AnswerOnly);

    // Canonical form: keys ascending everywhere, `sync` only when answer_only, default omitted.
    let mut b = builder();
    b.field("b.m", "z", FieldSpec::new(FieldKind::Int))
        .expect("ok");
    b.field("b.m", "a", FieldSpec::new(FieldKind::Counter).cap(5))
        .expect("ok");
    b.field("a.m", "s", FieldSpec::new(FieldKind::String).answer_only())
        .expect("ok");
    let text = String::from_utf8(b.build().expect("ok").encode().expect("encodes")).expect("utf8");
    assert_eq!(
        text,
        concat!(
            r#"{"models":{"a.m":{"fields":{"s":{"kind":"string","sync":"answer_only"}}},"#,
            r#""b.m":{"fields":{"a":{"cap":5,"kind":"counter"},"z":{"kind":"int"}}}},"#,
            r#""schema":"g1","wire":1}"#
        )
    );
}

#[test]
fn schema_builder_refuses_rule_violations() {
    let cases: Vec<(FieldSpec, &str, &str)> = vec![
        (
            FieldSpec::new(FieldKind::Many2One),
            "models.m.fields.f.target",
            "many2one needs a target model",
        ),
        (
            FieldSpec::new(FieldKind::X2Many),
            "models.m.fields.f.target",
            "x2many needs a target model",
        ),
        (
            FieldSpec::new(FieldKind::Int).target("m"),
            "models.m.fields.f.target",
            "target is only allowed on many2one and x2many",
        ),
        (
            FieldSpec::new(FieldKind::Reference).target("m"),
            "models.m.fields.f.target",
            "target is only allowed on many2one and x2many",
        ),
        (
            FieldSpec::new(FieldKind::Int).cap(3),
            "models.m.fields.f.cap",
            "cap is only allowed on counter",
        ),
        (
            FieldSpec::new(FieldKind::Counter).cap(0),
            "models.m.fields.f.cap",
            "cap must be 1..=2^53-1",
        ),
        (
            FieldSpec::new(FieldKind::Counter).answer_only(),
            "models.m.fields.f.sync",
            "sync is not allowed on counter",
        ),
    ];
    for (spec, path, reason) in cases {
        let message = format!("{path}: bad schema: {reason}");
        assert_err(
            builder().field("m", "f", spec),
            "bad_schema",
            Some(path),
            &message,
        );
    }
    // Reserved and malformed names.
    assert_err(
        builder().field("m", "id", FieldSpec::new(FieldKind::Int)),
        "bad_name",
        Some("models.m.fields.id"),
        "models.m.fields.id: bad name: field name is reserved",
    );
    assert_err(
        builder().field("m", "delete", FieldSpec::new(FieldKind::Int)),
        "bad_name",
        Some("models.m.fields.delete"),
        "models.m.fields.delete: bad name: field name is reserved",
    );
    assert_err(
        builder().field("M x", "f", FieldSpec::new(FieldKind::Int)),
        "bad_name",
        Some("models.M x"),
        "models.M x: bad name: model name must be lowercase letters, digits, '_' and '.', starting with a letter",
    );
    // Duplicate field; a target that is not a model is found at build.
    let mut b = builder();
    b.field("m", "f", FieldSpec::new(FieldKind::Int))
        .expect("ok");
    assert_err(
        b.field("m", "f", FieldSpec::new(FieldKind::Int)),
        "bad_schema",
        Some("models.m.fields.f"),
        "models.m.fields.f: bad schema: field declared twice",
    );
    let mut b = builder();
    b.field(
        "m",
        "f",
        FieldSpec::new(FieldKind::Many2One).target("nowhere"),
    )
    .expect("ok");
    assert_err(
        b.build(),
        "bad_schema",
        Some("models.m.fields.f.target"),
        "models.m.fields.f.target: bad schema: target nowhere is not a model of the schema",
    );
}

#[test]
fn schema_decoder_refuses_what_the_builder_refuses() {
    let decoder = Decoder::new(WireLimits::default());
    let schema = |fields: &str| {
        format!(r#"{{"models":{{"m":{{"fields":{{"f":{fields}}}}}}},"schema":"g","wire":1}}"#)
    };
    let cases: [(&str, &str, &str); 7] = [
        (
            r#"{"kind":"blob"}"#,
            "models.m.fields.f.kind",
            "unknown kind blob",
        ),
        (
            r#"{"kind":"many2one"}"#,
            "models.m.fields.f.target",
            "many2one needs a target model",
        ),
        (
            r#"{"kind":"many2one","target":"zz"}"#,
            "models.m.fields.f.target",
            "target zz is not a model of the schema",
        ),
        (
            r#"{"kind":"int","cap":3}"#,
            "models.m.fields.f.cap",
            "cap is only allowed on counter",
        ),
        (
            r#"{"kind":"counter","sync":"state"}"#,
            "models.m.fields.f.sync",
            "sync is not allowed on counter",
        ),
        (
            r#"{"kind":"int","sync":"other"}"#,
            "models.m.fields.f.sync",
            "sync must be state or answer_only",
        ),
        (
            r#"{"kind":"int","extra":1}"#,
            "models.m.fields.f.extra",
            "unknown member",
        ),
    ];
    for (fields, path, reason) in cases {
        let message = format!("{path}: bad schema: {reason}");
        assert_err(
            decoder.decode_schema(schema(fields).as_bytes()),
            "bad_schema",
            Some(path),
            &message,
        );
    }
    // Missing header members are malformed; a foreign major is refused first.
    assert_err(
        decoder.decode_schema(br#"{"models":{},"schema":"g"}"#),
        "malformed",
        Some("wire"),
        "wire: malformed payload: missing member",
    );
    let err = decoder
        .decode_schema(br#"{"wire":2,"junk":true}"#)
        .expect_err("refused");
    assert_eq!(err.code(), "wire_major_unsupported");
    // Limits apply to schemas too.
    let lowered = WireLimits::default()
        .lowered(Limit::SchemaFields, 1)
        .expect("lower");
    let two = br#"{"models":{"m":{"fields":{"a":{"kind":"int"},"b":{"kind":"int"}}}},"schema":"g","wire":1}"#;
    assert_err(
        Decoder::new(lowered).decode_schema(two),
        "limit_exceeded",
        Some("models.m.fields"),
        "models.m.fields: limit schema_fields exceeded (maximum 1)",
    );
}
