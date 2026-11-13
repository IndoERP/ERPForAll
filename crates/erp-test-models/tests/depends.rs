//! Computed fields through relational paths (slice S4b): trigger trees, marking across models,
//! recomputation in one flush, related fields through relations, precompute through many2one
//! fields, recursive fields, and deletions through cascades.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::{RawVals, Records, Transaction, Value};
use erp_testkit::assert_query_count;
use support::map;

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

fn s(v: &str) -> Value {
    Value::str(v)
}

fn int(records: &Records, i: usize) -> Value {
    Value::Int(records.ids()[i])
}

async fn write(tx: &mut Transaction, records: &Records, pairs: &[(&str, Value)]) {
    records.write_values(tx, &map(pairs)).await.unwrap();
}

async fn floats(tx: &mut Transaction, records: &Records, field: &str) -> Vec<f64> {
    records
        .values(tx, field)
        .await
        .unwrap()
        .into_iter()
        .map(|v| match v {
            Value::Float(f) => f,
            Value::Int(i) => i as f64,
            other => unreachable!("not a number: {other:?}"),
        })
        .collect()
}

/// An order with lines `(qty, price)`.
async fn order(tx: &mut Transaction, name: &str, lines: &[(f64, f64)]) -> Records {
    let order = create(tx, "test_dep.order", &[&[("name", s(name))]]).await;
    let rows: Vec<Vec<(&str, Value)>> = lines
        .iter()
        .map(|(q, p)| {
            vec![
                ("order_id", int(&order, 0)),
                ("qty", Value::Float(*q)),
                ("price_unit", Value::Float(*p)),
            ]
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    if !refs.is_empty() {
        create(tx, "test_dep.line", &refs).await;
    }
    order
}

#[tokio::test]
async fn spec_comp_009_every_prefix_triggers_and_inverse_changes_reach_both_owners() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let o1 = order(&mut tx, "o1", &[(1.0, 10.0), (2.0, 5.0)]).await;
    let o2 = order(&mut tx, "o2", &[(1.0, 1.0)]).await;
    assert_eq!(floats(&mut tx, &o1, "amount_total").await, [20.0]);
    assert_eq!(floats(&mut tx, &o2, "amount_total").await, [1.0]);
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let lines = o1.related(&mut tx, "line_ids").await.unwrap();
    // the last step changes: the lines' order
    write(
        &mut tx,
        &lines.browse_ids([lines.rec_ids()[0]]),
        &[("price_unit", Value::Float(20.0))],
    )
    .await;
    assert_eq!(floats(&mut tx, &o1, "amount_total").await, [30.0]);
    // the inverse of the one2many changes: old and new orders
    write(
        &mut tx,
        &lines.browse_ids([lines.rec_ids()[1]]),
        &[("order_id", int(&o2, 0))],
    )
    .await;
    assert_eq!(floats(&mut tx, &o1, "amount_total").await, [20.0]);
    assert_eq!(floats(&mut tx, &o2, "amount_total").await, [11.0]);
    // a line is created
    create(
        &mut tx,
        "test_dep.line",
        &[&[
            ("order_id", int(&o1, 0)),
            ("qty", Value::Float(3.0)),
            ("price_unit", Value::Float(1.0)),
        ]],
    )
    .await;
    assert_eq!(floats(&mut tx, &o1, "amount_total").await, [23.0]);
    // persisted
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        floats(&mut tx, &o1.union(&o2).unwrap(), "amount_total").await,
        [23.0, 11.0]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_010_two_relational_levels_in_one_flush() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partner = create(&mut tx, "test_dep.partner", &[&[("name", s("P"))]]).await;
    let o1 = order(&mut tx, "o1", &[(1.0, 10.0)]).await;
    let o2 = order(&mut tx, "o2", &[(2.0, 10.0)]).await;
    write(
        &mut tx,
        &o1.union(&o2).unwrap(),
        &[("partner_id", int(&partner, 0))],
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(floats(&mut tx, &partner, "total_amount").await, [30.0]);
    let line = o2.related(&mut tx, "line_ids").await.unwrap();
    write(&mut tx, &line, &[("qty", Value::Float(5.0))]).await;
    // one flush settles the line, the order and the partner
    tx.flush().await.unwrap();
    assert!(
        tx.fields_to_compute().is_empty(),
        "{:?}",
        tx.fields_to_compute()
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(floats(&mut tx, &o2, "amount_total").await, [50.0]);
    assert_eq!(floats(&mut tx, &partner, "total_amount").await, [60.0]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_012_regular_models_do_not_trigger_transient_ones() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partner = create(&mut tx, "test_dep.partner", &[&[("name", s("Old"))]]).await;
    let wizard = create(
        &mut tx,
        "test_dep.wizard",
        &[&[("partner_id", int(&partner, 0))]],
    )
    .await;
    assert_eq!(
        wizard.values(&mut tx, "partner_label").await.unwrap(),
        [s("Old")]
    );
    tx.flush().await.unwrap();
    write(&mut tx, &partner, &[("name", s("New"))]).await;
    let pending = tx.records_to_compute(&wizard, "partner_label").unwrap();
    assert!(pending.is_empty(), "the wizard is not marked");
    assert_eq!(
        wizard.values(&mut tx, "partner_label").await.unwrap(),
        [s("Old")]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn chain(tx: &mut Transaction, names: &[&str]) -> Vec<Records> {
    let mut out: Vec<Records> = Vec::new();
    for name in names {
        let mut row = vec![("name", s(name))];
        if let Some(parent) = out.last() {
            row.push(("parent", int(parent, 0)));
        }
        out.push(create(tx, "test_dep.recursive", &[&row]).await);
    }
    out
}

async fn labels(tx: &mut Transaction, records: &[Records], field: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for r in records {
        out.push(r.value(tx, field).await.unwrap());
    }
    out
}

#[tokio::test]
async fn spec_comp_022_recursive_fields_follow_the_hierarchy() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let [a, b, c, d]: [Records; 4] = chain(&mut tx, &["A", "B", "C", "D"])
        .await
        .try_into()
        .unwrap();
    let all = [a.clone(), b.clone(), c.clone(), d.clone()];
    for field in ["full_name", "display_label"] {
        assert_eq!(
            labels(&mut tx, &all, field).await,
            [s("A"), s("A / B"), s("A / B / C"), s("A / B / C / D")],
            "{field}"
        );
    }
    write(&mut tx, &a, &[("name", s("A1"))]).await;
    for field in ["full_name", "display_label"] {
        assert_eq!(
            labels(&mut tx, &all, field).await,
            [s("A1"), s("A1 / B"), s("A1 / B / C"), s("A1 / B / C / D")],
            "{field}"
        );
    }
    write(&mut tx, &b, &[("parent", Value::False)]).await;
    for field in ["full_name", "display_label"] {
        assert_eq!(
            labels(&mut tx, &all, field).await,
            [s("A1"), s("B"), s("B / C"), s("B / C / D")],
            "{field}"
        );
    }
    // several records renamed at once
    write(
        &mut tx,
        &d.union_all([&c, &b]).unwrap(),
        &[("name", s("X"))],
    )
    .await;
    for field in ["full_name", "display_label"] {
        assert_eq!(
            labels(&mut tx, &all, field).await,
            [s("A1"), s("X"), s("X / X"), s("X / X / X")],
            "{field}"
        );
    }
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        labels(&mut tx, &all, "display_label").await,
        [s("A1"), s("X"), s("X / X"), s("X / X / X")],
        "stored"
    );
    // deleting B cascades to C and D; recomputation ends
    b.unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();
    assert!(tx.fields_to_compute().is_empty());
    assert_eq!(a.value(&mut tx, "display_label").await.unwrap(), s("A1"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// hierarchy settles in one flush, level by level, beyond the flush round limit)
#[tokio::test]
async fn spec_comp_013_self_dependency_is_recursive() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let names: Vec<String> = (0..40).map(|i| format!("n{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let nodes = chain(&mut tx, &refs).await;
    tx.flush().await.unwrap();
    write(&mut tx, &nodes[0], &[("name", s("root"))]).await;
    tx.flush().await.unwrap();
    assert!(tx.fields_to_compute().is_empty());
    tx.invalidate_all().await.unwrap();
    let last = nodes[39].value(&mut tx, "display_label").await.unwrap();
    let expected = std::iter::once("root".to_owned())
        .chain((1..40).map(|i| format!("n{i}")))
        .collect::<Vec<_>>()
        .join(" / ");
    assert_eq!(last, Value::str(expected));
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- / -016 / -019 -----------------------------------------------------------------

struct ModifiedFixture {
    modified: Records,
    line: Records,
    child: Records,
    grandchild: Records,
}

async fn modified_fixture(tx: &mut Transaction) -> ModifiedFixture {
    let modified = create(tx, "test_dep.modified", &[&[("name", s("Test"))]]).await;
    let line = create(
        tx,
        "test_dep.modified_line",
        &[&[
            ("modified_id", int(&modified, 0)),
            ("quantity", Value::Int(5)),
            ("price", Value::Float(1.0)),
        ]],
    )
    .await;
    let child = create(
        tx,
        "test_dep.modified_line",
        &[&[
            ("modified_id", int(&modified, 0)),
            ("quantity", Value::Int(5)),
            ("price", Value::Float(2.0)),
            ("parent_id", int(&line, 0)),
        ]],
    )
    .await;
    let grandchild = create(
        tx,
        "test_dep.modified_line",
        &[&[
            ("modified_id", int(&modified, 0)),
            ("quantity", Value::Int(5)),
            ("price", Value::Float(3.0)),
            ("parent_id", int(&child, 0)),
        ]],
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    ModifiedFixture {
        modified,
        line,
        child,
        grandchild,
    }
}

#[tokio::test]
async fn spec_comp_015_modified_costs_nothing_on_an_empty_cache() {
    let h = support::dep_orm().await;
    // a related field on lines
    let mut tx = h.begin().await.unwrap();
    let f = modified_fixture(&mut tx).await;
    assert_query_count!(tx, <= 0, {
        write(&mut tx, &f.modified, &[("name", s("Other"))]).await;
    });
    assert_eq!(
        f.line.value(&mut tx, "modified_name").await.unwrap(),
        s("Other")
    );
    tx.rollback().await.unwrap();
    // a non-stored total over the lines
    let mut tx = h.begin().await.unwrap();
    let f = modified_fixture(&mut tx).await;
    assert_query_count!(tx, <= 0, {
        write(&mut tx, &f.line, &[("quantity", Value::Int(8))]).await;
    });
    assert_eq!(
        f.modified.value(&mut tx, "total_quantity").await.unwrap(),
        Value::Int(18)
    );
    tx.rollback().await.unwrap();
    // a recursive non-stored total, empty cache
    let mut tx = h.begin().await.unwrap();
    let f = modified_fixture(&mut tx).await;
    assert_query_count!(tx, <= 0, {
        write(&mut tx, &f.grandchild, &[("price", Value::Float(4.0))]).await;
    });
    assert_eq!(floats(&mut tx, &f.line, "total_price").await, [7.0]);
    assert_eq!(
        floats(&mut tx, &f.line, "total_price_quantity").await,
        [35.0]
    );
    assert_eq!(floats(&mut tx, &f.child, "total_price").await, [6.0]);
    assert_eq!(
        floats(&mut tx, &f.child, "total_price_quantity").await,
        [30.0]
    );
    tx.rollback().await.unwrap();
    // the same with the cache filled by an earlier read
    let mut tx = h.begin().await.unwrap();
    let f = modified_fixture(&mut tx).await;
    assert_eq!(floats(&mut tx, &f.line, "total_price").await, [6.0]);
    assert_eq!(
        floats(&mut tx, &f.line, "total_price_quantity").await,
        [30.0]
    );
    assert_query_count!(tx, <= 0, {
        write(&mut tx, &f.grandchild, &[("price", Value::Float(4.0))]).await;
    });
    assert_eq!(
        floats(&mut tx, &f.line, "total_price_quantity").await,
        [35.0]
    );
    assert_eq!(floats(&mut tx, &f.line, "total_price").await, [7.0]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_016_partial_cache_drives_traversal_queries() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = modified_fixture(&mut tx).await;
    assert_eq!(
        floats(&mut tx, &f.child, "total_price_quantity").await,
        [25.0]
    );
    tx.invalidate_records(&f.grandchild, None).await.unwrap();
    f.child.value(&mut tx, "price").await.unwrap();
    tx.capture_statements(true);
    // one SELECT of the grand-child's columns, to follow its parent
    assert_query_count!(tx, >= 1, <= 1, {
        write(&mut tx, &f.grandchild, &[("price", Value::Float(4.0))]).await;
    });
    assert!(
        tx.captured()[0].starts_with("SELECT \"id\""),
        "{:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    assert_eq!(
        floats(&mut tx, &f.grandchild, "total_price_quantity").await,
        [20.0]
    );
    assert_eq!(
        floats(&mut tx, &f.child, "total_price_quantity").await,
        [30.0]
    );
    assert_eq!(
        floats(&mut tx, &f.line, "total_price_quantity").await,
        [35.0]
    );
    assert_eq!(floats(&mut tx, &f.line, "total_price").await, [7.0]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_019_no_inverse_traversal_and_create() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = modified_fixture(&mut tx).await;
    let positive = create(
        &mut tx,
        "test_dep.modified_positive",
        &[&[("line_ids", Value::list([int(&f.line, 0)]))]],
    )
    .await;
    assert_eq!(
        positive.value(&mut tx, "is_positive").await.unwrap(),
        Value::Bool(true)
    );
    let lines = tx.env().records("test_dep.modified_line").unwrap();
    lines.create(&mut tx, vec![RawVals::new()]).await.unwrap(); // warm the statement
    // creating a line: one INSERT, no traversal (nobody can point at a new record)
    assert_query_count!(tx, >= 1, <= 1, {
        lines.create(&mut tx, vec![RawVals::new()]).await.unwrap();
    });
    // writing a line's quantity: one query of the relation table finds the positives
    tx.capture_statements(true);
    assert_query_count!(tx, >= 1, <= 1, {
        write(&mut tx, &f.line, &[("quantity", Value::Int(-5))]).await;
    });
    assert!(
        tx.captured()[0].contains("SELECT DISTINCT"),
        "{:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    assert_eq!(
        positive.value(&mut tx, "is_positive").await.unwrap(),
        Value::Bool(false)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- / -018 / -020 -----------------------------------------------------------------

#[tokio::test]
async fn spec_comp_017_relational_change_recomputes_old_and_new_targets() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let d = create(
        &mut tx,
        "test_dep.discussion",
        &[&[("name", s("D1"))], &[("name", s("D2"))]],
    )
    .await;
    let (d1, d2) = (
        d.browse_ids([d.rec_ids()[0]]),
        d.browse_ids([d.rec_ids()[1]]),
    );
    let m = create(
        &mut tx,
        "test_dep.message",
        &[&[("discussion_id", int(&d1, 0)), ("author", s("Ann"))]],
    )
    .await;
    assert_eq!(m.value(&mut tx, "name").await.unwrap(), s("[D1] Ann"));
    assert_eq!(
        d1.value(&mut tx, "message_count").await.unwrap(),
        Value::Int(1)
    );
    tx.flush().await.unwrap();
    write(&mut tx, &m, &[("discussion_id", int(&d2, 0))]).await;
    assert_eq!(m.value(&mut tx, "name").await.unwrap(), s("[D2] Ann"));
    assert_eq!(
        d1.value(&mut tx, "message_count").await.unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        d2.value(&mut tx, "message_count").await.unwrap(),
        Value::Int(1)
    );
    // renaming the discussion recomputes the message
    write(&mut tx, &d2, &[("name", s("D2b"))]).await;
    assert_eq!(m.value(&mut tx, "name").await.unwrap(), s("[D2b] Ann"));
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(m.value(&mut tx, "name").await.unwrap(), s("[D2b] Ann"));
    assert_eq!(
        d1.value(&mut tx, "message_count").await.unwrap(),
        Value::Int(0)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_018_many2many_marks_only_changed_records() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let users = create(
        &mut tx,
        "test_dep.user",
        &[
            &[("name", s("u1"))],
            &[("name", s("u2"))],
            &[("name", s("u3"))],
        ],
    )
    .await;
    let [u1, u2, u3] = [0, 1, 2].map(|i| users.browse_ids([users.rec_ids()[i]]));
    let g = create(
        &mut tx,
        "test_dep.group",
        &[&[("name", s("G")), ("user_ids", Value::list([int(&u1, 0)]))]],
    )
    .await;
    tx.flush().await.unwrap();
    let pending = |tx: &Transaction| -> Vec<i64> {
        tx.records_to_compute(&users, "group_count").unwrap().ids()
    };
    let cmd = |code: i64, id: Value| Value::list([Value::Int(code), id, Value::Int(0)]);
    let link = |r: &Records| cmd(4, int(r, 0));
    write(
        &mut tx,
        &g,
        &[("user_ids", Value::list([link(&u1), link(&u2), link(&u3)]))],
    )
    .await;
    assert_eq!(pending(&tx), [u2.ids()[0], u3.ids()[0]]);
    tx.flush().await.unwrap();
    write(
        &mut tx,
        &g,
        &[("user_ids", Value::list([cmd(3, int(&u2, 0))]))],
    )
    .await;
    assert_eq!(pending(&tx), [u2.ids()[0]]);
    tx.flush().await.unwrap();
    write(
        &mut tx,
        &g,
        &[("user_ids", Value::list([int(&u1, 0), int(&u2, 0)]))],
    )
    .await;
    assert_eq!(pending(&tx), [u2.ids()[0], u3.ids()[0]]);
    tx.flush().await.unwrap();
    write(&mut tx, &u3, &[("group_ids", Value::list([link(&g)]))]).await;
    assert_eq!(pending(&tx), [u3.ids()[0]]);
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        users.values(&mut tx, "group_count").await.unwrap(),
        [Value::Int(1), Value::Int(1), Value::Int(1)]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_020_traversal_ignores_the_active_flag() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let tree = create(&mut tx, "test_dep.tree", &[&[("name", s("T"))]]).await;
    let leaves = create(
        &mut tx,
        "test_dep.leaf",
        &[
            &[("tree_id", int(&tree, 0)), ("value", Value::Int(1))],
            &[
                ("tree_id", int(&tree, 0)),
                ("value", Value::Int(2)),
                ("active", Value::Bool(false)),
            ],
        ],
    )
    .await;
    assert_eq!(
        tree.value(&mut tx, "leaf_sum").await.unwrap(),
        Value::Int(1)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    // the archived leaf changes: its tree is found and marked
    let archived = leaves.browse_ids([leaves.rec_ids()[1]]);
    write(&mut tx, &archived, &[("value", Value::Int(5))]).await;
    assert_eq!(
        tx.records_to_compute(&tree, "leaf_sum").unwrap().ids(),
        tree.ids()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- (paths) / -------------------------------------------------------

#[tokio::test]
async fn spec_comp_038_related_through_relations() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let d = create(
        &mut tx,
        "test_dep.discussion",
        &[&[("name", s("Foo"))], &[("name", s("Bar"))]],
    )
    .await;
    let m = create(
        &mut tx,
        "test_dep.message",
        &[
            &[("discussion_id", int(&d, 0))],
            &[("discussion_id", int(&d, 1))],
        ],
    )
    .await;
    assert_eq!(
        m.values(&mut tx, "discussion_name").await.unwrap(),
        [s("Foo"), s("Bar")]
    );
    // the target changes
    write(
        &mut tx,
        &d.browse_ids([d.rec_ids()[0]]),
        &[("name", s("Foo2"))],
    )
    .await;
    assert_eq!(
        m.values(&mut tx, "discussion_name").await.unwrap(),
        [s("Foo2"), s("Bar")]
    );
    // writing the related field writes every target
    write(&mut tx, &m, &[("discussion_name", s("Baz"))]).await;
    assert_eq!(
        d.values(&mut tx, "name").await.unwrap(),
        [s("Baz"), s("Baz")]
    );
    assert_eq!(
        m.values(&mut tx, "name").await.unwrap(),
        [s("[Baz] "), s("[Baz] ")]
    );
    // two hops, stored, through many2one fields
    let country = create(
        &mut tx,
        "test_dep.country",
        &[&[("name", s("Belgium")), ("code", s("BE"))]],
    )
    .await;
    let partner = create(
        &mut tx,
        "test_dep.partner",
        &[&[("name", s("P")), ("country_id", int(&country, 0))]],
    )
    .await;
    let o = create(
        &mut tx,
        "test_dep.order",
        &[&[("name", s("o")), ("partner_id", int(&partner, 0))]],
    )
    .await;
    assert_eq!(
        o.value(&mut tx, "partner_country_code").await.unwrap(),
        s("BE")
    );
    write(&mut tx, &country, &[("code", s("BX"))]).await;
    assert_eq!(
        o.value(&mut tx, "partner_country_code").await.unwrap(),
        s("BX")
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        o.value(&mut tx, "partner_country_code").await.unwrap(),
        s("BX")
    );
    // an editable related field on a model whose target is set later
    write(&mut tx, &o, &[("partner_name_rw", s("Renamed"))]).await;
    assert_eq!(partner.value(&mut tx, "name").await.unwrap(), s("Renamed"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_072_stored_computed_many2one_feeds_its_one2many() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let container = create(&mut tx, "test_dep.container", &[&[("name", s("Foo"))]]).await;
    let member = create(&mut tx, "test_dep.member", &[&[("name", s("Foo"))]]).await;
    assert_eq!(
        container
            .related(&mut tx, "member_ids")
            .await
            .unwrap()
            .ids(),
        member.ids()
    );
    assert_eq!(
        container.value(&mut tx, "member_count").await.unwrap(),
        Value::Int(1)
    );
    write(&mut tx, &member, &[("name", s("Bar"))]).await;
    assert_eq!(
        container.value(&mut tx, "member_count").await.unwrap(),
        Value::Int(0)
    );
    write(&mut tx, &member, &[("name", s("Foo"))]).await;
    assert_eq!(
        container.value(&mut tx, "member_count").await.unwrap(),
        Value::Int(1)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        container.value(&mut tx, "member_count").await.unwrap(),
        Value::Int(1)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- precompute through many2one fields ---------------------------

#[tokio::test]
async fn spec_crud_013_required_related_is_precomputed() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = create(
        &mut tx,
        "test_dep.partner",
        &[
            &[("name", s("p1"))],
            &[("name", s("p2"))],
            &[("name", s("p3"))],
        ],
    )
    .await;
    let model = tx.env().records("test_dep.required").unwrap();
    let row = |i: usize| map(&[("partner_id", int(&partners, i))]);
    model.create_values(&mut tx, &[row(0)]).await.unwrap(); // warm the statement
    // the partner's name is cached: one INSERT
    tx.capture_statements(true);
    let one = assert_query_count!(tx, >= 1, <= 1, {
        model.create_values(&mut tx, &[row(1)]).await.unwrap()
    });
    assert!(tx.captured()[0].contains("\"name\""), "{:?}", tx.captured());
    tx.capture_statements(false);
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    // three records: one SELECT of the partners' names, one INSERT
    let three = assert_query_count!(tx, >= 2, <= 2, {
        model
            .create_values(&mut tx, &[row(0), row(1), row(2)])
            .await
            .unwrap()
    });
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(one.values(&mut tx, "name").await.unwrap(), [s("p2")]);
    assert_eq!(
        three.values(&mut tx, "name").await.unwrap(),
        [s("p1"), s("p2"), s("p3")]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_054_create_then_update_of_stored_computes_over_lines() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = tx.env().records("test_dep.order").unwrap();
    let with_lines = |n: i64| {
        RawVals::new()
            .set_cell("name", erp_sdk::Cell::Str("X".into()))
            .set_commands(
                "line_ids",
                (0..n).map(|v| {
                    erp_sdk::Command::create(
                        RawVals::new().set_cell("price_unit", erp_sdk::Cell::Float(v as f64)),
                    )
                }),
            )
    };
    orders.create(&mut tx, vec![with_lines(0)]).await.unwrap();
    orders.create(&mut tx, vec![with_lines(10)]).await.unwrap();
    tx.flush().await.unwrap();
    // INSERT, then the UPDATE of the stored total at flush
    let o = assert_query_count!(tx, >= 1, <= 2, {
        let o = orders.create(&mut tx, vec![with_lines(0)]).await.unwrap();
        tx.flush().await.unwrap();
        o
    });
    // with 10 lines: INSERT, INSERT of the lines (prices precomputed), UPDATE of the total
    let o10 = assert_query_count!(tx, >= 2, <= 3, {
        let o = orders.create(&mut tx, vec![with_lines(10)]).await.unwrap();
        tx.flush().await.unwrap();
        o
    });
    tx.invalidate_all().await.unwrap();
    assert_eq!(floats(&mut tx, &o, "amount_total").await, [0.0]);
    assert_eq!(floats(&mut tx, &o10, "amount_total").await, [45.0]);
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- `_inherits` and cascades --------------------------------------------------------------------

#[tokio::test]
async fn spec_crud_025_dependency_on_a_delegated_field() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let boxed = create(&mut tx, "test_dep.boxed", &[&[("name", s("U1"))]]).await;
    assert_eq!(boxed.value(&mut tx, "unit_name").await.unwrap(), s("U1"));
    tx.flush().await.unwrap();
    // through the child
    write(&mut tx, &boxed, &[("name", s("U2"))]).await;
    assert_eq!(boxed.value(&mut tx, "unit_name").await.unwrap(), s("U2"));
    // on the parent directly
    let unit = boxed.related(&mut tx, "unit_id").await.unwrap();
    write(&mut tx, &unit, &[("name", s("U3"))]).await;
    assert_eq!(boxed.value(&mut tx, "unit_name").await.unwrap(), s("U3"));
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(boxed.value(&mut tx, "unit_name").await.unwrap(), s("U3"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `c.b_id` null and deletes `d`; the stored computes of `c` follow, cached or not)
#[tokio::test]
async fn spec_comp_023_cascades_of_cascades_notify_dependents() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let a = create(&mut tx, "test_dep.cas_a", &[&[("name", s("a"))]]).await;
    let b = create(
        &mut tx,
        "test_dep.cas_b",
        &[&[("name", s("b")), ("a_id", int(&a, 0))]],
    )
    .await;
    let c = create(
        &mut tx,
        "test_dep.cas_c",
        &[
            &[("name", s("c1")), ("b_id", int(&b, 0))],
            &[("name", s("c2")), ("b_id", int(&b, 0))],
        ],
    )
    .await;
    create(
        &mut tx,
        "test_dep.cas_d",
        &[
            &[("c_id", int(&c, 0)), ("b_id", int(&b, 0))],
            &[("c_id", int(&c, 0))],
            &[("c_id", int(&c, 1)), ("b_id", int(&b, 0))],
        ],
    )
    .await;
    assert_eq!(
        c.values(&mut tx, "has_b").await.unwrap(),
        [Value::Bool(true), Value::Bool(true)]
    );
    assert_eq!(
        c.values(&mut tx, "d_count").await.unwrap(),
        [Value::Int(2), Value::Int(1)]
    );
    tx.flush().await.unwrap();
    // c2 is not in the cache any more, c1 is
    tx.invalidate_all().await.unwrap();
    c.browse_ids([c.rec_ids()[0]])
        .values(&mut tx, "d_count")
        .await
        .unwrap();
    a.unlink(&mut tx).await.unwrap();
    assert_eq!(
        c.values(&mut tx, "has_b").await.unwrap(),
        [Value::Bool(false), Value::Bool(false)]
    );
    assert_eq!(
        c.values(&mut tx, "d_count").await.unwrap(),
        [Value::Int(1), Value::Int(0)]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        c.values(&mut tx, "d_count").await.unwrap(),
        [Value::Int(1), Value::Int(0)],
        "stored"
    );
    assert_eq!(
        c.values(&mut tx, "has_b").await.unwrap(),
        [Value::Bool(false), Value::Bool(false)],
        "stored"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- BENCH-005 shape -----------------------------------------------------------------------------

/// BENCH-005 shape (workload as a budget test): 1,000 orders of 10 lines; a price change
/// on every line recomputes the stored line subtotals and order totals in one flush, with a
/// number of statements that does not depend on the number of records: the lines' columns (one
/// prefetch), the orders' lines (one query), the orders' old totals (one prefetch), one UPDATE of
/// the lines and one of the orders.
#[tokio::test]
async fn bench_005_shape_price_change_recomputes_order_totals_in_one_flush() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = tx.env().records("test_dep.order").unwrap();
    let vals: Vec<RawVals> = (0..1000)
        .map(|o| {
            RawVals::new()
                .set_cell("name", erp_sdk::Cell::Str(format!("o{o}").into()))
                .set_commands(
                    "line_ids",
                    (0..10).map(|l| {
                        erp_sdk::Command::create(
                            RawVals::new()
                                .set_cell("product", erp_sdk::Cell::Str(format!("p{l}").into()))
                                .set_cell("price_unit", erp_sdk::Cell::Float(1.0)),
                        )
                    }),
                )
        })
        .collect();
    let created = orders.create(&mut tx, vals).await.unwrap();
    tx.flush().await.unwrap();
    // a new request: nothing cached
    tx.invalidate_all().await.unwrap();
    let lines = tx
        .env()
        .records("test_dep.line")
        .unwrap()
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(lines.len(), 10_000);
    let before = tx.query_count();
    tx.capture_statements(true);
    write(&mut tx, &lines, &[("price_unit", Value::Float(2.0))]).await;
    tx.flush().await.unwrap();
    let used = tx.query_count() - before;
    let captured = tx.captured().to_vec();
    tx.capture_statements(false);
    eprintln!("BENCH-005 shape: {used} statements for 10,000 lines and 1,000 orders");
    assert!((3..=5).contains(&used), "{used} statements: {captured:#?}");
    assert!(tx.fields_to_compute().is_empty());
    tx.invalidate_all().await.unwrap();
    let totals = floats(&mut tx, &created, "amount_total").await;
    assert_eq!(totals.len(), 1000);
    assert!(
        totals.iter().all(|t| (*t - 20.0).abs() < 1e-9),
        "{:?}",
        &totals[..3]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// record, then the flush writes the records and the lines)
#[tokio::test]
async fn spec_comp_065_prefetch_is_preserved_through_assignment() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_dep.prefetch").unwrap();
    let vals: Vec<RawVals> = (0..4)
        .map(|_| {
            RawVals::new().set_commands("line_ids", [erp_sdk::Command::create(RawVals::new())])
        })
        .collect();
    let records = model.create(&mut tx, vals).await.unwrap();
    tx.flush().await.unwrap();
    for (round, offset) in [(0, 1), (1, 2)] {
        tx.invalidate_all().await.unwrap();
        assert_query_count!(tx, >= 3, <= 3, {
            for (i, record) in records.iter().enumerate() {
                let value = Value::Int(i64::try_from(i).unwrap() + offset);
                if round == 0 {
                    record.set_value(&mut tx, "harry", value).await.unwrap();
                } else {
                    write(&mut tx, &record, &[("harry", value)]).await;
                }
            }
            tx.flush().await.unwrap();
        });
        tx.invalidate_all().await.unwrap();
        let lines = records.related(&mut tx, "line_ids").await.unwrap();
        let expected: Vec<Value> = (0..4).map(|i| Value::Int(i + offset)).collect();
        assert_eq!(lines.values(&mut tx, "harry").await.unwrap(), expected);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_071_searching_a_one2many_flushes_the_pending_inverse() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let moves = create(&mut tx, "test_dep.move", &[&[], &[]]).await;
    let (m1, m2) = (
        moves.browse_ids([moves.rec_ids()[0]]),
        moves.browse_ids([moves.rec_ids()[1]]),
    );
    let line = create(
        &mut tx,
        "test_dep.move_line",
        &[&[("move_id", int(&m1, 0))]],
    )
    .await;
    let find = |l: &Records| erp_sdk::Domain::field("line_ids").unwrap().in_(l.ids());
    let model = tx.env().records("test_dep.move").unwrap();
    assert_eq!(model.search(&mut tx, &find(&line)).await.unwrap(), m1);
    write(&mut tx, &line, &[("move_id", int(&m2, 0))]).await;
    assert!(tx.is_dirty(&line, "move_id").unwrap());
    assert_eq!(model.search(&mut tx, &find(&line)).await.unwrap(), m2);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_038_search_on_related_fields() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let d = create(
        &mut tx,
        "test_dep.discussion",
        &[&[("name", s("Foo"))], &[("name", s("Bar"))]],
    )
    .await;
    let m = create(
        &mut tx,
        "test_dep.message",
        &[
            &[("discussion_id", int(&d, 0))],
            &[("discussion_id", int(&d, 1))],
            &[],
        ],
    )
    .await;
    let messages = tx.env().records("test_dep.message").unwrap();
    let by = |json: erp_sdk::serde_json::Value| support::dom(json);
    let related = messages
        .search(
            &mut tx,
            &by(erp_sdk::serde_json::json!([[
                "discussion_name",
                "=",
                "Bar"
            ]])),
        )
        .await
        .unwrap();
    let path = messages
        .search(
            &mut tx,
            &by(erp_sdk::serde_json::json!([[
                "discussion_id.name",
                "=",
                "Bar"
            ]])),
        )
        .await
        .unwrap();
    assert_eq!(related, path);
    assert_eq!(related.ids(), [m.ids()[1]]);
    // an empty value matches records without a discussion ('s semantics)
    let empty = messages
        .search(
            &mut tx,
            &by(erp_sdk::serde_json::json!([[
                "discussion_name",
                "=",
                false
            ]])),
        )
        .await
        .unwrap();
    assert_eq!(empty.ids(), [m.ids()[2]]);
    // a negation: the complement of the positive condition
    let not_bar = messages
        .search(
            &mut tx,
            &by(erp_sdk::serde_json::json!([[
                "discussion_name",
                "!=",
                "Bar"
            ]])),
        )
        .await
        .unwrap();
    assert_eq!(not_bar.ids(), [m.ids()[0], m.ids()[2]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// through a one2many, observed in the INSERT columns; the declared half is in compute_graph.rs)
#[tokio::test]
async fn spec_comp_049_precompute_validity_through_relations() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partner = create(&mut tx, "test_dep.partner", &[&[("name", s("P"))]]).await;
    tx.capture_statements(true);
    let o = create(
        &mut tx,
        "test_dep.order",
        &[&[("name", s("o")), ("partner_id", int(&partner, 0))]],
    )
    .await;
    let insert = tx.captured().last().cloned().unwrap();
    // a stored related field through many2one fields is in the INSERT; a stored sum over the
    // lines (a one2many) is not
    assert!(insert.contains("\"partner_country_code\""), "{insert}");
    assert!(!insert.contains("\"amount_total\""), "{insert}");
    create(
        &mut tx,
        "test_dep.line",
        &[&[("order_id", int(&o, 0)), ("price_unit", Value::Float(2.0))]],
    )
    .await;
    let insert = tx.captured().last().cloned().unwrap();
    // two many2one steps (order → partner → name), and a field of the record
    assert!(insert.contains("\"order_partner\""), "{insert}");
    assert!(insert.contains("\"price_subtotal\""), "{insert}");
    tx.capture_statements(false);
    let line = o.related(&mut tx, "line_ids").await.unwrap();
    assert_eq!(line.value(&mut tx, "order_partner").await.unwrap(), s("P"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// an implicitly precomputed stored compute is not protected during `create`, so an
/// inverse or an editable related write that rewrites its input marks it and the flush recomputes
/// it from the final input. Fails on the code before PC-1: there `z` keeps the value computed
/// from the empty `y` and the message keeps the old discussion name.
#[tokio::test]
async fn spec_comp_075_stored_compute_rewritten_inside_create_gets_the_flush_value() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_dep.inverse_input").unwrap();
    // warm the statements and the access state
    create(&mut tx, "test_dep.inverse_input", &[&[("y", s("w"))]]).await;
    create(&mut tx, "test_dep.inverse_input", &[&[("x", s("w"))]]).await;
    tx.flush().await.unwrap();

    // the inverse writes `y` after the INSERT: INSERT (with `z` from the empty `y`), then at the
    // flush one UPDATE of `y` and the recomputed `z`
    tx.capture_statements(true);
    let inversed = assert_query_count!(tx, >= 2, <= 2, {
        let r = model
            .create_values(&mut tx, &[map(&[("x", s("abc"))])])
            .await
            .unwrap();
        tx.flush().await.unwrap();
        r
    });
    let update = tx.captured().last().cloned().unwrap();
    tx.capture_statements(false);
    assert!(update.starts_with("UPDATE"), "{update}");
    assert!(update.contains("\"z\""), "z is recomputed: {update}");
    // no inverse: `z` is written once, in the INSERT, and nothing at the flush
    let plain = assert_query_count!(tx, >= 1, <= 1, {
        model
            .create_values(&mut tx, &[map(&[("y", s("ABC"))])])
            .await
            .unwrap()
    });
    assert_query_count!(tx, <= 0, { tx.flush().await.unwrap() });
    tx.invalidate_all().await.unwrap();
    // the inverse wrote the `y` the plain record was given, and `z` is what the plain path
    // computes from it
    assert_eq!(
        inversed.values(&mut tx, "y").await.unwrap(),
        plain.values(&mut tx, "y").await.unwrap()
    );
    assert_eq!(
        inversed.values(&mut tx, "z").await.unwrap(),
        plain.values(&mut tx, "z").await.unwrap()
    );

    // an editable related field given at create renames the discussion after the INSERT; the
    // message's stored name reads the discussion's name through `discussion_id`
    let d = create(&mut tx, "test_dep.discussion", &[&[("name", s("Old"))]]).await;
    tx.flush().await.unwrap();
    // INSERT of the message (its name from the cached discussion), then at the flush one UPDATE of
    // the discussion (name, message count) and one of the recomputed message name
    let renamed = assert_query_count!(tx, >= 3, <= 3, {
        let r = create(
            &mut tx,
            "test_dep.message",
            &[&[
                ("discussion_id", int(&d, 0)),
                ("author", s("A")),
                ("discussion_name", s("New")),
            ]],
        )
        .await;
        tx.flush().await.unwrap();
        r
    });
    let later = create(
        &mut tx,
        "test_dep.message",
        &[&[("discussion_id", int(&d, 0)), ("author", s("A"))]],
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    // the related write renamed the discussion (the value given at create), and the message's
    // stored name is what a message created on the renamed discussion computes
    assert_eq!(d.values(&mut tx, "name").await.unwrap(), [s("New")]);
    assert_eq!(
        renamed.values(&mut tx, "name").await.unwrap(),
        later.values(&mut tx, "name").await.unwrap()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_084_inverse_one2many_with_a_domain_prefetches_once() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let o = order(&mut tx, "o", &[(1.0, 1.0); 10]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let related = assert_query_count!(tx, >= 1, <= 2, {
        let lines = o.related(&mut tx, "line_ids").await.unwrap();
        lines.related(&mut tx, "related_order_id").await.unwrap()
    });
    assert_eq!(related, o);
    // the inverse one2many over the related many2one, with its domain
    let lines = o.related(&mut tx, "related_line_ids").await.unwrap();
    assert_eq!(lines.len(), 10);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_077_editable_related_field_readback_and_protection() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();

    // 1. Create target with initial value: target create override normalises to "NORM:init".
    let target = create(
        &mut tx,
        "test_dep.norm_target",
        &[&[("name", s("T1")), ("value", s("init"))]],
    )
    .await;
    assert_eq!(
        target.values(&mut tx, "value").await.unwrap(),
        [s("NORM:init")]
    );

    // 2. Create source pointing to target with editable non-stored related field:
    // Create passes related_value = "created_val".
    let source = create(
        &mut tx,
        "test_dep.norm_source",
        &[&[
            ("name", s("S1")),
            ("target_id", int(&target, 0)),
            ("related_value", s("created_val")),
        ]],
    )
    .await;
    // Same transaction: source reads back the raw assigned value (protected from recomputation).
    assert_eq!(
        source.values(&mut tx, "related_value").await.unwrap(),
        [s("created_val")],
        "source reads assigned value in same transaction after create"
    );
    // Target was written and normalised the value.
    assert_eq!(
        target.values(&mut tx, "value").await.unwrap(),
        [s("NORM:created_val")],
        "target normalised the value"
    );
    // Later read (fresh cache): source follows relation and reads normalised value.
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        source.values(&mut tx, "related_value").await.unwrap(),
        [s("NORM:created_val")],
        "source reads normalised value from target after cache clear"
    );

    // 3. Write on source: related_value = "written_val".
    write(&mut tx, &source, &[("related_value", s("written_val"))]).await;
    // Same transaction: source reads back raw written value.
    assert_eq!(
        source.values(&mut tx, "related_value").await.unwrap(),
        [s("written_val")],
        "source reads written value in same transaction"
    );
    // Target was updated and normalised.
    assert_eq!(
        target.values(&mut tx, "value").await.unwrap(),
        [s("NORM:written_val")],
        "target normalised on write"
    );
    // Later read (fresh transaction after commit).
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        source.values(&mut tx, "related_value").await.unwrap(),
        [s("NORM:written_val")],
        "source reads normalised value in fresh transaction"
    );

    // 4. Multi-record write: write across multiple sources.
    let t2 = create(
        &mut tx,
        "test_dep.norm_target",
        &[&[("name", s("T2")), ("value", s("init2"))]],
    )
    .await;
    let s2 = create(
        &mut tx,
        "test_dep.norm_source",
        &[&[
            ("name", s("S2")),
            ("target_id", int(&t2, 0)),
            ("related_value", s("init_s2")),
        ]],
    )
    .await;
    let multi_sources = source.union(&s2).unwrap();
    write(
        &mut tx,
        &multi_sources,
        &[("related_value", s("batch_val"))],
    )
    .await;
    // Same transaction: both sources read back "batch_val".
    assert_eq!(
        multi_sources
            .values(&mut tx, "related_value")
            .await
            .unwrap(),
        [s("batch_val"), s("batch_val")],
        "multi-record write reads back assigned value in same transaction"
    );
    // Targets were normalised.
    assert_eq!(
        target.values(&mut tx, "value").await.unwrap(),
        [s("NORM:batch_val")]
    );
    assert_eq!(
        t2.values(&mut tx, "value").await.unwrap(),
        [s("NORM:batch_val")]
    );
    // In fresh transaction, both reflect normalised values.
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        multi_sources
            .values(&mut tx, "related_value")
            .await
            .unwrap(),
        [s("NORM:batch_val"), s("NORM:batch_val")]
    );

    // 5. Stored editable related field: write and create do not panic on dirty cache invalidation.
    let s3 = create(
        &mut tx,
        "test_dep.norm_source",
        &[&[
            ("name", s("S3")),
            ("target_id", int(&target, 0)),
            ("stored_related_value", s("stored_init")),
        ]],
    )
    .await;
    assert_eq!(
        s3.values(&mut tx, "stored_related_value").await.unwrap(),
        [s("stored_init")],
        "stored editable related field reads back written value in same transaction"
    );
    assert_eq!(
        target.values(&mut tx, "value").await.unwrap(),
        [s("NORM:stored_init")]
    );
    // Write on stored editable related field.
    write(
        &mut tx,
        &s3,
        &[("stored_related_value", s("stored_update"))],
    )
    .await;
    assert_eq!(
        s3.values(&mut tx, "stored_related_value").await.unwrap(),
        [s("stored_update")]
    );
    assert_eq!(
        target.values(&mut tx, "value").await.unwrap(),
        [s("NORM:stored_update")]
    );
    // Flush and commit work cleanly.
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();

    // 6. New records (draft/onchange): writing an editable related field on a new record.
    let draft = tx
        .new_record(
            &tx.env().records("test_dep.norm_source").unwrap(),
            erp_sdk::RawVals::new()
                .set_cell("name", erp_sdk::Cell::Str("Draft".into()))
                .set_cell("target_id", erp_sdk::Cell::Id(target.rec_ids()[0].get())),
            None,
            None,
        )
        .await
        .unwrap();
    draft
        .write_values(
            &mut tx,
            &support::map(&[("related_value", s("draft_edit"))]),
        )
        .await
        .unwrap();
    assert_eq!(
        draft.values(&mut tx, "related_value").await.unwrap(),
        [s("draft_edit")],
        "draft record reads back assigned value"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
