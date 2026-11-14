#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::serde_json::json;
use erp_sdk::{Command, ErrorKind, RawVals, Records, Transaction, Value};
use erp_testkit::assert_query_count;
use support::{dom, map, one};

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

async fn count(tx: &mut Transaction, sql: &str) -> i64 {
    one(tx, sql).await.unwrap().parse().unwrap()
}

/// A discussion with two messages (cascade) and a reference row (`set null`).
async fn discussion(tx: &mut Transaction) -> (Records, Records, Records) {
    let d = create(tx, "test_rel.discussion", &[&[("name", Value::str("d"))]]).await;
    let did = d.ids()[0];
    let msgs = create(
        tx,
        "test_rel.message",
        &[
            &[("discussion", Value::Int(did))],
            &[("discussion", Value::Int(did))],
        ],
    )
    .await;
    let x = create(
        tx,
        "test_rel.dref",
        &[&[("discussion_id", Value::Int(did))]],
    )
    .await;
    (d, msgs, x)
}

/// through foreign keys are invalidated, the database outcomes match)
#[tokio::test]
async fn spec_rel_013_cascade_and_set_null() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let (d, msgs, x) = discussion(&mut tx).await;
    tx.invalidate_all().await.unwrap();
    msgs.cells(&mut tx, "body").await.unwrap();
    x.cells(&mut tx, "discussion_id").await.unwrap();
    d.unlink(&mut tx).await.unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM test_rel_message").await,
        0
    );
    // x's field became empty, in cache and in the database
    assert_eq!(
        x.value(&mut tx, "discussion_id").await.unwrap(),
        Value::False
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        x.value(&mut tx, "discussion_id").await.unwrap(),
        Value::False
    );
    // the deleted messages are gone from the cache
    let err = msgs
        .at(0)
        .unwrap()
        .value(&mut tx, "body")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// would re-read it)
#[tokio::test]
async fn spec_rel_013_unrelated_caches_survive() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let unrelated = create(&mut tx, "test_rel.partner", &[&[("name", Value::str("p"))]]).await;
    let (d, _, _) = discussion(&mut tx).await;
    tx.invalidate_all().await.unwrap();
    unrelated.cells(&mut tx, "name").await.unwrap();
    d.unlink(&mut tx).await.unwrap();
    assert_query_count!(tx, <= 0, {
        assert_eq!(unrelated.value(&mut tx, "name").await.unwrap(), Value::str("p"));
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_013_restrict_refuses() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let d3 = create(
        &mut tx,
        "test_rel.discussion",
        &[&[("name", Value::str("d3"))]],
    )
    .await;
    create(
        &mut tx,
        "test_rel.dreq",
        &[&[("discussion_id", Value::Int(d3.ids()[0]))]],
    )
    .await;
    let err = tx
        .savepoint(async |tx| d3.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert!(
        err.message()
            .contains("Other records still point to the record you want to delete"),
        "{err}"
    );
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM test_rel_discussion WHERE id = {}",
                d3.ids()[0]
            )
        )
        .await,
        1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_036_delegation_and_deletion() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    // deleting a delegating record keeps its parent; deleting the parent cascades
    let boxes = create(
        &mut tx,
        "test_rel.box",
        &[&[("name", Value::str("u"))], &[("name", Value::str("v"))]],
    )
    .await;
    let units = boxes.related(&mut tx, "unit_id").await.unwrap();
    boxes.at(0).unwrap().unlink(&mut tx).await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM test_rel_unit WHERE id = {}",
                units.ids()[0]
            )
        )
        .await,
        1
    );
    units.at(1).unwrap().unlink(&mut tx).await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM test_rel_box WHERE id = {}",
                boxes.ids()[1]
            )
        )
        .await,
        0
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_029_unlink_basics() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cs = create(
        &mut tx,
        "test_rel.tag",
        &[&[("name", Value::str("c1"))], &[("name", Value::str("c2"))]],
    )
    .await;
    let (c1, c2) = (cs.at(0).unwrap(), cs.at(1).unwrap());
    assert_query_count!(tx, <= 0, { cs.browse_ids([]).unlink(&mut tx).await.unwrap() });
    c1.unlink(&mut tx).await.unwrap();
    c1.unlink(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin_as(7).await.unwrap();
    let read = tx
        .env()
        .records("test_rel.tag")
        .unwrap()
        .browse([c1.ids()[0], c2.ids()[0]])
        .unwrap()
        .read(&mut tx, &["name"])
        .await
        .unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0]["id"], Value::Int(c2.ids()[0]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// cached values are kept)
#[tokio::test]
async fn spec_crud_033_cascade_deletion_and_cache_visibility() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tx.env().records("test_rel.category").unwrap();
    let mut ids = Vec::new();
    let root = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("0"))]],
    )
    .await;
    ids.push(root.ids()[0]);
    for (parent, names) in [
        (0usize, ["1", "2", "3"]),
        (3, ["4", "5", "6"]),
        (6, ["7", "8", "9"]),
    ] {
        let rows: Vec<Vec<(&str, Value)>> = names
            .iter()
            .map(|n| {
                vec![
                    ("name", Value::str(*n)),
                    ("parent_id", Value::Int(ids[parent])),
                ]
            })
            .collect();
        let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
        ids.extend(create(&mut tx, "test_rel.category", &refs).await.ids());
    }
    tx.invalidate_all().await.unwrap();
    let all = cats.browse(ids.clone()).unwrap();
    all.cells(&mut tx, "name").await.unwrap();
    // a pending write on another model is flushed before the DELETE
    let p = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("before"))]],
    )
    .await;
    tx.flush().await.unwrap();
    p.set_value(&mut tx, "name", Value::str("after"))
        .await
        .unwrap();
    assert!(tx.is_dirty(&p, "name").unwrap());
    cats.browse([ids[6]])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert!(!tx.is_dirty(&p, "name").unwrap(), "flushed first");
    assert_eq!(
        one(
            &mut tx,
            &format!(
                "SELECT name FROM test_rel_partner WHERE id = {}",
                p.ids()[0]
            )
        )
        .await,
        Some("after".into())
    );
    let left = cats
        .search(&mut tx, &dom(json!([["id", "child_of", ids[0]]])))
        .await
        .unwrap();
    let mut left = left.ids();
    left.sort_unstable();
    assert_eq!(left, ids[..6].to_vec());
    let err = cats
        .browse([ids[7]])
        .unwrap()
        .value(&mut tx, "name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    // the surviving categories and the partner are still cached
    assert_query_count!(tx, <= 0, {
        cats.browse([ids[1]]).unwrap().value(&mut tx, "name").await.unwrap();
        p.value(&mut tx, "name").await.unwrap();
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_034_many2many_ondelete() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    // relation 1: B's mirror field restricts deleting A
    let a = create(&mut tx, "test_rel.model_a", &[&[("name", Value::str("a"))]]).await;
    let b = create(&mut tx, "test_rel.model_b", &[&[("name", Value::str("b"))]]).await;
    a.set_value(
        &mut tx,
        "a_restricted_b_ids",
        Value::list([Value::Int(b.ids()[0])]),
    )
    .await
    .unwrap();
    let err = tx
        .savepoint(async |tx| a.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    b.unlink(&mut tx).await.unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM rel_model_a_model_b_1").await,
        0
    );
    assert!(
        a.related(&mut tx, "a_restricted_b_ids")
            .await
            .unwrap()
            .is_empty()
    );
    // relation 2: the converse
    let a2 = create(
        &mut tx,
        "test_rel.model_a",
        &[&[("name", Value::str("a2"))]],
    )
    .await;
    let b2 = create(
        &mut tx,
        "test_rel.model_b",
        &[&[("name", Value::str("b2"))]],
    )
    .await;
    a2.set_value(
        &mut tx,
        "b_restricted_b_ids",
        Value::list([Value::Int(b2.ids()[0])]),
    )
    .await
    .unwrap();
    let err = tx
        .savepoint(async |tx| b2.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    a2.unlink(&mut tx).await.unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM rel_model_a_model_b_2").await,
        0
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn budget_delete_one_one2many_line() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let order = create(&mut tx, "test_rel.base", &[&[]]).await;
    let lines = create(
        &mut tx,
        "test_rel.line",
        &[
            &[
                ("base_id", Value::Int(order.ids()[0])),
                ("value", Value::Int(1)),
            ],
            &[
                ("base_id", Value::Int(order.ids()[0])),
                ("value", Value::Int(2)),
            ],
        ],
    )
    .await;
    // warm: the order's lines are known
    order.cells(&mut tx, "line_ids").await.unwrap();
    tx.flush().await.unwrap();
    assert_query_count!(tx, >= 1, <= 4, {
        order
            .write(
                &mut tx,
                RawVals::new().set_commands("line_ids", [Command::delete(lines.rec_ids()[0])]),
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
    });
    assert_eq!(
        order.related(&mut tx, "line_ids").await.unwrap().ids(),
        [lines.ids()[1]]
    );
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM test_rel_line").await,
        1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
