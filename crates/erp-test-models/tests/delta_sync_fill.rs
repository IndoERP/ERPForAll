// SPDX-License-Identifier: MIT OR Apache-2.0
//! amendment B7: insert-only fill columns of the synchronous guarded apply, against
//! PostgreSQL. The consumer is a declared model whose table has a required timestamp outside the
//! grain and measures (the shape of `stock.quant.in_date`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::num::NonZeroU32;

use erp_fields::{Date, DateTime, Digits, ErrorKind};
use erp_registry::{
    AddonDescriptor, FieldDecl, KeyDecl, ModelDecl, TableObjectDecl, TableObjectKind,
};
use erp_sdk::Transaction;
use erp_sdk::delta::{
    DeltaLimits, GrainColumn, GrainType, GrainValue, InsertFill, MeasureColumn, MeasureType,
    MeasureValue, NettedBatch, ProjectionShape, SyncApply, apply_synchronous,
};
use erp_test_models::base_users;
use erp_testkit::Harness;

const MODEL: &str = "h27fill.stock";
const TABLE: &str = "h27fill_stock";
const CONSTRAINT: &str = "h27fill_stock_key";

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "h27fill",
    depends: &["base"],
    exact_only: false,
};
static FIELDS: &[FieldDecl] = &[
    FieldDecl::integer("product").required(true),
    FieldDecl::float_digits("quantity", Digits::Unrounded),
    FieldDecl::datetime("received").required(true),
    FieldDecl::integer("twice").generated("product * 2"),
];
static OBJECTS: &[TableObjectDecl] = &[TableObjectDecl {
    name: "_key",
    kind: TableObjectKind::Key(KeyDecl::new(&["product"], "One row per product.")),
}];

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("h27fill", MODEL)
            .fields(FIELDS)
            .table_objects(OBJECTS)])
        .build()
        .await
        .unwrap()
}

fn shape() -> ProjectionShape {
    ProjectionShape::new(
        MODEL,
        NonZeroU32::new(1).unwrap(),
        vec![GrainColumn::new("product", GrainType::Int8, false)],
        vec![MeasureColumn::new(
            "quantity",
            MeasureType::Numeric {
                precision: 12,
                scale: 3,
            },
        )],
        &DeltaLimits::default(),
    )
    .unwrap()
}

fn batch(s: &ProjectionShape, product: i64, quantity: &str) -> NettedBatch {
    let key = s.admit_key(vec![GrainValue::Int8(product)]).unwrap();
    let measures = s
        .admit_measures(vec![MeasureValue::Numeric(quantity.parse().unwrap())])
        .unwrap();
    s.net(vec![(key, measures)], &DeltaLimits::default())
        .unwrap()
}

fn sync(tx: &Transaction, s: &ProjectionShape, fill: Option<InsertFill>) -> SyncApply {
    let apply = SyncApply::new(s, TABLE, CONSTRAINT, Vec::new()).unwrap();
    match fill {
        Some(fill) => {
            let registry = tx.registry().clone();
            apply
                .with_insert_fill(registry.model(MODEL).unwrap(), "received", fill)
                .unwrap()
        }
        None => apply,
    }
}

/// `(quantity, received)` of `product`, as text.
async fn stored(tx: &mut Transaction, product: i32) -> (String, String) {
    let row = tx
        .query(
            "SELECT trim_scale(quantity)::text, received::text FROM h27fill_stock WHERE product = $1",
            &[&product],
        )
        .await
        .unwrap();
    assert_eq!(row.len(), 1, "one row of product {product}");
    (row[0].get(0), row[0].get(1))
}

fn at(day: u32) -> DateTime {
    DateTime::from_parts(Date::from_ymd(2026, 5, day).unwrap(), 10, 0, 0, 0).unwrap()
}

/// Without a fill, the required column fails the statement with 23502, also for an existing key:
/// PostgreSQL checks NOT NULL on the proposed row before it resolves the conflict.
#[tokio::test]
async fn without_a_fill_a_required_column_fails_the_apply() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO h27fill_stock (product, quantity, received) VALUES (1, 1, now())",
        &[],
    )
    .await
    .unwrap();
    let s = shape();
    let apply = sync(&tx, &s, None);
    let failure = apply_synchronous(&mut tx, &apply, &batch(&s, 1, "1"))
        .await
        .unwrap_err();
    assert_eq!(failure.error().sqlstate(), Some("23502"), "{failure:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The transaction-time fill is the transaction's `now()` in UTC.
#[tokio::test]
async fn transaction_time_fill_is_the_transaction_start() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = shape();
    let apply = sync(&tx, &s, Some(InsertFill::TransactionTime));
    let applied = apply_synchronous(&mut tx, &apply, &batch(&s, 1, "2"))
        .await
        .unwrap();
    assert!(applied.rows()[0].inserted());
    let now: String = tx
        .query("SELECT (now() AT TIME ZONE 'UTC')::text", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored(&mut tx, 1).await, ("2".into(), now));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A bound value fills a new key; a second apply with another value leaves the stored one.
#[tokio::test]
async fn value_fill_is_insert_only() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = shape();
    let first = sync(&tx, &s, Some(InsertFill::Value(at(1))));
    apply_synchronous(&mut tx, &first, &batch(&s, 2, "1"))
        .await
        .unwrap();
    let stored_first = stored(&mut tx, 2).await;
    assert_eq!(stored_first, ("1".into(), "2026-05-01 10:00:00".into()));
    let second = sync(&tx, &s, Some(InsertFill::Value(at(9))));
    let applied = apply_synchronous(&mut tx, &second, &batch(&s, 2, "1"))
        .await
        .unwrap();
    assert!(!applied.rows()[0].inserted());
    assert_eq!(
        stored(&mut tx, 2).await,
        ("2".into(), "2026-05-01 10:00:00".into())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The build-time refusals against the installed model.
#[tokio::test]
async fn fills_are_refused_by_name_against_the_installed_model() {
    let h = harness().await;
    let tx = h.begin().await.unwrap();
    let registry = tx.registry().clone();
    let model = registry.model(MODEL).unwrap();
    let s = shape();
    let apply = || SyncApply::new(&s, TABLE, CONSTRAINT, Vec::new()).unwrap();
    let fill = InsertFill::TransactionTime;
    let prefix = "H27 InvalidDefinition: h27fill.stock: insert fill column";
    for (column, reason) in [
        ("nope", "does not exist on model h27fill.stock"),
        ("twice", "is a generated column"),
        ("product", "is a grain or measure column"),
        ("quantity", "is a grain or measure column"),
    ] {
        let error = apply().with_insert_fill(model, column, fill).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(error.message(), format!("{prefix} {column} {reason}"));
    }
    let error = apply()
        .with_insert_fill(model, "received", fill)
        .unwrap()
        .with_insert_fill(model, "received", fill)
        .unwrap_err();
    assert_eq!(error.message(), format!("{prefix} received is named twice"));
    drop(tx);
    h.cleanup().await.unwrap();
}
