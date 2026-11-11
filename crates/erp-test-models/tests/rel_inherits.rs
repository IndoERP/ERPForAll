//! `_inherits` at run time (CRUD §4.2 step 5, §4.4; MODEL-022, REL-080): parents created or
//! written through the child, inherited fields read on the parent.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::{Command, ErrorKind, RawVals, Records, Transaction, Value};
use support::map;

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

#[tokio::test]
async fn spec_crud_016_model_022_delegation_on_create() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    // (a) a unit, a box and a pallet are created and linked
    let pallet = create(
        &mut tx,
        "test_rel.pallet",
        &[&[
            ("name", Value::str("B")),
            ("field_in_box", Value::str("box")),
            ("field_in_pallet", Value::str("pallet")),
        ]],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let read = pallet
        .read(
            &mut tx,
            &["name", "field_in_box", "field_in_pallet", "state"],
        )
        .await
        .unwrap();
    assert_eq!(read[0]["name"], Value::str("B"));
    assert_eq!(read[0]["field_in_box"], Value::str("box"));
    assert_eq!(read[0]["field_in_pallet"], Value::str("pallet"));
    assert_eq!(read[0]["state"], Value::str("a"), "the unit's default");
    let boxes = pallet.related(&mut tx, "box_id").await.unwrap();
    let unit = boxes.related(&mut tx, "unit_id").await.unwrap();
    assert_eq!(unit.value(&mut tx, "name").await.unwrap(), Value::str("B"));
    assert_eq!(
        boxes.value(&mut tx, "field_in_box").await.unwrap(),
        Value::str("box")
    );
    // (b) with the grand-parent given: no new unit, defaults of its fields ignored
    let units = tx.env().records("test_rel.unit").unwrap();
    let u = create(&mut tx, "test_rel.unit", &[&[("name", Value::str("U"))]]).await;
    let before = units
        .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
        .await
        .unwrap();
    let pallets = tx.env().records("test_rel.pallet").unwrap().with_context([
        ("default_state", Value::str("b")),
        ("default_size", Value::Int(2)),
    ]);
    let p2 = pallets
        .create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("P")),
                ("unit_id", Value::Int(u.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    assert_eq!(
        units
            .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
            .await
            .unwrap(),
        before,
        "no new unit"
    );
    assert_eq!(u.value(&mut tx, "state").await.unwrap(), Value::str("a"));
    assert_eq!(u.value(&mut tx, "name").await.unwrap(), Value::str("P"));
    assert_eq!(
        p2.value(&mut tx, "size").await.unwrap(),
        Value::Int(2),
        "a box field"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_025_writes_on_delegated_fields_go_to_the_parent() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let pallet = create(&mut tx, "test_rel.pallet", &[&[("name", Value::str("A"))]]).await;
    pallet
        .set_value(&mut tx, "name", Value::str("C"))
        .await
        .unwrap();
    let unit = pallet
        .related(&mut tx, "box_id")
        .await
        .unwrap()
        .related(&mut tx, "unit_id")
        .await
        .unwrap();
    assert_eq!(unit.value(&mut tx, "name").await.unwrap(), Value::str("C"));
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        pallet.value(&mut tx, "name").await.unwrap(),
        Value::str("C")
    );
    // a readonly inherited field is written on the parent (readonly is a UI flag)
    let boxes = create(&mut tx, "test_rel.box", &[&[("name", Value::str("box"))]]).await;
    boxes
        .set_value(&mut tx, "readonly_name", Value::str("Superuser's box"))
        .await
        .unwrap();
    let u = boxes.related(&mut tx, "unit_id").await.unwrap();
    assert_eq!(
        u.value(&mut tx, "readonly_name").await.unwrap(),
        Value::str("Superuser's box")
    );
    // five boxes sharing a unit move to another one
    let foo = create(&mut tx, "test_rel.unit", &[&[("name", Value::str("foo"))]]).await;
    let bar = create(&mut tx, "test_rel.unit", &[&[("name", Value::str("bar"))]]).await;
    let rows: Vec<Vec<(&str, Value)>> = (0..5)
        .map(|_| vec![("unit_id", Value::Int(foo.ids()[0]))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let five = create(&mut tx, "test_rel.box", &refs).await;
    five.set_value(&mut tx, "unit_id", Value::Int(bar.ids()[0]))
        .await
        .unwrap();
    let names = five.values(&mut tx, "name").await.unwrap();
    assert!(names.iter().all(|n| *n == Value::str("bar")), "{names:?}");
    // the child's write_date changes when an inherited field is written
    tx.flush().await.unwrap();
    let before = boxes.value(&mut tx, "write_date").await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let boxes = tx
        .env()
        .records("test_rel.box")
        .unwrap()
        .browse(boxes.ids())
        .unwrap();
    boxes
        .set_value(&mut tx, "name", Value::str("renamed"))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_ne!(boxes.value(&mut tx, "write_date").await.unwrap(), before);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_080_inherited_one2many_write() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let b = create(&mut tx, "test_rel.box", &[&[("name", Value::str("b"))]]).await;
    let write = async |tx: &mut Transaction, command: Command| {
        b.write(tx, RawVals::new().set_commands("line_ids", [command]))
            .await
            .unwrap();
    };
    let line = |name: &str| {
        let mut v = RawVals::new();
        v.insert("name".into(), erp_sdk::Cell::Str(name.into()));
        v
    };
    write(&mut tx, Command::create(line("Line 1"))).await;
    tx.invalidate_all().await.unwrap();
    write(&mut tx, Command::create(line("Line 2"))).await;
    let lines = b.related(&mut tx, "line_ids").await.unwrap();
    let unit_lines = b
        .related(&mut tx, "unit_id")
        .await
        .unwrap()
        .related(&mut tx, "line_ids")
        .await
        .unwrap();
    assert_eq!(lines, unit_lines);
    assert!(lines.ids().iter().all(|i| *i > 0), "real ids");
    assert_eq!(
        lines.values(&mut tx, "name").await.unwrap(),
        [Value::str("Line 1"), Value::str("Line 2")]
    );
    write(
        &mut tx,
        Command::update(lines.rec_ids()[0], line("First line")),
    )
    .await;
    let lines = b.related(&mut tx, "line_ids").await.unwrap();
    assert_eq!(
        lines.values(&mut tx, "name").await.unwrap(),
        [Value::str("First line"), Value::str("Line 2")]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// is written, so a parent changed in the same write is the one checked)
#[tokio::test]
async fn spec_crud_026_constraints_across_delegation_on_write() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let boxes = tx.env().records("test_rel.another_box").unwrap();
    let err = tx
        .savepoint(async |tx| {
            let vals = map(&[("val1", Value::Int(1)), ("val2", Value::Int(2))]);
            boxes.create_values(tx, &[vals]).await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "on create: {err}");
    let write = async |tx: &mut Transaction, b: &Records, vals: &[(&str, Value)]| {
        let vals = map(vals);
        tx.savepoint(async |tx| b.write_values(tx, &vals).await)
            .await
    };
    let b = create(
        &mut tx,
        "test_rel.another_box",
        &[&[("val1", Value::Int(1)), ("val2", Value::Int(1))]],
    )
    .await;
    let err = write(&mut tx, &b, &[("val2", Value::Int(2))])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    write(
        &mut tx,
        &b,
        &[("val1", Value::Int(2)), ("val2", Value::Int(2))],
    )
    .await
    .unwrap();
    // the parent changes in the same write
    let units = create(
        &mut tx,
        "test_rel.another_unit",
        &[
            &[("val1", Value::Int(1))],
            &[("val1", Value::Int(2))],
            &[("val1", Value::Int(3))],
            &[("val1", Value::Int(5))],
            &[("val1", Value::Int(7))],
        ],
    )
    .await;
    let u = |i: usize| ("another_unit_id", Value::Int(units.ids()[i]));
    let b = create(
        &mut tx,
        "test_rel.another_box",
        &[&[u(0), ("val2", Value::Int(1))]],
    )
    .await;
    let cases: Vec<(Vec<(&str, Value)>, bool)> = vec![
        (vec![u(1), ("val2", Value::Int(2))], true),
        (
            vec![u(2), ("val1", Value::Int(4)), ("val2", Value::Int(4))],
            true,
        ),
        (vec![u(3), ("val2", Value::Int(6))], false),
        (
            vec![u(4), ("val1", Value::Int(8)), ("val2", Value::Int(7))],
            false,
        ),
    ];
    for (vals, ok) in &cases {
        let result = write(&mut tx, &b, vals).await;
        assert_eq!(result.is_ok(), *ok, "{vals:?}: {result:?}");
        if let Err(err) = result {
            assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
        }
    }
    assert_eq!(b.value(&mut tx, "val1").await.unwrap(), Value::Int(4));
    drop(tx);
    h.cleanup().await.unwrap();
}
