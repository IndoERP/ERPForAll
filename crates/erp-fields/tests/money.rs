//! Exact money primitives (D3, hook H41). The currency-record half of each case is in
//! `crates/erp-test-models/tests/base_money.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::cmp::Ordering;

use erp_fields::Decimal;
use erp_fields::money::{
    Amount, CurrencyCode, ExactRatio, FxSnapshot, Increment, MAX_DECIMAL_BYTES, MoneyError,
    RateDateRule, RateOrigin, RateRef, RoundingMode, canonical_string, parse_decimal,
};
use proptest::prelude::*;

fn dec(text: &str) -> Decimal {
    parse_decimal(text).unwrap()
}

fn inc(text: &str) -> Increment {
    Increment::new(dec(text)).unwrap()
}

fn round(amount: &str, increment: &str, mode: RoundingMode) -> String {
    canonical_string(inc(increment).quantize(dec(amount), mode).unwrap())
}

const ALL_MODES: [RoundingMode; 7] = [
    RoundingMode::HalfAwayFromZero,
    RoundingMode::HalfTowardZero,
    RoundingMode::HalfEven,
    RoundingMode::AwayFromZero,
    RoundingMode::TowardZero,
    RoundingMode::Ceiling,
    RoundingMode::Floor,
];

#[test]
fn half_up_ties_cross_the_boundary_without_binary_artifacts() {
    // 2.675 is a tie at two decimals; as a binary float it is 2.67499999...
    assert_eq!(
        round("2.675", "0.01", RoundingMode::HalfAwayFromZero),
        "2.68"
    );
    assert_eq!(
        round("-2.675", "0.01", RoundingMode::HalfAwayFromZero),
        "-2.68"
    );
    assert_eq!(
        round("2.674", "0.01", RoundingMode::HalfAwayFromZero),
        "2.67"
    );
}

#[test]
fn half_even_rounds_ties_to_the_even_neighbour() {
    assert_eq!(round("2.5", "1", RoundingMode::HalfEven), "2");
    assert_eq!(round("3.5", "1", RoundingMode::HalfEven), "4");
    assert_eq!(round("-2.5", "1", RoundingMode::HalfEven), "-2");
}

#[test]
fn up_and_down_move_any_fraction() {
    assert_eq!(round("1.21", "0.1", RoundingMode::AwayFromZero), "1.3");
    assert_eq!(round("1.21", "0.1", RoundingMode::TowardZero), "1.2");
    assert_eq!(round("-1.21", "0.1", RoundingMode::AwayFromZero), "-1.3");
}

#[test]
fn every_mode_on_signed_ties_and_non_ties() {
    // (value, [half-away, half-toward, half-even, away, toward, ceiling, floor]) at increment 1
    let table: [(&str, [&str; 7]); 6] = [
        ("2.5", ["3", "2", "2", "3", "2", "3", "2"]),
        ("-2.5", ["-3", "-2", "-2", "-3", "-2", "-2", "-3"]),
        ("3.5", ["4", "3", "4", "4", "3", "4", "3"]),
        ("2.4", ["2", "2", "2", "3", "2", "3", "2"]),
        ("-2.6", ["-3", "-3", "-3", "-3", "-2", "-2", "-3"]),
        ("5", ["5", "5", "5", "5", "5", "5", "5"]),
    ];
    for (value, expected) in table {
        for (mode, want) in ALL_MODES.into_iter().zip(expected) {
            assert_eq!(round(value, "1", mode), want, "{value} with {mode:?}");
        }
    }
}

#[test]
fn is_zero_applies_the_rounding_to_the_amount() {
    let cent = inc("0.01");
    let mode = RoundingMode::HalfAwayFromZero;
    assert!(cent.is_zero(dec("0.004"), mode).unwrap());
    assert!(!cent.is_zero(dec("0.01"), mode).unwrap());
    assert!(
        !cent.is_zero(dec("0.005"), mode).unwrap(),
        "a tie rounds away from zero"
    );
    assert!(cent.is_zero(dec("-0.004"), mode).unwrap());
}

#[test]
fn compare_rounds_both_sides_first() {
    let cent = inc("0.01");
    let mode = RoundingMode::HalfAwayFromZero;
    assert_eq!(
        cent.compare(dec("1.432"), dec("1.431"), mode).unwrap(),
        Ordering::Equal
    );
    assert_eq!(
        cent.compare(dec("1.436"), dec("1.431"), mode).unwrap(),
        Ordering::Greater
    );
    assert_eq!(
        cent.compare(dec("-1.436"), dec("1.431"), mode).unwrap(),
        Ordering::Less
    );
}

#[test]
fn is_zero_of_difference_differs_from_compare_equal() {
    let cent = inc("0.01");
    let mode = RoundingMode::HalfAwayFromZero;
    let (a, b) = (dec("0.006"), dec("0.002"));
    assert!(cent.is_zero(a - b, mode).unwrap());
    assert_ne!(cent.compare(a, b, mode).unwrap(), Ordering::Equal);
}

#[test]
fn quotients_stay_exact_until_quantized() {
    let eleven_twelfths = ExactRatio::quotient(dec("11"), dec("12")).unwrap();
    let wages = ExactRatio::from_decimal(dec("24000000")).unwrap();
    let owed = wages.checked_mul(&eleven_twelfths).unwrap();
    assert_eq!(owed.to_string(), "22000000");
    assert_eq!(
        owed.quantize(&Increment::unit(), RoundingMode::HalfAwayFromZero)
            .unwrap(),
        dec("22000000")
    );

    let third = ExactRatio::quotient(dec("1"), dec("3")).unwrap();
    assert_eq!(
        (third.numerator_string(), third.denominator_string()),
        ("1".to_owned(), "3".to_owned())
    );
    let one = third
        .checked_add(&third)
        .unwrap()
        .checked_add(&third)
        .unwrap();
    assert_eq!(one, ExactRatio::from_i64(1));
    // quantizing 1/3 twice is not the same as one third of the quantized parts: no early rounding
    let two_thirds = third.checked_add(&third).unwrap();
    assert_eq!(
        two_thirds
            .quantize(&inc("0.01"), RoundingMode::HalfAwayFromZero)
            .unwrap(),
        dec("0.67")
    );
    assert_eq!(
        third
            .quantize(&inc("0.01"), RoundingMode::HalfAwayFromZero)
            .unwrap(),
        dec("0.33")
    );
}

#[test]
fn signed_arithmetic_and_ordering() {
    let a = ExactRatio::quotient(dec("1"), dec("3")).unwrap();
    let b = ExactRatio::quotient(dec("-1"), dec("2")).unwrap();
    assert_eq!(a.checked_add(&b).unwrap().to_string(), "-1/6");
    assert_eq!(a.checked_sub(&b).unwrap().to_string(), "5/6");
    assert_eq!(a.checked_mul(&b).unwrap().to_string(), "-1/6");
    assert_eq!(a.checked_div(&b).unwrap().to_string(), "-2/3");
    assert!(b < a);
    assert!(b.negated() > a);
    assert_eq!(a.checked_sub(&a).unwrap(), ExactRatio::zero());
    assert_eq!(ExactRatio::zero().negated().signum(), 0, "no negative zero");
}

#[test]
fn nondecimal_and_cash_increments() {
    assert_eq!(
        round("1.00", "0.07", RoundingMode::HalfAwayFromZero),
        "0.98"
    );
    assert_eq!(
        round("1.03", "0.05", RoundingMode::HalfAwayFromZero),
        "1.05"
    );
    assert_eq!(round("1.02", "0.05", RoundingMode::HalfAwayFromZero), "1");
    assert_eq!(
        round("0.13", "0.25", RoundingMode::HalfAwayFromZero),
        "0.25"
    );
    // the result keeps the increment's scale: presentation scale is metadata, not value
    let cent = inc("0.01")
        .quantize(dec("85"), RoundingMode::HalfAwayFromZero)
        .unwrap();
    assert_eq!(cent.scale(), 2);
    assert_eq!(cent, dec("85"));
    assert!(inc("0.05").contains(dec("1.35")).unwrap());
    assert!(!inc("0.05").contains(dec("1.36")).unwrap());
}

#[test]
fn invalid_increments_and_zero_denominators_are_errors() {
    for bad in ["0", "-0.01", "-1"] {
        assert_eq!(
            Increment::new(dec(bad)).unwrap_err(),
            MoneyError::InvalidIncrement
        );
    }
    assert_eq!(
        Increment::from_places(29).unwrap_err(),
        MoneyError::InvalidIncrement
    );
    assert_eq!(
        ExactRatio::quotient(dec("1"), Decimal::ZERO).unwrap_err(),
        MoneyError::DivisionByZero
    );
    let one = ExactRatio::from_i64(1);
    assert_eq!(
        one.checked_div(&ExactRatio::zero()).unwrap_err(),
        MoneyError::DivisionByZero
    );
}

#[test]
fn bounds_are_refused_loudly() {
    let big = ExactRatio::from_decimal(Decimal::MAX).unwrap();
    let mut acc = ExactRatio::from_i64(1);
    let mut refused = None;
    for step in 0..80 {
        match acc.checked_mul(&big) {
            Ok(next) => acc = next,
            Err(e) => {
                refused = Some((step, e));
                break;
            }
        }
    }
    let (step, error) = refused.expect("the product must eventually exceed 4096 bits");
    assert!(
        matches!(error, MoneyError::NumericLimit { .. }),
        "{error:?}"
    );
    // 4096 bits hold 42 factors of 96 bits, not 43
    assert_eq!(step, 42);

    // a quotient larger than the boundary decimal range cannot be quantized into one
    let huge = big.checked_mul(&ExactRatio::from_i64(10)).unwrap();
    assert!(matches!(
        huge.quantize(&inc("0.01"), RoundingMode::HalfAwayFromZero)
            .unwrap_err(),
        MoneyError::NumericLimit {
            operation: "quantize"
        }
    ));
    // and the same value is fine as an exact ratio
    assert_eq!(huge.signum(), 1);
}

#[test]
fn strict_decimal_text() {
    for good in [
        "0",
        "-0",
        "1",
        "-12.3400",
        "0.0000000000000000000000000001",
        "79228162514264337593543950335",
    ] {
        assert!(parse_decimal(good).is_ok(), "{good}");
    }
    let too_long = format!("1{}", "0".repeat(MAX_DECIMAL_BYTES));
    for bad in [
        "",
        "-",
        "+1",
        " 1",
        "1 ",
        "1e3",
        "1E3",
        ".5",
        "5.",
        "1.2.3",
        "NaN",
        "inf",
        "0x10",
        "1_0",
        "79228162514264337593543950336",
        "0.00000000000000000000000000001",
        &too_long,
    ] {
        assert!(
            matches!(parse_decimal(bad), Err(MoneyError::InvalidDecimal { .. })),
            "{bad:?} must be refused"
        );
    }
    assert_eq!(canonical_string(dec("-0")), "0");
    assert_eq!(canonical_string(dec("-12.3400")), "-12.34");
    assert_eq!(canonical_string(dec("100")), "100");
    assert_eq!(
        canonical_string(dec("0.0000000000000000000000000001")),
        "0.0000000000000000000000000001"
    );
}

#[test]
fn fx_snapshot_direction_is_explicit() {
    let day = erp_fields::Date::from_ymd(2026, 6, 1).unwrap();
    let usd = CurrencyCode::new("USD").unwrap();
    let eur = CurrencyCode::new("EUR").unwrap();
    let usd_rate = RateRef::new(Decimal::ONE, 1, day, RateOrigin::Preceding).unwrap();
    let eur_rate = RateRef::new(dec("0.85"), 2, day, RateOrigin::Preceding).unwrap();
    let rule = RateDateRule::StrictlyBefore;
    let forward = FxSnapshot::between(usd, eur, 1, day, rule, usd_rate, eur_rate).unwrap();
    let back = FxSnapshot::between(eur, usd, 1, day, rule, eur_rate, usd_rate).unwrap();
    let cent = inc("0.01");
    let to_eur = forward.convert_exact(dec("100")).unwrap();
    assert_eq!(
        to_eur
            .quantize(&cent, RoundingMode::HalfAwayFromZero)
            .unwrap(),
        dec("85")
    );
    let to_usd = back.convert_exact(dec("85")).unwrap();
    assert_eq!(
        to_usd
            .quantize(&cent, RoundingMode::HalfAwayFromZero)
            .unwrap(),
        dec("100")
    );
    // a rate of 3 makes 1/3 exact in the factor, and rounding only happens at the end
    let three = RateRef::new(dec("3"), 3, day, RateOrigin::LaterFallback).unwrap();
    let gbp = CurrencyCode::new("GBP").unwrap();
    let odd = FxSnapshot::between(gbp, usd, 1, day, rule, three, usd_rate).unwrap();
    assert_eq!(odd.factor().unwrap().to_string(), "1/3");
    assert_eq!(odd.rates().unwrap().0.origin(), RateOrigin::LaterFallback);
    let identity = FxSnapshot::identity(usd, 1, day, rule);
    assert!(identity.rates().is_none());
    assert_eq!(
        identity.convert_exact(dec("12.345")).unwrap().to_string(),
        "2469/200"
    );
}

#[test]
fn invalid_rates_and_mixed_currencies_are_errors() {
    let day = erp_fields::Date::from_ymd(2026, 6, 1).unwrap();
    for rate in ["0", "-1"] {
        assert_eq!(
            RateRef::new(dec(rate), 1, day, RateOrigin::Preceding).unwrap_err(),
            MoneyError::InvalidRate
        );
    }
    for code in ["US", "USDX", "us$", ""] {
        assert_eq!(
            CurrencyCode::new(code).unwrap_err(),
            MoneyError::InvalidCurrencyCode
        );
    }
    let usd = CurrencyCode::new("USD").unwrap();
    let eur = CurrencyCode::new("EUR").unwrap();
    let a = Amount::new(dec("1"), usd);
    let b = Amount::new(dec("2"), eur);
    assert_eq!(a.checked_add(&b).unwrap_err(), MoneyError::CurrencyMismatch);
    assert_eq!(
        a.checked_add(&Amount::new(dec("2"), usd)).unwrap().value(),
        dec("3")
    );
    let error: erp_fields::Error = MoneyError::CurrencyMismatch.into();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
}

/// The reference rounding: floor-based on i128 integers, written independently of the primitive.
fn reference(n: i128, d: i128, mode: RoundingMode) -> i128 {
    let floor = n.div_euclid(d);
    let rem = n.rem_euclid(d);
    if rem == 0 {
        return floor;
    }
    let ceil = floor + 1;
    let twice = 2 * rem;
    let positive = n >= 0;
    match mode {
        RoundingMode::Floor => floor,
        RoundingMode::Ceiling => ceil,
        RoundingMode::TowardZero => {
            if positive {
                floor
            } else {
                ceil
            }
        }
        RoundingMode::AwayFromZero => {
            if positive {
                ceil
            } else {
                floor
            }
        }
        RoundingMode::HalfAwayFromZero => match twice.cmp(&d) {
            Ordering::Greater => ceil,
            Ordering::Less => floor,
            Ordering::Equal => {
                if positive {
                    ceil
                } else {
                    floor
                }
            }
        },
        RoundingMode::HalfTowardZero => match twice.cmp(&d) {
            Ordering::Greater => ceil,
            Ordering::Less => floor,
            Ordering::Equal => {
                if positive {
                    floor
                } else {
                    ceil
                }
            }
        },
        RoundingMode::HalfEven => match twice.cmp(&d) {
            Ordering::Greater => ceil,
            Ordering::Less => floor,
            Ordering::Equal => {
                if floor % 2 == 0 {
                    floor
                } else {
                    ceil
                }
            }
        },
        _ => panic!("a mode was added without a reference"),
    }
}

proptest! {
    #[test]
    fn quantize_matches_integer_reference(
        n in -1_000_000_000_000i64..1_000_000_000_000,
        a in 0u32..8,
        m in 1u32..2000,
        b in 0u32..8,
    ) {
        let amount = Decimal::new(n, a);
        let increment = Increment::new(Decimal::new(i64::from(m), b)).unwrap();
        // x / q = (n / 10^a) / (m / 10^b) = n * 10^b / (10^a * m), on the increment's normalized form
        let normalized = increment.as_decimal();
        let (mm, bb) = (normalized.mantissa(), normalized.scale());
        let numerator = i128::from(n) * 10i128.pow(bb);
        let denominator = 10i128.pow(a) * mm;
        for mode in ALL_MODES {
            let units = reference(numerator, denominator, mode);
            let want = Decimal::from_i128_with_scale(units * mm, bb);
            let got = increment.quantize(amount, mode).unwrap();
            prop_assert_eq!(got, want, "{:?}", mode);
            prop_assert_eq!(got.scale(), bb);
        }
    }

    #[test]
    fn ratio_arithmetic_is_exact(
        a in -10_000i64..10_000, b in 1i64..10_000,
        c in -10_000i64..10_000, d in 1i64..10_000,
        e in -10_000i64..10_000, f in 1i64..10_000,
    ) {
        let x = ExactRatio::quotient(Decimal::from(a), Decimal::from(b)).unwrap();
        let y = ExactRatio::quotient(Decimal::from(c), Decimal::from(d)).unwrap();
        let z = ExactRatio::quotient(Decimal::from(e), Decimal::from(f)).unwrap();
        let left = x.checked_add(&y).unwrap().checked_add(&z).unwrap();
        let right = x.checked_add(&y.checked_add(&z).unwrap()).unwrap();
        prop_assert_eq!(&left, &right);
        let dist = x.checked_mul(&y.checked_add(&z).unwrap()).unwrap();
        let expanded = x.checked_mul(&y).unwrap().checked_add(&x.checked_mul(&z).unwrap()).unwrap();
        prop_assert_eq!(dist, expanded);
        prop_assert_eq!(left.checked_sub(&right).unwrap(), ExactRatio::zero());
    }
}
