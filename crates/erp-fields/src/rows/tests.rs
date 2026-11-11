#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use rust_decimal::Decimal;

use super::*;
use crate::{Date, DateRange, DateTime, ErrorKind, RangeValue, RecId, ValueMap};

fn keys(names: &[&'static str]) -> Vec<Key> {
    names.iter().map(|n| Key::from(*n)).collect()
}

fn map(pairs: &[(&str, Value)]) -> ValueMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn hash_of(v: &Value) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn programming(result: Result<impl std::fmt::Debug>) -> String {
    let error = result.expect_err("an error");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    error.message().to_owned()
}

/// Order lines: id, code (text), qty, price_unit, price_subtotal.
fn lines(n: usize) -> Rows {
    let layout = Layout::for_model(
        "bench.order.line",
        keys(&["id", "code", "qty", "price_unit", "price_subtotal"]),
    )
    .unwrap();
    let mut b = RowsBuilder::new(layout, n).unwrap();
    b.column("id", Column::new(ColumnData::Int((1..=n as i64).collect())))
        .unwrap();
    b.column(
        "code",
        Column::new(ColumnData::Str(
            (0..n).map(|i| Some(Arc::from(format!("L{i}")))).collect(),
        )),
    )
    .unwrap();
    for (key, factor) in [("qty", 1.0), ("price_unit", 1.5), ("price_subtotal", 2.5)] {
        b.column(
            key,
            Column::new(ColumnData::Float(
                (0..n).map(|i| i as f64 * factor).collect(),
            )),
        )
        .unwrap();
    }
    b.finish().unwrap()
}

// ---------------------------------------------------------------- layout

#[test]
fn a_layout_keeps_key_order_and_escapes_keys_once() {
    let layout = Layout::new([
        Key::from("id"),
        Key::from("naïve \"key\""),
        Key::from(String::from("added")),
    ])
    .unwrap();
    assert_eq!(
        layout.keys().collect::<Vec<_>>(),
        ["id", "naïve \"key\"", "added"]
    );
    assert_eq!(layout.position("added"), Some(2));
    assert_eq!(layout.position("missing"), None);
    let mut expected = Vec::new();
    crate::json::write_string(&mut expected, "naïve \"key\"");
    expected.push(b':');
    assert_eq!(layout.json_key(1), Some(expected.as_slice()));
    assert_eq!(layout.json_key(3), None);
    assert_eq!(layout.model(), None);
}

#[test]
fn duplicate_keys_are_refused_in_short_and_wide_layouts() {
    assert_eq!(
        programming(Layout::for_model(
            "res.partner",
            keys(&["id", "name", "id"])
        )),
        "duplicate key 'id' in the layout of res.partner rows"
    );
    let mut wide: Vec<Key> = (0..40).map(|i| Key::from(format!("k{i}"))).collect();
    wide.push(Key::from("k7"));
    assert_eq!(
        programming(Layout::new(wide)),
        "duplicate key 'k7' in a row layout"
    );
}

#[test]
fn a_500_key_layout_finds_every_key() {
    let layout = Layout::new((0..500).map(|i| Key::from(format!("field_{i}")))).unwrap();
    for i in [0, 15, 16, 17, 250, 499] {
        assert_eq!(layout.position(&format!("field_{i}")), Some(i));
    }
    assert_eq!(layout.position("field_500"), None);
    // the error lists 20 keys and says how many more there are
    let rows = Rows::empty(layout);
    let message = programming(rows.get_opt(0, "nope"));
    assert!(
        message.starts_with("row has no key 'nope' (keys: field_0, field_1,"),
        "{message}"
    );
    assert!(message.ends_with("field_19 … and 480 more)"), "{message}");
}

#[test]
fn a_layout_refuses_more_than_max_keys() {
    let too_many = (0..=MAX_KEYS).map(|i| Key::from(format!("k{i}")));
    assert_eq!(
        programming(Layout::new(too_many)),
        format!(
            "a row layout holds at most {MAX_KEYS} keys, not {}",
            MAX_KEYS + 1
        )
    );
}

// ---------------------------------------------------------------- builder

#[test]
fn the_builder_checks_every_column() {
    let layout = Layout::for_model("res.partner", keys(&["id", "name"])).unwrap();
    let int = |n: usize| Column::new(ColumnData::Int(vec![1; n]));

    let mut b = RowsBuilder::new(Arc::clone(&layout), 2).unwrap();
    b.column("id", int(2)).unwrap();
    assert_eq!(
        programming(b.finish()),
        "column 'name' of res.partner rows was not given"
    );

    let mut b = RowsBuilder::new(Arc::clone(&layout), 2).unwrap();
    b.column("id", int(2)).unwrap();
    assert_eq!(
        programming(b.column("id", int(2)).map(|_| ())),
        "column 'id' of res.partner rows is given twice"
    );
    assert_eq!(
        programming(b.column("nam", int(2)).map(|_| ())),
        "res.partner row has no key 'nam' (keys: id, name)"
    );
    assert_eq!(
        programming(b.column_at(2, int(2)).map(|_| ())),
        "column position 2 is past the 2 keys of res.partner rows"
    );

    let mut b = RowsBuilder::new(Arc::clone(&layout), 2).unwrap();
    b.column("id", int(2)).unwrap();
    b.column("name", int(3)).unwrap();
    assert_eq!(
        programming(b.finish()),
        "column 'name' of res.partner rows does not hold 2 rows of integers"
    );

    let mut b = RowsBuilder::new(Arc::clone(&layout), 2).unwrap();
    b.column("id", int(2)).unwrap();
    b.column(
        "name",
        Column::new(ColumnData::Str(vec![None, None])).with_false(Bitmap::from_rows([1]).unwrap()),
    )
    .unwrap();
    assert_eq!(
        programming(b.finish()),
        "column 'name' of res.partner rows holds text: False is None (or NO_TARGET) there, \
         not a falsy bit"
    );

    let mut b = RowsBuilder::new(Arc::clone(&layout), 2).unwrap();
    b.column("id", int(2).with_absent(Bitmap::from_rows([2]).unwrap()))
        .unwrap();
    b.column("name", int(2)).unwrap();
    assert_eq!(
        programming(b.finish()),
        "column 'id' of res.partner rows marks rows past its 2 rows"
    );

    assert_eq!(
        programming(RowsBuilder::new(layout, MAX_ROWS + 1)),
        format!(
            "a row batch holds at most {MAX_ROWS} rows, not {}",
            MAX_ROWS + 1
        )
    );
}

#[test]
fn an_empty_batch_has_no_rows() {
    let rows = lines(0);
    assert!(rows.is_empty());
    assert_eq!(rows.indices(), 0..0);
    assert_eq!(rows.iter().count(), 0);
    assert_eq!(
        programming(rows.get(0, "id")),
        "row 0 of bench.order.line rows is out of range (0 rows)"
    );
    assert!(rows.clone().into_values().is_empty());
    assert_eq!(Value::from(rows.clone()), Value::list([]));
    assert!(!Value::from(rows).is_truthy());
}

// ---------------------------------------------------------------- access

#[test]
fn every_column_kind_reads_its_values_and_false() {
    let layout = Layout::new(keys(&[
        "flag", "int", "float", "dec", "text", "raw", "day", "at", "doc", "span", "ids", "label",
    ]))
    .unwrap();
    let date = Date::from_ymd(2026, 10, 19).unwrap();
    let at = DateTime::from_parts(date, 13, 5, 9, 0).unwrap();
    let range = RangeValue::Date(DateRange::new(Some(date), None));
    let doc = Arc::new(serde_json::json!({"a": [1, null]}));
    let ids: Arc<[RecId]> = [RecId::from_real(4).unwrap(), RecId::from_real(9).unwrap()].into();
    let falsy = || Bitmap::from_rows([1]).unwrap();
    let mut flags = Bitmap::new();
    flags.set(0, true).unwrap();
    let mut b = RowsBuilder::new(layout, 2).unwrap();
    b.column(
        "flag",
        Column::new(ColumnData::Bool(flags)).with_false(falsy()),
    )
    .unwrap();
    b.column(
        "int",
        Column::new(ColumnData::Int(vec![-7, 0])).with_false(falsy()),
    )
    .unwrap();
    b.column(
        "float",
        Column::new(ColumnData::Float(vec![1.5, 0.0])).with_false(falsy()),
    )
    .unwrap();
    b.column(
        "dec",
        Column::new(ColumnData::Decimal(vec![
            Decimal::new(1250, 2),
            Decimal::ZERO,
        ]))
        .with_false(falsy()),
    )
    .unwrap();
    b.column(
        "text",
        Column::new(ColumnData::Str(vec![Some("é".into()), None])),
    )
    .unwrap();
    b.column(
        "raw",
        Column::new(ColumnData::Bytes(vec![
            Some(Arc::from(&b"x\x01"[..])),
            None,
        ])),
    )
    .unwrap();
    b.column(
        "day",
        Column::new(ColumnData::Date(vec![date, date])).with_false(falsy()),
    )
    .unwrap();
    b.column(
        "at",
        Column::new(ColumnData::DateTime(vec![at, at])).with_false(falsy()),
    )
    .unwrap();
    b.column(
        "doc",
        Column::new(ColumnData::Json(vec![Some(Arc::clone(&doc)), None])),
    )
    .unwrap();
    b.column(
        "span",
        Column::new(ColumnData::Range(vec![Some(range), None])),
    )
    .unwrap();
    b.column(
        "ids",
        Column::new(ColumnData::Ids(vec![Some(Arc::clone(&ids)), None])),
    )
    .unwrap();
    b.column(
        "label",
        Column::new(ColumnData::Label {
            ids: vec![3, 0],
            names: vec![Some("Azure".into()), None],
        })
        .with_false(falsy()),
    )
    .unwrap();
    let rows = b.finish().unwrap();

    let first = [
        ("flag", Value::Bool(true)),
        ("int", Value::Int(-7)),
        ("float", Value::Float(1.5)),
        ("dec", Value::Decimal(Decimal::new(1250, 2))),
        ("text", Value::str("é")),
        ("raw", Value::Bytes(Arc::from(&b"x\x01"[..]))),
        ("day", Value::Date(date)),
        ("at", Value::DateTime(at)),
        ("doc", Value::Json(doc)),
        ("span", Value::Range(range)),
        ("ids", Value::list([Value::Int(4), Value::Int(9)])),
        ("label", Value::list([Value::Int(3), Value::str("Azure")])),
    ];
    for (key, expected) in &first {
        assert_eq!(&rows.get(0, key).unwrap(), expected, "{key}");
        assert_eq!(rows.get(1, key).unwrap(), Value::False, "{key}");
    }
    assert_eq!(rows.str(0, "text").unwrap(), Some("é"));
    assert_eq!(rows.str(1, "text").unwrap(), None);
    assert_eq!(rows.i64(0, "int").unwrap(), Some(-7));
    assert_eq!(rows.i64(1, "int").unwrap(), None);
    assert_eq!(rows.f64(0, "float").unwrap(), Some(1.5));
    assert_eq!(rows.f64(1, "float").unwrap(), None);
    assert_eq!(rows.bool(0, "flag").unwrap(), Some(true));
    assert_eq!(rows.bool(1, "flag").unwrap(), None);
    // a row reads as the map of the same entries
    assert_eq!(
        Value::Row(rows.row(0).unwrap()),
        Value::Map(Arc::new(map(&first)))
    );
}

#[test]
fn errors_name_the_key_the_row_and_the_model() {
    let rows = lines(10);
    assert_eq!(
        programming(rows.get(0, "qtyy")),
        "bench.order.line row has no key 'qtyy' (keys: id, code, qty, price_unit, price_subtotal)"
    );
    assert_eq!(
        programming(rows.get(12, "qty")),
        "row 12 of bench.order.line rows is out of range (10 rows)"
    );
    assert_eq!(
        programming(rows.f64(0, "code")),
        "key 'code' of bench.order.line rows holds text, not a float"
    );
    assert_eq!(
        programming(rows.str(0, "qty")),
        "key 'qty' of bench.order.line rows holds floats, not text"
    );
    assert_eq!(
        programming(rows.i64(0, "qty")),
        "key 'qty' of bench.order.line rows holds floats, not an integer"
    );
    assert_eq!(
        programming(rows.bool(0, "qty")),
        "key 'qty' of bench.order.line rows holds floats, not a boolean"
    );
    assert_eq!(
        programming(rows.children(0, "qty")),
        "key 'qty' of bench.order.line rows holds floats, not child rows"
    );
    assert_eq!(
        programming(rows.row(10)),
        "row 10 of bench.order.line rows is out of range (10 rows)"
    );
    // without a model
    let plain = Rows::from_maps([map(&[("id", Value::Int(1))])]).unwrap();
    assert_eq!(
        programming(plain.get(3, "id")),
        "row 3 is out of range (1 rows)"
    );
    assert_eq!(
        programming(plain.get(0, "x")),
        "row has no key 'x' (keys: id)"
    );
}

#[test]
fn an_absent_key_is_an_error_for_get_and_none_for_get_opt() {
    let mut rows = lines(4);
    rows.insert_key("running_balance").unwrap();
    rows.set(1, "running_balance", 12.5).unwrap();
    assert_eq!(
        programming(rows.get(3, "running_balance")),
        "key 'running_balance' is absent in row 3 of bench.order.line rows"
    );
    assert_eq!(
        programming(rows.f64(3, "running_balance")),
        "key 'running_balance' is absent in row 3 of bench.order.line rows"
    );
    assert_eq!(rows.get_opt(3, "running_balance").unwrap(), None);
    assert_eq!(
        rows.get_opt(1, "running_balance").unwrap(),
        Some(Value::Float(12.5))
    );
    // get_opt still refuses a key the layout lacks
    assert_eq!(
        programming(rows.get_opt(3, "running_balanc")),
        "bench.order.line row has no key 'running_balanc' (keys: id, code, qty, price_unit, \
         price_subtotal, running_balance)"
    );
    let row = rows.row(3).unwrap();
    assert!(!row.contains("running_balance"));
    assert!(rows.row(1).unwrap().contains("running_balance"));
    assert_eq!(row.len(), 5);
    assert_eq!(rows.row(1).unwrap().len(), 6);
}

#[test]
fn dynamic_columns_read_typed_values_and_refuse_others() {
    let rows = Rows::from_maps([
        map(&[("x", Value::Int(1))]),
        map(&[("x", Value::str("a"))]),
        map(&[("x", Value::False)]),
    ])
    .unwrap();
    assert!(matches!(rows.column(0).unwrap().data(), ColumnData::Dyn(_)));
    assert_eq!(rows.i64(0, "x").unwrap(), Some(1));
    assert_eq!(rows.str(1, "x").unwrap(), Some("a"));
    assert_eq!(rows.i64(2, "x").unwrap(), None);
    assert_eq!(
        programming(rows.i64(1, "x")),
        "key 'x' in row 1 holds 'a', not an integer"
    );
    assert_eq!(
        programming(rows.f64(0, "x")),
        "key 'x' in row 0 holds 1, not a float"
    );
    assert_eq!(
        programming(rows.bool(0, "x")),
        "key 'x' in row 0 holds 1, not a boolean"
    );
    assert_eq!(
        programming(rows.str(0, "x")),
        "key 'x' in row 0 holds 1, not text"
    );
}

// ---------------------------------------------------------------- children

/// Partners (distinct targets), tags (shared many2many targets) and lines (one2many) under
/// three orders, the BENCH-002 shape.
fn orders() -> Rows {
    let two = |model: &'static str, ids: &[i64], codes: &[&str]| {
        let layout = Layout::for_model(model, keys(&["id", "code"])).unwrap();
        let mut b = RowsBuilder::new(layout, ids.len()).unwrap();
        b.column("id", Column::new(ColumnData::Int(ids.to_vec())))
            .unwrap();
        b.column(
            "code",
            Column::new(ColumnData::Str(
                codes.iter().map(|c| Some(Arc::from(*c))).collect(),
            )),
        )
        .unwrap();
        b.finish().unwrap()
    };
    let partners = two("res.partner", &[10, 11], &["P10", "P11"]);
    let tags = two("bench.tag", &[20, 21, 22], &["T20", "T21", "T22"]);
    let layout = Layout::for_model(
        "bench.order",
        keys(&["id", "partner_id", "line_ids", "tag_ids"]),
    )
    .unwrap();
    let mut b = RowsBuilder::new(layout, 3).unwrap();
    b.column("id", Column::new(ColumnData::Int(vec![1, 2, 3])))
        .unwrap();
    b.column(
        "partner_id",
        Column::new(ColumnData::One(
            Targets::new(partners, vec![1, NO_TARGET, 1]).unwrap(),
        )),
    )
    .unwrap();
    b.column(
        "line_ids",
        Column::new(ColumnData::Many(
            Children::contiguous(lines(5), vec![0, 2, 2, 5]).unwrap(),
        )),
    )
    .unwrap();
    b.column(
        "tag_ids",
        Column::new(ColumnData::Many(
            Children::indexed(tags, vec![0, 2, 3, 4], vec![2, 0, 0, 2]).unwrap(),
        )),
    )
    .unwrap();
    b.finish().unwrap()
}

fn line_map(i: usize) -> Value {
    let i64i = i as i64;
    Value::Map(Arc::new(map(&[
        ("id", Value::Int(i64i + 1)),
        ("code", Value::str(format!("L{i}"))),
        ("qty", Value::Float(i as f64)),
        ("price_unit", Value::Float(i as f64 * 1.5)),
        ("price_subtotal", Value::Float(i as f64 * 2.5)),
    ])))
}

fn two_map(id: i64, code: &str) -> Value {
    Value::Map(Arc::new(map(&[
        ("id", Value::Int(id)),
        ("code", Value::str(code)),
    ])))
}

#[test]
fn children_read_by_position_and_equal_the_copied_maps() {
    let rows = orders();
    assert_eq!(rows.get(0, "partner_id").unwrap(), two_map(11, "P11"));
    assert_eq!(rows.get(1, "partner_id").unwrap(), Value::False);
    assert_eq!(
        rows.get(0, "line_ids").unwrap(),
        Value::list([line_map(0), line_map(1)])
    );
    assert_eq!(rows.get(1, "line_ids").unwrap(), Value::list([]));
    assert_eq!(
        rows.get(2, "line_ids").unwrap(),
        Value::list([line_map(2), line_map(3), line_map(4)])
    );
    assert_eq!(
        rows.get(0, "tag_ids").unwrap(),
        Value::list([two_map(22, "T22"), two_map(20, "T20")])
    );
    // the shared tag T20 and partner P11 are one child row each, referenced twice
    assert_eq!(
        rows.child_positions(1, "tag_ids")
            .unwrap()
            .collect::<Vec<_>>(),
        [0]
    );
    assert_eq!(
        rows.child_positions(0, "tag_ids")
            .unwrap()
            .collect::<Vec<_>>(),
        [2, 0]
    );
    assert_eq!(
        rows.child_positions(0, "partner_id")
            .unwrap()
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(
        rows.child_positions(2, "partner_id")
            .unwrap()
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(rows.child_positions(1, "partner_id").unwrap().count(), 0);
    assert_eq!(
        rows.child_positions(2, "line_ids")
            .unwrap()
            .collect::<Vec<_>>(),
        [2, 3, 4]
    );
    let children = rows.children(2, "line_ids").unwrap();
    assert_eq!(children.len(), 3);
    assert_eq!(
        children.get(1).unwrap().get("code").unwrap(),
        Value::str("L3")
    );
    assert_eq!(
        programming(children.get(3)),
        "row 3 is out of range (3 rows)"
    );
    assert_eq!(
        programming(rows.children(0, "partner_id")),
        "key 'partner_id' of bench.order rows holds a child row, not child rows"
    );
    match rows.get(0, "partner_id").unwrap() {
        Value::Row(row) => {
            assert_eq!(row.rows().model(), Some("res.partner"));
            assert_eq!(row.index(), 1);
        }
        other => panic!("not a row handle: {other:?}"),
    }
}

#[test]
fn child_levels_refuse_positions_past_their_batch() {
    assert_eq!(
        programming(Targets::new(lines(2), vec![0, 2])),
        "parent row 1 points at child row 2, past the 2 child rows"
    );
    assert_eq!(
        programming(Children::contiguous(lines(2), vec![0, 3])),
        "child offsets end at 3, past the 2 child positions"
    );
    assert_eq!(
        programming(Children::contiguous(lines(2), vec![0, 2, 1])),
        "child offsets decrease at parent row 1"
    );
    assert_eq!(
        programming(Children::contiguous(lines(2), vec![])),
        "child offsets need one entry more than there are parent rows; got none"
    );
    assert_eq!(
        programming(Children::indexed(lines(2), vec![0, 1], vec![5])),
        "child position 0 points at child row 5, past the 2 child rows"
    );
    assert_eq!(
        programming(Children::indexed(lines(2), vec![0, 2], vec![1])),
        "child offsets end at 2, past the 1 child positions"
    );
    // a child level for 2 parents under a batch of 3 rows
    let layout = Layout::new(keys(&["line_ids"])).unwrap();
    let mut b = RowsBuilder::new(layout, 3).unwrap();
    b.column(
        "line_ids",
        Column::new(ColumnData::Many(
            Children::contiguous(lines(2), vec![0, 1, 2]).unwrap(),
        )),
    )
    .unwrap();
    assert_eq!(
        programming(b.finish()),
        "column 'line_ids' does not hold 3 rows of child rows"
    );
}

// ---------------------------------------------------------------- mutation

#[test]
fn setting_false_keeps_the_type_and_another_type_retypes_once() {
    let mut rows = lines(3);
    rows.set(1, "qty", Value::False).unwrap();
    assert!(matches!(
        rows.column(2).unwrap().data(),
        ColumnData::Float(_)
    ));
    assert_eq!(rows.get(1, "qty").unwrap(), Value::False);
    rows.set(1, "qty", 4.0).unwrap();
    assert_eq!(rows.f64(1, "qty").unwrap(), Some(4.0));
    rows.set(2, "code", Value::False).unwrap();
    assert_eq!(rows.str(2, "code").unwrap(), None);
    // a float rendered as "HH:MM" text (planning's template override): the column turns dynamic
    rows.set(0, "qty", "00:30").unwrap();
    assert!(matches!(rows.column(2).unwrap().data(), ColumnData::Dyn(_)));
    assert_eq!(rows.get(0, "qty").unwrap(), Value::str("00:30"));
    assert_eq!(rows.get(1, "qty").unwrap(), Value::Float(4.0));
    assert_eq!(rows.get(2, "qty").unwrap(), Value::Float(2.0));
    assert_eq!(
        programming(rows.set(3, "qty", 1.0)),
        "row 3 of bench.order.line rows is out of range (3 rows)"
    );
    assert_eq!(
        programming(rows.set(0, "qtyy", 1.0)),
        "bench.order.line row has no key 'qtyy' (keys: id, code, qty, price_unit, price_subtotal)"
    );
}

#[test]
fn retyping_a_child_column_keeps_its_children() {
    let mut rows = orders();
    let before = rows.get(2, "line_ids").unwrap();
    rows.set(0, "line_ids", Value::str("hidden")).unwrap();
    assert_eq!(rows.get(0, "line_ids").unwrap(), Value::str("hidden"));
    assert_eq!(rows.get(2, "line_ids").unwrap(), before);
    rows.set(0, "partner_id", Value::Int(5)).unwrap();
    assert_eq!(rows.get(0, "partner_id").unwrap(), Value::Int(5));
    assert_eq!(rows.get(2, "partner_id").unwrap(), two_map(11, "P11"));
    assert_eq!(rows.get(1, "partner_id").unwrap(), Value::False);
}

#[test]
fn keys_are_added_and_removed_once_per_batch() {
    let mut rows = lines(3);
    rows.insert_key(String::from("help")).unwrap();
    assert_eq!(
        programming(rows.insert_key("help")),
        "key 'help' already exists in bench.order.line rows"
    );
    rows.set(2, "help", "Read me").unwrap();
    assert_eq!(rows.get_opt(0, "help").unwrap(), None);
    assert_eq!(rows.get(2, "help").unwrap(), Value::str("Read me"));
    rows.remove_key("price_unit").unwrap();
    assert_eq!(
        rows.keys().collect::<Vec<_>>(),
        ["id", "code", "qty", "price_subtotal", "help"]
    );
    assert_eq!(rows.get(1, "price_subtotal").unwrap(), Value::Float(2.5));
    assert_eq!(
        programming(rows.remove_key("price_unit")),
        "bench.order.line row has no key 'price_unit' (keys: id, code, qty, price_subtotal, help)"
    );
    // the id key removed, as onchange's diff does
    rows.remove_key("id").unwrap();
    assert_eq!(
        rows.row(0).unwrap().keys().collect::<Vec<_>>(),
        ["code", "qty", "price_subtotal"]
    );
}

#[test]
fn mutation_copies_a_batch_a_handle_still_holds() {
    let mut rows = lines(2);
    let held = rows.row(0).unwrap();
    let before = rows.0.columns.as_ptr();
    rows.set(0, "qty", 9.0).unwrap();
    assert_ne!(
        rows.0.columns.as_ptr(),
        before,
        "the shared batch was copied"
    );
    assert_eq!(held.get("qty").unwrap(), Value::Float(0.0));
    assert_eq!(rows.f64(0, "qty").unwrap(), Some(9.0));
    drop(held);
    // the sole owner mutates in place
    let before = rows.0.columns.as_ptr();
    rows.set(1, "qty", 8.0).unwrap();
    assert_eq!(rows.0.columns.as_ptr(), before);
}

#[test]
fn children_mut_changes_the_child_level_by_key() {
    let mut rows = orders();
    let held = rows.get(0, "line_ids").unwrap();
    {
        let mut level = rows.children_mut("line_ids").unwrap();
        assert_eq!(level.len(), 5);
        level.set(0, "code", "changed").unwrap();
        level.insert_key("note").unwrap();
        level.remove_key("price_unit").unwrap();
        assert_eq!(
            programming(level.children_mut("code").map(|_| ())),
            "key 'code' of bench.order.line rows holds text, not child rows"
        );
    }
    let children = rows.children(0, "line_ids").unwrap();
    assert_eq!(
        children.get(0).unwrap().get("code").unwrap(),
        Value::str("changed")
    );
    assert!(!children.get(0).unwrap().contains("price_unit"));
    // the handle taken before still sees the old child level
    assert_eq!(held, Value::list([line_map(0), line_map(1)]));
    rows.children_mut("partner_id")
        .unwrap()
        .set(1, "code", "P-ELEVEN")
        .unwrap();
    assert_eq!(rows.get(2, "partner_id").unwrap(), two_map(11, "P-ELEVEN"));
    assert_eq!(
        programming(rows.children_mut("id").map(|_| ())),
        "key 'id' of bench.order rows holds integers, not child rows"
    );
}

// ---------------------------------------------------------------- from_maps

#[test]
fn from_maps_takes_the_union_of_keys_and_infers_typed_columns() {
    let maps = vec![
        map(&[
            ("id", Value::Int(1)),
            ("amount", Value::Float(2.5)),
            ("note", Value::False),
        ]),
        map(&[
            ("id", Value::Int(2)),
            ("amount", Value::False),
            ("extra", Value::str("x")),
        ]),
        map(&[("id", Value::Int(2)), ("amount", Value::Float(0.0))]),
    ];
    let rows = Rows::from_maps(maps.clone()).unwrap();
    assert_eq!(
        rows.keys().collect::<Vec<_>>(),
        ["id", "amount", "note", "extra"]
    );
    assert!(matches!(rows.column(0).unwrap().data(), ColumnData::Int(_)));
    assert!(matches!(
        rows.column(1).unwrap().data(),
        ColumnData::Float(_)
    ));
    assert!(matches!(rows.column(2).unwrap().data(), ColumnData::Dyn(_)));
    assert!(matches!(rows.column(3).unwrap().data(), ColumnData::Str(_)));
    // round trip: each row equals and converts back to its map (duplicate ids are two rows)
    for (i, m) in maps.iter().enumerate() {
        let row = rows.row(i).unwrap();
        assert_eq!(&row.to_map().unwrap(), m);
        assert_eq!(Value::Row(row), Value::Map(Arc::new(m.clone())));
    }
    assert_eq!(rows.get_opt(2, "note").unwrap(), None);
    assert!(Rows::from_maps(Vec::<ValueMap>::new()).unwrap().is_empty());
}

#[test]
fn from_maps_keeps_every_scalar_type_typed() {
    let date = Date::from_ymd(2024, 2, 29).unwrap();
    let samples = [
        Value::Bool(true),
        Value::Int(i64::MIN),
        Value::Float(-0.5),
        Value::Decimal(Decimal::new(-12345, 3)),
        Value::str("ü\n"),
        Value::Bytes(Arc::from(&b"\x00\xff"[..])),
        Value::Date(date),
        Value::DateTime(date.at_midnight()),
        Value::Json(Arc::new(serde_json::json!([1]))),
        Value::Range(RangeValue::Date(DateRange::new(None, Some(date)))),
    ];
    for sample in samples {
        let maps = [
            map(&[("v", sample.clone())]),
            map(&[("v", Value::False)]),
            map(&[]),
        ];
        let rows = Rows::from_maps(maps).unwrap();
        assert!(
            !matches!(rows.column(0).unwrap().data(), ColumnData::Dyn(_)),
            "{sample} should get a typed column"
        );
        assert_eq!(rows.get(0, "v").unwrap(), sample);
        assert_eq!(rows.get(1, "v").unwrap(), Value::False);
        assert_eq!(rows.get_opt(2, "v").unwrap(), None);
    }
}

// ---------------------------------------------------------------- Value integration

#[test]
fn row_and_rows_values_compare_hash_and_print_like_maps_and_lists() {
    let rows = orders();
    let row = Value::Row(rows.row(0).unwrap());
    let as_map = Value::Map(Arc::new(rows.row(0).unwrap().to_map().unwrap()));
    assert_eq!(row, as_map);
    assert_eq!(as_map, row);
    assert_eq!(hash_of(&row), hash_of(&as_map));
    let children = rows.get(2, "line_ids").unwrap();
    let as_list = Value::list([line_map(2), line_map(3), line_map(4)]);
    assert_eq!(children, as_list);
    assert_eq!(as_list, children);
    assert_eq!(hash_of(&children), hash_of(&as_list));
    assert_ne!(rows.get(0, "line_ids").unwrap(), as_list);
    assert_eq!(row.to_string(), as_map.to_string());
    assert_eq!(children.to_string(), as_list.to_string());
    assert_eq!(
        Value::Row(lines(1).row(0).unwrap()).to_string(),
        "{'id': 1, 'code': 'L0', 'qty': 0.0, 'price_unit': 0.0, 'price_subtotal': 0.0}"
    );
    assert!(row.is_truthy());
    assert!(!rows.get(1, "line_ids").unwrap().is_truthy());
    // a row is not equal to a map with one entry more or less
    let mut bigger = rows.row(0).unwrap().to_map().unwrap();
    bigger.insert("x".to_owned(), Value::False);
    assert_ne!(row, Value::Map(Arc::new(bigger)));
    // two handles to equal rows of different batches are equal
    assert_eq!(
        Value::Row(lines(3).row(1).unwrap()),
        Value::Row(lines(2).row(1).unwrap())
    );
    assert_eq!(Value::from(lines(2)), Value::from(lines(2)));
    assert_eq!(lines(2), lines(2));
    assert_ne!(lines(2), lines(3));
}

#[test]
fn the_dict_view_reads_maps_and_rows_alike() {
    let rows = lines(1);
    let row = Value::Row(rows.row(0).unwrap());
    let as_map = Value::Map(Arc::new(rows.row(0).unwrap().to_map().unwrap()));
    for value in [&row, &as_map] {
        let dict = value.dict().unwrap();
        assert_eq!(dict.get("code").unwrap(), Value::str("L0"));
        assert_eq!(dict.get_opt("code").unwrap(), Some(Value::str("L0")));
        assert!(dict.contains("qty"));
        assert!(!dict.contains("nope"));
        assert_eq!(dict.len(), 5);
        assert!(!dict.is_empty());
        assert_eq!(
            dict.keys().collect::<Vec<_>>(),
            ["id", "code", "qty", "price_unit", "price_subtotal"]
        );
        let entries: Vec<(&str, Value)> = dict.iter().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[1], ("code", Value::str("L0")));
    }
    assert_eq!(
        programming(as_map.dict().unwrap().get("nope")),
        "dictionary has no key 'nope' (keys: id, code, qty, price_unit, price_subtotal)"
    );
    assert_eq!(as_map.dict().unwrap().get_opt("nope").unwrap(), None);
    assert_eq!(
        programming(row.dict().unwrap().get("nope")),
        "bench.order.line row has no key 'nope' (keys: id, code, qty, price_unit, price_subtotal)"
    );
    assert!(Value::Int(1).dict().is_none());
    assert!(Value::list([]).dict().is_none());
}

#[test]
#[ignore = "DEFERRED_PERFORMANCE: milestone end"]
fn value_stays_32_bytes_and_handles_are_small() {
    assert_eq!(std::mem::size_of::<Value>(), 32);
    assert_eq!(std::mem::size_of::<RowRef>(), 16);
    assert!(std::mem::size_of::<RowsRef>() <= 24);
}

#[test]
fn into_values_gives_one_handle_per_row() {
    let rows = lines(3);
    let values = rows.clone().into_values();
    assert_eq!(values.len(), 3);
    for (i, value) in values.iter().enumerate() {
        assert_eq!(value, &Value::Row(rows.row(i).unwrap()));
    }
}

#[test]
fn batches_cross_threads() {
    let rows = orders();
    let handle = std::thread::spawn({
        let rows = rows.clone();
        move || rows.get(2, "line_ids").unwrap().to_string()
    });
    assert_eq!(
        handle.join().unwrap(),
        rows.get(2, "line_ids").unwrap().to_string()
    );
}

#[test]
fn json_string_escapes_match_serde_json() {
    let mut text: String = (0u8..0x80).map(char::from).collect();
    text.push_str("é漢字😀\u{7f}\u{2028}");
    for sample in [text.as_str(), "", "plain", "\"\\", "end\\"] {
        let mut ours = Vec::new();
        crate::json::write_string(&mut ours, sample);
        assert_eq!(
            String::from_utf8(ours).unwrap(),
            serde_json::to_string(sample).unwrap(),
            "{sample:?}"
        );
    }
}

#[test]
fn a_bitmap_is_bounded_and_equal_whatever_its_trailing_zero_words() {
    let mut bits = Bitmap::from_rows([70]).unwrap();
    assert!(bits.get(70));
    assert_ne!(bits, Bitmap::new());
    bits.set(70, false).unwrap();
    // the second word is still allocated, but all zero: equal to an empty bitmap
    assert!(bits.is_clear());
    assert_eq!(bits, Bitmap::new());
    assert_eq!(Bitmap::new(), bits);
    let message = programming(bits.set(MAX_ROWS, true));
    assert!(
        message.contains("past the most rows a batch holds"),
        "{message}"
    );
    assert!(!bits.get(MAX_ROWS));
    assert!(Bitmap::from_rows([MAX_ROWS]).is_err());
}

#[test]
fn keys_are_equal_and_hash_alike_by_their_text() {
    let field = Key::Field("id");
    let owned = Key::from(Arc::<str>::from("id"));
    assert_eq!(field, owned);
    let hash = |key: &Key| {
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        h.finish()
    };
    assert_eq!(hash(&field), hash(&owned));
    assert_ne!(field, Key::from(String::from("name")));
}

#[test]
fn from_maps_reads_every_row_in_the_first_seen_key_order() {
    let rows = Rows::from_maps([
        map(&[("a", Value::Int(1)), ("b", Value::Int(2))]),
        map(&[("b", Value::Int(3)), ("a", Value::Int(4))]),
    ])
    .unwrap();
    assert_eq!(rows.keys().collect::<Vec<_>>(), ["a", "b"]);
    // the second map's own order (b, a) is not kept: its row reads a, then b
    let second = Value::Row(rows.row(1).unwrap());
    let in_layout_order = Value::Map(Arc::new(map(&[("a", Value::Int(4)), ("b", Value::Int(3))])));
    let in_own_order = Value::Map(Arc::new(map(&[("b", Value::Int(3)), ("a", Value::Int(4))])));
    assert_eq!(second.to_string(), in_layout_order.to_string());
    assert_ne!(second.to_string(), in_own_order.to_string());
}

#[test]
fn a_cell_is_resolved_and_borrowed() {
    let rows = orders();
    let pos = |key: &str| rows.layout().position(key).unwrap();
    assert!(matches!(rows.cell(0, pos("id")).unwrap(), CellRef::Int(1)));
    // many2one with a sub-specification: the child row, or False without a target
    match rows.cell(0, pos("partner_id")).unwrap() {
        CellRef::Row { rows: child, row } => {
            assert_eq!(row, 1);
            assert_eq!(child.get(row, "code").unwrap(), Value::str("P11"));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        rows.cell(1, pos("partner_id")).unwrap(),
        CellRef::False
    ));
    // x2many children, contiguous and indexed, by position
    let children = |row: usize, key: &str| match rows.cell(row, pos(key)).unwrap() {
        CellRef::Children { positions, .. } => positions.collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(children(0, "line_ids"), [0, 1]);
    assert_eq!(children(1, "line_ids"), Vec::<usize>::new());
    assert_eq!(children(0, "tag_ids"), [2, 0]);
    assert_eq!(children(2, "tag_ids"), [2]);

    // absent keys, False in a typed column, text borrowed from the column
    let mut rows = Rows::from_maps([
        map(&[("id", Value::Int(1)), ("code", Value::str("A"))]),
        map(&[("id", Value::Int(2)), ("code", Value::False)]),
        map(&[("id", Value::Int(3))]),
    ])
    .unwrap();
    rows.insert_key("help").unwrap();
    assert!(matches!(rows.cell(0, 1).unwrap(), CellRef::Str("A")));
    assert!(matches!(rows.cell(1, 1).unwrap(), CellRef::False));
    assert!(matches!(rows.cell(2, 1).unwrap(), CellRef::Absent));
    assert!(matches!(rows.cell(0, 2).unwrap(), CellRef::Absent));
    // a value set on a dynamic column after a retype
    rows.set(0, "help", Value::list([Value::Int(1)])).unwrap();
    match rows.cell(0, 2).unwrap() {
        CellRef::Value(value) => assert_eq!(*value, Value::list([Value::Int(1)])),
        other => panic!("{other:?}"),
    }
    assert!(programming(rows.cell(3, 0)).contains("row 3 is out of range (3 rows)"));
    assert!(programming(rows.cell(0, 3)).contains("key position 3 is past the 3 keys"));
}

#[test]
fn row_and_rows_values_convert_to_json_like_their_maps() {
    use crate::convert::value_to_json;
    let rows = orders();
    let as_maps = Value::list(
        rows.iter()
            .map(|row| Value::Map(Arc::new(row.to_map().unwrap()))),
    );
    assert_eq!(
        value_to_json(&Value::from(rows.clone())).unwrap(),
        value_to_json(&as_maps).unwrap()
    );
    assert_eq!(
        value_to_json(&Value::Row(rows.row(2).unwrap())).unwrap(),
        value_to_json(&Value::Map(Arc::new(
            rows.row(2).unwrap().to_map().unwrap()
        )))
        .unwrap()
    );
    assert_eq!(
        value_to_json(&rows.get(0, "line_ids").unwrap()).unwrap(),
        value_to_json(&Value::list([line_map(0), line_map(1)])).unwrap()
    );
}

/// perf-stream §3.6: a batch with one owner gives its buffers back emptied, with their capacity,
/// and the builder fills them and the batch's own allocation again: the column vector, the
/// typed buffers and the `Arc` are the same memory before and after.
#[test]
fn try_recycle_reuses_every_buffer_of_a_sole_owner() {
    let rows = lines(40);
    let batch = Arc::as_ptr(&rows.0);
    let column_vector = rows.0.columns.as_ptr().cast::<u8>();
    let qty = match rows.0.columns[2].data() {
        ColumnData::Float(v) => (v.as_ptr(), v.capacity()),
        _ => panic!("qty is a float column"),
    };
    let layout = Arc::clone(rows.layout());
    let mut builder = rows.try_recycle().expect("one owner");
    assert!(Arc::ptr_eq(builder.layout(), &layout));
    assert_eq!(builder.columns.as_ptr().cast::<u8>(), column_vector);
    let mut taken = Vec::new();
    for pos in 0..layout.len() {
        let (data, falsy, absent) = builder.take_column(pos).expect("recycled").into_parts();
        assert!(falsy.is_clear());
        assert!(absent.is_none_or(|a| a.is_clear()));
        taken.push(data);
    }
    assert!(builder.take_column(0).is_none(), "taken once");
    let ColumnData::Float(mut q) = taken.remove(2) else {
        panic!("qty stays a float column");
    };
    assert!(q.is_empty());
    assert_eq!((q.as_ptr(), q.capacity()), qty);
    q.extend([1.0, 2.0]);
    builder.set_len(2).unwrap();
    builder
        .column_at(2, Column::new(ColumnData::Float(q)))
        .unwrap();
    for (pos, data) in [0, 1, 3, 4].into_iter().zip(taken) {
        let data = match data {
            ColumnData::Int(mut v) => {
                v.extend([7, 8]);
                ColumnData::Int(v)
            }
            ColumnData::Str(mut v) => {
                v.extend([Some(Arc::from("a")), None]);
                ColumnData::Str(v)
            }
            ColumnData::Float(mut v) => {
                v.extend([0.5, 0.25]);
                ColumnData::Float(v)
            }
            other => panic!("unexpected column {other:?}"),
        };
        builder.column_at(pos, Column::new(data)).unwrap();
    }
    let again = builder.finish().unwrap();
    assert_eq!(Arc::as_ptr(&again.0), batch);
    assert_eq!(again.0.columns.as_ptr().cast::<u8>(), column_vector);
    assert_eq!(again.len(), 2);
    assert_eq!(again.get(1, "id").unwrap(), Value::Int(8));
    assert_eq!(again.get(1, "code").unwrap(), Value::False);
    assert_eq!(again.f64(0, "qty").unwrap(), Some(1.0));
}

/// A batch that is shared (a clone, or a row handle into it) is given back unchanged: the other
/// owner keeps reading the same rows.
#[test]
fn try_recycle_refuses_a_shared_batch() {
    let rows = lines(3);
    let clone = rows.clone();
    let rows = rows.try_recycle().expect_err("shared by a clone");
    assert_eq!(rows, clone);
    drop(clone);
    let handle = rows.row(1).unwrap();
    let rows = rows.try_recycle().expect_err("shared by a row handle");
    assert_eq!(handle.get("id").unwrap(), Value::Int(2));
    assert_eq!(rows.len(), 3);
    drop(handle);
    let mut builder = rows.try_recycle().expect("sole owner again");
    // a recycled builder still checks every column: one taken out and not given back
    assert!(builder.take_column(1).is_some());
    assert_eq!(
        programming(builder.finish()),
        "column 'code' of bench.order.line rows was not given"
    );
}
