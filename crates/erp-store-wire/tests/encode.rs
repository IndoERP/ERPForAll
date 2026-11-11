//! Encoder tests: to 008 and 015.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{assert_err, cursor_104, read_only, schema, writing, x};
use erp_store_wire::{
    AnswerBuilder, BusCursor, FieldValue, FrameBuilder, Limit, RecordRef, StoreSchema, WireLimits,
};

const T: &str = "test.thread";

fn s(text: &str) -> String {
    text.to_owned()
}

fn answer(schema: &StoreSchema) -> AnswerBuilder<'_> {
    AnswerBuilder::new(schema, read_only())
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).expect("the encoder writes UTF-8")
}

const ASOF_READ: &str = r#"{"own":null,"xip":["100","103"],"xmax":"104","xmin":"100"}"#;
const ASOF_WRITE: &str = r#"{"own":"104","xip":["100","103"],"xmax":"104","xmin":"100"}"#;

#[test]
fn records_grouped_by_model_canonical() {
    let schema = schema();
    let fill = |b: &mut AnswerBuilder<'_>| {
        // Models, ids and fields deliberately added out of order.
        b.set(T, 9, "title", FieldValue::String(s("b")))
            .expect("set");
        b.set("test.message", 3, "text", FieldValue::String(s("hi")))
            .expect("set");
        b.set(T, 7, "title", FieldValue::String(s("a")))
            .expect("set");
        b.set(T, 7, "active", FieldValue::Bool(true)).expect("set");
    };
    let mut first = answer(&schema);
    fill(&mut first);
    let first = text(first.encode().expect("encodes"));
    let mut second = answer(&schema);
    fill(&mut second);
    assert_eq!(
        first,
        text(second.encode().expect("encodes")),
        "byte identical"
    );
    let expected = format!(
        concat!(
            r#"{{"asof":{},"records":{{"#,
            r#""test.message":[{{"id":3,"text":"hi"}}],"#,
            r#""test.thread":[{{"active":true,"id":7,"title":"a"}},{{"id":9,"title":"b"}}]}},"#,
            r#""schema":"g1","wire":1}}"#
        ),
        ASOF_READ
    );
    assert_eq!(first, expected);
    // A model nobody touched (test.partner) is absent; an empty answer has an empty records map.
    assert!(!first.contains("test.partner"));
    let empty = text(answer(&schema).encode().expect("encodes"));
    assert_eq!(
        empty,
        format!(r#"{{"asof":{ASOF_READ},"records":{{}},"schema":"g1","wire":1}}"#)
    );
}

#[test]
fn scalar_values_by_kind_and_refusals() {
    let schema = schema();
    let mut b = answer(&schema);
    let at = |field: &str| format!("records.{T}[id=7].{field}");
    for (field, value) in [
        ("active", FieldValue::Bool(false)),
        ("seq", FieldValue::Int(-4)),
        ("ratio", FieldValue::Float(2.5)),
        ("title", FieldValue::String(s("quo\"te é"))),
        ("body", FieldValue::Html(s("<p>x</p>"))),
        ("day", FieldValue::Date(s("2028-02-29"))),
        ("stamp", FieldValue::Datetime(s("2026-11-06 17:45:59"))),
        ("state", FieldValue::Selection(s("open"))),
    ] {
        b.set(T, 7, field, value).expect("set");
    }
    // html travels as a plain string: no in-band markup tag.
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            concat!(
                r#"{{"asof":{},"records":{{"test.thread":[{{"active":false,"body":"<p>x</p>","#,
                r#""day":"2028-02-29","id":7,"ratio":2.5,"seq":-4,"stamp":"2026-11-06 17:45:59","#,
                r#""state":"open","title":"quo\"te é"}}]}},"schema":"g1","wire":1}}"#
            ),
            ASOF_READ
        )
    );

    // A value of the wrong kind names the expected kind.
    let mismatches: [(&str, FieldValue, &str); 6] = [
        ("active", FieldValue::Int(1), "bool"),
        ("seq", FieldValue::Float(1.0), "int"),
        ("title", FieldValue::Html(s("x")), "string"),
        ("body", FieldValue::String(s("x")), "html"),
        ("state", FieldValue::String(s("x")), "selection"),
        (
            "day",
            FieldValue::Datetime(s("2026-01-01 00:00:00")),
            "date",
        ),
    ];
    for (field, value, expected) in mismatches {
        let path = at(field);
        let mut b = answer(&schema);
        assert_err(
            b.set(T, 7, field, value),
            "type_mismatch",
            Some(&path),
            &format!("{path}: expected {expected}"),
        );
    }
    // Non-finite floats, unsafe integers and impossible dates are out of range.
    let range: [(&str, FieldValue, &str); 7] = [
        ("ratio", FieldValue::Float(f64::NAN), "float is not finite"),
        (
            "ratio",
            FieldValue::Float(f64::INFINITY),
            "float is not finite",
        ),
        (
            "seq",
            FieldValue::Int(1 << 53),
            "integer is outside the safe range +-(2^53-1)",
        ),
        (
            "seq",
            FieldValue::Int(-(1 << 53)),
            "integer is outside the safe range +-(2^53-1)",
        ),
        (
            "day",
            FieldValue::Date(s("2026-02-30")),
            "not a valid YYYY-MM-DD date",
        ),
        (
            "day",
            FieldValue::Date(s("2026-2-3")),
            "not a valid YYYY-MM-DD date",
        ),
        (
            "stamp",
            FieldValue::Datetime(s("2026-11-06T17:45:59")),
            "not a valid YYYY-MM-DD HH:MM:SS datetime",
        ),
    ];
    for (field, value, reason) in range {
        let path = at(field);
        let mut b = answer(&schema);
        assert_err(
            b.set(T, 7, field, value),
            "value_out_of_range",
            Some(&path),
            &format!("{path}: {reason}"),
        );
    }
    // The boundary values themselves are accepted.
    let mut b = answer(&schema);
    b.set(T, 7, "seq", FieldValue::Int((1 << 53) - 1))
        .expect("max safe");
    b.set(T, 7, "ratio", FieldValue::Float(-0.0))
        .expect("finite");
    // Unknown names.
    assert_err(
        b.set(T, 7, "nope", FieldValue::Bool(true)),
        "unknown_field",
        Some(&at("nope")),
        &format!("{}: unknown field nope", at("nope")),
    );
    assert_err(
        b.set("nowhere", 1, "title", FieldValue::Bool(true)),
        "unknown_model",
        Some("records.nowhere"),
        "records.nowhere: unknown model nowhere",
    );
    // A string above the limit is refused by name.
    let limits = WireLimits::default()
        .lowered(Limit::StringBytes, 4)
        .expect("lower");
    let mut b = answer(&schema).with_limits(limits);
    assert_err(
        b.set(T, 7, "title", FieldValue::String(s("12345"))),
        "limit_exceeded",
        Some(&at("title")),
        &format!("{}: limit string_bytes exceeded (maximum 4)", at("title")),
    );
}

#[test]
fn x2many_replace_and_duplicate() {
    let schema = schema();
    let path = format!("records.{T}[id=7].messages");
    let mut b = answer(&schema);
    b.set(T, 7, "messages", FieldValue::X2ManyReplace(vec![9, 3, 5]))
        .expect("set");
    b.set(T, 8, "messages", FieldValue::X2ManyReplace(vec![]))
        .expect("an empty replace clears");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            concat!(
                r#"{{"asof":{},"records":{{"test.thread":[{{"id":7,"messages":[3,5,9]}},"#,
                r#"{{"id":8,"messages":[]}}]}},"schema":"g1","wire":1}}"#
            ),
            ASOF_READ
        )
    );
    let mut b = answer(&schema);
    assert_err(
        b.set(T, 7, "messages", FieldValue::X2ManyReplace(vec![3, 5, 3])),
        "duplicate_id",
        Some(&path),
        &format!("{path}: duplicate id 3"),
    );
    assert_err(
        b.set(T, 7, "messages", FieldValue::X2ManyReplace(vec![3, 0])),
        "bad_id",
        Some(&format!("{path}[1]")),
        &format!("{path}[1]: bad record id 0"),
    );
    let limits = WireLimits::default()
        .lowered(Limit::IdsPerList, 2)
        .expect("lower");
    let mut b = answer(&schema).with_limits(limits);
    assert_err(
        b.set(T, 7, "messages", FieldValue::X2ManyReplace(vec![1, 2, 3])),
        "limit_exceeded",
        Some(&path),
        &format!("{path}: limit ids_per_list exceeded (maximum 2)"),
    );
}

#[test]
fn x2many_add_and_remove_and_refusals() {
    let schema = schema();
    let path = format!("records.{T}[id=7].messages");
    let frame = || FrameBuilder::new(&schema, cursor_104(), writing()).expect("frame");
    let mut b = frame();
    b.set(
        T,
        7,
        "messages",
        FieldValue::X2ManyDelta {
            add: vec![5, 2],
            remove: vec![9],
        },
    )
    .expect("set");
    b.set(
        T,
        8,
        "messages",
        FieldValue::X2ManyDelta {
            add: vec![],
            remove: vec![9],
        },
    )
    .expect("set");
    b.set(
        T,
        9,
        "messages",
        FieldValue::X2ManyDelta {
            add: vec![4],
            remove: vec![],
        },
    )
    .expect("set");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            concat!(
                r#"{{"asof":{},"cursor":{{"id":"5512","xid":"104"}},"records":{{"test.thread":["#,
                r#"{{"id":7,"messages":{{"add":[2,5],"remove":[9]}}}},"#,
                r#"{{"id":8,"messages":{{"remove":[9]}}}},"#,
                r#"{{"id":9,"messages":{{"add":[4]}}}}]}},"schema":"g1","wire":1}}"#
            ),
            ASOF_WRITE
        )
    );
    let mut b = frame();
    assert_err(
        b.set(
            T,
            7,
            "messages",
            FieldValue::X2ManyDelta {
                add: vec![5, 6],
                remove: vec![6],
            },
        ),
        "add_remove_overlap",
        Some(&path),
        &format!("{path}: id 6 is in both add and remove"),
    );
    assert_err(
        b.set(
            T,
            7,
            "messages",
            FieldValue::X2ManyDelta {
                add: vec![],
                remove: vec![],
            },
        ),
        "empty_operation",
        Some(&path),
        &format!("{path}: empty operation"),
    );
    assert_err(
        b.set(
            T,
            7,
            "messages",
            FieldValue::X2ManyDelta {
                add: vec![1, 1],
                remove: vec![],
            },
        ),
        "duplicate_id",
        Some(&format!("{path}.add")),
        &format!("{path}.add: duplicate id 1"),
    );
    // One shape per value: a second value for the same field (replace after add/remove) is refused.
    let mut b = answer(&schema);
    b.set(
        T,
        7,
        "messages",
        FieldValue::X2ManyDelta {
            add: vec![1],
            remove: vec![],
        },
    )
    .expect("set");
    assert_err(
        b.set(T, 7, "messages", FieldValue::X2ManyReplace(vec![2])),
        "duplicate_field",
        Some(&path),
        &format!("{path}: duplicate field"),
    );
}

#[test]
fn counter_inc_rules() {
    let schema = schema();
    let unread = format!("records.{T}[id=7].unread");
    let frame = || FrameBuilder::new(&schema, cursor_104(), writing()).expect("frame");
    let mut b = frame();
    b.set(T, 7, "unread", FieldValue::Inc(-3)).expect("set");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            concat!(
                r#"{{"asof":{},"cursor":{{"id":"5512","xid":"104"}},"records":{{"test.thread":["#,
                r#"{{"id":7,"unread":{{"inc":-3}}}}]}},"schema":"g1","wire":1}}"#
            ),
            ASOF_WRITE
        )
    );
    let seq = format!("records.{T}[id=7].seq");
    let mut b = frame();
    assert_err(
        b.set(T, 7, "seq", FieldValue::Inc(1)),
        "operation_not_allowed",
        Some(&seq),
        &format!("{seq}: operation not allowed: inc is only allowed on a counter field"),
    );
    assert_err(
        b.set(T, 7, "unread", FieldValue::Inc(0)),
        "zero_delta",
        Some(&unread),
        &format!("{unread}: zero counter delta"),
    );
    // An answer sets counters as plain integers only.
    let mut b = answer(&schema);
    b.set(T, 7, "unread", FieldValue::Counter(12)).expect("set");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            concat!(
                r#"{{"asof":{},"records":{{"test.thread":[{{"id":7,"unread":12}}]}},"#,
                r#""schema":"g1","wire":1}}"#
            ),
            ASOF_READ
        )
    );
    assert_err(
        b.set(T, 8, "unread", FieldValue::Inc(1)),
        "operation_not_allowed",
        Some(&format!("records.{T}[id=8].unread")),
        &format!(
            "records.{T}[id=8].unread: operation not allowed: inc is only allowed in a bus frame"
        ),
    );
    assert_err(
        b.set(T, 8, "unread", FieldValue::Int(1)),
        "type_mismatch",
        Some(&format!("records.{T}[id=8].unread")),
        &format!("records.{T}[id=8].unread: expected counter"),
    );
    // A declared cap: the value may equal it (saturated), not exceed it.
    b.set("test.partner", 4, "score", FieldValue::Counter(99))
        .expect("at cap");
    assert_err(
        b.set("test.partner", 5, "score", FieldValue::Counter(100)),
        "value_out_of_range",
        Some("records.test.partner[id=5].score"),
        "records.test.partner[id=5].score: counter value is above its cap 99",
    );
}

#[test]
fn frame_refusals() {
    let schema = schema();
    let mut b = FrameBuilder::new(&schema, cursor_104(), writing()).expect("frame");
    let counter = format!("records.{T}[id=7].unread");
    assert_err(
        b.set(T, 7, "unread", FieldValue::Counter(1)),
        "operation_not_allowed",
        Some(&counter),
        &format!("{counter}: operation not allowed: a frame carries counters as inc deltas"),
    );
    let messages = format!("records.{T}[id=7].messages");
    assert_err(
        b.set(T, 7, "messages", FieldValue::X2ManyReplace(vec![1])),
        "operation_not_allowed",
        Some(&messages),
        &format!("{messages}: operation not allowed: x2many replace is not allowed in a frame"),
    );
    let stats = format!("records.{T}[id=7].stats");
    assert_err(
        b.set(T, 7, "stats", FieldValue::String(s("x"))),
        "operation_not_allowed",
        Some(&stats),
        &format!("{stats}: operation not allowed: answer_only field is not allowed in a frame"),
    );
    let mut a = answer(&schema);
    a.set(T, 7, "stats", FieldValue::String(s("x")))
        .expect("answers may carry answer_only fields");
}

#[test]
fn relations_plain_ids_and_reference_pair() {
    let schema = schema();
    let mut b = answer(&schema);
    b.set(T, 7, "author", FieldValue::Many2One(Some(4)))
        .expect("set");
    b.set(T, 8, "author", FieldValue::Many2One(None))
        .expect("set");
    let pair = RecordRef::new("test.partner", 4).expect("ref");
    b.set(T, 7, "owner", FieldValue::Reference(Some(pair)))
        .expect("set");
    b.set(T, 8, "owner", FieldValue::Reference(None))
        .expect("set");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            concat!(
                r#"{{"asof":{},"records":{{"test.thread":["#,
                r#"{{"author":4,"id":7,"owner":{{"id":4,"model":"test.partner"}}}},"#,
                r#"{{"author":null,"id":8,"owner":null}}]}},"schema":"g1","wire":1}}"#
            ),
            ASOF_READ
        )
    );
    let owner = format!("records.{T}[id=7].owner");
    let ghost = RecordRef::new("nowhere", 1).expect("a well formed name");
    let mut b = answer(&schema);
    assert_err(
        b.set(T, 7, "owner", FieldValue::Reference(Some(ghost))),
        "unknown_model",
        Some(&format!("{owner}.model")),
        &format!("{owner}.model: unknown model nowhere"),
    );
    // A pair on a plain relation, and a plain id on the reference field.
    let author = format!("records.{T}[id=7].author");
    let pair = RecordRef::new("test.partner", 4).expect("ref");
    assert_err(
        b.set(T, 7, "author", FieldValue::Reference(Some(pair))),
        "type_mismatch",
        Some(&author),
        &format!("{author}: expected many2one"),
    );
    assert_err(
        b.set(T, 7, "owner", FieldValue::Many2One(Some(4))),
        "type_mismatch",
        Some(&owner),
        &format!("{owner}: expected reference"),
    );
    assert_err(
        b.set(T, 7, "author", FieldValue::Many2One(Some(0))),
        "bad_id",
        Some(&author),
        &format!("{author}: bad record id 0"),
    );
}

#[test]
fn record_id_bounds() {
    let schema = schema();
    let max = (1_u64 << 53) - 1;
    let mut b = answer(&schema);
    b.set(T, 1, "seq", FieldValue::Int(1)).expect("lowest id");
    b.set(T, max, "seq", FieldValue::Int(1))
        .expect("highest id");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            concat!(
                r#"{{"asof":{},"records":{{"test.thread":[{{"id":1,"seq":1}},"#,
                r#"{{"id":{},"seq":1}}]}},"schema":"g1","wire":1}}"#
            ),
            ASOF_READ, max
        )
    );
    for bad in [0_u64, 1 << 53, u64::MAX] {
        let path = format!("records.{T}[id={bad}]");
        let mut b = answer(&schema);
        assert_err(
            b.set(T, bad, "seq", FieldValue::Int(1)),
            "bad_id",
            Some(&path),
            &format!("{path}: bad record id {bad}"),
        );
        assert_err(
            b.delete(T, bad),
            "bad_id",
            Some(&path),
            &format!("{path}: bad record id {bad}"),
        );
    }
}

#[test]
fn delete_marker_and_conflicts() {
    let schema = schema();
    let at = format!("records.{T}[id=7]");
    let mut b = answer(&schema);
    b.delete(T, 7).expect("delete");
    b.delete(T, 7).expect("the same fact twice is one marker");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            r#"{{"asof":{ASOF_READ},"records":{{"test.thread":[{{"delete":true,"id":7}}]}},"schema":"g1","wire":1}}"#
        )
    );
    // Delete plus fields, in either order.
    let message = format!("{at}: delete marker together with fields");
    assert_err(
        b.set(T, 7, "title", FieldValue::String(s("x"))),
        "delete_with_fields",
        Some(&at),
        &message,
    );
    let mut b = answer(&schema);
    b.set(T, 7, "title", FieldValue::String(s("x")))
        .expect("set");
    assert_err(b.delete(T, 7), "delete_with_fields", Some(&at), &message);
    // A repeated field; sets for one record merge into one entry.
    let title = format!("{at}.title");
    assert_err(
        b.set(T, 7, "title", FieldValue::String(s("y"))),
        "duplicate_field",
        Some(&title),
        &format!("{title}: duplicate field"),
    );
    b.set(T, 7, "seq", FieldValue::Int(2)).expect("merges");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            r#"{{"asof":{ASOF_READ},"records":{{"test.thread":[{{"id":7,"seq":2,"title":"x"}}]}},"schema":"g1","wire":1}}"#
        )
    );
}

#[test]
fn frame_cursor_order_and_own_match() {
    let schema = schema();
    // A frame whose descriptor own differs from the cursor xid, or has none, is refused.
    let reason = "own xid must equal the cursor xid";
    for asof in [
        read_only(),
        erp_store_wire::SnapshotDescriptor::new(x(100), x(105), vec![], Some(x(105))).expect("ok"),
    ] {
        assert_err(
            FrameBuilder::new(&schema, cursor_104(), asof),
            "bad_descriptor",
            Some("asof.own"),
            &format!("asof.own: bad descriptor: {reason}"),
        );
    }
    let mut b = FrameBuilder::new(&schema, cursor_104(), writing()).expect("matching frame");
    b.set(T, 7, "seq", FieldValue::Int(1)).expect("set");
    let bytes = text(b.encode().expect("encodes"));
    assert!(
        bytes.contains(r#""cursor":{"id":"5512","xid":"104"}"#),
        "{bytes}"
    );
    assert!(
        bytes.contains(&format!(r#""asof":{ASOF_WRITE}"#)),
        "{bytes}"
    );
    // Ordering is numeric on (xid, id); as strings "9" sorts after "10".
    let c = |xid: &str, id: &str| BusCursor::from_wire_strings(xid, id).expect("cursor");
    assert!("9" > "10");
    assert!(c("9", "7") < c("10", "7"));
    assert!(c("10", "9") < c("10", "10"));
}

#[test]
fn written_markers_are_checked_when_encoded() {
    let schema = schema();
    // No own xid: refused at the call.
    let mut b = answer(&schema);
    assert_err(
        b.mark_written(T, 7, "seq"),
        "bad_written",
        Some("written"),
        "written: bad written marker: answer has no own xid",
    );
    // Counters cannot be written markers.
    let mut b = AnswerBuilder::new(&schema, writing());
    assert_err(
        b.mark_written(T, 7, "unread"),
        "bad_written",
        Some("written.test.thread.7.unread"),
        "written.test.thread.7.unread: bad written marker: counter fields cannot be written markers",
    );
    b.mark_written(T, 7, "seq").expect("marker");
    assert_err(
        b.mark_written(T, 7, "seq"),
        "duplicate_field",
        Some("written.test.thread.7.seq"),
        "written.test.thread.7.seq: duplicate field",
    );
    // A marker for a field the payload does not set is refused at encode.
    assert_err(
        b.encode(),
        "bad_written",
        Some("written.test.thread.7"),
        "written.test.thread.7: bad written marker: record is not in the payload",
    );
    b.set(T, 7, "title", FieldValue::String(s("x")))
        .expect("set");
    assert_err(
        b.encode(),
        "bad_written",
        Some("written.test.thread.7.seq"),
        "written.test.thread.7.seq: bad written marker: field is not set by the entry",
    );
    b.set(T, 7, "seq", FieldValue::Int(1)).expect("set");
    assert_eq!(
        text(b.encode().expect("encodes")),
        format!(
            concat!(
                r#"{{"asof":{},"records":{{"test.thread":[{{"id":7,"seq":1,"title":"x"}}]}},"#,
                r#""schema":"g1","wire":1,"written":{{"test.thread":{{"7":["seq"]}}}}}}"#
            ),
            ASOF_WRITE
        )
    );
}
