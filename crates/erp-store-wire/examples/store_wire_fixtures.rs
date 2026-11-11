//! Build tooling: write the golden fixture file on stdout.
//!
//! Every payload is produced by this crate's canonical encoder (or is a hand-written input to be
//! refused) and every expected result is what this crate's decoder and predicates return, never a
//! typed literal. The TypeScript conformance test checks its decoder against
//! the file. `cargo run --locked -p erp-store-wire --example store_wire_fixtures > FILE`.
use std::{collections::BTreeMap, error::Error};

use erp_store_wire::doc::{
    AsofDoc, ClientMajorCaseDoc, CursorCaseDoc, CursorDoc, CursorOrderDoc, CursorOutcomeDoc,
    DescriptorCaseDoc, DescriptorOutcomeDoc, FixtureFileDoc, LimitValueDoc, PayloadCaseDoc,
    PayloadKindDoc, PayloadOutcomeDoc, ProbeDoc, ReadOrderDoc, SchemaCaseDoc, SchemaOutcomeDoc,
};
use erp_store_wire::{
    AnswerBuilder, BusCursor, Decoder, FieldKind, FieldSpec, FieldValue, FrameBuilder, Limit,
    RecordRef, SchemaGeneration, SnapshotDescriptor, StoreSchema, WireLimits, WireMajor, Xid8,
    accept_client_major,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const T: &str = "test.thread";
const M: &str = "test.message";
const P: &str = "test.partner";

fn main() -> Result<()> {
    let file = build()?;
    println!("{}", serde_json::to_string_pretty(&file)?);
    Ok(())
}

fn xid(value: u64) -> Result<Xid8> {
    Ok(Xid8::new(value)?)
}

fn asof(own: Option<u64>) -> Result<SnapshotDescriptor> {
    let own = own.map(xid).transpose()?;
    Ok(SnapshotDescriptor::new(
        xid(100)?,
        xid(104)?,
        vec![xid(100)?, xid(103)?],
        own,
    )?)
}

/// A generic three-model schema covering every kind and sync class.
fn fixture_schema(generation: &str) -> Result<StoreSchema> {
    let mut b = StoreSchema::builder(SchemaGeneration::new(generation)?);
    for (name, kind) in [
        ("active", FieldKind::Bool),
        ("seq", FieldKind::Int),
        ("ratio", FieldKind::Float),
        ("title", FieldKind::String),
        ("body", FieldKind::Html),
        ("day", FieldKind::Date),
        ("stamp", FieldKind::Datetime),
        ("state", FieldKind::Selection),
        ("owner", FieldKind::Reference),
        ("unread", FieldKind::Counter),
    ] {
        b.field(T, name, FieldSpec::new(kind))?;
    }
    b.field(T, "author", FieldSpec::new(FieldKind::Many2One).target(P))?;
    b.field(T, "messages", FieldSpec::new(FieldKind::X2Many).target(M))?;
    b.field(T, "stats", FieldSpec::new(FieldKind::String).answer_only())?;
    b.field(M, "text", FieldSpec::new(FieldKind::String))?;
    b.field(M, "thread", FieldSpec::new(FieldKind::Many2One).target(T))?;
    b.field(P, "name", FieldSpec::new(FieldKind::String))?;
    b.field(P, "score", FieldSpec::new(FieldKind::Counter).cap(99))?;
    Ok(b.build()?)
}

struct Payloads {
    schemas: BTreeMap<String, String>,
    cases: Vec<PayloadCaseDoc>,
}

impl Payloads {
    /// Decode `wire` under the named schema and limits and record Rust's result.
    fn add(
        &mut self,
        name: &str,
        kind: PayloadKindDoc,
        schema: Option<&str>,
        limits: &[(Limit, u64)],
        wire: &str,
    ) -> Result<()> {
        let mut applied = WireLimits::default();
        for (limit, value) in limits {
            applied = applied.lowered(*limit, *value)?;
        }
        let mut decoder = Decoder::new(applied);
        if let Some(key) = schema {
            let text = self.schemas.get(key).ok_or("unknown schema key")?;
            decoder.set_schema(Decoder::new(WireLimits::default()).decode_schema(text.as_bytes())?);
        }
        let result = match kind {
            PayloadKindDoc::Answer => decoder.decode_answer(wire.as_bytes()),
            PayloadKindDoc::Frame => decoder.decode_frame(wire.as_bytes()),
        };
        self.cases.push(PayloadCaseDoc {
            kind,
            limits: limit_values(limits),
            name: name.to_owned(),
            outcome: PayloadOutcomeDoc::of(&result),
            schema: schema.map(str::to_owned),
            wire: wire.to_owned(),
        });
        Ok(())
    }
}

fn limit_values(limits: &[(Limit, u64)]) -> Vec<LimitValueDoc> {
    limits
        .iter()
        .map(|(limit, value)| LimitValueDoc {
            limit: (*limit).into(),
            value: *value,
        })
        .collect()
}

fn text(bytes: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(bytes)?)
}

const ASOF_READ: &str = r#"{"own":null,"xip":["100","103"],"xmax":"104","xmin":"100"}"#;
const ASOF_WRITE: &str = r#"{"own":"104","xip":["100","103"],"xmax":"104","xmin":"100"}"#;

fn answer_text(records: &str) -> String {
    format!(r#"{{"asof":{ASOF_READ},"records":{records},"schema":"g1","wire":1}}"#)
}

fn frame_text(records: &str) -> String {
    format!(
        r#"{{"asof":{ASOF_WRITE},"cursor":{{"id":"5512","xid":"104"}},"records":{records},"schema":"g1","wire":1}}"#
    )
}

fn payload_cases(schema: &StoreSchema) -> Result<Payloads> {
    let mut out = Payloads {
        schemas: BTreeMap::new(),
        cases: Vec::new(),
    };
    out.schemas.insert("g1".to_owned(), text(schema.encode()?)?);
    out.schemas
        .insert("g0".to_owned(), text(fixture_schema("g0")?.encode()?)?);
    let (a, f) = (PayloadKindDoc::Answer, PayloadKindDoc::Frame);

    // Valid payloads produced by the encoder.
    let mut b = AnswerBuilder::new(schema, asof(None)?);
    out.add("answer_empty", a, Some("g1"), &[], &text(b.encode()?)?)?;
    b = AnswerBuilder::new(schema, asof(Some(104))?);
    b.set(T, 7, "active", FieldValue::Bool(false))?;
    b.set(T, 7, "seq", FieldValue::Int(-3))?;
    b.set(T, 7, "ratio", FieldValue::Float(0.1))?;
    b.set(
        T,
        7,
        "title",
        FieldValue::String("t\u{e9}st \"q\"".to_owned()),
    )?;
    b.set(T, 7, "body", FieldValue::Html("<p>x</p>".to_owned()))?;
    b.set(T, 7, "day", FieldValue::Date("2026-02-28".to_owned()))?;
    b.set(
        T,
        7,
        "stamp",
        FieldValue::Datetime("2026-11-06 18:30:00".to_owned()),
    )?;
    b.set(T, 7, "state", FieldValue::Selection("open".to_owned()))?;
    b.set(T, 7, "author", FieldValue::Many2One(Some(4)))?;
    b.set(
        T,
        7,
        "owner",
        FieldValue::Reference(Some(RecordRef::new(P, 4)?)),
    )?;
    b.set(T, 7, "messages", FieldValue::X2ManyReplace(vec![5, 1, 9]))?;
    b.set(T, 7, "unread", FieldValue::Counter(12))?;
    b.set(T, 7, "stats", FieldValue::String("s".to_owned()))?;
    b.set(T, 8, "author", FieldValue::Many2One(None))?;
    b.set(T, 8, "owner", FieldValue::Reference(None))?;
    b.set(T, 8, "messages", FieldValue::X2ManyReplace(vec![]))?;
    b.set(M, 5, "text", FieldValue::String("hi".to_owned()))?;
    b.set(P, 3, "score", FieldValue::Counter(99))?;
    b.delete(M, 2)?;
    b.delete(T, 9)?;
    b.mark_written(T, 7, "messages")?;
    b.mark_written(T, 7, "title")?;
    let every_kind = text(b.encode()?)?;
    out.add(
        "answer_every_kind_with_written_and_deletes",
        a,
        Some("g1"),
        &[],
        &every_kind,
    )?;

    let cursor = BusCursor::new(xid(104)?, 5512)?;
    let mut fb = FrameBuilder::new(schema, cursor, asof(Some(104))?)?;
    fb.set(
        T,
        7,
        "messages",
        FieldValue::X2ManyDelta {
            add: vec![2, 5],
            remove: vec![9],
        },
    )?;
    fb.set(T, 7, "unread", FieldValue::Inc(-3))?;
    fb.set(P, 3, "score", FieldValue::Inc(2))?;
    fb.set(T, 8, "title", FieldValue::String("x".to_owned()))?;
    fb.delete(M, 2)?;
    out.add(
        "frame_inc_delta_and_delete",
        f,
        Some("g1"),
        &[],
        &text(fb.encode()?)?,
    )?;

    // Same bytes, other decoder state: no schema yet, and a held schema of another generation.
    out.add(
        "answer_needs_schema_when_none_held",
        a,
        None,
        &[],
        &every_kind,
    )?;
    out.add(
        "answer_needs_schema_when_other_generation_held",
        a,
        Some("g0"),
        &[],
        &every_kind,
    )?;

    // Refusals: inputs written by hand or mutated from valid bytes; the result is Rust's.
    let first = answer_text(r#"{"test.thread":[{"id":7,"seq":1}]}"#);
    out.add(
        "refuse_wire_major_2_before_content",
        a,
        Some("g1"),
        &[],
        r#"{"wire":2,"records":42}"#,
    )?;
    out.add(
        "refuse_missing_wire",
        a,
        Some("g1"),
        &[],
        &first.replace(r#","wire":1"#, ""),
    )?;
    out.add(
        "refuse_text_wire",
        a,
        Some("g1"),
        &[],
        &first.replace(r#""wire":1"#, r#""wire":"1""#),
    )?;
    out.add("refuse_not_json", a, Some("g1"), &[], "{")?;
    out.add("refuse_not_an_object", a, Some("g1"), &[], "[]")?;
    out.add(
        "refuse_unknown_member",
        a,
        Some("g1"),
        &[],
        &first.replace(r#""wire":1"#, r#""wire":1,"extra":1"#),
    )?;
    out.add(
        "refuse_unknown_model",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.nope":[{"id":1}]}"#),
    )?;
    out.add(
        "refuse_unknown_field",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":1,"nope":1}]}"#),
    )?;
    out.add(
        "refuse_type_mismatch",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":1,"seq":"1"}]}"#),
    )?;
    out.add(
        "refuse_unsafe_integer",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":1,"seq":9007199254740992}]}"#),
    )?;
    out.add(
        "refuse_bad_date",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"day":"2026-02-30","id":1}]}"#),
    )?;
    out.add(
        "refuse_bad_datetime",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":1,"stamp":"2026-02-03T01:02:03"}]}"#),
    )?;
    out.add(
        "refuse_id_zero",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":0,"seq":1}]}"#),
    )?;
    out.add(
        "refuse_id_above_safe_range",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":9007199254740992,"seq":1}]}"#),
    )?;
    out.add(
        "refuse_duplicate_record_id",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":1,"seq":1},{"id":1,"seq":2}]}"#),
    )?;
    out.add(
        "refuse_delete_with_fields",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"delete":true,"id":1,"seq":1}]}"#),
    )?;
    out.add(
        "refuse_delete_not_true",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"delete":false,"id":1}]}"#),
    )?;
    out.add(
        "refuse_duplicate_ids_in_replace",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":1,"messages":[2,2]}]}"#),
    )?;
    out.add(
        "refuse_add_remove_overlap",
        f,
        Some("g1"),
        &[],
        &frame_text(r#"{"test.thread":[{"id":1,"messages":{"add":[2],"remove":[2]}}]}"#),
    )?;
    out.add(
        "refuse_empty_x2many_delta",
        f,
        Some("g1"),
        &[],
        &frame_text(r#"{"test.thread":[{"id":1,"messages":{"add":[]}}]}"#),
    )?;
    out.add(
        "refuse_unknown_x2many_operation",
        f,
        Some("g1"),
        &[],
        &frame_text(r#"{"test.thread":[{"id":1,"messages":{"set":[1]}}]}"#),
    )?;
    out.add(
        "refuse_x2many_replace_in_frame",
        f,
        Some("g1"),
        &[],
        &frame_text(r#"{"test.thread":[{"id":1,"messages":[1]}]}"#),
    )?;
    out.add(
        "refuse_zero_counter_delta",
        f,
        Some("g1"),
        &[],
        &frame_text(r#"{"test.thread":[{"id":1,"unread":{"inc":0}}]}"#),
    )?;
    out.add(
        "refuse_counter_absolute_in_frame",
        f,
        Some("g1"),
        &[],
        &frame_text(r#"{"test.thread":[{"id":1,"unread":5}]}"#),
    )?;
    out.add(
        "refuse_counter_delta_in_answer",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.thread":[{"id":1,"unread":{"inc":1}}]}"#),
    )?;
    out.add(
        "refuse_answer_only_field_in_frame",
        f,
        Some("g1"),
        &[],
        &frame_text(r#"{"test.thread":[{"id":1,"stats":"s"}]}"#),
    )?;
    out.add(
        "refuse_counter_above_cap",
        a,
        Some("g1"),
        &[],
        &answer_text(r#"{"test.partner":[{"id":1,"score":100}]}"#),
    )?;
    out.add(
        "refuse_frame_without_cursor",
        f,
        Some("g1"),
        &[],
        &answer_text("{}"),
    )?;
    out.add(
        "refuse_frame_own_differs_from_cursor",
        f,
        Some("g1"),
        &[],
        &frame_text("{}").replace(r#""xid":"104""#, r#""xid":"103""#),
    )?;
    out.add(
        "refuse_written_without_own",
        a,
        Some("g1"),
        &[],
        &first.replace(
            r#""wire":1"#,
            r#""wire":1,"written":{"test.thread":{"7":["seq"]}}"#,
        ),
    )?;
    let own_answer = |written: &str| {
        format!(
            r#"{{"asof":{ASOF_WRITE},"records":{{"test.thread":[{{"id":7,"seq":1,"unread":2}}]}},"schema":"g1","wire":1,"written":{written}}}"#
        )
    };
    out.add(
        "refuse_written_unknown_record",
        a,
        Some("g1"),
        &[],
        &own_answer(r#"{"test.thread":{"8":["seq"]}}"#),
    )?;
    out.add(
        "refuse_written_field_not_set",
        a,
        Some("g1"),
        &[],
        &own_answer(r#"{"test.thread":{"7":["title"]}}"#),
    )?;
    out.add(
        "refuse_written_counter",
        a,
        Some("g1"),
        &[],
        &own_answer(r#"{"test.thread":{"7":["unread"]}}"#),
    )?;
    out.add(
        "refuse_written_duplicate_name",
        a,
        Some("g1"),
        &[],
        &own_answer(r#"{"test.thread":{"7":["seq","seq"]}}"#),
    )?;

    // Limits lowered so a small input crosses each bound.
    let two_models =
        answer_text(r#"{"test.message":[{"id":1,"text":"a"}],"test.thread":[{"id":1,"seq":1}]}"#);
    out.add(
        "limit_bytes",
        a,
        Some("g1"),
        &[(Limit::Bytes, 64)],
        &two_models,
    )?;
    out.add("limit_depth", a, Some("g1"), &[(Limit::Depth, 3)], &first)?;
    out.add(
        "limit_models",
        a,
        Some("g1"),
        &[(Limit::Models, 1)],
        &two_models,
    )?;
    out.add(
        "limit_records",
        a,
        Some("g1"),
        &[(Limit::Records, 1)],
        &answer_text(r#"{"test.thread":[{"id":1,"seq":1},{"id":2,"seq":1}]}"#),
    )?;
    out.add(
        "limit_ids_per_list",
        a,
        Some("g1"),
        &[(Limit::IdsPerList, 2)],
        &answer_text(r#"{"test.thread":[{"id":1,"messages":[1,2,3]}]}"#),
    )?;
    out.add("limit_xip", a, Some("g1"), &[(Limit::Xip, 1)], &first)?;
    out.add(
        "limit_string_bytes",
        a,
        Some("g1"),
        &[(Limit::StringBytes, 4)],
        &answer_text(r#"{"test.thread":[{"id":1,"title":"hello"}]}"#),
    )?;
    out.add(
        "limit_schema_token_bytes",
        a,
        Some("g1"),
        &[(Limit::SchemaTokenBytes, 1)],
        &first,
    )?;
    Ok(out)
}

fn schema_cases(schema: &StoreSchema) -> Result<Vec<SchemaCaseDoc>> {
    let mut out = Vec::new();
    let mut add = |name: &str, limits: &[(Limit, u64)], wire: String| -> Result<()> {
        let mut applied = WireLimits::default();
        for (limit, value) in limits {
            applied = applied.lowered(*limit, *value)?;
        }
        let result = Decoder::new(applied).decode_schema(wire.as_bytes());
        out.push(SchemaCaseDoc {
            limits: limit_values(limits),
            name: name.to_owned(),
            outcome: SchemaOutcomeDoc::of(&result),
            wire,
        });
        Ok(())
    };
    let one = |field: &str| {
        format!(r#"{{"models":{{"m":{{"fields":{{"f":{field}}}}}}},"schema":"g1","wire":1}}"#)
    };
    add("schema_every_kind", &[], text(schema.encode()?)?)?;
    add(
        "refuse_schema_wire_major_2",
        &[],
        r#"{"models":{},"schema":"g1","wire":2}"#.to_owned(),
    )?;
    add("refuse_schema_not_json", &[], "{".to_owned())?;
    add("refuse_schema_unknown_kind", &[], one(r#"{"kind":"blob"}"#))?;
    add(
        "refuse_schema_many2one_without_target",
        &[],
        one(r#"{"kind":"many2one"}"#),
    )?;
    add(
        "refuse_schema_target_on_int",
        &[],
        one(r#"{"kind":"int","target":"m"}"#),
    )?;
    add(
        "refuse_schema_cap_on_int",
        &[],
        one(r#"{"cap":3,"kind":"int"}"#),
    )?;
    add(
        "refuse_schema_cap_zero",
        &[],
        one(r#"{"cap":0,"kind":"counter"}"#),
    )?;
    add(
        "refuse_schema_sync_on_counter",
        &[],
        one(r#"{"kind":"counter","sync":"answer_only"}"#),
    )?;
    add(
        "refuse_schema_unknown_sync",
        &[],
        one(r#"{"kind":"int","sync":"lazy"}"#),
    )?;
    add(
        "refuse_schema_target_not_a_model",
        &[],
        one(r#"{"kind":"many2one","target":"other"}"#),
    )?;
    add(
        "refuse_schema_reserved_field_name",
        &[],
        r#"{"models":{"m":{"fields":{"id":{"kind":"int"}}}},"schema":"g1","wire":1}"#.to_owned(),
    )?;
    add(
        "refuse_schema_bad_model_name",
        &[],
        r#"{"models":{"M":{"fields":{}}},"schema":"g1","wire":1}"#.to_owned(),
    )?;
    add(
        "refuse_schema_empty_generation",
        &[],
        r#"{"models":{},"schema":"","wire":1}"#.to_owned(),
    )?;
    add(
        "limit_schema_fields",
        &[(Limit::SchemaFields, 2)],
        text(schema.encode()?)?,
    )?;
    add(
        "limit_schema_models",
        &[(Limit::Models, 2)],
        text(schema.encode()?)?,
    )?;
    Ok(out)
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}

fn descriptor_cases() -> Result<Vec<DescriptorCaseDoc>> {
    let mut out = Vec::new();
    let mut add =
        |name: &str, xmin: &str, xmax: &str, xip: &[&str], own: Option<&str>| -> Result<()> {
            let wire = AsofDoc {
                own: own.map(str::to_owned),
                xip: strings(xip),
                xmax: xmax.to_owned(),
                xmin: xmin.to_owned(),
            };
            let xip_refs: Vec<&str> = xip.to_vec();
            let built = SnapshotDescriptor::from_wire_strings(
                xmin,
                xmax,
                &xip_refs,
                own,
                &WireLimits::default(),
            );
            let outcome = match &built {
                Ok(descriptor) => {
                    let mut probes: Vec<u64> = vec![
                        descriptor.xmin().get().saturating_sub(1),
                        descriptor.xmin().get(),
                        descriptor.xmin().get().saturating_add(1),
                        descriptor.xmax().get().saturating_sub(1),
                        descriptor.xmax().get(),
                        descriptor.xmax().get().saturating_add(1),
                    ];
                    for entry in descriptor.xip() {
                        probes.extend([
                            entry.get().saturating_sub(1),
                            entry.get(),
                            entry.get().saturating_add(1),
                        ]);
                    }
                    if let Some(own) = descriptor.own() {
                        probes.extend([
                            own.get().saturating_sub(1),
                            own.get(),
                            own.get().saturating_add(1),
                        ]);
                    }
                    probes.retain(|probe| *probe > 0);
                    probes.sort_unstable();
                    probes.dedup();
                    let probes = probes
                        .into_iter()
                        .map(|probe| {
                            Xid8::new(probe).map(|x| ProbeDoc {
                                reflects: descriptor.reflects(x),
                                xid: x.to_string(),
                            })
                        })
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    DescriptorOutcomeDoc::Valid {
                        asof: descriptor.into(),
                        completed_count: descriptor.completed_count().to_string(),
                        probes,
                    }
                }
                Err(error) => DescriptorOutcomeDoc::Refused {
                    refusal: error.into(),
                },
            };
            out.push(DescriptorCaseDoc {
                name: name.to_owned(),
                outcome,
                wire,
            });
            Ok(())
        };
    add(
        "read_only_with_in_progress",
        "100",
        "105",
        &["100", "103"],
        None,
    )?;
    add(
        "refuse_own_below_xmax",
        "100",
        "105",
        &["100", "103"],
        Some("104"),
    )?;
    add("writer_at_xmax", "100", "105", &["100", "103"], Some("105"))?;
    add(
        "writer_above_xmax",
        "100",
        "105",
        &["100", "103"],
        Some("110"),
    )?;
    add("no_in_progress", "7", "7", &[], None)?;
    add(
        "above_2_pow_53",
        "9007199254740993",
        "9007199254740999",
        &["9007199254740995"],
        Some("9007199254741001"),
    )?;
    add(
        "near_u64_max",
        "18446744073709551610",
        "18446744073709551615",
        &["18446744073709551612"],
        None,
    )?;
    add("refuse_xmin_above_xmax", "6", "5", &[], None)?;
    add("refuse_xip_below_xmin", "100", "105", &["99"], None)?;
    add("refuse_xip_at_xmax", "100", "105", &["105"], None)?;
    add(
        "refuse_xip_not_ascending",
        "100",
        "105",
        &["103", "101"],
        None,
    )?;
    add("refuse_xip_duplicate", "100", "105", &["103", "103"], None)?;
    add(
        "refuse_own_in_xip",
        "100",
        "105",
        &["100", "103"],
        Some("103"),
    )?;
    add("refuse_zero_xid", "0", "5", &[], None)?;
    add("refuse_leading_zero", "0100", "105", &[], None)?;
    add("refuse_non_digit", "1e2", "105", &[], None)?;
    add("refuse_negative", "-1", "105", &[], None)?;
    add("refuse_above_u64", "1", "18446744073709551616", &[], None)?;
    add("refuse_empty_xip_entry", "100", "105", &[""], None)?;
    Ok(out)
}

fn cursor_cases() -> Vec<CursorCaseDoc> {
    let mut out = Vec::new();
    for (name, xid, id) in [
        ("cursor_plain", "104", "5512"),
        (
            "cursor_above_2_pow_53",
            "9007199254740993",
            "9223372036854775807",
        ),
        ("refuse_cursor_id_zero", "104", "0"),
        ("refuse_cursor_id_above_i64", "104", "9223372036854775808"),
        ("refuse_cursor_id_leading_zero", "104", "05"),
        ("refuse_cursor_xid_zero", "0", "5"),
        ("refuse_cursor_xid_not_decimal", "x", "5"),
    ] {
        let outcome = match BusCursor::from_wire_strings(xid, id) {
            Ok(cursor) => CursorOutcomeDoc::Valid {
                cursor: (&cursor).into(),
            },
            Err(error) => CursorOutcomeDoc::Refused {
                refusal: (&error).into(),
            },
        };
        out.push(CursorCaseDoc {
            name: name.to_owned(),
            outcome,
            wire: CursorDoc {
                id: id.to_owned(),
                xid: xid.to_owned(),
            },
        });
    }
    out
}

fn cursor_order() -> Result<CursorOrderDoc> {
    let mut cursors = Vec::new();
    for (x, i) in [(10, 1), (9, 2), (9, 1), (100, 1), (10, 2), (9, 10), (9, 9)] {
        cursors.push(BusCursor::new(xid(x)?, i)?);
    }
    let mut sorted = cursors.clone();
    sorted.sort();
    Ok(CursorOrderDoc {
        shuffled: cursors.iter().map(CursorDoc::from).collect(),
        sorted: sorted.iter().map(CursorDoc::from).collect(),
    })
}

fn read_order() -> Result<ReadOrderDoc> {
    let descriptors = [
        SnapshotDescriptor::new(xid(100)?, xid(105)?, vec![xid(100)?, xid(103)?], None)?,
        SnapshotDescriptor::new(xid(100)?, xid(105)?, vec![xid(100)?], Some(xid(105)?))?,
        SnapshotDescriptor::new(xid(90)?, xid(101)?, vec![], None)?,
        SnapshotDescriptor::new(xid(100)?, xid(105)?, vec![xid(101)?, xid(102)?], None)?,
        SnapshotDescriptor::new(xid(9)?, xid(10)?, vec![xid(9)?], None)?,
    ];
    let counts: Vec<u64> = descriptors
        .iter()
        .map(SnapshotDescriptor::completed_count)
        .collect();
    let mut order: Vec<u32> = (0..u32::try_from(descriptors.len())?).collect();
    order.sort_by_key(|i| counts[*i as usize]);
    Ok(ReadOrderDoc {
        ascending: order,
        counts: counts.iter().map(ToString::to_string).collect(),
        descriptors: descriptors.iter().map(AsofDoc::from).collect(),
    })
}

fn client_majors() -> Vec<ClientMajorCaseDoc> {
    [0_u64, 1, 2, u64::from(u32::MAX) + 1]
        .into_iter()
        .map(|client| match accept_client_major(client) {
            Ok(major) => ClientMajorCaseDoc {
                accepted: Some(major.get()),
                client,
                refusal: None,
            },
            Err(error) => ClientMajorCaseDoc {
                accepted: None,
                client,
                refusal: Some((&error).into()),
            },
        })
        .collect()
}

fn build() -> Result<FixtureFileDoc> {
    let schema = fixture_schema("g1")?;
    let payloads = payload_cases(&schema)?;
    Ok(FixtureFileDoc {
        client_majors: client_majors(),
        cursor_cases: cursor_cases(),
        cursor_order: cursor_order()?,
        descriptor_cases: descriptor_cases()?,
        format: 1,
        limits: Limit::ALL
            .into_iter()
            .map(|limit| LimitValueDoc {
                limit: limit.into(),
                value: limit.maximum(),
            })
            .collect(),
        payload_cases: payloads.cases,
        read_order: read_order()?,
        schema_cases: schema_cases(&schema)?,
        schemas: payloads.schemas,
        wire: WireMajor::CURRENT.get(),
    })
}
