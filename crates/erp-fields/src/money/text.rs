//! The strict decimal text of the money boundary.

use rust_decimal::Decimal;

use super::error::MoneyError;

/// The longest accepted decimal text, in bytes.
pub const MAX_DECIMAL_BYTES: usize = 64;

const MAX_SCALE: usize = 28;
/// The coefficient of a boundary decimal is below `2^96`.
const COEFFICIENT_LIMIT: u128 = 1 << 96;

fn invalid(reason: &'static str) -> MoneyError {
    MoneyError::InvalidDecimal { reason }
}

/// Parses a strict decimal string: an optional `-`, digits, and optionally `.` with digits.
///
/// Refused: exponent notation, a leading `+`, whitespace, `NaN` and infinities, `.5` and `5.`,
/// more than 28 fractional digits, a coefficient of 2^96 or more, and text longer than
/// [`MAX_DECIMAL_BYTES`]. `-0` is zero (no negative zero). Trailing zeroes are kept as scale
/// (`"1.50"` has scale 2); [`canonical_string`] removes them.
///
/// # Errors
///
/// [`MoneyError::InvalidDecimal`] naming the first violated rule.
///
/// ```
/// use erp_fields::money::{canonical_string, parse_decimal};
/// let d = parse_decimal("-12.3400")?;
/// assert_eq!(canonical_string(d), "-12.34");
/// assert!(parse_decimal("1e3").is_err());
/// assert!(parse_decimal(" 1").is_err());
/// # Ok::<(), erp_fields::money::MoneyError>(())
/// ```
pub fn parse_decimal(text: &str) -> Result<Decimal, MoneyError> {
    if text.len() > MAX_DECIMAL_BYTES {
        return Err(invalid("longer than 64 bytes"));
    }
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int, frac) = match rest.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (rest, None),
    };
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("expected digits, optionally preceded by '-'"));
    }
    if let Some(frac) = frac
        && (frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid("expected digits after the decimal point"));
    }
    let frac_digits = frac.unwrap_or("");
    if frac_digits.len() > MAX_SCALE {
        return Err(invalid("more than 28 fractional digits"));
    }
    let mut coefficient: u128 = 0;
    for byte in int.bytes().chain(frac_digits.bytes()) {
        coefficient = coefficient
            .checked_mul(10)
            .and_then(|c| c.checked_add(u128::from(byte - b'0')))
            .filter(|c| *c < COEFFICIENT_LIMIT)
            .ok_or_else(|| invalid("more than 96 bits of significant digits"))?;
    }
    // coefficient < 2^96, so the i128 conversion cannot fail; the scale is at most 28
    let magnitude = i128::try_from(coefficient).map_err(|_| invalid("coefficient too large"))?;
    let signed = if negative { -magnitude } else { magnitude };
    Decimal::try_from_i128_with_scale(signed, frac_digits.len() as u32)
        .map_err(|_| invalid("outside the decimal range"))
}

/// The canonical text of a decimal: no insignificant trailing zeroes, no exponent, no negative
/// zero.
pub fn canonical_string(value: Decimal) -> String {
    if value.is_zero() {
        "0".to_owned()
    } else {
        value.normalize().to_string()
    }
}
