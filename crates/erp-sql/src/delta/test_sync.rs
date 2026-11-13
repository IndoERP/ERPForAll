// SPDX-License-Identifier: MIT OR Apache-2.0
//! Unit tests of `erp_sql::delta::sync` (slice D2).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use super::*;

fn shape() -> ProjectionShape {
    ProjectionShape::new(
        "t.stock",
        NonZeroU32::new(1).unwrap(),
        vec![GrainColumn::new("k", GrainType::Int8, true)],
        vec![
            MeasureColumn::new("n", MeasureType::Int8),
            MeasureColumn::new(
                "a",
                MeasureType::Numeric {
                    precision: 10,
                    scale: 2,
                },
            ),
        ],
        &DeltaLimits::default(),
    )
    .unwrap()
}

fn key(s: &ProjectionShape, k: Option<i64>) -> CanonicalKey {
    s.admit_key(vec![k.map_or(GrainValue::Null, GrainValue::Int8)])
        .unwrap()
}

fn vector(s: &ProjectionShape, n: i64, a: &str) -> MeasureVector {
    s.admit_measures(vec![
        MeasureValue::Int8(n),
        MeasureValue::Numeric(a.parse().unwrap()),
    ])
    .unwrap()
}

fn net(
    s: &ProjectionShape,
    rows: &[(Option<i64>, i64, &str)],
    limits: &DeltaLimits,
) -> Result<NettedBatch> {
    s.net(
        rows.iter()
            .map(|(k, n, a)| (key(s, *k), vector(s, *n, a)))
            .collect(),
        limits,
    )
}

fn grain(k: &CanonicalKey) -> Option<i64> {
    match k.values() {
        [GrainValue::Int8(v)] => Some(*v),
        [GrainValue::Null] => None,
        other => panic!("unexpected {other:?}"),
    }
}

fn nets(s: &ProjectionShape, b: &NettedBatch) -> Vec<Vec<MeasureValue>> {
    b.vectors()
        .iter()
        .map(|v| s.measure_values(v).unwrap())
        .collect()
}

fn refuse<T: std::fmt::Debug>(r: Result<T>, kind: ErrorKind, message: &str) {
    let e = r.unwrap_err();
    assert_eq!(e.kind(), kind);
    assert_eq!(e.message(), message);
}

#[test]
fn netting_is_order_independent_and_range_checks_only_the_net() {
    let s = shape();
    let contributions = [
        (Some(1), i64::MAX, "0"),
        (Some(1), 1, "0"),
        (Some(1), -1, "0"),
    ];
    let mut results = Vec::new();
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let rows: Vec<_> = order.iter().map(|i| contributions[*i]).collect();
        let b = net(&s, &rows, &DeltaLimits::default()).unwrap();
        assert_eq!(b.len(), 1);
        results.push(nets(&s, &b));
    }
    let expected = s.measure_values(&vector(&s, i64::MAX, "0")).unwrap();
    assert!(results.iter().all(|r| r == &vec![expected.clone()]));

    // A net out of range is refused, whatever the order.
    refuse(
        net(
            &s,
            &[(Some(1), i64::MAX, "0"), (Some(1), 1, "0")],
            &DeltaLimits::default(),
        ),
        ErrorKind::Validation,
        "H27 NumericOutOfRange: t.stock: n",
    );
    refuse(
        net(
            &s,
            &[(Some(1), 0, "99999999.99"), (Some(1), 0, "0.01")],
            &DeltaLimits::default(),
        ),
        ErrorKind::Validation,
        "H27 NumericOutOfRange: t.stock: a",
    );
}

#[test]
fn zero_nets_are_dropped_null_keys_net_together_and_order_is_canonical() {
    let s = shape();
    let b = net(
        &s,
        &[
            (None, 1, "1.50"),
            (Some(9), 2, "0"),
            (Some(3), 5, "0.10"),
            (None, 2, "-0.50"),
            (Some(3), -5, "-0.10"),
            (Some(1), 0, "0"),
        ],
        &DeltaLimits::default(),
    )
    .unwrap();
    // key 3 nets to zero and key 1 is zero already; NULL sorts last.
    assert_eq!(
        b.keys().iter().map(grain).collect::<Vec<_>>(),
        [Some(9), None]
    );
    assert_eq!(b.keys().len(), b.vectors().len());
    assert_eq!(
        nets(&s, &b)[1],
        s.measure_values(&vector(&s, 3, "1.00")).unwrap()
    );
    assert!(!b.is_empty());
    let none = net(
        &s,
        &[(Some(1), 1, "0"), (Some(1), -1, "0")],
        &DeltaLimits::default(),
    );
    assert!(none.unwrap().is_empty());
}

#[test]
fn batch_row_and_byte_limits_refuse_before_any_sql() {
    let s = shape();
    let rows = [(Some(1), 1, "0"), (Some(2), 1, "0")];
    // Each row: int8 key 8 + int8 measure 8 + numeric measure 16 = 32 bytes.
    let at = DeltaLimits::default().with_batch(2, 64).unwrap();
    assert_eq!(net(&s, &rows, &at).unwrap().len(), 2);
    refuse(
        net(
            &s,
            &rows,
            &DeltaLimits::default().with_batch(1, 64).unwrap(),
        ),
        ErrorKind::ResourceExhausted,
        "H27 LimitExceeded: batch rows",
    );
    refuse(
        net(
            &s,
            &rows,
            &DeltaLimits::default().with_batch(2, 63).unwrap(),
        ),
        ErrorKind::ResourceExhausted,
        "H27 LimitExceeded: batch bytes",
    );
    // A NULL key part counts one byte.
    let null = net(
        &s,
        &[(None, 1, "0")],
        &DeltaLimits::default().with_batch(1, 25).unwrap(),
    );
    assert_eq!(null.unwrap().len(), 1);
    refuse(
        net(
            &s,
            &[(None, 1, "0")],
            &DeltaLimits::default().with_batch(1, 24).unwrap(),
        ),
        ErrorKind::ResourceExhausted,
        "H27 LimitExceeded: batch bytes",
    );
}

#[test]
fn batch_limits_can_be_lowered_never_zeroed_or_raised() {
    let d = DeltaLimits::default();
    assert_eq!((d.batch_rows(), d.batch_bytes()), (2_000, 8_388_608));
    let low = d.with_batch(1, 1).unwrap();
    assert_eq!((low.batch_rows(), low.batch_bytes()), (1, 1));
    assert_eq!(low.grain_columns(), 16);
    for (rows, bytes, reason) in [
        (0, 1, "batch rows must be 1..=2000"),
        (2_001, 1, "batch rows must be 1..=2000"),
        (1, 0, "batch bytes must be 1..=8388608"),
        (1, 8_388_609, "batch bytes must be 1..=8388608"),
    ] {
        refuse(
            d.with_batch(rows, bytes),
            ErrorKind::Programming,
            &format!("H27 InvalidDefinition: <limits>: {reason}"),
        );
    }
    // `lower` keeps the batch defaults.
    let lowered = DeltaLimits::lower(1, 1, 8, 8).unwrap();
    assert_eq!(lowered.batch_rows(), 2_000);
}

#[test]
fn keys_vectors_and_batches_of_another_shape_are_refused() {
    let s = shape();
    let other = ProjectionShape::new(
        "t.other",
        NonZeroU32::new(1).unwrap(),
        vec![GrainColumn::new("k", GrainType::Int8, true)],
        vec![MeasureColumn::new("n", MeasureType::Int8)],
        &DeltaLimits::default(),
    )
    .unwrap();
    let foreign_key = other.admit_key(vec![GrainValue::Int8(1)]).unwrap();
    refuse(
        s.net(
            vec![(foreign_key, vector(&s, 1, "0"))],
            &DeltaLimits::default(),
        ),
        ErrorKind::Programming,
        "H27 InvalidDefinition: t.stock: key was admitted against another shape",
    );
    let foreign_vector = other.admit_measures(vec![MeasureValue::Int8(1)]).unwrap();
    refuse(
        s.net(
            vec![(key(&s, Some(1)), foreign_vector)],
            &DeltaLimits::default(),
        ),
        ErrorKind::Programming,
        "H27 InvalidDefinition: t.stock: vector belongs to another shape",
    );
    let batch = net(&s, &[(Some(1), 1, "0")], &DeltaLimits::default()).unwrap();
    let apply = SyncApply::new(&other, "t_other", "t_other_key", vec![]).unwrap();
    let failure = apply.interpret(&batch, &[]).unwrap_err();
    assert_eq!(failure.error().kind(), ErrorKind::Programming);
    assert_eq!(
        failure.error().message(),
        "H27 InvalidDefinition: t.other: batch was netted against another shape"
    );
    assert!(failure.refused().is_none());
}

fn guards() -> Vec<MeasureGuard> {
    vec![
        MeasureGuard::at_least("n", MeasureValue::Int8(0), "refuse n"),
        MeasureGuard::at_least("a", MeasureValue::Numeric("0".parse().unwrap()), "refuse a"),
    ]
}

fn apply() -> SyncApply {
    SyncApply::new(&shape(), "t_stock", "t_stock_key", guards()).unwrap()
}

#[test]
fn declarations_are_validated_with_the_complete_message() {
    let s = shape();
    let bad = |table: &str, constraint: &str, guards: Vec<MeasureGuard>, reason: &str| {
        refuse(
            SyncApply::new(&s, table, constraint, guards),
            ErrorKind::Programming,
            &format!("H27 InvalidDefinition: t.stock: {reason}"),
        );
    };
    SyncApply::new(&s, "t_stock", "t_stock_key", guards()).unwrap();
    bad("T", "k", vec![], "table name is not valid");
    bad("", "k", vec![], "table name is not valid");
    bad(&"t".repeat(64), "k", vec![], "table name is not valid");
    bad("t", "bad-name", vec![], "constraint name is not valid");
    bad(
        "t",
        "k",
        vec![MeasureGuard::at_least("zzz", MeasureValue::Int8(0), "x")],
        "a guard names an unknown measure",
    );
    bad(
        "t",
        "k",
        vec![
            MeasureGuard::at_least("n", MeasureValue::Int8(0), "x"),
            MeasureGuard::at_least("n", MeasureValue::Int8(1), "y"),
        ],
        "measure n has two guards",
    );
    bad(
        "t",
        "k",
        vec![MeasureGuard::at_least("n", MeasureValue::Int8(0), "")],
        "guard refusal text of n is empty",
    );
    bad(
        "t",
        "k",
        vec![MeasureGuard::at_least(
            "n",
            MeasureValue::Numeric("1".parse().unwrap()),
            "x",
        )],
        "guard floor of n is not admissible",
    );
    bad(
        "t",
        "k",
        vec![MeasureGuard::at_least(
            "a",
            MeasureValue::Numeric("0.001".parse().unwrap()),
            "x",
        )],
        "guard floor of a is not admissible",
    );
}

#[test]
fn statement_text_has_the_guarded_update_arm_and_the_insert_filter() {
    let s = shape();
    let sql = apply().statement().unwrap();
    let target = quote("__t");
    let sum =
        |c: &str| format!("COALESCE({target}.\"{c}\", 0)::numeric + EXCLUDED.\"{c}\"::numeric");
    let range_n = s.measure_range_predicate(0, &sum("n")).unwrap();
    let range_a = s.measure_range_predicate(1, &sum("a")).unwrap();
    let guard = format!(
        "{range_n} AND {range_a} AND ((COALESCE({target}.\"n\", 0)::numeric + \
         EXCLUDED.\"n\"::numeric) >= (0)) AND ((COALESCE({target}.\"a\", 0)::numeric + \
         EXCLUDED.\"a\"::numeric) >= (0.00))"
    );
    assert!(
        sql.contains(&format!(
            "DO UPDATE SET \"n\" = COALESCE({target}.\"n\", 0) + EXCLUDED.\"n\", \"a\" = \
             COALESCE({target}.\"a\", 0) + EXCLUDED.\"a\" WHERE (EXCLUDED.\"n\" <> 0 OR \
             EXCLUDED.\"a\" <> 0) AND ({guard}) RETURNING"
        )),
        "{sql}"
    );
    let insert_n = s.measure_range_predicate(0, "\"__in\".\"n\"").unwrap();
    let insert_a = s.measure_range_predicate(1, "\"__in\".\"a\"").unwrap();
    assert!(
        sql.contains(&format!(
            "FROM \"__in\" WHERE (({insert_n} AND {insert_a} AND ((\"__in\".\"n\")::numeric \
             >= (0)) AND ((\"__in\".\"a\")::numeric >= (0.00))) OR EXISTS (SELECT FROM \
             \"t_stock\" AS \"__e\" WHERE (\"__in\".\"k\" = \"__e\".\"k\" OR (\"__in\".\"k\" \
             IS NULL AND \"__e\".\"k\" IS NULL)))) ORDER BY \"__ord\" ON CONFLICT ON CONSTRAINT \
             \"t_stock_key\""
        )),
        "{sql}"
    );
    assert!(
        sql.contains("unnest($1::int8[], $2::int8[], $3::numeric[])"),
        "{sql}"
    );
    // A negative floor is parenthesized.
    let negative = SyncApply::new(
        &s,
        "t_stock",
        "t_stock_key",
        vec![MeasureGuard::at_least("n", MeasureValue::Int8(-5), "x")],
    )
    .unwrap()
    .statement()
    .unwrap();
    assert!(negative.contains(">= (-5))"), "{negative}");
}

/// A row of an existing key (not inserted, `existed`).
fn returned(ordinal: i64, written: bool, n: i64, a: &str) -> SyncReturned {
    SyncReturned::new(
        ordinal,
        false,
        written,
        true,
        vec![
            MeasureValue::Int8(n),
            MeasureValue::Numeric(a.parse().unwrap()),
        ],
    )
}

/// A row the statement inserted for an absent key.
fn inserted_row(ordinal: i64, n: i64, a: &str) -> SyncReturned {
    SyncReturned::new(
        ordinal,
        true,
        true,
        false,
        vec![
            MeasureValue::Int8(n),
            MeasureValue::Numeric(a.parse().unwrap()),
        ],
    )
}

fn refused_with(failure: SyncApplyFailure, kind: ErrorKind, message: &str, key: Option<i64>) {
    assert_eq!(failure.error().kind(), kind);
    assert_eq!(failure.error().message(), message);
    assert_eq!(failure.refused().map(grain), Some(key));
}

#[test]
fn interpret_attributes_each_refusal_exactly() {
    let s = shape();
    let a = apply();
    let batch = net(
        &s,
        &[(Some(1), -3, "0"), (Some(2), 0, "-1.00")],
        &DeltaLimits::default(),
    )
    .unwrap();
    // Everything written: the stored values come back in order.
    let good = net(
        &s,
        &[(Some(1), 3, "0"), (Some(2), 0, "-1.00")],
        &DeltaLimits::default(),
    )
    .unwrap();
    let ok = a
        .interpret(&good, &[returned(2, true, 0, "0"), inserted_row(1, 3, "0")])
        .unwrap();
    assert_eq!(ok.rows().len(), 2);
    assert!(ok.rows()[0].inserted());
    assert!(!ok.rows()[1].inserted());
    assert_eq!(ok.rows()[0].measures()[0], MeasureValue::Int8(3));

    // Existing 2 with -3: the update arm refused it (returned unwritten).
    let f = a
        .interpret(
            &batch,
            &[returned(1, false, 2, "5"), returned(2, true, 0, "0")],
        )
        .unwrap_err();
    refused_with(f, ErrorKind::Validation, "refuse n", Some(1));
    // Absent key with -3: the insert arm filtered it (no row).
    let f = a
        .interpret(&batch, &[returned(2, true, 0, "0")])
        .unwrap_err();
    refused_with(f, ErrorKind::Validation, "refuse n", Some(1));
    // The second guard names its own text; the first refused key in canonical order wins.
    let f = a
        .interpret(
            &batch,
            &[returned(1, true, 0, "0"), returned(2, false, 0, "0.50")],
        )
        .unwrap_err();
    refused_with(f, ErrorKind::Validation, "refuse a", Some(2));
    // Both guards violated: the first declared.
    let both = net(&s, &[(Some(4), -1, "-1")], &DeltaLimits::default()).unwrap();
    let f = a.interpret(&both, &[]).unwrap_err();
    refused_with(f, ErrorKind::Validation, "refuse n", Some(4));
}

/// statement refuses before 22003)
#[test]
fn interpret_names_out_of_range_sums_before_floors() {
    let s = shape();
    let a = apply();
    let batch = net(&s, &[(Some(1), 1, "0")], &DeltaLimits::default()).unwrap();
    let f = a
        .interpret(&batch, &[returned(1, false, i64::MAX, "0")])
        .unwrap_err();
    refused_with(
        f,
        ErrorKind::Validation,
        "H27 NumericOutOfRange: t.stock: n",
        Some(1),
    );
    let batch = net(&s, &[(Some(1), 0, "0.01")], &DeltaLimits::default()).unwrap();
    let f = a
        .interpret(&batch, &[returned(1, false, 0, "99999999.99")])
        .unwrap_err();
    refused_with(
        f,
        ErrorKind::Validation,
        "H27 NumericOutOfRange: t.stock: a",
        Some(1),
    );
}

#[test]
fn interpret_fails_loudly_when_nothing_explains_a_row() {
    let s = shape();
    let a = apply();
    let batch = net(&s, &[(Some(1), 1, "0")], &DeltaLimits::default()).unwrap();
    let message = "H27 ApplyUnattributed: t.stock";
    // Unwritten although current + delta is in range and meets every floor.
    let f = a
        .interpret(&batch, &[returned(1, false, 5, "0")])
        .unwrap_err();
    refused_with(f, ErrorKind::Internal, message, Some(1));
    // Missing although the net alone meets every floor.
    let f = a.interpret(&batch, &[]).unwrap_err();
    refused_with(f, ErrorKind::Internal, message, Some(1));
    // Duplicate, zero and out-of-range ordinals, a wrong measure count.
    for rows in [
        vec![returned(1, true, 1, "0"), returned(1, true, 1, "0")],
        vec![returned(0, true, 1, "0")],
        vec![returned(2, true, 1, "0")],
        vec![returned(-1, true, 1, "0")],
        vec![SyncReturned::new(
            1,
            true,
            true,
            false,
            vec![MeasureValue::Int8(1)],
        )],
    ] {
        let f = a.interpret(&batch, &rows).unwrap_err();
        assert_eq!(f.error().kind(), ErrorKind::Internal);
        assert_eq!(f.error().message(), message);
        assert!(f.refused().is_none());
    }
    // The failure converts to its error.
    let f = a.interpret(&batch, &[]).unwrap_err();
    let e: Error = f.into();
    assert_eq!(e.message(), message);
}

#[test]
fn interpret_rechecks_every_written_row() {
    let s = shape();
    let a = apply();
    let batch = net(&s, &[(Some(1), -3, "0")], &DeltaLimits::default()).unwrap();
    // Written below its floor (the insert arm took a negative delta of a key deleted by a
    // concurrent transaction): refused with the guard's own text and the key.
    let f = a
        .interpret(&batch, &[returned(1, true, -3, "0")])
        .unwrap_err();
    refused_with(f, ErrorKind::Validation, "refuse n", Some(1));
    let f = a
        .interpret(&batch, &[inserted_row(1, -3, "0")])
        .unwrap_err();
    refused_with(f, ErrorKind::Validation, "refuse n", Some(1));
    // The numeric guard names its own text.
    let f = a
        .interpret(&batch, &[returned(1, true, 0, "-0.50")])
        .unwrap_err();
    refused_with(f, ErrorKind::Validation, "refuse a", Some(1));
    // A stored measure out of range is named, not trusted.
    let f = a
        .interpret(&batch, &[returned(1, true, 0, "100000000.00")])
        .unwrap_err();
    refused_with(
        f,
        ErrorKind::Validation,
        "H27 NumericOutOfRange: t.stock: a",
        Some(1),
    );
    // An inserted row must hold exactly the net.
    let up = net(&s, &[(Some(1), 1, "0")], &DeltaLimits::default()).unwrap();
    let f = a.interpret(&up, &[inserted_row(1, 9, "0")]).unwrap_err();
    refused_with(
        f,
        ErrorKind::Internal,
        "H27 ApplyUnattributed: t.stock",
        Some(1),
    );
    a.interpret(&up, &[inserted_row(1, 1, "0")]).unwrap();
    // An inserted row whose key existed in the snapshot: a second row for the grain (a key
    // constraint that does not arbitrate it) or a concurrent delete.
    let f = a
        .interpret(
            &up,
            &[SyncReturned::new(
                1,
                true,
                true,
                true,
                vec![
                    MeasureValue::Int8(1),
                    MeasureValue::Numeric("0".parse().unwrap()),
                ],
            )],
        )
        .unwrap_err();
    refused_with(
        f,
        ErrorKind::Internal,
        "H27 ApplyUnattributed: t.stock",
        Some(1),
    );
}

#[test]
fn contribution_cap_refuses_before_netting_and_can_only_be_lowered() {
    let s = shape();
    let d = DeltaLimits::default();
    assert_eq!(d.contribution_rows(), 32_000);
    let low = d.with_contributions(3).unwrap();
    assert_eq!(low.contribution_rows(), 3);
    assert_eq!(low.batch_rows(), 2_000);
    for rows in [0, 32_001] {
        refuse(
            d.with_contributions(rows),
            ErrorKind::Programming,
            "H27 InvalidDefinition: <limits>: contribution rows must be 1..=32000",
        );
    }
    let three = [(Some(1), 1, "0"), (Some(1), 1, "0"), (Some(1), 1, "0")];
    assert_eq!(net(&s, &three, &low).unwrap().len(), 1);
    let four = [
        (Some(1), 1, "0"),
        (Some(1), 1, "0"),
        (Some(1), 1, "0"),
        (Some(1), 1, "0"),
    ];
    refuse(
        net(&s, &four, &low),
        ErrorKind::ResourceExhausted,
        "H27 LimitExceeded: contribution rows",
    );
    // Distinct keys are counted while grouping, so keys that net to zero count too.
    let zero_pairs = [
        (Some(1), 1, "0"),
        (Some(1), -1, "0"),
        (Some(2), 1, "0"),
        (Some(2), -1, "0"),
    ];
    let one_row = DeltaLimits::default().with_batch(1, 64).unwrap();
    refuse(
        net(&s, &zero_pairs, &one_row),
        ErrorKind::ResourceExhausted,
        "H27 LimitExceeded: batch rows",
    );
}

#[test]
fn failure_debug_redacts_the_refused_key() {
    let s = shape();
    let batch = net(&s, &[(Some(424_242), -3, "0")], &DeltaLimits::default()).unwrap();
    let f = apply().interpret(&batch, &[]).unwrap_err();
    assert!(f.refused().is_some());
    let shown = format!("{f:?}");
    assert!(!shown.contains("424242"), "{shown}");
    assert!(shown.contains("refused: Some(<redacted>)"), "{shown}");
    assert!(shown.contains("refuse n"), "{shown}");
    let none = SyncApplyFailure::from(Error::internal("x"));
    assert!(format!("{none:?}").contains("refused: None"));
}

mod insert_fill {
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};

    use super::*;

    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_sync_fill",
        depends: &["base"],
        exact_only: false,
    };

    static FIELDS: [FieldDecl; 7] = [
        FieldDecl::integer("k"),
        FieldDecl::integer("n"),
        FieldDecl::integer("a"),
        FieldDecl::datetime("received").required(true),
        FieldDecl::datetime("counted"),
        FieldDecl::integer("twice").generated("k * 2"),
        FieldDecl::char("code"),
    ];

    static OTHER: [FieldDecl; 1] = [FieldDecl::datetime("received")];

    fn registry() -> Registry {
        let stock: &'static ModelDecl = Box::leak(Box::new(
            ModelDecl::new("test_sync_fill", "t.stock").fields(&FIELDS),
        ));
        let other: &'static ModelDecl = Box::leak(Box::new(
            ModelDecl::new("test_sync_fill", "t.other").fields(&OTHER),
        ));
        RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([stock, other])
            .build()
            .unwrap()
    }

    fn at(day: u8) -> DateTime {
        DateTime::from_parts(
            erp_fields::Date::from_ymd(2026, 5, u32::from(day)).unwrap(),
            10,
            0,
            0,
            0,
        )
        .unwrap()
    }

    /// B7: the fills are insert columns only, the transaction time is `now()` in UTC, a
    /// bound value is the parameter after the arrays, and the update arm never names a fill.
    #[test]
    fn insert_fills_render_on_the_insert_arm_only() {
        let registry = registry();
        let model = registry.model("t.stock").unwrap();
        let filled = apply()
            .with_insert_fill(model, "received", InsertFill::TransactionTime)
            .unwrap()
            .with_insert_fill(model, "counted", InsertFill::Value(at(3)))
            .unwrap();
        let sql = filled.statement().unwrap();
        assert!(
            sql.contains("INSERT INTO \"t_stock\" AS \"__t\" (\"k\", \"n\", \"a\", \"received\", \"counted\")"),
            "{sql}"
        );
        assert!(
            sql.contains("SELECT \"k\", \"n\", \"a\", (now() AT TIME ZONE 'UTC'), $4::timestamp FROM \"__in\""),
            "{sql}"
        );
        let update = &sql[sql.find("DO UPDATE SET").unwrap()..];
        assert!(
            !update.contains("received") && !update.contains("counted"),
            "{sql}"
        );
        assert_eq!(filled.insert_fill_values(), vec![at(3)]);
        assert!(apply().insert_fill_values().is_empty());
    }

    /// B7: every refusal names its reason at build time.
    #[test]
    fn insert_fills_are_validated_by_name() {
        let registry = registry();
        let model = registry.model("t.stock").unwrap();
        let other = registry.model("t.other").unwrap();
        let fill = InsertFill::TransactionTime;
        let prefix = "H27 InvalidDefinition: t.stock: insert fill";
        for (model, column, reason) in [
            (
                other,
                "received",
                "model t.other has table t_other, not t_stock",
            ),
            (model, "nope", "column nope does not exist on model t.stock"),
            (model, "twice", "column twice is a generated column"),
            (model, "k", "column k is a grain or measure column"),
            (model, "n", "column n is a grain or measure column"),
            (
                model,
                "code",
                "column code is not a stored timestamp column",
            ),
        ] {
            refuse(
                apply().with_insert_fill(model, column, fill),
                ErrorKind::Programming,
                &format!("{prefix} {reason}"),
            );
        }
        refuse(
            apply()
                .with_insert_fill(model, "received", fill)
                .unwrap()
                .with_insert_fill(model, "received", InsertFill::Value(at(1))),
            ErrorKind::Programming,
            &format!("{prefix} column received is named twice"),
        );
    }
}
