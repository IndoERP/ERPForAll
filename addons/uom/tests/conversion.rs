//! Arithmetic-only coverage; model persistence and dispatch are covered separately.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_addon_uom::conversion::{
    Unit, exact_decimal, packaging_quantity, precision_compare, precision_is_zero, precision_round,
    price_exact, quantity_exact, quantity_rounded, rpc_exact,
};
use erp_addon_uom::error::UomError;
use erp_sdk::Decimal;
use erp_sdk::money::{ExactRatio, Increment, RoundingMode};
use std::error::Error;
use std::str::FromStr;

fn d(text: &str) -> Decimal {
    Decimal::from_str(text).unwrap()
}

fn unit(id: i64, root: i64, factor: &str) -> Unit {
    Unit::new(id, root, d(factor)).unwrap()
}

#[test]
fn exact_quantity_and_inverse_price_use_the_same_root() {
    let grams = unit(1, 1, "1");
    let tons = unit(3, 1, "1000000");
    assert_eq!(
        quantity_exact(&grams, &tons, d("1020000")).unwrap(),
        ExactRatio::from_decimal(d("1.02")).unwrap()
    );
    assert_eq!(
        price_exact(&grams, &tons, d("2")).unwrap(),
        ExactRatio::from_i64(2000000)
    );
    assert_eq!(
        quantity_exact(&tons, &grams, d("-1.02")).unwrap(),
        ExactRatio::from_i64(-1020000)
    );
    assert_eq!(
        quantity_exact(&grams, &grams, d("1.234")).unwrap(),
        ExactRatio::from_decimal(d("1.234")).unwrap()
    );
    assert_eq!(grams.id(), 1);
    assert_eq!(grams.root(), 1);
    assert_eq!(grams.factor(), d("1"));
}

#[test]
fn cross_root_zero_and_nonzero_conversions_fail_with_both_identities() {
    let meter = unit(11, 11, "1");
    let kg = unit(22, 21, "1000");
    for amount in [Decimal::ZERO, d("10")] {
        for result in [
            quantity_exact(&meter, &kg, amount),
            price_exact(&meter, &kg, amount),
        ] {
            let error = result.unwrap_err();
            assert!(matches!(
                error,
                UomError::IncompatibleRoots {
                    source_id: 11,
                    target_id: 22
                }
            ));
            assert_eq!(
                error.to_string(),
                "Units of measure 11 and 22 do not share a reference root."
            );
            assert!(error.source().is_none());
        }
    }
}

#[test]
fn unit_construction_rejects_invalid_identity_and_zero_factor() {
    for (id, root, factor, expected) in [
        (0, 1, "1", "Unit of measure id 0 must be positive."),
        (1, 0, "1", "Unit of measure root id 0 must be positive."),
        (
            1,
            1,
            "0",
            "The conversion factor for unit of measure 1 cannot be 0.",
        ),
    ] {
        let error = Unit::new(id, root, d(factor)).unwrap_err();
        match (id, root) {
            (0, _) => assert!(matches!(error, UomError::InvalidUnitId { id: 0 })),
            (_, 0) => assert!(matches!(error, UomError::InvalidRootId { id: 0 })),
            _ => assert!(matches!(error, UomError::ZeroFactor { id: 1 })),
        }
        assert_eq!(error.to_string(), expected);
    }
    assert_eq!(unit(1, 1, "-1").factor(), d("-1"));
}

#[test]
fn rounds_exact_ratios_using_each_supported_mode() {
    let one = unit(1, 1, "1");
    let two = unit(2, 1, "2");
    let increment = Increment::unit();
    for (mode, positive, negative) in [
        (RoundingMode::HalfAwayFromZero, "3", "-3"),
        (RoundingMode::HalfTowardZero, "2", "-2"),
        (RoundingMode::HalfEven, "2", "-2"),
        (RoundingMode::AwayFromZero, "3", "-3"),
        (RoundingMode::TowardZero, "2", "-2"),
        (RoundingMode::Ceiling, "3", "-2"),
        (RoundingMode::Floor, "2", "-3"),
    ] {
        assert_eq!(
            quantity_rounded(&one, &two, d("5"), &increment, mode).unwrap(),
            d(positive)
        );
        assert_eq!(
            quantity_rounded(&one, &two, d("-5"), &increment, mode).unwrap(),
            d(negative)
        );
    }
    assert_eq!(
        quantity_rounded(
            &one,
            &one,
            d("0.001"),
            &Increment::from_places(2).unwrap(),
            RoundingMode::AwayFromZero
        )
        .unwrap(),
        d("0.01")
    );
}

#[test]
fn exact_decimal_distinguishes_terminating_recurring_and_out_of_range_results() {
    for value in [
        Decimal::ZERO,
        d("-1.02"),
        Decimal::MAX,
        Decimal::MIN,
        d("0.0000000000000000000000000001"),
    ] {
        assert_eq!(
            exact_decimal(&ExactRatio::from_decimal(value).unwrap()).unwrap(),
            Some(value)
        );
    }
    let third = ExactRatio::quotient(Decimal::ONE, d("3")).unwrap();
    assert_eq!(third.numerator_string(), "1");
    assert_eq!(third.denominator_string(), "3");
    assert_eq!(exact_decimal(&third).unwrap(), None);
    let tiny = ExactRatio::from_decimal(d("0.0000000000000000000000000001"))
        .unwrap()
        .checked_div(&ExactRatio::from_i64(10))
        .unwrap();
    assert_eq!(exact_decimal(&tiny).unwrap(), None);
    let huge = ExactRatio::from_decimal(Decimal::MAX)
        .unwrap()
        .checked_mul(&ExactRatio::from_i64(10))
        .unwrap();
    let error = exact_decimal(&huge).unwrap_err();
    assert!(matches!(
        error,
        UomError::Numeric(erp_sdk::money::MoneyError::NumericLimit {
            operation: "uom exact decimal"
        })
    ));
    assert_eq!(
        error.to_string(),
        "The monetary value is out of the supported range (uom exact decimal)."
    );
    let large_fraction = ExactRatio::from_decimal(d("7922816251426433759354395033.5")).unwrap();
    assert_eq!(
        exact_decimal(&large_fraction).unwrap(),
        Some(d("7922816251426433759354395033.5"))
    );
}

#[test]
fn packaging_rounds_one_pack_then_integer_multiple_and_preserves_same_unit() {
    let pack = unit(6, 1, "6");
    let one = unit(1, 1, "1");
    let increment = Increment::from_places(2).unwrap();
    assert_eq!(
        packaging_quantity(
            &pack,
            &one,
            d("22.43"),
            &increment,
            RoundingMode::HalfAwayFromZero
        )
        .unwrap(),
        d("24")
    );
    assert_eq!(
        packaging_quantity(
            &pack,
            &one,
            d("22.43"),
            &increment,
            RoundingMode::TowardZero
        )
        .unwrap(),
        d("18")
    );
    assert_eq!(
        packaging_quantity(
            &pack,
            &one,
            d("-22.43"),
            &increment,
            RoundingMode::AwayFromZero
        )
        .unwrap(),
        d("-24")
    );
    assert_eq!(
        packaging_quantity(&one, &one, d("22.43"), &increment, RoundingMode::TowardZero).unwrap(),
        d("22.43")
    );
    let recurring = unit(3, 1, "3");
    // One source unit converts to 0.34 target units (default UP), then 1 / 0.34
    // rounds to three packs, hence 1.02 target units rather than 0.99.
    assert_eq!(
        packaging_quantity(
            &one,
            &recurring,
            d("1"),
            &increment,
            RoundingMode::HalfAwayFromZero
        )
        .unwrap(),
        d("1.02")
    );
}

#[test]
fn rounded_quantity_retains_numeric_error_source() {
    let huge = unit(2, 1, "79228162514264337593543950335");
    let one = unit(1, 1, "1");
    let error = quantity_rounded(
        &huge,
        &one,
        d("2"),
        &Increment::unit(),
        RoundingMode::AwayFromZero,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        UomError::Numeric(erp_sdk::money::MoneyError::NumericLimit {
            operation: "quantize"
        })
    ));
    assert_eq!(
        error.to_string(),
        "The monetary value is out of the supported range (quantize)."
    );
    assert_eq!(error.source().unwrap().to_string(), error.to_string());
}

#[test]
fn precision_helpers_compare_independent_operands() {
    use std::cmp::Ordering;
    let precision = Increment::from_places(2).unwrap();
    assert_eq!(
        precision_round(d("-2.675"), &precision, RoundingMode::HalfAwayFromZero).unwrap(),
        d("-2.68")
    );
    assert_eq!(
        precision_compare(d("0.006"), d("0.002"), &precision).unwrap(),
        Ordering::Greater
    );
    assert_eq!(
        precision_compare(d("1.432"), d("1.431"), &precision).unwrap(),
        Ordering::Equal
    );
    assert!(precision_is_zero(d("0.004"), &precision).unwrap());
    assert!(!precision_is_zero(d("0.005"), &precision).unwrap());
}

#[test]
fn exact_rpc_returns_decimal_or_canonical_string_pair() {
    use erp_sdk::Value;
    let decimal = ExactRatio::from_decimal(d("-1.25")).unwrap();
    assert_eq!(rpc_exact(&decimal).unwrap(), Value::Decimal(d("-1.25")));
    let third = ExactRatio::quotient(Decimal::ONE, d("3")).unwrap();
    assert_eq!(
        rpc_exact(&third).unwrap(),
        Value::Map(std::sync::Arc::new(
            [
                ("numerator".to_owned(), Value::str("1")),
                ("denominator".to_owned(), Value::str("3")),
            ]
            .into_iter()
            .collect()
        ))
    );
    let huge = ExactRatio::from_decimal(Decimal::MAX)
        .unwrap()
        .checked_mul(&ExactRatio::from_i64(10))
        .unwrap();
    assert_eq!(
        rpc_exact(&huge).unwrap(),
        Value::Map(std::sync::Arc::new(
            [
                (
                    "numerator".to_owned(),
                    Value::str("792281625142643375935439503350")
                ),
                ("denominator".to_owned(), Value::str("1")),
            ]
            .into_iter()
            .collect()
        ))
    );
}

#[test]
fn packaging_checks_roots_before_zero_and_identity_shortcuts() {
    // Cross-root validation must happen before the zero quantity shortcut.
    let a = unit(1, 1, "1");
    let b = unit(2, 2, "1");
    let error = packaging_quantity(
        &a,
        &b,
        Decimal::ZERO,
        &Increment::unit(),
        RoundingMode::HalfAwayFromZero,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        UomError::IncompatibleRoots {
            source_id: 1,
            target_id: 2
        }
    ));
    assert_eq!(
        error.to_string(),
        "Units of measure 1 and 2 do not share a reference root."
    );
}

#[test]
fn typed_errors_keep_validation_kind_at_model_boundary() {
    use erp_sdk::{Error as FrameworkError, ErrorKind};
    let error = Unit::new(-1, 1, Decimal::ONE).unwrap_err();
    let error: FrameworkError = error.into();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Unit of measure id -1 must be positive.");
    assert_eq!(error.source().unwrap().to_string(), error.message());
}

#[test]
fn negative_nonzero_factors_are_not_newly_prohibited() {
    let one = unit(1, 1, "1");
    let negative = unit(2, 1, "-6");
    assert_eq!(
        quantity_exact(&negative, &one, d("2")).unwrap(),
        ExactRatio::from_i64(-12)
    );
    assert_eq!(
        price_exact(&negative, &one, d("12")).unwrap(),
        ExactRatio::from_i64(-2)
    );
}

#[test]
fn common_root_is_checked_before_same_identity_shortcut() {
    let source = unit(2, 1, "6");
    let inconsistent_target = unit(2, 3, "6");
    let error = quantity_exact(&source, &inconsistent_target, Decimal::ZERO).unwrap_err();
    assert!(matches!(
        error,
        UomError::IncompatibleRoots {
            source_id: 2,
            target_id: 2
        }
    ));
    assert_eq!(
        error.to_string(),
        "Units of measure 2 and 2 do not share a reference root."
    );
}
