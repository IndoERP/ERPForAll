//! Ordering by fields inherited through `_inherits` (review `review-, O1 to O4):
//! the order joins the parent through the delegate, flushes the delegate column of every level,
//! refuses two different joins under one alias, and keyset pagination refuses these orders
//! with a clear error. Superuser transactions on the plain harness: the access side of the same
//! join is (`spec_whole.rs`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;

use erp_domain::{Domain, Operator, Value as DomainValue};
use erp_fields::{ErrorKind, OnDelete, Value};
use erp_orm::{SearchOptions, Transaction};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_testkit::Harness;

static FIXTURE: AddonDescriptor = AddonDescriptor {
    name: "x_ord",
    depends: &["base"],
    exact_only: false,
};

const CATEG: &str = "x.categ";
const UNIT: &str = "x.unit";
const BOX: &str = "x.box";
const CRATE: &str = "x.crate";
const PALLET: &str = "x.pallet";
const CL_A: &str = "x.cl_a";

const NAME_FIELDS: &[FieldDecl] = &[FieldDecl::char("name")];
const UNIT_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("readonly_name"),
    FieldDecl::many2one("categ_id", CATEG),
];
const BOX_FIELDS: &[FieldDecl] = &[
    FieldDecl::many2one("unit_id", UNIT)
        .required(true)
        .ondelete(OnDelete::Cascade),
    FieldDecl::integer("size"),
];
const CRATE_FIELDS: &[FieldDecl] = &[FieldDecl::many2one("box_id", BOX)
    .required(true)
    .ondelete(OnDelete::Cascade)];
const PALLET_FIELDS: &[FieldDecl] = &[FieldDecl::many2one("box_id", BOX), FieldDecl::char("name")];
// the alias of the inherited `c` (`x_cl_a__b__c`) is also the alias of the own field `b__c`
const CL_B_FIELDS: &[FieldDecl] = &[FieldDecl::many2one("c", "x.cl_c")];
const CL_A_FIELDS: &[FieldDecl] = &[
    FieldDecl::many2one("b", "x.cl_b")
        .required(true)
        .ondelete(OnDelete::Cascade),
    FieldDecl::many2one("b__c", "x.cl_d"),
];

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &FIXTURE,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_ord", CATEG)
            .description("Ord Category")
            .order("name")
            .fields(NAME_FIELDS),
        ModelDecl::new("x_ord", UNIT)
            .description("Ord Unit")
            .fields(UNIT_FIELDS),
        ModelDecl::new("x_ord", BOX)
            .description("Ord Box")
            .inherits(&[(UNIT, "unit_id")])
            .order("readonly_name")
            .fields(BOX_FIELDS),
        ModelDecl::new("x_ord", CRATE)
            .description("Ord Crate")
            .inherits(&[(BOX, "box_id")])
            .fields(CRATE_FIELDS),
        ModelDecl::new("x_ord", PALLET)
            .description("Ord Pallet")
            .fields(PALLET_FIELDS),
        ModelDecl::new("x_ord", "x.cl_c")
            .description("Ord C")
            .order("name")
            .fields(NAME_FIELDS),
        ModelDecl::new("x_ord", "x.cl_d")
            .description("Ord D")
            .order("name")
            .fields(NAME_FIELDS),
        ModelDecl::new("x_ord", "x.cl_b")
            .description("Ord B")
            .fields(CL_B_FIELDS),
        ModelDecl::new("x_ord", CL_A)
            .description("Ord A")
            .inherits(&[("x.cl_b", "b")])
            .fields(CL_A_FIELDS),
    ])
    .build()
    .await
    .expect("harness")
}

fn row(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

async fn create(tx: &mut Transaction, model: &str, rows: &[BTreeMap<String, Value>]) -> Vec<i64> {
    tx.env()
        .records(model)
        .unwrap()
        .create_values(tx, rows)
        .await
        .unwrap()
        .ids()
}

/// The ids in `ids` ordered by their keys (NULLs per `nulls_first`), ties by id: what the
/// database must return for the term, computed from the values the fixture created.
fn sorted_by(ids: &[i64], keys: &[Option<&str>], descending: bool, nulls_first: bool) -> Vec<i64> {
    let mut pairs: Vec<(i64, Option<&str>)> =
        ids.iter().copied().zip(keys.iter().copied()).collect();
    pairs.sort_by(|(ia, a), (ib, b)| {
        let by_key = match (a, b) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => {
                if nulls_first {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }
            (Some(_), None) => {
                if nulls_first {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            }
            (Some(a), Some(b)) => {
                if descending {
                    b.cmp(a)
                } else {
                    a.cmp(b)
                }
            }
        };
        by_key.then(ia.cmp(ib))
    });
    pairs.into_iter().map(|(id, _)| id).collect()
}

/// `search(id in ids, order)` and the statements it ran.
async fn ordered(
    tx: &mut Transaction,
    model: &str,
    ids: &[i64],
    order: &str,
) -> (Vec<i64>, Vec<String>) {
    let records = tx.env().records(model).unwrap();
    let domain = Domain::condition("id", Operator::In, DomainValue::list(ids.to_vec())).unwrap();
    tx.capture_statements(true);
    let found = records
        .search_with(tx, &domain, &SearchOptions::new().order(order))
        .await
        .unwrap()
        .ids();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    (found, statements)
}

fn joins(sql: &str) -> usize {
    sql.matches(" JOIN ").count()
}

struct World {
    units: [i64; 3],
    boxes: [i64; 3],
    crates: [i64; 3],
    pallets: [i64; 3],
}

/// Units `zeta` (category b-cat), `alpha` (category a-cat) and one without a name or category;
/// a box, a crate (grandchild) and a pallet (many2one to the box) on each. Committed.
async fn world(h: &Harness) -> World {
    let mut tx = h.begin().await.unwrap();
    let categs = create(
        &mut tx,
        CATEG,
        &[
            row(&[("name", Value::str("b-cat"))]),
            row(&[("name", Value::str("a-cat"))]),
        ],
    )
    .await;
    let units = create(
        &mut tx,
        UNIT,
        &[
            row(&[
                ("readonly_name", Value::str("zeta")),
                ("categ_id", Value::Int(categs[0])),
            ]),
            row(&[
                ("readonly_name", Value::str("alpha")),
                ("categ_id", Value::Int(categs[1])),
            ]),
            row(&[]),
        ],
    )
    .await;
    let boxes = create(
        &mut tx,
        BOX,
        &units
            .iter()
            .map(|u| row(&[("unit_id", Value::Int(*u))]))
            .collect::<Vec<_>>(),
    )
    .await;
    let crates = create(
        &mut tx,
        CRATE,
        &boxes
            .iter()
            .map(|b| row(&[("box_id", Value::Int(*b))]))
            .collect::<Vec<_>>(),
    )
    .await;
    let pallets = create(
        &mut tx,
        PALLET,
        &boxes
            .iter()
            .map(|b| row(&[("box_id", Value::Int(*b))]))
            .collect::<Vec<_>>(),
    )
    .await;
    tx.commit().await.unwrap();
    World {
        units: units.try_into().unwrap(),
        boxes: boxes.try_into().unwrap(),
        crates: crates.try_into().unwrap(),
        pallets: pallets.try_into().unwrap(),
    }
}

const NAMES: [Option<&str>; 3] = [Some("zeta"), Some("alpha"), None];

/// O4 (a), (b), (d): an inherited order with no inherited condition (the join comes from the
/// order alone), through a grandparent, descending with NULLs first.
#[tokio::test]
async fn ordering_by_an_inherited_field_joins_the_parents() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = h.begin().await.unwrap();
    for (model, ids, hops) in [(BOX, &w.boxes, 1), (CRATE, &w.crates, 2)] {
        for (order, descending, nulls_first) in [
            ("readonly_name", false, false),
            ("readonly_name desc", true, true),
            ("readonly_name desc nulls last", true, false),
            ("readonly_name asc nulls first", false, true),
        ] {
            let (found, statements) = ordered(&mut tx, model, ids, order).await;
            assert_eq!(
                found,
                sorted_by(ids, &NAMES, descending, nulls_first),
                "{model} order {order:?}"
            );
            assert_eq!(statements.len(), 1, "{statements:#?}");
            assert_eq!(joins(&statements[0]), hops, "{}", statements[0]);
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// O4 (c): an inherited many2one is ordered by its comodel's order, or by its raw id.
#[tokio::test]
async fn an_inherited_many2one_is_ordered_by_its_comodel_order_or_its_id() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = h.begin().await.unwrap();
    // units zeta -> "b-cat", alpha -> "a-cat", the third has none
    let categ_names = [Some("b-cat"), Some("a-cat"), None];
    for (order, descending, nulls_first) in
        [("categ_id", false, false), ("categ_id desc", true, true)]
    {
        let (found, statements) = ordered(&mut tx, BOX, &w.boxes, order).await;
        assert_eq!(
            found,
            sorted_by(&w.boxes, &categ_names, descending, nulls_first),
            "{order}"
        );
        assert_eq!(
            joins(&statements[0]),
            2,
            "unit and category: {}",
            statements[0]
        );
    }
    // by the category's id: b-cat was created first
    let (found, statements) = ordered(&mut tx, BOX, &w.boxes, "categ_id.id").await;
    assert_eq!(found, [w.boxes[0], w.boxes[1], w.boxes[2]]);
    assert_eq!(joins(&statements[0]), 1, "{}", statements[0]);
    let (found, _) = ordered(&mut tx, BOX, &w.boxes, "categ_id.id desc nulls last").await;
    assert_eq!(found, [w.boxes[1], w.boxes[0], w.boxes[2]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// O4 (e): a many2one whose comodel's `_order` names an inherited field.
#[tokio::test]
async fn a_comodel_ordered_by_an_inherited_field() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = h.begin().await.unwrap();
    for (order, descending) in [("box_id", false), ("box_id desc", true)] {
        let (found, _) = ordered(&mut tx, PALLET, &w.pallets, order).await;
        // a descending many2one flips the comodel order, NULLs included
        assert_eq!(
            found,
            sorted_by(&w.pallets, &NAMES, descending, descending),
            "{order}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// O1: the delegate column is flushed before the ordered search, at every level.
#[tokio::test]
async fn pending_delegate_writes_are_flushed_before_an_inherited_order() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = h.begin().await.unwrap();
    let boxes = tx.env().records(BOX).unwrap();
    let (before, _) = ordered(&mut tx, BOX, &w.boxes[..2], "readonly_name").await;
    assert_eq!(before, [w.boxes[1], w.boxes[0]]);
    // swap the units of the two boxes, not flushed yet
    boxes
        .browse([w.boxes[0]])
        .unwrap()
        .write_values(&mut tx, &row(&[("unit_id", Value::Int(w.units[1]))]))
        .await
        .unwrap();
    boxes
        .browse([w.boxes[1]])
        .unwrap()
        .write_values(&mut tx, &row(&[("unit_id", Value::Int(w.units[0]))]))
        .await
        .unwrap();
    let (after, statements) = ordered(&mut tx, BOX, &w.boxes[..2], "readonly_name").await;
    assert_eq!(after, [w.boxes[0], w.boxes[1]], "{statements:#?}");

    // the same through a grandparent: the crates' own delegate and the boxes'
    let crates = tx.env().records(CRATE).unwrap();
    crates
        .browse([w.crates[0]])
        .unwrap()
        .write_values(&mut tx, &row(&[("box_id", Value::Int(w.boxes[2]))]))
        .await
        .unwrap();
    let (found, _) = ordered(&mut tx, CRATE, &w.crates[..1], "readonly_name").await;
    assert_eq!(found, [w.crates[0]]);
    let (found, statements) = ordered(&mut tx, CRATE, &w.crates[..2], "readonly_name desc").await;
    // crate 0 now sits on the box without a name (first when descending), crate 1 on "zeta"
    assert_eq!(found, [w.crates[0], w.crates[1]], "{statements:#?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// O2: two different joins under one alias are an error, not a silently dropped join.
#[tokio::test]
async fn two_different_joins_under_one_alias_are_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records(CL_A).unwrap();
    // the inherited `c` (through `b`) and the own `b__c` both join as `x_cl_a__b__c`
    let err = records
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("c, b__c"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    assert_eq!(
        err.message(),
        "Cannot change the JOIN condition of the alias x_cl_a__b__c: \
         LEFT JOIN \"x_cl_c\" AS \"x_cl_a__b__c\" ON (\"x_cl_a__b\".\"c\" = \"x_cl_a__b__c\".\"id\") \
         against LEFT JOIN \"x_cl_d\" AS \"x_cl_a__b__c\" ON (\"x_cl_a\".\"b__c\" = \"x_cl_a__b__c\".\"id\")"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// O3: keyset pagination over an inherited order is refused with an accurate message.
#[tokio::test]
async fn keyset_pagination_refuses_inherited_order_terms() {
    let h = harness().await;
    world(&h).await;
    let mut tx = h.begin().await.unwrap();
    let boxes = tx.env().records(BOX).unwrap();
    for field in ["readonly_name", "categ_id"] {
        let err = boxes
            .search_after(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().order(field).limit(2),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
        assert_eq!(
            err.message(),
            format!(
                "keyset pagination cannot order by x.box.{field}: it is inherited through \
                 _inherits, so its key is on the parent's table, which keyset pagination does \
                 not join"
            )
        );
    }
    // the box's own columns still page
    let page = boxes
        .search_after(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("size, id").limit(2),
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.records.ids().len(), 2);
    drop(tx);
    h.cleanup().await.unwrap();
}
