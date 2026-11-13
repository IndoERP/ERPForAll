// SPDX-License-Identifier: MIT OR Apache-2.0
//! slice D2: the synchronous guarded apply against PostgreSQL
//! (int8 attribution),
//! on a generic stock-shaped fixture: a destination table with a typed grain (one nullable part),
//! an exact `numeric` measure and an `int8` measure, plus a receipt date and an addon column that
//! the apply must never touch.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::num::NonZeroU32;

use erp_fields::{Decimal, ErrorKind};
use erp_sdk::delta::{
    CanonicalKey, DeltaLimits, GrainColumn, GrainType, GrainValue, MeasureColumn, MeasureGuard,
    MeasureType, MeasureValue, MeasureVector, NettedBatch, ProjectionShape, SyncApply, Uuid16,
    apply_synchronous,
};
use erp_sdk::{BeginOptions, Isolation, SUPERUSER_ID, Transaction};
use erp_sql::quote;
use erp_test_models::base_users;
use erp_testkit::{Harness, assert_query_count};
use time::{Date, Month, PrimitiveDateTime, Time};

const TABLE: &str = "h27_stock";
const CONSTRAINT: &str = "h27_stock_grain_key";
const QUANTITY_REFUSAL: &str = "H27 test: quantity would drop below zero";
const RESERVED_REFUSAL: &str = "H27 test: reserved would drop below zero";

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap()
}

fn shape() -> ProjectionShape {
    ProjectionShape::new(
        "h27.stock",
        NonZeroU32::new(1).unwrap(),
        vec![
            GrainColumn::new("product", GrainType::Int8, false),
            GrainColumn::new("lot", GrainType::Int8, true),
        ],
        vec![
            MeasureColumn::new(
                "quantity",
                MeasureType::Numeric {
                    precision: 12,
                    scale: 3,
                },
            ),
            MeasureColumn::new("reserved", MeasureType::Int8),
        ],
        &DeltaLimits::default(),
    )
    .unwrap()
}

fn apply(s: &ProjectionShape) -> SyncApply {
    SyncApply::new(
        s,
        TABLE,
        CONSTRAINT,
        vec![
            MeasureGuard::at_least(
                "quantity",
                MeasureValue::Numeric(Decimal::ZERO),
                QUANTITY_REFUSAL,
            ),
            MeasureGuard::at_least("reserved", MeasureValue::Int8(0), RESERVED_REFUSAL),
        ],
    )
    .unwrap()
}

/// One row of the fixture: product, lot, quantity and reserved (text, so no literal is rounded).
type Spec = (i64, Option<i64>, &'static str, i64);

fn key(s: &ProjectionShape, product: i64, lot: Option<i64>) -> CanonicalKey {
    s.admit_key(vec![
        GrainValue::Int8(product),
        lot.map_or(GrainValue::Null, GrainValue::Int8),
    ])
    .unwrap()
}

fn vector(s: &ProjectionShape, quantity: &str, reserved: i64) -> MeasureVector {
    s.admit_measures(vec![
        MeasureValue::Numeric(quantity.parse().unwrap()),
        MeasureValue::Int8(reserved),
    ])
    .unwrap()
}

fn contributions(s: &ProjectionShape, specs: &[Spec]) -> Vec<(CanonicalKey, MeasureVector)> {
    specs
        .iter()
        .map(|(p, l, q, r)| (key(s, *p, *l), vector(s, q, *r)))
        .collect()
}

fn net(s: &ProjectionShape, specs: &[Spec]) -> NettedBatch {
    s.net(contributions(s, specs), &DeltaLimits::default())
        .unwrap()
}

/// The destination table (built from the shipped fragments), a source table, an addon column and
/// a receipt column of the nonadditive state the apply must leave alone.
async fn create(tx: &mut Transaction, s: &ProjectionShape) {
    create_with_key(tx, s, &s.grain_key_definition()).await;
}

/// As [`create`], with the key constraint's definition chosen by the caller.
async fn create_with_key(tx: &mut Transaction, s: &ProjectionShape, key_definition: &str) {
    let mut parts = vec!["id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY".to_owned()];
    parts.extend(s.grain_column_defs());
    parts.extend(s.measure_column_defs());
    parts.push("receipt_date date".to_owned());
    parts.push("addon_note text".to_owned());
    parts.push(format!("CONSTRAINT {} {key_definition}", quote(CONSTRAINT)));
    for (name, check) in s.measure_check_constraints().unwrap() {
        parts.push(format!("CONSTRAINT {} {check}", quote(&name)));
    }
    tx.execute(
        &format!("CREATE TABLE {} ({})", quote(TABLE), parts.join(", ")),
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "CREATE TABLE h27_source (id int PRIMARY KEY, quantity numeric NOT NULL)",
        &[],
    )
    .await
    .unwrap();
    tx.execute("INSERT INTO h27_source VALUES (1, 100)", &[])
        .await
        .unwrap();
}

async fn seed(tx: &mut Transaction, rows: &[Spec]) {
    for (p, l, q, r) in rows {
        tx.execute(
            "INSERT INTO h27_stock (product, lot, quantity, reserved, receipt_date, addon_note) \
             VALUES ($1, $2, $3::text::numeric, $4, DATE '2024-01-05', 'keep')",
            &[p, l, &q.to_string(), r],
        )
        .await
        .unwrap();
    }
}

/// A row as stored: grain, exact measures (admitted by our code), receipt and addon columns.
#[derive(Debug, PartialEq)]
struct Stored {
    product: i64,
    lot: Option<i64>,
    measures: Vec<MeasureValue>,
    receipt: Option<String>,
    note: Option<String>,
    id: i64,
}

async fn stored(tx: &mut Transaction, s: &ProjectionShape) -> Vec<Stored> {
    let rows = tx
        .query(
            "SELECT product, lot, quantity::text, reserved, receipt_date::text, addon_note, id \
             FROM h27_stock ORDER BY product, lot NULLS LAST",
            &[],
        )
        .await
        .unwrap();
    rows.iter()
        .map(|r| {
            let quantity: String = r.get(2);
            let admitted = s
                .admit_measures(vec![
                    MeasureValue::Numeric(quantity.parse().unwrap()),
                    MeasureValue::Int8(r.get(3)),
                ])
                .unwrap();
            Stored {
                product: r.get(0),
                lot: r.get(1),
                measures: s.measure_values(&admitted).unwrap(),
                receipt: r.get(4),
                note: r.get(5),
                id: r.get(6),
            }
        })
        .collect()
}

async fn source_quantity(tx: &mut Transaction) -> String {
    tx.query("SELECT quantity::text FROM h27_source WHERE id = 1", &[])
        .await
        .unwrap()[0]
        .get(0)
}

/// Existing-row state plus the batch's net, summed by our checked arithmetic.
fn expected_sum(
    s: &ProjectionShape,
    existing: Option<&Stored>,
    delta: &MeasureVector,
) -> Vec<MeasureValue> {
    let base = match existing {
        Some(row) => s.admit_measures(row.measures.clone()).unwrap(),
        None => vector(s, "0", 0),
    };
    s.measure_values(&base.checked_add(s, delta).unwrap())
        .unwrap()
}

fn grain_of(key: &CanonicalKey) -> (i64, Option<i64>) {
    let [GrainValue::Int8(product), lot] = key.values() else {
        panic!("unexpected grain {:?}", key.values())
    };
    let lot = match lot {
        GrainValue::Int8(l) => Some(*l),
        GrainValue::Null => None,
        other => panic!("unexpected lot {other:?}"),
    };
    (*product, lot)
}

/// The netted transition commits with the source row: repeated and NULL keys in one statement,
/// exact values, preserved receipt and addon columns, new keys inserted, and the source write of
/// the same transaction visible with the projection after commit.
async fn committed_transition() {
    let h = harness().await;
    let s = shape();
    let sync = apply(&s);
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &s).await;
    seed(
        &mut tx,
        &[
            (1, None, "10.000", 5),
            (2, Some(7), "4.000", 1),
            (9, None, "1.000", 1),
        ],
    )
    .await;
    tx.commit().await.unwrap();

    let (before_rows, source_before) = {
        let mut tx = h.begin().await.unwrap();
        let state = (stored(&mut tx, &s).await, source_quantity(&mut tx).await);
        tx.rollback().await.unwrap();
        state
    };
    // The source after the adapter's own write, from our decimal arithmetic.
    let source_expected = source_before
        .parse::<Decimal>()
        .unwrap()
        .checked_sub(Decimal::from(4))
        .unwrap();
    assert_ne!(source_expected, source_before.parse::<Decimal>().unwrap());

    // Repeated keys, a NULL key part, a pair that nets to zero, new keys, any input order.
    let specs: [Spec; 9] = [
        (4, Some(2), "3.250", 2),
        (1, None, "2.500", -1),
        (3, None, "6.000", 3),
        (2, Some(7), "1.000", 0),
        (1, None, "-0.500", -1),
        (2, Some(7), "-1.000", 0),
        (4, Some(2), "0.000", 0),
        (3, None, "0.000", 0),
        (9, None, "-1.000", -1),
    ];
    let batch = net(&s, &specs);
    // The pair on (2,7) nets to zero and is dropped; keys are distinct and canonical.
    assert!(batch.len() < specs.len());
    assert!(!batch.keys().iter().any(|k| grain_of(k) == (2, Some(7))));

    let mut tx = h.begin().await.unwrap();
    // The source write of the same transaction (the adapter's own statement).
    tx.execute(
        "UPDATE h27_source SET quantity = quantity - 4 WHERE id = 1",
        &[],
    )
    .await
    .unwrap();
    let applied = assert_query_count!(tx, >= 1, <= 1, {
        apply_synchronous(&mut tx, &sync, &batch).await.unwrap()
    });
    assert_eq!(applied.rows().len(), batch.len());
    let after = stored(&mut tx, &s).await;
    assert_eq!(
        source_quantity(&mut tx).await.parse::<Decimal>().unwrap(),
        source_expected
    );

    for ((k, delta), applied_row) in batch.keys().iter().zip(batch.vectors()).zip(applied.rows()) {
        let (product, lot) = grain_of(k);
        let existing = before_rows
            .iter()
            .find(|r| (r.product, r.lot) == (product, lot));
        let row = after
            .iter()
            .find(|r| (r.product, r.lot) == (product, lot))
            .unwrap();
        assert_eq!(
            row.measures,
            expected_sum(&s, existing, delta),
            "{product} {lot:?}"
        );
        assert_eq!(applied_row.measures(), row.measures.as_slice());
        assert_eq!(applied_row.inserted(), existing.is_none());
        match existing {
            Some(old) => {
                assert_eq!(row.id, old.id);
                assert_eq!(row.receipt, old.receipt);
                assert_eq!(row.note, old.note);
            }
            None => {
                assert_eq!(row.receipt, None);
                assert_eq!(row.note, None);
            }
        }
    }
    // The netted-to-zero key was not touched; the zero-delta rows of the other keys changed
    // nothing but their own nets.
    let untouched = after
        .iter()
        .find(|r| (r.product, r.lot) == (2, Some(7)))
        .unwrap();
    let old = before_rows
        .iter()
        .find(|r| (r.product, r.lot) == (2, Some(7)))
        .unwrap();
    assert_eq!(untouched, old);
    tx.commit().await.unwrap();

    // After commit, read back from the database: source and projection changed together.
    let mut tx = h.begin().await.unwrap();
    assert_eq!(stored(&mut tx, &s).await, after);
    assert_eq!(
        source_quantity(&mut tx).await.parse::<Decimal>().unwrap(),
        source_expected
    );
    tx.rollback().await.unwrap();
}

/// committed netted transition changes the source row and the projection (read back from the
/// database); every refusal (existing key, first key, either guard) poisons the commit and leaves
/// source, siblings, receipt and addon columns as they were.
#[tokio::test]
async fn source_and_projection_commit_or_roll_back_together() {
    committed_transition().await;
    refusals_roll_back().await;
}

/// Run one batch in a fresh transaction after a source write, expecting a refusal; check the
/// refusal, that the transaction cannot commit, then that nothing of it survived the rollback.
async fn refused(
    h: &Harness,
    s: &ProjectionShape,
    specs: &[Spec],
    message: &str,
    bad: (i64, Option<i64>),
) {
    let sync = apply(s);
    let (rows_before, source_before) = {
        let mut tx = h.begin().await.unwrap();
        let state = (stored(&mut tx, s).await, source_quantity(&mut tx).await);
        tx.rollback().await.unwrap();
        state
    };
    let batch = net(s, specs);
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE h27_source SET quantity = quantity - 1 WHERE id = 1",
        &[],
    )
    .await
    .unwrap();
    let failure = apply_synchronous(&mut tx, &sync, &batch).await.unwrap_err();
    assert_eq!(failure.error().kind(), ErrorKind::Validation);
    assert_eq!(failure.error().message(), message);
    assert_eq!(failure.error().sqlstate(), None);
    let refused_key = failure.refused().expect("the refused key");
    assert_eq!(grain_of(refused_key), bad);
    assert!(tx.is_poisoned());
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::Validation);
    assert_eq!(commit.message(), message);
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    assert_eq!(stored(&mut tx, s).await, rows_before);
    assert_eq!(source_quantity(&mut tx).await, source_before);
    tx.rollback().await.unwrap();
}

/// The refusals: an existing key taking a negative delta, the first-key and existing-key refusals
/// with the consumer's text, a refused commit, and after rollback the source row, sibling keys and
/// receipt/addon columns unchanged.
async fn refusals_roll_back() {
    let h = harness().await;
    let s = shape();
    let sync = apply(&s);
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &s).await;
    seed(&mut tx, &[(1, None, "10.000", 10), (2, None, "4.000", 2)]).await;
    tx.commit().await.unwrap();

    // An existing key of 10 reserved takes -3 (existing-key success: nothing refused).
    let mut tx = h.begin().await.unwrap();
    let stored_before = stored(&mut tx, &s).await;
    let ok = net(&s, &[(1, None, "0", -1), (1, None, "0", -2)]);
    assert_eq!(ok.len(), 1, "the repeated key nets to one row");
    let applied = apply_synchronous(&mut tx, &sync, &ok).await.unwrap();
    assert_eq!(applied.rows().len(), 1);
    assert!(!applied.rows()[0].inserted());
    let after = stored(&mut tx, &s).await;
    let row = after.iter().find(|r| r.product == 1).unwrap();
    let seeded = stored_before.iter().find(|r| r.product == 1).unwrap();
    assert_eq!(
        row.measures,
        expected_sum(&s, Some(seeded), &ok.vectors()[0])
    );
    assert_eq!(row.id, seeded.id);
    assert_eq!(row.receipt, seeded.receipt);
    assert_eq!(row.note, seeded.note);
    assert_eq!(row.receipt.as_deref(), Some("2024-01-05"));
    tx.rollback().await.unwrap();

    // First key refused: an absent key with -3 (insert arm), siblings in the same batch are
    // good updates and good inserts that must not survive.
    refused(
        &h,
        &s,
        &[(1, None, "1", 0), (5, None, "0", 2), (6, None, "0", -3)],
        RESERVED_REFUSAL,
        (6, None),
    )
    .await;
    // Existing key refused: 2 with -3 (update arm).
    refused(
        &h,
        &s,
        &[
            (1, None, "1", 0),
            (2, None, "0", -1),
            (7, Some(1), "5", 1),
            (2, None, "0", -2),
        ],
        RESERVED_REFUSAL,
        (2, None),
    )
    .await;
    // The quantity guard, on the numeric measure, with its own text.
    refused(
        &h,
        &s,
        &[(1, None, "1", 0), (2, None, "-5", 0)],
        QUANTITY_REFUSAL,
        (2, None),
    )
    .await;
    // A NULL lot key part is the same key: (1,NULL) exists, so a negative delta reaches the
    // update arm and is refused there, never inserted as a second row.
    refused(&h, &s, &[(1, None, "-11", 0)], QUANTITY_REFUSAL, (1, None)).await;
}

/// by the apply, not an anonymous 22003; the source-unchanged half comes with D3)
#[tokio::test]
async fn out_of_range_sums_on_existing_keys_are_named_not_anonymous() {
    let h = harness().await;
    let s = shape();
    let sync = apply(&s);
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &s).await;
    tx.execute(
        "INSERT INTO h27_stock (product, lot, quantity, reserved) \
         VALUES (1, NULL, 999999999.999, 9223372036854775807)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    for (specs, measure) in [
        ([(1, None, "0", 1)], "reserved"),
        ([(1, None, "0.001", 0)], "quantity"),
    ] {
        let batch = net(&s, &specs);
        let mut tx = h.begin().await.unwrap();
        let failure = apply_synchronous(&mut tx, &sync, &batch).await.unwrap_err();
        assert_eq!(failure.error().kind(), ErrorKind::Validation);
        assert_eq!(
            failure.error().message(),
            format!("H27 NumericOutOfRange: h27.stock: {measure}")
        );
        assert_eq!(failure.error().sqlstate(), None);
        assert_eq!(grain_of(failure.refused().unwrap()), (1, None));
        assert!(tx.is_poisoned());
        tx.rollback().await.unwrap();
    }
}

const GRAINS_TABLE: &str = "h27_grains";
const GRAINS_CONSTRAINT: &str = "h27_grains_key";

/// One grain column of each type, one nullable, and a single int8 measure.
fn grains_shape() -> ProjectionShape {
    ProjectionShape::new(
        "h27.grains",
        NonZeroU32::new(1).unwrap(),
        vec![
            GrainColumn::new("flag", GrainType::Bool, false),
            GrainColumn::new("day", GrainType::Date, false),
            GrainColumn::new("stamp", GrainType::TimestampTz, false),
            GrainColumn::new("ident", GrainType::Uuid, false),
            GrainColumn::new("label", GrainType::Text { max_bytes: 32 }, false),
            GrainColumn::new("opt", GrainType::Text { max_bytes: 32 }, true),
        ],
        vec![MeasureColumn::new("n", MeasureType::Int8)],
        &DeltaLimits::default(),
    )
    .unwrap()
}

/// refusal and rollback halves are `source_and_projection_commit_or_roll_back_together`)
#[tokio::test]
async fn every_grain_type_updates_the_existing_row() {
    let h = harness().await;
    let s = grains_shape();
    let mut tx = h.begin().await.unwrap();
    let mut parts = vec!["id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY".to_owned()];
    parts.extend(s.grain_column_defs());
    parts.extend(s.measure_column_defs());
    parts.push(format!(
        "CONSTRAINT {} {}",
        quote(GRAINS_CONSTRAINT),
        s.grain_key_definition()
    ));
    for (name, check) in s.measure_check_constraints().unwrap() {
        parts.push(format!("CONSTRAINT {} {check}", quote(&name)));
    }
    tx.execute(
        &format!(
            "CREATE TABLE {} ({})",
            quote(GRAINS_TABLE),
            parts.join(", ")
        ),
        &[],
    )
    .await
    .unwrap();
    // The existing row is written with SQL literals, the key below is built from Rust values.
    tx.execute(
        "INSERT INTO h27_grains (flag, day, stamp, ident, label, opt, n) VALUES (true, \
         DATE '2024-02-29', TIMESTAMPTZ '2024-02-29 12:34:56.789012+00', \
         '123e4567-e89b-12d3-a456-426614174000', 'abc', NULL, 5)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let uuid = Uuid16::from_bytes([
        0x12, 0x3e, 0x45, 0x67, 0xe8, 0x9b, 0x12, 0xd3, 0xa4, 0x56, 0x42, 0x66, 0x14, 0x17, 0x40,
        0x00,
    ]);
    let day = Date::from_calendar_date(2024, Month::February, 29).unwrap();
    let stamp = PrimitiveDateTime::new(day, Time::from_hms_micro(12, 34, 56, 789_012).unwrap())
        .assume_utc();
    let key = s
        .admit_key(vec![
            GrainValue::Bool(true),
            GrainValue::Date(day),
            GrainValue::TimestampTz(stamp),
            GrainValue::Uuid(uuid),
            GrainValue::Text("abc".to_owned()),
            GrainValue::Null,
        ])
        .unwrap();
    let delta = s.admit_measures(vec![MeasureValue::Int8(3)]).unwrap();
    let batch = s
        .net(vec![(key, delta.clone())], &DeltaLimits::default())
        .unwrap();
    let sync = SyncApply::new(&s, GRAINS_TABLE, GRAINS_CONSTRAINT, vec![]).unwrap();

    let mut tx = h.begin().await.unwrap();
    let id_before: i64 = tx.query("SELECT id FROM h27_grains", &[]).await.unwrap()[0].get(0);
    let applied = apply_synchronous(&mut tx, &sync, &batch).await.unwrap();
    assert_eq!(applied.rows().len(), 1);
    assert!(!applied.rows()[0].inserted());
    let expected = s
        .measure_values(
            &s.admit_measures(vec![MeasureValue::Int8(5)])
                .unwrap()
                .checked_add(&s, &delta)
                .unwrap(),
        )
        .unwrap();
    assert_eq!(applied.rows()[0].measures(), expected.as_slice());
    let rows = tx.query("SELECT id, n FROM h27_grains", &[]).await.unwrap();
    assert_eq!(rows.len(), 1, "no new row");
    assert_eq!(rows[0].get::<_, i64>(0), id_before);
    assert_eq!(
        vec![MeasureValue::Int8(rows[0].get(1))],
        expected,
        "the existing row was updated"
    );
    tx.commit().await.unwrap();
}

/// never a second row)
#[tokio::test]
async fn a_key_constraint_that_treats_null_grains_as_distinct_fails_loudly() {
    let h = harness().await;
    let s = shape();
    let sync = apply(&s);
    let mut tx = h.begin().await.unwrap();
    // Plain UNIQUE: NULLs are distinct, so (1, NULL) never conflicts with itself.
    create_with_key(&mut tx, &s, "UNIQUE (product, lot)").await;
    seed(&mut tx, &[(1, None, "10.000", 5)]).await;
    tx.commit().await.unwrap();

    let batch = net(&s, &[(1, None, "2.000", 1)]);
    let mut tx = h.begin().await.unwrap();
    let failure = apply_synchronous(&mut tx, &sync, &batch).await.unwrap_err();
    assert_eq!(failure.error().kind(), ErrorKind::Internal);
    assert_eq!(
        failure.error().message(),
        "H27 ApplyUnattributed: h27.stock"
    );
    assert_eq!(grain_of(failure.refused().unwrap()), (1, None));
    assert!(tx.is_poisoned());
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::Internal);
    assert_eq!(commit.message(), "H27 ApplyUnattributed: h27.stock");
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = stored(&mut tx, &s).await;
    assert_eq!(rows.len(), 1, "no duplicate row after the rollback");
    assert_eq!(
        rows[0].measures,
        s.measure_values(&vector(&s, "10.000", 5)).unwrap()
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn read_committed_is_refused_before_anything_is_sent() {
    let h = harness().await;
    let s = shape();
    let sync = apply(&s);
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &s).await;
    seed(&mut tx, &[(1, None, "10.000", 5)]).await;
    tx.commit().await.unwrap();

    let batch = net(&s, &[(1, None, "1.000", 1), (2, None, "1.000", 1)]);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let failure = apply_synchronous(&mut tx, &sync, &batch).await.unwrap_err();
    assert_eq!(failure.error().kind(), ErrorKind::Programming);
    assert_eq!(
        failure.error().message(),
        "H27 Unsupported: synchronous apply at READ COMMITTED; requires a later release"
    );
    assert!(failure.refused().is_none());
    // Nothing was written, so nothing is poisoned and the transaction can still commit.
    assert!(!tx.is_poisoned());
    assert_eq!(stored(&mut tx, &s).await.len(), 1);
    tx.commit().await.unwrap();
}
