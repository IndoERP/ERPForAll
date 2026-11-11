//! S7 delegation acceptance: events follow physical ownership and actual database changes.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::{Cell, OnDelete};
use erp_orm::{BeginOptions, OperationIdentity, RawVals, SUPERUSER_ID, Transaction};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_outbox_delegation",
    depends: &["base"],
    exact_only: false,
};
const PARENT: &str = "test.outbox.delegation.parent";
const CHILD: &str = "test.outbox.delegation.child";

async fn begin(h: &Harness) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}
async fn harness(parent: bool, child: bool) -> Harness {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new(ADDON.name, PARENT)
                .table("outbox_delegation_parent")
                .eventful(parent)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new(ADDON.name, CHILD)
                .table("outbox_delegation_child")
                .eventful(child)
                .inherits(&[(PARENT, "parent_id")])
                .fields(
                    const {
                        &[
                            FieldDecl::many2one("parent_id", PARENT)
                                .required(true)
                                .ondelete(OnDelete::Cascade),
                            FieldDecl::char("note"),
                        ]
                    },
                ),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.commit().await.unwrap();
    h
}
async fn seed(h: &Harness, shared: bool) -> (Vec<i64>, Vec<i64>) {
    let mut tx = begin(h).await;
    let parents = tx
        .env()
        .records(PARENT)
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("original".into())); 2],
        )
        .await
        .unwrap()
        .ids();
    let links = [parents[0], if shared { parents[0] } else { parents[1] }];
    let children = tx
        .env()
        .records(CHILD)
        .unwrap()
        .create(
            &mut tx,
            links
                .into_iter()
                .map(|id| {
                    RawVals::new()
                        .set_cell("parent_id", Cell::Id(id))
                        .set_cell("note", Cell::Str("initial".into()))
                })
                .collect(),
        )
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    (parents, children)
}
#[derive(Debug, PartialEq, Eq)]
struct Event {
    model: String,
    ids: Vec<i64>,
    fields: Vec<String>,
}
async fn events(h: &Harness, op: &str) -> Vec<Event> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx.query("SELECT model, record_ids, field_names FROM erp_event.ir_event_outbox WHERE operation_id::text = $1 AND kind = 'write' ORDER BY id", &[&op]).await.unwrap();
    let result = rows
        .iter()
        .map(|row| Event {
            model: row.get(0),
            ids: serde_json::from_value(row.get(1)).unwrap(),
            fields: row.get(2),
        })
        .collect();
    tx.rollback().await.unwrap();
    result
}
fn event(model: &str, ids: &[i64], fields: &[&str]) -> Event {
    Event {
        model: model.into(),
        ids: ids.to_vec(),
        fields: fields.iter().map(|s| (*s).into()).collect(),
    }
}
fn inserts(tx: &Transaction) -> usize {
    tx.captured()
        .iter()
        .filter(|s| s.starts_with("INSERT INTO \"erp_event\".\"ir_event_outbox\""))
        .count()
}
async fn state(h: &Harness) -> (Vec<(i64, String)>, Vec<(i64, i64, String)>) {
    let mut tx = h.begin().await.unwrap();
    let parents = tx
        .query(
            "SELECT id, name FROM outbox_delegation_parent ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let children = tx
        .query(
            "SELECT id, parent_id, note FROM outbox_delegation_child ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    tx.rollback().await.unwrap();
    (parents, children)
}

#[tokio::test]
async fn mixed_writes_capture_each_opted_in_physical_owner() {
    for (p, c) in [(true, true), (true, false), (false, true), (false, false)] {
        let h = harness(p, c).await;
        let (parents, children) = seed(&h, false).await;
        let mut tx = begin(&h).await;
        let op = tx
            .env()
            .operation_identity()
            .unwrap()
            .operation()
            .to_string();
        tx.capture_statements(true);
        tx.env()
            .records(CHILD)
            .unwrap()
            .browse([children[0]])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("name", Cell::Str("parent changed".into()))
                    .set_cell("note", Cell::Str("child changed".into())),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(inserts(&tx), usize::from(p || c));
        let mut expected = Vec::new();
        if p {
            expected.push(event(PARENT, &[parents[0]], &["name"]));
        }
        if c {
            expected.push(event(CHILD, &[children[0]], &["note"]));
        }
        assert_eq!(events(&h, &op).await, expected, "parent={p}, child={c}");
        let (ps, cs) = state(&h).await;
        assert_eq!(ps[0], (parents[0], "parent changed".into()));
        assert_eq!(cs[0], (children[0], parents[0], "child changed".into()));
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn reassignment_changes_new_parent_and_child_only() {
    let h = harness(true, true).await;
    let (parents, children) = seed(&h, true).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.capture_statements(true);
    tx.env()
        .records(CHILD)
        .unwrap()
        .browse([children[0]])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("parent_id", Cell::Id(parents[1]))
                .set_cell("name", Cell::Str("new parent changed".into()))
                .set_cell("note", Cell::Str("moved".into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    assert_eq!(
        events(&h, &op).await,
        vec![
            event(PARENT, &[parents[1]], &["name"]),
            event(CHILD, &[children[0]], &["note", "parent_id"])
        ]
    );
    let (ps, cs) = state(&h).await;
    assert_eq!(
        ps,
        vec![
            (parents[0], "original".into()),
            (parents[1], "new parent changed".into())
        ]
    );
    assert_eq!(
        cs,
        vec![
            (children[0], parents[1], "moved".into()),
            (children[1], parents[0], "initial".into())
        ]
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn link_only_reassignment_and_noop_emit_only_actual_changes() {
    let h = harness(true, true).await;
    let (parents, children) = seed(&h, true).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.env()
        .records(CHILD)
        .unwrap()
        .browse([children[0]])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_id", Cell::Id(parents[1])),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        events(&h, &op).await,
        vec![event(CHILD, &[children[0]], &["parent_id"])]
    );
    let before = state(&h).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.capture_statements(true);
    tx.env()
        .records(CHILD)
        .unwrap()
        .browse([children[0]])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("parent_id", Cell::Id(parents[1]))
                .set_cell("name", Cell::Str("original".into()))
                .set_cell("note", Cell::Str("initial".into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 0);
    assert_eq!(events(&h, &op).await, vec![]);
    assert_eq!(state(&h).await, before);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn batched_children_deduplicate_parents_in_warm_and_cold_caches() {
    for shared in [true, false] {
        for warm in [false, true] {
            let h = harness(true, true).await;
            let (parents, children) = seed(&h, shared).await;
            let mut tx = begin(&h).await;
            let op = tx
                .env()
                .operation_identity()
                .unwrap()
                .operation()
                .to_string();
            let records = tx
                .env()
                .records(CHILD)
                .unwrap()
                .browse(children.iter().copied())
                .unwrap();
            if warm {
                records
                    .read(&mut tx, &["name", "parent_id", "note"])
                    .await
                    .unwrap();
            }
            tx.capture_statements(true);
            records
                .write(
                    &mut tx,
                    RawVals::new()
                        .set_cell("name", Cell::Str("batch parent".into()))
                        .set_cell("note", Cell::Str("batch child".into())),
                )
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(inserts(&tx), 1);
            let parent_ids = if shared { &parents[..1] } else { &parents[..] };
            assert_eq!(
                events(&h, &op).await,
                vec![
                    event(PARENT, parent_ids, &["name"]),
                    event(CHILD, &children, &["note"])
                ],
                "shared={shared} warm={warm}"
            );
            let (ps, cs) = state(&h).await;
            assert_eq!(ps[0].1, "batch parent");
            assert_eq!(ps[1].1, if shared { "original" } else { "batch parent" });
            assert!(cs.iter().all(|row| row.2 == "batch child"));
            h.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn successive_mixed_writes_preserve_parent_child_operation_order() {
    let h = harness(true, true).await;
    let (parents, children) = seed(&h, false).await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    let mut operations = Vec::new();
    for value in ["first", "second", "first"] {
        // Distinct server operations make a whole-pair permutation observable.
        let mut origin = begin(&h).await;
        let writer = origin.env().clone();
        operations.push(writer.operation_identity().unwrap().operation().to_string());
        origin.rollback().await.unwrap();
        writer
            .records(CHILD)
            .unwrap()
            .browse([children[0]])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("name", Cell::Str(value.into()))
                    .set_cell("note", Cell::Str(value.into())),
            )
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    for op in &operations {
        assert_eq!(
            events(&h, op).await,
            vec![
                event(PARENT, &[parents[0]], &["name"]),
                event(CHILD, &[children[0]], &["note"]),
            ]
        );
    }
    let mut check = h.begin().await.unwrap();
    let actual: Vec<(String, String)> = check.query(
        "SELECT model, operation_id::text FROM erp_event.ir_event_outbox WHERE kind = 'write' AND operation_id::text = ANY($1) ORDER BY id",
        &[&operations],
    ).await.unwrap().iter().map(|row| (row.get(0), row.get(1))).collect();
    check.rollback().await.unwrap();
    let expected: Vec<_> = operations
        .iter()
        .flat_map(|op| {
            [
                (PARENT.to_owned(), op.clone()),
                (CHILD.to_owned(), op.clone()),
            ]
        })
        .collect();
    assert_eq!(
        actual, expected,
        "whole operation pairs must retain their originating order"
    );
    let (ps, cs) = state(&h).await;
    assert_eq!(ps[0].1, "first");
    assert_eq!(cs[0].2, "first");
    h.cleanup().await.unwrap();
}
