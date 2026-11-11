//! Property tests: decode(encode(x)) = x for generated valid payloads (answers and frames).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;

use common::{cursor_104, read_only, schema, writing};
use erp_store_wire::{
    AnswerBuilder, Decoder, FieldValue, FrameBuilder, Outcome, RecordRef, WireLimits,
};
use proptest::prelude::*;

const T: &str = "test.thread";

fn ids() -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::btree_set(1_u64..1000, 0..6).prop_map(|s| s.into_iter().collect())
}

/// A valid value for a `test.thread` field, shaped for an answer or for a frame.
fn value(field: &'static str, frame: bool) -> BoxedStrategy<FieldValue> {
    match field {
        "active" => any::<bool>().prop_map(FieldValue::Bool).boxed(),
        "seq" => (-(1_i64 << 53) + 1..(1_i64 << 53))
            .prop_map(FieldValue::Int)
            .boxed(),
        "ratio" => (-1.0e9_f64..1.0e9).prop_map(FieldValue::Float).boxed(),
        "title" => "\\PC{0,12}".prop_map(FieldValue::String).boxed(),
        "day" => (1_u32..=9999, 1_u32..=12, 1_u32..=28)
            .prop_map(|(y, m, d)| FieldValue::Date(format!("{y:04}-{m:02}-{d:02}")))
            .boxed(),
        "author" => proptest::option::of(1_u64..1000)
            .prop_map(FieldValue::Many2One)
            .boxed(),
        "owner" => proptest::option::of(1_u64..1000)
            .prop_map(|id| {
                FieldValue::Reference(id.map(|id| RecordRef::new("test.partner", id).expect("ref")))
            })
            .boxed(),
        "messages" if frame => (ids(), ids())
            .prop_filter_map(
                "delta must change something and not overlap",
                |(add, remove)| {
                    let remove: Vec<u64> =
                        remove.into_iter().filter(|r| !add.contains(r)).collect();
                    (!add.is_empty() || !remove.is_empty())
                        .then_some(FieldValue::X2ManyDelta { add, remove })
                },
            )
            .boxed(),
        "messages" => ids().prop_map(FieldValue::X2ManyReplace).boxed(),
        "unread" if frame => (-1000_i64..1000)
            .prop_filter("zero delta is refused", |n| *n != 0)
            .prop_map(FieldValue::Inc)
            .boxed(),
        "unread" => (0_i64..1000).prop_map(FieldValue::Counter).boxed(),
        other => panic!("unknown field {other}"),
    }
}

const FIELDS: [&str; 9] = [
    "active", "seq", "ratio", "title", "day", "author", "owner", "messages", "unread",
];

/// Per record id: either a delete marker or a set of fields.
type Plan = BTreeMap<u64, Option<Vec<(&'static str, FieldValue)>>>;

fn plan(frame: bool) -> impl Strategy<Value = Plan> {
    let fields = proptest::sample::subsequence(FIELDS.to_vec(), 1..=FIELDS.len()).prop_flat_map(
        move |names| {
            names
                .into_iter()
                .map(|n| value(n, frame).prop_map(move |v| (n, v)))
                .collect::<Vec<_>>()
        },
    );
    proptest::collection::btree_map(1_u64..100, proptest::option::weighted(0.8, fields), 0..8)
}

fn run(frame: bool, plan: &Plan) {
    let schema = schema();
    let decoder = Decoder::new(WireLimits::default()).with_schema(schema.clone());
    let bytes = if frame {
        let mut b = FrameBuilder::new(&schema, cursor_104(), writing()).expect("frame");
        for (id, entry) in plan {
            match entry {
                None => b.delete(T, *id).expect("delete"),
                Some(fields) => {
                    for (name, v) in fields {
                        b.set(T, *id, name, v.clone()).expect("valid value");
                    }
                }
            }
        }
        b.encode().expect("encodes")
    } else {
        let mut b = AnswerBuilder::new(&schema, read_only());
        for (id, entry) in plan {
            match entry {
                None => b.delete(T, *id).expect("delete"),
                Some(fields) => {
                    for (name, v) in fields {
                        b.set(T, *id, name, v.clone()).expect("valid value");
                    }
                }
            }
        }
        b.encode().expect("encodes")
    };
    let outcome = if frame {
        decoder.decode_frame(&bytes)
    } else {
        decoder.decode_answer(&bytes)
    };
    let Outcome::Decoded(payload) = outcome.expect("decodes") else {
        panic!("schema is held")
    };
    // Expected form: upserts by ascending id with fields by ascending name, deletes by id.
    let mut expected_upserts = Vec::new();
    let mut expected_deletes = Vec::new();
    for (id, entry) in plan {
        match entry {
            None => expected_deletes.push(*id),
            Some(fields) => {
                let mut sorted: Vec<(String, FieldValue)> = fields
                    .iter()
                    .map(|(n, v)| ((*n).to_owned(), normalise(v)))
                    .collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                expected_upserts.push((*id, sorted));
            }
        }
    }
    let upserts: Vec<(u64, Vec<(String, FieldValue)>)> = payload
        .upserts()
        .iter()
        .map(|r| {
            (
                r.id(),
                r.fields()
                    .iter()
                    .map(|f| (f.name().as_str().to_owned(), f.value().clone()))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(upserts, expected_upserts);
    let deletes: Vec<u64> = payload.deletes().iter().map(|r| r.id()).collect();
    assert_eq!(deletes, expected_deletes);
}

/// The canonical decoded form of an input value (id lists ascending).
fn normalise(v: &FieldValue) -> FieldValue {
    match v {
        FieldValue::X2ManyReplace(ids) => {
            let mut ids = ids.clone();
            ids.sort_unstable();
            FieldValue::X2ManyReplace(ids)
        }
        FieldValue::X2ManyDelta { add, remove } => {
            let (mut add, mut remove) = (add.clone(), remove.clone());
            add.sort_unstable();
            remove.sort_unstable();
            FieldValue::X2ManyDelta { add, remove }
        }
        other => other.clone(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn answer_round_trip(p in plan(false)) {
        run(false, &p);
    }

    #[test]
    fn frame_round_trip(p in plan(true)) {
        run(true, &p);
    }
}
