// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded grid and persistence prerequisites; no valuation engine is exercised here.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use erp_fields::money::{
    CanonicalFraction, ExactNumericError, ExactRatio, GridValue, Increment, parse_decimal,
};

fn grid(text: &str, scale: u32) -> GridValue {
    GridValue::parse(text, scale).unwrap()
}

#[test]
fn rejects_noncanonical_fractions_without_reducing_corruption() {
    for (n, d) in [
        ("1", "0"),
        ("1", "-3"),
        ("2", "6"),
        ("0", "2"),
        ("-0", "1"),
        ("1.5", "3"),
        ("01", "3"),
        ("+1", "3"),
    ] {
        let error = CanonicalFraction::parse(n, d).unwrap_err();
        assert_eq!(error, ExactNumericError::InvalidEncoding);
        assert_eq!(error.to_string(), "Invalid canonical exact cost encoding");
    }
    for (n, d) in [("0", "1"), ("-2", "3"), ("2", "3")] {
        let f = CanonicalFraction::parse(n, d).unwrap();
        assert_eq!(f.numerator().to_string(), n);
        assert_eq!(f.denominator().to_string(), d);
    }
}

#[test]
fn grid_preserves_wide_coefficients_and_refuses_rounding_to_fit() {
    // 2^256 - 1, independently specified as a decimal integer boundary.
    let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let too_big = "115792089237316195423570985008687907853269984665640564039457584007913129639936";
    assert_eq!(grid(max, 0).to_string(), max);
    let error = GridValue::parse(too_big, 0).unwrap_err();
    assert_eq!(error, ExactNumericError::NumericLimit);
    assert_eq!(error.to_string(), "Exact cost arithmetic limit exceeded");
    let wide = grid("79228162514264337593543950336.000000000000000001", 18);
    assert_eq!(
        wide.to_string(),
        "79228162514264337593543950336.000000000000000001"
    );
    for text in [
        "",
        "-0",
        " 1",
        "+1",
        "1e2",
        "NaN",
        "1.0000000000000000001",
        "١",
    ] {
        let error = GridValue::parse(text, 18).unwrap_err();
        assert_eq!(error, ExactNumericError::InvalidInput);
        assert_eq!(error.to_string(), "Invalid exact cost input");
    }
    assert_eq!(grid("1.000", 18), grid("1", 18));
    assert_eq!(grid("0.0", 18).to_string(), "0");
}

#[test]
fn quantizer_keeps_signed_ties_and_reconstructs_its_input() {
    for (n, d, expected) in [
        ("1", "3", "0.333333333333333333"),
        ("-1", "3", "-0.333333333333333333"),
        ("1", "2000000000000000000", "0.000000000000000001"),
        ("-1", "2000000000000000000", "-0.000000000000000001"),
    ] {
        let input = CanonicalFraction::parse(n, d).unwrap();
        let result = GridValue::quantize(input.as_ratio(), 18).unwrap();
        assert_eq!(result.value().to_string(), expected);
        assert_eq!(
            result
                .value()
                .to_ratio()
                .unwrap()
                .checked_add(result.remainder().as_ratio())
                .unwrap(),
            *input.as_ratio()
        );
    }
    // Independent small-integer half-away oracle over every signed numerator here.
    for n in -100_i64..=100 {
        for d in 1_i64..=17 {
            let ratio = ExactRatio::quotient(n.into(), d.into()).unwrap();
            let q = GridValue::quantize(&ratio, 2).unwrap();
            let scaled = n.unsigned_abs() * 100;
            let magnitude = scaled / d as u64 + u64::from((scaled % d as u64) * 2 >= d as u64);
            let signed = if n < 0 {
                -(magnitude as i64)
            } else {
                magnitude as i64
            };
            assert_eq!(q.value().coefficient().to_string(), signed.to_string());
        }
    }
}

#[test]
fn carry_uses_normalized_increment_and_includes_both_tie_endpoints() {
    let cent = Increment::new(parse_decimal("0.0100").unwrap()).unwrap();
    for value in ["-0.005", "0", "0.005"] {
        assert_eq!(grid(value, 18).validate_carry(&cent).unwrap().scale(), 18);
    }
    for value in ["-0.005000000000000001", "0.005000000000000001"] {
        let error = grid(value, 18).validate_carry(&cent).unwrap_err();
        assert_eq!(error, ExactNumericError::InvalidEncoding);
        assert_eq!(error.to_string(), "Invalid canonical exact cost encoding");
    }
    let tiny = Increment::new(parse_decimal("0.0000000000000000002").unwrap()).unwrap();
    assert_eq!(
        grid("0.0000000000000000001", 19)
            .validate_carry(&tiny)
            .unwrap()
            .scale(),
        19
    );
}

// Independent decimal-digit arithmetic builds bit-boundary fixtures without the library backend.
fn power_two_decimal(bits: usize) -> String {
    let mut digits = vec![1_u8];
    for _ in 0..bits {
        let mut carry = 0;
        for digit in &mut digits {
            let next = *digit * 2 + carry;
            *digit = next % 10;
            carry = next / 10;
        }
        if carry != 0 {
            digits.push(carry);
        }
    }
    digits.iter().rev().map(|d| char::from(b'0' + d)).collect()
}

#[test]
fn exact_bit_boundaries_are_admitted_and_first_overflow_is_refused() {
    use erp_fields::money::CanonicalInteger;
    let beyond = power_two_decimal(4096);
    let mut max = beyond.clone();
    assert_eq!(max.pop(), Some('6'));
    max.push('5');
    let value = CanonicalInteger::parse(&max).unwrap();
    assert_eq!(value.bits(), 4096);
    assert_eq!(value.to_string(), max);
    let pair = CanonicalFraction::parse("1", &max).unwrap();
    assert_eq!(pair.denominator().to_string(), max);
    for text in [&beyond, &"9".repeat(2000)] {
        let error = CanonicalInteger::parse(text).unwrap_err();
        assert_eq!(error, ExactNumericError::NumericLimit);
        assert_eq!(error.to_string(), "Exact cost arithmetic limit exceeded");
    }
    let input = CanonicalFraction::parse(
        "231584178474632390847141970017375815706539969331281128078915168015826259279871",
        "2",
    )
    .unwrap();
    let error = GridValue::quantize(input.as_ratio(), 0).unwrap_err();
    assert_eq!(error, ExactNumericError::NumericLimit);
    assert_eq!(error.to_string(), "Exact cost arithmetic limit exceeded");
    let max = grid(
        "115792089237316195423570985008687907853269984665640564039457584007913129639935",
        0,
    );
    let error = max.checked_add(&grid("1", 0)).unwrap_err();
    assert_eq!(error, ExactNumericError::NumericLimit);
    assert_eq!(error.to_string(), "Exact cost arithmetic limit exceeded");
    assert_eq!(
        grid("1.25", 2)
            .checked_sub(&grid("2.5", 1))
            .unwrap()
            .to_string(),
        "-1.25"
    );
    let error = grid("0.005", 3).rescale(2).unwrap_err();
    assert_eq!(error, ExactNumericError::InvalidInput);
    assert_eq!(error.to_string(), "Invalid exact cost input");
}
