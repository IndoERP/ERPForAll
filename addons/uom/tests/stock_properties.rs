// SPDX-License-Identifier: MIT OR Apache-2.0
//! Property tests of the stock-facing conversion (pure, no database).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use erp_addon_uom::conversion::{Unit, exact_decimal};
use erp_addon_uom::error::UomError;
use erp_addon_uom::stock::{ProductUnitPreview, StockRounding, product_quantity_sql};
use erp_sdk::Decimal;
use erp_sdk::money::{ExactRatio, Increment, MoneyError, RoundingMode};
use proptest::prelude::*;

fn config() -> ProptestConfig {
    ProptestConfig::with_cases(10_000)
}

fn quantity() -> impl Strategy<Value = Decimal> {
    (-1_000_000_000_000i64..=1_000_000_000_000, 0u32..=6).prop_map(|(m, s)| Decimal::new(m, s))
}

/// Nonzero factors of either sign (units keep negative nonzero factors).
fn factor() -> impl Strategy<Value = Decimal> {
    (prop_oneof![-1_000_000i64..=-1, 1i64..=1_000_000], 0u32..=4)
        .prop_map(|(m, s)| Decimal::new(m, s))
}

fn preview(source: Decimal, target: Decimal, digits: u32) -> ProductUnitPreview {
    ProductUnitPreview::new(
        &Unit::new(1, 9, source).unwrap(),
        &Unit::new(2, 9, target).unwrap(),
        digits,
    )
    .unwrap()
}

fn abs(value: &ExactRatio) -> ExactRatio {
    if value.signum() < 0 {
        value.negated()
    } else {
        value.clone()
    }
}

fn exact(value: Decimal) -> ExactRatio {
    ExactRatio::from_decimal(value).unwrap()
}

fn step(digits: u32) -> ExactRatio {
    exact(Decimal::new(1, digits))
}

/// The rounded value is on the grid, on the exact value's side, never nearer zero and under a
/// step away from it.
fn assert_up(e: &ExactRatio, r: Decimal, d: u32) -> Result<(), TestCaseError> {
    prop_assert_eq!(r.scale(), d);
    prop_assert_eq!(exact(r).signum(), e.signum());
    let (ae, ar) = (abs(e), abs(&exact(r)));
    prop_assert!(ar >= ae);
    prop_assert!(ar.checked_sub(&ae).unwrap() < step(d));
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn exact_conversions_match_independent_ratios_and_invert(q in quantity(), s in factor(), t in factor(), d in 0u32..=6) {
        let p = preview(s, t, d);
        let forward = p.line_to_product_exact(q).unwrap();
        // Formed another way: the product first, then one division.
        prop_assert_eq!(&forward, &ExactRatio::quotient(q * s, t).unwrap());
        prop_assert_eq!(p.product_to_line_exact(q).unwrap(), ExactRatio::quotient(q * t, s).unwrap());
        // When the forward value is a plain decimal, the reverse function returns the input.
        if let Some(u) = exact_decimal(&forward).unwrap() {
            prop_assert_eq!(p.product_to_line_exact(u).unwrap(), exact(q));
        }
    }

    #[test]
    fn half_up_is_within_half_step_and_ties_go_away(q in quantity(), s in factor(), t in factor(), d in 0u32..=6) {
        let p = preview(s, t, d);
        let e = p.line_to_product_exact(q).unwrap();
        let r = p.product_quantity(q).unwrap();
        prop_assert_eq!(r, p.line_to_product(q, StockRounding::HalfUp).unwrap());
        prop_assert_eq!(r.scale(), d);
        let diff = abs(&e.checked_sub(&exact(r)).unwrap());
        let half = step(d).checked_div(&ExactRatio::from_i64(2)).unwrap();
        prop_assert!(diff <= half);
        if diff == half {
            prop_assert!(abs(&exact(r)) > abs(&e));
        }
    }

    #[test]
    fn generated_ties_round_away_from_zero(m in 0i64..1_000_000_000, k in 1i64..1000, d in 0u32..=6, negative in any::<bool>()) {
        // q = (2m+1) * 10^-d, source k, target 2k: the exact value is (m + 1/2) steps.
        let sign = if negative { -1 } else { 1 };
        let q = Decimal::new(sign * (2 * m + 1), d);
        let p = preview(Decimal::from(k), Decimal::from(2 * k), d);
        prop_assert_eq!(p.product_quantity(q).unwrap(), Decimal::new(sign * (m + 1), d));
        // The same tie under UP is also away from zero.
        prop_assert_eq!(
            p.line_to_product(q, StockRounding::Up).unwrap(),
            Decimal::new(sign * (m + 1), d)
        );
    }

    #[test]
    fn up_is_away_from_zero_within_one_step(q in quantity(), s in factor(), t in factor(), d in 0u32..=6) {
        let p = preview(s, t, d);
        let e = p.line_to_product_exact(q).unwrap();
        assert_up(&e, p.line_to_product(q, StockRounding::Up).unwrap(), d)?;
    }

    #[test]
    fn product_to_line_up_is_away_from_zero_within_one_step(q in quantity(), s in factor(), t in factor(), d in 0u32..=6) {
        let p = preview(s, t, d);
        let e = p.product_to_line_exact(q).unwrap();
        assert_up(&e, p.product_to_line(q, StockRounding::Up).unwrap(), d)?;
    }

    #[test]
    fn cross_root_is_always_refused(s in factor(), t in factor(), d in 0u32..=6) {
        let error = ProductUnitPreview::new(
            &Unit::new(1, 8, s).unwrap(),
            &Unit::new(2, 9, t).unwrap(),
            d,
        ).unwrap_err();
        prop_assert_eq!(
            error.to_string(),
            "Units of measure 1 and 2 do not share a reference root."
        );
        prop_assert_eq!(error, UomError::IncompatibleRoots { source_id: 1, target_id: 2 });
    }

    #[test]
    fn same_unit_only_rounds(q in quantity(), s in factor(), d in 0u32..=6) {
        let unit = Unit::new(1, 9, s).unwrap();
        let p = ProductUnitPreview::new(&unit, &unit, d).unwrap();
        let rounded = exact(q)
            .quantize(&Increment::from_places(d).unwrap(), RoundingMode::HalfAwayFromZero)
            .unwrap();
        prop_assert_eq!(p.product_quantity(q).unwrap(), rounded);
    }
}

#[test]
fn half_cases_round_away_from_zero() {
    let p = preview(Decimal::ONE, Decimal::from(2), 0);
    assert_eq!(
        p.product_quantity(Decimal::from(5)).unwrap(),
        Decimal::from(3)
    );
    assert_eq!(
        p.product_quantity(Decimal::from(-5)).unwrap(),
        Decimal::from(-3)
    );
}

#[test]
fn digits_above_28_are_refused() {
    let unit = Unit::new(1, 9, Decimal::ONE).unwrap();
    let error = ProductUnitPreview::new(&unit, &unit, 29).unwrap_err();
    assert_eq!(error, UomError::InvalidDigits { digits: 29 });
    assert_eq!(
        error.to_string(),
        "Product Unit digits 29 are outside the supported range 0 to 28."
    );
    let converted = erp_sdk::Error::from(error);
    assert_eq!(converted.kind(), erp_sdk::ErrorKind::Programming);
}

#[test]
fn result_beyond_the_decimal_range_is_a_numeric_error() {
    let p = preview(Decimal::MAX, Decimal::ONE, 0);
    let error = p.product_quantity(Decimal::MAX).unwrap_err();
    assert_eq!(
        error,
        UomError::Numeric(MoneyError::NumericLimit {
            operation: "quantize"
        })
    );
    assert_eq!(
        error.to_string(),
        "The monetary value is out of the supported range (quantize)."
    );
    assert_eq!(
        erp_sdk::Error::from(error).kind(),
        erp_sdk::ErrorKind::Validation
    );
}

#[test]
fn sql_builder_refuses_unsafe_identifiers_and_quotes_the_rest() {
    for bad in [
        "",
        "Qty",
        "1qty",
        "qty; drop table x",
        "a\"b",
        &"a".repeat(64),
    ] {
        let error = product_quantity_sql(bad, "s", "t", "d").unwrap_err();
        assert_eq!(
            error,
            UomError::InvalidSqlIdentifier {
                identifier: bad.to_owned()
            }
        );
        assert_eq!(
            error.to_string(),
            format!(
                "'{bad}' is not a lowercase SQL identifier (letters, digits and underscores, at most 63 bytes, not starting with a digit)."
            )
        );
        let converted = erp_sdk::Error::from(error);
        assert_eq!(converted.kind(), erp_sdk::ErrorKind::Programming);
    }
    let sql = product_quantity_sql("user", "current_date", "t_2", &"d".repeat(63)).unwrap();
    assert!(sql.contains("\"user\"") && sql.contains("\"current_date\""));
    assert!(!sql.contains(" user ") && !sql.contains("(user"));
}
