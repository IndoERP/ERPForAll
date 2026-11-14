//! GRP cases of slice S5, the onchange protocol (GRP §4.12) and `#[onchange]` methods.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_orm::Spec;
use erp_registry::ModelSpec;
use erp_sdk::RawVals;
use erp_sdk::RecId;
use erp_sdk::ValueMap;
use erp_sdk::serde_json::json;
use erp_sdk::{Domain, Error, ErrorKind, Operation, Records, Result, Transaction, Value};
use erp_test_models::test_web::Multi;
use erp_test_models::{base_users, test_rel, test_web};
use erp_testkit::{Harness, assert_query_count};
use support::map;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .build()
        .await
        .expect("harness")
}

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    let created = records.create_values(tx, &vals).await.unwrap();
    tx.flush().await.unwrap();
    created
}

fn id(records: &Records, i: usize) -> i64 {
    records.ids()[i]
}

fn int(i: i64) -> Value {
    Value::Int(i)
}

fn s(text: &str) -> Value {
    Value::str(text)
}

const F: Value = Value::False;

fn obj(pairs: &[(&str, Value)]) -> Value {
    Value::Map(Arc::new(support::vmap(pairs)))
}

fn v(json: erp_sdk::serde_json::Value) -> Value {
    use erp_sdk::serde_json::Value as J;
    match json {
        J::Null => Value::False,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap())),
        J::String(t) => Value::str(t),
        J::Array(items) => Value::list(items.into_iter().map(v)),
        J::Object(m) => Value::Map(Arc::new(m.into_iter().map(|(k, x)| (k, v(x))).collect())),
    }
}

fn spec(json: erp_sdk::serde_json::Value) -> Spec {
    Spec::from_value(&v(json)).unwrap()
}

fn values(json: erp_sdk::serde_json::Value) -> erp_sdk::ValueMap {
    match v(json) {
        Value::Map(m) => m.as_ref().clone(),
        other => panic!("{other}"),
    }
}

fn names(items: &[&str]) -> Vec<String> {
    items.iter().map(|n| (*n).to_owned()).collect()
}

/// `result['value']`.
fn value_of(result: &Value) -> Value {
    match result {
        Value::Map(m) => m.get("value").cloned().unwrap_or(F),
        other => panic!("{other}"),
    }
}

fn warning_of(result: &Value) -> Option<Value> {
    match result {
        Value::Map(m) => m.get("warning").cloned(),
        other => panic!("{other}"),
    }
}

const MESSAGE: &str = "test_web.message";
const DISCUSSION: &str = "test_web.discussion";

fn message_spec() -> Spec {
    spec(json!({
        "discussion": {},
        "body": {},
        "author": {"fields": {"display_name": {}}},
        "name": {},
        "size": {},
    }))
}

#[tokio::test]
async fn spec_grp_047_onchange_first_call() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let messages = tx.env().records(MESSAGE).unwrap();
    let result = messages
        .onchange(
            &mut tx,
            erp_sdk::ValueMap::new(),
            Vec::new(),
            message_spec(),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[
            ("discussion", F),
            ("body", F),
            (
                "author",
                obj(&[("id", int(1)), ("display_name", s("__system__"))])
            ),
            ("name", s("[] __system__")),
            ("size", int(0)),
        ])
    );
    assert_eq!(warning_of(&result), None);
    // x2many defaults from the context are sent as links
    let person = create(&mut tx, "test_web.person", &[&[("name", s("p"))]]).await;
    let p = id(&person, 0);
    let discussions = tx
        .env()
        .records(DISCUSSION)
        .unwrap()
        .with_context([("default_participants", v(json!([[6, 0, [p]]])))]);
    let result = discussions
        .onchange(
            &mut tx,
            erp_sdk::ValueMap::new(),
            Vec::new(),
            spec(json!({"participants": {}})),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[(
            "participants",
            Value::list([Value::list([int(4), int(p), obj(&[("id", int(p))])])])
        )])
    );
    // the dynamic surface
    let result = messages
        .web_call(
            &mut tx,
            "onchange",
            &obj(&[
                ("values", obj(&[])),
                ("field_names", Value::list([])),
                ("fields_spec", message_spec().to_value()),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(
        match value_of(&result) {
            Value::Map(m) => m.get("name").cloned(),
            _ => None,
        },
        Some(s("[] __system__"))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_048_onchange_returns_only_changes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let discussions = create(
        &mut tx,
        DISCUSSION,
        &[&[("name", s("General"))], &[("name", s("Other"))]],
    )
    .await;
    let (d1, d2) = (id(&discussions, 0), id(&discussions, 1));
    let messages = tx.env().records(MESSAGE).unwrap();
    let base = json!({
        "discussion": d1,
        "body": "hello",
        "author": 1,
        "name": "[General] __system__",
        "size": 5,
    });
    // changing the discussion recomputes the name only
    let mut vals = values(base.clone());
    vals.insert("discussion".to_owned(), int(d2));
    let result = messages
        .onchange(&mut tx, vals, names(&["discussion"]), message_spec())
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("name", s("[Other] __system__"))]));
    // changing the body recomputes the size only
    let mut vals = values(base.clone());
    vals.insert("body".to_owned(), s("hello world"));
    let result = messages
        .onchange(&mut tx, vals, names(&["body"]), message_spec())
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("size", int(11))]));
    // an inconsistent name is not a dependency of the body: not returned
    let mut vals = values(base);
    vals.insert("body".to_owned(), s("hi"));
    vals.insert("name".to_owned(), s("whatever"));
    let result = messages
        .onchange(&mut tx, vals, names(&["body"]), message_spec())
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("size", int(2))]));
    // on an existing record, the stored values are the starting point
    let message = create(
        &mut tx,
        MESSAGE,
        &[&[("discussion", int(d1)), ("body", s("abc"))]],
    )
    .await;
    let result = message
        .onchange(
            &mut tx,
            values(json!({"discussion": d2})),
            names(&["discussion"]),
            message_spec(),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("name", s("[Other] __system__"))]));
    // nothing was written
    assert_eq!(message.value(&mut tx, "discussion").await.unwrap(), int(d1));
    assert!(!tx.is_poisoned());
    drop(tx);
    h.cleanup().await.unwrap();
}

fn discussion_spec() -> Spec {
    spec(json!({
        "name": {},
        "note": {},
        "moderator": {},
        "messages": {"fields": {"body": {}, "size": {}}},
    }))
}

#[tokio::test]
async fn spec_grp_049_onchange_x2many_commands() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let discussion = create(&mut tx, DISCUSSION, &[&[("name", s("General"))]]).await;
    let d = id(&discussion, 0);
    let lines = create(
        &mut tx,
        MESSAGE,
        &[
            &[("discussion", int(d)), ("body", s("one"))],
            &[("discussion", int(d)), ("body", s("two"))],
        ],
    )
    .await;
    let (m1, m2) = (id(&lines, 0), id(&lines, 1));
    // the form holds m2 modified and a new line virtual3; the name changes
    let vals = values(json!({
        "name": "Renamed",
        "messages": [
            [1, m2, {"body": "edited"}],
            [0, "virtual3", {"body": "new line"}],
        ],
    }));
    let result = discussion
        .onchange(&mut tx, vals, names(&["name"]), discussion_spec())
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[(
            "messages",
            Value::list([
                Value::list([
                    int(1),
                    int(m1),
                    obj(&[("body", s("Renamed")), ("size", int(7))])
                ]),
                Value::list([
                    int(1),
                    int(m2),
                    obj(&[("body", s("Renamed")), ("size", int(7))])
                ]),
                Value::list([
                    int(1),
                    s("virtual3"),
                    obj(&[("body", s("Renamed")), ("size", int(7))])
                ]),
            ])
        )])
    );
    // a line removed by the form is a DELETE (one2many); nothing else changed
    let vals = values(json!({"note": "x", "messages": [[2, m1, 0]]}));
    let result = discussion
        .onchange(&mut tx, vals, names(&["note"]), discussion_spec())
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[]));
    // first call of a new record linking database lines: links with the lines' values
    let result = tx
        .env()
        .records(DISCUSSION)
        .unwrap()
        .onchange(
            &mut tx,
            values(json!({"messages": [[4, m1, 0], [4, m2, 0]]})),
            Vec::new(),
            // without `name`, whose onchange method would rewrite the lines' bodies
            spec(json!({"note": {}, "messages": {"fields": {"body": {}, "size": {}}}})),
        )
        .await
        .unwrap();
    let Value::Map(value) = value_of(&result) else {
        panic!()
    };
    assert_eq!(
        value.get("messages"),
        Some(&Value::list([
            Value::list([
                int(4),
                int(m1),
                obj(&[("id", int(m1)), ("body", s("one")), ("size", int(3))])
            ]),
            Value::list([
                int(4),
                int(m2),
                obj(&[("id", int(m2)), ("body", s("two")), ("size", int(3))])
            ]),
        ]))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_050_onchange_methods_and_warnings() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let person = create(&mut tx, "test_web.person", &[&[("name", s("demo"))]]).await;
    let demo = id(&person, 0);
    let discussion = create(&mut tx, DISCUSSION, &[&[("name", s("General"))]]).await;
    let simple = spec(json!({"name": {}, "note": {}, "moderator": {}}));
    // the moderator's onchange method assigns the name
    let result = discussion
        .onchange(
            &mut tx,
            values(json!({"moderator": demo})),
            names(&["moderator"]),
            simple.clone(),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("name", s("Moderated by demo"))]));
    // one warning: type defaults to dialog; returned values are assigned
    let result = discussion
        .onchange(
            &mut tx,
            values(json!({"name": "one warning"})),
            names(&["name"]),
            simple.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        warning_of(&result),
        Some(obj(&[
            ("title", s("Careful")),
            ("message", s("only warning")),
            ("type", s("dialog")),
        ]))
    );
    assert_eq!(value_of(&result), obj(&[("note", s("noted"))]));
    // two warnings: merged under "Warnings", in method-name order
    let result = discussion
        .onchange(
            &mut tx,
            values(json!({"name": "warn"})),
            names(&["name"]),
            simple,
        )
        .await
        .unwrap();
    assert_eq!(
        warning_of(&result),
        Some(obj(&[
            ("title", s("Warnings")),
            (
                "message",
                s("Careful\n\nfirst warning\n\nBeware\n\nsecond warning")
            ),
            ("type", s("dialog")),
        ]))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn onchange_warning_options_survive_deduplication_and_do_not_persist() {
    // Optional browser fixtures come from the actual native result, never a hand-written wire DTO.
    let capture_dir = match std::env::var("WARNINGS_CAPTURE_DIR") {
        Ok(path) => {
            assert!(!path.is_empty(), "WARNINGS_CAPTURE_DIR must not be empty");
            let path = std::path::PathBuf::from(path);
            std::fs::create_dir_all(&path).expect("create WARNINGS_CAPTURE_DIR");
            Some(path)
        }
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("Invalid WARNINGS_CAPTURE_DIR: {error}"),
    };
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let discussion = create(&mut tx, DISCUSSION, &[&[("name", s("General"))]]).await;
    let discussion_id = id(&discussion, 0);
    let simple = spec(json!({"name": {}, "note": {}}));
    for (trigger, expected_warning) in [
        (
            "options-single",
            obj(&[
                ("title", s("Notice")),
                ("message", s("first")),
                ("type", s("notification")),
                ("className", s("addon_notice _urgent-2")),
                ("sticky", Value::Bool(true)),
            ]),
        ),
        (
            "options-single-timed",
            obj(&[
                ("title", s("Notice")),
                ("message", s("first")),
                ("type", s("notification")),
                ("className", s("addon_notice _urgent-2")),
                ("sticky", Value::Bool(false)),
            ]),
        ),
        (
            "options-duplicate",
            obj(&[
                ("title", s("Notice")),
                ("message", s("first")),
                ("type", s("notification")),
                ("className", s("addon_notice")),
            ]),
        ),
        (
            "options-false-distinct",
            obj(&[
                ("title", s("Warnings")),
                ("message", s("Notice\n\nfirst\n\nNotice\n\nfirst")),
                ("type", s("dialog")),
            ]),
        ),
        (
            "options-class-distinct",
            obj(&[
                ("title", s("Warnings")),
                ("message", s("Notice\n\nfirst\n\nNotice\n\nfirst")),
                ("type", s("dialog")),
            ]),
        ),
        (
            "options-ordered",
            obj(&[
                ("title", s("Warnings")),
                ("message", s("Notice\n\nfirst\n\nNotice\n\nsecond")),
                ("type", s("dialog")),
            ]),
        ),
    ] {
        let result = discussion
            .onchange(
                &mut tx,
                values(json!({"name": trigger})),
                names(&["name"]),
                simple.clone(),
            )
            .await
            .unwrap();
        assert_eq!(warning_of(&result), Some(expected_warning), "{trigger}");
        if let Some(dir) = &capture_dir {
            let filename = match trigger {
                "options-single" => Some("single-sticky.json"),
                "options-single-timed" => Some("single-timed.json"),
                _ => None,
            };
            if let Some(filename) = filename {
                let json = erp_fields::convert::value_to_json(&result)
                    .expect("serialize actual onchange result");
                let bytes =
                    erp_sdk::serde_json::to_vec(&json).expect("encode actual onchange JSON");
                std::fs::write(dir.join(filename), bytes)
                    .expect("write actual onchange browser fixture");
            }
        }
        assert_eq!(value_of(&result), obj(&[("note", s("noted"))]), "{trigger}");
        assert_eq!(
            discussion.value(&mut tx, "name").await.unwrap(),
            s("General"),
            "{trigger}"
        );
        assert_eq!(
            discussion.value(&mut tx, "note").await.unwrap(),
            F,
            "{trigger}"
        );
    }
    let error = discussion
        .onchange(
            &mut tx,
            values(json!({"name": "options-invalid"})),
            names(&["name"]),
            simple,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Invalid onchange warning className: tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces"
    );
    assert!(!tx.is_poisoned());
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let discussion = tx
        .env()
        .records(DISCUSSION)
        .unwrap()
        .browse([discussion_id])
        .unwrap();
    assert_eq!(
        discussion.value(&mut tx, "name").await.unwrap(),
        s("General")
    );
    assert_eq!(discussion.value(&mut tx, "note").await.unwrap(), F);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Users may not create messages.
struct NoCreate;

impl erp_testkit::TestRules for NoCreate {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if model.name == MESSAGE && op == Operation::Create {
            return Err(Error::access(
                "Your access rights do not let you create 'Message' records.",
            ));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<Domain> {
        Ok(Domain::TRUE)
    }
}

#[tokio::test]
async fn spec_grp_052_onchange_guards() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(NoCreate)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let as_user = tx.env().with_user(2).records(MESSAGE).unwrap();
    let err = as_user
        .onchange(
            &mut tx,
            erp_sdk::ValueMap::new(),
            Vec::new(),
            message_spec(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    let messages = tx.env().records(MESSAGE).unwrap();
    let result = messages
        .onchange(
            &mut tx,
            values(json!({"body": "x"})),
            names(&["nope"]),
            message_spec(),
        )
        .await
        .unwrap();
    assert_eq!(result, obj(&[]));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_054_onchange_batch() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let messages = tx.env().records(MESSAGE).unwrap();
    let list = vec![
        values(json!({"body": "a"})),
        values(json!({"body": "bb"})),
        values(json!({"body": "ccc"})),
    ];
    let batch = messages
        .onchange_batch(&mut tx, list.clone(), Vec::new(), message_spec())
        .await
        .unwrap();
    for (vals, got) in list.into_iter().zip(batch) {
        let one = messages
            .onchange(&mut tx, vals, Vec::new(), message_spec())
            .await
            .unwrap();
        assert_eq!(got, one);
    }
    let record = create(&mut tx, MESSAGE, &[&[("body", s("x"))]]).await;
    let err = record
        .onchange_batch(
            &mut tx,
            vec![erp_sdk::ValueMap::new()],
            Vec::new(),
            message_spec(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// GRP §8 W5: the lines are read once and the result does not cost one query per line: the same
/// onchange on a discussion with 3 and with 30 lines issues the same number of statements.
#[tokio::test]
async fn onchange_cost_does_not_grow_with_lines() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut counts = Vec::new();
    for n in [3usize, 30] {
        let discussion = create(&mut tx, DISCUSSION, &[&[("name", s("d"))]]).await;
        let d = id(&discussion, 0);
        let rows: Vec<Vec<(&str, Value)>> = (0..n)
            .map(|i| {
                vec![
                    ("discussion", int(d)),
                    ("body", Value::str(format!("line {i}"))),
                ]
            })
            .collect();
        let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
        create(&mut tx, MESSAGE, &refs).await;
        tx.invalidate_all().await.unwrap();
        let before = tx.query_count();
        let result = discussion
            .onchange(
                &mut tx,
                values(json!({"note": "changed"})),
                names(&["note"]),
                discussion_spec(),
            )
            .await
            .unwrap();
        counts.push(tx.query_count() - before);
        assert_eq!(value_of(&result), obj(&[]));
    }
    assert_eq!(counts[0], counts[1], "{counts:?}");
    // two reads (the record, its lines); no SAVEPOINT when it only reads
    assert!(counts[0] <= 3, "{counts:?}");
    // budget with the lower bound: it reads something
    let discussion = create(&mut tx, DISCUSSION, &[&[("name", s("e"))]]).await;
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, >= 2, <= 3, {
        discussion
            .onchange(
                &mut tx,
                values(json!({"note": "changed"})),
                names(&["note"]),
                discussion_spec(),
            )
            .await
            .unwrap()
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 1: removing a line the form created removes that line only (two client lines
/// and one database line).
#[tokio::test]
async fn onchange_remove_command_targets_one_line() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let discussion = create(&mut tx, DISCUSSION, &[&[("name", s("General"))]]).await;
    let d = id(&discussion, 0);
    let lines = create(
        &mut tx,
        MESSAGE,
        &[&[("discussion", int(d)), ("body", s("one"))]],
    )
    .await;
    let m1 = id(&lines, 0);
    let result = tx
        .env()
        .records(DISCUSSION)
        .unwrap()
        .onchange(
            &mut tx,
            values(json!({"messages": [
                [4, m1, 0],
                [0, "v1", {"body": "a"}],
                [0, "v2", {"body": "b"}],
                [2, "v1", 0],
            ]})),
            Vec::new(),
            spec(json!({"note": {}, "messages": {"fields": {"body": {}}}})),
        )
        .await
        .unwrap();
    let Value::Map(value) = value_of(&result) else {
        panic!()
    };
    assert_eq!(
        value.get("messages"),
        Some(&Value::list([
            Value::list([int(4), int(m1), obj(&[("id", int(m1)), ("body", s("one"))])]),
            Value::list([int(0), s("v2"), obj(&[("body", s("b"))])]),
        ]))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// an onchange leaves the cache as it was (values cached before stay cached), sends
/// no savepoint when it only reads, and when one of its methods writes and flushes, a savepoint
/// opened just before that flush is rolled back: the method's write never reaches the database
/// and the cache shows the values from before the call.
#[tokio::test]
async fn onchange_keeps_the_cache_and_discards_its_changes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let person = create(
        &mut tx,
        "test_web.person",
        &[&[("name", s("mod")), ("street", s("old"))]],
    )
    .await;
    let p = id(&person, 0);
    let discussion = create(
        &mut tx,
        DISCUSSION,
        &[&[("name", s("General")), ("moderator", int(p))]],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    assert_eq!(person.value(&mut tx, "street").await.unwrap(), s("old"));
    assert_eq!(
        discussion.value(&mut tx, "name").await.unwrap(),
        s("General")
    );

    // read-only onchange: no savepoint, and the cache is warm afterwards
    tx.capture_statements(true);
    let result = assert_query_count!(tx, >= 1, <= 3, {
        discussion
            .onchange(
                &mut tx,
                values(json!({"note": "changed"})),
                names(&["note"]),
                discussion_spec(),
            )
            .await
            .unwrap()
    });
    assert_eq!(value_of(&result), obj(&[]));
    assert!(
        tx.captured().iter().all(|q| !q.contains("SAVEPOINT")),
        "{:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    assert_query_count!(tx, <= 0, {
        assert_eq!(person.value(&mut tx, "street").await.unwrap(), s("old"));
        assert_eq!(
            discussion.value(&mut tx, "name").await.unwrap(),
            s("General")
        );
    });

    // a pending write of the caller, then an onchange whose method writes the moderator and
    // flushes: the caller's write is flushed before the onchange, the method's inside a savepoint
    person
        .set_value(&mut tx, "name", s("pending"))
        .await
        .unwrap();
    tx.capture_statements(true);
    discussion
        .onchange(
            &mut tx,
            values(json!({"note": "persist"})),
            names(&["note"]),
            discussion_spec(),
        )
        .await
        .unwrap();
    let captured = tx.captured().to_vec();
    tx.capture_statements(false);
    let savepoint = captured
        .iter()
        .position(|q| q.starts_with("SAVEPOINT"))
        .expect("a savepoint");
    let updates: Vec<usize> = captured
        .iter()
        .enumerate()
        .filter(|(_, q)| q.starts_with("UPDATE"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(updates.len(), 2, "{captured:?}");
    assert!(
        updates[0] < savepoint && savepoint < updates[1],
        "{captured:?}"
    );
    assert!(
        captured
            .last()
            .is_some_and(|q| q.starts_with("ROLLBACK TO SAVEPOINT")),
        "{captured:?}"
    );
    // the cache is as before the call (the caller's write included): no statement
    assert_query_count!(tx, <= 0, {
        assert_eq!(person.value(&mut tx, "street").await.unwrap(), s("old"));
        assert_eq!(person.value(&mut tx, "name").await.unwrap(), s("pending"));
    });
    assert!(!tx.is_dirty(&person, "name").unwrap());
    assert!(!tx.is_poisoned());
    tx.commit().await.unwrap();
    // the caller's write is in the database, the onchange's is not
    assert_eq!(person.value(&mut tx, "name").await.unwrap(), s("pending"));
    assert_eq!(person.value(&mut tx, "street").await.unwrap(), s("old"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// (review H1): statements that change data without being INSERT/UPDATE/DELETE —
/// `TRUNCATE`, `CALL` of a procedure that writes — open the onchange's savepoint too, so nothing
/// they did survives the onchange.
#[tokio::test]
async fn onchange_isolation_covers_truncate_and_call() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let person = create(
        &mut tx,
        "test_web.person",
        &[&[("name", s("mod")), ("street", s("old"))]],
    )
    .await;
    let p = id(&person, 0);
    let discussion = create(
        &mut tx,
        DISCUSSION,
        &[&[("name", s("General")), ("moderator", int(p))]],
    )
    .await;
    let d = id(&discussion, 0);
    create(
        &mut tx,
        MESSAGE,
        &[&[("discussion", int(d)), ("body", s("m"))]],
    )
    .await;
    tx.batch(
        "CREATE PROCEDURE test_web_touch_people() LANGUAGE sql AS \
         $$ UPDATE test_web_person SET street = 'called' $$",
    )
    .await
    .unwrap();
    let simple = spec(json!({"name": {}, "note": {}, "moderator": {}}));
    for note in ["truncate", "call"] {
        tx.capture_statements(true);
        discussion
            .onchange(
                &mut tx,
                values(json!({ "note": note })),
                names(&["note"]),
                simple.clone(),
            )
            .await
            .unwrap();
        let captured = tx.captured().to_vec();
        tx.capture_statements(false);
        assert!(
            captured
                .iter()
                .any(|q| q.starts_with("SAVEPOINT \"erp_iso_")),
            "{note}: {captured:?}"
        );
        assert_eq!(
            support::one(&mut tx, "SELECT count(*)::text FROM test_web_message")
                .await
                .as_deref(),
            Some("1"),
            "{note}"
        );
        assert_eq!(
            support::one(
                &mut tx,
                &format!("SELECT street FROM test_web_person WHERE id = {p}")
            )
            .await
            .as_deref(),
            Some("old"),
            "{note}"
        );
    }
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// an explicit savepoint released inside an onchange is covered by the onchange's
/// own savepoint, sent first, so what it wrote is rolled back with the onchange; a commit inside
/// an onchange is refused and leaves the transaction usable.
#[tokio::test]
async fn onchange_isolation_covers_nested_savepoints_and_refuses_commit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let person = create(
        &mut tx,
        "test_web.person",
        &[&[("name", s("mod")), ("street", s("old"))]],
    )
    .await;
    let p = id(&person, 0);
    let discussion = create(
        &mut tx,
        DISCUSSION,
        &[&[("name", s("General")), ("moderator", int(p))]],
    )
    .await;
    let simple = spec(json!({"name": {}, "note": {}, "moderator": {}}));
    tx.capture_statements(true);
    discussion
        .onchange(
            &mut tx,
            values(json!({"note": "nested"})),
            names(&["note"]),
            simple.clone(),
        )
        .await
        .unwrap();
    let captured = tx.captured().to_vec();
    tx.capture_statements(false);
    let outer = captured
        .iter()
        .position(|q| q.starts_with("SAVEPOINT \"erp_iso_"))
        .expect("the onchange's savepoint");
    let inner = captured
        .iter()
        .position(|q| q.starts_with("SAVEPOINT \"erp_sp_"))
        .expect("the method's savepoint");
    assert!(outer < inner, "{captured:?}");
    assert!(
        captured
            .last()
            .is_some_and(|q| q.starts_with("ROLLBACK TO SAVEPOINT \"erp_iso_")),
        "{captured:?}"
    );
    assert_eq!(
        support::one(
            &mut tx,
            &format!("SELECT street FROM test_web_person WHERE id = {p}")
        )
        .await
        .as_deref(),
        Some("old")
    );
    let err = discussion
        .onchange(
            &mut tx,
            values(json!({"note": "commit"})),
            names(&["note"]),
            simple,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("isolated"), "{}", err.message());
    assert!(!tx.is_poisoned());
    assert_eq!(person.value(&mut tx, "street").await.unwrap(), s("old"));
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Lead decision G4: whatever an onchange method does, nothing persists (the computation is
/// isolated); errors inside leave the transaction usable.
#[tokio::test]
async fn onchange_never_persists() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let person = create(&mut tx, "test_web.person", &[&[("name", s("mod"))]]).await;
    let p = id(&person, 0);
    let discussion = create(
        &mut tx,
        DISCUSSION,
        &[&[("name", s("General")), ("moderator", int(p))]],
    )
    .await;
    let simple = spec(json!({"name": {}, "note": {}, "moderator": {}}));
    discussion
        .onchange(
            &mut tx,
            values(json!({"note": "persist"})),
            names(&["note"]),
            simple.clone(),
        )
        .await
        .unwrap();
    assert_eq!(person.value(&mut tx, "street").await.unwrap(), F);
    // review item 9: an onchange method returning a value for an unknown field is a programming
    // error naming the method and the key
    let err = discussion
        .onchange(
            &mut tx,
            values(json!({"name": "bad key"})),
            names(&["name"]),
            simple,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains("_onchange_name_warning") && err.message().contains("'nope'"),
        "{}",
        err.message()
    );
    assert!(!tx.is_poisoned());
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 2: client input the protocol refuses is a `Validation` error (422).
#[tokio::test]
async fn onchange_client_errors_are_validation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let discussions = create(
        &mut tx,
        DISCUSSION,
        &[&[("name", s("a"))], &[("name", s("b"))]],
    )
    .await;
    let err = discussions
        .onchange(
            &mut tx,
            ValueMap::new(),
            Vec::new(),
            spec(json!({"name": {}})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    let one = discussions.at(0).unwrap();
    let err = one
        .onchange(
            &mut tx,
            values(json!({"note": "x", "messages": [[0, "v1", {"discussion": 1, "body": "a", "important": true}]]})),
            names(&["note"]),
            spec(json!({"note": {}, "messages": {"fields": {"body": {}}}})),
        )
        .await;
    assert!(err.is_ok(), "{err:?}");
    // a line value for a field the comodel does not have
    let persons = tx.env().records("test_web.person").unwrap();
    let err = tx
        .env()
        .records(DISCUSSION)
        .unwrap()
        .onchange(
            &mut tx,
            values(json!({"messages": [[0, "v1", {"body": "a", "nope_x2many": []}]]})),
            Vec::new(),
            spec(json!({"messages": {"fields": {"body": {}}}})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{}", err.message());
    let _ = persons;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 12: a first call linking 300 lines stays within the budget (no per-line query,
/// no quadratic scans).
#[tokio::test]
async fn onchange_force_with_300_lines_within_budget() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..300)
        .map(|i| vec![("body", Value::str(format!("line {i}")))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let lines = create(&mut tx, MESSAGE, &refs).await;
    tx.invalidate_all().await.unwrap();
    let discussions = tx.env().records(DISCUSSION).unwrap();
    let ids: Vec<Value> = lines.ids().into_iter().map(int).collect();
    let vals = support::vmap(&[(
        "messages",
        Value::list([Value::list([int(6), int(0), Value::list(ids)])]),
    )]);
    let result = assert_query_count!(tx, >= 1, <= 4, {
        discussions
            .onchange(
                &mut tx,
                vals,
                Vec::new(),
                spec(json!({"note": {}, "messages": {"fields": {"body": {}, "size": {}}}})),
            )
            .await
            .unwrap()
    });
    let Value::Map(value) = value_of(&result) else {
        panic!()
    };
    let Some(Value::List(commands)) = value.get("messages") else {
        panic!()
    };
    assert_eq!(commands.len(), 300);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A form on `test_web.compute_onchange`: the client's values, updated with each onchange result.
async fn form_step(
    tx: &mut Transaction,
    records: &Records,
    form: &mut ValueMap,
    changed: &[&str],
    spec: &Spec,
) {
    let result = records
        .onchange(tx, form.clone(), names(changed), spec.clone())
        .await
        .unwrap();
    if let Value::Map(value) = value_of(&result) {
        for (k, v) in value.iter() {
            form.insert(k.clone(), v.clone());
        }
    }
}

#[tokio::test]
async fn spec_grp_051_onchange_passes_and_protection() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_web.compute_onchange").unwrap();
    let spec = spec(json!({"active": {}, "foo": {}, "bar": {}, "baz": {}}));
    let mut form = ValueMap::new();
    form_step(&mut tx, &model, &mut form, &[], &spec).await;
    let get = |form: &ValueMap, k: &str| form.get(k).cloned().unwrap_or(F);
    assert_eq!((get(&form, "bar"), get(&form, "baz")), (s("r"), F));
    let set = async |tx: &mut Transaction, form: &mut ValueMap, k: &str, v: Value| {
        form.insert(k.to_owned(), v);
        form_step(tx, &model, form, &[k], &spec).await;
    };
    set(&mut tx, &mut form, "active", Value::Bool(true)).await;
    assert_eq!((get(&form, "bar"), get(&form, "baz")), (s("r"), s("z")));
    let calls = test_web::FOO_ONCHANGE_CALLS.load(std::sync::atomic::Ordering::SeqCst);
    set(&mut tx, &mut form, "foo", s("foo1")).await;
    // the field-bound onchange method runs once per change
    assert_eq!(
        test_web::FOO_ONCHANGE_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        calls + 1
    );
    assert_eq!(
        (get(&form, "bar"), get(&form, "baz")),
        (s("foo1r"), s("foo1z"))
    );
    // the user overrides baz: it stays while active is false
    set(&mut tx, &mut form, "baz", s("baz2")).await;
    assert_eq!(
        (get(&form, "bar"), get(&form, "baz")),
        (s("foo1r"), s("baz2"))
    );
    set(&mut tx, &mut form, "active", Value::Bool(false)).await;
    assert_eq!(
        (get(&form, "bar"), get(&form, "baz")),
        (s("foo1r"), s("baz2"))
    );
    set(&mut tx, &mut form, "foo", s("foo3")).await;
    assert_eq!(
        (get(&form, "bar"), get(&form, "baz")),
        (s("foo3r"), s("baz2"))
    );
    set(&mut tx, &mut form, "active", Value::Bool(true)).await;
    assert_eq!(
        (get(&form, "bar"), get(&form, "baz")),
        (s("foo3r"), s("foo3z"))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `#[onchange]` declarations are checked when the ORM is built and their methods are ordinary
/// methods of the chain.
#[tokio::test]
async fn onchange_methods_are_model_methods() {
    let h = harness().await;
    let model = h.registry().model_id(DISCUSSION).unwrap();
    let chain = h
        .orm()
        .dispatch()
        .implementations(model, "_onchange_moderator");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].addon, "test_web");
    let onchange = h.orm().dispatch().implementations(model, "onchange");
    assert_eq!((onchange[0].addon, onchange[0].model), ("web", "base"));
    h.cleanup().await.unwrap();
}

/// A person moderating a discussion, committed; the transaction continues.
async fn moderated(tx: &mut Transaction) -> (Records, i64) {
    let person = create(
        tx,
        "test_web.person",
        &[&[("name", s("mod")), ("street", s("old"))]],
    )
    .await;
    let p = id(&person, 0);
    let discussion = create(
        tx,
        DISCUSSION,
        &[&[("name", s("General")), ("moderator", int(p))]],
    )
    .await;
    tx.commit().await.unwrap();
    (discussion, p)
}

async fn note_onchange(tx: &mut Transaction, discussion: &Records, note: &str) -> Result<Value> {
    discussion
        .onchange(
            tx,
            values(json!({ "note": note })),
            names(&["note"]),
            spec(json!({"name": {}, "note": {}, "moderator": {}})),
        )
        .await
}

/// Review M4: the error paths of an onchange's isolation.
/// - A server error on a read before anything changed aborts the transaction (no savepoint covers
///   it); rollback recovers.
/// - A unique violation after the savepoint was opened is rolled back with it: the transaction
///   stays usable and unpoisoned, nothing the method wrote remains.
/// - A cancelled onchange (future dropped inside the scope) poisons the transaction.
/// - When the scope's own `ROLLBACK TO SAVEPOINT` fails, the transaction is released.
#[tokio::test]
async fn onchange_isolation_error_paths() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (discussion, p) = moderated(&mut tx).await;
    let street = format!("SELECT street FROM test_web_person WHERE id = {p}");

    let err = note_onchange(&mut tx, &discussion, "bad read")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), Some("22012"), "{err:?}");
    let next = tx.query("SELECT 1", &[]).await.unwrap_err();
    assert_eq!(next.sqlstate(), Some("22012"), "aborted: {next:?}");
    tx.rollback().await.unwrap();

    let err = note_onchange(&mut tx, &discussion, "duplicate")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), Some("23505"), "{err:?}");
    assert!(!tx.is_poisoned());
    assert_eq!(support::one(&mut tx, &street).await.as_deref(), Some("old"));
    tx.commit().await.unwrap();

    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        note_onchange(&mut tx, &discussion, "slow"),
    )
    .await;
    assert!(cancelled.is_err(), "the onchange should still be running");
    assert!(tx.is_poisoned());
    let err = tx.query("SELECT 1", &[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    assert_eq!(support::one(&mut tx, &street).await.as_deref(), Some("old"));
    let err = note_onchange(&mut tx, &discussion, "raw rollback")
        .await
        .unwrap_err();
    assert!(err.message().contains("released"), "{}", err.message());
    assert!(tx.is_released());
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(support::one(&mut tx, &street).await.as_deref(), Some("old"));
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- the mirror many2many of in-memory lines --------

const BOARD: &str = "test_web.board";
const CATEGORY: &str = "test_web.category";

/// Categories C1, linked to the saved board B1, and C3, linked to another board B5:
/// `(B1, C1, C3)`.
async fn board_fixture(tx: &mut Transaction) -> (Records, i64, i64) {
    let categories = create(tx, CATEGORY, &[&[("name", s("C1"))], &[("name", s("C3"))]]).await;
    let (c1, c3) = (id(&categories, 0), id(&categories, 1));
    let b1 = create(
        tx,
        BOARD,
        &[&[
            ("name", s("B1")),
            ("category_ids", v(json!([[6, 0, [c1]]]))),
        ]],
    )
    .await;
    create(
        tx,
        BOARD,
        &[&[
            ("name", s("B5")),
            ("category_ids", v(json!([[6, 0, [c3]]]))),
        ]],
    )
    .await;
    (b1, c1, c3)
}

/// A new form's categories: a created line and C3 linked.
fn created_and_linked(c3: i64) -> ValueMap {
    values(json!({"category_ids": [[0, "virtual_1", {"name": "New"}], [4, c3, 0]]}))
}

fn board_spec() -> Spec {
    spec(json!({
        "name": {},
        "category_ids": {"fields": {"name": {}, "board_count": {}}},
        "category_links": {},
    }))
}

/// The commands a new form gets back for [`created_and_linked`]: the created line counts the
/// form's record; C3 keeps the stored count copied before the record was built.
fn created_and_linked_commands(c3: i64) -> Value {
    Value::list([
        Value::list([
            int(0),
            s("virtual_1"),
            obj(&[("name", s("New")), ("board_count", int(1))]),
        ]),
        Value::list([
            int(4),
            int(c3),
            obj(&[("id", int(c3)), ("name", s("C3")), ("board_count", int(1))]),
        ]),
    ])
}

#[tokio::test]
async fn spec_grp_063_lines_list_the_form_record_in_their_mirror() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (b1, c1, c3) = board_fixture(&mut tx).await;
    // (a) first call on a new board: the created line lists the form's record, C3 lists B5 and it
    let result = tx
        .env()
        .records(BOARD)
        .unwrap()
        .onchange(&mut tx, created_and_linked(c3), vec![], board_spec())
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[
            ("name", F),
            ("category_ids", created_and_linked_commands(c3)),
            ("category_links", int(3)),
        ])
    );
    // (b) the saved board links C3: C1 lists B1's in-memory record once (the in-memory record of
    // its stored link), C3 lists B5 and it; the changed `category_ids` is part of snapshot 0, so
    // it is not returned
    let b1_spec = spec(json!({
        "name": {},
        "category_ids": {"fields": {"name": {}}},
        "category_links": {},
    }));
    let result = b1
        .onchange(
            &mut tx,
            values(json!({"category_ids": [[4, c3, 0]]})),
            names(&["category_ids"]),
            b1_spec.clone(),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("category_links", int(3))]));
    // (c) unlinking C1 leaves no line to count
    let result = b1
        .onchange(
            &mut tx,
            values(json!({"category_ids": [[3, c1, 0]]})),
            names(&["category_ids"]),
            b1_spec,
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("category_links", int(0))]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The links an onchange puts in the lines' mirrors end with it: the same first call
/// twice in one transaction gives the same values (not one more board per call), and C3's
/// in-memory record afterwards lists its stored board only.
#[tokio::test]
async fn mirror_links_do_not_outlive_the_onchange() {
    use erp_sdk::RawVals;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, _, c3) = board_fixture(&mut tx).await;
    let boards = tx.env().records(BOARD).unwrap();
    let first = boards
        .onchange(&mut tx, created_and_linked(c3), vec![], board_spec())
        .await
        .unwrap();
    let second = boards
        .onchange(&mut tx, created_and_linked(c3), vec![], board_spec())
        .await
        .unwrap();
    assert_eq!(
        map_of(&value_of(&first)).get("category_links"),
        Some(&int(3))
    );
    assert_eq!(value_of(&second), value_of(&first));
    let categories = tx.env().records(CATEGORY).unwrap();
    let origin = categories.browse([c3]).unwrap().rec_ids()[0];
    let in_memory = tx
        .new_record(&categories, RawVals::new(), Some(origin), None)
        .await
        .unwrap();
    assert_eq!(
        in_memory.value(&mut tx, "board_count").await.unwrap(),
        int(1)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn line_linked_through_two_many2many_keeps_its_mirror() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, _, c3) = board_fixture(&mut tx).await;
    let result = tx
        .env()
        .records(BOARD)
        .unwrap()
        .onchange(
            &mut tx,
            values(json!({
                "name": "New",
                "category_ids": [[4, c3, 0]],
                "featured_ids": [[4, c3, 0]],
            })),
            names(&["featured_ids"]),
            spec(json!({
                "name": {},
                "category_ids": {},
                "featured_ids": {},
                "category_links": {},
                "featured_links": {},
            })),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("featured_links", int(2))]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The lines of a delegated many2many list the in-memory parent, like the form's own lines: a new
/// board note's first call.
#[tokio::test]
async fn delegated_many2many_lines_list_the_in_memory_parent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, _, c3) = board_fixture(&mut tx).await;
    let result = tx
        .env()
        .records("test_web.board_note")
        .unwrap()
        .onchange(
            &mut tx,
            created_and_linked(c3),
            vec![],
            spec(json!({
                "note": {},
                "category_ids": {"fields": {"name": {}, "board_count": {}}},
                "category_links": {},
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[
            ("note", F),
            ("category_ids", created_and_linked_commands(c3)),
            ("category_links", int(3)),
        ])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Linking the lines back costs no statement per line: a first call with 2 and with 8 created
/// and 2 and 8 linked categories issues the same statements.
#[tokio::test]
async fn mirror_links_cost_does_not_grow_with_lines() {
    let h = harness().await;
    let mut counts = Vec::new();
    for n in [2_i64, 8] {
        let mut tx = h.begin().await.unwrap();
        let rows: Vec<Vec<(&str, Value)>> = (0..n)
            .map(|i| vec![("name", Value::str(format!("C{i}")))])
            .collect();
        let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
        let linked = create(&mut tx, CATEGORY, &refs).await.ids();
        create(
            &mut tx,
            BOARD,
            &[&[
                ("name", s("B5")),
                ("category_ids", v(json!([[6, 0, linked]]))),
            ]],
        )
        .await;
        let mut commands: Vec<erp_sdk::serde_json::Value> = (0..n)
            .map(|i| json!([0, format!("virtual_{i}"), {"name": format!("N{i}")}]))
            .collect();
        commands.extend(linked.iter().map(|c| json!([4, c, 0])));
        tx.invalidate_all().await.unwrap();
        let before = tx.query_count();
        let result = tx
            .env()
            .records(BOARD)
            .unwrap()
            .onchange(
                &mut tx,
                values(json!({"category_ids": commands})),
                vec![],
                board_spec(),
            )
            .await
            .unwrap();
        counts.push(tx.query_count() - before);
        // each created line lists the form's record, each linked one B5 and the form's record
        assert_eq!(
            map_of(&value_of(&result)).get("category_links"),
            Some(&int(3 * n))
        );
        drop(tx);
    }
    assert_eq!(
        counts[0], counts[1],
        "queries for 2 and 8 lines: {counts:?}"
    );
    // the linked lines' stored values, their stored boards
    assert_eq!(counts[0], 2, "{counts:?}");
    h.cleanup().await.unwrap();
}

// ----- onchange on `_inherits` fields --------------

const PAYMENT: &str = "test_web.payment";
const MOVE: &str = "test_web.move";

fn payment_spec() -> Spec {
    spec(json!({
        "move_id": {},
        "tag_id": {},
        "tag_name": {},
        "tag_repeat": {},
        "tag_string": {},
    }))
}

/// Tags Foo and Bar.
async fn tags(tx: &mut Transaction) -> (i64, i64) {
    let tags = create(
        tx,
        "test_web.multi_tag",
        &[&[("name", s("Foo"))], &[("name", s("Bar"))]],
    )
    .await;
    (id(&tags, 0), id(&tags, 1))
}

/// A map value as an owned map.
fn map_of(value: &Value) -> ValueMap {
    match value {
        Value::Map(m) => m.as_ref().clone(),
        other => panic!("not a map: {other}"),
    }
}

/// The form's values after an onchange: `before` updated with the returned values.
fn merged(before: &ValueMap, result: &Value) -> ValueMap {
    let mut out = before.clone();
    out.extend(map_of(&value_of(result)));
    out
}

async fn move_row(tx: &mut Transaction, move_id: i64) -> (Option<i64>, i32) {
    let rows = tx
        .query(
            "SELECT tag_id, tag_repeat FROM test_web_move WHERE id = $1",
            &[&move_id],
        )
        .await
        .unwrap();
    (rows[0].get(0), rows[0].get(1))
}

/// setting the delegated `tag_id` recomputes the delegated related `tag_name`, then `tag_repeat`
/// the parent's `tag_string`)
#[tokio::test]
async fn spec_grp_053_new_child_delegated_change() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, _) = tags(&mut tx).await;
    let payments = tx.env().records(PAYMENT).unwrap();
    let first = payments
        .onchange(&mut tx, ValueMap::new(), vec![], payment_spec())
        .await
        .unwrap();
    let form = map_of(&value_of(&first));
    assert_eq!(form["tag_name"], F, "{form:?}");
    assert_eq!(form["tag_repeat"], int(0), "{form:?}");

    let mut sent = form.clone();
    sent.insert("tag_id".into(), int(foo));
    let result = payments
        .onchange(&mut tx, sent.clone(), names(&["tag_id"]), payment_spec())
        .await
        .unwrap();
    // only the recomputed delegated value comes back ('' * 0 is unchanged)
    assert_eq!(value_of(&result), obj(&[("tag_name", s("Foo"))]));

    let mut sent = merged(&sent, &result);
    sent.insert("tag_repeat".into(), int(2));
    let result = payments
        .onchange(
            &mut tx,
            sent.clone(),
            names(&["tag_repeat"]),
            payment_spec(),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("tag_string", s("FooFoo"))]));

    // saving the form's values persists them on a new parent
    let saved = create(
        &mut tx,
        PAYMENT,
        &[&[("tag_id", int(foo)), ("tag_repeat", int(2))]],
    )
    .await;
    assert_eq!(saved.value(&mut tx, "tag_name").await.unwrap(), s("Foo"));
    assert_eq!(
        saved.value(&mut tx, "tag_string").await.unwrap(),
        s("FooFoo")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// the onchange goes through an in-memory parent and never changes the database parent; two
/// onchanges in a row agree)
#[tokio::test]
async fn spec_grp_053_existing_child_delegated_change() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, bar) = tags(&mut tx).await;
    let payment = create(
        &mut tx,
        PAYMENT,
        &[&[("tag_id", int(foo)), ("tag_repeat", int(2))]],
    )
    .await;
    let move_id = match payment.value(&mut tx, "move_id").await.unwrap() {
        Value::Int(i) => i,
        other => panic!("{other}"),
    };
    let form = values(json!({
        "move_id": move_id,
        "tag_id": bar,
        "tag_name": "Foo",
        "tag_repeat": 2,
        "tag_string": "FooFoo",
    }));
    for _ in 0..2 {
        let result = payment
            .onchange(&mut tx, form.clone(), names(&["tag_id"]), payment_spec())
            .await
            .unwrap();
        assert_eq!(
            value_of(&result),
            obj(&[("tag_name", s("Bar")), ("tag_string", s("BarBar"))])
        );
    }
    let mut sent = form.clone();
    sent.insert("tag_name".into(), s("Bar"));
    sent.insert("tag_string".into(), s("BarBar"));
    sent.insert("tag_repeat".into(), int(3));
    let result = payment
        .onchange(&mut tx, sent, names(&["tag_repeat"]), payment_spec())
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("tag_string", s("BarBarBar"))]));

    // nothing reached the records or the database
    assert_eq!(payment.value(&mut tx, "tag_id").await.unwrap(), int(foo));
    assert_eq!(
        payment.value(&mut tx, "tag_string").await.unwrap(),
        s("FooFoo")
    );
    tx.flush().await.unwrap();
    assert_eq!(move_row(&mut tx, move_id).await, (Some(foo), 2));

    // saving persists through the parent
    payment
        .write_values(
            &mut tx,
            &map(&[("tag_id", int(bar)), ("tag_repeat", int(3))]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(move_row(&mut tx, move_id).await, (Some(bar), 3));
    assert_eq!(
        payment.value(&mut tx, "tag_string").await.unwrap(),
        s("BarBarBar")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// so the parent's compute uses the form's repeat 3, not the stored 1)
#[tokio::test]
async fn spec_grp_053_unchanged_delegated_form_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, bar) = tags(&mut tx).await;
    let payment = create(
        &mut tx,
        PAYMENT,
        &[&[("tag_id", int(foo)), ("tag_repeat", int(1))]],
    )
    .await;
    let form = values(json!({
        "tag_id": bar,
        "tag_name": "Foo",
        "tag_repeat": 3,
        "tag_string": "FooFooFoo",
    }));
    let result = payment
        .onchange(&mut tx, form, names(&["tag_id"]), payment_spec())
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[("tag_name", s("Bar")), ("tag_string", s("BarBarBar"))])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// compute sees them; a value the form sends, zero included, wins over the default)
#[tokio::test]
async fn spec_grp_053_first_call_delegated_defaults() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, _) = tags(&mut tx).await;
    let payments = tx
        .env()
        .records(PAYMENT)
        .unwrap()
        .with_context([("default_tag_repeat", int(2)), ("default_tag_id", int(foo))]);
    let result = payments
        .onchange(&mut tx, ValueMap::new(), vec![], payment_spec())
        .await
        .unwrap();
    let value = map_of(&value_of(&result));
    assert_eq!(value["tag_id"], int(foo), "{value:?}");
    assert_eq!(value["tag_repeat"], int(2), "{value:?}");
    assert_eq!(value["tag_name"], s("Foo"), "{value:?}");
    assert_eq!(value["tag_string"], s("FooFoo"), "{value:?}");

    let result = payments
        .onchange(
            &mut tx,
            values(json!({"tag_repeat": 0})),
            vec![],
            payment_spec(),
        )
        .await
        .unwrap();
    let value = map_of(&value_of(&result));
    assert_eq!(value["tag_repeat"], int(0), "{value:?}");
    assert_eq!(value["tag_name"], s("Foo"), "{value:?}");
    assert_eq!(value["tag_string"], s(""), "{value:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// computes through an in-memory parent)
#[tokio::test]
async fn spec_grp_053_absent_or_false_delegate_link() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, _) = tags(&mut tx).await;
    let payments = tx.env().records(PAYMENT).unwrap();
    for form in [
        values(json!({"tag_id": foo, "tag_repeat": 1})),
        values(json!({"move_id": false, "tag_id": foo, "tag_repeat": 1})),
    ] {
        let result = payments
            .onchange(&mut tx, form, names(&["tag_repeat"]), payment_spec())
            .await
            .unwrap();
        let value = map_of(&value_of(&result));
        assert_eq!(value.get("tag_string"), Some(&s("Foo")), "{value:?}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// parent's aggregate over its payments sees the unsaved payment's amount, 0 → 10 → 0)
#[tokio::test]
async fn spec_grp_053_child_amount_to_parent_aggregate() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let payments = tx.env().records(PAYMENT).unwrap();
    let fields = spec(json!({"amount": {}, "payment_amount": {}}));
    let first = payments
        .onchange(&mut tx, ValueMap::new(), vec![], fields.clone())
        .await
        .unwrap();
    assert_eq!(
        value_of(&first),
        obj(&[("amount", int(0)), ("payment_amount", int(0))])
    );
    let mut form = map_of(&value_of(&first));
    for (amount, expected) in [(10, 10), (0, 0)] {
        form.insert("amount".into(), int(amount));
        let result = payments
            .onchange(&mut tx, form.clone(), names(&["amount"]), fields.clone())
            .await
            .unwrap();
        assert_eq!(
            value_of(&result),
            obj(&[("payment_amount", int(expected))]),
            "amount {amount}"
        );
        form = merged(&form, &result);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// line of a move form sets the delegated `tag_repeat`; its parent is that move, and saving keeps
/// it)
#[tokio::test]
async fn spec_grp_053_delegated_value_in_one2many_line() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, _) = tags(&mut tx).await;
    let mv = create(&mut tx, MOVE, &[&[("tag_id", int(foo))]]).await;
    let fields = spec(json!({
        "tag_string": {},
        "payment_ids": {"fields": {"tag_repeat": {}}},
    }));
    let result = mv
        .onchange(
            &mut tx,
            values(json!({
                "tag_string": "",
                "payment_ids": [[0, "virtual_1", {"tag_repeat": 1}]],
            })),
            names(&["payment_ids"]),
            fields,
        )
        .await
        .unwrap();
    // the line's parent is the form's move: its repeat changed, so its string is returned
    assert_eq!(value_of(&result), obj(&[("tag_string", s("Foo"))]));
    // nothing saved yet
    assert_eq!(mv.related(&mut tx, "payment_ids").await.unwrap().len(), 0);
    // the save the form then sends
    mv.write_values(
        &mut tx,
        &map(&[("payment_ids", v(json!([[0, 0, {"tag_repeat": 1}]])))]),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    let lines = mv.related(&mut tx, "payment_ids").await.unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines.value(&mut tx, "move_id").await.unwrap(),
        int(id(&mv, 0))
    );
    assert_eq!(lines.value(&mut tx, "tag_repeat").await.unwrap(), int(1));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// payment lines' amounts reach the move's aggregate, 0 → 10 → 0; the changed lines themselves
/// are not echoed)
#[tokio::test]
async fn spec_grp_053_one2many_aggregate_with_zero() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let moves = tx.env().records(MOVE).unwrap();
    let fields = spec(json!({"payment_amount": {}, "payment_ids": {"fields": {"amount": {}}}}));
    let first = moves
        .onchange(&mut tx, ValueMap::new(), vec![], fields.clone())
        .await
        .unwrap();
    assert_eq!(map_of(&value_of(&first))["payment_amount"], int(0));
    for (amount, expected) in [(10, 10), (0, 0)] {
        let form = values(json!({
            "payment_amount": 10 - expected,
            "payment_ids": [[0, "virtual_7", {"amount": amount}]],
        }));
        let result = moves
            .onchange(&mut tx, form, names(&["payment_ids"]), fields.clone())
            .await
            .unwrap();
        let value = map_of(&value_of(&result));
        assert_eq!(value["payment_amount"], int(expected), "{value:?}");
        // the changed lines equal their snapshot: no command is returned for them
        assert!(!value.contains_key("payment_ids"), "{value:?}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An onchange method assigning a delegated field writes it on the in-memory parent: the parent's
/// compute follows, on a new and on a saved payment, and the database parent is untouched.
#[tokio::test]
async fn delegated_field_assigned_by_onchange_method() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, _) = tags(&mut tx).await;
    let fields = spec(json!({
        "repeat_hint": {},
        "tag_id": {},
        "tag_repeat": {},
        "tag_string": {},
    }));
    let payments = tx.env().records(PAYMENT).unwrap();
    let form = values(json!({
        "repeat_hint": 2,
        "tag_id": foo,
        "tag_repeat": 0,
        "tag_string": "",
    }));
    let result = payments
        .onchange(&mut tx, form, names(&["repeat_hint"]), fields.clone())
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[("tag_repeat", int(2)), ("tag_string", s("FooFoo"))])
    );

    let payment = create(
        &mut tx,
        PAYMENT,
        &[&[("tag_id", int(foo)), ("tag_repeat", int(1))]],
    )
    .await;
    let move_id = match payment.value(&mut tx, "move_id").await.unwrap() {
        Value::Int(i) => i,
        other => panic!("{other}"),
    };
    let form = values(json!({
        "repeat_hint": 3,
        "tag_id": foo,
        "tag_repeat": 1,
        "tag_string": "Foo",
    }));
    let result = payment
        .onchange(&mut tx, form, names(&["repeat_hint"]), fields)
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[("tag_repeat", int(3)), ("tag_string", s("FooFooFoo"))])
    );
    tx.flush().await.unwrap();
    assert_eq!(move_row(&mut tx, move_id).await, (Some(foo), 1));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Delegated fields two `_inherits` levels deep (note → payment → move): a changed value reaches
/// the move, and the move's aggregate sees the in-memory payment's amount.
#[tokio::test]
async fn delegated_fields_two_levels_deep() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, _) = tags(&mut tx).await;
    let notes = tx.env().records("test_web.payment_note").unwrap();
    let fields = spec(json!({
        "tag_id": {},
        "tag_name": {},
        "tag_repeat": {},
        "tag_string": {},
        "amount": {},
        "payment_amount": {},
    }));
    let first = notes
        .onchange(&mut tx, ValueMap::new(), vec![], fields.clone())
        .await
        .unwrap();
    let mut form = map_of(&value_of(&first));
    assert_eq!(form["payment_amount"], int(0), "{form:?}");
    form.insert("tag_id".into(), int(foo));
    form.insert("tag_repeat".into(), int(2));
    let result = notes
        .onchange(&mut tx, form.clone(), names(&["tag_id"]), fields.clone())
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[("tag_name", s("Foo")), ("tag_string", s("FooFoo"))])
    );
    let mut form = merged(&form, &result);
    form.insert("amount".into(), int(7));
    let result = notes
        .onchange(&mut tx, form, names(&["amount"]), fields)
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("payment_amount", int(7))]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Writing delegated values on a new record: accepted when its delegate is a new record (the
/// value goes to that in-memory parent, also when the same write sets the delegate), refused when
/// the delegate is empty or a database record (that parent would change, or the value be lost).
#[tokio::test]
async fn delegated_write_on_new_records() {
    use test_web::{Move, Payment};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let moves = tx.env().model::<Move>().unwrap();
    let payments = tx.env().model::<Payment>().unwrap();
    let parent = moves.new_record(&mut tx, moves.vals()).await.unwrap();
    let parent_id = parent.as_records().rec_ids()[0];
    let linked = payments
        .new_record(&mut tx, payments.vals().set(Payment::MOVE_ID, parent_id))
        .await
        .unwrap();
    linked
        .as_records()
        .set_value(&mut tx, "tag_repeat", int(4))
        .await
        .unwrap();
    assert_eq!(
        parent
            .as_records()
            .value(&mut tx, "tag_repeat")
            .await
            .unwrap(),
        int(4)
    );
    assert_eq!(
        linked
            .as_records()
            .value(&mut tx, "tag_repeat")
            .await
            .unwrap(),
        int(4)
    );

    // the delegate set by the same write counts
    let other = moves.new_record(&mut tx, moves.vals()).await.unwrap();
    let other_id = other.as_records().rec_ids()[0];
    let unlinked = payments.new_record(&mut tx, payments.vals()).await.unwrap();
    unlinked
        .set(&mut tx, Payment::MOVE_ID, other_id)
        .await
        .unwrap();
    unlinked
        .as_records()
        .set_value(&mut tx, "tag_repeat", int(5))
        .await
        .unwrap();
    assert_eq!(
        other
            .as_records()
            .value(&mut tx, "tag_repeat")
            .await
            .unwrap(),
        int(5)
    );

    // no delegate
    let empty = payments.new_record(&mut tx, payments.vals()).await.unwrap();
    let err = empty
        .as_records()
        .set_value(&mut tx, "tag_repeat", int(1))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "test_web.payment.tag_repeat: new record {} has no move_id; delegated values of new \
             records go to an in-memory parent, so the value would be lost",
            empty.as_records().rec_ids()[0]
        )
    );
    // a database delegate
    let stored = create(&mut tx, MOVE, &[&[("tag_repeat", int(9))]]).await;
    let stored_id = stored.rec_ids()[0];
    let on_stored = payments
        .new_record(&mut tx, payments.vals().set(Payment::MOVE_ID, stored_id))
        .await
        .unwrap();
    let err = on_stored
        .as_records()
        .set_value(&mut tx, "tag_repeat", int(1))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "test_web.payment.tag_repeat: new record {} has move_id = test_web.move({},), a \
             database record; delegated values of new records go to an in-memory parent, so the \
             database record would change",
            on_stored.as_records().rec_ids()[0],
            id(&stored, 0)
        )
    );
    tx.flush().await.unwrap();
    assert_eq!(move_row(&mut tx, id(&stored, 0)).await, (None, 9));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// back as `LINK` commands with the attendees' values inside the line's `CREATE`)
#[tokio::test]
async fn nested_line_commands_on_a_new_course() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let people = create(
        &mut tx,
        "test_web.person",
        &[&[("name", s("Ann"))], &[("name", s("Bob"))]],
    )
    .await;
    let (ann, bob) = (id(&people, 0), id(&people, 1));
    let courses = tx.env().records("test_web.course").unwrap();
    let fields =
        spec(json!({"lesson_ids": {"fields": {"attendee_ids": {"fields": {"name": {}}}}}}));
    let result = courses
        .onchange(
            &mut tx,
            values(json!({"lesson_ids": [[0, "virtual_1", {
                "attendee_ids": [[6, 0, [ann]], [4, bob, 0]],
            }]]})),
            vec![],
            fields,
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lesson_ids": [[0, "virtual_1", {"attendee_ids": [
            [4, ann, {"id": ann, "name": "Ann"}],
            [4, bob, {"id": bob, "name": "Bob"}],
        ]}]]}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- review round 1 ------------------------------------------------------------------

/// The database id of a record's many2one `field`.
async fn link_of(tx: &mut Transaction, record: &Records, field: &str) -> i64 {
    match record.value(tx, field).await.unwrap() {
        Value::Int(i) => i,
        other => panic!("{record}.{field} is {other}"),
    }
}

/// payment, on its own form and as a line of its move's form, counts it once in the move's total)
#[tokio::test]
async fn spec_grp_053_saved_child_counted_once_in_parent_aggregate() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(&mut tx, PAYMENT, &[&[("amount", int(5))]]).await;
    let move_id = link_of(&mut tx, &p, "move_id").await;
    let q = create(
        &mut tx,
        PAYMENT,
        &[&[("move_id", int(move_id)), ("amount", int(3))]],
    )
    .await;
    assert_eq!(link_of(&mut tx, &q, "move_id").await, move_id);
    let mv = tx.env().records(MOVE).unwrap().browse([move_id]).unwrap();
    assert_eq!(mv.value(&mut tx, "payment_amount").await.unwrap(), int(8));

    // the payment's form
    let fields = spec(json!({"amount": {}, "payment_amount": {}}));
    let result = p
        .onchange(
            &mut tx,
            values(json!({"amount": 10, "payment_amount": 8})),
            names(&["amount"]),
            fields,
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("payment_amount", int(13))]));

    // the move's form, the payment edited as a line
    let fields = spec(json!({
        "payment_amount": {},
        "payment_ids": {"fields": {"amount": {}}},
    }));
    let result = mv
        .onchange(
            &mut tx,
            values(json!({
                "payment_amount": 8,
                "payment_ids": [[1, id(&p, 0), {"amount": 10}]],
            })),
            names(&["payment_ids"]),
            fields,
        )
        .await
        .unwrap();
    let value = map_of(&value_of(&result));
    assert_eq!(value.get("payment_amount"), Some(&int(13)), "{value:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// line's in-memory grandparent like its delegated scalar values, and the grandparent's aggregate
/// over it is returned in the line's `CREATE`)
#[tokio::test]
async fn nested_delegated_x2many_in_line() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let moves = tx.env().records(MOVE).unwrap();
    let fields = spec(json!({"note_ids": {"fields": {"note": {}, "payment_amount": {}}}}));
    let result = moves
        .onchange(
            &mut tx,
            values(json!({
                "note_ids": [[0, "virtual_1", {
                    "note": "n",
                    "payment_ids": [[0, 0, {"amount": 7}], [0, 0, {"amount": 2}]],
                }]],
            })),
            vec![],
            fields,
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"note_ids": [[0, "virtual_1", {"note": "n", "payment_amount": 9}]]}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A saved payment moved to another move on its form: the new move's total counts it, and the
/// in-memory record of its old move no longer lists it (read by an onchange method of `move_id`).
#[tokio::test]
async fn delegate_relinked_leaves_the_old_parent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(&mut tx, PAYMENT, &[&[("amount", int(5))]]).await;
    let q = create(&mut tx, PAYMENT, &[&[("amount", int(3))]]).await;
    let old_move = link_of(&mut tx, &p, "move_id").await;
    let new_move = link_of(&mut tx, &q, "move_id").await;
    assert_ne!(old_move, new_move);
    let fields = spec(json!({
        "move_id": {},
        "amount": {},
        "payment_amount": {},
        "previous_move_amount": {},
    }));
    let result = p
        .onchange(
            &mut tx,
            values(json!({
                "move_id": new_move,
                "amount": 5,
                "payment_amount": 5,
                "previous_move_amount": -1,
            })),
            names(&["move_id"]),
            fields,
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[("payment_amount", int(8)), ("previous_move_amount", int(0))])
    );
    // nothing reached the database
    tx.flush().await.unwrap();
    assert_eq!(link_of(&mut tx, &p, "move_id").await, old_move);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn delegate_link_to_missing_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let payments = tx.env().records(PAYMENT).unwrap();
    let err = payments
        .onchange(
            &mut tx,
            values(json!({"move_id": 999_999_999, "amount": 1})),
            names(&["move_id"]),
            spec(json!({"move_id": {}, "amount": {}})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        format!(
            "Record does not exist or has been deleted.\n(Record: test_web.move(999999999,), \
             User: {})",
            tx.env().uid()
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn saved_line_relinked_to_the_form_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, bar) = tags(&mut tx).await;
    let p = create(
        &mut tx,
        PAYMENT,
        &[&[("tag_id", int(bar)), ("tag_repeat", int(1))]],
    )
    .await;
    let saved_move = link_of(&mut tx, &p, "move_id").await;
    let moves = tx.env().records(MOVE).unwrap();
    let fields = spec(json!({
        "tag_id": {},
        "tag_string": {},
        "payment_ids": {"fields": {"tag_repeat": {}}},
    }));
    let result = moves
        .onchange(
            &mut tx,
            values(json!({
                "tag_id": foo,
                "tag_string": "",
                "payment_ids": [[1, id(&p, 0), {"tag_repeat": 2}]],
            })),
            names(&["payment_ids"]),
            fields,
        )
        .await
        .unwrap();
    let value = map_of(&value_of(&result));
    assert_eq!(value.get("tag_string"), Some(&s("FooFoo")), "{value:?}");
    tx.flush().await.unwrap();
    assert_eq!(move_row(&mut tx, saved_move).await, (Some(bar), 1));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Lines of a one2many without `_inherits` get the form's record as their inverse too: a new
/// message line of a new discussion computes its name from the form's discussion name (first
/// call: every value comes back).
#[tokio::test]
async fn new_lines_point_at_the_form_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let discussions = tx.env().records(DISCUSSION).unwrap();
    let result = discussions
        .onchange(
            &mut tx,
            values(json!({
                "name": "General",
                "messages": [[0, "virtual_1", {"body": "hi"}]],
            })),
            vec![],
            spec(json!({"name": {}, "messages": {"fields": {"name": {}, "body": {}}}})),
        )
        .await
        .unwrap();
    let value = map_of(&value_of(&result));
    let Some(Value::List(commands)) = value.get("messages") else {
        panic!("{value:?}")
    };
    assert_eq!(commands.len(), 1, "{commands:?}");
    let Value::List(create_line) = &commands[0] else {
        panic!("{commands:?}")
    };
    assert_eq!(create_line[0], int(0), "{create_line:?}");
    assert_eq!(
        map_of(&create_line[2]).get("name"),
        Some(&s("[General] __system__")),
        "{create_line:?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Delegated values of lines whose parents are database records: the lines' links and their
/// parents are read together, so 8 lines cost as many queries as 2.
#[tokio::test]
async fn delegated_line_values_read_parents_together() {
    let h = harness().await;
    let mut counts = Vec::new();
    for n in [2_i64, 8] {
        let mut tx = h.begin().await.unwrap();
        let mv = create(&mut tx, MOVE, &[&[]]).await;
        let mut commands = Vec::new();
        for i in 0..n {
            let payment = create(&mut tx, PAYMENT, &[&[("amount", int(i))]]).await;
            let note = create(
                &mut tx,
                "test_web.payment_note",
                &[&[
                    ("payment_id", int(id(&payment, 0))),
                    ("move_ref_id", int(id(&mv, 0))),
                ]],
            )
            .await;
            commands.push(json!([1, id(&note, 0), {"amount": 100 + i}]));
        }
        tx.invalidate_all().await.unwrap();
        let before = tx.query_count();
        let result = mv
            .onchange(
                &mut tx,
                values(json!({"note_ids": commands})),
                names(&["note_ids"]),
                spec(json!({"note_ids": {"fields": {"note": {}, "amount": {}}}})),
            )
            .await
            .unwrap();
        counts.push(tx.query_count() - before);
        // the changed lines equal their snapshot
        assert_eq!(value_of(&result), obj(&[]));
        drop(tx);
    }
    assert_eq!(
        counts[0], counts[1],
        "queries for 2 and 8 lines: {counts:?}"
    );
    h.cleanup().await.unwrap();
}

/// Writing delegated values on database records through a new parent, or while emptying the
/// delegate, is refused: the values would stay in memory and never reach the database.
#[tokio::test]
async fn delegated_write_on_database_records_needs_a_database_parent() {
    use erp_sdk::{Cell, RawVals};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(&mut tx, PAYMENT, &[&[("tag_repeat", int(1))]]).await;
    let move_id = link_of(&mut tx, &p, "move_id").await;
    let moves = tx.env().records(MOVE).unwrap();
    let in_memory = tx
        .new_record(&moves, RawVals::new(), None, None)
        .await
        .unwrap();
    let new_move = in_memory.rec_ids()[0];
    let err = p
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("move_id", Cell::Id(new_move.get()))
                .set_cell("tag_repeat", Cell::Int(3)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "test_web.payment: delegated values (tag_repeat) cannot be written through move_id = \
             {new_move}, a new record, on database records test_web.payment({},)",
            id(&p, 0)
        )
    );
    // a batch of a new and a database payment
    let payments = tx.env().records(PAYMENT).unwrap();
    let linked = tx
        .new_record(
            &payments,
            RawVals::new().set_cell("move_id", Cell::Id(new_move.get())),
            None,
            None,
        )
        .await
        .unwrap();
    let mixed = payments.browse_ids([linked.rec_ids()[0], p.rec_ids()[0]]);
    let err = mixed
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("move_id", Cell::Id(new_move.get()))
                .set_cell("tag_repeat", Cell::Int(3)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "test_web.payment: delegated values (tag_repeat) cannot be written through move_id = \
             {new_move}, a new record, on database records test_web.payment({},)",
            id(&p, 0)
        )
    );
    // emptying the delegate in the same write
    let err = p
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("move_id", Cell::Null)
                .set_cell("tag_repeat", Cell::Int(3)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "test_web.payment: delegated values (tag_repeat) cannot be written while move_id is \
             emptied on test_web.payment({},)",
            id(&p, 0)
        )
    );
    tx.flush().await.unwrap();
    assert_eq!(move_row(&mut tx, move_id).await, (None, 1));
    assert_eq!(link_of(&mut tx, &p, "move_id").await, move_id);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A batch of a new payment (with an in-memory parent) and a saved one: the delegated value goes to
/// both parents (the in-memory one in the cache, the saved one in the database); with a new payment
/// that has no parent, the whole write is refused and nothing is written.
#[tokio::test]
async fn delegated_write_on_mixed_new_and_database_records() {
    use erp_sdk::{Cell, RawVals};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(&mut tx, PAYMENT, &[&[("tag_repeat", int(1))]]).await;
    let move_id = link_of(&mut tx, &p, "move_id").await;
    let moves = tx.env().records(MOVE).unwrap();
    let payments = tx.env().records(PAYMENT).unwrap();
    let in_memory = tx
        .new_record(&moves, RawVals::new(), None, None)
        .await
        .unwrap();
    let linked = tx
        .new_record(
            &payments,
            RawVals::new().set_cell("move_id", Cell::Id(in_memory.rec_ids()[0].get())),
            None,
            None,
        )
        .await
        .unwrap();
    let mixed = payments.browse_ids([linked.rec_ids()[0], p.rec_ids()[0]]);
    mixed
        .write(&mut tx, RawVals::new().set_cell("tag_repeat", Cell::Int(6)))
        .await
        .unwrap();
    assert_eq!(
        in_memory.value(&mut tx, "tag_repeat").await.unwrap(),
        int(6)
    );
    tx.flush().await.unwrap();
    assert_eq!(move_row(&mut tx, move_id).await, (None, 6));

    let orphan = tx
        .new_record(&payments, RawVals::new(), None, None)
        .await
        .unwrap();
    let mixed = payments.browse_ids([p.rec_ids()[0], orphan.rec_ids()[0]]);
    let err = mixed
        .write(&mut tx, RawVals::new().set_cell("tag_repeat", Cell::Int(7)))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "test_web.payment.tag_repeat: new record {} has no move_id; delegated values of new \
             records go to an in-memory parent, so the value would be lost",
            orphan.rec_ids()[0]
        )
    );
    tx.flush().await.unwrap();
    assert_eq!(move_row(&mut tx, move_id).await, (None, 6));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A many2one sent as a dictionary needs its `id`.
#[tokio::test]
async fn many2one_map_without_id_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let payments = tx.env().records(PAYMENT).unwrap();
    let err = payments
        .onchange(
            &mut tx,
            values(json!({"move_id": {"display_name": "M"}})),
            names(&["move_id"]),
            spec(json!({"move_id": {}})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_web.payment.move_id: {} (a many2one sent as a dictionary needs \
             its 'id')",
            v(json!({"display_name": "M"}))
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn many2one_map_with_id_ignores_other_keys() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let discussion = create(&mut tx, DISCUSSION, &[&[("name", s("General"))]]).await;
    let d = id(&discussion, 0);
    let mut vals = values(json!({
        "body": "hello",
        "author": 1,
        "name": "[] __system__",
        "size": 5,
    }));
    vals.insert(
        "discussion".to_owned(),
        obj(&[("id", int(d)), ("name", s("Renamed"))]),
    );
    let result = tx
        .env()
        .records(MESSAGE)
        .unwrap()
        .onchange(&mut tx, vals, names(&["discussion"]), message_spec())
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        obj(&[("name", s("[General] __system__"))])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The fixture's `tag_string` is bounded: a huge repeat count from the client is an error, not a
/// huge allocation; a negative one gives the empty string, as Python's `str * n`.
#[tokio::test]
async fn tag_string_repeat_is_bounded() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, _) = tags(&mut tx).await;
    let payments = tx.env().records(PAYMENT).unwrap();
    let form =
        |repeat: i64| values(json!({"tag_id": foo, "tag_repeat": repeat, "tag_string": "x"}));
    let result = payments
        .onchange(&mut tx, form(-3), names(&["tag_repeat"]), payment_spec())
        .await
        .unwrap();
    assert_eq!(map_of(&value_of(&result)).get("tag_string"), Some(&s("")));
    let err = payments
        .onchange(
            &mut tx,
            form(i64::from(i32::MAX)),
            names(&["tag_repeat"]),
            payment_spec(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "test_web.move: tag_string of 3 bytes repeated {} times is longer than {} bytes",
            i32::MAX,
            test_web::TAG_STRING_MAX_BYTES
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn reference_inverse_of_lines_is_emptied() {
    let h = Harness::builder([&base_users::ADDON, &test_rel::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let owner = create(&mut tx, "test_rel.attach_owner", &[&[("name", s("o"))]]).await;
    let att = create(
        &mut tx,
        "test_rel.attachment",
        &[&[
            ("name", s("a")),
            ("res_model", s("test_rel.attach_owner")),
            ("res_id", int(id(&owner, 0))),
        ]],
    )
    .await;
    assert_eq!(
        owner
            .related(&mut tx, "attachment_ids")
            .await
            .unwrap()
            .ids(),
        att.ids()
    );
    let result = owner
        .onchange(
            &mut tx,
            values(json!({"attachment_ids": [[4, id(&att, 0), false]]})),
            vec![],
            spec(json!({"attachment_ids": {"fields": {"name": {}, "res_id": {}}}})),
        )
        .await
        .unwrap();
    let value = map_of(&value_of(&result));
    let Some(Value::List(commands)) = value.get("attachment_ids") else {
        panic!("{value:?}")
    };
    assert_eq!(commands.len(), 2, "{commands:?}");
    let Value::List(link) = &commands[0] else {
        panic!("{commands:?}")
    };
    assert_eq!(link[0], int(4), "{commands:?}");
    assert_eq!(map_of(&link[2]).get("res_id"), Some(&int(id(&owner, 0))));
    assert_eq!(
        commands[1],
        v(json!([1, id(&att, 0), {"res_id": 0}])),
        "{commands:?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Both halves of the case: a delegated field changed on the child's form, and inside a one2many
/// line of the parent's form; each time the parent's computed field depending on it is recomputed
/// and returned.
#[tokio::test]
async fn spec_grp_053_onchange_and_inherits() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (foo, _) = tags(&mut tx).await;

    // the child's form: delegated `tag_repeat` → the parent's `tag_string`
    let payments = tx.env().records(PAYMENT).unwrap();
    let result = payments
        .onchange(
            &mut tx,
            values(json!({
                "move_id": false,
                "tag_id": foo,
                "tag_name": "Foo",
                "tag_repeat": 2,
                "tag_string": "Foo",
            })),
            names(&["tag_repeat"]),
            payment_spec(),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("tag_string", s("FooFoo"))]));

    // a one2many line of the parent's form: the line's delegated `tag_repeat` reaches the form's
    // move, whose `tag_string` is returned
    let mv = create(
        &mut tx,
        MOVE,
        &[&[("tag_id", int(foo)), ("tag_repeat", int(1))]],
    )
    .await;
    let result = mv
        .onchange(
            &mut tx,
            values(json!({
                "tag_string": "Foo",
                "payment_ids": [[0, "virtual_1", {"tag_repeat": 3}]],
            })),
            names(&["payment_ids"]),
            spec(json!({
                "tag_string": {},
                "payment_ids": {"fields": {"tag_repeat": {}}},
            })),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("tag_string", s("FooFooFoo"))]));
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- review round 2 ------------------------------------------------------------------

const NOTE: &str = "test_web.payment_note";
const VAULT: &str = "test_web.vault";
const VAULT_ENTRY: &str = "test_web.vault_entry";

/// Vaults named "sealed" are hidden from reads outside superuser mode (a record rule); everything
/// else is allowed.
struct HideSealedVaults;

impl erp_testkit::TestRules for HideSealedVaults {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        Ok(match (model.name, op) {
            (VAULT, Operation::Read) => {
                Domain::condition("name", erp_sdk::Operator::Ne, "sealed").unwrap()
            }
            _ => Domain::TRUE,
        })
    }
}

/// Review item 2: the database parent of a delegate is checked by reading its id only, never one of
/// its columns. As uid 2, who may read the open vault but is outside the group of its first column
/// (`secret`; field groups are enforced), the onchange of a new entry and of
/// the saved entry succeeds and no statement reads `secret`, while the vault is still checked. With
/// the record rule in force, a vault it hides is an `Access` error and one that does not exist a
/// `Missing` error.
#[tokio::test]
async fn delegate_parent_checked_by_id_only() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(HideSealedVaults)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    // the user the record checks run for (its name goes into the access evidence)
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)")
        .await
        .unwrap();
    let vaults = create(
        &mut tx,
        VAULT,
        &[
            &[("secret", s("s1")), ("name", s("open"))],
            &[("secret", s("s2")), ("name", s("sealed"))],
        ],
    )
    .await;
    let (open, sealed) = (id(&vaults, 0), id(&vaults, 1));
    let entry = create(
        &mut tx,
        VAULT_ENTRY,
        &[&[("vault_id", int(open)), ("label", s("a"))]],
    )
    .await;
    let uid = 2;
    let entries = tx.env().with_user(uid).records(VAULT_ENTRY).unwrap();
    let saved = entries.browse([id(&entry, 0)]).unwrap();
    let fields = spec(json!({"vault_id": {}, "label": {}}));

    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    let result = entries
        .onchange(
            &mut tx,
            values(json!({"vault_id": open, "label": "b"})),
            names(&["vault_id"]),
            fields.clone(),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[]));
    let result = saved
        .onchange(
            &mut tx,
            values(json!({"vault_id": open, "label": "c"})),
            names(&["label"]),
            fields.clone(),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[]));
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    let vault_reads = statements
        .iter()
        .filter(|q| q.contains("FROM \"test_web_vault\""))
        .count();
    assert_eq!(vault_reads, 2, "one check per onchange: {statements:#?}");
    assert!(
        statements.iter().all(|q| !q.contains("\"secret\"")),
        "{statements:#?}"
    );

    let err = entries
        .onchange(
            &mut tx,
            values(json!({"vault_id": sealed, "label": "b"})),
            names(&["vault_id"]),
            fields.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!(
            "Your access rights do not let you access 'Vault' (test_web.vault) records.\n\nRecords: \
             test_web.vault({sealed},), User: {uid}"
        )
    );
    let err = entries
        .onchange(
            &mut tx,
            values(json!({"vault_id": 999_999_999, "label": "b"})),
            names(&["vault_id"]),
            fields,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        format!(
            "Record does not exist or has been deleted.\n(Record: test_web.vault(999999999,), \
             User: {uid})"
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 3: saved payments of other moves, edited as lines of a new move's form, are relinked
/// to the form's record, so their amounts and delegated `tag_repeat` reach it; 8 lines cost as many
/// statements as 2.
#[tokio::test]
async fn lines_relinked_to_the_form_record_cost_does_not_grow() {
    let h = harness().await;
    let mut counts = Vec::new();
    for n in [2_i64, 8] {
        let mut tx = h.begin().await.unwrap();
        let (foo, _) = tags(&mut tx).await;
        let mut commands = Vec::new();
        let mut total = 0;
        for i in 1..=n {
            // each saved payment has a move of its own
            let payment = create(&mut tx, PAYMENT, &[&[("amount", int(i))]]).await;
            commands.push(json!([1, id(&payment, 0), {"amount": 10 * i, "tag_repeat": 2}]));
            total += 10 * i;
        }
        tx.invalidate_all().await.unwrap();
        let moves = tx.env().records(MOVE).unwrap();
        let before = tx.query_count();
        let result = moves
            .onchange(
                &mut tx,
                values(json!({
                    "tag_id": foo,
                    "tag_string": "",
                    "payment_amount": 0,
                    "payment_ids": commands,
                })),
                names(&["payment_ids"]),
                spec(json!({
                    "tag_id": {},
                    "tag_string": {},
                    "payment_amount": {},
                    "payment_ids": {"fields": {"amount": {}, "tag_repeat": {}}},
                })),
            )
            .await
            .unwrap();
        counts.push(tx.query_count() - before);
        let value = map_of(&value_of(&result));
        assert_eq!(value.get("payment_amount"), Some(&int(total)), "{value:?}");
        assert_eq!(value.get("tag_string"), Some(&s("FooFoo")), "{value:?}");
        drop(tx);
    }
    assert_eq!(
        counts[0], counts[1],
        "queries for 2 and 8 lines: {counts:?}"
    );
    // the saved payments (their values and links), the name of the form's tag
    assert_eq!(counts[0], 2, "{counts:?}");
    h.cleanup().await.unwrap();
}

/// Review item 3: saved messages of another discussion, linked into a saved discussion's form on
/// the first call (every value comes back), point at the form's record (`link_lines` on lines
/// without `_inherits`): each comes back linked, then updated with the form's discussion; 30 lines
/// cost as many statements as 3.
#[tokio::test]
async fn linked_plain_lines_cost_does_not_grow() {
    let h = harness().await;
    let mut counts = Vec::new();
    for n in [3_usize, 30] {
        let mut tx = h.begin().await.unwrap();
        let other = create(&mut tx, DISCUSSION, &[&[("name", s("other"))]]).await;
        let rows: Vec<Vec<(&str, Value)>> = (0..n)
            .map(|i| {
                vec![
                    ("discussion", int(id(&other, 0))),
                    ("body", Value::str(format!("line {i}"))),
                ]
            })
            .collect();
        let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
        let messages = create(&mut tx, MESSAGE, &refs).await;
        let form = create(&mut tx, DISCUSSION, &[&[("name", s("form"))]]).await;
        let commands: Vec<_> = messages
            .ids()
            .iter()
            .map(|m| json!([4, m, false]))
            .collect();
        tx.invalidate_all().await.unwrap();
        let before = tx.query_count();
        let result = form
            .onchange(
                &mut tx,
                values(json!({"messages": commands})),
                vec![],
                spec(json!({"messages": {"fields": {"body": {}, "discussion": {}}}})),
            )
            .await
            .unwrap();
        counts.push(tx.query_count() - before);
        let value = map_of(&value_of(&result));
        let Some(Value::List(got)) = value.get("messages") else {
            panic!("{value:?}")
        };
        let codes: Vec<i64> = got
            .iter()
            .map(|c| match c {
                Value::List(c) => match c[0] {
                    Value::Int(code) => code,
                    ref other => panic!("{other}"),
                },
                other => panic!("{other}"),
            })
            .collect();
        assert_eq!(codes, [4, 1].repeat(n), "{got:?}");
        let updates: Vec<Value> = got
            .iter()
            .filter(|c| matches!(c, Value::List(c) if c[0] == int(1)))
            .cloned()
            .collect();
        let expected: Vec<Value> = messages
            .ids()
            .iter()
            .map(|m| v(json!([1, m, {"discussion": id(&form, 0)}])))
            .collect();
        assert_eq!(updates, expected);
        drop(tx);
    }
    assert_eq!(
        counts[0], counts[1],
        "queries for 3 and 30 lines: {counts:?}"
    );
    // the form's own lines, the stored values of the linked lines
    assert_eq!(counts[0], 2, "{counts:?}");
    h.cleanup().await.unwrap();
}

/// Review item 3: a saved note moved to another move through its delegated `move_id`, two
/// `_inherits` levels up (note → payment → move). The note's in-memory payment leaves the in-memory
/// record of its old move, the note's old grandparent, whose aggregate then counts only the old
/// move's other payments (read by an onchange method of the note), and the new move's aggregate
/// counts it; nothing reaches the database. The statements do not grow with the old move's other
/// payments (2 or 8), within a budget.
#[tokio::test]
async fn two_level_relink_leaves_the_old_grandparent() {
    let h = harness().await;
    let mut counts = Vec::new();
    for k in [2_i64, 8] {
        let mut tx = h.begin().await.unwrap();
        let p = create(&mut tx, PAYMENT, &[&[("amount", int(5))]]).await;
        let old_move = link_of(&mut tx, &p, "move_id").await;
        let mut others = 0;
        for i in 1..=k {
            create(
                &mut tx,
                PAYMENT,
                &[&[("move_id", int(old_move)), ("amount", int(i))]],
            )
            .await;
            others += i;
        }
        let q = create(&mut tx, PAYMENT, &[&[("amount", int(1000))]]).await;
        let new_move = link_of(&mut tx, &q, "move_id").await;
        let note = create(&mut tx, NOTE, &[&[("payment_id", int(id(&p, 0)))]]).await;
        tx.invalidate_all().await.unwrap();
        let fields = spec(json!({
            "move_id": {},
            "amount": {},
            "payment_amount": {},
            "old_move_amount": {},
        }));
        let before = tx.query_count();
        let result = note
            .onchange(
                &mut tx,
                values(json!({
                    "move_id": new_move,
                    "amount": 5,
                    "payment_amount": 5 + others,
                    "old_move_amount": -1,
                })),
                names(&["move_id"]),
                fields,
            )
            .await
            .unwrap();
        counts.push(tx.query_count() - before);
        assert_eq!(
            value_of(&result),
            obj(&[
                ("payment_amount", int(1005)),
                ("old_move_amount", int(others))
            ])
        );
        tx.flush().await.unwrap();
        assert_eq!(link_of(&mut tx, &p, "move_id").await, old_move);
        drop(tx);
    }
    assert_eq!(
        counts[0], counts[1],
        "queries with 2 and 8 other payments: {counts:?}"
    );
    // the note, its payment, the old move's payments (the stored total the form starts from), the
    // id-only checks of the payment and of both moves, the new move's payments
    assert_eq!(counts[0], 7, "{counts:?}");
    h.cleanup().await.unwrap();
}

// -----: x2many values inside lines ---------------------------

const MULTI: &str = "test_web.multi";
const MULTI_LINE: &str = "test_web.multi_line";
const MULTI_SUB: &str = "test_web.multi_sub";
const MULTI_TAG: &str = "test_web.multi_tag";

/// A saved multi (partner "A partner") with line "one" holding sub-lines s1 (amount 1) and s2
/// (amount 2), and sub-line s9 (amount 9) of line "nine" of another multi.
struct SavedMulti {
    multi: Records,
    line: i64,
    s1: i64,
    s2: i64,
    nine: i64,
    s9: i64,
}

async fn saved_multi(tx: &mut Transaction) -> SavedMulti {
    let partner = create(tx, "test_web.person", &[&[("name", s("A partner"))]]).await;
    let multi = create(tx, MULTI, &[&[("partner", int(id(&partner, 0)))]]).await;
    let line = create(
        tx,
        MULTI_LINE,
        &[&[("multi", int(id(&multi, 0))), ("name", s("one"))]],
    )
    .await;
    let line = id(&line, 0);
    let subs = create(
        tx,
        MULTI_SUB,
        &[
            &[
                ("line_id", int(line)),
                ("name", s("s1")),
                ("amount", int(1)),
            ],
            &[
                ("line_id", int(line)),
                ("name", s("s2")),
                ("amount", int(2)),
            ],
        ],
    )
    .await;
    let other = create(tx, MULTI, &[&[]]).await;
    let nine = create(
        tx,
        MULTI_LINE,
        &[&[("multi", int(id(&other, 0))), ("name", s("nine"))]],
    )
    .await;
    let nine = id(&nine, 0);
    let s9 = create(
        tx,
        MULTI_SUB,
        &[&[
            ("line_id", int(nine)),
            ("name", s("s9")),
            ("amount", int(9)),
        ]],
    )
    .await;
    SavedMulti {
        multi,
        line,
        s1: id(&subs, 0),
        s2: id(&subs, 1),
        nine,
        s9: id(&s9, 0),
    }
}

/// The sub-lines in the database: `(id, line_id, parent_id, amount)` in id order.
async fn sub_rows(tx: &mut Transaction) -> Vec<(i64, Option<i64>, Option<i64>, i32)> {
    tx.query(
        "SELECT id, line_id, parent_id, amount FROM test_web_multi_sub ORDER BY id",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
    .collect()
}

#[tokio::test]
async fn spec_grp_061_onchange_applies_nested_commands() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let saved = saved_multi(&mut tx).await;
    let partner2 = create(
        &mut tx,
        "test_web.person",
        &[&[("name", s("A second partner"))]],
    )
    .await;
    let p2 = id(&partner2, 0);

    let base = json!({
        "name": {},
        "partner": {"fields": {"display_name": {}}},
        "lines": {"fields": {
            "name": {},
            "partner": {"fields": {"display_name": {}}},
            "tags": {"fields": {"name": {}}},
        }},
    });
    let mut with_id = base.clone();
    with_id["lines"]["fields"]["tags"]["fields"]["id"] = json!({});
    let mut with_inverse = base.clone();
    with_inverse["lines"]["fields"]["multi"] = json!({});
    let changed_line = v(json!({
        "name": "A second partner",
        "partner": {"id": p2, "display_name": "A second partner"},
    }));
    for tags_value in [json!([[5, 0, 0]]), json!([[0, 0, {"name": "Tag"}]])] {
        for fields in [&base, &with_id, &with_inverse] {
            let result = saved
                .multi
                .onchange(
                    &mut tx,
                    values(json!({
                        "name": "A partner",
                        "partner": p2,
                        "lines": [[0, "virtual2", {
                            "name": null,
                            "partner": null,
                            "tags": tags_value,
                        }]],
                    })),
                    names(&["partner"]),
                    spec(fields.clone()),
                )
                .await
                .unwrap();
            assert_eq!(
                value_of(&result),
                obj(&[
                    ("name", s("A second partner")),
                    (
                        "lines",
                        Value::list([
                            Value::list([int(1), int(saved.line), changed_line.clone()]),
                            Value::list([int(1), s("virtual2"), changed_line.clone()]),
                        ])
                    ),
                ]),
                "{tags_value} {fields}"
            );
        }
    }

    // nested commands as changed values: s1 updated, s2 removed, a sub-line created and s9 linked
    // on the saved line, a new line with a sub-line; the form's total (3 before) sees them all
    let result = saved
        .multi
        .onchange(
            &mut tx,
            values(json!({
                "total": 3,
                "lines": [
                    [1, saved.line, {"sub_ids": [
                        [1, saved.s1, {"amount": 10}],
                        [2, saved.s2, 0],
                        [0, "v_s3", {"name": "new", "amount": 5}],
                        [4, saved.s9, 0],
                    ]}],
                    [0, "v_l2", {"name": "two", "tags": [], "sub_ids": [[0, "v_s4", {"amount": 7}]]}],
                ],
            })),
            names(&["lines"]),
            spec(json!({
                "total": {},
                "lines": {"fields": {
                    "name": {},
                    "amount": {},
                    "sub_ids": {"fields": {"amount": {}, "line_name": {}}},
                }},
            })),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("total", int(10 + 5 + 9 + 7))]));

    // a first call, three levels deep: the new sub-line and the linked s9 point at the in-memory
    // line (their `line_name`), the new child at its in-memory sub-line
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[0, "v1", {"name": "L", "sub_ids": [
                [0, "v2", {"name": "süß ✓", "amount": 3, "child_ids": [
                    [0, "v3", {"name": "c", "amount": 100}],
                ]}],
                [4, saved.s9, 0],
            ]}]]})),
            vec![],
            spec(json!({
                "total": {},
                "lines": {"fields": {
                    "name": {},
                    "sub_ids": {"fields": {
                        "name": {},
                        "line_name": {},
                        "child_ids": {"fields": {"name": {}, "amount": {}}},
                    }},
                }},
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({
            "total": 12,
            "lines": [[0, "v1", {"name": "L", "sub_ids": [
                [0, "v2", {"name": "süß ✓", "line_name": "L", "child_ids": [
                    [0, "v3", {"name": "c", "amount": 100}],
                ]}],
                [4, saved.s9, {"id": saved.s9, "name": "s9", "line_name": "nine", "child_ids": []}],
                [1, saved.s9, {"line_name": "L"}],
            ]}]],
        }))
    );

    // a saved line with tags: its nested commands apply to its stored tags (in-memory records of
    // the same origins): one unlinked, one created, the tag that stays is not returned, as changed
    // values (the line is part of snapshot 0) and as the form's first call (compared with the
    // stored line)
    let tags = create(
        &mut tx,
        MULTI_TAG,
        &[&[("name", s("t1"))], &[("name", s("t2"))]],
    )
    .await;
    let (t1, t2) = (id(&tags, 0), id(&tags, 1));
    let tagged_multi = create(&mut tx, MULTI, &[&[]]).await;
    let tagged = create(
        &mut tx,
        MULTI_LINE,
        &[&[
            ("multi", int(id(&tagged_multi, 0))),
            ("tags", v(json!([[6, 0, [t1, t2]]]))),
        ]],
    )
    .await;
    let tagged = id(&tagged, 0);
    let tag_commands = json!([[3, t1, 0], [0, "v_t", {"name": "new"}]]);
    let tag_spec = spec(json!({"lines": {"fields": {"tags": {"fields": {"name": {}}}}}}));
    // the form's `tag_total` (a count over the lines' tags) is a stale 99 on the client: the
    // server's value follows the nested commands (t1 unlinked, a tag created: t2 and the new tag;
    // then every tag unlinked but the created one), which shows they were applied
    let total_spec = spec(json!({
        "tag_total": {},
        "lines": {"fields": {"tags": {"fields": {"name": {}}}}},
    }));
    let result = tagged_multi
        .onchange(
            &mut tx,
            values(json!({
                "tag_total": 99,
                "lines": [[1, tagged, {"tags": tag_commands}]],
            })),
            names(&["lines"]),
            total_spec.clone(),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("tag_total", int(2))]));
    // one more command: the line is left with the new tag only
    let result = tagged_multi
        .onchange(
            &mut tx,
            values(json!({
                "tag_total": 99,
                "lines": [[1, tagged, {"tags": [[3, t1, 0], [3, t2, 0], [0, "v_t", {"name": "new"}]]}]],
            })),
            names(&["lines"]),
            total_spec,
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("tag_total", int(1))]));
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[4, tagged, 0], [1, tagged, {"tags": tag_commands}]]})),
            vec![],
            tag_spec,
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lines": [
            [4, tagged, {"id": tagged, "tags": [
                {"id": t1, "name": "t1"},
                {"id": t2, "name": "t2"},
            ]}],
            [1, tagged, {"tags": [[3, t1, 0], [0, "v_t", {"name": "new"}]]}],
        ]}))
    );

    // nothing reached the database: the sub-lines, the tags and the line-tag relation are as
    // created
    tx.flush().await.unwrap();
    assert_eq!(
        sub_rows(&mut tx).await,
        [
            (saved.s1, Some(saved.line), None, 1),
            (saved.s2, Some(saved.line), None, 2),
            (saved.s9, Some(saved.nine), None, 9),
        ]
    );
    let tag_rows: Vec<(i64, String)> = tx
        .query("SELECT id, name FROM test_web_multi_tag ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(tag_rows, [(t1, "t1".to_owned()), (t2, "t2".to_owned())]);
    let relation: Vec<(i64, i64)> = tx
        .query(
            "SELECT line_id, tag_id FROM test_web_multi_line_tag_rel ORDER BY line_id, tag_id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(relation, [(tagged, t1), (tagged, t2)]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_062_onchange_returns_nested_commands() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let saved = saved_multi(&mut tx).await;
    let tag = create(&mut tx, MULTI_TAG, &[&[("name", s("tag"))]]).await;
    let tag = id(&tag, 0);

    // the form's tags added to every line's tags, the lines read with `special_tag`
    let result = saved
        .multi
        .onchange(
            &mut tx,
            values(json!({
                "lines": [[0, "virtual1", {"name": "X", "tags": []}]],
                "tags": [[4, tag, 0]],
            })),
            names(&["tags"]),
            spec(json!({
                "name": {},
                "partner": {"fields": {"display_name": {}}},
                "lines": {
                    "context": {"special_tag": true},
                    "fields": {
                        "name": {},
                        "partner": {"fields": {"display_name": {}}},
                        "tags": {"fields": {"label": {}}},
                    },
                },
                "tags": {"fields": {"label": {}}},
            })),
        )
        .await
        .unwrap();
    let linked = v(json!([[4, tag, {"id": tag, "label": "tag!"}]]));
    assert_eq!(
        value_of(&result),
        obj(&[(
            "lines",
            Value::list([
                Value::list([int(1), int(saved.line), obj(&[("tags", linked.clone())])]),
                Value::list([int(1), s("virtual1"), obj(&[("tags", linked)])]),
            ])
        )])
    );

    // an onchange method changing the sub-lines of a saved and of a new line
    let fields = spec(json!({
        "total": {},
        "lines": {"fields": {"amount": {}, "sub_ids": {"fields": {"name": {}, "amount": {}}}}},
    }));
    let form = |bonus: i64| {
        values(json!({
            "total": 3,
            "bonus": bonus,
            "lines": [[0, "v_l2", {"sub_ids": [[0, "v_s3", {"name": "x", "amount": 4}]]}]],
        }))
    };
    // every amount raised
    let result = saved
        .multi
        .onchange(&mut tx, form(10), names(&["bonus"]), fields.clone())
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({
            "total": 37,
            "lines": [
                [1, saved.line, {"amount": 23, "sub_ids": [
                    [1, saved.s1, {"amount": 11}],
                    [1, saved.s2, {"amount": 12}],
                ]}],
                [1, "v_l2", {"amount": 14, "sub_ids": [[1, "v_s3", {"amount": 14}]]}],
            ],
        }))
    );
    // a sub-line added to every line
    let result = saved
        .multi
        .onchange(&mut tx, form(100), names(&["bonus"]), fields.clone())
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({
            "total": 207,
            "lines": [
                [1, saved.line, {"amount": 103, "sub_ids": [[0, 0, {"name": "bonus", "amount": 100}]]}],
                [1, "v_l2", {"amount": 104, "sub_ids": [[0, 0, {"name": "bonus", "amount": 100}]]}],
            ],
        }))
    );
    // every line's first sub-line removed
    let result = saved
        .multi
        .onchange(&mut tx, form(-1), names(&["bonus"]), fields)
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({
            "total": 2,
            "lines": [
                [1, saved.line, {"amount": 2, "sub_ids": [[2, saved.s1, 0]]}],
                [1, "v_l2", {"amount": 0, "sub_ids": [[2, "v_s3", 0]]}],
            ],
        }))
    );

    // a first call: the linked saved line comes back with its stored sub-lines, then an update
    // matching them by origin; the new line holds a new and a linked sub-line
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({
                "bonus": 10,
                "lines": [
                    [4, saved.line, 0],
                    [0, "v1", {"sub_ids": [
                        [0, "v2", {"name": "n", "amount": 1}],
                        [4, saved.s9, 0],
                    ]}],
                ],
            })),
            vec![],
            spec(json!({
                "lines": {"fields": {"amount": {}, "sub_ids": {"fields": {"name": {}, "amount": {}}}}},
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lines": [
            [4, saved.line, {"id": saved.line, "amount": 3, "sub_ids": [
                {"id": saved.s1, "name": "s1", "amount": 1},
                {"id": saved.s2, "name": "s2", "amount": 2},
            ]}],
            [1, saved.line, {"amount": 23, "sub_ids": [
                [1, saved.s1, {"amount": 11}],
                [1, saved.s2, {"amount": 12}],
            ]}],
            [0, "v1", {"amount": 30, "sub_ids": [
                [0, "v2", {"name": "n", "amount": 11}],
                [4, saved.s9, {"id": saved.s9, "name": "s9", "amount": 9}],
                [1, saved.s9, {"amount": 19}],
            ]}],
        ]}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// tag linked twice and set twice inside a new line)
#[tokio::test]
async fn nested_duplicate_commands_name_one_line() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let saved = saved_multi(&mut tx).await;
    let tag = create(&mut tx, MULTI_TAG, &[&[("name", s("t"))]]).await;
    let t = id(&tag, 0);
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [
                [6, 0, [saved.line, saved.line]],
                [0, "v1", {"tags": [[4, t, 0], [4, t, 0], [6, 0, [t, t]]]}],
            ]})),
            vec![],
            spec(json!({"lines": {"fields": {"name": {}, "tags": {}}}})),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lines": [
            [4, saved.line, {"id": saved.line, "name": "one", "tags": []}],
            [0, "v1", {"name": null, "tags": [[4, t, {"id": t}]]}],
        ]}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nested_values_of_removed_lines_still_apply() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tag = create(&mut tx, MULTI_TAG, &[&[("name", s("tag"))]]).await;
    let t = id(&tag, 0);
    let multi = create(&mut tx, MULTI, &[&[]]).await;
    let lines = create(
        &mut tx,
        MULTI_LINE,
        &[
            &[
                ("multi", int(id(&multi, 0))),
                ("tags", Value::list([int(t)])),
            ],
            &[
                ("multi", int(id(&multi, 0))),
                ("tags", Value::list([int(t)])),
            ],
        ],
    )
    .await;
    let (removed, kept) = (id(&lines, 0), id(&lines, 1));
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [
                [4, removed, 0],
                [1, removed, {"tags": [[1, t, {"name": "renamed"}]]}],
                [2, removed, 0],
                [4, kept, 0],
            ]})),
            vec![],
            spec(json!({"lines": {"fields": {"tags": {"fields": {"name": {}}}}}})),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lines": [
            [4, kept, {"id": kept, "tags": [{"id": t, "name": "tag"}]}],
            [1, kept, {"tags": [[1, t, {"name": "renamed"}]]}],
        ]}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A first call's values whose command lists are nested `levels` deep: `lines` (level 1),
/// `sub_ids` (level 2), then `child_ids` (levels 3 and more); every sub-line has amount 1.
fn nested_values(levels: usize) -> ValueMap {
    assert!(levels >= 2, "{levels}");
    let mut list = json!([[0, 0, {"name": "leaf", "amount": 1}]]);
    for _ in 2..levels {
        list = json!([[0, 0, {"amount": 1, "child_ids": list}]]);
    }
    values(json!({"lines": [[0, 0, {"sub_ids": list}]]}))
}

/// Untrusted input (engineering standards §4): command lists nested 16 levels deep are applied
/// (the total sees the sub-line at level 2); one level more is a `Validation` error naming the
/// field.
#[tokio::test]
async fn nested_commands_depth_is_bounded() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            nested_values(erp_orm::MAX_SPEC_DEPTH),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("total", int(1))]));
    let err = multis
        .onchange(
            &mut tx,
            nested_values(erp_orm::MAX_SPEC_DEPTH + 1),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_web.multi_sub.child_ids: x2many commands nested deeper than 16 \
         levels"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Untrusted input: more than 100,000 line commands in one call are a `Validation` error naming
/// the field whose commands exceed the budget, counted over every level (a `SET` counts its ids)
/// and before the commands are applied (the ids need not exist).
#[tokio::test]
async fn nested_commands_count_is_bounded() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    let ids: Vec<i64> = (1..=100_000).collect();
    let err = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[0, 0, {"sub_ids": [[6, 0, ids]]}]]})),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_web.multi_line.sub_ids: more than 100000 x2many commands in one \
         onchange"
    );
    let many: Vec<_> = (0..100_001).map(|_| json!([0, 0, {}])).collect();
    let err = multis
        .onchange(
            &mut tx,
            values(json!({"lines": many})),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_web.multi.lines: more than 100000 x2many commands in one onchange"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Untrusted input: snapshots and diffs recurse no deeper than a specification may be parsed (16
/// levels), also for one built in code. With saved records at every depth down to 17 (the form,
/// its line, a sub-line and a chain of 14 children), a 16-level specification works and a
/// 17-level one is a `Validation` error.
#[tokio::test]
async fn nested_specification_depth_is_bounded() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let saved = saved_multi(&mut tx).await;
    let mut parent = saved.s1;
    for _ in 0..14 {
        let child = create(
            &mut tx,
            MULTI_SUB,
            &[&[("parent_id", int(parent)), ("amount", int(1))]],
        )
        .await;
        parent = id(&child, 0);
    }
    // depth 1 the form, 2 its lines, 3 their sub-lines, then one level per `child_ids`; every
    // sub-line level reads `amount`
    let fields = |levels: usize| {
        let mut item = Spec::new().field("amount");
        for _ in 3..levels {
            item = Spec::new()
                .field("amount")
                .with("child_ids", erp_orm::SpecItem::fields(item));
        }
        Spec::new().with(
            "lines",
            erp_orm::SpecItem::fields(Spec::new().with("sub_ids", erp_orm::SpecItem::fields(item))),
        )
    };
    let result = saved
        .multi
        .onchange(
            &mut tx,
            values(json!({"bonus": 1})),
            names(&["bonus"]),
            fields(erp_orm::MAX_SPEC_DEPTH),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lines": [[1, saved.line, {"sub_ids": [
            [1, saved.s1, {"amount": 2}],
            [1, saved.s2, {"amount": 3}],
        ]}]]}))
    );
    let err = saved
        .multi
        .onchange(
            &mut tx,
            values(json!({"bonus": 1})),
            names(&["bonus"]),
            fields(erp_orm::MAX_SPEC_DEPTH + 1),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Invalid specification: nested deeper than 16 levels"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Client input inside lines is checked like the form's own: a nested value that is not a command
/// list, an unknown command, a field the nested comodel does not have, a bad id are `Validation`
/// errors naming the nested model and field.
#[tokio::test]
async fn nested_bad_values_are_validation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    let cases = [
        (
            json!([[0, 0, {"sub_ids": "oops"}]]),
            "Wrong value for test_web.multi_line.sub_ids: 'oops'",
        ),
        (
            json!([[0, 0, {"sub_ids": [[9, 0, 0]]}]]),
            "Unknown command 9 for test_web.multi_line.sub_ids: [9, 0, 0]",
        ),
        (
            json!([[0, 0, {"sub_ids": [[3, 1, 7]]}]]),
            "Wrong value for test_web.multi_line.sub_ids: [3, 1, 7]",
        ),
        (
            json!([[0, 0, {"sub_ids": [[0, 0, {"nope": 1}]]}]]),
            "Invalid field 'nope' on model 'test_web.multi_sub'",
        ),
        (
            json!([[0, 0, {"sub_ids": [[0, 0, {"child_ids": [[1, -5, {}]]}]]}]]),
            "Wrong value for test_web.multi_sub.child_ids: -5",
        ),
    ];
    for (lines, message) in cases {
        let err = multis
            .onchange(
                &mut tx,
                values(json!({"lines": lines})),
                vec![],
                spec(json!({"total": {}})),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{lines}");
        assert_eq!(err.message(), message, "{lines}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Statements of one onchange call: how many, and which tables they touch (sorted).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cost {
    count: u64,
    tables: Vec<String>,
}

/// A saved form with `n` lines, each with its own tag and `subs` saved sub-lines, and per line
/// loose saved records to link: another tag, a spare sub-line (with a saved child of its own), a
/// loose sub-line (to link as a child) and a label.
struct CostForm {
    multi: Records,
    lines: Vec<i64>,
    extras: Vec<i64>,
    firsts: Vec<i64>,
    spares: Vec<i64>,
    loose: Vec<i64>,
    labels: Vec<i64>,
}

async fn nested_cost_fixture(tx: &mut Transaction, n: usize, subs: usize) -> CostForm {
    let names_of =
        |prefix: &str| -> Vec<String> { (0..n).map(|i| format!("{prefix}{i}")).collect() };
    let tag_names = names_of("tag ");
    let tag_rows: Vec<Vec<(&str, Value)>> = tag_names
        .iter()
        .map(|t| vec![("name", Value::str(t.clone()))])
        .collect();
    let tags = create(
        tx,
        MULTI_TAG,
        &tag_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
    .await
    .ids();
    let extra_names = names_of("extra ");
    let extra_rows: Vec<Vec<(&str, Value)>> = extra_names
        .iter()
        .map(|t| vec![("name", Value::str(t.clone()))])
        .collect();
    let extras = create(
        tx,
        MULTI_TAG,
        &extra_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
    .await
    .ids();
    let multi = create(tx, MULTI, &[&[], &[]]).await;
    let (m, m2) = (id(&multi, 0), id(&multi, 1));
    let line_rows: Vec<Vec<(&str, Value)>> = (0..n)
        .map(|i| vec![("multi", int(m)), ("tags", Value::list([int(tags[i])]))])
        .collect();
    let lines = create(
        tx,
        MULTI_LINE,
        &line_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
    .await
    .ids();
    let sub_rows: Vec<Vec<(&str, Value)>> = lines
        .iter()
        .flat_map(|l| (0..subs).map(move |_| vec![("line_id", int(*l)), ("amount", int(1))]))
        .collect();
    create(
        tx,
        MULTI_SUB,
        &sub_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
    .await;
    let holder = create(tx, MULTI_LINE, &[&[("multi", int(m2))]]).await;
    let holder = id(&holder, 0);
    let spare_rows: Vec<Vec<(&str, Value)>> = (0..n)
        .map(|_| vec![("line_id", int(holder)), ("amount", int(4))])
        .collect();
    let spares = create(
        tx,
        MULTI_SUB,
        &spare_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
    .await
    .ids();
    let kid_rows: Vec<Vec<(&str, Value)>> = spares
        .iter()
        .map(|sp| vec![("parent_id", int(*sp)), ("amount", int(6))])
        .collect();
    create(
        tx,
        MULTI_SUB,
        &kid_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
    .await;
    let loose_rows: Vec<Vec<(&str, Value)>> = (0..n).map(|_| vec![("amount", int(2))]).collect();
    let loose = create(
        tx,
        MULTI_SUB,
        &loose_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
    .await
    .ids();
    // the first saved sub-line of each line
    let firsts: Vec<i64> = tx
        .query(
            "SELECT min(id) FROM test_web_multi_sub WHERE line_id = ANY($1) GROUP BY line_id \
             ORDER BY line_id",
            &[&lines],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let label_names = names_of("label ");
    let label_rows: Vec<Vec<(&str, Value)>> = label_names
        .iter()
        .map(|t| vec![("name", Value::str(t.clone()))])
        .collect();
    let labels = create(
        tx,
        MULTI_LABEL,
        &label_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
    .await
    .ids();
    CostForm {
        multi: tx.env().records(MULTI).unwrap().browse([m]).unwrap(),
        lines,
        extras,
        firsts,
        spares,
        loose,
        labels,
    }
}

/// The cost of the nested calls (the commands applied, the nested changes an onchange method
/// returns, the labels a method links, and with `first_call` a first call) for a form with `n`
/// lines of `subs` sub-lines, as user `uid` (the harness superuser when `None`); `children` has the
/// sub-lines' `child_ids` in the specification.
async fn nested_cost(
    h: &Harness,
    uid: Option<i64>,
    n: usize,
    subs: usize,
    children: bool,
    first_call: bool,
) -> Vec<Cost> {
    let mut tx = h.begin().await.unwrap();
    let CostForm {
        multi,
        lines,
        extras,
        firsts,
        spares,
        loose,
        labels,
    } = nested_cost_fixture(&mut tx, n, subs).await;
    let multi = match uid {
        Some(uid) => tx
            .env()
            .with_user(uid)
            .records(MULTI)
            .unwrap()
            .browse(multi.ids())
            .unwrap(),
        None => multi,
    };
    let multis = match uid {
        Some(uid) => tx.env().with_user(uid).records(MULTI).unwrap(),
        None => tx.env().records(MULTI).unwrap(),
    };
    let mut sub_fields = json!({
        "amount": {},
        "line_name": {},
        "labels": {"fields": {"name": {}}},
    });
    if children {
        sub_fields["child_ids"] = json!({"fields": {"amount": {}}});
    }
    let fields = spec(json!({
        "total": {},
        "lines": {"fields": {
            "name": {},
            "tags": {"fields": {"name": {}}},
            "sub_ids": {"fields": sub_fields},
        }},
    }));
    let n_i64 = i64::try_from(n).unwrap();
    let subs_i64 = i64::try_from(subs).unwrap();
    let mut costs = Vec::new();
    let mut measure = |tx: &mut Transaction, before: u64| {
        let statements = tx.captured().to_vec();
        tx.capture_statements(false);
        costs.push(Cost {
            count: tx.query_count() - before,
            tables: tables_of(&statements),
        });
    };

    // (1) commands applied: a tag, sub-line updates with a loose child linked, a new sub-line with
    // a label and a spare sub-line linked on every line
    let commands: Vec<_> = (0..n)
        .map(|i| {
            json!([1, lines[i], {
                "tags": [[4, extras[i], 0]],
                "sub_ids": [
                    [1, firsts[i], {"amount": 5, "child_ids": [[4, loose[i], 0]]}],
                    [0, 0, {"amount": 3, "labels": [[4, labels[i], 0]]}],
                    [4, spares[i], 0],
                ],
            }])
        })
        .collect();
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    let before = tx.query_count();
    let result = multi
        .onchange(
            &mut tx,
            values(json!({"lines": commands})),
            names(&["lines"]),
            fields.clone(),
        )
        .await
        .unwrap();
    measure(&mut tx, before);
    // each line's sub-lines: the first raised to 5, a new 3, the spare 4, the loose child (2)
    // belongs to the first sub-line and counts for nothing in the total
    assert_eq!(
        value_of(&result),
        obj(&[("total", int(n_i64 * (subs_i64 - 1 + 5 + 3 + 4)))]),
        "{n} lines"
    );

    // (2) the nested changes an onchange method returns
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    let before = tx.query_count();
    let result = multi
        .onchange(
            &mut tx,
            values(json!({"bonus": 1})),
            names(&["bonus"]),
            fields.clone(),
        )
        .await
        .unwrap();
    measure(&mut tx, before);
    let value = map_of(&value_of(&result));
    assert_eq!(value["total"], int(n_i64 * subs_i64 * 2), "{value:?}");
    let Some(Value::List(changed)) = value.get("lines") else {
        panic!("{value:?}")
    };
    assert_eq!(changed.len(), n, "{value:?}");

    // (2b) records no snapshot has read: an onchange method links the k-th label to every
    // sub-line of the k-th line
    let linking =
        multi.with_context([("link_labels", Value::list(labels.iter().map(|l| int(*l))))]);
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    let before = tx.query_count();
    let result = linking
        .onchange(
            &mut tx,
            values(json!({"bonus": Multi::BONUS_LINK_LABELS})),
            names(&["bonus"]),
            fields.clone(),
        )
        .await
        .unwrap();
    measure(&mut tx, before);
    let value = map_of(&value_of(&result));
    let Some(Value::List(changed)) = value.get("lines") else {
        panic!("{value:?}")
    };
    assert_eq!(changed.len(), n, "{value:?}");

    if first_call {
        // (3) a first call: a new line per saved line, its saved tag and a new and a spare sub-line
        let commands: Vec<_> = (0..n)
            .map(|i| {
                json!([0, format!("v{i}"), {
                    "name": "L",
                    "tags": [[4, extras[i], 0]],
                    "sub_ids": [
                        [0, 0, {"amount": 1, "labels": [[4, labels[i], 0]]}],
                        [4, spares[i], 0],
                    ],
                }])
            })
            .collect();
        tx.invalidate_all().await.unwrap();
        tx.capture_statements(true);
        let before = tx.query_count();
        let result = multis
            .onchange(&mut tx, values(json!({"lines": commands})), vec![], fields)
            .await
            .unwrap();
        measure(&mut tx, before);
        let value = map_of(&value_of(&result));
        assert_eq!(value["total"], int(n_i64 * 5), "{value:?}");
    }
    drop(tx);
    costs
}

/// A cost: one statement per table read, `(table suffix, statements)`.
fn cost(reads: &[(&str, usize)]) -> Cost {
    let mut tables: Vec<String> = reads
        .iter()
        .flat_map(|(table, n)| (0..*n).map(move |_| format!("FROM test_web_multi_{table}")))
        .collect();
    tables.sort();
    Cost {
        count: u64::try_from(tables.len()).unwrap(),
        tables,
    }
}

/// GRP §8 W5 one level down: applying nested commands, returning the nested changes an onchange
/// method makes and a first call cost as many statements for 30 lines of 20 sub-lines as for 3
/// lines of 2, each line with its own tag, its own newly linked sub-line (with a child of its own)
/// and its own newly linked child and label (some of them the records of a stored line no earlier
/// step has read: the label an onchange method links per line). The statements are pinned by the
/// tables they read (the log of the statements, not only their number), so a regression that swaps
/// one read for another fails too. Run as the superuser, and as a user the record rules apply to.
///
/// The user's run covers the same four steps with exact counts, the same for both sizes.
#[tokio::test]
async fn nested_onchange_cost_does_not_grow_with_lines() {
    // Superuser. Statements of each call:
    // (1) the commands applied: the form's lines (`line`); their sub-lines (o2m by `line_id`),
    //     the children the sub-lines hold (o2m by `parent_id`, with its domain) and the spare
    //     sub-lines' children (the same read for the stored lines the commands link), the rows
    //     of the sub-lines and of the loose children the commands name (the inverse pre-read of
    //     the level, one per level); the sub-lines' labels (m2m) and the rows of the labels
    //     linked; the lines' tags (m2m), their names and the linked tags'.
    // (2) the nested changes of an onchange method: lines, sub-lines, children (o2m reads),
    //     labels and tags (m2m reads), the tags' names.
    // (3) an onchange method links a label no step has read: as (2), the labels' names read
    //     once for all, and the labels' rows.
    // (4) a first call, the form's `lines` all new: the spare sub-lines' and the labels' rows,
    //     the spares' children, labels and tags, and the stored line the spares belong to.
    let su = [
        cost(&[("label", 4), ("line", 1), ("sub", 5), ("tag", 3)]),
        cost(&[("label", 1), ("line", 1), ("sub", 2), ("tag", 2)]),
        cost(&[("label", 4), ("line", 1), ("sub", 2), ("tag", 2)]),
        cost(&[("label", 3), ("line", 1), ("sub", 2), ("tag", 1)]),
    ];
    let h = harness().await;
    let small = nested_cost(&h, None, 3, 2, true, true).await;
    let large = nested_cost(&h, None, 30, 20, true, true).await;
    assert_eq!(small, su, "3 lines of 2 sub-lines");
    assert_eq!(large, su, "30 lines of 20 sub-lines");
    h.cleanup().await.unwrap();

    // A user the record rules apply to (here: a rule on the sub-lines), `child_ids` and the first
    // call included.
    // Writes to in-memory records (an onchange method setting amounts, lines linked in a
    // first call) do not end the decisions. Measured against the superuser's counts, the user
    // adds only reads of the sub-lines (the rule's queries, one per step for the records not
    // decided yet, and the origins' children): four in (1), one in (2), one in (3), two in (4).
    let user = [
        cost(&[("label", 4), ("line", 1), ("sub", 9), ("tag", 3)]),
        cost(&[("label", 1), ("line", 1), ("sub", 3), ("tag", 2)]),
        cost(&[("label", 4), ("line", 1), ("sub", 3), ("tag", 2)]),
        cost(&[("label", 3), ("line", 1), ("sub", 4), ("tag", 1)]),
    ];
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(HideHiddenSubs)
        .build()
        .await
        .unwrap();
    let small = nested_cost(&h, Some(2), 3, 2, true, true).await;
    let large = nested_cost(&h, Some(2), 30, 20, true, true).await;
    assert_eq!(small, user, "3 lines of 2 sub-lines, as a user");
    assert_eq!(large, user, "30 lines of 20 sub-lines, as a user");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nested_link_of_a_missing_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    let err = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[0, 0, {"sub_ids": [[4, 999_999_999, 0]]}]]})),
            vec![],
            spec(json!({"lines": {"fields": {"sub_ids": {"fields": {"amount": {}}}}}})),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        format!(
            "Record does not exist or has been deleted.\n(Record: test_web.multi_sub(999999999,), \
             User: {})",
            tx.env().uid()
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Boundary ids from the client: an id an in-memory record cannot carry (2^62 and more, up to
/// `i64::MAX`) is a `Validation` error like any refused input, at the form's level and inside a
/// line.
#[tokio::test]
async fn nested_commands_refuse_ids_beyond_the_in_memory_range() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    for id in [1_i64 << 62, i64::MAX] {
        for lines in [
            json!([[4, id, 0]]),
            json!([[0, 0, {"sub_ids": [[1, id, {"amount": 1}]]}]]),
            json!([[0, 0, {"sub_ids": [[6, 0, [id]]]}]]),
        ] {
            let err = multis
                .onchange(
                    &mut tx,
                    values(json!({"lines": lines})),
                    vec![],
                    spec(json!({"total": {}})),
                )
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Validation, "{lines}");
            assert_eq!(err.message(), format!("Invalid record id {id}"), "{lines}");
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A cycle in the values (a sub-line made its own child through its client reference) ends: the
/// snapshots and the diff follow the specification, not the records.
#[tokio::test]
async fn nested_cycle_in_values_ends() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[0, "l", {"sub_ids": [[0, "c", {
                "name": "c",
                "child_ids": [[1, "c", {"amount": 5}]],
            }]]}]]})),
            vec![],
            spec(json!({"lines": {"fields": {"sub_ids": {"fields": {
                "name": {},
                "amount": {},
                "child_ids": {"fields": {"name": {}, "child_ids": {}}},
            }}}}})),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lines": [[0, "l", {"sub_ids": [[0, "c", {
            "name": "c",
            "amount": 5,
            "child_ids": [[0, "c", {"name": "c", "child_ids": [[0, "c", {}]]}]],
        }]]}]]}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The users the rule explanations name (the access evidence reads the asking user's name).
async fn seed_users(tx: &mut Transaction) {
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true), \
         (3, 'demo3', 'demo3', true) ON CONFLICT (id) DO NOTHING",
    )
    .await
    .unwrap();
}

// ----- review round ---------------------------------------------------------------

const MULTI_LABEL: &str = "test_web.multi_label";

/// Sub-lines named "hidden" are hidden from reads outside superuser mode (a record rule).
struct HideHiddenSubs;

impl erp_testkit::TestRules for HideHiddenSubs {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        Ok(match (model.name, op) {
            (MULTI_SUB, Operation::Read) => {
                Domain::condition("name", erp_sdk::Operator::Ne, "hidden").unwrap()
            }
            _ => Domain::TRUE,
        })
    }
}

/// The table each recorded statement reads or writes (the first quoted name after `FROM`, `INTO`
/// or `UPDATE`), sorted: which relations a call touched, whatever the order of the statements.
fn tables_of(statements: &[String]) -> Vec<String> {
    let mut out: Vec<String> = statements
        .iter()
        .map(|q| {
            for keyword in ["FROM \"", "INTO \"", "UPDATE \""] {
                if let Some(start) = q.find(keyword) {
                    let rest = &q[start + keyword.len()..];
                    let name = rest.split('"').next().unwrap_or_default();
                    return format!("{} {name}", keyword.split_whitespace().next().unwrap());
                }
            }
            q.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
        })
        .collect();
    out.sort();
    out
}

/// As a user who cannot read the sub-line named "hidden", a saved line also holds an archived
/// sub-line; one of its sub-lines carries an active and an archived label. An onchange that
/// updates the line succeeds (no `Access` error for the hidden sub-line), and neither the hidden
/// nor the archived sub-line, nor the archived label, is part of the values, the total or the
/// returned commands.
#[tokio::test]
async fn nested_lines_follow_rules_and_active_of_their_origin() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(HideHiddenSubs)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let labels = create(
        &mut tx,
        MULTI_LABEL,
        &[
            &[("name", s("shown"))],
            &[("name", s("archived")), ("active", Value::Bool(false))],
        ],
    )
    .await;
    let (shown, archived) = (id(&labels, 0), id(&labels, 1));
    let multi = create(&mut tx, MULTI, &[&[]]).await;
    let line = create(
        &mut tx,
        MULTI_LINE,
        &[&[("multi", int(id(&multi, 0))), ("name", s("one"))]],
    )
    .await;
    let line = id(&line, 0);
    let subs = create(
        &mut tx,
        MULTI_SUB,
        &[
            &[
                ("line_id", int(line)),
                ("name", s("open")),
                ("amount", int(1)),
                ("labels", v(json!([[6, 0, [shown, archived]]]))),
            ],
            &[
                ("line_id", int(line)),
                ("name", s("hidden")),
                ("amount", int(100)),
            ],
            &[
                ("line_id", int(line)),
                ("name", s("archived")),
                ("amount", int(50)),
                ("active", Value::Bool(false)),
            ],
        ],
    )
    .await;
    let open = id(&subs, 0);
    let uid = 2;
    // a first call on a new form that links the saved line and updates its sub-lines: the line is
    // compared with the stored line, which the user reads through the rules and the active test
    let multis = tx.env().with_user(uid).records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [
                [4, line, 0],
                [1, line, {"sub_ids": [
                    [1, open, {"labels": [[5, 0, 0]]}],
                    [0, "n1", {"amount": 7}],
                ]}],
            ]})),
            vec![],
            spec(json!({
                "total": {},
                "lines": {"fields": {"sub_ids": {"fields": {
                    "name": {},
                    "amount": {},
                    "labels": {"fields": {"name": {}}},
                }}}},
            })),
        )
        .await
        .unwrap();
    // the total counts the open sub-line and the new one only; the stored line holds the open
    // sub-line and its one visible label; the update takes that label off and adds the new
    // sub-line
    assert_eq!(
        value_of(&result),
        v(json!({
            "total": 8,
            "lines": [
                [4, line, {"id": line, "sub_ids": [
                    {"id": open, "name": "open", "amount": 1, "labels": [{"id": shown, "name": "shown"}]},
                ]}],
                [1, line, {"sub_ids": [
                    [1, open, {"labels": [[3, shown, 0]]}],
                    [0, "n1", {"name": null, "amount": 7}],
                ]}],
            ],
        }))
    );
    // the same as changed values on the saved form: the total
    let saved = multis.browse([id(&multi, 0)]).unwrap();
    let result = saved
        .onchange(
            &mut tx,
            values(json!({"lines": [[1, line, {"sub_ids": [[0, "n1", {"amount": 7}]]}]]})),
            names(&["lines"]),
            spec(
                json!({"total": {}, "lines": {"fields": {"sub_ids": {"fields": {"amount": {}}}}}}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("total", int(8))]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// R3/R1: the labels of in-memory sub-lines (the form's lines, their sub-lines, their labels: three
/// levels) list their sub-lines in the mirror many2many, as the form's own many2many lines do: the
/// stored label counts the two sub-lines that carry it, the created label the one.
#[tokio::test]
async fn mirror_many2many_is_kept_three_levels_deep() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let label = create(&mut tx, MULTI_LABEL, &[&[("name", s("A"))]]).await;
    let a = id(&label, 0);
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[0, "v1", {"sub_ids": [
                [0, "v2", {"labels": [[4, a, 0], [0, "v_x", {"name": "x"}]]}],
                [0, "v3", {"labels": [[4, a, 0]]}],
            ]}]]})),
            vec![],
            spec(json!({"lines": {"fields": {"sub_ids": {"fields": {
                "labels": {"fields": {"name": {}, "sub_count": {}}},
            }}}}})),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lines": [[0, "v1", {"sub_ids": [
            [0, "v2", {"labels": [
                [4, a, {"id": a, "name": "A", "sub_count": 0}],
                [1, a, {"sub_count": 2}],
                [0, "v_x", {"name": "x", "sub_count": 1}],
            ]}],
            [0, "v3", {"labels": [
                [4, a, {"id": a, "name": "A", "sub_count": 0}],
                [1, a, {"sub_count": 2}],
            ]}],
        ]}]]}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// R1: many sub-lines carrying one label cost linear time in the mirror patching. Each label
/// command of a sub-line adds the sub-line to the label's mirror value; doing it one sub-line at a
/// time rebuilt the label's whole list each time (quadratic in the number of sub-lines).
#[tokio::test]
#[ignore = "DEFERRED_PERFORMANCE: milestone-end timing gate; semantic coverage runs in ordinary CI"]
async fn nested_many_lines_on_one_label_are_linear() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let label = create(&mut tx, MULTI_LABEL, &[&[("name", s("A"))]]).await;
    let a = id(&label, 0);
    let multis = tx.env().records(MULTI).unwrap();
    // 1 line + n sub-lines + n label commands: 99,999 commands, the budget is 100,000
    let n = 49_999;
    let subs: Vec<_> = (0..n)
        .map(|_| json!([0, 0, {"labels": [[4, a, 0]]}]))
        .collect();
    let started = std::time::Instant::now();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[0, 0, {"sub_ids": subs}]]})),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(value_of(&result), obj(&[("total", int(0))]));
    assert!(
        elapsed < std::time::Duration::from_secs(LINEAR_SECONDS),
        "{n} sub-lines on one label took {elapsed:?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

const LINEAR_SECONDS: u64 = 10;

/// D3: one x2many value of 50,000 created lines (within the budget) is linked back to the form's
/// record in linear time: the inverse one2many value already holds every line, and merging the
/// moved lines into it must not scan it once per line.
#[tokio::test]
#[ignore = "DEFERRED_PERFORMANCE: milestone-end timing gate; semantic coverage runs in ordinary CI"]
async fn many_created_lines_link_in_linear_time() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    let lines: Vec<_> = (0..100_000).map(|_| json!([0, 0, {}])).collect();
    let started = std::time::Instant::now();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": lines})),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(value_of(&result), obj(&[("total", int(0))]));
    assert!(
        elapsed < std::time::Duration::from_secs(LINEAR_SECONDS),
        "100,000 lines took {elapsed:?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// R6: the budget is inclusive: exactly 100,000 line commands are applied (`nested_commands_count_is_bounded`
/// shows 100,001 refused). The budget bounds the commands, and so the records one call builds or
/// reads: each command names or builds at most one line.
#[tokio::test]
async fn nested_commands_budget_is_inclusive() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    // 1 line + 99,999 sub-lines
    let subs: Vec<_> = (0..99_999).map(|_| json!([0, 0, {"amount": 1}])).collect();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[0, 0, {"sub_ids": subs}]]})),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("total", int(99_999))]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// R2: the work of snapshots and diffs is bounded, not only their depth. 60 sub-lines that all carry
/// the same 60 labels (3,600 commands) are a dense graph (each label lists the 60 sub-lines through
/// the mirror many2many): a specification that follows `labels` and `sub_ids` alternately 14 levels
/// deep describes 60^k paths of length k. The call fails with a `Validation` error naming the field,
/// after a bounded number of visits. This is the first call of a new form (the dense graph is
/// walked by the snapshot after the commands and by the diff).
#[tokio::test]
async fn nested_fan_out_work_is_bounded() {
    fan_out_trips_the_budget(false).await;
}

/// R2 on a saved form with changed values: the dense graph is part of the values sent with
/// `bonus` (which changed), so snapshot 0 (the values before the change) and the comparison of
/// the final values with it walk the graph too, not only the final snapshot of a first call.
#[tokio::test]
async fn nested_fan_out_work_is_bounded_for_changed_values() {
    fan_out_trips_the_budget(true).await;
}

async fn fan_out_trips_the_budget(saved_form: bool) {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    let fan = 60;
    let all: Vec<_> = (0..fan).map(|i| json!([1, format!("k{i}"), {}])).collect();
    let subs: Vec<_> = (0..fan)
        .map(|i| json!([0, format!("c{i}"), {"labels": all}]))
        .collect();
    // the form (depth 1), its lines (2), then sub-lines at the odd depths and labels at the even
    // ones, down to the deepest level a specification may have
    let mut item = Spec::new().field("name");
    for depth in (3..erp_orm::MAX_SPEC_DEPTH).rev() {
        item = if depth % 2 == 1 {
            Spec::new()
                .field("amount")
                .with("labels", erp_orm::SpecItem::fields(item))
        } else {
            Spec::new()
                .field("name")
                .with("sub_ids", erp_orm::SpecItem::fields(item))
        };
    }
    let fields = Spec::new().with(
        "lines",
        erp_orm::SpecItem::fields(Spec::new().with("sub_ids", erp_orm::SpecItem::fields(item))),
    );
    let (err, visits) = erp_orm::count_onchange_visits_for_tests(async {
        if saved_form {
            let form = create(&mut tx, MULTI, &[&[]]).await;
            form.onchange(
                &mut tx,
                values(json!({"bonus": 0, "lines": [[0, "l", {"sub_ids": subs}]]})),
                names(&["bonus"]),
                fields,
            )
            .await
            .unwrap_err()
        } else {
            multis
                .onchange(
                    &mut tx,
                    values(json!({"lines": [[0, "l", {"sub_ids": subs}]]})),
                    vec![],
                    fields,
                )
                .await
                .unwrap_err()
        }
    })
    .await;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_web.multi_sub.labels: the specification and the values visit more \
         than 1000000 lines in one onchange"
    );
    // Count the actual charged work, including the first rejected visit. Scheduler delays
    // cannot change this assertion, and postponing the guard until after more work fails it.
    assert_eq!(visits, 1_000_001);
    eprintln!("onchange attempted line visits: {visits}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The earlier
/// line is left without it, no error.
#[tokio::test]
async fn nested_sub_line_claimed_by_two_lines_ends_on_the_later_one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let saved = saved_multi(&mut tx).await;
    let multis = tx.env().records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [
                [0, "a", {"name": "A", "sub_ids": [[4, saved.s9, 0]]}],
                [0, "b", {"name": "B", "sub_ids": [[4, saved.s9, 0]]}],
            ]})),
            vec![],
            spec(json!({"lines": {"fields": {
                "name": {},
                "sub_ids": {"fields": {"amount": {}, "line_name": {}}},
            }}})),
        )
        .await
        .unwrap();
    assert_eq!(
        value_of(&result),
        v(json!({"lines": [
            [0, "a", {"name": "A"}],
            [0, "b", {"name": "B", "sub_ids": [
                [4, saved.s9, {"id": saved.s9, "amount": 9, "line_name": "nine"}],
                [1, saved.s9, {"line_name": "B"}],
            ]}],
        ]}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The stored count of sub-lines of every line: `(id, sub_count)` in id order.
async fn line_counts(tx: &mut Transaction) -> Vec<(i64, i32)> {
    tx.query(
        "SELECT id, sub_count FROM test_web_multi_line ORDER BY id",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect()
}

/// D4: relinking in-memory sub-lines from a saved line to another line must not touch the saved
/// line's stored count of sub-lines (a stored dependent of `sub_ids`): no recompute, no write, no
/// SAVEPOINT inside the onchange, and the stored values are as they were afterwards.
#[tokio::test]
async fn nested_relink_does_not_recompute_stored_dependents_of_saved_lines() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let saved = saved_multi(&mut tx).await;
    let before = line_counts(&mut tx).await;
    assert_eq!(before, [(saved.line, 2), (saved.nine, 1)]);
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    // s1 and s2 move from the saved line to the saved line "nine"; a bonus is added to every
    // sub-line afterwards, so the onchange method runs on the relinked lines
    let result = saved
        .multi
        .onchange(
            &mut tx,
            values(json!({"bonus": 1, "lines": [
                [1, saved.line, {"sub_ids": [[3, saved.s1, 0], [3, saved.s2, 0]]}],
                [4, saved.nine, 0],
                [1, saved.nine, {"sub_ids": [[4, saved.s1, 0], [4, saved.s2, 0]]}],
            ]})),
            names(&["lines", "bonus"]),
            spec(json!({"lines": {"fields": {"sub_count": {}, "amount": {}}}})),
        )
        .await
        .unwrap();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    assert!(
        statements
            .iter()
            .all(|q| !q.contains("SAVEPOINT") && !q.contains("UPDATE \"")),
        "{statements:#?}"
    );
    drop(result);
    assert_eq!(line_counts(&mut tx).await, before);
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- re-review: visibility decided at every read (F1, F3, F4, F5) --------------------

/// A saved line with three sub-lines: "open" (amount 1), "hidden" (100, hidden from user 2 by
/// [`HideHiddenSubs`]) and "archived" (50, `active` false).
struct HiddenSubs {
    line: Records,
    open: i64,
    hidden: i64,
    archived: i64,
}

async fn hidden_subs(tx: &mut Transaction) -> HiddenSubs {
    let multi = create(tx, MULTI, &[&[]]).await;
    let line = create(tx, MULTI_LINE, &[&[("multi", int(id(&multi, 0)))]]).await;
    let l = int(id(&line, 0));
    let subs = create(
        tx,
        MULTI_SUB,
        &[
            &[
                ("line_id", l.clone()),
                ("name", s("open")),
                ("amount", int(1)),
            ],
            &[
                ("line_id", l.clone()),
                ("name", s("hidden")),
                ("amount", int(100)),
            ],
            &[
                ("line_id", l),
                ("name", s("archived")),
                ("amount", int(50)),
                ("active", Value::Bool(false)),
            ],
        ],
    )
    .await;
    HiddenSubs {
        line,
        open: id(&subs, 0),
        hidden: id(&subs, 1),
        archived: id(&subs, 2),
    }
}

/// An in-memory record of the saved line (its origin), in the harness superuser's environment.
async fn new_line_of(tx: &mut Transaction, fixture: &HiddenSubs) -> Records {
    let model = tx.env().records(MULTI_LINE).unwrap();
    tx.new_record(
        &model,
        RawVals::new(),
        Some(fixture.line.rec_ids()[0]),
        None,
    )
    .await
    .unwrap()
}

/// The sub-lines `line` lists: their origin ids, names and amounts (reading the values must not
/// fail either: a hidden sub-line would raise `Access` there).
async fn sub_values(tx: &mut Transaction, line: &Records) -> (Vec<i64>, Vec<Value>, Vec<Value>) {
    let subs = line.related(tx, "sub_ids").await.unwrap();
    let names = subs.values(tx, "name").await.unwrap();
    let amounts = subs.values(tx, "amount").await.unwrap();
    (subs.ids(), names, amounts)
}

fn only_open(fixture: &HiddenSubs) -> (Vec<i64>, Vec<Value>, Vec<Value>) {
    (vec![fixture.open], vec![s("open")], vec![int(1)])
}

fn open_and_hidden(fixture: &HiddenSubs) -> (Vec<i64>, Vec<Value>, Vec<Value>) {
    (
        vec![fixture.open, fixture.hidden],
        vec![s("open"), s("hidden")],
        vec![int(1), int(100)],
    )
}

fn inactive() -> [(&'static str, Value); 1] {
    [("active_test", Value::Bool(false))]
}

async fn hide_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(HideHiddenSubs)
        .build()
        .await
        .unwrap()
}

/// Reading the
/// stored `sub_count` first (computed as superuser, listing every sub-line) and the sub-lines
/// after it as user 2 gives the open sub-line only, with no `Access` error; then the superuser
/// reads the same record and sees the hidden sub-line too (F3). The reverse order gives the same.
#[tokio::test]
async fn in_memory_line_lists_what_the_reader_may_see_whoever_read_first() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;

    // `sub_count` first, then `sub_ids`
    let line = new_line_of(&mut tx, &fixture).await;
    let user = line.with_user(2);
    let count = user.value(&mut tx, "sub_count").await.unwrap();
    assert!(matches!(count, Value::Int(_)), "{count}");
    assert_eq!(sub_values(&mut tx, &user).await, only_open(&fixture));
    // the superuser after the user: all but the archived one (F3)
    assert_eq!(
        sub_values(&mut tx, &line.sudo(true)).await,
        open_and_hidden(&fixture)
    );
    // and the user again
    assert_eq!(sub_values(&mut tx, &user).await, only_open(&fixture));

    // the reverse order, on a fresh record
    tx.invalidate_all().await.unwrap();
    let line = new_line_of(&mut tx, &fixture).await;
    let user = line.with_user(2);
    assert_eq!(sub_values(&mut tx, &user).await, only_open(&fixture));
    let count_after = user.value(&mut tx, "sub_count").await.unwrap();
    assert_eq!(count_after, count);
    assert_eq!(sub_values(&mut tx, &user).await, only_open(&fixture));

    // the superuser first, then the user
    tx.invalidate_all().await.unwrap();
    let line = new_line_of(&mut tx, &fixture).await;
    assert_eq!(
        sub_values(&mut tx, &line.sudo(true)).await,
        open_and_hidden(&fixture)
    );
    assert_eq!(
        sub_values(&mut tx, &line.with_user(2)).await,
        only_open(&fixture)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// F1 with the active test: a read with `active_test` off first (the user sees the archived
/// sub-line, not the hidden one), then a default read (the archived one is gone); the superuser
/// with the test off sees all three.
#[tokio::test]
async fn in_memory_line_active_test_is_decided_at_every_read() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let line = new_line_of(&mut tx, &fixture).await;
    let user = line.with_user(2);
    let all_for_user = (
        vec![fixture.open, fixture.archived],
        vec![s("open"), s("archived")],
        vec![int(1), int(50)],
    );
    assert_eq!(
        sub_values(&mut tx, &user.with_context(inactive())).await,
        all_for_user
    );
    assert_eq!(sub_values(&mut tx, &user).await, only_open(&fixture));
    assert_eq!(
        sub_values(&mut tx, &line.sudo(true).with_context(inactive())).await,
        (
            vec![fixture.open, fixture.hidden, fixture.archived],
            vec![s("open"), s("hidden"), s("archived")],
            vec![int(1), int(100), int(50)],
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn in_memory_record_of_an_unreadable_origin_is_an_access_error() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_users(&mut tx).await;
    let fixture = hidden_subs(&mut tx).await;
    let model = tx.env().records(MULTI_SUB).unwrap();
    let origin = model.browse([fixture.hidden]).unwrap().rec_ids()[0];
    let new = tx
        .new_record(&model, RawVals::new(), Some(origin), None)
        .await
        .unwrap();
    let uid = 2;
    let err = new
        .with_user(uid)
        .related(&mut tx, "child_ids")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!(
            "Your access rights do not let you access 'Multi sub-line' (test_web.multi_sub) records.\n\n\
             Records: test_web.multi_sub({},), User: {uid}",
            fixture.hidden
        )
    );
    // the superuser reads it
    let subs = new.sudo(true).related(&mut tx, "child_ids").await.unwrap();
    assert!(subs.is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn in_memory_record_of_a_deleted_origin_is_a_missing_error() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let model = tx.env().records(MULTI_SUB).unwrap();
    let doomed = model.browse([fixture.open]).unwrap();
    let new = tx
        .new_record(&model, RawVals::new(), Some(doomed.rec_ids()[0]), None)
        .await
        .unwrap();
    doomed.unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();
    let uid = 2;
    let err = new
        .with_user(uid)
        .related(&mut tx, "child_ids")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        format!(
            "Record does not exist or has been deleted.\n(Record: test_web.multi_sub({},), \
             User: {uid})",
            fixture.open
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- access re-review: the origin check, the rule-decision memo (A1-A9) ----------

/// Three children of the "open" sub-line: "kid" (readable), "hidden" (hidden from user 2 by
/// [`HideHiddenSubs`]) and "arch" (archived). They belong to no line.
async fn open_children(tx: &mut Transaction, fixture: &HiddenSubs) -> (i64, i64, i64) {
    let parent = int(fixture.open);
    let kids = create(
        tx,
        MULTI_SUB,
        &[
            &[("parent_id", parent.clone()), ("name", s("kid"))],
            &[("parent_id", parent.clone()), ("name", s("hidden"))],
            &[
                ("parent_id", parent),
                ("name", s("arch")),
                ("active", Value::Bool(false)),
            ],
        ],
    )
    .await;
    (id(&kids, 0), id(&kids, 1), id(&kids, 2))
}

/// A1: the origin of a requested record is checked, the origins of the records that are only in
/// its prefetch set are not. User 2 lists the sub-lines of a line (the prefetch set then holds
/// the in-memory records of the hidden and the archived sub-line too), then reads the children
/// and the labels of the visible one: no error, and nothing of the hidden one in the values.
#[tokio::test]
async fn prefetch_extras_with_unreadable_origins_never_fail_a_read() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let (kid, _hidden_kid, _arch) = open_children(&mut tx, &fixture).await;
    let line = new_line_of(&mut tx, &fixture).await;
    let user = line.with_user(2);
    let subs = user.related(&mut tx, "sub_ids").await.unwrap();
    assert_eq!(subs.ids(), vec![fixture.open]);
    let children = subs.related(&mut tx, "child_ids").await.unwrap();
    assert_eq!(children.ids(), vec![kid]);
    assert_eq!(
        children.values(&mut tx, "name").await.unwrap(),
        vec![s("kid")]
    );
    let labels = subs.related(&mut tx, "labels").await.unwrap();
    assert!(labels.is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A1 through an onchange: a line changed without `sub_ids` commands, the spec asking for
/// `lines.sub_ids.labels`. As user 2 the values hold the open sub-line only and nothing fails.
#[tokio::test]
async fn onchange_spec_through_a_line_changed_without_sub_ids_does_not_fail_for_a_user() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let line = id(&fixture.line, 0);
    let multis = tx.env().with_user(2).records(MULTI).unwrap();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[4, line, 0], [1, line, {"name": "changed"}]]})),
            vec![],
            spec(json!({"lines": {"fields": {
                "name": {},
                "sub_ids": {"fields": {"name": {}, "labels": {"fields": {"name": {}}}}},
            }}})),
        )
        .await
        .unwrap();
    let text = format!("{result:?}");
    assert!(text.contains("open"), "{text}");
    assert!(!text.contains("hidden"), "{text}");
    assert!(!text.contains("archived"), "{text}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A9: the prefetch extension of an in-memory record's x2many read is capped at one prefetch
/// chunk. With the default chunk the records that are only in the prefetch set and whose origin the
/// reader may read are filled with their origins' raw values (and decided when read); with a chunk
/// of 1 only the requested record is.
#[tokio::test]
async fn new_record_x2many_prefetch_is_capped_at_one_chunk() {
    for (chunk, extras_cached) in [(None, true), (Some(1), false)] {
        let mut builder =
            Harness::builder([&base_users::ADDON, &test_web::ADDON]).rules(HideHiddenSubs);
        if let Some(chunk) = chunk {
            builder = builder.config(erp_orm::OrmConfig {
                prefetch_chunk: chunk,
                ..erp_orm::OrmConfig::default()
            });
        }
        let h = builder.build().await.unwrap();
        let mut tx = h.begin().await.unwrap();
        let fixture = hidden_subs(&mut tx).await;
        let line = new_line_of(&mut tx, &fixture).await;
        // all three in-memory sub-lines, read by the superuser without the active test
        let all = line
            .sudo(true)
            .with_context(inactive())
            .related(&mut tx, "sub_ids")
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        let subs = line.with_user(2).related(&mut tx, "sub_ids").await.unwrap();
        assert_eq!(subs.ids(), vec![fixture.open]);
        let _ = subs.related(&mut tx, "child_ids").await.unwrap();
        let cached: Vec<bool> = all
            .rec_ids()
            .iter()
            .map(|i| tx.is_cached(&all.browse_ids([*i]), "child_ids").unwrap())
            .collect();
        // the requested record is always cached; the others only when the chunk has room, and
        // never the one whose origin the reader may not read (B5): 2 of 3 with room
        assert_eq!(
            cached.iter().filter(|c| **c).count(),
            if extras_cached { 2 } else { 1 },
            "chunk {chunk:?}: {cached:?}"
        );
        drop(tx);
        h.cleanup().await.unwrap();
    }
}

/// A reader the rules apply to gets
/// `Missing` from the access check of the origin (`__check_access_fill_cache`): the F5 test
/// `in_memory_record_of_a_deleted_origin_is_a_missing_error`.
#[tokio::test]
async fn superuser_in_memory_record_of_a_deleted_origin_has_an_empty_value() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let model = tx.env().records(MULTI_SUB).unwrap();
    let doomed = model.browse([fixture.open]).unwrap();
    let new = tx
        .new_record(&model, RawVals::new(), Some(doomed.rec_ids()[0]), None)
        .await
        .unwrap();
    doomed.unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();

    // the superuser: an empty value, one statement (the read of the origin's children)
    tx.capture_statements(true);
    let children = new.related(&mut tx, "child_ids").await.unwrap();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    assert!(children.is_empty());
    assert_eq!(
        tables_of(&statements),
        vec!["FROM test_web_multi_sub".to_owned()],
        "{statements:#?}"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

/// What `x2many_commands` of `line_id`'s `sub_ids` returns for the reader `env`, after the open
/// sub-line's `child_ids` were read (cached on its in-memory record).
async fn sub_commands(
    tx: &mut Transaction,
    fixture: &HiddenSubs,
    line: &Records,
) -> Vec<erp_sdk::Command> {
    let subs = line.related(tx, "sub_ids").await.unwrap();
    // the open sub-line's in-memory record caches its raw children
    for sub in subs.rec_ids() {
        let one = subs.browse_ids([*sub]);
        if one.ids() == vec![fixture.open] {
            let _ = one.related(tx, "child_ids").await.unwrap();
        }
    }
    line.x2many_commands(tx, "sub_ids").await.unwrap()
}

/// A3: `convert_to_write` compares and lists x2many values as the reader sees them. The open
/// sub-line has a readable, a hidden and an archived child. As the superuser (who sees the
/// hidden child, not the archived one) and as user 2 (who sees neither), the commands for the
/// line's sub-lines are the `SET` of the visible sub-lines and nothing else: no `UPDATE` of the
/// open sub-line listing children the reader does not see (or archived ones).
#[tokio::test]
async fn x2many_commands_list_what_the_reader_sees() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let (_kid, hidden_kid, arch) = open_children(&mut tx, &fixture).await;
    let sub_model = tx.env().records(MULTI_SUB).unwrap();
    let rec = |i: i64| sub_model.browse([i]).unwrap().rec_ids()[0];

    // the superuser: the open and the hidden sub-line, no UPDATE with the archived child
    let line = new_line_of(&mut tx, &fixture).await;
    let commands = sub_commands(&mut tx, &fixture, &line).await;
    assert_eq!(
        commands,
        vec![erp_sdk::Command::set(vec![
            rec(fixture.open),
            rec(fixture.hidden)
        ])],
        "superuser: {commands:?} (archived child {arch})"
    );

    // user 2: the open sub-line only, no UPDATE carrying the hidden child
    tx.invalidate_all().await.unwrap();
    let line = new_line_of(&mut tx, &fixture).await.with_user(2);
    let commands = sub_commands(&mut tx, &fixture, &line).await;
    assert_eq!(
        commands,
        vec![erp_sdk::Command::set(vec![rec(fixture.open)])],
        "user 2: {commands:?} (hidden child {hidden_kid})"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ---- the memo of rule decisions (A2, A4, A5, A6) ---------------------------------------------

/// Sub-lines named "hidden" are readable only for what the principal state allows: user 3, a
/// principal with company 2 (current or allowed), or a debug principal. A rule never sees the
/// context.
struct HideByState;

impl erp_testkit::TestRules for HideByState {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        let principal = b.principal();
        Ok(match (model.name, op) {
            (MULTI_SUB, Operation::Read) => {
                // a rule sees the principal only: never the context
                let sees = principal.subject().get() == 3
                    || principal
                        .companies()
                        .current()
                        .is_some_and(|c| c.get() == 2)
                    || principal.companies().allowed().iter().any(|c| c.get() == 2)
                    || principal.debug();
                if sees {
                    Domain::TRUE
                } else {
                    Domain::condition("name", erp_sdk::Operator::Ne, "hidden").unwrap()
                }
            }
            _ => Domain::TRUE,
        })
    }
}

async fn state_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(HideByState)
        .build()
        .await
        .unwrap()
}

/// The sub-lines of a fresh in-memory record of the saved line that `reader` (a function of the
/// record) lists, as origin ids. A fresh record each time: only the rule-decision memo can carry
/// a decision from one read to the next.
async fn listed(
    tx: &mut Transaction,
    fixture: &HiddenSubs,
    reader: impl Fn(&Records) -> Records,
) -> Vec<i64> {
    let line = new_line_of(tx, fixture).await;
    reader(&line).related(tx, "sub_ids").await.unwrap().ids()
}

fn user2(line: &Records) -> Records {
    line.with_user(2)
}

/// A2 / (g): the decisions are kept per principal and model by value (and per context,
/// which the rules of an access policy cannot see). Reading in turn as user 2 and as user 3, and as
/// two principals of the same user that differ only in their allowed companies (or their debug
/// flag) never lets one reader's decision answer for another; a context key does not change what
/// the rules decide, while the context's active companies do (they are the principal's companies,
/// ).
#[tokio::test]
async fn rule_decisions_are_kept_per_principal_and_context() {
    use erp_orm::{CompanyId, GroupSet, PrincipalBuilder, Resolution, UserId};
    let h = state_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let only_open = vec![fixture.open];
    let both = vec![fixture.open, fixture.hidden];

    // another user
    for _ in 0..2 {
        assert_eq!(listed(&mut tx, &fixture, user2).await, only_open);
        assert_eq!(listed(&mut tx, &fixture, |l| l.with_user(3)).await, both);
    }
    // a context key: rules decide by the principal only, so the key changes nothing
    // and the decision remembered for the principal answers both
    for _ in 0..2 {
        assert_eq!(
            listed(&mut tx, &fixture, |l| user2(l)
                .with_context([("see_hidden", Value::Bool(true))]))
            .await,
            only_open
        );
        assert_eq!(
            listed(&mut tx, &fixture, |l| user2(l)
                .with_context([("see_hidden", Value::Bool(false))]))
            .await,
            only_open
        );
    }
    // `company_ids` follows `with_company`: the environment's validated
    // companies [2, 1] are the ones the rule sees, so this reader sees the record
    for _ in 0..2 {
        assert_eq!(
            listed(&mut tx, &fixture, |l| {
                let env = user2(l).env().with_company(2).unwrap();
                l.with_env(env)
            })
            .await,
            both
        );
        assert_eq!(listed(&mut tx, &fixture, user2).await, only_open);
    }
    // two principals of one user, one context, differing in the allowed companies / debug
    let principal = |companies: &[i64], debug: bool| {
        let all = [1, 2].map(|c| CompanyId::new(c).unwrap());
        PrincipalBuilder::user(UserId::new(2).unwrap())
            .resolution(
                Resolution::in_id_order(GroupSet::default(), all, std::num::NonZeroU64::MIN)
                    .unwrap(),
            )
            .companies(
                companies
                    .iter()
                    .map(|c| CompanyId::new(*c).unwrap())
                    .collect(),
            )
            .debug(debug)
            .build()
            .unwrap()
    };
    let narrow = tx.env().with_principal_for_tests(principal(&[1], false));
    let wide = tx.env().with_principal_for_tests(principal(&[1, 2], false));
    let debug = tx.env().with_principal_for_tests(principal(&[1], true));
    for _ in 0..2 {
        for (env, expected) in [
            (&narrow, &only_open),
            (&wide, &both),
            (&narrow, &only_open),
            (&debug, &both),
        ] {
            let env = env.clone();
            assert_eq!(
                listed(&mut tx, &fixture, |l| l.with_env(env.clone())).await,
                *expected
            );
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A4/A6: a write to a database record that a rule reads, still pending in the cache, ends the
/// remembered decisions: the open sub-line, readable a moment ago, is renamed "hidden" and the
/// next read as user 2 does not list it.
#[tokio::test]
async fn pending_write_of_a_rule_input_ends_remembered_decisions() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    assert_eq!(listed(&mut tx, &fixture, user2).await, vec![fixture.open]);
    let subs = tx.env().records(MULTI_SUB).unwrap();
    subs.browse([fixture.open])
        .unwrap()
        .write_values(&mut tx, &map(&[("name", s("hidden"))]))
        .await
        .unwrap();
    assert!(listed(&mut tx, &fixture, user2).await.is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn in_memory_writes_keep_remembered_decisions() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    assert_eq!(listed(&mut tx, &fixture, user2).await, vec![fixture.open]);
    let model = tx.env().records(MULTI_SUB).unwrap();
    let new = tx
        .new_record(&model, RawVals::new(), None, None)
        .await
        .unwrap();
    new.write_values(
        &mut tx,
        &map(&[("name", s("in memory")), ("amount", int(3))]),
    )
    .await
    .unwrap();
    tx.capture_statements(true);
    assert_eq!(listed(&mut tx, &fixture, user2).await, vec![fixture.open]);
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    assert!(
        statements.iter().all(|q| !q.contains("multi_sub")),
        "{statements:#?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A6: a savepoint rolled back ends the decisions made inside it.
#[tokio::test]
async fn savepoint_rollback_ends_remembered_decisions() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let outcome = tx
        .savepoint(async |tx| {
            let subs = tx.env().records(MULTI_SUB).unwrap();
            subs.browse([fixture.open])
                .unwrap()
                .write_values(tx, &map(&[("name", s("hidden"))]))
                .await?;
            tx.flush().await?;
            assert!(listed(tx, &fixture, user2).await.is_empty());
            Err::<(), _>(Error::programming("roll back"))
        })
        .await;
    assert_eq!(outcome.unwrap_err().message(), "roll back");
    assert_eq!(listed(&mut tx, &fixture, user2).await, vec![fixture.open]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A function the tests use to change a rule input where the ORM cannot see it: a `SELECT` that
/// writes.
const HIDE_FN: &str = "CREATE FUNCTION erp_hide_sub(bigint) RETURNS void LANGUAGE sql AS \
                       $f$ UPDATE test_web_multi_sub SET name = 'hidden' WHERE id = $1 $f$";

/// A4/A6: raw statements. A statement the ORM classifies as a data change ends the remembered
/// decisions, whatever it starts with (`UPDATE`, a comment before it, a `DO` block, a `CALL`)
/// and so does every `batch`, even one that is a plain `SELECT` of a writing function. A writing
/// `SELECT` sent by `query` cannot be told from a read; the caller invalidates (next test).
#[tokio::test]
async fn raw_data_statements_end_remembered_decisions() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    tx.batch(HIDE_FN).await.unwrap();
    let open = fixture.open;
    let reset = format!("UPDATE test_web_multi_sub SET name = 'open' WHERE id = {open}");
    let statements = [
        format!("UPDATE test_web_multi_sub SET name = 'hidden' WHERE id = {open}"),
        format!("/* hide */ UPDATE test_web_multi_sub SET name = 'hidden' WHERE id = {open}"),
        format!("-- hide\nUPDATE test_web_multi_sub SET name = 'hidden' WHERE id = {open}"),
        format!(
            "DO $$ BEGIN UPDATE test_web_multi_sub SET name = 'hidden' WHERE id = {open}; END $$"
        ),
        format!(
            "WITH hide AS (UPDATE test_web_multi_sub SET name = 'hidden' WHERE id = {open} \
             RETURNING id) SELECT id FROM hide"
        ),
    ];
    for sql in &statements {
        tx.execute(&reset, &[]).await.unwrap();
        assert_eq!(
            listed(&mut tx, &fixture, user2).await,
            vec![open],
            "before: {sql}"
        );
        if sql.starts_with("WITH") {
            tx.query(sql, &[]).await.unwrap();
        } else {
            tx.execute(sql, &[]).await.unwrap();
        }
        assert!(
            listed(&mut tx, &fixture, user2).await.is_empty(),
            "after: {sql}"
        );
    }
    // a batch
    tx.execute(&reset, &[]).await.unwrap();
    assert_eq!(listed(&mut tx, &fixture, user2).await, vec![open]);
    tx.batch(&format!("SELECT erp_hide_sub({open})"))
        .await
        .unwrap();
    assert!(listed(&mut tx, &fixture, user2).await.is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalidation_ends_remembered_decisions() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    tx.batch(HIDE_FN).await.unwrap();
    let open = fixture.open;
    let reset = format!("UPDATE test_web_multi_sub SET name = 'open' WHERE id = {open}");
    for variant in ["all", "model", "records"] {
        tx.execute(&reset, &[]).await.unwrap();
        assert_eq!(
            listed(&mut tx, &fixture, user2).await,
            vec![open],
            "{variant}"
        );
        tx.query(&format!("SELECT erp_hide_sub({open})"), &[])
            .await
            .unwrap();
        match variant {
            "all" => tx.invalidate_all().await.unwrap(),
            "model" => tx.invalidate_model(MULTI_SUB, None).await.unwrap(),
            _ => {
                let subs = tx.env().records(MULTI_SUB).unwrap();
                let records = subs.browse([open]).unwrap();
                tx.invalidate_records(&records, None).await.unwrap();
            }
        }
        assert!(
            listed(&mut tx, &fixture, user2).await.is_empty(),
            "{variant}: after the invalidation"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- access re-check (B1, B2, B4, B5, B8) ---------------------------------------

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// The error a read of a hidden record gives.
fn hidden_message(description: &str, model: &str, id: i64, uid: i64) -> String {
    format!(
        "Your access rights do not let you access '{description}' ({model}) records.\n\nRecords: \
         {model}({id},), User: {uid}"
    )
}

/// Lines whose stored related `order_partner` is "secret" are hidden from reads outside
/// superuser mode.
struct HideSecretLines;

impl erp_testkit::TestRules for HideSecretLines {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        Ok(match (model.name, op) {
            ("test_dep.line", Operation::Read) => {
                Domain::condition("order_partner", erp_sdk::Operator::Ne, "secret").unwrap()
            }
            _ => Domain::TRUE,
        })
    }
}

/// B1: a stored related field a rule reads is only marked for recompute when its dependency
/// changes (the partner is renamed, a pending write that is not a rule input). The decision made
/// before ("readable") must not answer after it, whichever way the line is read: `fetch`, `read`,
/// `web_read` or a listing of the order's lines.
#[tokio::test]
async fn pending_recompute_of_a_stored_rule_input_ends_remembered_decisions() {
    let h = Harness::builder([&base_users::ADDON, &erp_test_models::test_depends::ADDON])
        .rules(HideSecretLines)
        .build()
        .await
        .unwrap();
    for variant in ["fetch", "read", "web_read", "listing"] {
        let mut tx = h.begin().await.unwrap();
        seed_users(&mut tx).await;
        let partner = create(&mut tx, "test_dep.partner", &[&[("name", s("ok"))]]).await;
        let order = create(
            &mut tx,
            "test_dep.order",
            &[&[("name", s("o")), ("partner_id", int(id(&partner, 0)))]],
        )
        .await;
        let line = create(
            &mut tx,
            "test_dep.line",
            &[&[
                ("order_id", int(id(&order, 0))),
                ("product", s("p")),
                ("qty", Value::Float(1.0)),
                ("price_unit", Value::Float(2.0)),
            ]],
        )
        .await;
        let line_id = id(&line, 0);
        let user_line = line.with_user(2);
        let user_order = order.with_user(2);
        async fn touch(
            variant: &str,
            tx: &mut Transaction,
            line: &Records,
            order: &Records,
        ) -> Result<Vec<i64>> {
            match variant {
                "fetch" => line.fetch(tx, None).await.map(|()| line.ids()),
                "read" => line.read(tx, &["product"]).await.map(|_| line.ids()),
                "web_read" => line
                    .web_read(tx, spec(json!({"product": {}})))
                    .await
                    .map(|_| line.ids()),
                _ => order.related(tx, "line_ids").await.map(|l| l.ids()),
            }
        }
        let before = touch(variant, &mut tx, &user_line, &user_order)
            .await
            .unwrap();
        assert_eq!(before, vec![line_id], "{variant}: readable before");
        // the partner is renamed: the line's stored related field is pending, not recomputed
        partner
            .write_values(&mut tx, &map(&[("name", s("secret"))]))
            .await
            .unwrap();
        let after = touch(variant, &mut tx, &user_line, &user_order).await;
        if variant == "listing" {
            assert!(after.unwrap().is_empty(), "{variant}: hidden after");
        } else {
            let err = after.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Access, "{variant}");
            assert_eq!(
                err.message(),
                hidden_message("Dep Line", "test_dep.line", line_id, 2),
                "{variant}"
            );
        }
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// Sub-lines are readable when they are in the tree below the sub-line `root` (`child_of`).
struct SubsBelow {
    root: Arc<AtomicI64>,
}

impl erp_testkit::TestRules for SubsBelow {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        Ok(match (model.name, op) {
            (MULTI_SUB, Operation::Read) => Domain::condition(
                "id",
                erp_sdk::Operator::ChildOf,
                self.root.load(Ordering::SeqCst),
            )
            .unwrap(),
            _ => Domain::TRUE,
        })
    }
}

/// B2: a rule with `child_of` reads the parent many2one. A pending write of the parent (the
/// record leaves the tree) ends the remembered decisions. Through `fetch` the rule's resolution
/// flushes the parent anyway; a listing of the line's sub-lines, whose decisions all come from the
/// memo, is the path that only the recorded input protects. The write comes before any decision
/// or after one.
#[tokio::test]
async fn pending_parent_write_changes_a_child_of_rule_decision() {
    let root = Arc::new(AtomicI64::new(0));
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(SubsBelow {
            root: Arc::clone(&root),
        })
        .build()
        .await
        .unwrap();
    for (variant, decided_first) in [
        ("fetch", false),
        ("fetch", true),
        ("listing", false),
        ("listing", true),
    ] {
        let mut tx = h.begin().await.unwrap();
        seed_users(&mut tx).await;
        let multi = create(&mut tx, MULTI, &[&[]]).await;
        let line = create(&mut tx, MULTI_LINE, &[&[("multi", int(id(&multi, 0)))]]).await;
        let top = create(
            &mut tx,
            MULTI_SUB,
            &[&[("name", s("root")), ("line_id", int(id(&line, 0)))]],
        )
        .await;
        root.store(id(&top, 0), Ordering::SeqCst);
        let kid = create(
            &mut tx,
            MULTI_SUB,
            &[&[
                ("name", s("kid")),
                ("parent_id", int(id(&top, 0))),
                ("line_id", int(id(&line, 0))),
            ]],
        )
        .await;
        let kid_id = id(&kid, 0);
        let user_kid = kid.with_user(2);
        let user_line = line.with_user(2);
        // every value cached by the superuser: the user's read is decided by the rules alone
        kid.fetch(&mut tx, None).await.unwrap();
        line.fetch(&mut tx, None).await.unwrap();
        if decided_first {
            user_kid.fetch(&mut tx, None).await.unwrap();
            let both = user_line.related(&mut tx, "sub_ids").await.unwrap().ids();
            assert_eq!(both, vec![id(&top, 0), kid_id], "{variant}: in the tree");
        }
        // the kid leaves the tree: a pending write as the superuser
        tx.capture_statements(true);
        kid.write_values(&mut tx, &map(&[("parent_id", F)]))
            .await
            .unwrap();
        let sent = tx.captured().to_vec();
        tx.capture_statements(false);
        assert!(sent.is_empty(), "the write stays pending: {sent:#?}");
        let label = format!("{variant}, decided first: {decided_first}");
        if variant == "fetch" {
            let err = user_kid.fetch(&mut tx, None).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Access, "{label}");
            assert_eq!(
                err.message(),
                hidden_message("Multi sub-line", MULTI_SUB, kid_id, 2),
                "{label}"
            );
        } else {
            let left = user_line.related(&mut tx, "sub_ids").await.unwrap().ids();
            assert_eq!(left, vec![id(&top, 0)], "{label}");
        }
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// Tagged records whose stored related `line_partner` (itself related to the stored
/// `line.order_partner`) is "secret" are hidden from reads outside superuser mode.
struct HideSecretChainTag;

impl erp_testkit::TestRules for HideSecretChainTag {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        Ok(match (model.name, op) {
            ("test_dep.chain_tag", Operation::Read) => {
                Domain::condition("line_partner", erp_sdk::Operator::Ne, "secret").unwrap()
            }
            _ => Domain::TRUE,
        })
    }
}

/// C1: a rule reads a stored field two stored levels below the written one (partner name, then the
/// line's stored related `order_partner`, then the tagged record's stored related `line_partner`).
/// The partner's rename marks the first level for recompute only; the second is marked when the
/// first recomputes, which is after the next decision. A decision remembered before the rename must
/// not answer after it, whichever way the record is read.
#[tokio::test]
async fn pending_change_two_stored_levels_up_ends_remembered_decisions() {
    let h = Harness::builder([&base_users::ADDON, &erp_test_models::test_depends::ADDON])
        .rules(HideSecretChainTag)
        .build()
        .await
        .unwrap();
    for variant in ["fetch", "read", "web_read", "listing"] {
        let mut tx = h.begin().await.unwrap();
        seed_users(&mut tx).await;
        let partner = create(&mut tx, "test_dep.partner", &[&[("name", s("ok"))]]).await;
        let order = create(
            &mut tx,
            "test_dep.order",
            &[&[("name", s("o")), ("partner_id", int(id(&partner, 0)))]],
        )
        .await;
        let line = create(
            &mut tx,
            "test_dep.line",
            &[&[("order_id", int(id(&order, 0))), ("product", s("p"))]],
        )
        .await;
        let tagged = create(
            &mut tx,
            "test_dep.chain_tag",
            &[&[("line_id", int(id(&line, 0)))]],
        )
        .await;
        let tagged_id = id(&tagged, 0);
        assert_eq!(
            tagged.values(&mut tx, "line_partner").await.unwrap(),
            vec![s("ok")]
        );
        let user_tagged = tagged.with_user(2);
        let user_line = line.with_user(2);
        async fn touch(
            variant: &str,
            tx: &mut Transaction,
            tagged: &Records,
            line: &Records,
        ) -> Result<Vec<i64>> {
            match variant {
                "fetch" => tagged.fetch(tx, None).await.map(|()| tagged.ids()),
                "read" => tagged.read(tx, &["line_id"]).await.map(|_| tagged.ids()),
                "web_read" => tagged
                    .web_read(tx, spec(json!({"line_id": {}})))
                    .await
                    .map(|_| tagged.ids()),
                _ => line.related(tx, "chain_tag_ids").await.map(|t| t.ids()),
            }
        }
        let before = touch(variant, &mut tx, &user_tagged, &user_line)
            .await
            .unwrap();
        assert_eq!(before, vec![tagged_id], "{variant}: readable before");
        tx.capture_statements(true);
        partner
            .write_values(&mut tx, &map(&[("name", s("secret"))]))
            .await
            .unwrap();
        let sent = tx.captured().to_vec();
        tx.capture_statements(false);
        let writes: Vec<&String> = sent
            .iter()
            .filter(|q| !q.trim_start().to_uppercase().starts_with("SELECT"))
            .collect();
        assert!(writes.is_empty(), "the write stays pending: {sent:#?}");
        let after = touch(variant, &mut tx, &user_tagged, &user_line).await;
        if variant == "listing" {
            assert!(after.unwrap().is_empty(), "{variant}: hidden after");
        } else {
            let err = after.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Access, "{variant}");
            assert_eq!(
                err.message(),
                hidden_message("Dep Chain Tag", "test_dep.chain_tag", tagged_id, 2),
                "{variant}"
            );
        }
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// Categorized records are readable when their category is in the tree below the categories whose
/// name matches "root" (`category_id child_of "root"`): the start set is found
/// by a name search, so the compiled rule does not list the data it depends on.
struct CategoryByName;

impl erp_testkit::TestRules for CategoryByName {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        Ok(match (model.name, op) {
            ("test_dep.categorized", Operation::Read) => {
                Domain::condition("category_id", erp_sdk::Operator::ChildOf, "root").unwrap()
            }
            _ => Domain::TRUE,
        })
    }
}

/// C2: the start records of `child_of` given by name are searched when the rule is resolved, and
/// the rename that moves a category in or out of the start set is a pending write the compiled
/// rule does not list.
#[tokio::test]
async fn child_of_by_name_rule_is_asked_again_after_a_pending_rename() {
    let h = Harness::builder([&base_users::ADDON, &erp_test_models::test_depends::ADDON])
        .rules(CategoryByName)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed_users(&mut tx).await;
    let inside = create(&mut tx, "test_dep.category", &[&[("name", s("root-in"))]]).await;
    let outside = create(&mut tx, "test_dep.category", &[&[("name", s("out"))]]).await;
    let item_in = create(
        &mut tx,
        "test_dep.categorized",
        &[&[("name", s("a")), ("category_id", int(id(&inside, 0)))]],
    )
    .await;
    let item_out = create(
        &mut tx,
        "test_dep.categorized",
        &[&[("name", s("b")), ("category_id", int(id(&outside, 0)))]],
    )
    .await;
    let (in_id, out_id) = (id(&item_in, 0), id(&item_out, 0));
    // the listing of a category's items is decided from the remembered decisions
    let user_inside = inside.with_user(2);
    let user_outside = outside.with_user(2);
    assert_eq!(
        user_inside
            .related(&mut tx, "item_ids")
            .await
            .unwrap()
            .ids(),
        vec![in_id]
    );
    assert!(
        user_outside
            .related(&mut tx, "item_ids")
            .await
            .unwrap()
            .ids()
            .is_empty()
    );
    // the categories swap places in the start set, as pending writes
    tx.capture_statements(true);
    inside
        .write_values(&mut tx, &map(&[("name", s("left"))]))
        .await
        .unwrap();
    outside
        .write_values(&mut tx, &map(&[("name", s("root-out"))]))
        .await
        .unwrap();
    let sent = tx.captured().to_vec();
    tx.capture_statements(false);
    let writes: Vec<&String> = sent
        .iter()
        .filter(|q| !q.trim_start().to_uppercase().starts_with("SELECT"))
        .collect();
    assert!(writes.is_empty(), "the renames stay pending: {sent:#?}");
    assert_eq!(
        user_outside
            .related(&mut tx, "item_ids")
            .await
            .unwrap()
            .ids(),
        vec![out_id],
        "moved into the start set"
    );
    assert!(
        user_inside
            .related(&mut tx, "item_ids")
            .await
            .unwrap()
            .ids()
            .is_empty(),
        "moved out of the start set"
    );
    // read directly, the rule's resolution flushes the rename first: the same answer
    let err = item_in.with_user(2).fetch(&mut tx, None).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        hidden_message("Dep Categorized", "test_dep.categorized", in_id, 2)
    );
    item_out.with_user(2).fetch(&mut tx, None).await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Reading sub-lines is refused to user 2 at the model level.
struct DenySubRead;

impl erp_testkit::TestRules for DenySubRead {
    fn check_model(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if model.name == MULTI_SUB && op == Operation::Read && b.principal().subject().get() == 2 {
            return Err(Error::access("model access denied for user 2"));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<Domain> {
        Ok(Domain::TRUE)
    }
}

/// C4: only prefetch extras have origins (the requested record has none), and the reader's model
/// access right is refused: no extra is filled for that reader, and nothing fails because of them.
#[tokio::test]
async fn prefetch_extras_are_not_filled_for_a_reader_without_model_access() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(DenySubRead)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let subs = tx.env().records(MULTI_SUB).unwrap();
    let open = subs.browse([fixture.open]).unwrap();
    let fresh = tx
        .new_record(&subs, RawVals::new(), None, None)
        .await
        .unwrap();
    let extra = tx
        .new_record(&subs, RawVals::new(), Some(open.rec_ids()[0]), None)
        .await
        .unwrap();
    let both = fresh.concat(&extra).unwrap();
    let requested = fresh
        .with_prefetch(Some(Arc::from(both.rec_ids())))
        .with_user(2);
    requested
        .related(&mut tx, "child_ids")
        .await
        .expect("the requested record has no origin: nothing to refuse");
    let extra_user = both.browse_ids([extra.rec_ids()[0]]).with_user(2);
    assert!(
        !tx.is_cached(&extra_user, "child_ids").unwrap(),
        "the extra's value was filled for a reader who may not read the model"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Sub-lines named "hidden" are hidden, and once `strict` is set a sub-line also needs a
/// non-negative amount, a rule that reads another field.
struct GrowingRule {
    strict: Arc<AtomicBool>,
}

impl erp_testkit::TestRules for GrowingRule {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        Ok(match (model.name, op) {
            (MULTI_SUB, Operation::Read) => {
                let name = Domain::condition("name", erp_sdk::Operator::Ne, "hidden").unwrap();
                if self.strict.load(Ordering::SeqCst) {
                    Domain::and([
                        name,
                        Domain::condition("amount", erp_sdk::Operator::Ge, 0).unwrap(),
                    ])
                } else {
                    name
                }
            }
            _ => Domain::TRUE,
        })
    }
}

/// B4: the flush before a rule's query may send statements, which end every remembered decision,
/// the ones this very call relies on included. A record decided readable before the flush is
/// decided again after it, never reported unreadable. Here the rule changes between two calls
/// (it reads `amount` from the second one on) while a write to `amount` is pending, so the flush
/// has work and the entry, which did not read `amount`, was not dropped by the write.
#[tokio::test]
async fn flush_inside_a_decision_does_not_turn_decided_records_unreadable() {
    let strict = Arc::new(AtomicBool::new(false));
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(GrowingRule {
            strict: Arc::clone(&strict),
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let fixture = hidden_subs(&mut tx).await;
    let subs = tx.env().records(MULTI_SUB).unwrap();
    let open = subs.browse([fixture.open]).unwrap();
    // decided readable under the first rule
    open.with_user(2).fetch(&mut tx, None).await.unwrap();
    // a pending write of `amount`, which only the second rule reads, then the second rule
    subs.browse([fixture.hidden])
        .unwrap()
        .write_values(&mut tx, &map(&[("amount", int(7))]))
        .await
        .unwrap();
    strict.store(true, Ordering::SeqCst);
    let line = new_line_of(&mut tx, &fixture).await;
    let listed = line.with_user(2).related(&mut tx, "sub_ids").await.unwrap();
    assert_eq!(listed.ids(), vec![fixture.open]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// B5: a prefetch extra (an in-memory sub-line whose origin the reader may not read) is not filled
/// for that reader. The reader's later read of that very record is an `Access` error, whatever was
/// read before.
#[tokio::test]
async fn prefetch_extra_with_a_hidden_origin_is_never_filled_for_the_reader() {
    let h = hide_harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_users(&mut tx).await;
    let fixture = hidden_subs(&mut tx).await;
    let line = new_line_of(&mut tx, &fixture).await;
    let all = line
        .sudo(true)
        .with_context(inactive())
        .related(&mut tx, "sub_ids")
        .await
        .unwrap();
    let hidden_new = all
        .rec_ids()
        .iter()
        .copied()
        .find(|i| i.origin() == RecId::from_real(fixture.hidden).and_then(|r| r.as_real()))
        .expect("the in-memory record of the hidden sub-line");
    let subs = line.with_user(2).related(&mut tx, "sub_ids").await.unwrap();
    assert_eq!(subs.ids(), vec![fixture.open]);
    // the visible record is read: the hidden one is an extra of its prefetch set
    let _ = subs.related(&mut tx, "child_ids").await.unwrap();
    let hidden = all.browse_ids([hidden_new]).with_user(2);
    assert!(!tx.is_cached(&hidden, "child_ids").unwrap());
    let err = hidden.related(&mut tx, "child_ids").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        hidden_message("Multi sub-line", MULTI_SUB, fixture.hidden, 2)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// B8: the pending write of a rule input through `fetch`, `read` and `web_read` of the record
/// itself (not only through an x2many listing): readable first, hidden after the rename.
#[tokio::test]
async fn pending_write_of_a_rule_input_hides_the_record_from_fetch_read_and_web_read() {
    let h = hide_harness().await;
    for variant in ["fetch", "read", "web_read"] {
        let mut tx = h.begin().await.unwrap();
        seed_users(&mut tx).await;
        let fixture = hidden_subs(&mut tx).await;
        let subs = tx.env().records(MULTI_SUB).unwrap();
        let open = subs.browse([fixture.open]).unwrap();
        let user_open = open.with_user(2);
        async fn touch(variant: &str, tx: &mut Transaction, r: &Records) -> Result<()> {
            match variant {
                "fetch" => r.fetch(tx, None).await,
                "read" => r.read(tx, &["name"]).await.map(|_| ()),
                _ => r.web_read(tx, spec(json!({"name": {}}))).await.map(|_| ()),
            }
        }
        touch(variant, &mut tx, &user_open).await.unwrap();
        open.write_values(&mut tx, &map(&[("name", s("hidden"))]))
            .await
            .unwrap();
        let err = touch(variant, &mut tx, &user_open).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{variant}");
        assert_eq!(
            err.message(),
            hidden_message("Multi sub-line", MULTI_SUB, fixture.open, 2),
            "{variant}"
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// B8: decisions through `fetch` are kept per principal and per context, as through a listing.
#[tokio::test]
async fn fetch_decisions_are_kept_per_principal_and_context() {
    let h = state_harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_users(&mut tx).await;
    let fixture = hidden_subs(&mut tx).await;
    let subs = tx.env().records(MULTI_SUB).unwrap();
    let hidden = subs.browse([fixture.hidden]).unwrap();
    let denied = hidden_message("Multi sub-line", MULTI_SUB, fixture.hidden, 2);
    for _ in 0..2 {
        hidden.with_user(3).fetch(&mut tx, None).await.unwrap();
        let err = hidden.with_user(2).fetch(&mut tx, None).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access);
        assert_eq!(err.message(), denied);
        // a rule never sees the context: a context key does not open the record
        for see in [true, false] {
            let err = hidden
                .with_user(2)
                .with_context([("see_hidden", Value::Bool(see))])
                .fetch(&mut tx, None)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Access);
            assert_eq!(err.message(), denied);
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Sub-lines are hidden when their line is named "secret" (a rule that reads another model).
struct HideSecretLineSubs;

impl erp_testkit::TestRules for HideSecretLineSubs {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        Ok(match (model.name, op) {
            (MULTI_SUB, Operation::Read) => {
                Domain::condition("line_id.name", erp_sdk::Operator::Ne, "secret").unwrap()
            }
            _ => Domain::TRUE,
        })
    }
}

/// B8: an input of another model, read through a dotted path (`line_id.name`): renaming the line
/// (pending) hides its sub-lines from the next `fetch`; a value the superuser cached before is
/// still decided by the rule for the user; a sub-line created after a decision is decided fresh.
#[tokio::test]
async fn rule_input_on_another_model_and_superuser_cached_values() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(HideSecretLineSubs)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed_users(&mut tx).await;
    let fixture = hidden_subs(&mut tx).await;
    let subs = tx.env().records(MULTI_SUB).unwrap();
    let open = subs.browse([fixture.open]).unwrap();
    // the superuser's read caches the values
    assert_eq!(open.values(&mut tx, "name").await.unwrap(), vec![s("open")]);
    open.with_user(2).fetch(&mut tx, None).await.unwrap();
    // a sub-line created after the decision is decided on its own row
    let later = create(
        &mut tx,
        MULTI_SUB,
        &[&[("line_id", int(id(&fixture.line, 0))), ("name", s("later"))]],
    )
    .await;
    later.with_user(2).fetch(&mut tx, None).await.unwrap();
    // the line is renamed, pending
    fixture
        .line
        .write_values(&mut tx, &map(&[("name", s("secret"))]))
        .await
        .unwrap();
    for record in [&open, &later] {
        let err = record.with_user(2).fetch(&mut tx, None).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access);
        assert_eq!(
            err.message(),
            hidden_message("Multi sub-line", MULTI_SUB, record.ids()[0], 2)
        );
    }
    // still cached for the superuser
    assert_eq!(open.values(&mut tx, "name").await.unwrap(), vec![s("open")]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Functional result at the original command bound; no timing assertion.
#[tokio::test]
async fn nested_many_lines_on_one_label_semantics() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let label = create(&mut tx, MULTI_LABEL, &[&[("name", s("A"))]]).await;
    let a = id(&label, 0);
    let multis = tx.env().records(MULTI).unwrap();
    // 1 line + n sub-lines + n label commands: 99,999 commands, the budget is 100,000
    let n = 49_999;
    let subs: Vec<_> = (0..n)
        .map(|_| json!([0, 0, {"labels": [[4, a, 0]]}]))
        .collect();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": [[0, 0, {"sub_ids": subs}]]})),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("total", int(0))]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Functional result at the original command bound; no timing assertion.
#[tokio::test]
async fn many_created_lines_link_semantics() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let multis = tx.env().records(MULTI).unwrap();
    let lines: Vec<_> = (0..100_000).map(|_| json!([0, 0, {}])).collect();
    let result = multis
        .onchange(
            &mut tx,
            values(json!({"lines": lines})),
            vec![],
            spec(json!({"total": {}})),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), obj(&[("total", int(0))]));
    drop(tx);
    h.cleanup().await.unwrap();
}
