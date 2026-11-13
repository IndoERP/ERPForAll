//! x2many commands (REL §4.8): one2many and many2many in write and create mode, ordering, the
//! full-delete guard, cache coherence of inverse and sibling many2many fields, query counts.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::collections::BTreeSet;

use erp_sdk::{Command, ErrorKind, RawVals, RecId, Records, Transaction, Value};
use erp_testkit::assert_query_count;
use support::{map, one};

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

async fn write(tx: &mut Transaction, records: &Records, field: &str, commands: Vec<Command>) {
    records
        .write(tx, RawVals::new().set_commands(field.to_owned(), commands))
        .await
        .unwrap();
}

fn rid(i: i64) -> RecId {
    RecId::from_real(i).unwrap()
}

fn set(ids: Vec<i64>) -> BTreeSet<i64> {
    ids.into_iter().collect()
}

async fn ids(tx: &mut Transaction, records: &Records, field: &str) -> Vec<i64> {
    records.related(tx, field).await.unwrap().ids()
}

async fn count(tx: &mut Transaction, sql: &str) -> i64 {
    one(tx, sql).await.unwrap().parse().unwrap()
}

fn line(base: i64, value: i64) -> Vec<(&'static str, Value)> {
    vec![("base_id", Value::Int(base)), ("value", Value::Int(value))]
}

async fn lines(tx: &mut Transaction, rows: &[Vec<(&'static str, Value)>]) -> Records {
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(tx, "test_rel.line", &refs).await
}

fn vals(pairs: &[(&'static str, Value)]) -> RawVals {
    let mut v = RawVals::new();
    for (k, value) in pairs {
        let cell = match value {
            Value::Int(i) => erp_sdk::Cell::Int(*i as i32),
            Value::Bool(b) => erp_sdk::Cell::Bool(*b),
            Value::Str(s) => erp_sdk::Cell::Str(s.clone()),
            _ => erp_sdk::Cell::Null,
        };
        v.insert((*k).into(), cell);
    }
    v
}

#[tokio::test]
async fn spec_rel_043_command_lists_are_commands() {
    let h = support::rel_orm().await;
    let tx = h.begin().await.unwrap();
    let bases = tx.env().records("test_rel.base").unwrap();
    let tags = tx.env().records("test_rel.tag").unwrap();
    let value = Value::list([
        Value::list([
            Value::Int(6),
            Value::Int(0),
            Value::list([Value::Int(1), Value::Int(2)]),
        ]),
        Value::list([Value::Int(4), Value::Int(3), Value::Int(0)]),
        Value::list([Value::Int(5), Value::Int(0), Value::Int(0)]),
        Value::list([
            Value::Int(0),
            Value::Int(0),
            Value::Map(std::sync::Arc::new(support::vmap(&[(
                "name",
                Value::str("x"),
            )]))),
        ]),
    ]);
    let commands = bases.commands_from_value(&tags, "tag_ids", &value).unwrap();
    assert_eq!(commands[0], Command::set([rid(1), rid(2)]));
    assert_eq!(commands[1], Command::link(rid(3)));
    assert_eq!(commands[2], Command::clear());
    assert_eq!(commands[3].code(), 0);
    assert_eq!(commands[3].to_value(), value_at(&value, 3));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn x2many_third_element_and_unknown_code_are_refused() {
    let h = support::rel_orm().await;
    let tx = h.begin().await.unwrap();
    let bases = tx.env().records("test_rel.base").unwrap();
    let tags = tx.env().records("test_rel.tag").unwrap();
    for code in [2, 3, 4] {
        let err = bases
            .commands_from_value(
                &tags,
                "tag_ids",
                &Value::list([Value::list([
                    Value::Int(code),
                    Value::Int(1),
                    Value::Int(7),
                ])]),
            )
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!("Wrong value for test_rel.base.tag_ids: [{code}, 1, 7]")
        );
    }
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([Value::Int(9), Value::Int(0), Value::Int(0)])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Unknown command 9 for test_rel.base.tag_ids: [9, 0, 0]"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_086_accepted_lengths_of_x2many_command_tuples() {
    let h = support::rel_orm().await;
    let tx = h.begin().await.unwrap();
    let bases = tx.env().records("test_rel.base").unwrap();
    let tags = tx.env().records("test_rel.tag").unwrap();

    // Accepted short commands: [4, id], [3, id], [2, id], [5], [5, 0]
    let value = Value::list([
        Value::list([Value::Int(4), Value::Int(1)]),
        Value::list([Value::Int(3), Value::Int(2)]),
        Value::list([Value::Int(2), Value::Int(3)]),
        Value::list([Value::Int(5)]),
        Value::list([Value::Int(5), Value::Int(0)]),
        Value::list([Value::Int(5), Value::Int(0), Value::Int(0)]),
        Value::list([Value::Int(4), Value::Int(4), Value::Int(0)]),
    ]);
    let commands = bases.commands_from_value(&tags, "tag_ids", &value).unwrap();
    assert_eq!(commands[0], Command::link(rid(1)));
    assert_eq!(commands[1], Command::unlink(rid(2)));
    assert_eq!(commands[2], Command::delete(rid(3)));
    assert_eq!(commands[3], Command::clear());
    assert_eq!(commands[4], Command::clear());
    assert_eq!(commands[5], Command::clear());
    assert_eq!(commands[6], Command::link(rid(4)));

    // Rejected command lengths:
    // [4] (len 1, LINK requires id)
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([Value::Int(4)])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "Wrong value for test_rel.base.tag_ids: [4]");

    // [4, 1, 0, 0] (len 4, too long)
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([
                Value::Int(4),
                Value::Int(1),
                Value::Int(0),
                Value::Int(0),
            ])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_rel.base.tag_ids: [4, 1, 0, 0]"
    );

    // [2] (len 1, DELETE requires id)
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([Value::Int(2)])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "Wrong value for test_rel.base.tag_ids: [2]");

    // [3] (len 1, UNLINK requires id)
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([Value::Int(3)])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "Wrong value for test_rel.base.tag_ids: [3]");

    // [5, 0, 0, 0] (len 4, CLEAR max len 3)
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([
                Value::Int(5),
                Value::Int(0),
                Value::Int(0),
                Value::Int(0),
            ])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_rel.base.tag_ids: [5, 0, 0, 0]"
    );

    // [6, [1]] (len 2, SET requires 3 elements)
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([Value::Int(6), Value::list([Value::Int(1)])])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_rel.base.tag_ids: [6, [1]]"
    );

    // [0, 0] (len 2, CREATE requires 3 elements)
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([Value::Int(0), Value::Int(0)])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_rel.base.tag_ids: [0, 0]"
    );

    // [1, 1] (len 2, UPDATE requires 3 elements)
    let err = bases
        .commands_from_value(
            &tags,
            "tag_ids",
            &Value::list([Value::list([Value::Int(1), Value::Int(1)])]),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Wrong value for test_rel.base.tag_ids: [1, 1]"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

fn value_at(value: &Value, i: usize) -> Value {
    match value {
        Value::List(items) => items[i].clone(),
        _ => unreachable!(),
    }
}

/// divergent kind: `Validation`)
#[tokio::test]
async fn spec_rel_044_059_normalizing_x2many_values() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let t = create(
        &mut tx,
        "test_rel.tag",
        &[
            &[("name", Value::str("t1"))],
            &[("name", Value::str("t2"))],
            &[("name", Value::str("t3"))],
        ],
    )
    .await;
    let (t1, t2, t3) = (t.ids()[0], t.ids()[1], t.ids()[2]);
    let rec = create(
        &mut tx,
        "test_rel.base",
        &[&[("tag_ids", Value::list([Value::Int(t1), Value::Int(t2)]))]],
    )
    .await;
    // a tuple of ids (typed: a Vec<RecId> cell means SET)
    rec.write(
        &mut tx,
        RawVals::new().set_cell("tag_ids", erp_sdk::Cell::Ids(vec![rid(t3)].into())),
    )
    .await
    .unwrap();
    assert_eq!(ids(&mut tx, &rec, "tag_ids").await, [t3]);
    // a recordset
    rec.assign(&mut tx, "tag_ids", &t.browse_ids([rid(t1), rid(t2)]))
        .await
        .unwrap();
    assert_eq!(set(ids(&mut tx, &rec, "tag_ids").await), set(vec![t1, t2]));
    // a list of ids
    rec.set_value(&mut tx, "tag_ids", Value::list([Value::Int(t1)]))
        .await
        .unwrap();
    assert_eq!(ids(&mut tx, &rec, "tag_ids").await, [t1]);
    // False
    rec.set_value(&mut tx, "tag_ids", Value::False)
        .await
        .unwrap();
    assert!(ids(&mut tx, &rec, "tag_ids").await.is_empty());
    // []: nothing changes, no relation statement
    rec.set_value(&mut tx, "tag_ids", Value::list([Value::Int(t2)]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.capture_statements(true);
    rec.set_value(&mut tx, "tag_ids", Value::list([]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert!(
        tx.captured()
            .iter()
            .all(|s| !s.contains("test_rel_base_test_rel_tag_rel")),
        "{:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    assert_eq!(ids(&mut tx, &rec, "tag_ids").await, [t2]);
    // a string
    let err = rec
        .set_value(&mut tx, "tag_ids", Value::str("abc"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message()
            .starts_with("Wrong value for test_rel.base.tag_ids:"),
        "{err}"
    );
    // an unknown command code
    let err = rec
        .set_value(
            &mut tx,
            "tag_ids",
            Value::list([Value::list([Value::Int(7), Value::Int(0), Value::Int(0)])]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(ids(&mut tx, &rec, "tag_ids").await, [t2], "value unchanged");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_059_mix_of_real_and_new_records() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let real = create(&mut tx, "test_rel.base", &[&[]]).await;
    let bases = tx
        .env()
        .model::<erp_test_models::test_rel::BaseOrder>()
        .unwrap();
    let new = bases.new_record(&mut tx, bases.vals()).await.unwrap();
    let mixed = real.concat(new.as_records()).unwrap();
    let err = mixed
        .write(
            &mut tx,
            RawVals::new().set_commands("line_ids", [Command::Clear]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message()
            .ends_with("contains a mix of real and new records. It is not supported."),
        "{err}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_027_one2many_with_a_domain() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let d = create(
        &mut tx,
        "test_rel.discussion",
        &[&[("name", Value::str("d"))]],
    )
    .await;
    write(
        &mut tx,
        &d,
        "messages",
        (0..3)
            .map(|i| Command::create(vals(&[("body", Value::str(format!("m{i}")))])))
            .collect(),
    )
    .await;
    write(
        &mut tx,
        &d,
        "important_messages",
        vec![Command::create(vals(&[
            ("body", Value::str("important")),
            ("important", Value::Bool(true)),
        ]))],
    )
    .await;
    assert_eq!(ids(&mut tx, &d, "messages").await.len(), 4);
    assert_eq!(ids(&mut tx, &d, "important_messages").await.len(), 1);
    // a line that does not match the field's domain cannot be linked through it
    let err = tx
        .savepoint(async |tx| {
            d.write(
                tx,
                RawVals::new().set_commands(
                    "important_messages",
                    [Command::create(vals(&[
                        ("body", Value::str("plain")),
                        ("important", Value::Bool(false)),
                    ]))],
                ),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Cannot link inaccessible records in test_rel.discussion.important_messages \
         ([('important', '=', True)])"
    );
    // CLEAR through a field with a domain only removes the matching lines (cascade: deleted)
    write(&mut tx, &d, "very_important_messages", vec![Command::Clear]).await;
    tx.invalidate_all().await.unwrap();
    assert_eq!(ids(&mut tx, &d, "messages").await.len(), 3);
    assert!(ids(&mut tx, &d, "important_messages").await.is_empty());
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM test_rel_message").await,
        3,
        "the important message was deleted, the others untouched"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_035_many2many_inverse_cache_coherence() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ds = create(&mut tx, "test_rel.discussion", &[&[], &[]]).await;
    let cs = create(&mut tx, "test_rel.dcategory", &[&[], &[]]).await;
    let (d1, d2) = (ds.at(0).unwrap(), ds.at(1).unwrap());
    let (c1, c2) = (cs.ids()[0], cs.ids()[1]);
    tx.invalidate_all().await.unwrap();
    d1.cells(&mut tx, "categories").await.unwrap();
    d2.cells(&mut tx, "categories").await.unwrap();
    write(
        &mut tx,
        &cs,
        "discussions",
        vec![Command::link(rid(d1.ids()[0]))],
    )
    .await;
    let order = assert_query_count!(tx, <= 0, { ids(&mut tx, &d1, "categories").await });
    assert_eq!(
        order,
        [c1, c2],
        "appended in the order of the written records"
    );
    let reversed = cs.browse_ids([rid(c2), rid(c1)]);
    write(
        &mut tx,
        &reversed,
        "discussions",
        vec![Command::link(rid(d2.ids()[0]))],
    )
    .await;
    let order = assert_query_count!(tx, <= 0, { ids(&mut tx, &d2, "categories").await });
    assert_eq!(order, [c2, c1]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_036_many2many_sibling_cache_coherence() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let names = ["A", "B", "C", "Z", "AA", "AAA", "ZZ"];
    let rows: Vec<Vec<(&str, Value)>> = names
        .iter()
        .map(|n| {
            let mut r = vec![("name", Value::str(*n))];
            if matches!(*n, "B" | "ZZ") {
                r.push(("active", Value::Bool(false)));
            }
            r
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let recs = create(&mut tx, "test_rel.active", &refs).await;
    let id = |n: &str| recs.ids()[names.iter().position(|x| *x == n).unwrap()];
    let parent = create(
        &mut tx,
        "test_rel.active",
        &[&[(
            "all_relatives_ids",
            Value::list(["A", "B", "C", "Z"].map(|n| Value::Int(id(n)))),
        )]],
    )
    .await;
    // the sibling values are in cache
    for f in [
        "relatives_ids",
        "all_relatives_ids",
        "a_relatives_ids",
        "z_relatives_ids",
    ] {
        parent.cells(&mut tx, f).await.unwrap();
    }
    write(
        &mut tx,
        &parent,
        "all_relatives_ids",
        vec![Command::set(["A", "B", "C", "AA"].map(|n| rid(id(n))))],
    )
    .await;
    assert_eq!(
        set(ids(&mut tx, &parent, "a_relatives_ids").await),
        set(vec![id("A"), id("AA")])
    );
    assert!(ids(&mut tx, &parent, "z_relatives_ids").await.is_empty());
    write(
        &mut tx,
        &parent,
        "a_relatives_ids",
        vec![Command::set(["AA", "AAA"].map(|n| rid(id(n))))],
    )
    .await;
    write(
        &mut tx,
        &parent,
        "z_relatives_ids",
        vec![Command::set([rid(id("Z"))])],
    )
    .await;
    write(
        &mut tx,
        &parent,
        "z_relatives_ids",
        vec![Command::link(rid(id("ZZ")))],
    )
    .await;
    let expect_all = set(["AA", "AAA", "B", "C", "Z", "ZZ"].map(id).to_vec());
    let expect_active = set(["AA", "AAA", "C", "Z"].map(id).to_vec());
    assert_eq!(
        set(ids(&mut tx, &parent, "all_relatives_ids").await),
        expect_all
    );
    assert_eq!(
        set(ids(&mut tx, &parent, "relatives_ids").await),
        expect_active
    );
    // the same from the database
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        set(ids(&mut tx, &parent, "all_relatives_ids").await),
        expect_all
    );
    assert_eq!(
        set(ids(&mut tx, &parent, "relatives_ids").await),
        expect_active
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// LINK, 3/2 for CLEAR; the no-op writes send no INSERT and no DELETE at all; pairs are written in
/// `(column1, column2)` order)
#[tokio::test]
async fn spec_rel_038_many2many_write_sql_and_no_op() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..12)
        .map(|i| vec![("name", Value::str(format!("t{i}")))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let tags = create(&mut tx, "test_rel.tag", &refs).await;
    let rec2 = create(&mut tx, "test_rel.base", &[&[]]).await;
    tx.invalidate_all().await.unwrap();
    let tag_ids: Vec<RecId> = tags.rec_ids().to_vec();
    let run = async |tx: &mut Transaction, commands: Vec<Command>| -> Vec<String> {
        tx.capture_statements(true);
        write(tx, &rec2, "tag_ids", commands).await;
        tx.flush().await.unwrap();
        let out = tx.captured().to_vec();
        tx.capture_statements(false);
        out
    };
    let first = assert_query_count!(tx, >= 1, <= 3, {
        run(&mut tx, vec![Command::set(tag_ids.clone())]).await
    });
    let inserts: Vec<&String> = first
        .iter()
        .filter(|s| s.starts_with("INSERT INTO \"test_rel_base_test_rel_tag_rel\""))
        .collect();
    assert_eq!(inserts.len(), 1, "{first:?}");
    support::assert_sql(
        inserts[0],
        "INSERT INTO \"test_rel_base_test_rel_tag_rel\" (\"test_rel_base_id\", \
         \"test_rel_tag_id\") SELECT * FROM unnest($1::int8[], $2::int8[]) ORDER BY 1, 2 ON \
         CONFLICT DO NOTHING",
    );
    let no_dml = |statements: &[String]| {
        statements
            .iter()
            .all(|s| !s.starts_with("INSERT") && !s.starts_with("DELETE") && !s.contains("_rel\""))
    };
    let again = assert_query_count!(tx, <= 2, {
        run(&mut tx, vec![Command::set(tag_ids.clone())]).await
    });
    assert!(no_dml(&again), "{again:?}");
    let link =
        assert_query_count!(tx, <= 2, { run(&mut tx, vec![Command::link(tag_ids[0])]).await });
    assert!(no_dml(&link), "{link:?}");
    let clear = assert_query_count!(tx, >= 1, <= 3, { run(&mut tx, vec![Command::Clear]).await });
    let deletes: Vec<&String> = clear.iter().filter(|s| s.starts_with("DELETE")).collect();
    assert_eq!(deletes.len(), 1, "{clear:?}");
    support::assert_sql(
        deletes[0],
        "DELETE FROM \"test_rel_base_test_rel_tag_rel\" WHERE (\"test_rel_base_id\", \
         \"test_rel_tag_id\") IN (SELECT * FROM unnest($1::int8[], $2::int8[]) ORDER BY 1, 2)",
    );
    let again = assert_query_count!(tx, <= 2, { run(&mut tx, vec![Command::Clear]).await });
    assert!(no_dml(&again), "{again:?}");
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*)::text FROM test_rel_base_test_rel_tag_rel"
        )
        .await,
        0
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_039_many2many_writes_keep_archived_links() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ab = create(
        &mut tx,
        "test_rel.active",
        &[
            &[("name", Value::str("A"))],
            &[("name", Value::str("B")), ("active", Value::Bool(false))],
        ],
    )
    .await;
    let parent = create(
        &mut tx,
        "test_rel.active",
        &[&[(
            "relatives_ids",
            Value::list(ab.ids().into_iter().map(Value::Int)),
        )]],
    )
    .await;
    parent
        .assign(&mut tx, "relatives_ids", &ab.at(0).unwrap())
        .await
        .unwrap();
    let all = parent.with_context([("active_test", Value::Bool(false))]);
    assert_eq!(
        set(ids(&mut tx, &all, "relatives_ids").await),
        set(ab.ids())
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        set(ids(&mut tx, &all, "relatives_ids").await),
        set(ab.ids())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_045_one2many_create_in_write_mode() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = create(&mut tx, "test_rel.base", &[&[], &[]]).await;
    let (o1, o2) = (orders.at(0).unwrap(), orders.at(1).unwrap());
    assert_query_count!(tx, >= 1, <= 3, {
        write(&mut tx, &orders, "line_ids", vec![Command::create(vals(&[("value", Value::Int(1))]))]).await;
        tx.flush().await.unwrap();
    });
    for o in [&o1, &o2] {
        let l = o.related(&mut tx, "line_ids").await.unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l.value(&mut tx, "value").await.unwrap(), Value::Int(1));
        assert_eq!(
            l.value(&mut tx, "base_id").await.unwrap(),
            Value::Int(o.ids()[0])
        );
    }
    tx.capture_statements(true);
    assert_query_count!(tx, >= 1, <= 4, {
        write(
            &mut tx,
            &o1,
            "line_ids",
            (2..=12)
                .map(|v| Command::create(vals(&[("value", Value::Int(v))])))
                .collect(),
        )
        .await;
        tx.flush().await.unwrap();
    });
    let inserts = tx
        .captured()
        .iter()
        .filter(|s| s.contains("INSERT INTO \"test_rel_line\""))
        .count();
    assert_eq!(inserts, 1, "one INSERT for the 11 lines");
    tx.capture_statements(false);
    assert_eq!(ids(&mut tx, &o1, "line_ids").await.len(), 12);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_046_one2many_update_and_delete_only_own_lines() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = create(&mut tx, "test_rel.base", &[&[], &[]]).await;
    let (o1, o2) = (orders.at(0).unwrap(), orders.at(1).unwrap());
    let ls = lines(&mut tx, &[line(o1.ids()[0], 1), line(o2.ids()[0], 2)]).await;
    let (l1, l2) = (ls.at(0).unwrap(), ls.at(1).unwrap());
    for command in [
        Command::update(rid(l2.ids()[0]), vals(&[("value", Value::Int(42))])),
        Command::delete(rid(l2.ids()[0])),
    ] {
        let err = tx
            .savepoint(async |tx| {
                o1.write(tx, RawVals::new().set_commands("line_ids", [command]))
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    }
    assert_eq!(l2.value(&mut tx, "value").await.unwrap(), Value::Int(2));
    assert_eq!(ids(&mut tx, &o2, "line_ids").await, [l2.ids()[0]]);
    // the record's own line
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, <= 2, {
        write(
            &mut tx,
            &o1,
            "line_ids",
            vec![Command::update(
                rid(l1.ids()[0]),
                vals(&[("value", Value::Int(42))]),
            )],
        )
        .await;
    });
    assert_eq!(l1.value(&mut tx, "value").await.unwrap(), Value::Int(42));
    write(
        &mut tx,
        &o1,
        "line_ids",
        vec![Command::delete(rid(l1.ids()[0]))],
    )
    .await;
    assert!(ids(&mut tx, &o1, "line_ids").await.is_empty());
    tx.flush().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM test_rel_line WHERE id = {}",
                l1.ids()[0]
            )
        )
        .await,
        0
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_047_one2many_unlink_follows_inverse_ondelete() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let order = create(&mut tx, "test_rel.base", &[&[]]).await;
    let l = lines(&mut tx, &[line(order.ids()[0], 1)]).await;
    write(
        &mut tx,
        &order,
        "line_ids",
        vec![Command::unlink(rid(l.ids()[0]))],
    )
    .await;
    tx.flush().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM test_rel_line WHERE id = {}",
                l.ids()[0]
            )
        )
        .await,
        0,
        "base_id cascades: the line is deleted"
    );
    let parent = create(&mut tx, "test_rel.active", &[&[("name", Value::str("P"))]]).await;
    let c = create(
        &mut tx,
        "test_rel.active",
        &[&[
            ("name", Value::str("c")),
            ("parent_id", Value::Int(parent.ids()[0])),
        ]],
    )
    .await;
    write(
        &mut tx,
        &parent,
        "children_ids",
        vec![Command::unlink(rid(c.ids()[0]))],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    assert_eq!(c.value(&mut tx, "parent_id").await.unwrap(), Value::False);
    assert!(ids(&mut tx, &parent, "children_ids").await.is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_048_one2many_link_reparents_and_validates() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let o = create(&mut tx, "test_rel.base", &[&[]]).await;
    let oid = o.ids()[0];
    let ls = lines(&mut tx, &[line(oid, 0), line(oid, 1001), line(oid, 1002)]).await;
    let (l0, l1, l2) = (ls.rec_ids()[0], ls.rec_ids()[1], ls.rec_ids()[2]);
    // deleting l1 also deletes l2 (business override); l2 was already linked: not re-checked
    write(
        &mut tx,
        &o,
        "line_ids",
        vec![Command::link(l0), Command::delete(l1), Command::link(l2)],
    )
    .await;
    assert_eq!(ids(&mut tx, &o, "line_ids").await, [l0.get()]);
    // linking a line that no longer exists fails
    let err = tx
        .savepoint(async |tx| {
            o.write(
                tx,
                RawVals::new().set_commands("line_ids", [Command::link(l0), Command::link(l1)]),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing, "{err}");
    // moving 12 lines from rec1 to rec2
    let recs = create(&mut tx, "test_rel.base", &[&[], &[]]).await;
    let (rec1, rec2) = (recs.at(0).unwrap(), recs.at(1).unwrap());
    let rows: Vec<_> = (0..12).map(|v| line(rec1.ids()[0], v)).collect();
    let moved = lines(&mut tx, &rows).await;
    write(
        &mut tx,
        &rec2,
        "line_ids",
        moved.rec_ids().iter().map(|l| Command::link(*l)).collect(),
    )
    .await;
    assert_eq!(set(ids(&mut tx, &rec2, "line_ids").await), set(moved.ids()));
    assert!(ids(&mut tx, &rec1, "line_ids").await.is_empty());
    tx.invalidate_all().await.unwrap();
    assert_eq!(set(ids(&mut tx, &rec2, "line_ids").await), set(moved.ids()));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_049_one2many_set_and_clear_in_write_mode() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "test_rel.active", &[&[("name", Value::str("P"))]]).await;
    let pid = parent.ids()[0];
    let child = |name: &str, active: bool| {
        vec![
            ("name", Value::str(name)),
            ("parent_id", Value::Int(pid)),
            ("active", Value::Bool(active)),
        ]
    };
    let rows = [
        child("c1", true),
        child("c2", false),
        child("c3", true),
        child("c4", false),
    ];
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let cs = create(&mut tx, "test_rel.active", &refs).await;
    let [c1, c2, c3, c4] = [0, 1, 2, 3].map(|i| cs.ids()[i]);
    write(
        &mut tx,
        &parent,
        "children_ids",
        vec![Command::set([rid(c1)])],
    )
    .await;
    assert_eq!(ids(&mut tx, &parent, "children_ids").await, [c1]);
    assert_eq!(
        set(ids(&mut tx, &parent, "all_children_ids").await),
        set(vec![c1, c2, c4])
    );
    assert_eq!(
        cs.at(2).unwrap().value(&mut tx, "parent_id").await.unwrap(),
        Value::False,
        "c3 detached"
    );
    let _ = c3;
    let all = parent.with_context([("active_test", Value::Bool(false))]);
    write(&mut tx, &all, "children_ids", vec![Command::set([rid(c1)])]).await;
    assert_eq!(ids(&mut tx, &parent, "all_children_ids").await, [c1]);
    write(
        &mut tx,
        &parent,
        "children_ids",
        vec![Command::set([rid(c4)])],
    )
    .await;
    assert_eq!(ids(&mut tx, &parent, "all_children_ids").await, [c4]);
    // several records written: SET attaches to the last one only
    let ps = create(&mut tx, "test_rel.active", &[&[], &[]]).await;
    write(&mut tx, &ps, "children_ids", vec![Command::set([rid(c1)])]).await;
    assert_eq!(
        cs.at(0).unwrap().value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(ps.ids()[1])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_050_one2many_full_delete_guard() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = create(&mut tx, "test_rel.base", &[&[], &[]]).await;
    let (o, other) = (orders.at(0).unwrap(), orders.at(1).unwrap());
    let ls = lines(
        &mut tx,
        &[
            line(o.ids()[0], 1),
            line(o.ids()[0], 2),
            line(other.ids()[0], 3),
        ],
    )
    .await;
    let (l1, l2, l3) = (ls.ids()[0], ls.ids()[1], ls.ids()[2]);
    // (a) a CREATE before a CLEAR: nothing is removed
    write(
        &mut tx,
        &o,
        "line_ids",
        vec![
            Command::create(vals(&[("value", Value::Int(9))])),
            Command::Clear,
        ],
    )
    .await;
    let after = ids(&mut tx, &o, "line_ids").await;
    assert_eq!(after.len(), 3);
    assert!(after.contains(&l1) && after.contains(&l2));
    // (b) a LINK before a SET: SET only links
    write(
        &mut tx,
        &o,
        "line_ids",
        vec![Command::link(rid(l3)), Command::set([rid(l1)])],
    )
    .await;
    let after = ids(&mut tx, &o, "line_ids").await;
    assert!(
        after.contains(&l1) && after.contains(&l2) && after.contains(&l3),
        "{after:?}"
    );
    // (c) CLEAR then CREATE: the old lines are deleted
    let fresh = create(&mut tx, "test_rel.base", &[&[]]).await;
    let fl = lines(&mut tx, &[line(fresh.ids()[0], 1), line(fresh.ids()[0], 2)]).await;
    write(
        &mut tx,
        &fresh,
        "line_ids",
        vec![
            Command::Clear,
            Command::create(vals(&[("value", Value::Int(9))])),
        ],
    )
    .await;
    let after = ids(&mut tx, &fresh, "line_ids").await;
    assert_eq!(after.len(), 1);
    assert!(!after.contains(&fl.ids()[0]) && !after.contains(&fl.ids()[1]));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_051_one2many_deletes_before_creates() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let order = create(&mut tx, "test_rel.base", &[&[]]).await;
    let oid = order.ids()[0];
    let rows: Vec<_> = (0..12).map(|v| line(oid, v)).collect();
    lines(&mut tx, &rows).await;
    let mut commands = vec![Command::Clear];
    commands.extend((0..6).map(|v| Command::create(vals(&[("value", Value::Int(v))]))));
    write(&mut tx, &order, "line_ids", commands).await;
    tx.flush().await.unwrap();
    assert_eq!(ids(&mut tx, &order, "line_ids").await.len(), 6);
    let zero = tx
        .env()
        .records("test_rel.line")
        .unwrap()
        .search(
            &mut tx,
            &support::dom(erp_sdk::serde_json::json!([
                ["base_id", "=", oid],
                ["value", "=", 0]
            ])),
        )
        .await
        .unwrap();
    write(
        &mut tx,
        &order,
        "line_ids",
        vec![
            Command::create(vals(&[("value", Value::Int(0))])),
            Command::delete(zero.rec_ids()[0]),
        ],
    )
    .await;
    tx.flush().await.unwrap();
    let after = ids(&mut tx, &order, "line_ids").await;
    assert_eq!(after.len(), 6);
    assert!(!after.contains(&zero.ids()[0]), "the line was replaced");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_052_one2many_commands_on_create() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let o0 = create(&mut tx, "test_rel.base", &[&[]]).await;
    let ls = lines(&mut tx, &[line(o0.ids()[0], 1), line(o0.ids()[0], 2)]).await;
    let (l1, l2) = (ls.rec_ids()[0], ls.rec_ids()[1]);
    let bases = tx.env().records("test_rel.base").unwrap();
    let first = assert_query_count!(tx, >= 1, <= 2, {
        let r = bases
            .create(
                &mut tx,
                vec![RawVals::new().set_commands("line_ids", [Command::set([l1])])],
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        r
    });
    assert_eq!(ids(&mut tx, &first, "line_ids").await, [l1.get()]);
    assert_eq!(
        ids(&mut tx, &o0, "line_ids").await,
        [l2.get()],
        "moved, not deleted"
    );
    let second = assert_query_count!(tx, >= 1, <= 3, {
        let r = bases
            .create(
                &mut tx,
                vec![RawVals::new().set_commands("line_ids", [Command::set([l1, l2])])],
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        r
    });
    assert_eq!(
        set(ids(&mut tx, &second, "line_ids").await),
        set(vec![l1.get(), l2.get()])
    );
    assert!(ids(&mut tx, &first, "line_ids").await.is_empty());
    tx.invalidate_all().await.unwrap();
    assert!(ids(&mut tx, &first, "line_ids").await.is_empty());
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM test_rel_line").await,
        2,
        "nothing deleted"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_053_many2many_create_links_one_record_to_all() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ls = create(&mut tx, "test_rel.multi_line", &[&[], &[]]).await;
    write(
        &mut tx,
        &ls,
        "tags",
        vec![Command::create(vals(&[("name", Value::str("x"))]))],
    )
    .await;
    let a = ids(&mut tx, &ls.at(0).unwrap(), "tags").await;
    let b = ids(&mut tx, &ls.at(1).unwrap(), "tags").await;
    assert_eq!(a.len(), 1);
    assert_eq!(a, b, "one tag linked to both lines");
    let lines_model = tx.env().records("test_rel.multi_line").unwrap();
    tx.capture_statements(true);
    let created = lines_model
        .create(
            &mut tx,
            (0..3)
                .map(|_| {
                    RawVals::new().set_commands(
                        "tags",
                        (0..3).map(|i| {
                            Command::create(vals(&[("name", Value::str(format!("t{i}")))]))
                        }),
                    )
                })
                .collect(),
        )
        .await
        .unwrap();
    let tag_inserts = tx
        .captured()
        .iter()
        .filter(|s| s.contains("INSERT INTO \"test_rel_multi_tag\""))
        .count();
    tx.capture_statements(false);
    assert_eq!(tag_inserts, 1, "batched across the parent records");
    let mut all = BTreeSet::new();
    for l in created.iter() {
        let tags = ids(&mut tx, &l, "tags").await;
        assert_eq!(tags.len(), 3);
        all.extend(tags);
    }
    assert_eq!(all.len(), 9);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_054_many2many_commands() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..12)
        .map(|i| vec![("name", Value::str(format!("t{i}")))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let tags = create(&mut tx, "test_rel.tag", &refs).await;
    let all: Vec<RecId> = tags.rec_ids().to_vec();
    let recs = create(
        &mut tx,
        "test_rel.base",
        &[
            &[(
                "tag_ids",
                Value::list(tags.ids().into_iter().map(Value::Int)),
            )],
            &[],
        ],
    )
    .await;
    let (rec1, rec2) = (recs.at(0).unwrap(), recs.at(1).unwrap());
    write(
        &mut tx,
        &rec1,
        "tag_ids",
        all.iter()
            .map(|t| Command::update(*t, vals(&[("name", Value::str("updated"))])))
            .collect(),
    )
    .await;
    assert!(
        tags.values(&mut tx, "name")
            .await
            .unwrap()
            .iter()
            .all(|n| *n == Value::str("updated"))
    );
    assert_eq!(ids(&mut tx, &rec1, "tag_ids").await.len(), 12);
    write(&mut tx, &rec1, "tag_ids", vec![Command::delete(all[0])]).await;
    assert_eq!(ids(&mut tx, &rec1, "tag_ids").await.len(), 11);
    tx.flush().await.unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM test_rel_tag").await,
        11
    );
    write(
        &mut tx,
        &rec1,
        "tag_ids",
        all[1..].iter().map(|t| Command::unlink(*t)).collect(),
    )
    .await;
    assert!(ids(&mut tx, &rec1, "tag_ids").await.is_empty());
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM test_rel_tag").await,
        11
    );
    write(
        &mut tx,
        &rec2,
        "tag_ids",
        all[1..].iter().map(|t| Command::link(*t)).collect(),
    )
    .await;
    assert_eq!(ids(&mut tx, &rec2, "tag_ids").await.len(), 11);
    write(&mut tx, &rec2, "tag_ids", vec![Command::Clear]).await;
    assert!(ids(&mut tx, &rec2, "tag_ids").await.is_empty());
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM test_rel_tag").await,
        11
    );
    write(
        &mut tx,
        &rec2,
        "tag_ids",
        vec![Command::set(all[1..4].to_vec())],
    )
    .await;
    assert_eq!(
        set(ids(&mut tx, &rec2, "tag_ids").await),
        set(all[1..4].iter().map(|r| r.get()).collect())
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        set(ids(&mut tx, &rec2, "tag_ids").await),
        set(all[1..4].iter().map(|r| r.get()).collect())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_055_many2many_ordering_in_one_list() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let t1 = create(&mut tx, "test_rel.tag", &[&[("name", Value::str("t1"))]]).await;
    let rec = create(
        &mut tx,
        "test_rel.base",
        &[&[("tag_ids", Value::list([Value::Int(t1.ids()[0])]))]],
    )
    .await;
    write(
        &mut tx,
        &rec,
        "tag_ids",
        vec![
            Command::create(vals(&[("name", Value::str("n"))])),
            Command::set([t1.rec_ids()[0]]),
        ],
    )
    .await;
    assert_eq!(ids(&mut tx, &rec, "tag_ids").await, t1.ids());
    tx.flush().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*)::text FROM test_rel_tag WHERE name = 'n'"
        )
        .await,
        1
    );
    write(
        &mut tx,
        &rec,
        "tag_ids",
        vec![
            Command::delete(t1.rec_ids()[0]),
            Command::create(vals(&[("name", Value::str("m"))])),
        ],
    )
    .await;
    tx.flush().await.unwrap();
    let linked = rec.related(&mut tx, "tag_ids").await.unwrap();
    assert_eq!(
        linked.values(&mut tx, "name").await.unwrap(),
        [Value::str("m")]
    );
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM test_rel_tag WHERE id = {}",
                t1.ids()[0]
            )
        )
        .await,
        0
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_063_x2many_written_after_other_fields() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let order = create(&mut tx, "test_rel.base", &[&[]]).await;
    // the line's create override reads its order's value: the value is written first
    let mut map = std::collections::BTreeMap::new();
    map.insert(
        "line_ids".to_owned(),
        Value::list([Value::list([
            Value::Int(0),
            Value::Int(0),
            Value::Map(Default::default()),
        ])]),
    );
    map.insert("value".to_owned(), Value::Int(10));
    order.write_values(&mut tx, &map).await.unwrap();
    let l = order.related(&mut tx, "line_ids").await.unwrap();
    assert_eq!(l.value(&mut tx, "value").await.unwrap(), Value::Int(10));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_064_create_with_x2many_batching() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let bases = tx.env().records("test_rel.base").unwrap();
    bases.create(&mut tx, vec![RawVals::new()]).await.unwrap();
    let r = assert_query_count!(tx, >= 1, <= 1, {
        bases.create(&mut tx, vec![RawVals::new()]).await.unwrap()
    });
    assert_query_count!(tx, <= 0, {
        assert!(ids(&mut tx, &r, "line_ids").await.is_empty());
        assert!(ids(&mut tx, &r, "tag_ids").await.is_empty());
    });
    assert_query_count!(tx, >= 1, <= 1, {
        bases
            .create(
                &mut tx,
                vec![RawVals::new().set_commands("line_ids", []).set_commands("tag_ids", [])],
            )
            .await
            .unwrap()
    });
    let r = assert_query_count!(tx, >= 3, <= 4, {
        bases
            .create(
                &mut tx,
                vec![RawVals::new()
                    .set_commands("line_ids", [Command::create(vals(&[("value", Value::Int(1))]))])
                    .set_commands("tag_ids", [Command::create(vals(&[("name", Value::str("t"))]))])],
            )
            .await
            .unwrap()
    });
    assert_query_count!(tx, <= 0, {
        assert_eq!(ids(&mut tx, &r, "line_ids").await.len(), 1);
        assert_eq!(ids(&mut tx, &r, "tag_ids").await.len(), 1);
    });
    // 10 lines: one INSERT for the order, one INSERT for the lines
    create_with_ten_lines(&mut tx).await;
    tx.capture_statements(true);
    let r = assert_query_count!(tx, >= 2, <= 2, { create_with_ten_lines(&mut tx).await });
    assert!(
        tx.captured()
            .iter()
            .all(|s| s.starts_with("WITH \"__r\" AS MATERIALIZED")),
        "no id reservation: {:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    let lines = ids(&mut tx, &r, "line_ids").await;
    tx.invalidate_all().await.unwrap();
    assert_eq!(ids(&mut tx, &r, "line_ids").await, lines);
    assert!(
        lines.windows(2).all(|w| w[0] < w[1]),
        "ids follow the input order"
    );
    let values = tx
        .env()
        .records("test_rel.line")
        .unwrap()
        .browse(lines.iter().copied())
        .unwrap()
        .values(&mut tx, "value")
        .await
        .unwrap();
    assert_eq!(values, (0..10).map(Value::Int).collect::<Vec<_>>());
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn create_with_ten_lines(tx: &mut Transaction) -> Records {
    let bases = tx.env().records("test_rel.base").unwrap();
    bases
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", erp_sdk::Cell::Str("X".into()))
                    .set_commands(
                        "line_ids",
                        (0..10).map(|v| Command::create(vals(&[("value", Value::Int(v))]))),
                    ),
            ],
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn spec_crud_015_x2many_written_with_a_clean_context() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx
        .env()
        .records("test_rel.partner")
        .unwrap()
        .with_context([("default_name", Value::str("Ctx"))]);
    let child = |email: &str| Command::create(vals(&[("email", Value::str(email))]));
    let created = partners
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", erp_sdk::Cell::Str("Foo".into()))
                    .set_commands("child_ids", (0..4).map(|i| child(&format!("f{i}")))),
                RawVals::new()
                    .set_cell("name", erp_sdk::Cell::Str("Bar".into()))
                    .set_commands("child_ids", (0..2).map(|i| child(&format!("b{i}")))),
            ],
        )
        .await
        .unwrap();
    let foo = created
        .at(0)
        .unwrap()
        .related(&mut tx, "child_ids")
        .await
        .unwrap();
    let bar = created
        .at(1)
        .unwrap()
        .related(&mut tx, "child_ids")
        .await
        .unwrap();
    assert_eq!((foo.len(), bar.len()), (4, 2));
    let names = foo.values(&mut tx, "name").await.unwrap();
    assert!(names.iter().all(|n| *n == Value::False), "{names:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}
