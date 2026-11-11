//! PERF.ROWS-READ (design note, §8): `web_read` builds
//! its result as columnar batches straight from the cache, and returns what the map-based builder
//! returned. The JSON-2 body of every shape BENCH-002 and the web client read (many2one with and
//! without a sub-specification and with `display_name`, one2many with `order` and `limit`,
//! many2many targets shared by several orders, an x2many without a sub-specification, NULLs,
//! datetimes, non-ASCII and escaped text, duplicate, deleted and id-only records) equals, byte for
//! byte, the body of the same data built as maps from the database rows by this test.
//!
//! Perf slice, not a spec case: the behaviour is GRP §4.8's, covered by `web.rs`; this file pins
//! the representation change. Run once on `d864e9c6` (the map-based builder): it passed there.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod bench_fixture;

use std::collections::BTreeMap;
use std::sync::Arc;

use bench_fixture::{create, harness, orders, set_ids, spec_002, vals};
use erp_orm::{Spec, SpecItem};
use erp_sdk::{DateTime, Transaction, Value, ValueMap};

const LIMIT: usize = 64 << 20;

/// A line row: `(id, order_id, code, qty, price_unit, price_subtotal)`.
type LineRow = (i64, i64, String, f64, Option<f64>, Option<f64>);

/// The database rows of the fixture, read with SQL (the expected values never come from the
/// read path under test).
struct Db {
    orders: BTreeMap<i64, Order>,
    partners: BTreeMap<i64, (String, String)>,
    /// `(id, order_id, code, qty, price_unit, price_subtotal)` in the line model's order
    /// (`order_id, id`).
    lines: Vec<LineRow>,
    tags: BTreeMap<i64, String>,
    /// tag ids per order, in the tag model's order (`name, id`).
    tag_ids: BTreeMap<i64, Vec<i64>>,
}

struct Order {
    code: String,
    name: String,
    state: String,
    date_order: Option<DateTime>,
    amount_total: Option<f64>,
    partner_id: i64,
}

async fn load(tx: &mut Transaction) -> Db {
    let mut db = Db {
        orders: BTreeMap::new(),
        partners: BTreeMap::new(),
        lines: Vec::new(),
        tags: BTreeMap::new(),
        tag_ids: BTreeMap::new(),
    };
    for r in tx
        .query(
            "SELECT id, code, name, state, date_order, amount_total, partner_id FROM bench_order",
            &[],
        )
        .await
        .unwrap()
    {
        db.orders.insert(
            r.get(0),
            Order {
                code: r.get(1),
                name: r.get(2),
                state: r.get(3),
                date_order: r.get(4),
                amount_total: r.get(5),
                partner_id: r.get(6),
            },
        );
    }
    for r in tx
        .query("SELECT id, code, name FROM bench_partner", &[])
        .await
        .unwrap()
    {
        db.partners.insert(r.get(0), (r.get(1), r.get(2)));
    }
    for r in tx
        .query(
            "SELECT id, order_id, code, qty, price_unit, price_subtotal FROM bench_order_line \
             ORDER BY order_id, id",
            &[],
        )
        .await
        .unwrap()
    {
        db.lines
            .push((r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)));
    }
    for r in tx
        .query("SELECT id, code FROM bench_tag", &[])
        .await
        .unwrap()
    {
        db.tags.insert(r.get(0), r.get(1));
    }
    for r in tx
        .query(
            "SELECT rel.bench_order_id, rel.bench_tag_id FROM bench_order_bench_tag_rel rel \
             JOIN bench_tag t ON t.id = rel.bench_tag_id ORDER BY t.name, t.id",
            &[],
        )
        .await
        .unwrap()
    {
        db.tag_ids
            .entry(r.get(0))
            .or_default()
            .push(r.get::<_, i64>(1));
    }
    db
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(Arc::new(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<ValueMap>(),
    ))
}

fn text(s: &str) -> Value {
    Value::str(s)
}

/// A NULL float reads `0.0`, as `read` maps it.
fn float(f: Option<f64>) -> Value {
    Value::Float(f.unwrap_or(0.0))
}

fn date(d: Option<DateTime>) -> Value {
    d.map_or(Value::False, Value::DateTime)
}

/// One order as BENCH-002's `web_read` answers it.
fn order_002(db: &Db, id: i64) -> Value {
    let o = &db.orders[&id];
    let (pcode, pname) = &db.partners[&o.partner_id];
    let lines: Vec<Value> = db
        .lines
        .iter()
        .filter(|l| l.1 == id)
        .map(|l| {
            map(vec![
                ("id", Value::Int(l.0)),
                ("code", text(&l.2)),
                ("qty", Value::Float(l.3)),
                ("price_unit", float(l.4)),
                ("price_subtotal", float(l.5)),
            ])
        })
        .collect();
    let tags: Vec<Value> = db
        .tag_ids
        .get(&id)
        .into_iter()
        .flatten()
        .map(|t| map(vec![("id", Value::Int(*t)), ("code", text(&db.tags[t]))]))
        .collect();
    map(vec![
        ("id", Value::Int(id)),
        ("code", text(&o.code)),
        ("name", text(&o.name)),
        ("state", text(&o.state)),
        ("date_order", date(o.date_order)),
        ("amount_total", float(o.amount_total)),
        ("partner_name", text(pname)),
        (
            "partner_id",
            map(vec![
                ("id", Value::Int(o.partner_id)),
                ("code", text(pcode)),
            ]),
        ),
        ("line_ids", Value::list(lines)),
        ("tag_ids", Value::list(tags)),
    ])
}

/// `partner_id` with its `display_name`, lines ordered by `code desc` with their raw `order_id`,
/// tags as ids.
fn spec_ordered() -> Spec {
    Spec::new()
        .field("code")
        .with(
            "partner_id",
            SpecItem::fields(Spec::new().field("code").field("display_name")),
        )
        .with(
            "line_ids",
            SpecItem::fields(Spec::new().field("code").field("order_id")).order("code desc"),
        )
        .field("tag_ids")
}

fn order_ordered(db: &Db, id: i64) -> Value {
    let o = &db.orders[&id];
    let (pcode, pname) = &db.partners[&o.partner_id];
    let mut lines: Vec<_> = db.lines.iter().filter(|l| l.1 == id).collect();
    lines.sort_by(|a, b| b.2.cmp(&a.2));
    map(vec![
        ("id", Value::Int(id)),
        ("code", text(&o.code)),
        (
            "partner_id",
            map(vec![
                ("id", Value::Int(o.partner_id)),
                ("code", text(pcode)),
                ("display_name", text(pname)),
            ]),
        ),
        (
            "line_ids",
            Value::list(lines.iter().map(|l| {
                map(vec![
                    ("id", Value::Int(l.0)),
                    ("code", text(&l.2)),
                    ("order_id", Value::Int(l.1)),
                ])
            })),
        ),
        (
            "tag_ids",
            Value::list(
                db.tag_ids
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .map(|t| Value::Int(*t)),
            ),
        ),
    ])
}

/// Stored fields only, with lines and tags.
fn spec_stored() -> Spec {
    Spec::new()
        .field("code")
        .field("amount_total")
        .with("line_ids", SpecItem::fields(Spec::new().field("code")))
        .with("tag_ids", SpecItem::fields(Spec::new().field("code")))
}

fn order_stored(db: &Db, id: i64) -> Value {
    let o = &db.orders[&id];
    map(vec![
        ("id", Value::Int(id)),
        ("code", text(&o.code)),
        ("amount_total", float(o.amount_total)),
        (
            "line_ids",
            Value::list(
                db.lines
                    .iter()
                    .filter(|l| l.1 == id)
                    .map(|l| map(vec![("id", Value::Int(l.0)), ("code", text(&l.2))])),
            ),
        ),
        (
            "tag_ids",
            Value::list(
                db.tag_ids
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .map(|t| map(vec![("id", Value::Int(*t)), ("code", text(&db.tags[t]))])),
            ),
        ),
    ])
}

/// Lines limited to one per order: the others answer `{'id': id}`.
fn spec_limited() -> Spec {
    Spec::new().with(
        "line_ids",
        SpecItem::fields(Spec::new().field("code")).limit(1),
    )
}

fn order_limited(db: &Db, id: i64) -> Value {
    let lines: Vec<Value> = db
        .lines
        .iter()
        .filter(|l| l.1 == id)
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                map(vec![("id", Value::Int(l.0)), ("code", text(&l.2))])
            } else {
                map(vec![("id", Value::Int(l.0))])
            }
        })
        .collect();
    map(vec![
        ("id", Value::Int(id)),
        ("line_ids", Value::list(lines)),
    ])
}

/// Three orders, one deleted; non-ASCII and escaped text, a NULL date, a NULL price, an order
/// without lines or tags, a tag shared by two orders. Returns `[o1, o2, o3, deleted]`.
async fn fixture(tx: &mut Transaction) -> [i64; 4] {
    let partners = create(
        tx,
        "bench.partner",
        &[
            vals(vec![
                ("code", text("P1")),
                ("name", text("Ünïcødé Pärtner ✓")),
            ]),
            vals(vec![
                ("code", text("P2")),
                ("name", text("Partner \"quoted\"\n\\ tab\t")),
            ]),
        ],
    )
    .await;
    let tags = create(
        tx,
        "bench.tag",
        &[
            vals(vec![("code", text("T1")), ("name", text("B tag"))]),
            vals(vec![("code", text("T2")), ("name", text("A tag"))]),
            vals(vec![("code", text("T3")), ("name", text("C tag"))]),
        ],
    )
    .await;
    let when = DateTime::parse_odoo("2024-02-29 13:45:01").unwrap();
    let orders = create(
        tx,
        "bench.order",
        &[
            vals(vec![
                ("code", text("O1")),
                ("name", text("Ördér 1")),
                ("partner_id", Value::Int(partners[0])),
                ("date_order", Value::DateTime(when)),
                ("tag_ids", set_ids(&[tags[0], tags[1]])),
            ]),
            vals(vec![
                ("code", text("O2")),
                ("name", text("Order 2")),
                ("partner_id", Value::Int(partners[1])),
                ("date_order", Value::False),
            ]),
            vals(vec![
                ("code", text("O3")),
                ("name", text("Order 3")),
                ("partner_id", Value::Int(partners[0])),
                ("tag_ids", set_ids(&[tags[1]])),
            ]),
            vals(vec![
                ("code", text("O4")),
                ("name", text("Order 4")),
                ("partner_id", Value::Int(partners[1])),
            ]),
        ],
    )
    .await;
    let line = |code: &str, order: i64, qty: f64, price: Option<f64>| {
        let mut v = vec![
            ("code", text(code)),
            ("order_id", Value::Int(order)),
            ("product_name", text("Prödüct")),
            ("qty", Value::Float(qty)),
        ];
        if let Some(p) = price {
            v.push(("price_unit", Value::Float(p)));
        }
        vals(v)
    };
    create(
        tx,
        "bench.order.line",
        &[
            line("L1a", orders[0], 2.0, Some(3.5)),
            line("L1c", orders[0], 1.0, None),
            line("L1b", orders[0], 0.5, Some(10.0)),
            line("L3a", orders[2], 4.0, Some(0.25)),
            line("L4a", orders[3], 1.0, Some(1.0)),
        ],
    )
    .await;
    tx.env()
        .records("bench.order")
        .unwrap()
        .browse([orders[3]])
        .unwrap()
        .unlink(tx)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    [orders[0], orders[1], orders[2], orders[3]]
}

fn body(values: &[Value]) -> String {
    let bytes = erp_rpc::__output::serialize(&Value::list(values.iter().cloned()), LIMIT).unwrap();
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn web_read_batches_write_the_json_of_the_map_rows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [o1, o2, o3, gone] = fixture(&mut tx).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(2).await.unwrap();
    let db = load(&mut tx).await;
    // duplicates and a non-id order in the recordset; a deleted record below
    let ids = [o3, o1, o2, o1];
    let present = ids;

    tx.invalidate_all().await.unwrap();
    let before = tx.query_count();
    let got = orders(&tx, &ids)
        .web_read(&mut tx, spec_002())
        .await
        .unwrap();
    let statements = tx.query_count() - before;
    let want: Vec<Value> = present.iter().map(|id| order_002(&db, *id)).collect();
    assert_eq!(body(&got), body(&want));
    assert_eq!(got, want);
    // the rows are batch rows, not maps (step 1)
    assert!(got.iter().all(|v| matches!(v, Value::Row(_))), "{got:?}");
    // the statements of the map-based builder (counted on d864e9c6): orders, their lines, the
    // tag links, then partners and tags in one flight (asserted, not read from code)
    assert_eq!(
        statements, STATEMENTS_002,
        "statements of the BENCH-002 web_read"
    );

    tx.invalidate_all().await.unwrap();
    let got = orders(&tx, &ids)
        .web_read(&mut tx, spec_ordered())
        .await
        .unwrap();
    let want: Vec<Value> = present.iter().map(|id| order_ordered(&db, *id)).collect();
    assert_eq!(body(&got), body(&want));

    tx.invalidate_all().await.unwrap();
    let got = orders(&tx, &ids)
        .web_read(&mut tx, spec_limited())
        .await
        .unwrap();
    let want: Vec<Value> = present.iter().map(|id| order_limited(&db, *id)).collect();
    assert_eq!(body(&got), body(&want));

    // a deleted record is left out (`read` skips it). The specification reads stored
    // fields only: a non-stored computed field (`partner_name`) over a recordset holding a deleted
    // record fails with `Missing` on `d864e9c6` too (its compute batch takes the deleted record from
    // the prefetch set), a defect outside this slice reported with it
    let with_gone = [o3, gone, o1];
    tx.invalidate_all().await.unwrap();
    let got = orders(&tx, &with_gone)
        .web_read(&mut tx, spec_stored())
        .await
        .unwrap();
    let want: Vec<Value> = [o3, o1].iter().map(|id| order_stored(&db, *id)).collect();
    assert_eq!(body(&got), body(&want));

    let ids = [o3, o1, gone, o1];
    for spec in [Spec::new(), Spec::new().field("id")] {
        let before = tx.query_count();
        let got = orders(&tx, &ids).web_read(&mut tx, spec).await.unwrap();
        assert_eq!(
            tx.query_count(),
            before,
            "an id-only web_read sends no query"
        );
        let want: Vec<Value> = ids
            .iter()
            .map(|id| map(vec![("id", Value::Int(*id))]))
            .collect();
        assert_eq!(body(&got), body(&want));
    }

    // an empty recordset: no row, no query
    let before = tx.query_count();
    let got = orders(&tx, &[])
        .web_read(&mut tx, spec_002())
        .await
        .unwrap();
    assert!(got.is_empty());
    assert_eq!(tx.query_count(), before);
    tx.rollback().await.unwrap();
}

/// Statements of BENCH-002's `web_read` over the fixture (measured on the map-based builder).
const STATEMENTS_002: u64 = 5;

/// `read` and `search_read` keep their dictionaries: the batch is converted at the public
/// boundary, with the values `read` always returned (`[id, display_name]` many2one labels, id
/// lists, the NULL mapping), for every field the environment may read when none is named.
#[tokio::test]
async fn read_keeps_its_dictionaries() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [o1, o2, o3, gone] = fixture(&mut tx).await;
    tx.commit().await.unwrap();
    let mut tx = h.begin_as(2).await.unwrap();
    let db = load(&mut tx).await;
    let got = orders(&tx, &[o3, o1, gone, o2])
        .read(
            &mut tx,
            &["partner_id", "tag_ids", "amount_total", "code", "code"],
        )
        .await
        .unwrap();
    let want: Vec<BTreeMap<String, Value>> = [o3, o1, o2]
        .iter()
        .map(|id| {
            let o = &db.orders[id];
            let (_, pname) = &db.partners[&o.partner_id];
            [
                ("id", Value::Int(*id)),
                (
                    "partner_id",
                    Value::list([Value::Int(o.partner_id), text(pname)]),
                ),
                (
                    "tag_ids",
                    Value::list(
                        db.tag_ids
                            .get(id)
                            .into_iter()
                            .flatten()
                            .map(|t| Value::Int(*t)),
                    ),
                ),
                ("amount_total", float(o.amount_total)),
                ("code", text(&o.code)),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
        })
        .collect();
    assert_eq!(got, want);

    let rows = orders(&tx, &[o3, o1])
        .read_rows(
            &mut tx,
            &["code", "partner_id", "code", "id"],
            erp_orm::Load::Raw,
        )
        .await
        .unwrap();
    assert_eq!(
        rows.keys().collect::<Vec<_>>(),
        ["id", "code", "partner_id"]
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.get(0, "partner_id").unwrap(),
        Value::Int(db.orders[&o3].partner_id)
    );
    assert_eq!(
        rows.str(1, "code").unwrap(),
        Some(db.orders[&o1].code.as_str())
    );
    tx.rollback().await.unwrap();
}

/// Review finding 2: new (in-memory) records. Every field a new record has not been given reads
/// its default, its origin's value or its computed value, cached by `ensure` before the column is
/// gathered (`engine.rs` `ensure`: `fill_defaults`, `copy_from_origin`, `compute_group`), so the
/// batch reads exactly what the field-by-field path (`Records::values`) reads, and an unset field
/// without a default reads `False`; nothing is substituted by the batch builder.
#[tokio::test]
async fn new_records_read_what_their_fields_read() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [o1, _, _, _] = fixture(&mut tx).await;
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let db = load(&mut tx).await;
    let model = tx.env().records("bench.order").unwrap();
    // only `code` given: `name` and `partner_id` have no default, `state` and `date_order` have
    // one, `amount_total` is computed, the x2many fields are empty
    let fresh = tx
        .new_record(
            &model,
            erp_sdk::RawVals::new().set_cell("code", erp_sdk::Cell::Str("N1".into())),
            None,
            None,
        )
        .await
        .unwrap();
    // nothing given, with an origin: the origin's values
    let origin = orders(&tx, &[o1]).rec_ids()[0];
    let copy = tx
        .new_record(&model, erp_sdk::RawVals::new(), Some(origin), None)
        .await
        .unwrap();
    let fields = [
        "code",
        "name",
        "partner_id",
        "state",
        "date_order",
        "amount_total",
        "tag_ids",
        "line_ids",
    ];
    for (records, load) in [
        (&fresh, erp_orm::Load::Raw),
        (&copy, erp_orm::Load::Raw),
        (&fresh, erp_orm::Load::Classic),
    ] {
        let rows = records.read_rows(&mut tx, &fields, load).await.unwrap();
        assert_eq!(rows.len(), 1);
        for field in fields {
            let want = match (field, load) {
                // the label of an unset many2one is `False` too
                ("partner_id", erp_orm::Load::Classic) => Value::False,
                _ => records.values(&mut tx, field).await.unwrap().remove(0),
            };
            assert_eq!(rows.get(0, field).unwrap(), want, "{field} ({load:?})");
        }
    }
    let rows = fresh
        .read_rows(&mut tx, &fields, erp_orm::Load::Raw)
        .await
        .unwrap();
    assert_eq!(rows.get(0, "name").unwrap(), Value::False);
    assert_eq!(rows.get(0, "partner_id").unwrap(), Value::False);
    assert_eq!(rows.get(0, "line_ids").unwrap(), Value::list([]));
    let rows = copy
        .read_rows(&mut tx, &fields, erp_orm::Load::Raw)
        .await
        .unwrap();
    let o = &db.orders[&o1];
    assert_eq!(rows.str(0, "name").unwrap(), Some(o.name.as_str()));
    assert_eq!(rows.get(0, "partner_id").unwrap(), Value::Int(o.partner_id));
    // web_read: the same values, the origin's partner expanded from the database's row
    let spec = Spec::new()
        .field("name")
        .with("partner_id", SpecItem::fields(Spec::new().field("code")))
        .with("line_ids", SpecItem::fields(Spec::new().field("code")));
    let got = fresh.web_read(&mut tx, spec.clone()).await.unwrap();
    let row = got[0].dict().unwrap();
    assert_eq!(row.get("name").unwrap(), Value::False);
    assert_eq!(row.get("partner_id").unwrap(), Value::False);
    assert_eq!(row.get("line_ids").unwrap(), Value::list([]));
    let got = copy.web_read(&mut tx, spec).await.unwrap();
    let row = got[0].dict().unwrap();
    assert_eq!(row.get("name").unwrap(), text(&o.name));
    let (pcode, _) = &db.partners[&o.partner_id];
    assert_eq!(
        row.get("partner_id").unwrap(),
        map(vec![
            ("id", Value::Int(o.partner_id)),
            ("code", text(pcode))
        ])
    );
    // the copy's lines are new records with origins: their web_read rows carry their real origin
    // IDs and the origin lines' values (/).
    let new_lines = copy.related(&mut tx, "line_ids").await.unwrap();
    let origins = new_lines.origin().ids();
    assert_eq!(
        origins,
        db.lines
            .iter()
            .filter(|l| l.1 == o1)
            .map(|l| l.0)
            .collect::<Vec<_>>()
    );
    let lines: Vec<Value> = origins
        .iter()
        .map(|origin| {
            let line = db.lines.iter().find(|l| l.0 == *origin).unwrap();
            map(vec![("id", Value::Int(*origin)), ("code", text(&line.2))])
        })
        .collect();
    assert_eq!(
        body(&[row.get("line_ids").unwrap()]),
        body(&[Value::list(lines)])
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn classic_labels_of_deleted_targets_read_false() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [o1, _, o3, _] = fixture(&mut tx).await;
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let db = load(&mut tx).await;
    let line_ids: Vec<i64> = db
        .lines
        .iter()
        .filter(|l| l.1 == o1 || l.1 == o3)
        .map(|l| l.0)
        .collect();
    let lines = tx
        .env()
        .records("bench.order.line")
        .unwrap()
        .browse(line_ids.iter().copied())
        .unwrap();
    // the lines' `code` and `order_id` are cached; then `o1` goes (its lines with it, in the
    // database only) and the cache forgets the orders
    lines
        .read_rows(&mut tx, &["code", "order_id"], erp_orm::Load::Raw)
        .await
        .unwrap();
    tx.execute("DELETE FROM bench_order WHERE id = $1", &[&o1])
        .await
        .unwrap();
    tx.invalidate_model("bench.order", None).await.unwrap();
    let left: Vec<(i64, String)> = tx
        .query(
            "SELECT id, name FROM bench_order WHERE id = ANY($1)",
            &[&vec![o1, o3]],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let rows = lines
        .read_rows(&mut tx, &["code", "order_id"], erp_orm::Load::Classic)
        .await
        .unwrap();
    assert_eq!(rows.len(), line_ids.len());
    for (i, id) in line_ids.iter().enumerate() {
        let line = db.lines.iter().find(|l| l.0 == *id).unwrap();
        let want = match left.iter().find(|(o, _)| *o == line.1) {
            Some((o, name)) => Value::list([Value::Int(*o), text(name)]),
            None => Value::False,
        };
        assert_eq!(rows.get(i, "order_id").unwrap(), want, "line {id}");
        assert_eq!(rows.str(i, "code").unwrap(), Some(line.2.as_str()));
    }
    // both kinds are in the result: one target gone, one kept
    assert_eq!(left.iter().map(|(o, _)| *o).collect::<Vec<_>>(), [o3]);
    tx.rollback().await.unwrap();
}
