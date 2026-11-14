//! UoM model persistence and hierarchy contracts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;
#[path = "support/guard_fixture.rs"]
mod guard_fixture;

fn text(value: impl Into<Arc<str>>) -> Cell {
    Cell::Str(value.into())
}

fn dec(text: &str) -> Decimal {
    Decimal::from_str_exact(text).unwrap()
}

fn assert_factor_numeric_source(error: &erp_sdk::Error, expected_operation: &str) {
    use erp_addon_uom::error::UomError;
    use erp_sdk::money::MoneyError;
    let source = std::error::Error::source(error)
        .expect("factor rejection must retain its typed numeric-limit source");
    let Some(UomError::Numeric(MoneyError::NumericLimit { operation })) =
        source.downcast_ref::<UomError>()
    else {
        panic!("factor rejection must contain UomError::Numeric(NumericLimit), got {source:?}")
    };
    assert_eq!(*operation, expected_operation);
}

async fn harness() -> Harness {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .await
        .unwrap();
    guard_fixture::install(&h).await;
    h
}

async fn unit(tx: &mut Transaction, name: &str, parent: Option<i64>, factor: &str) -> Records {
    let model = tx.env().records("uom.uom").unwrap();
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("relative_factor", Cell::Decimal(dec(factor)));
    if let Some(parent) = parent {
        vals = vals.set_cell("relative_uom_id", Cell::Id(parent));
    }
    model.create(tx, vec![vals]).await.unwrap()
}

async fn factor(tx: &mut Transaction, record: &Records) -> Decimal {
    match record.cell(tx, "factor").await.unwrap() {
        Cell::Decimal(value) => value,
        other => panic!("expected decimal factor, got {other:?}"),
    }
}

#[tokio::test]
async fn root_child_grandchild_factors_persist_and_paths_follow_parent_store() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    let pack = unit(&mut tx, "Pack of 6", Some(root.ids()[0]), "6").await;
    let carton = unit(&mut tx, "Box of 24", Some(pack.ids()[0]), "4").await;
    assert_eq!(factor(&mut tx, &root).await, dec("1"));
    assert_eq!(factor(&mut tx, &pack).await, dec("6"));
    assert_eq!(factor(&mut tx, &carton).await, dec("24"));
    let expected_path = format!("{}/{}/{}/", root.ids()[0], pack.ids()[0], carton.ids()[0]);
    assert_eq!(
        carton.cell(&mut tx, "parent_path").await.unwrap(),
        text(expected_path)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(factor(&mut tx, &carton).await, dec("24"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn root_relative_factor_must_be_one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("uom.uom")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("Invalid root"))
                            .set_cell("relative_factor", Cell::Decimal(dec("2"))),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), "Reference unit of measure is missing.");
    assert_eq!(
        tx.env()
            .records("uom.uom")
            .unwrap()
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        0
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn zero_relative_factor_is_rejected_by_constraint() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("uom.uom")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("Zero"))
                            .set_cell("relative_uom_id", Cell::Id(root.ids()[0]))
                            .set_cell("relative_factor", Cell::Decimal(Decimal::ZERO)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The database refused the change: A unit of measure needs a conversion ratio other than 0."
    );
    assert_eq!(factor(&mut tx, &root).await, Decimal::ONE);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn sequence_is_computed_and_ancestor_edits_recompute_descendants() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    let pack = unit(&mut tx, "Dozens", Some(root.ids()[0]), "12").await;
    let half = unit(&mut tx, "Half", Some(pack.ids()[0]), "0.5").await;
    assert_eq!(
        pack.cell(&mut tx, "sequence").await.unwrap(),
        Cell::Int(1000)
    );
    assert_eq!(half.cell(&mut tx, "sequence").await.unwrap(), Cell::Int(50));
    assert_eq!(factor(&mut tx, &half).await, dec("6"));
    pack.write(
        &mut tx,
        RawVals::new().set_cell("relative_factor", Cell::Decimal(dec("10"))),
    )
    .await
    .unwrap();
    assert_eq!(factor(&mut tx, &half).await, dec("5"));
    let other = unit(&mut tx, "Other root", None, "1").await;
    pack.write(
        &mut tx,
        RawVals::new().set_cell("relative_uom_id", Cell::Id(other.ids()[0])),
    )
    .await
    .unwrap();
    assert_eq!(
        half.cell(&mut tx, "parent_path").await.unwrap(),
        text(format!(
            "{}/{}/{}/",
            other.ids()[0],
            pack.ids()[0],
            half.ids()[0]
        ))
    );
    assert_eq!(factor(&mut tx, &half).await, dec("5"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn descendant_scale_loss_rolls_back_whole_ancestor_edit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    let parent = unit(&mut tx, "Small", Some(root.ids()[0]), "0.00000000000001").await;
    let child = unit(
        &mut tx,
        "Smaller",
        Some(parent.ids()[0]),
        "0.00000000000001",
    )
    .await;
    assert_eq!(
        factor(&mut tx, &child).await,
        dec("0.0000000000000000000000000001")
    );
    // No caller savepoint: the addon mutation must restore its own descendant closure.
    let error = parent
        .write(
            &mut tx,
            RawVals::new().set_cell("relative_factor", Cell::Decimal(dec("0.000000000000001"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Unit of measure factor cannot be represented exactly as a decimal."
    );
    assert_factor_numeric_source(&error, "uom persisted factor");
    let rows = tx
        .query(
            "SELECT factor::text FROM uom_uom WHERE id = ANY($1) ORDER BY id",
            &[&vec![parent.ids()[0], child.ids()[0]]],
        )
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, String>(0).parse::<Decimal>().unwrap(),
        dec("0.00000000000001")
    );
    assert_eq!(
        rows[1].get::<_, String>(0).parse::<Decimal>().unwrap(),
        dec("0.0000000000000000000000000001")
    );
    assert_eq!(factor(&mut tx, &parent).await, dec("0.00000000000001"));
    assert_eq!(
        factor(&mut tx, &child).await,
        dec("0.0000000000000000000000000001")
    );
    assert!(tx.is_poisoned());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn negative_factor_is_preserved_and_cycle_keeps_old_paths() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    let child = unit(&mut tx, "Negative", Some(root.ids()[0]), "-2").await;
    assert_eq!(factor(&mut tx, &child).await, dec("-2"));
    let error = tx
        .savepoint(async |tx| {
            root.write(
                tx,
                RawVals::new().set_cell("relative_uom_id", Cell::Id(child.ids()[0])),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "This parent would make a loop in the 'Unit of Measure' records: a record cannot sit under itself or under one of its own children. Choose another parent."
    );
    assert_eq!(
        root.cell(&mut tx, "parent_path").await.unwrap(),
        text(format!("{}/", root.ids()[0]))
    );
    assert_eq!(factor(&mut tx, &child).await, dec("-2"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn descendant_magnitude_overflow_keeps_prior_tree() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    let parent = unit(&mut tx, "Big", Some(root.ids()[0]), "1000000").await;
    let middle = unit(&mut tx, "Middle", Some(parent.ids()[0]), "1000000").await;
    let next = unit(&mut tx, "Next", Some(middle.ids()[0]), "1000000").await;
    let almost = unit(&mut tx, "Almost", Some(next.ids()[0]), "1000000").await;
    let child = unit(&mut tx, "Bigger", Some(almost.ids()[0]), "10000").await;
    assert_eq!(
        factor(&mut tx, &child).await,
        dec("10000000000000000000000000000")
    );
    let error = parent
        .write(
            &mut tx,
            RawVals::new().set_cell("relative_factor", Cell::Decimal(dec("10000000"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Unit of measure factor cannot be represented exactly as a decimal."
    );
    assert_factor_numeric_source(&error, "uom exact decimal");
    let rows = tx
        .query(
            "SELECT relative_factor::text, factor::text FROM uom_uom WHERE id = $1",
            &[&parent.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, String>(0).parse::<Decimal>().unwrap(),
        dec("1000000")
    );
    assert_eq!(
        rows[0].get::<_, String>(1).parse::<Decimal>().unwrap(),
        dec("1000000")
    );
    assert_eq!(
        factor(&mut tx, &child).await,
        dec("10000000000000000000000000000")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn display_name_uses_ratio_only_in_formatted_context() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    let dozen = unit(&mut tx, "Dozens", Some(root.ids()[0]), "12").await;
    assert_eq!(
        dozen.cell(&mut tx, "display_name").await.unwrap(),
        text("Dozens")
    );
    let formatted = dozen.with_context([("formatted_display_name", Value::Bool(true))]);
    assert_eq!(
        formatted.cell(&mut tx, "display_name").await.unwrap(),
        text("Dozens\t--12.0 Units--")
    );
    assert_eq!(
        dozen.cell(&mut tx, "display_name").await.unwrap(),
        text("Dozens")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_model_dispatch_uses_fresh_exact_factors() {
    use erp_sdk::money::ExactRatio;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let gram = unit(&mut tx, "Gram", None, "1").await;
    let kilogram = unit(&mut tx, "Kilogram", Some(gram.ids()[0]), "1000").await;
    let ton = unit(&mut tx, "Ton", Some(kilogram.ids()[0]), "1000").await;
    let result = erp_sdk::__private::call_method::<(Decimal, i64), ExactRatio>(
        &mut tx,
        &gram,
        "_compute_quantity",
        (dec("1020000"), ton.ids()[0]),
    )
    .await
    .unwrap();
    assert_eq!(result, ExactRatio::from_decimal(dec("1.02")).unwrap());
    let price = erp_sdk::__private::call_method::<(Decimal, i64), ExactRatio>(
        &mut tx,
        &gram,
        "_compute_price",
        (dec("2"), ton.ids()[0]),
    )
    .await
    .unwrap();
    assert_eq!(price, ExactRatio::from_decimal(dec("2000000")).unwrap());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_model_dispatch_rejects_cross_root_zero_and_nonzero() {
    use erp_sdk::money::ExactRatio;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let meter = unit(&mut tx, "Meter", None, "1").await;
    let gram = unit(&mut tx, "Gram", None, "1").await;
    for method in ["_compute_quantity", "_compute_price"] {
        for value in [Decimal::ZERO, dec("2")] {
            let error = erp_sdk::__private::call_method::<(Decimal, i64), ExactRatio>(
                &mut tx,
                &meter,
                method,
                (value, gram.ids()[0]),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                format!(
                    "Units of measure {} and {} do not share a reference root.",
                    meter.ids()[0],
                    gram.ids()[0]
                )
            );
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_precision_helpers_use_current_product_unit_digits() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = tx.env().records("uom.uom").unwrap();
    let precision = tx
        .env()
        .records("decimal.precision")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Product Unit"))
                    .set_cell("digits", Cell::Int(2)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        erp_sdk::__private::call_method::<(Decimal,), Decimal>(
            &mut tx,
            &units,
            "round",
            (dec("1.004"),)
        )
        .await
        .unwrap(),
        dec("1.00")
    );
    assert_eq!(
        erp_sdk::__private::call_method::<(Decimal, Decimal), i32>(
            &mut tx,
            &units,
            "compare",
            (dec("1.001"), dec("1.004"))
        )
        .await
        .unwrap(),
        0
    );
    assert!(
        erp_sdk::__private::call_method::<(Decimal,), bool>(
            &mut tx,
            &units,
            "is_zero",
            (dec("0.0004"),)
        )
        .await
        .unwrap()
    );
    precision
        .write(&mut tx, RawVals::new().set_cell("digits", Cell::Int(3)))
        .await
        .unwrap();
    assert_eq!(
        erp_sdk::__private::call_method::<(Decimal,), Decimal>(
            &mut tx,
            &units,
            "round",
            (dec("1.004"),)
        )
        .await
        .unwrap(),
        dec("1.004")
    );
    assert_eq!(
        erp_sdk::__private::call_method::<(Decimal, Decimal), i32>(
            &mut tx,
            &units,
            "compare",
            (dec("0.006"), dec("0.002"))
        )
        .await
        .unwrap(),
        1
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn sequence_underflow_is_rejected_without_clamping() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    for relative in [
        "-21474836.49",
        "-21474836.489",
        "21474836.48",
        "100000000000000000000",
    ] {
        let error = tx
            .savepoint(async |tx| {
                tx.env()
                    .records("uom.uom")?
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("name", text("Outside sequence range"))
                                .set_cell("relative_uom_id", Cell::Id(root.ids()[0]))
                                .set_cell("relative_factor", Cell::Decimal(dec(relative))),
                        ],
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Unit of measure sequence is outside the integer range."
        );
    }
    let minimum = unit(&mut tx, "Minimum", Some(root.ids()[0]), "-21474836.48").await;
    let maximum = unit(&mut tx, "Maximum", Some(root.ids()[0]), "21474836.47").await;
    assert_eq!(
        minimum.cell(&mut tx, "sequence").await.unwrap(),
        Cell::Int(i32::MIN)
    );
    assert_eq!(
        maximum.cell(&mut tx, "sequence").await.unwrap(),
        Cell::Int(1000)
    );
    assert_eq!(factor(&mut tx, &root).await, Decimal::ONE);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn conversion_rejects_malformed_and_over_depth_paths() {
    use erp_sdk::money::ExactRatio;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    for (path, expected) in [
        (
            "not/a/path/".to_owned(),
            format!(
                "Unit of measure {} has an invalid hierarchy path.",
                root.ids()[0]
            ),
        ),
        (
            format!("{}/", root.ids()[0]).repeat(257),
            "Unit of measure hierarchy exceeds 256 levels.".to_owned(),
        ),
    ] {
        // Deliberate owner-only corruption fixture: runtime writes now refuse forged paths.
        tx.batch("ALTER TABLE uom_uom DISABLE TRIGGER erp_uom_after_update")
            .await
            .unwrap();
        tx.execute(
            "UPDATE uom_uom SET parent_path = $1 WHERE id = $2",
            &[&path, &root.ids()[0]],
        )
        .await
        .unwrap();
        tx.batch("ALTER TABLE uom_uom ENABLE TRIGGER erp_uom_after_update")
            .await
            .unwrap();
        tx.invalidate_all().await.unwrap();
        let error = erp_sdk::__private::call_method::<(Decimal, i64), ExactRatio>(
            &mut tx,
            &root,
            "_compute_quantity",
            (Decimal::ZERO, root.ids()[0]),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), expected);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn conversion_rejects_a_missing_target_before_zero_shortcut() {
    use erp_sdk::money::ExactRatio;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Units", None, "1").await;
    let error = erp_sdk::__private::call_method::<(Decimal, i64), ExactRatio>(
        &mut tx,
        &root,
        "_compute_quantity",
        (Decimal::ZERO, 900000000),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Missing);
    assert_eq!(error.message(), "Unit of measure 900000000 does not exist.");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_round_with_honors_explicit_mode_and_rejects_unknown_mode() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = tx.env().records("uom.uom").unwrap();
    let down = erp_sdk::__private::call_method::<(Decimal, String), Decimal>(
        &mut tx,
        &units,
        "round_with",
        (dec("1.009"), "DOWN".to_owned()),
    )
    .await
    .unwrap();
    assert_eq!(down, dec("1.00"));
    let error = erp_sdk::__private::call_method::<(Decimal, String), Decimal>(
        &mut tx,
        &units,
        "round_with",
        (dec("1.009"), "INVALID".to_owned()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Unsupported unit of measure rounding method 'INVALID'."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn factor_assignment_write_is_rejected_before_touching_warm_closure() {
    use erp_addon_uom::Uom;
    use std::collections::BTreeMap;

    let h = harness().await;
    for dynamic in [false, true] {
        for mixed in [false, true] {
            let mut tx = h.begin().await.unwrap();
            let root = unit(&mut tx, "Units", None, "1").await;
            let parent = unit(&mut tx, "Pack", Some(root.ids()[0]), "6").await;
            let child = unit(&mut tx, "Carton", Some(parent.ids()[0]), "4").await;
            assert_eq!(factor(&mut tx, &root).await, dec("1"));
            assert_eq!(factor(&mut tx, &parent).await, dec("6"));
            assert_eq!(factor(&mut tx, &child).await, dec("24"));
            // There is deliberately no caller savepoint: rejection must precede all mutation.
            let result = if dynamic {
                let mut values =
                    BTreeMap::from([("factor".to_owned(), Value::Decimal(dec("999")))]);
                if mixed {
                    values.insert("relative_factor".to_owned(), Value::Decimal(dec("5")));
                }
                parent.write_values(&mut tx, &values).await
            } else {
                let parent = tx
                    .env()
                    .model::<Uom>()
                    .unwrap()
                    .browse(parent.ids())
                    .unwrap();
                let mut values = Vals::<Uom>::new().set(Uom::FACTOR, dec("999"));
                if mixed {
                    values = values.set(Uom::RELATIVE_FACTOR, dec("5"));
                }
                parent.write(&mut tx, values).await
            };
            let error = result.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Field uom.uom.factor is maintained by the unit hierarchy and cannot be assigned."
            );
            let ids = vec![root.ids()[0], parent.ids()[0], child.ids()[0]];
            let rows = tx.query("SELECT id, relative_factor::text, factor::text FROM uom_uom WHERE id = ANY($1) ORDER BY id", &[&ids]).await.unwrap();
            assert_eq!(rows.len(), 3);
            for (row, (id, relative, absolute)) in
                rows.iter()
                    .zip([(ids[0], "1", "1"), (ids[1], "6", "6"), (ids[2], "4", "24")])
            {
                assert_eq!(row.get::<_, i64>(0), id);
                assert_eq!(
                    row.get::<_, String>(1).parse::<Decimal>().unwrap(),
                    dec(relative)
                );
                assert_eq!(
                    row.get::<_, String>(2).parse::<Decimal>().unwrap(),
                    dec(absolute)
                );
            }
            assert_eq!(factor(&mut tx, &root).await, dec("1"));
            assert_eq!(factor(&mut tx, &parent).await, dec("6"));
            assert_eq!(factor(&mut tx, &child).await, dec("24"));
            assert_eq!(
                parent.cell(&mut tx, "relative_factor").await.unwrap(),
                Cell::Decimal(dec("6"))
            );
            tx.rollback().await.unwrap();
            drop(tx);
        }
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn factor_assignment_create_is_rejected_before_inserting_any_batch_row() {
    use erp_addon_uom::Uom;
    use std::collections::BTreeMap;

    let h = harness().await;
    for dynamic in [false, true] {
        let mut tx = h.begin().await.unwrap();
        let error = if dynamic {
            tx.env()
                .records("uom.uom")
                .unwrap()
                .create_values(
                    &mut tx,
                    &[
                        BTreeMap::from([("name".to_owned(), Value::str("Otherwise valid"))]),
                        BTreeMap::from([
                            ("name".to_owned(), Value::str("Forged")),
                            ("relative_factor".to_owned(), Value::Decimal(Decimal::ONE)),
                            ("factor".to_owned(), Value::Decimal(Decimal::ONE)),
                        ]),
                    ],
                )
                .await
                .unwrap_err()
        } else {
            tx.env()
                .model::<Uom>()
                .unwrap()
                .create(
                    &mut tx,
                    vec![
                        Vals::<Uom>::new().set(Uom::NAME, "Otherwise valid"),
                        Vals::<Uom>::new()
                            .set(Uom::NAME, "Forged")
                            .set(Uom::RELATIVE_FACTOR, Decimal::ONE)
                            .set(Uom::FACTOR, Decimal::ONE),
                    ],
                )
                .await
                .unwrap_err()
        };
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Field uom.uom.factor is maintained by the unit hierarchy and cannot be assigned."
        );
        let rows = tx
            .query("SELECT count(*)::bigint FROM uom_uom", &[])
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 0);
        tx.rollback().await.unwrap();
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn factor_context_default_is_rejected_before_inserting() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .with_context([("default_factor", Value::Decimal(dec("999")))])
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Defaulted root".into()))],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Field uom.uom.factor is maintained by the unit hierarchy and cannot be assigned."
    );
    let rows = tx
        .query("SELECT count(*)::bigint FROM uom_uom", &[])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 0);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn maintained_factor_guard_preserves_empty_write_noop() {
    use erp_addon_uom::Uom;
    use std::collections::BTreeMap;

    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let units = tx.env().model::<Uom>().unwrap();
    let before_create = tx.query_count();
    let created = units
        .create(&mut tx, Vec::<Vals<Uom>>::new())
        .await
        .unwrap();
    assert!(created.is_empty());
    assert_eq!(
        tx.query_count() - before_create,
        0,
        "empty typed create must not send SQL"
    );
    let before_typed = tx.query_count();
    units
        .write(&mut tx, Vals::<Uom>::new().set(Uom::FACTOR, dec("999")))
        .await
        .unwrap();
    assert_eq!(
        tx.query_count() - before_typed,
        0,
        "empty typed write must not send SQL"
    );
    let values = BTreeMap::from([("factor".to_owned(), Value::Decimal(dec("999")))]);
    let before_dynamic = tx.query_count();
    units
        .as_records()
        .write_values(&mut tx, &values)
        .await
        .unwrap();
    assert_eq!(
        tx.query_count() - before_dynamic,
        0,
        "empty dynamic write must not send SQL"
    );
    let before_unknown = tx.query_count();
    units
        .as_records()
        .write(&mut tx, RawVals::new().set_cell("nope", Cell::Int(1)))
        .await
        .unwrap();
    assert_eq!(
        tx.query_count() - before_unknown,
        0,
        "empty raw typed-cell write ignores unknown fields without SQL"
    );
    assert!(!tx.is_poisoned());
    let rows = tx
        .query("SELECT count(*)::bigint FROM uom_uom", &[])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 0);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
