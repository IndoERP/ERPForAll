//! GRP cases of slice S5, the web methods of the `web` addon: `web_read`, `web_search_read`,
//! `web_name_search`, `web_save(_multi)`, `web_resequence`, `web_unlink`,
//! `formatted_read_group(ing_sets)`, `web_read_group`, `read_progress_bar`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use support::currency_fixture;

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_orm::{
    FormattedReadGroup, ReadGroup, SEARCH_PANEL_ERROR_MESSAGE, SearchPanelSelectMultiRange,
    SearchPanelSelectRange, Spec, SpecItem, WebReadGroup, WebSave, WebSearchRead,
};
use erp_registry::ModelSpec;
use erp_sdk::serde_json::json;
use erp_sdk::{
    Date, DateTime, Domain, Error, ErrorKind, Operation, Records, Result, Transaction, Value,
};
use erp_test_models::{base_users, test_read_group, test_web};
use erp_testkit::{Harness, assert_query_count};
use support::{dom, map};

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_web::ADDON,
        &test_read_group::ADDON,
    ])
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

/// A JSON value as a dynamic value (`null` is `False`).
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

const PERSON: &str = "test_web.person";
const COURSE: &str = "test_web.course";
const LESSON: &str = "test_web.lesson";

/// Users may not read persons (only their display names, read as superuser).
struct NoPersons;

impl erp_testkit::TestRules for NoPersons {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if model.name == PERSON {
            return Err(Error::access(format!(
                "You are not allowed to {} '{}' ({}) records.",
                op.as_str(),
                model.description,
                model.name
            )));
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

/// A course by `ged` with lessons day1 (attendees 123, 456, 789; teacher `ged`) and day2, and a
/// course without author.
struct Courses {
    ged: i64,
    course: Records,
    authorless: Records,
    day1: i64,
    day2: i64,
    attendees: Records,
}

async fn courses(tx: &mut Transaction) -> Courses {
    let ged = create(tx, PERSON, &[&[("name", s("ged"))]]).await;
    let attendees = create(
        tx,
        PERSON,
        &[
            &[("name", s("123"))],
            &[("name", s("456"))],
            &[("name", s("789"))],
        ],
    )
    .await;
    let course = create(
        tx,
        COURSE,
        &[&[("name", s("Course 1")), ("author_id", int(id(&ged, 0)))]],
    )
    .await;
    let authorless = create(tx, COURSE, &[&[("name", s("Course 2"))]]).await;
    let lessons = create(
        tx,
        LESSON,
        &[
            &[
                ("name", s("day1")),
                ("course_id", int(id(&course, 0))),
                ("teacher_id", int(id(&ged, 0))),
                (
                    "attendee_ids",
                    Value::list(attendees.ids().into_iter().map(int)),
                ),
            ],
            &[
                ("name", s("day2")),
                ("course_id", int(id(&course, 0))),
                ("teacher_id", int(id(&ged, 0))),
            ],
        ],
    )
    .await;
    Courses {
        ged: id(&ged, 0),
        course,
        authorless,
        day1: id(&lessons, 0),
        day2: id(&lessons, 1),
        attendees,
    }
}

#[tokio::test]
async fn spec_grp_023_web_read_basics() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(NoPersons)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let c = courses(&mut tx).await;
    let course_id = id(&c.course, 0);
    let read = async |tx: &mut Transaction, records: &Records, json| {
        records.web_read(tx, spec(json)).await
    };
    assert_eq!(
        read(&mut tx, &c.course, json!({"display_name": {}}))
            .await
            .unwrap(),
        vec![obj(&[
            ("id", int(course_id)),
            ("display_name", s("Course 1"))
        ])]
    );
    assert_eq!(
        read(&mut tx, &c.course, json!({"author_id": {}}))
            .await
            .unwrap(),
        vec![obj(&[("id", int(course_id)), ("author_id", int(c.ged))])]
    );
    assert_eq!(
        read(&mut tx, &c.course, json!({"author_id": {"fields": {}}}))
            .await
            .unwrap(),
        vec![obj(&[
            ("id", int(course_id)),
            ("author_id", obj(&[("id", int(c.ged))]))
        ])]
    );
    let with_date = read(
        &mut tx,
        &c.course,
        json!({"author_id": {"fields": {"write_date": {}}}}),
    )
    .await
    .unwrap();
    let first = support::deep(&with_date[0]);
    let Value::Map(row) = &first else {
        panic!("{with_date:?}")
    };
    let Some(Value::Map(author)) = row.get("author_id") else {
        panic!("{row:?}")
    };
    assert_eq!(author.get("id"), Some(&int(c.ged)));
    assert!(matches!(author.get("write_date"), Some(Value::DateTime(_))));
    let expected_name = vec![obj(&[
        ("id", int(course_id)),
        (
            "author_id",
            obj(&[("id", int(c.ged)), ("display_name", s("ged"))]),
        ),
    ])];
    let name_spec = json!({"author_id": {"fields": {"display_name": {}}}});
    assert_eq!(
        read(&mut tx, &c.course, name_spec.clone()).await.unwrap(),
        expected_name
    );
    // a user without access to persons still gets the display name (read as superuser) ...
    let as_user = c.course.with_user(2);
    assert_eq!(
        read(&mut tx, &as_user, name_spec.clone()).await.unwrap(),
        expected_name
    );
    // ... but not other fields
    let err = read(
        &mut tx,
        &as_user,
        json!({"author_id": {"fields": {"write_date": {}}}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        read(&mut tx, &c.authorless, name_spec).await.unwrap(),
        vec![obj(&[("id", int(id(&c.authorless, 0))), ("author_id", F)])]
    );
    // unknown fields are client errors
    let err = read(&mut tx, &c.course, json!({"nope": {}}))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Dictionaries keep the client's order (lead decision G3): a specification sent as a dynamic
/// value reads its fields in the order given, and `web_read` returns `id` first, then the fields
/// in that order.
#[tokio::test]
async fn web_read_keeps_the_specification_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = courses(&mut tx).await;
    let spec_value = Value::Map(Arc::new(support::vmap(&[
        ("name", obj(&[])),
        ("author_id", obj(&[])),
        ("display_name", obj(&[])),
    ])));
    let got = c
        .course
        .web_call(&mut tx, "web_read", &obj(&[("specification", spec_value)]))
        .await
        .unwrap();
    let Value::List(rows) = got else { panic!() };
    let first = support::deep(&rows[0]);
    let Value::Map(row) = &first else { panic!() };
    let keys: Vec<&str> = row.keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "name", "author_id", "display_name"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_025_web_read_x2many_options() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = courses(&mut tx).await;
    let course_id = id(&c.course, 0);
    let got = c
        .course
        .web_read(&mut tx, spec(json!({"lesson_ids": {}})))
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(course_id)),
            ("lesson_ids", Value::list([int(c.day1), int(c.day2)]))
        ])]
    );
    let got = c
        .course
        .web_read(
            &mut tx,
            spec(json!({"lesson_ids": {"fields": {"name": {}}}})),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(course_id)),
            (
                "lesson_ids",
                Value::list([
                    obj(&[("id", int(c.day1)), ("name", s("day1"))]),
                    obj(&[("id", int(c.day2)), ("name", s("day2"))]),
                ])
            )
        ])]
    );
    let day1 = tx.env().records(LESSON).unwrap().browse([c.day1]).unwrap();
    let a = |i| int(id(&c.attendees, i));
    let got = day1
        .web_read(
            &mut tx,
            spec(json!({"attendee_ids": {"limit": 2, "fields": {"display_name": {}}}})),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(c.day1)),
            (
                "attendee_ids",
                Value::list([
                    obj(&[("id", a(0)), ("display_name", s("123"))]),
                    obj(&[("id", a(1)), ("display_name", s("456"))]),
                    obj(&[("id", a(2))]),
                ])
            )
        ])]
    );
    let got = day1
        .web_read(&mut tx, spec(json!({"attendee_ids": {"limit": 2}})))
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(c.day1)),
            ("attendee_ids", Value::list([a(0), a(1), a(2)]))
        ])]
    );
    let got = c
        .course
        .web_read(&mut tx, spec(json!({"lesson_ids": {"order": "name desc"}})))
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(course_id)),
            ("lesson_ids", Value::list([int(c.day2), int(c.day1)]))
        ])]
    );
    let got = c
        .course
        .web_read(
            &mut tx,
            spec(json!({"lesson_ids": {"order": "name desc", "fields": {"name": {}}}})),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(course_id)),
            (
                "lesson_ids",
                Value::list([
                    obj(&[("id", int(c.day2)), ("name", s("day2"))]),
                    obj(&[("id", int(c.day1)), ("name", s("day1"))]),
                ])
            )
        ])]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_026_web_read_query_counts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = courses(&mut tx).await;
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, >= 1, <= 1, {
        c.course
            .web_read(
                &mut tx,
                spec(json!({"display_name": {}, "author_id": {"fields": {}}})),
            )
            .await
            .unwrap()
    });
    tx.invalidate_all().await.unwrap();
    let multi = json!({
        "display_name": {},
        "author_id": {"fields": {"display_name": {}}},
        "lesson_ids": {"fields": {"teacher_id": {"fields": {"display_name": {}}}}},
    });
    assert_query_count!(tx, >= 1, <= 5, {
        c.course.web_read(&mut tx, spec(multi)).await.unwrap()
    });
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, >= 1, <= 3, {
        c.course
            .web_read(&mut tx, spec(json!({"lesson_ids": {"fields": {"name": {}}}})))
            .await
            .unwrap()
    });
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, >= 1, <= 4, {
        c.course
            .web_read(
                &mut tx,
                spec(json!({"lesson_ids": {"order": "name desc", "fields": {"name": {}}}})),
            )
            .await
            .unwrap()
    });
    tx.invalidate_all().await.unwrap();
    let courses = tx.env().records(COURSE).unwrap();
    assert_query_count!(tx, >= 1, <= 2, {
        courses
            .web_search_read(
                &mut tx,
                WebSearchRead::new(
                    Domain::TRUE,
                    spec(json!({"author_id": {"fields": {"write_date": {}}}})),
                ),
            )
            .await
            .unwrap()
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_028_web_read_reference_fields() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = courses(&mut tx).await;
    let course_id = id(&c.course, 0);
    c.course
        .write_values(
            &mut tx,
            &map(&[
                ("reference", s(&format!("test_web.lesson,{}", c.day1))),
                ("m2o_reference_model", s(LESSON)),
                ("m2o_reference_id", int(c.day1)),
            ]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let got = c
        .course
        .web_read(
            &mut tx,
            spec(json!({"reference": {}, "m2o_reference_id": {}})),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(course_id)),
            ("reference", s(&format!("test_web.lesson,{}", c.day1))),
            ("m2o_reference_id", int(c.day1)),
        ])]
    );
    let got = c
        .course
        .web_read(
            &mut tx,
            spec(json!({
                "reference": {"fields": {"display_name": {}}},
                "m2o_reference_id": {"fields": {}},
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(course_id)),
            (
                "reference",
                obj(&[
                    ("id", obj(&[("id", int(c.day1)), ("model", s(LESSON))])),
                    ("display_name", s("day1")),
                ])
            ),
            ("m2o_reference_id", obj(&[("id", int(c.day1))])),
        ])]
    );
    // a deleted target reads as False, and clears the model field
    tx.execute("DELETE FROM test_web_lesson WHERE id = $1", &[&c.day1])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    let got = c
        .course
        .web_read(
            &mut tx,
            spec(json!({
                "reference": {"fields": {}},
                "m2o_reference_id": {"fields": {}},
                "m2o_reference_model": {},
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![obj(&[
            ("id", int(course_id)),
            ("reference", F),
            ("m2o_reference_id", F),
            ("m2o_reference_model", F),
        ])]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_029_web_search_read_length() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..25)
        .map(|i| {
            vec![
                ("name", Value::str(format!("p{i:02}"))),
                ("active", Value::Bool(i % 5 != 0)),
            ]
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, PERSON, &refs).await;
    let n = 25;
    let persons = tx
        .env()
        .records(PERSON)
        .unwrap()
        .with_context([("active_test", Value::Bool(false))]);
    let call = |offset: u64, limit: Option<u64>, count_limit: Option<u64>| {
        let mut args = WebSearchRead::new(Domain::TRUE, Spec::new());
        args.offset = offset;
        args.limit = limit;
        args.count_limit = count_limit;
        args
    };
    let length = |v: &Value| match v {
        Value::Map(m) => m.get("length").cloned(),
        _ => None,
    };
    let got = assert_query_count!(tx, >= 1, <= 1, {
        persons.web_search_read(&mut tx, call(0, None, None)).await.unwrap()
    });
    assert_eq!(length(&got), Some(int(n)));
    let got = assert_query_count!(tx, >= 2, <= 2, {
        persons.web_search_read(&mut tx, call(0, Some(2), None)).await.unwrap()
    });
    assert_eq!(length(&got), Some(int(n)));
    let got = assert_query_count!(tx, >= 2, <= 2, {
        persons.web_search_read(&mut tx, call(10, Some(2), None)).await.unwrap()
    });
    assert_eq!(length(&got), Some(int(n)));
    let got = assert_query_count!(tx, >= 1, <= 1, {
        persons
            .web_search_read(&mut tx, call(0, Some(2), Some(2)))
            .await
            .unwrap()
    });
    assert_eq!(length(&got), Some(int(2)));
    let got = assert_query_count!(tx, >= 2, <= 2, {
        persons
            .web_search_read(&mut tx, call(10, Some(2), Some(20)))
            .await
            .unwrap()
    });
    assert_eq!(length(&got), Some(int(20)));
    let got = assert_query_count!(tx, >= 1, <= 1, {
        persons
            .web_search_read(&mut tx, call(10, Some(2), Some(12)))
            .await
            .unwrap()
    });
    assert_eq!(length(&got), Some(int(12)));
    let got = persons
        .web_search_read(&mut tx, call(100, Some(2), None))
        .await
        .unwrap();
    assert_eq!(
        got,
        obj(&[("length", int(0)), ("records", Value::list([]))])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_030_web_name_search() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(
        &mut tx,
        PERSON,
        &[&[("name", s("Ada")), ("street", s("Main St"))]],
    )
    .await;
    let persons = tx.env().records(PERSON).unwrap();
    let call = |spec_json| obj(&[("name", s("")), ("specification", v(spec_json))]);
    let got = persons
        .web_call(
            &mut tx,
            "web_name_search",
            &call(json!({"display_name": {}})),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        Value::list([obj(&[
            ("id", int(id(&p, 0))),
            ("display_name", s("Ada")),
            ("__formatted_display_name", s("Ada")),
        ])])
    );
    let got = persons
        .web_call(
            &mut tx,
            "web_name_search",
            &call(json!({"display_name": {}, "street": {}})),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        Value::list([obj(&[
            ("id", int(id(&p, 0))),
            ("display_name", s("Ada")),
            ("street", s("Main St")),
            ("__formatted_display_name", s("Ada")),
        ])])
    );
    let got = persons
        .web_call(&mut tx, "web_name_search", &call(json!({"street": {}})))
        .await
        .unwrap();
    assert_eq!(
        got,
        Value::list([obj(&[("id", int(id(&p, 0))), ("street", s("Main St"))])])
    );
    // unknown keyword arguments are client errors
    let err = persons
        .web_call(
            &mut tx,
            "web_name_search",
            &obj(&[("specification", obj(&[])), ("nope", int(1))]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_031_web_save() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let persons = tx.env().records(PERSON).unwrap();
    let save = |vals: &[(&str, Value)], spec_json| WebSave {
        vals: support::vmap(vals),
        specification: spec(spec_json),
        next_id: None,
    };
    let created = persons
        .web_save(&mut tx, save(&[("name", s("ged"))], json!({})))
        .await
        .unwrap();
    let first = support::deep(&created[0]);
    let Value::Map(row) = &first else { panic!() };
    let Some(Value::Int(new_id)) = row.get("id").cloned() else {
        panic!()
    };
    assert_eq!(created, vec![obj(&[("id", int(new_id))])]);
    let created = persons
        .web_save(
            &mut tx,
            save(&[("name", s("ged"))], json!({"display_name": {}})),
        )
        .await
        .unwrap();
    let first = support::deep(&created[0]);
    let Value::Map(row) = &first else { panic!() };
    assert_eq!(row.get("display_name"), Some(&s("ged")));
    let existing = persons.browse([new_id]).unwrap();
    let written = existing
        .web_save(&mut tx, save(&[("name", s("aab"))], json!({})))
        .await
        .unwrap();
    assert_eq!(written, vec![obj(&[("id", int(new_id))])]);
    assert_eq!(existing.value(&mut tx, "name").await.unwrap(), s("aab"));
    let other = create(&mut tx, PERSON, &[&[("name", s("next"))]]).await;
    let mut args = save(&[("name", s("abc"))], json!({"name": {}}));
    args.next_id = Some(id(&other, 0));
    assert_eq!(
        existing.web_save(&mut tx, args).await.unwrap(),
        vec![obj(&[("id", int(id(&other, 0))), ("name", s("next"))])]
    );
    // pending attachments need ir.attachment (M3); an empty list is dropped from the context
    let empty = existing.with_context([("pending_attachment_ids", Value::list([]))]);
    empty
        .web_save(&mut tx, save(&[("name", s("x"))], json!({})))
        .await
        .unwrap();
    let pending = existing.with_context([("pending_attachment_ids", Value::list([int(1)]))]);
    let err = pending
        .web_save(&mut tx, save(&[("name", s("x"))], json!({})))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    // values go through the input converter: a lossy value is refused
    let err = persons
        .web_call(
            &mut tx,
            "web_save",
            &obj(&[
                ("vals", obj(&[("sequence", Value::Float(1.5))])),
                ("specification", obj(&[])),
            ]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_032_web_save_multi_and_resequence() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mixed = create(
        &mut tx,
        "test_web.mixed",
        &[
            &[("char", s("Record 1")), ("integer", int(1))],
            &[("char", s("Record 2")), ("integer", int(2))],
        ],
    )
    .await;
    let got = assert_query_count!(tx, <= 2, {
        mixed
            .web_save_multi(
                &mut tx,
                vec![
                    support::vmap(&[
                        ("char", s("Updated 1")),
                        ("integer", int(150)),
                        ("number", Value::Float(1.5)),
                    ]),
                    support::vmap(&[("integer", int(250)), ("number", Value::Float(2.5))]),
                ],
                spec(json!({"char": {}, "integer": {}})),
            )
            .await
            .unwrap()
    });
    assert_eq!(
        got,
        vec![
            obj(&[
                ("id", int(id(&mixed, 0))),
                ("char", s("Updated 1")),
                ("integer", int(150))
            ]),
            obj(&[
                ("id", int(id(&mixed, 1))),
                ("char", s("Record 2")),
                ("integer", int(250))
            ]),
        ]
    );
    let err = mixed
        .web_save_multi(&mut tx, vec![support::vmap(&[])], Spec::new())
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Each record must have a corresponding vals entry."
    );
    assert_eq!(err.kind(), ErrorKind::Validation);
    // resequence: one UPDATE for the five records at flush
    let rows: Vec<Vec<(&str, Value)>> = (0..5)
        .map(|i| vec![("name", Value::str(format!("p{i}")))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let persons = create(&mut tx, PERSON, &refs).await;
    let reversed = persons.slice(None, None, -1).unwrap();
    let got = reversed
        .web_resequence(&mut tx, spec(json!({"sequence": {}})), "sequence", 10)
        .await
        .unwrap();
    let sequences: Vec<Value> = got
        .iter()
        .map(|r| match support::deep(r) {
            Value::Map(m) => m.get("sequence").cloned().unwrap(),
            _ => F,
        })
        .collect();
    assert_eq!(sequences, vec![int(10), int(11), int(12), int(13), int(14)]);
    tx.capture_statements(true);
    tx.flush().await.unwrap();
    let updates = tx
        .captured()
        .iter()
        .filter(|q| q.starts_with("UPDATE"))
        .count();
    assert_eq!(updates, 1);
    assert_eq!(
        reversed
            .web_resequence(&mut tx, Spec::new(), "nope", 0)
            .await
            .unwrap(),
        Vec::<Value>::new()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_032_web_save_multi_keeps_values_that_render_alike() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..4)
        .map(|i| vec![("char", Value::str(format!("r{i}")))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let mixed = create(&mut tx, "test_web.mixed", &refs).await;
    // NaN and infinity both render as JSON `null`; `1` and `1.0` are one number in JSON. Each
    // pair differs as values, so every record keeps its own.
    let got = mixed
        .web_save_multi(
            &mut tx,
            vec![
                support::vmap(&[("char", Value::Float(f64::NAN))]),
                support::vmap(&[("char", Value::Float(f64::INFINITY))]),
                support::vmap(&[("char", int(1))]),
                support::vmap(&[("char", Value::Float(1.0))]),
            ],
            spec(json!({"char": {}})),
        )
        .await
        .unwrap();
    let chars: Vec<Value> = got
        .iter()
        .map(|r| match support::deep(r) {
            Value::Map(m) => m.get("char").cloned().unwrap(),
            other => panic!("not a record: {other:?}"),
        })
        .collect();
    assert_eq!(chars, vec![s("NaN"), s("inf"), s("1"), s("1.0")]);
    // equal maps in another key order are one group: both records get the value
    let got = mixed
        .web_save_multi(
            &mut tx,
            vec![
                support::vmap(&[("char", s("same")), ("integer", int(7))]),
                support::vmap(&[("integer", int(7)), ("char", s("same"))]),
                support::vmap(&[("char", s("other")), ("integer", int(7))]),
                support::vmap(&[("char", s("same")), ("integer", int(8))]),
            ],
            spec(json!({"char": {}, "integer": {}})),
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![
            obj(&[
                ("id", int(id(&mixed, 0))),
                ("char", s("same")),
                ("integer", int(7))
            ]),
            obj(&[
                ("id", int(id(&mixed, 1))),
                ("char", s("same")),
                ("integer", int(7))
            ]),
            obj(&[
                ("id", int(id(&mixed, 2))),
                ("char", s("other")),
                ("integer", int(7))
            ]),
            obj(&[
                ("id", int(id(&mixed, 3))),
                ("char", s("same")),
                ("integer", int(8))
            ]),
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// batch, whatever their rendering)
#[tokio::test]
async fn spec_grp_032_web_save_multi_groups_equal_json_documents() {
    let h = Harness::builder([
        &base_users::ADDON,
        &test_web::ADDON,
        &erp_test_models::test_orm::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .build()
    .await
    .expect("harness");
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..3).map(|_| vec![("priority", int(0))]).collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let recs = create(&mut tx, "test_orm.write_override", &refs).await;
    let doc = |zero: f64, reversed: bool| {
        let mut m = erp_sdk::serde_json::Map::new();
        let entries = [("a", json!(zero)), ("b", json!("x"))];
        let ordered: Vec<_> = if reversed {
            entries.into_iter().rev().collect()
        } else {
            entries.into_iter().collect()
        };
        for (k, v) in ordered {
            m.insert(k.to_owned(), v);
        }
        Value::Json(Arc::new(erp_sdk::serde_json::Value::Object(m)))
    };
    // equal as JSON (`0.0 == -0.0`; key order ignored), rendered differently: one write of two
    // records; the third document differs and is a write of its own
    erp_test_models::test_orm::WRITE_BATCH_SIZES
        .lock()
        .unwrap()
        .clear();
    recs.web_save_multi(
        &mut tx,
        vec![
            support::vmap(&[("meta", doc(0.0, false))]),
            support::vmap(&[("meta", doc(-0.0, true))]),
            support::vmap(&[("meta", doc(1.0, false))]),
        ],
        Spec::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        *erp_test_models::test_orm::WRITE_BATCH_SIZES.lock().unwrap(),
        vec![2, 1]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// recordset)
#[tokio::test]
async fn spec_grp_032_web_save_multi_duplicate_ids_keep_reference_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mixed = create(&mut tx, "test_web.mixed", &[&[("char", s("start"))]]).await;
    let one = id(&mixed, 0);
    let thrice = mixed.browse([one, one, one]).unwrap();
    thrice
        .web_save_multi(
            &mut tx,
            vec![
                support::vmap(&[("char", s("A"))]),
                support::vmap(&[("char", s("B"))]),
                support::vmap(&[("char", s("A"))]),
            ],
            Spec::new(),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let rows = tx
        .query("SELECT char FROM test_web_mixed WHERE id = $1", &[&one])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, Option<String>>(0).as_deref(), Some("A"));
    // B, A, B ends with B
    thrice
        .web_save_multi(
            &mut tx,
            vec![
                support::vmap(&[("char", s("B"))]),
                support::vmap(&[("char", s("A"))]),
                support::vmap(&[("char", s("B"))]),
            ],
            Spec::new(),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let rows = tx
        .query("SELECT char FROM test_web_mixed WHERE id = $1", &[&one])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, Option<String>>(0).as_deref(), Some("B"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// with the number of records)
#[tokio::test]
async fn spec_grp_032_web_resequence_cap_and_query_count() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            &format!(
                "INSERT INTO test_web_person (name, sequence) SELECT 'p' || g, 0 \
                 FROM generate_series(1, {}) g RETURNING id::int8",
                erp_orm::MAX_RESEQUENCE_RECORDS + 1
            ),
            &[],
        )
        .await
        .unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r.get::<_, i64>(0)).collect();
    let persons = tx.env().records(PERSON).unwrap();
    let all = persons.browse(ids.iter().copied()).unwrap();
    let err = all
        .web_resequence(&mut tx, Spec::new(), "sequence", 0)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_resequence accepts at most 1000 records, got 1001"
    );
    // the same refusal through the dynamic entry point
    let err = all
        .web_call(
            &mut tx,
            "web_resequence",
            &obj(&[("specification", obj(&[]))]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_resequence accepts at most 1000 records, got 1001"
    );
    // exactly at the cap is accepted
    let at_cap = persons
        .browse(ids[..erp_orm::MAX_RESEQUENCE_RECORDS].iter().copied())
        .unwrap();
    at_cap
        .web_resequence(&mut tx, Spec::new(), "sequence", 0)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    // statements of resequence + flush + read: the same for 5 and for 50 records (no N+1),
    // pinned at the measured value. Disjoint records, every value changes: a record whose
    // sequence stays the same adds its own `write_uid`/`write_date` statement.
    let mut counts = Vec::new();
    for (start, n) in [(0, 5), (5, 50)] {
        let some = persons
            .browse(ids[start..start + n].iter().copied())
            .unwrap();
        let before = tx.query_count();
        let got = some
            .web_resequence(&mut tx, spec(json!({"sequence": {}})), "sequence", 100)
            .await
            .unwrap();
        tx.flush().await.unwrap();
        counts.push(tx.query_count() - before);
        assert_eq!(got.len(), n);
    }
    assert_eq!(
        counts[0], counts[1],
        "statement count grows with the records"
    );
    assert_eq!(counts[0], RESEQUENCE_STATEMENTS, "measured statement count");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Statements of `web_resequence` with a read of `sequence` plus the flush, measured: the flush's one
/// `UPDATE … FROM unnest(…)`; the read is served from the cache the writes filled.
const RESEQUENCE_STATEMENTS: u64 = 1;

/// `web_call` refuses the same `null`s as the JSON-2 binders (`rpc/core.rs`), and checks every
/// argument before `field_name: null` returns `[]`.
#[tokio::test]
async fn web_call_argument_rules_match_the_binders() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(&mut tx, PERSON, &[&[("name", s("Ada"))]]).await;
    let persons = tx.env().records(PERSON).unwrap();
    let err = persons
        .web_call(
            &mut tx,
            "web_name_search",
            &obj(&[
                ("name", s("Ada")),
                ("specification", obj(&[])),
                ("operator", F),
            ]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_name_search(): invalid value False for argument 'operator' (a domain operator)"
    );
    let err = persons
        .web_call(
            &mut tx,
            "web_name_search",
            &obj(&[("specification", obj(&[]))]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_name_search() missing 1 required argument: 'name'"
    );
    // domain null is no domain; operator absent is ilike
    let got = persons
        .web_call(
            &mut tx,
            "web_name_search",
            &obj(&[
                ("name", s("ad")),
                ("specification", obj(&[])),
                ("domain", F),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(got, Value::list([obj(&[("id", int(id(&p, 0)))])]));
    let err = p
        .web_call(
            &mut tx,
            "web_resequence",
            &obj(&[
                ("specification", obj(&[])),
                ("field_name", F),
                ("offset", s("x")),
            ]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_resequence(): invalid value 'x' for argument 'offset' (an integer)"
    );
    let err = p
        .web_call(
            &mut tx,
            "web_resequence",
            &obj(&[("specification", obj(&[])), ("offset", F)]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_resequence(): invalid value False for argument 'offset' (an integer)"
    );
    let err = p
        .web_call(
            &mut tx,
            "web_resequence",
            &obj(&[("specification", obj(&[])), ("field_name", int(3))]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_resequence(): invalid value 3 for argument 'field_name' (a string or null)"
    );
    // a negative limit names what the argument accepts, as the JSON-2 binder does (D-S3)
    let err = persons
        .web_call(
            &mut tx,
            "web_name_search",
            &obj(&[
                ("name", s("Ada")),
                ("specification", obj(&[])),
                ("limit", int(-1)),
            ]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_name_search(): invalid value -1 for argument 'limit' (a non-negative integer or null)"
    );
    let err = persons
        .web_call(
            &mut tx,
            "web_search_read",
            &obj(&[("specification", obj(&[])), ("offset", F)]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "web_search_read(): invalid value False for argument 'offset' (a non-negative integer)"
    );
    let got = p
        .web_call(
            &mut tx,
            "web_resequence",
            &obj(&[("specification", obj(&[])), ("field_name", F)]),
        )
        .await
        .unwrap();
    assert_eq!(got, Value::list([]));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_033_web_unlink_blocked_by_references() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let persons = create(
        &mut tx,
        PERSON,
        &[
            &[("name", s("a"))],
            &[("name", s("b"))],
            &[("name", s("c"))],
        ],
    )
    .await;
    let rooms = create(
        &mut tx,
        "test_web.room",
        &[&[("name", s("r1"))], &[("name", s("r2"))]],
    )
    .await;
    create(
        &mut tx,
        "test_web.booking",
        &[
            &[
                ("person_id", int(id(&persons, 1))),
                ("room_id", int(id(&rooms, 0))),
            ],
            &[("person_id", int(id(&persons, 2)))],
        ],
    )
    .await;
    let err = persons.web_unlink(&mut tx).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "The operation cannot be completed: this record is used by Booking"
    );
    let ctx = err.context().unwrap().clone();
    assert_eq!(
        ctx,
        obj(&[
            ("archivable", Value::Bool(true)),
            ("model_name", s("Booking")),
            ("res_model", s(PERSON)),
            ("res_ids", Value::list(persons.ids().into_iter().map(int))),
            (
                "blocked_ids",
                Value::list([int(id(&persons, 1)), int(id(&persons, 2))])
            ),
        ])
    );
    // nothing was deleted, and the transaction is usable
    assert_eq!(persons.exists(&mut tx).await.unwrap().len(), 3);
    assert!(!tx.is_poisoned());
    let err = rooms.web_unlink(&mut tx).await.unwrap_err();
    let Some(Value::Map(ctx)) = err.context() else {
        panic!()
    };
    assert_eq!(ctx.get("archivable"), Some(&Value::Bool(false)));
    assert_eq!(
        ctx.get("blocked_ids"),
        Some(&Value::list([int(id(&rooms, 0))]))
    );
    assert!(rooms.at(1).unwrap().web_unlink(&mut tx).await.unwrap());
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

const AGG: &str = "test_read_group.aggregate";

async fn fixture_a(tx: &mut Transaction) -> (i64, i64, Records) {
    let partners = create(
        tx,
        "test_read_group.partner",
        &[&[("name", s("z_one"))], &[("name", s("a_two"))]],
    )
    .await;
    let (p1, p2) = (id(&partners, 0), id(&partners, 1));
    let rows: Vec<Vec<(&str, Value)>> = [
        (Some(1), Some(p1), Some(1)),
        (Some(1), Some(p1), Some(2)),
        (Some(1), Some(p2), Some(3)),
        (Some(2), Some(p2), Some(4)),
        (Some(2), Some(p2), None),
        (Some(2), None, Some(5)),
        (None, Some(p2), Some(5)),
        (None, None, Some(6)),
        (None, None, None),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (k, p, val))| {
        let mut row = vec![("name", Value::str(format!("record{}", i + 1)))];
        if let Some(k) = k {
            row.push(("key", int(k)));
        }
        if let Some(p) = p {
            row.push(("partner_id", int(p)));
        }
        if let Some(val) = val {
            row.push(("value", int(val)));
        }
        row
    })
    .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let records = create(tx, AGG, &refs).await;
    (p1, p2, records)
}

fn eq(field: &str, value: Value) -> Value {
    Value::list([s(field), s("="), value])
}

fn group(pairs: &[(&str, Value)]) -> erp_sdk::ValueMap {
    support::vmap(pairs)
}

#[tokio::test]
async fn spec_grp_004_formatted_empty_domain() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    fixture_a(&mut tx).await;
    let records = tx.env().records(AGG).unwrap();
    let empty = || dom(json!([["id", "in", []]]));
    let got = assert_query_count!(tx, <= 0, {
        records
            .formatted_read_group(&mut tx, FormattedReadGroup::new(empty(), ["partner_id"]))
            .await
            .unwrap()
    });
    assert!(got.groups.is_empty());
    let got = assert_query_count!(tx, <= 0, {
        records
            .formatted_read_group(
                &mut tx,
                FormattedReadGroup::new(empty(), Vec::<String>::new()).aggregates([
                    "__count",
                    "partner_id:count",
                    "partner_id:count_distinct",
                ]),
            )
            .await
            .unwrap()
    });
    assert_eq!(
        got.groups,
        vec![group(&[
            ("__count", int(0)),
            ("__extra_domain", Value::list([])),
            ("partner_id:count", int(0)),
            ("partner_id:count_distinct", int(0)),
        ])]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_034_formatted_groups() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (p1, p2, records) = fixture_a(&mut tx).await;
    let model = tx.env().records(AGG).unwrap();
    let call = |groupby: &[&str]| {
        obj(&[
            ("domain", Value::list([])),
            ("groupby", Value::list(groupby.iter().map(|g| s(g)))),
            ("aggregates", Value::list([s("value:sum")])),
        ])
    };
    let got = model
        .web_call(&mut tx, "formatted_read_group", &call(&["key"]))
        .await
        .unwrap();
    assert_eq!(
        got,
        Value::list([
            obj(&[
                ("__extra_domain", Value::list([eq("key", int(1))])),
                ("key", int(1)),
                ("value:sum", int(6)),
            ]),
            obj(&[
                ("__extra_domain", Value::list([eq("key", int(2))])),
                ("key", int(2)),
                ("value:sum", int(9)),
            ]),
            obj(&[
                ("__extra_domain", Value::list([eq("key", F)])),
                ("key", F),
                ("value:sum", int(11)),
            ]),
        ])
    );
    let got = model
        .web_call(
            &mut tx,
            "formatted_read_group",
            &call(&["key", "partner_id"]),
        )
        .await
        .unwrap();
    let Value::List(groups) = &got else { panic!() };
    // ordered by the comodel's name: a_two (p2) first
    assert_eq!(
        groups[0],
        obj(&[
            (
                "__extra_domain",
                Value::list([s("&"), eq("key", int(1)), eq("partner_id", int(p2))])
            ),
            ("key", int(1)),
            ("partner_id", Value::list([int(p2), s("a_two")])),
            ("value:sum", int(3)),
        ])
    );
    assert_eq!(
        groups[1],
        obj(&[
            (
                "__extra_domain",
                Value::list([s("&"), eq("key", int(1)), eq("partner_id", int(p1))])
            ),
            ("key", int(1)),
            ("partner_id", Value::list([int(p1), s("z_one")])),
            ("value:sum", int(3)),
        ])
    );
    assert_eq!(
        groups[3],
        obj(&[
            (
                "__extra_domain",
                Value::list([s("&"), eq("key", int(2)), eq("partner_id", F)])
            ),
            ("key", int(2)),
            ("partner_id", F),
            ("value:sum", int(5)),
        ])
    );
    let got = model
        .formatted_read_group(
            &mut tx,
            FormattedReadGroup::new(
                Domain::field("id").unwrap().in_(vec![id(&records, 0)]),
                ["id"],
            )
            .aggregates(["__count"]),
        )
        .await
        .unwrap();
    assert_eq!(
        got.groups,
        vec![group(&[
            ("__count", int(1)),
            (
                "__extra_domain",
                Value::list([eq("id", int(id(&records, 0)))])
            ),
            ("id", Value::list([int(id(&records, 0)), s("record1")])),
        ])]
    );
    // the extra domain selects the group's records
    let rows = model
        .search(&mut tx, &support::dom(json!([["key", "=", 1]])))
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    drop(tx);
    h.cleanup().await.unwrap();
}

const FT: &str = "test_read_group.fill_temporal";

fn date(y: i32, m: u32, d: u32) -> Date {
    Date::from_ymd(y, m, d).unwrap()
}

#[tokio::test]
async fn spec_grp_035_formatted_date_groups() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let dates = [
        None,
        Some(date(2022, 1, 29)),
        Some(date(2022, 1, 29)),
        Some(date(2022, 1, 30)),
        Some(date(2022, 1, 31)),
        Some(date(2022, 2, 1)),
        Some(date(2022, 5, 29)),
        Some(date(2023, 1, 29)),
    ];
    let rows: Vec<Vec<(&str, Value)>> = dates
        .iter()
        .map(|d| {
            d.map(|d| vec![("date", Value::Date(d))])
                .unwrap_or_default()
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, FT, &refs).await;
    let model = tx.env().records(FT).unwrap();
    let fmt = async |tx: &mut Transaction, records: &Records, g: &str| {
        records
            .formatted_read_group(
                tx,
                FormattedReadGroup::new(Domain::TRUE, [g]).aggregates(["__count"]),
            )
            .await
            .unwrap()
            .groups
    };
    let range = |field: &str, a: &str, b: &str| {
        Value::list([
            s("&"),
            Value::list([s(field), s(">="), s(a)]),
            Value::list([s(field), s("<"), s(b)]),
        ])
    };
    let days = fmt(&mut tx, &model, "date:day").await;
    assert_eq!(
        days[0],
        group(&[
            ("__count", int(2)),
            ("__extra_domain", range("date", "2022-01-29", "2022-01-30")),
            ("date:day", Value::list([s("2022-01-29"), s("29 Jan 2022")])),
        ])
    );
    assert_eq!(
        days.last().unwrap(),
        &group(&[
            ("__count", int(1)),
            ("__extra_domain", Value::list([eq("date", F)])),
            ("date:day", F),
        ])
    );
    let weeks = fmt(&mut tx, &model, "date:week").await;
    assert_eq!(
        weeks[0],
        group(&[
            ("__count", int(2)),
            ("__extra_domain", range("date", "2022-01-23", "2022-01-30")),
            ("date:week", Value::list([s("2022-01-23"), s("W5 2022")])),
        ])
    );
    assert_eq!(
        weeks[1].get("date:week"),
        Some(&Value::list([s("2022-01-30"), s("W6 2022")]))
    );
    let months = fmt(&mut tx, &model, "date:month").await;
    assert_eq!(
        months[0],
        group(&[
            ("__count", int(4)),
            ("__extra_domain", range("date", "2022-01-01", "2022-02-01")),
            (
                "date:month",
                Value::list([s("2022-01-01"), s("January 2022")])
            ),
        ])
    );
    // datetimes by day in Brussels: bounds are local midnights in UTC
    let at = |y, mo, d, h, mi| DateTime::from_parts(date(y, mo, d), h, mi, 0, 0).unwrap();
    let rows = create(
        &mut tx,
        FT,
        &[&[("datetime", Value::DateTime(at(2022, 6, 30, 22, 30)))]],
    )
    .await;
    let brussels = model.with_context([("tz", s("Europe/Brussels"))]);
    let got = brussels
        .formatted_read_group(
            &mut tx,
            FormattedReadGroup::new(
                Domain::field("id").unwrap().in_(rows.ids()),
                ["datetime:day"],
            )
            .aggregates(["__count"]),
        )
        .await
        .unwrap()
        .groups;
    assert_eq!(
        got,
        vec![group(&[
            ("__count", int(1)),
            (
                "__extra_domain",
                range("datetime", "2022-06-30 22:00:00", "2022-07-01 22:00:00")
            ),
            (
                "datetime:day",
                Value::list([s("2022-06-30 22:00:00"), s("01 Jul 2022")])
            ),
        ])]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

const TASK: &str = "test_read_group.task";

#[tokio::test]
async fn spec_grp_036_formatted_part_numbers_many2many_and_paths() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        FT,
        &[&[("date", Value::Date(date(2022, 1, 29)))], &[]],
    )
    .await;
    let model = tx.env().records(FT).unwrap();
    let got = model
        .formatted_read_group(
            &mut tx,
            FormattedReadGroup::new(Domain::TRUE, ["date:month_number"]).aggregates(["__count"]),
        )
        .await
        .unwrap()
        .groups;
    assert_eq!(
        got[0],
        group(&[
            ("__count", int(1)),
            (
                "__extra_domain",
                Value::list([eq("date.month_number", int(1))])
            ),
            ("date:month_number", int(1)),
        ])
    );
    let users = create(&mut tx, "test_read_group.user", &[&[("name", s("Mario"))]]).await;
    let mario = id(&users, 0);
    create(
        &mut tx,
        TASK,
        &[
            &[("name", s("a")), ("user_ids", Value::list([int(mario)]))],
            &[("name", s("b"))],
        ],
    )
    .await;
    let tasks = tx.env().records(TASK).unwrap();
    let got = tasks
        .formatted_read_group(
            &mut tx,
            FormattedReadGroup::new(Domain::TRUE, ["user_ids"]).aggregates(["__count"]),
        )
        .await
        .unwrap()
        .groups;
    assert_eq!(
        got,
        vec![
            group(&[
                ("__count", int(1)),
                ("__extra_domain", Value::list([eq("user_ids", int(mario))])),
                ("user_ids", Value::list([int(mario), s("Mario")])),
            ]),
            group(&[
                ("__count", int(1)),
                (
                    "__extra_domain",
                    Value::list([Value::list([s("user_ids"), s("not any"), Value::list([])])])
                ),
                ("user_ids", F),
            ]),
        ]
    );
    // many2one paths
    let bars = create(&mut tx, "test_read_group.bar", &[&[("name", s("bar_a"))]]).await;
    let bar = id(&bars, 0);
    let foos = create(
        &mut tx,
        "test_read_group.foo",
        &[&[("bar_id", int(bar))], &[]],
    )
    .await;
    create(
        &mut tx,
        "test_read_group.chain",
        &[
            &[("foo_id", int(id(&foos, 0)))],
            &[("foo_id", int(id(&foos, 1)))],
        ],
    )
    .await;
    let chains = tx.env().records("test_read_group.chain").unwrap();
    let got = chains
        .formatted_read_group(
            &mut tx,
            FormattedReadGroup::new(Domain::TRUE, ["foo_id.bar_id"]).aggregates(["__count"]),
        )
        .await
        .unwrap()
        .groups;
    let any = |sub: Value| Value::list([s("foo_id"), s("any"), sub]);
    assert_eq!(
        got,
        vec![
            group(&[
                ("__count", int(1)),
                (
                    "__extra_domain",
                    Value::list([any(Value::list([eq("bar_id", int(bar))]))])
                ),
                ("foo_id.bar_id", Value::list([int(bar), s("bar_a")])),
            ]),
            group(&[
                ("__count", int(1)),
                (
                    "__extra_domain",
                    Value::list([
                        s("|"),
                        Value::list([s("foo_id"), s("not any"), Value::list([])]),
                        any(Value::list([eq("bar_id", F)])),
                    ])
                ),
                ("foo_id.bar_id", F),
            ]),
        ]
    );
    // the extra domains select the groups' records
    for g in &got {
        let extra = g.get("__extra_domain").unwrap();
        let found = chains
            .search(&mut tx, &erp_orm::web::domain_from_value(extra).unwrap())
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "{extra}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_039_formatted_read_grouping_sets() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    fixture_a(&mut tx).await;
    let model = tx.env().records(AGG).unwrap();
    let sets = [vec!["key", "partner_id"], vec!["key"], vec![]];
    let got = model
        .formatted_read_grouping_sets(
            &mut tx,
            erp_orm::FormattedReadGroupingSets {
                domain: Domain::TRUE,
                grouping_sets: sets
                    .iter()
                    .map(|s| s.iter().map(|g| (*g).to_owned()).collect())
                    .collect(),
                aggregates: vec!["value:sum".to_owned()],
                order: None,
            },
        )
        .await
        .unwrap();
    for (set, groups) in sets.iter().zip(&got) {
        let one = model
            .formatted_read_group(
                &mut tx,
                FormattedReadGroup::new(Domain::TRUE, set.iter().copied())
                    .aggregates(["value:sum"]),
            )
            .await
            .unwrap()
            .to_value();
        assert_eq!(&Value::list(groups.clone()), &one, "{set:?}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn keyed_rows(tx: &mut Transaction) {
    let rows: Vec<Vec<(&str, Value)>> = [
        Some(1),
        Some(1),
        Some(1),
        Some(2),
        Some(2),
        Some(2),
        None,
        None,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, k)| {
        let mut row = vec![("value", int(i as i64))];
        if let Some(k) = k {
            row.push(("key", int(k)));
        }
        row
    })
    .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(tx, AGG, &refs).await;
}

fn groups_of(value: &Value) -> (Vec<Value>, Value) {
    let Value::Map(m) = value else {
        panic!("{value}")
    };
    let Some(Value::List(groups)) = m.get("groups") else {
        panic!("{value}")
    };
    (groups.to_vec(), m.get("length").cloned().unwrap())
}

fn field(group: &Value, name: &str) -> Value {
    let Value::Map(m) = support::deep(group) else {
        panic!()
    };
    m.get(name).cloned().unwrap_or(F)
}

#[tokio::test]
async fn spec_grp_040_web_read_group_length_and_query_count() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    keyed_rows(&mut tx).await;
    let model = tx.env().records(AGG).unwrap();
    let call = |limit: Option<u64>, offset: u64, order: Option<&str>| {
        let mut args = WebReadGroup::new(Domain::TRUE, ["key"]);
        args.limit = limit;
        args.offset = offset;
        args.order = order.map(str::to_owned);
        args
    };
    let got = assert_query_count!(tx, >= 1, <= 1, {
        model.web_read_group(&mut tx, call(Some(4), 0, None)).await.unwrap()
    });
    let (groups, length) = groups_of(&got);
    assert_eq!(groups.len(), 3);
    assert_eq!(length, int(3));
    assert_eq!(field(&groups[0], "__count"), int(3));
    let got = assert_query_count!(tx, >= 1, <= 1, {
        model.web_read_group(&mut tx, call(Some(2), 0, None)).await.unwrap()
    });
    let (groups, length) = groups_of(&got);
    assert_eq!((groups.len(), length), (2, int(3)));
    let got = assert_query_count!(tx, >= 1, <= 1, {
        model.web_read_group(&mut tx, call(None, 1, None)).await.unwrap()
    });
    let (groups, length) = groups_of(&got);
    assert_eq!((groups.len(), length), (2, int(3)));
    let got = assert_query_count!(tx, >= 1, <= 1, {
        model
            .web_read_group(&mut tx, call(Some(2), 1, Some("key DESC")))
            .await
            .unwrap()
    });
    let (groups, length) = groups_of(&got);
    assert_eq!(length, int(3));
    assert_eq!(
        groups.iter().map(|g| field(g, "key")).collect::<Vec<_>>(),
        vec![int(2), int(1)]
    );
    let err = model
        .web_read_group(
            &mut tx,
            WebReadGroup::new(Domain::TRUE, Vec::<String>::new()),
        )
        .await
        .unwrap_err();
    assert_eq!(err.message(), "Need at least one item to groupby");
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_041_auto_unfold_without_n_plus_one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    keyed_rows(&mut tx).await;
    tx.invalidate_all().await.unwrap();
    let model = tx
        .env()
        .records(AGG)
        .unwrap()
        .with_context([("max_number_opened_groups", int(2))]);
    let mut args = WebReadGroup::new(Domain::TRUE, ["key"]);
    args.auto_unfold = true;
    args.unfold_read_specification = Some(spec(json!({"key": {}, "value": {}})));
    let got = assert_query_count!(tx, >= 2, <= 2, {
        model.web_read_group(&mut tx, args.clone()).await.unwrap()
    });
    let (groups, _) = groups_of(&got);
    let records = |g: &Value| match field(g, "__records") {
        Value::List(items) => Some(items.to_vec()),
        _ => None,
    };
    let first = records(&groups[0]).unwrap();
    assert_eq!(first.len(), 3);
    assert_eq!(field(&first[0], "key"), int(1));
    assert_eq!(field(&first[0], "value"), int(0));
    assert_eq!(records(&groups[1]).unwrap().len(), 3);
    assert!(records(&groups[2]).is_none());
    // paged: the second group only, still two statements
    args.offset = 1;
    args.limit = Some(1);
    let got = assert_query_count!(tx, >= 2, <= 2, {
        model.web_read_group(&mut tx, args).await.unwrap()
    });
    let (groups, length) = groups_of(&got);
    assert_eq!(length, int(3));
    assert_eq!(field(&groups[0], "key"), int(2));
    assert_eq!(records(&groups[0]).unwrap().len(), 3);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_042_reopening_groups_from_opening_info() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (p1, p2, _) = fixture_a(&mut tx).await;
    let model = tx.env().records(AGG).unwrap();
    let mut args = WebReadGroup::new(Domain::TRUE, ["key", "partner_id"]);
    args.unfold_read_specification = Some(spec(json!({"value": {}})));
    args.opening_info = Some(vec![
        // key 1: open, its p1 sub-group open with an offset beyond its count (reset to 0)
        obj(&[
            ("value", int(1)),
            ("folded", Value::Bool(false)),
            ("limit", int(80)),
            ("offset", int(0)),
            (
                "groups",
                Value::list([obj(&[
                    ("value", int(p1)),
                    ("folded", Value::Bool(false)),
                    ("limit", int(80)),
                    ("offset", int(5)),
                ])]),
            ),
        ]),
        // key 2: folded
        obj(&[("value", int(2)), ("folded", Value::Bool(true))]),
        // empty key: open, its p2 sub-group restricted by a progress bar domain
        obj(&[
            ("value", F),
            ("folded", Value::Bool(false)),
            ("limit", int(80)),
            ("offset", int(0)),
            (
                "groups",
                Value::list([obj(&[
                    ("value", int(p2)),
                    ("folded", Value::Bool(false)),
                    ("limit", int(80)),
                    ("offset", int(0)),
                    ("progressbar_domain", Value::list([eq("value", int(42))])),
                ])]),
            ),
        ]),
    ]);
    let got = model.web_read_group(&mut tx, args).await.unwrap();
    let (groups, _) = groups_of(&got);
    let sub = |g: &Value| match field(g, "__groups") {
        Value::Map(m) => match m.get("groups") {
            Some(Value::List(items)) => items.to_vec(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    // key 1 opened; inside it only the p1 sub-group listed as open has records
    let key1 = sub(&groups[0]);
    assert_eq!(key1.len(), 2);
    let p1_group = key1
        .iter()
        .find(|g| {
            field(g, "partner_id") != F
                && field(g, "partner_id") == Value::list([int(p1), s("z_one")])
        })
        .unwrap();
    assert_eq!(field(p1_group, "__offset"), int(0));
    let Value::List(recs) = field(p1_group, "__records") else {
        panic!("{p1_group}")
    };
    assert_eq!(recs.len(), 2);
    let p2_group = key1
        .iter()
        .find(|g| field(g, "partner_id") == Value::list([int(p2), s("a_two")]))
        .unwrap();
    assert_eq!(field(p2_group, "__records"), F);
    // key 2 folded: no sub-groups; no __fold keys anywhere
    assert_eq!(field(&groups[1], "__groups"), F);
    assert!(groups.iter().all(|g| field(g, "__fold") == F));
    // empty key: the progress bar domain restricts the records (no value 42)
    let empty_key = sub(&groups[2]);
    let p2_in_empty = empty_key
        .iter()
        .find(|g| field(g, "partner_id") == Value::list([int(p2), s("a_two")]))
        .unwrap();
    assert_eq!(field(p2_in_empty, "__records"), Value::list([]));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_044_active_field_in_web_read_group() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..4)
        .map(|i| {
            vec![
                ("name", Value::str(format!("p{i}"))),
                ("active", Value::Bool(i % 2 == 0)),
                ("sequence", int(1)),
            ]
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, PERSON, &refs).await;
    let persons = tx.env().records(PERSON).unwrap();
    let count = async |tx: &mut Transaction, domain: erp_sdk::serde_json::Value| {
        let got = persons
            .web_read_group(tx, WebReadGroup::new(dom(domain), ["sequence"]))
            .await
            .unwrap();
        let (groups, _) = groups_of(&got);
        field(&groups[0], "__count")
    };
    assert_eq!(
        count(&mut tx, json!([["active", "in", [true, false]]])).await,
        int(4)
    );
    assert_eq!(
        count(&mut tx, json!([["active", "=", false]])).await,
        int(2)
    );
    assert_eq!(count(&mut tx, json!([])).await, int(2));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_045_groupby_read_specification() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (p1, p2, _) = fixture_a(&mut tx).await;
    let model = tx.env().records(AGG).unwrap();
    let mut args = WebReadGroup::new(Domain::TRUE, ["partner_id"]);
    args.groupby_read_specification = Some(BTreeMap::from([(
        "partner_id".to_owned(),
        spec(json!({"display_name": {}})),
    )]));
    let got = model.web_read_group(&mut tx, args).await.unwrap();
    let (groups, _) = groups_of(&got);
    let values: Vec<Value> = groups.iter().map(|g| field(g, "__values")).collect();
    assert_eq!(
        values,
        vec![
            obj(&[("id", int(p2)), ("display_name", s("a_two"))]),
            obj(&[("id", int(p1)), ("display_name", s("z_one"))]),
            obj(&[("id", F)]),
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_046_read_progress_bar() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = create(&mut tx, "test_read_group.partner", &[&[("name", s("p"))]]).await;
    let p = id(&partners, 0);
    create(
        &mut tx,
        "test_read_group.selection",
        &[
            &[("sel", s("a"))],
            &[("sel", s("a"))],
            &[("sel", s("b"))],
            &[("sel", s("c"))],
        ],
    )
    .await;
    let model = tx.env().records("test_read_group.selection").unwrap();
    let got = model
        .web_call(
            &mut tx,
            "read_progress_bar",
            &obj(&[
                ("domain", Value::list([])),
                ("group_by", s("flag")),
                (
                    "progress_bar",
                    obj(&[
                        ("field", s("sel")),
                        ("colors", obj(&[("a", s("success")), ("b", s("danger"))])),
                    ]),
                ),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(got, obj(&[("False", obj(&[("a", int(2)), ("b", int(1))]))]));
    let _ = p;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn web_unlink_blocked_through_a_cascade() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let persons = create(
        &mut tx,
        PERSON,
        &[&[("name", s("free"))], &[("name", s("seated"))]],
    )
    .await;
    let tickets = create(
        &mut tx,
        "test_web.ticket",
        &[
            &[("person_id", int(id(&persons, 0)))],
            &[("person_id", int(id(&persons, 1)))],
        ],
    )
    .await;
    create(
        &mut tx,
        "test_web.seat",
        &[&[("ticket_id", int(id(&tickets, 1)))]],
    )
    .await;
    let err = persons.web_unlink(&mut tx).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "The operation cannot be completed: this record is used by Seat"
    );
    let Some(Value::Map(ctx)) = err.context() else {
        panic!()
    };
    assert_eq!(
        ctx.get("blocked_ids"),
        Some(&Value::list([int(id(&persons, 1))]))
    );
    assert_eq!(persons.exists(&mut tx).await.unwrap().len(), 2);
    assert!(!tx.is_poisoned());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Lessons named "secret" are hidden from users.
struct HideSecret;

impl erp_testkit::TestRules for HideSecret {
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
        _op: Operation,
    ) -> Result<Domain> {
        Ok(match model.name {
            LESSON => Domain::condition("name", erp_sdk::Operator::Ne, "secret").unwrap(),
            _ => Domain::TRUE,
        })
    }
}

#[tokio::test]
async fn web_read_references_with_one_hidden_target() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(HideSecret)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    // the user the record checks run for (its name goes into the access evidence)
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)")
        .await
        .unwrap();
    let lessons = create(
        &mut tx,
        LESSON,
        &[&[("name", s("open"))], &[("name", s("secret"))]],
    )
    .await;
    let courses = create(
        &mut tx,
        COURSE,
        &[
            &[(
                "reference",
                s(&format!("test_web.lesson,{}", id(&lessons, 0))),
            )],
            &[(
                "reference",
                s(&format!("test_web.lesson,{}", id(&lessons, 1))),
            )],
        ],
    )
    .await;
    let as_user = courses.with_user(2);
    let got = as_user
        .web_read(
            &mut tx,
            spec(json!({"reference": {"fields": {"display_name": {}}}})),
        )
        .await
        .unwrap();
    let refs: Vec<Value> = got.iter().map(|r| field(r, "reference")).collect();
    assert_eq!(field(&refs[0], "display_name"), s("open"));
    assert_eq!(
        field(&refs[1], "display_name"),
        s("You don't have access to this record")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 5: records of opened groups are numbered after their page is cut, and ties on
/// the order key keep the order a search gives.
#[tokio::test]
async fn web_read_group_opened_records_with_ties() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..12)
        .map(|i| vec![("key", int(i % 2)), ("value", int(i / 4))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, AGG, &refs).await;
    let model = tx.env().records(AGG).unwrap();
    let mut args = WebReadGroup::new(Domain::TRUE, ["key"]);
    args.auto_unfold = true;
    args.order = Some("value desc".to_owned());
    args.unfold_read_default_limit = Some(4);
    args.unfold_read_specification = Some(spec(json!({"value": {}})));
    let got = model.web_read_group(&mut tx, args).await.unwrap();
    let (groups, _) = groups_of(&got);
    for g in &groups {
        let Value::List(records) = field(g, "__records") else {
            panic!()
        };
        let got_ids: Vec<Value> = records.iter().map(|r| field(r, "id")).collect();
        let extra = erp_orm::web::domain_from_value(&field(g, "__extra_domain")).unwrap();
        let expected = model
            .search_with(
                &mut tx,
                &extra,
                &erp_sdk::SearchOptions::new().order("value desc").limit(4),
            )
            .await
            .unwrap();
        assert_eq!(
            got_ids,
            expected.ids().into_iter().map(int).collect::<Vec<_>>()
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 2: malformed client input to the web methods is a `Validation` error.
#[tokio::test]
async fn web_client_bounds_are_validation_errors() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let persons = create(&mut tx, PERSON, &[&[("name", s("a"))], &[("name", s("b"))]]).await;
    let err = persons
        .web_resequence(&mut tx, Spec::new(), "sequence", i64::MAX)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    let err = persons
        .web_call(&mut tx, "nope", &obj(&[]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    // a specification nested deeper than 16 levels, or naming too many fields
    let mut deep = obj(&[]);
    for _ in 0..20 {
        deep = obj(&[("author_id", obj(&[("fields", deep)]))]);
    }
    let err = Spec::from_value(&deep).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    let wide = Value::Map(Arc::new(
        (0..6000).map(|i| (format!("f{i}"), obj(&[]))).collect(),
    ));
    let err = Spec::from_value(&wide).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_015_ordering_by_aggregates_and_day_of_week() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (p1, p2, _) = fixture_a(&mut tx).await;
    let model = tx.env().records(AGG).unwrap();
    let mut args = FormattedReadGroup::new(Domain::TRUE, ["partner_id"]).aggregates(["value:sum"]);
    args.order = Some("value:sum".to_owned());
    let got = model.formatted_read_group(&mut tx, args).await.unwrap();
    let sums: Vec<(Value, Value)> = got
        .groups
        .iter()
        .map(|g| (g["partner_id"].clone(), g["value:sum"].clone()))
        .collect();
    assert_eq!(
        sums,
        vec![
            (Value::list([int(p1), s("z_one")]), int(3)),
            (F, int(11)),
            (Value::list([int(p2), s("a_two")]), int(12)),
        ]
    );
    let dates = [
        date(2022, 1, 29),
        date(2022, 1, 29),
        date(2022, 1, 30),
        date(2022, 1, 31),
        date(2022, 2, 1),
        date(2022, 5, 29),
        date(2023, 1, 29),
    ];
    let rows: Vec<Vec<(&str, Value)>> = dates
        .iter()
        .map(|d| vec![("date", Value::Date(*d))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, FT, &refs).await;
    let ft = tx.env().records(FT).unwrap();
    for (order, expected) in [
        ("date:day_of_week ASC", vec![0, 1, 2, 6]),
        ("date:day_of_week DESC", vec![6, 2, 1, 0]),
    ] {
        let mut args =
            FormattedReadGroup::new(Domain::TRUE, ["date:day_of_week"]).aggregates(["__count"]);
        args.order = Some(order.to_owned());
        let got = ft.formatted_read_group(&mut tx, args).await.unwrap();
        let days: Vec<Value> = got
            .groups
            .iter()
            .map(|g| g["date:day_of_week"].clone())
            .collect();
        assert_eq!(
            days,
            expected.into_iter().map(int).collect::<Vec<_>>(),
            "{order}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_043_web_read_group_order_mapping() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = [
        (date(2022, 1, 5), 1),
        (date(2022, 1, 9), 7),
        (date(2022, 2, 3), 2),
        (date(2022, 3, 1), 4),
        (date(2022, 3, 2), 9),
    ]
    .into_iter()
    .map(|(d, v)| vec![("date", Value::Date(d)), ("value", int(v))])
    .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, FT, &refs).await;
    let ft = tx.env().records(FT).unwrap();
    let mut args = WebReadGroup::new(Domain::TRUE, ["date:month"]);
    args.aggregates = vec!["value:sum".to_owned()];
    args.order = Some("date desc, value desc, __count".to_owned());
    args.auto_unfold = true;
    args.unfold_read_specification = Some(spec(json!({"value": {}})));
    let got = ft.web_read_group(&mut tx, args).await.unwrap();
    let (groups, _) = groups_of(&got);
    let months: Vec<Value> = groups
        .iter()
        .map(|g| match field(g, "date:month") {
            Value::List(pair) => pair[0].clone(),
            other => other,
        })
        .collect();
    assert_eq!(
        months,
        vec![s("2022-03-01"), s("2022-02-01"), s("2022-01-01")]
    );
    // records inside a group: the non-group terms (value desc), then the model order
    let Value::List(march) = field(&groups[0], "__records") else {
        panic!()
    };
    let values: Vec<Value> = march.iter().map(|r| field(r, "value")).collect();
    assert_eq!(values, vec![int(9), int(4)]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The `web` addon's methods are ordinary model methods on `base`: an addon chain
/// reaches them and the registry always contains the addon.
#[tokio::test]
async fn web_methods_are_registered_on_base_by_the_web_addon() {
    let h = harness().await;
    let model = h.registry().model_id(COURSE).unwrap();
    for method in [
        "web_read",
        "web_search_read",
        "web_name_search",
        "web_save",
        "web_save_multi",
        "web_resequence",
        "web_unlink",
        "formatted_read_group",
        "formatted_read_grouping_sets",
        "web_read_group",
        "read_progress_bar",
    ] {
        let chain = h.orm().dispatch().implementations(model, method);
        assert_eq!(chain.len(), 1, "{method}");
        assert_eq!(
            (chain[0].addon, chain[0].model),
            ("web", "base"),
            "{method}"
        );
    }
    assert!(h.registry().addons().iter().any(|a| a.name == "web"));
    let _ = SpecItem::default();
    h.cleanup().await.unwrap();
}

/// Review item 11: a day bucket starting in a DST gap at midnight (São Paulo, 2018-11-04 00:00
/// did not exist) gets its UTC bounds from the first existing instant, like Python's `fold=0`;
/// the extra domain selects the record.
#[tokio::test]
async fn formatted_datetime_bucket_in_a_midnight_dst_gap() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let at = DateTime::from_parts(date(2018, 11, 4), 12, 0, 0, 0).unwrap();
    let rows = create(&mut tx, FT, &[&[("datetime", Value::DateTime(at))]]).await;
    let model = tx
        .env()
        .records(FT)
        .unwrap()
        .with_context([("tz", s("America/Sao_Paulo"))]);
    let got = model
        .formatted_read_group(
            &mut tx,
            FormattedReadGroup::new(
                Domain::field("id").unwrap().in_(rows.ids()),
                ["datetime:day"],
            )
            .aggregates(["__count"]),
        )
        .await
        .unwrap()
        .groups;
    let extra = got[0]["__extra_domain"].clone();
    assert_eq!(
        extra,
        Value::list([
            s("&"),
            Value::list([s("datetime"), s(">="), s("2018-11-04 03:00:00")]),
            Value::list([s("datetime"), s("<"), s("2018-11-05 02:00:00")]),
        ])
    );
    let found = model
        .search(&mut tx, &erp_orm::web::domain_from_value(&extra).unwrap())
        .await
        .unwrap();
    assert_eq!(found.ids(), rows.ids());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn web_read_group_orders_numeric_columns_by_their_sum() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = [(1, 10), (1, 1), (2, 3), (3, 7), (3, 7)]
        .into_iter()
        .map(|(k, v)| vec![("key", int(k)), ("value", int(v))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, AGG, &refs).await;
    let model = tx.env().records(AGG).unwrap();
    let mut args = WebReadGroup::new(Domain::TRUE, ["key"]);
    args.order = Some("value desc".to_owned());
    let got = model.web_read_group(&mut tx, args).await.unwrap();
    let (groups, _) = groups_of(&got);
    let keys: Vec<Value> = groups.iter().map(|g| field(g, "key")).collect();
    // sums: key 3 → 14, key 1 → 11, key 2 → 3
    assert_eq!(keys, vec![int(3), int(1), int(2)]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 4: a first level grouped by a many2one (ordered by the comodel) with a limit gets
/// its length from the same statement, the limit and window count applied around the grouped rows.
#[tokio::test]
async fn web_read_group_many2one_limit_and_length() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (p1, p2, _) = fixture_a(&mut tx).await;
    let model = tx.env().records(AGG).unwrap();
    let mut args = WebReadGroup::new(Domain::TRUE, ["partner_id"]);
    args.limit = Some(2);
    let got = assert_query_count!(tx, >= 1, <= 2, {
        model.web_read_group(&mut tx, args).await.unwrap()
    });
    let (groups, length) = groups_of(&got);
    assert_eq!(length, int(3));
    let values: Vec<Value> = groups.iter().map(|g| field(g, "partner_id")).collect();
    assert_eq!(
        values,
        vec![
            Value::list([int(p2), s("a_two")]),
            Value::list([int(p1), s("z_one")]),
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_019_sum_currency_aggregator_rejection() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_test_models::test_orm::ADDON,
        &test_read_group::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();

    // 1) Non-monetary field rejects with: Aggregator "sum_currency" only works on currency field for '...'
    let agg = tx.env().records("test_read_group.aggregate").unwrap();
    let err_non_monetary = agg
        .read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).aggregates(["value:sum_currency"]),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err_non_monetary.message(),
        "Aggregator \"sum_currency\" only works on currency field for 'value'"
    );

    // 2) Monetary field rejects until res.currency rates land
    let mixed = tx.env().records("test_orm.mixed").unwrap();
    let err_monetary = mixed
        .read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).aggregates(["monetary:sum_currency"]),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err_monetary.message(),
        "the 'sum_currency' aggregate needs currency rates (res.currency): not supported until M3"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_038_fill_temporal_context_fails_loudly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records(FT).unwrap();
    let with_ctx = model.with_context([("fill_temporal", Value::Bool(true))]);
    let err = with_ctx
        .formatted_read_group(
            &mut tx,
            FormattedReadGroup::new(Domain::TRUE, ["date:month"]).aggregates(["__count"]),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "the fill_temporal context key of grouped reads is not supported until M4 (web client views)"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

fn test_val_map(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Value {
    let mut map = erp_fields::ValueMap::new();
    for (k, v) in entries {
        map.insert(k.into(), v);
    }
    Value::Map(Arc::new(map))
}

fn test_map_get(v: &Value, key: &str) -> Option<Value> {
    let dict = v.dict().expect("panel dictionary");
    if dict.contains(key) {
        Some(dict.get(key).unwrap())
    } else {
        None
    }
}

fn test_as_list(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::List(values) => Some(values.to_vec()),
        Value::Rows(rows) => Some(rows.iter().unwrap().map(Value::Row).collect()),
        _ => None,
    }
}

#[tokio::test]
async fn spec_grp_068_search_panel_select_range() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    let source_model = "test_search_panel.source_model";
    let target_model = "test_search_panel.category_target_model";
    let target_no_parent = "test_search_panel.category_target_model_no_parent_name";

    // 1. Empty many2one
    let records = tx.env().records(source_model).unwrap();
    let res_empty = records
        .search_panel_select_range(&mut tx, SearchPanelSelectRange::new("folder_id"))
        .await
        .unwrap();
    assert_eq!(
        res_empty,
        test_val_map([
            ("parent_field".to_string(), Value::str("parent_name_id")),
            ("values".to_string(), Value::list([])),
        ])
    );

    // 2. Setup folders and records
    let p_folders = create(
        &mut tx,
        target_model,
        &[&[("name", s("Folder 1"))], &[("name", s("Folder 2"))]],
    )
    .await;
    let f1_id = id(&p_folders, 0);
    let f2_id = id(&p_folders, 1);

    let c_folders = create(
        &mut tx,
        target_model,
        &[
            &[("name", s("Folder 3")), ("parent_name_id", int(f1_id))],
            &[("name", s("Folder 4")), ("parent_name_id", int(f2_id))],
        ],
    )
    .await;
    let f3_id = id(&c_folders, 0);
    let f4_id = id(&c_folders, 1);

    create(
        &mut tx,
        source_model,
        &[
            &[
                ("name", s("Rec 1")),
                ("folder_id", int(f1_id)),
                ("state", s("a")),
            ],
            &[
                ("name", s("Rec 2")),
                ("folder_id", int(f3_id)),
                ("state", s("a")),
            ],
            &[
                ("name", s("Rec 3")),
                ("folder_id", int(f4_id)),
                ("state", s("b")),
            ],
            &[("name", s("Rec 4")), ("state", s("b"))],
        ],
    )
    .await;

    // 3. Category selection with hierarchy and counters: query count asserted
    let mut args = SearchPanelSelectRange::new("folder_id");
    args.enable_counters = true;
    args.expand = true;
    let res = assert_query_count!(tx, >= 1, <= 4, {
        records.search_panel_select_range(&mut tx, args).await.unwrap()
    });

    let expected_folders = Value::list([
        test_val_map([
            ("__count".to_string(), Value::Int(2)),
            ("display_name".to_string(), Value::str("Folder 1")),
            ("id".to_string(), Value::Int(f1_id)),
            ("parent_name_id".to_string(), Value::False),
        ]),
        test_val_map([
            ("__count".to_string(), Value::Int(1)),
            ("display_name".to_string(), Value::str("Folder 2")),
            ("id".to_string(), Value::Int(f2_id)),
            ("parent_name_id".to_string(), Value::False),
        ]),
        test_val_map([
            ("__count".to_string(), Value::Int(1)),
            ("display_name".to_string(), Value::str("Folder 3")),
            ("id".to_string(), Value::Int(f3_id)),
            ("parent_name_id".to_string(), Value::Int(f1_id)),
        ]),
        test_val_map([
            ("__count".to_string(), Value::Int(1)),
            ("display_name".to_string(), Value::str("Folder 4")),
            ("id".to_string(), Value::Int(f4_id)),
            ("parent_name_id".to_string(), Value::Int(f2_id)),
        ]),
    ]);
    assert_eq!(
        res,
        test_val_map([
            ("parent_field".to_string(), Value::str("parent_name_id")),
            ("values".to_string(), expected_folders),
        ])
    );

    // 4. Reached limit returns SEARCH_PANEL_ERROR_MESSAGE
    let mut args_lim = SearchPanelSelectRange::new("folder_id");
    args_lim.enable_counters = true;
    args_lim.expand = true;
    args_lim.limit = Some(2);
    let res_lim = records
        .search_panel_select_range(&mut tx, args_lim)
        .await
        .unwrap();
    assert_eq!(
        res_lim,
        test_val_map([(
            "error_msg".to_string(),
            Value::str(SEARCH_PANEL_ERROR_MESSAGE)
        )])
    );

    // 5. Selection field
    let mut args_sel = SearchPanelSelectRange::new("state");
    args_sel.enable_counters = true;
    args_sel.expand = true;
    let res_sel = records
        .search_panel_select_range(&mut tx, args_sel)
        .await
        .unwrap();
    assert_eq!(
        res_sel,
        test_val_map([
            ("parent_field".to_string(), Value::False),
            (
                "values".to_string(),
                Value::list([
                    test_val_map([
                        ("__count".to_string(), Value::Int(2)),
                        ("display_name".to_string(), Value::str("A")),
                        ("id".to_string(), Value::str("a")),
                    ]),
                    test_val_map([
                        ("__count".to_string(), Value::Int(2)),
                        ("display_name".to_string(), Value::str("B")),
                        ("id".to_string(), Value::str("b")),
                    ]),
                ])
            ),
        ])
    );

    // 6. Model without parent_name
    let np_folder = create(
        &mut tx,
        target_no_parent,
        &[&[("name", s("No Parent Folder"))]],
    )
    .await;
    let np_id = id(&np_folder, 0);
    create(
        &mut tx,
        source_model,
        &[&[("name", s("Rec 5")), ("categ_id", int(np_id))]],
    )
    .await;
    let mut args_np = SearchPanelSelectRange::new("categ_id");
    args_np.enable_counters = true;
    let res_np = records
        .search_panel_select_range(&mut tx, args_np)
        .await
        .unwrap();
    assert_eq!(test_map_get(&res_np, "parent_field"), Some(Value::False));

    // 7. Unsupported field type raises UserError with exact kind and message
    let err_unsupported = records
        .search_panel_select_range(&mut tx, SearchPanelSelectRange::new("name"))
        .await
        .unwrap_err();
    assert_eq!(err_unsupported.kind(), ErrorKind::UserError);
    assert_eq!(
        err_unsupported.message(),
        "Only types many2one, selection, many2many are supported for category (found type char)"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_069_search_panel_select_multi_range() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    let source_model = "test_search_panel.source_model";
    let categ_model = "test_search_panel.category_target_model";
    let filter_model = "test_search_panel.filter_target_model";

    let records = tx.env().records(source_model).unwrap();

    // 1. Empty filter
    let res_empty = records
        .search_panel_select_multi_range(&mut tx, SearchPanelSelectMultiRange::new("tag_id"))
        .await
        .unwrap();
    assert_eq!(
        res_empty,
        test_val_map([("values".to_string(), Value::list([]),)])
    );

    // 2. Setup targets and records
    let folders = create(
        &mut tx,
        categ_model,
        &[&[("name", s("Folder 1"))], &[("name", s("Folder 2"))]],
    )
    .await;
    let f1_id = id(&folders, 0);
    let f2_id = id(&folders, 1);

    let tags = create(
        &mut tx,
        filter_model,
        &[
            &[
                ("name", s("Tag 1")),
                ("status", s("cool")),
                ("color", s("Red")),
                ("folder_id", int(f2_id)),
            ],
            &[
                ("name", s("Tag 2")),
                ("status", s("cool")),
                ("color", s("Blue")),
                ("folder_id", int(f1_id)),
            ],
            &[
                ("name", s("Tag 3")),
                ("status", s("cool")),
                ("color", s("Green")),
            ],
        ],
    )
    .await;
    let t1_id = id(&tags, 0);
    let t2_id = id(&tags, 1);
    let t3_id = id(&tags, 2);

    create(
        &mut tx,
        source_model,
        &[
            &[
                ("name", s("Rec 1")),
                ("tag_id", int(t1_id)),
                ("state", s("a")),
            ],
            &[
                ("name", s("Rec 2")),
                ("tag_id", int(t1_id)),
                ("state", s("a")),
            ],
            &[
                ("name", s("Rec 3")),
                ("tag_id", int(t2_id)),
                ("state", s("b")),
            ],
            &[("name", s("Rec 4")), ("state", s("b"))],
        ],
    )
    .await;

    // 3. Multi range with group_by on many2one
    let mut args_gb = SearchPanelSelectMultiRange::new("tag_id");
    args_gb.enable_counters = true;
    args_gb.expand = true;
    args_gb.group_by = Some("folder_id".to_string());
    let res_gb = records
        .search_panel_select_multi_range(&mut tx, args_gb)
        .await
        .unwrap();

    let expected_tags = Value::list([
        test_val_map([
            ("__count".to_string(), Value::Int(2)),
            ("display_name".to_string(), Value::str("Tag 1")),
            ("group_id".to_string(), Value::Int(f2_id)),
            ("group_name".to_string(), Value::str("Folder 2")),
            ("id".to_string(), Value::Int(t1_id)),
        ]),
        test_val_map([
            ("__count".to_string(), Value::Int(1)),
            ("display_name".to_string(), Value::str("Tag 2")),
            ("group_id".to_string(), Value::Int(f1_id)),
            ("group_name".to_string(), Value::str("Folder 1")),
            ("id".to_string(), Value::Int(t2_id)),
        ]),
        test_val_map([
            ("__count".to_string(), Value::Int(0)),
            ("display_name".to_string(), Value::str("Tag 3")),
            ("group_id".to_string(), Value::False),
            ("group_name".to_string(), Value::str("Not Set")),
            ("id".to_string(), Value::Int(t3_id)),
        ]),
    ]);
    assert_eq!(
        res_gb,
        test_val_map([("values".to_string(), expected_tags,)])
    );

    // 4. Multi range with group_by on selection
    let mut args_sel = SearchPanelSelectMultiRange::new("tag_id");
    args_sel.enable_counters = true;
    args_sel.expand = true;
    args_sel.group_by = Some("status".to_string());
    let res_sel = records
        .search_panel_select_multi_range(&mut tx, args_sel)
        .await
        .unwrap();
    let values_sel = test_as_list(&test_map_get(&res_sel, "values").unwrap()).unwrap();
    assert_eq!(values_sel.len(), 3);
    assert_eq!(
        test_map_get(&values_sel[0], "group_id"),
        Some(Value::str("cool"))
    );
    assert_eq!(
        test_map_get(&values_sel[0], "group_name"),
        Some(Value::str("Cool"))
    );

    // 5. Many2many field: counts resolved via ONE grouped query (assert query budget)
    create(
        &mut tx,
        source_model,
        &[
            &[
                ("name", s("M2M Rec 1")),
                ("tag_ids", Value::list([int(t1_id), int(t2_id)])),
            ],
            &[
                ("name", s("M2M Rec 2")),
                ("tag_ids", Value::list([int(t1_id)])),
            ],
        ],
    )
    .await;

    let mut args_m2m = SearchPanelSelectMultiRange::new("tag_ids");
    args_m2m.enable_counters = true;
    args_m2m.expand = true;
    let res_m2m = assert_query_count!(tx, >= 1, <= 2, {
        records
            .search_panel_select_multi_range(&mut tx, args_m2m)
            .await
            .unwrap()
    });
    let m2m_values = test_as_list(&test_map_get(&res_m2m, "values").unwrap()).unwrap();
    assert_eq!(m2m_values.len(), 3);
    assert_eq!(test_map_get(&m2m_values[0], "__count"), Some(Value::Int(2)));
    assert_eq!(test_map_get(&m2m_values[1], "__count"), Some(Value::Int(1)));
    assert_eq!(test_map_get(&m2m_values[2], "__count"), Some(Value::Int(0)));

    // 6. Reached limit returns SEARCH_PANEL_ERROR_MESSAGE
    let mut args_lim = SearchPanelSelectMultiRange::new("tag_id");
    args_lim.expand = true;
    args_lim.limit = Some(2);
    let res_lim = records
        .search_panel_select_multi_range(&mut tx, args_lim)
        .await
        .unwrap();
    assert_eq!(
        res_lim,
        test_val_map([(
            "error_msg".to_string(),
            Value::str(SEARCH_PANEL_ERROR_MESSAGE),
        )])
    );

    // 7. Unsupported field type raises UserError with exact kind and message
    let err_unsupported = records
        .search_panel_select_multi_range(&mut tx, SearchPanelSelectMultiRange::new("name"))
        .await
        .unwrap_err();
    assert_eq!(err_unsupported.kind(), ErrorKind::UserError);
    assert_eq!(
        err_unsupported.message(),
        "Only types many2one, many2many, selection are supported for filter (found type char)"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

fn panel_values(response: &Value) -> Vec<Value> {
    test_as_list(&test_map_get(response, "values").unwrap()).unwrap()
}

#[tokio::test]
async fn search_panel_range_flat_comodel_domain_and_limit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let folders = create(
        &mut tx,
        "test_search_panel.category_target_model",
        &[&[("name", s("Parent"))], &[("name", s("Other"))]],
    )
    .await;
    let child = create(
        &mut tx,
        "test_search_panel.category_target_model",
        &[&[
            ("name", s("Child")),
            ("parent_name_id", int(id(&folders, 0))),
        ]],
    )
    .await;
    create(
        &mut tx,
        "test_search_panel.source_model",
        &[
            &[("name", s("child")), ("folder_id", int(id(&child, 0)))],
            &[("name", s("other")), ("folder_id", int(id(&folders, 1)))],
        ],
    )
    .await;
    let model = tx.env().records("test_search_panel.source_model").unwrap();
    let mut args = SearchPanelSelectRange::new("folder_id");
    args.hierarchize = false;
    args.enable_counters = true;
    args.comodel_domain = Some(Domain::field("id").unwrap().eq(id(&child, 0)));
    let response = model
        .search_panel_select_range(&mut tx, args)
        .await
        .unwrap();
    assert_eq!(test_map_get(&response, "parent_field"), Some(F));
    assert_eq!(
        panel_values(&response),
        &[test_val_map([
            ("id", int(id(&child, 0))),
            ("display_name", s("Child")),
            ("__count", int(1)),
        ])]
    );
    let mut args = SearchPanelSelectRange::new("folder_id");
    args.hierarchize = false;
    args.limit = Some(2);
    let response = model
        .search_panel_select_range(&mut tx, args)
        .await
        .unwrap();
    assert_eq!(
        response,
        test_val_map([("error_msg", s(SEARCH_PANEL_ERROR_MESSAGE))])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_selection_ignores_group_by() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        "test_search_panel.source_model",
        &[&[("name", s("one")), ("state", s("a"))]],
    )
    .await;
    let model = tx.env().records("test_search_panel.source_model").unwrap();
    let mut args = SearchPanelSelectMultiRange::new("state");
    args.enable_counters = true;
    args.group_by = Some("does_not_exist".into());
    let grouped = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap();
    let mut args = SearchPanelSelectMultiRange::new("state");
    args.enable_counters = true;
    let plain = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap();
    assert_eq!(grouped, plain);
    assert_eq!(panel_values(&grouped).len(), 1);
    assert_eq!(test_map_get(&panel_values(&grouped)[0], "group_id"), None);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_missing_limit_does_not_truncate_values_or_counts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..10_001)
        .map(|i| vec![("name", s(&format!("Folder {i:05}")))])
        .collect();
    let refs: Vec<_> = rows.iter().map(Vec::as_slice).collect();
    let folders = create(&mut tx, "test_search_panel.category_target_model", &refs).await;
    let rows: Vec<Vec<(&str, Value)>> = folders
        .ids()
        .iter()
        .map(|&folder| {
            vec![
                ("name", s("source")),
                ("folder_id", int(folder)),
                ("folder_ids", Value::list([int(folder)])),
            ]
        })
        .collect();
    let refs: Vec<_> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, "test_search_panel.source_model", &refs).await;
    let model = tx.env().records("test_search_panel.source_model").unwrap();
    for field in ["folder_id", "folder_ids"] {
        for expand in [false, true] {
            let mut args = SearchPanelSelectRange::new(field);
            args.hierarchize = false;
            args.enable_counters = true;
            args.expand = expand;
            let response = model
                .search_panel_select_range(&mut tx, args)
                .await
                .unwrap();
            assert_eq!(
                panel_values(&response).len(),
                folders.len(),
                "{field} expand={expand}"
            );
            for value in &panel_values(&response) {
                assert_eq!(
                    test_map_get(value, "__count"),
                    Some(int(1)),
                    "{field}: {value:?}"
                );
            }
            let mut args = SearchPanelSelectMultiRange::new(field);
            args.enable_counters = true;
            args.expand = expand;
            let response = model
                .search_panel_select_multi_range(&mut tx, args)
                .await
                .unwrap();
            assert_eq!(
                panel_values(&response).len(),
                folders.len(),
                "{field} expand={expand}"
            );
            for value in &panel_values(&response) {
                assert_eq!(
                    test_map_get(value, "__count"),
                    Some(int(1)),
                    "{field}: {value:?}"
                );
            }
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_group_domains_are_set_based_and_respect_common_domains() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..24)
        .map(|i| {
            vec![
                ("name", s(&format!("Tag {i:02}"))),
                ("status", if i % 2 == 0 { s("cool") } else { s("unknown") }),
            ]
        })
        .collect();
    let refs: Vec<_> = rows.iter().map(Vec::as_slice).collect();
    let tags = create(&mut tx, "test_search_panel.filter_target_model", &refs).await;
    let rows: Vec<Vec<(&str, Value)>> = tags
        .ids()
        .iter()
        .flat_map(|&tag| {
            [
                vec![
                    ("name", s("included")),
                    ("state", s("a")),
                    ("tag_id", int(tag)),
                    ("tag_ids", Value::list([int(tag)])),
                ],
                vec![
                    ("name", s("excluded")),
                    ("state", s("b")),
                    ("tag_id", int(tag)),
                    ("tag_ids", Value::list([int(tag)])),
                ],
            ]
        })
        .collect();
    let refs: Vec<_> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, "test_search_panel.source_model", &refs).await;
    // A source belongs to two groups, but only the unknown group's custom domain admits it.
    create(
        &mut tx,
        "test_search_panel.source_model",
        &[&[
            ("name", s("included")),
            ("state", s("a")),
            (
                "tag_ids",
                Value::list([int(id(&tags, 0)), int(id(&tags, 1))]),
            ),
        ]],
    )
    .await;
    let model = tx.env().records("test_search_panel.source_model").unwrap();
    for count in [1, 24] {
        for custom in [false, true] {
            let mut args = SearchPanelSelectMultiRange::new("tag_ids");
            args.expand = true;
            args.enable_counters = true;
            args.group_by = Some("status".into());
            args.comodel_domain = Some(
                Domain::field("id")
                    .unwrap()
                    .in_(tags.ids()[..count].to_vec()),
            );
            args.category_domain = Some(Domain::field("name").unwrap().eq("included"));
            args.filter_domain = Some(Domain::field("state").unwrap().eq("a"));
            args.group_domain = Some(if custom {
                test_val_map([
                    (
                        "\"cool\"",
                        Value::list([Value::list([s("state"), s("="), s("b")])]),
                    ),
                    ("\"unknown\"", Value::list([])),
                ])
            } else {
                test_val_map(std::iter::empty::<(&str, Value)>())
            });
            // One comodel read and one grouped count statement, independent of item count.
            let response = assert_query_count!(tx, >= 1, <= 2, {
                model.search_panel_select_multi_range(&mut tx, args).await.unwrap()
            });
            assert_eq!(panel_values(&response).len(), count);
            for value in &panel_values(&response) {
                let expected = if custom && test_map_get(value, "group_id") == Some(s("cool")) {
                    0
                } else if [int(id(&tags, 0)), int(id(&tags, 1))]
                    .iter()
                    .any(|tag_id| test_map_get(value, "id") == Some(tag_id.clone()))
                {
                    2
                } else {
                    1
                };
                assert_eq!(test_map_get(value, "__count"), Some(int(expected)));
            }
        }
    }
    let mut args = SearchPanelSelectMultiRange::new("tag_ids");
    args.limit = Some(2);
    let response = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap();
    assert_eq!(
        response,
        test_val_map([("error_msg", s(SEARCH_PANEL_ERROR_MESSAGE))])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_group_domain_validation_and_json_keys() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tags = create(
        &mut tx,
        "test_search_panel.filter_target_model",
        &[&[("name", s("Quoted")), ("status", s("quoted\"é"))]],
    )
    .await;
    create(
        &mut tx,
        "test_search_panel.source_model",
        &[&[
            ("name", s("one")),
            ("tag_ids", Value::list([int(id(&tags, 0))])),
        ]],
    )
    .await;
    let model = tx.env().records("test_search_panel.source_model").unwrap();
    let mut args = SearchPanelSelectMultiRange::new("tag_ids");
    args.enable_counters = true;
    args.group_by = Some("status".into());
    args.group_domain = Some(Value::list([]));
    let error = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "search_panel_select_multi_range: group_domain must be a map of JSON-encoded group ids to domains"
    );

    let mut args = SearchPanelSelectMultiRange::new("tag_ids");
    args.group_domain = Some(Value::Map(Arc::new(
        (0..65)
            .map(|id| (id.to_string(), Value::list([])))
            .collect(),
    )));
    let error = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "search_panel_select_multi_range: group_domain accepts at most 64 entries, got 65"
    );

    let key = erp_sdk::serde_json::to_string("quoted\"é").unwrap();
    let malformed = test_val_map(std::iter::empty::<(&str, Value)>());
    let expected = erp_orm::web::domain_from_value(&malformed).unwrap_err();
    let mut args = SearchPanelSelectMultiRange::new("tag_ids");
    args.enable_counters = true;
    args.group_by = Some("status".into());
    args.group_domain = Some(test_val_map([(key.clone(), malformed)]));
    let error = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), expected.kind());
    assert_eq!(error.message(), expected.message());

    let mut args = SearchPanelSelectMultiRange::new("tag_ids");
    args.enable_counters = true;
    args.group_by = Some("status".into());
    args.group_domain = Some(test_val_map([(
        key,
        Value::list([Value::list([s("id"), s("="), int(0)])]),
    )]));
    let response = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap();
    assert_eq!(panel_values(&response).len(), 1);
    assert_eq!(
        test_map_get(&panel_values(&response)[0], "group_id"),
        Some(s("quoted\"é"))
    );
    assert_eq!(
        test_map_get(&panel_values(&response)[0], "__count"),
        Some(int(0))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Hide source records and comodel records independently, so counters and listing must each
/// apply the corresponding model's rule rather than relying on the other model's visibility.
struct SearchPanelVisibility;

impl erp_testkit::TestRules for SearchPanelVisibility {
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
        _op: Operation,
    ) -> Result<Domain> {
        match model.name {
            "test_search_panel.source_model" | "test_search_panel.category_target_model" => {
                Ok(Domain::condition("name", erp_sdk::Operator::Ne, "secret")?)
            }
            _ => Ok(Domain::TRUE),
        }
    }
}

#[tokio::test]
async fn search_panel_respects_source_and_comodel_record_rules() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .rules(SearchPanelVisibility)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)")
        .await
        .unwrap();
    let folders = create(
        &mut tx,
        "test_search_panel.category_target_model",
        &[&[("name", s("open"))], &[("name", s("secret"))]],
    )
    .await;
    create(
        &mut tx,
        "test_search_panel.source_model",
        &[
            &[
                ("name", s("open")),
                ("folder_id", int(id(&folders, 0))),
                ("folder_ids", Value::list([int(id(&folders, 0))])),
            ],
            &[
                ("name", s("secret")),
                ("folder_id", int(id(&folders, 0))),
                ("folder_ids", Value::list([int(id(&folders, 0))])),
            ],
            &[
                ("name", s("open")),
                ("folder_id", int(id(&folders, 1))),
                ("folder_ids", Value::list([int(id(&folders, 1))])),
            ],
        ],
    )
    .await;
    let model = tx
        .env()
        .records("test_search_panel.source_model")
        .unwrap()
        .with_user(2);
    for field in ["folder_id", "folder_ids"] {
        for expand in [false, true] {
            let mut args = SearchPanelSelectRange::new(field);
            args.hierarchize = false;
            args.enable_counters = true;
            args.expand = expand;
            let response = model
                .search_panel_select_range(&mut tx, args)
                .await
                .unwrap();
            assert_eq!(panel_values(&response).len(), 1);
            assert_eq!(
                test_map_get(&panel_values(&response)[0], "id"),
                Some(int(id(&folders, 0)))
            );
            assert_eq!(
                test_map_get(&panel_values(&response)[0], "__count"),
                Some(int(1))
            );
            let mut args = SearchPanelSelectMultiRange::new(field);
            args.enable_counters = true;
            args.expand = expand;
            let response = model
                .search_panel_select_multi_range(&mut tx, args)
                .await
                .unwrap();
            assert_eq!(panel_values(&response).len(), 1);
            assert_eq!(
                test_map_get(&panel_values(&response)[0], "id"),
                Some(int(id(&folders, 0)))
            );
            assert_eq!(
                test_map_get(&panel_values(&response)[0], "__count"),
                Some(int(1))
            );
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_many2one_group_domain_and_char_groups() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tags = create(
        &mut tx,
        "test_search_panel.filter_target_model",
        &[&[("name", s("Tag")), ("color", s("Red"))]],
    )
    .await;
    create(
        &mut tx,
        "test_search_panel.source_model",
        &[
            &[
                ("name", s("included")),
                ("state", s("a")),
                ("tag_id", int(id(&tags, 0))),
            ],
            &[
                ("name", s("excluded")),
                ("state", s("b")),
                ("tag_id", int(id(&tags, 0))),
            ],
        ],
    )
    .await;
    let model = tx.env().records("test_search_panel.source_model").unwrap();
    let mut args = SearchPanelSelectMultiRange::new("tag_id");
    args.enable_counters = true;
    args.group_by = Some("color".into());
    args.group_domain = Some(Value::list([Value::list([s("state"), s("="), s("a")])]));
    let response = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap();
    assert_eq!(
        panel_values(&response),
        &[test_val_map([
            ("id", int(id(&tags, 0))),
            ("display_name", s("Tag")),
            ("group_id", s("Red")),
            ("group_name", s("Red")),
            ("__count", int(1)),
        ])]
    );

    let malformed = test_val_map(std::iter::empty::<(&str, Value)>());
    let expected = erp_orm::web::domain_from_value(&malformed).unwrap_err();
    let mut args = SearchPanelSelectMultiRange::new("tag_id");
    args.enable_counters = true;
    args.group_domain = Some(malformed);
    let error = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), expected.kind());
    assert_eq!(error.message(), expected.message());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_nonexpanded_hierarchy_includes_child_and_parent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        "test_search_panel.category_target_model",
        &[&[("name", s("Parent"))]],
    )
    .await;
    let child = create(
        &mut tx,
        "test_search_panel.category_target_model",
        &[&[
            ("name", s("Child")),
            ("parent_name_id", int(id(&parent, 0))),
        ]],
    )
    .await;
    create(
        &mut tx,
        "test_search_panel.source_model",
        &[&[("name", s("child only")), ("folder_id", int(id(&child, 0)))]],
    )
    .await;
    let model = tx.env().records("test_search_panel.source_model").unwrap();
    let mut args = SearchPanelSelectRange::new("folder_id");
    args.enable_counters = true;
    let response = model
        .search_panel_select_range(&mut tx, args)
        .await
        .unwrap();
    assert_eq!(
        test_map_get(&response, "parent_field"),
        Some(s("parent_name_id"))
    );
    assert_eq!(
        panel_values(&response),
        &[
            test_val_map([
                ("id", int(id(&child, 0))),
                ("display_name", s("Child")),
                ("parent_name_id", int(id(&parent, 0))),
                ("__count", int(1))
            ]),
            test_val_map([
                ("id", int(id(&parent, 0))),
                ("display_name", s("Parent")),
                ("parent_name_id", F),
                ("__count", int(1))
            ]),
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_expansion_checks_source_field_access_without_counters() {
    let source = "test_search_panel.source_model";
    let policy = erp_testkit::FieldPolicy::new().deny(source, "folder_id", erp_sdk::FieldOp::Read);
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)")
        .await
        .unwrap();
    let model = tx.env().records(source).unwrap().with_user(2);
    let expected = "You do not have enough rights to access the field \"folder_id\" on Source model (test_search_panel.source_model). Please contact your system administrator.\n\nOperation: read";
    let mut args = SearchPanelSelectRange::new("folder_id");
    args.expand = true;
    let error = model
        .search_panel_select_range(&mut tx, args)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), expected);
    let mut args = SearchPanelSelectMultiRange::new("folder_id");
    args.expand = true;
    let error = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_recomputes_across_independent_orm_instances() {
    let h = harness().await;
    let peer = erp_sdk::Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .policy(erp_sdk::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut writer = h.begin().await.unwrap();
    let folder = create(
        &mut writer,
        "test_search_panel.category_target_model",
        &[&[("name", s("Shared"))]],
    )
    .await;
    let folder_id = id(&folder, 0);
    create(
        &mut writer,
        "test_search_panel.source_model",
        &[&[
            ("name", s("first")),
            ("folder_id", int(folder_id)),
            ("folder_ids", Value::list([int(folder_id)])),
        ]],
    )
    .await;
    writer.commit().await.unwrap();
    for expected in [1, 2] {
        for orm in [h.orm(), &peer] {
            let mut reader = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
            let model = reader
                .env()
                .records("test_search_panel.source_model")
                .unwrap();
            let mut range = SearchPanelSelectRange::new("folder_id");
            range.hierarchize = false;
            range.enable_counters = true;
            let value = model
                .search_panel_select_range(&mut reader, range)
                .await
                .unwrap();
            assert_eq!(panel_values(&value).len(), 1);
            assert_eq!(
                test_map_get(&panel_values(&value)[0], "__count"),
                Some(int(expected))
            );
            let mut multi = SearchPanelSelectMultiRange::new("folder_ids");
            multi.enable_counters = true;
            let value = model
                .search_panel_select_multi_range(&mut reader, multi)
                .await
                .unwrap();
            assert_eq!(panel_values(&value).len(), 1);
            assert_eq!(
                test_map_get(&panel_values(&value)[0], "__count"),
                Some(int(expected))
            );
            reader.rollback().await.unwrap();
        }
        if expected == 1 {
            let mut writer = peer.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
            create(
                &mut writer,
                "test_search_panel.source_model",
                &[&[
                    ("name", s("second")),
                    ("folder_id", int(folder_id)),
                    ("folder_ids", Value::list([int(folder_id)])),
                ]],
            )
            .await;
            writer.commit().await.unwrap();
        }
    }
    drop(peer);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_panel_rejects_compound_group_by_on_empty_comodel() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_web.lesson").unwrap();
    let mut args = SearchPanelSelectMultiRange::new("course_id");
    args.expand = true;
    args.group_by = Some("lesson_ids".to_owned());
    let error = model
        .search_panel_select_multi_range(&mut tx, args)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Search panel group_by does not support type one2many"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
