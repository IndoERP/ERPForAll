//! PERF.ROWS-READ review finding 1: a limited x2many whose lines have relational sub-specifications
//! of their own. The lines past the limit answer `{'id': id}`; in the
//! batch they are rows appended to the line level after its many2one and x2many keys were
//! expanded, so those child columns must take a row that lacks the key. The JSON-2 body equals,
//! byte for byte, the body of the same data built as maps from the database rows by this test.
//!
//! Perf slice, not a spec case (GRP §4.8's behaviour, covered by `web.rs`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod bench_fixture;

use std::collections::BTreeMap;
use std::sync::Arc;

use bench_fixture::{create, set_ids, vals};
use erp_orm::{Spec, SpecItem};
use erp_sdk::{Transaction, Value, ValueMap};
use erp_test_models::{base_users, test_web};
use erp_testkit::Harness;

const LIMIT: usize = 64 << 20;

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(Arc::new(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<ValueMap>(),
    ))
}

fn text(s: Option<&str>) -> Value {
    s.map_or(Value::False, Value::str)
}

fn body(values: &[Value]) -> String {
    let bytes = erp_rpc::__output::serialize(&Value::list(values.iter().cloned()), LIMIT).unwrap();
    String::from_utf8(bytes).unwrap()
}

/// The database rows, read with SQL (the expected values never come from the read path).
struct Db {
    persons: BTreeMap<i64, Option<String>>,
    tags: BTreeMap<i64, Option<String>>,
    /// multi id → partner id
    multis: BTreeMap<i64, Option<i64>>,
    /// `(id, multi, name, partner)` in the line model's order (`id`)
    lines: Vec<(i64, i64, Option<String>, Option<i64>)>,
    /// tag ids per line, in the tag model's order (`id`)
    line_tags: BTreeMap<i64, Vec<i64>>,
    /// `(id, line_id, name)` in the sub-line model's order (`id`)
    subs: Vec<(i64, i64, Option<String>)>,
}

async fn load(tx: &mut Transaction) -> Db {
    let mut db = Db {
        persons: BTreeMap::new(),
        tags: BTreeMap::new(),
        multis: BTreeMap::new(),
        lines: Vec::new(),
        line_tags: BTreeMap::new(),
        subs: Vec::new(),
    };
    for r in tx
        .query("SELECT id, name FROM test_web_person", &[])
        .await
        .unwrap()
    {
        db.persons.insert(r.get(0), r.get(1));
    }
    for r in tx
        .query("SELECT id, name FROM test_web_multi_tag", &[])
        .await
        .unwrap()
    {
        db.tags.insert(r.get(0), r.get(1));
    }
    for r in tx
        .query("SELECT id, partner FROM test_web_multi", &[])
        .await
        .unwrap()
    {
        db.multis.insert(r.get(0), r.get(1));
    }
    for r in tx
        .query(
            "SELECT id, multi, name, partner FROM test_web_multi_line ORDER BY id",
            &[],
        )
        .await
        .unwrap()
    {
        db.lines.push((r.get(0), r.get(1), r.get(2), r.get(3)));
    }
    for r in tx
        .query(
            "SELECT line_id, tag_id FROM test_web_multi_line_tag_rel ORDER BY tag_id",
            &[],
        )
        .await
        .unwrap()
    {
        db.line_tags
            .entry(r.get(0))
            .or_default()
            .push(r.get::<_, i64>(1));
    }
    for r in tx
        .query(
            "SELECT id, line_id, name FROM test_web_multi_sub ORDER BY id",
            &[],
        )
        .await
        .unwrap()
    {
        db.subs.push((r.get(0), r.get(1), r.get(2)));
    }
    db
}

/// Each form's partner, and its lines limited to `limit`, each with its partner, tags and
/// sub-lines.
fn spec(limit: u64) -> Spec {
    Spec::new()
        .with("partner", SpecItem::fields(Spec::new().field("name")))
        .with(
            "lines",
            SpecItem::fields(
                Spec::new()
                    .field("name")
                    .with("partner", SpecItem::fields(Spec::new().field("name")))
                    .with("tags", SpecItem::fields(Spec::new().field("name")))
                    .with("sub_ids", SpecItem::fields(Spec::new().field("name"))),
            )
            .limit(limit),
        )
}

fn person(db: &Db, id: Option<i64>) -> Value {
    match id {
        Some(p) => map(vec![
            ("id", Value::Int(p)),
            ("name", text(db.persons[&p].as_deref())),
        ]),
        None => Value::False,
    }
}

fn multi(db: &Db, id: i64, limit: usize) -> Value {
    let lines: Vec<Value> = db
        .lines
        .iter()
        .filter(|l| l.1 == id)
        .enumerate()
        .map(|(i, l)| {
            if i >= limit {
                return map(vec![("id", Value::Int(l.0))]);
            }
            let tags = db.line_tags.get(&l.0).into_iter().flatten().map(|t| {
                map(vec![
                    ("id", Value::Int(*t)),
                    ("name", text(db.tags[t].as_deref())),
                ])
            });
            let subs = db.subs.iter().filter(|s| s.1 == l.0).map(|s| {
                map(vec![
                    ("id", Value::Int(s.0)),
                    ("name", text(s.2.as_deref())),
                ])
            });
            map(vec![
                ("id", Value::Int(l.0)),
                ("name", text(l.2.as_deref())),
                ("partner", person(db, l.3)),
                ("tags", Value::list(tags)),
                ("sub_ids", Value::list(subs)),
            ])
        })
        .collect();
    map(vec![
        ("id", Value::Int(id)),
        ("partner", person(db, db.multis[&id])),
        ("lines", Value::list(lines)),
    ])
}

#[tokio::test]
async fn limited_lines_with_nested_relations_answer_id_only_rows() {
    let h = Harness::builder([&base_users::ADDON, &test_web::ADDON])
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    let persons = create(
        &mut tx,
        "test_web.person",
        &[
            vals(vec![("name", Value::str("Ånna ✓"))]),
            vals(vec![("name", Value::str("Bob \"q\""))]),
        ],
    )
    .await;
    let tags = create(
        &mut tx,
        "test_web.multi_tag",
        &[
            vals(vec![("name", Value::str("x"))]),
            vals(vec![("name", Value::str("y"))]),
        ],
    )
    .await;
    let multis = create(
        &mut tx,
        "test_web.multi",
        &[
            vals(vec![("partner", Value::Int(persons[0]))]),
            vals(vec![("partner", Value::Int(persons[1]))]),
            vals(vec![]),
        ],
    )
    .await;
    let lines = create(
        &mut tx,
        "test_web.multi_line",
        &[
            vals(vec![
                ("multi", Value::Int(multis[0])),
                ("name", Value::str("l1")),
                ("partner", Value::Int(persons[1])),
                ("tags", set_ids(&[tags[1], tags[0]])),
            ]),
            vals(vec![
                ("multi", Value::Int(multis[0])),
                ("name", Value::str("l2")),
            ]),
            vals(vec![
                ("multi", Value::Int(multis[0])),
                ("partner", Value::Int(persons[0])),
                ("tags", set_ids(&[tags[0]])),
            ]),
            vals(vec![
                ("multi", Value::Int(multis[1])),
                ("name", Value::str("l4")),
                ("partner", Value::Int(persons[0])),
                ("tags", set_ids(&[tags[0]])),
            ]),
        ],
    )
    .await;
    create(
        &mut tx,
        "test_web.multi_sub",
        &[
            vals(vec![
                ("line_id", Value::Int(lines[0])),
                ("name", Value::str("s1")),
            ]),
            vals(vec![
                ("line_id", Value::Int(lines[0])),
                ("name", Value::str("s2")),
            ]),
            vals(vec![
                ("line_id", Value::Int(lines[1])),
                ("name", Value::str("s3")),
            ]),
            vals(vec![("line_id", Value::Int(lines[2]))]),
        ],
    )
    .await;
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let db = load(&mut tx).await;
    // a form with three lines, one with one, one without; a duplicate in the recordset
    let ids = [multis[1], multis[0], multis[2], multis[1]];
    for limit in [1_usize, 2, 10] {
        tx.invalidate_all().await.unwrap();
        let got = tx
            .env()
            .records("test_web.multi")
            .unwrap()
            .browse(ids)
            .unwrap()
            .web_read(&mut tx, spec(u64::try_from(limit).unwrap()))
            .await
            .unwrap();
        let want: Vec<Value> = ids.iter().map(|id| multi(&db, *id, limit)).collect();
        assert_eq!(body(&got), body(&want), "limit {limit}");
        assert_eq!(got, want, "limit {limit}");
    }
    tx.rollback().await.unwrap();
}
