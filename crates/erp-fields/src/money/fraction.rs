// SPDX-License-Identifier: MIT OR Apache-2.0
//! Validating persistence boundary for bounded exact fractions (3).
use std::fmt;

use super::nat::Nat;
use super::{ExactRatio, MAX_BITS};

/// Admission failure of the wide exact numeric boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExactNumericError {
    /// A persisted representation is malformed or noncanonical.
    InvalidEncoding,
    /// A caller supplied an invalid decimal or scale.
    InvalidInput,
    /// A coefficient, operand or arithmetic result exceeds its frozen policy bound.
    NumericLimit,
}
impl fmt::Display for ExactNumericError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidEncoding => "Invalid canonical exact cost encoding",
            Self::InvalidInput => "Invalid exact cost input",
            Self::NumericLimit => "Exact cost arithmetic limit exceeded",
        })
    }
}
impl std::error::Error for ExactNumericError {}
impl From<ExactNumericError> for crate::Error {
    fn from(value: ExactNumericError) -> Self {
        Self::validation(value.to_string()).with_source(value)
    }
}

/// Maximum decimal digits of an admitted integer, derived from the 4096-bit policy.
/// 30103/100000 is an upper bound for log10(2), not a measured value.
pub(super) const INTEGER_DIGITS: usize = (MAX_BITS as usize * 30103) / 100000 + 1;

/// A signed canonical integer below 2^4096 in magnitude. SQL binding is numeric, not int8.
/// It is only a fraction component: admit both components with [`CanonicalFraction::new`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CanonicalInteger {
    pub(super) negative: bool,
    pub(super) magnitude: Nat,
}
impl CanonicalInteger {
    /// Parse canonical ASCII integer text without normalizing a malformed encoding.
    /// # Errors
    /// InvalidEncoding for bad syntax/negative zero; NumericLimit beyond 4096 bits.
    pub fn parse(text: &str) -> Result<Self, ExactNumericError> {
        if text.len() > INTEGER_DIGITS + 1 {
            return Err(ExactNumericError::NumericLimit);
        }
        let (negative, digits) = match text.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, text),
        };
        if digits.is_empty()
            || !digits.bytes().all(|c| c.is_ascii_digit())
            || (digits.len() > 1 && digits.starts_with('0'))
            || (negative && digits == "0")
        {
            return Err(ExactNumericError::InvalidEncoding);
        }
        Ok(Self {
            negative,
            magnitude: parse_nat(digits, MAX_BITS)?,
        })
    }
    /// Number of significant magnitude bits; zero has none.
    pub fn bits(&self) -> u32 {
        self.magnitude.bits()
    }
}
impl fmt::Display for CanonicalInteger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            f.write_str("-")?;
        }
        f.write_str(&self.magnitude.to_decimal_string())
    }
}

/// Parse at most a bounded integer, rejecting growth before an oversized buffer is built.
pub(super) fn parse_nat(digits: &str, bits: u32) -> Result<Nat, ExactNumericError> {
    let mut value = Nat::zero();
    let max = max_for_bits(bits, Nat::from_u128(1))?;
    for digit in digits.bytes() {
        // Check the exact multiplication limit using division before allocating the product.
        let ten = Nat::from_u128(10);
        let remaining = max
            .checked_sub(&Nat::from_u128(u128::from(digit - b'0')))
            .ok_or(ExactNumericError::NumericLimit)?;
        let (threshold, _) = remaining
            .divrem(&ten)
            .ok_or(ExactNumericError::NumericLimit)?;
        if value > threshold {
            return Err(ExactNumericError::NumericLimit);
        }
        value = value
            .mul_small(10)
            .add(&Nat::from_u128(u128::from(digit - b'0')));
    }
    Ok(value)
}
fn max_for_bits(bits: u32, mut one: Nat) -> Result<Nat, ExactNumericError> {
    for _ in 1..bits {
        one = one.mul_small(2);
    }
    Ok(one
        .checked_sub(&Nat::from_u128(1))
        .ok_or(ExactNumericError::NumericLimit)?
        .mul_small(2)
        .add(&Nat::from_u128(1)))
}

/// A canonical, bounded fraction. Construction refuses corruption instead of reducing it.
///
/// ```
/// use erp_fields::money::{CanonicalFraction, ExactNumericError};
/// let value = CanonicalFraction::parse("-2", "3")?;
/// assert_eq!(value.as_ratio().to_string(), "-2/3");
/// assert_eq!(CanonicalFraction::parse("2", "6"), Err(ExactNumericError::InvalidEncoding));
/// # Ok::<(), ExactNumericError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CanonicalFraction {
    ratio: ExactRatio,
}
impl CanonicalFraction {
    /// Admit a complete pair, each component below 2^4096, denominator positive, gcd one.
    /// # Errors
    /// InvalidEncoding for noncanonical pairs; NumericLimit for oversized components.
    pub fn parse(numerator: &str, denominator: &str) -> Result<Self, ExactNumericError> {
        Self::new(
            CanonicalInteger::parse(numerator)?,
            CanonicalInteger::parse(denominator)?,
        )
    }
    /// Admit both decoded SQL integers together. A scalar decode alone is not pair admission.
    /// # Errors
    /// InvalidEncoding for nonpositive denominator, non-reduced pair or a zero other than 0/1.
    pub fn new(
        numerator: CanonicalInteger,
        denominator: CanonicalInteger,
    ) -> Result<Self, ExactNumericError> {
        if denominator.negative
            || denominator.magnitude.is_zero()
            || !numerator.magnitude.gcd(&denominator.magnitude).is_one()
            || (numerator.magnitude.is_zero() && !denominator.magnitude.is_one())
        {
            return Err(ExactNumericError::InvalidEncoding);
        }
        let ratio = ExactRatio::normalized(
            numerator.negative,
            numerator.magnitude,
            denominator.magnitude,
        )
        .map_err(|_| ExactNumericError::NumericLimit)?;
        Ok(Self { ratio })
    }
    /// Retain an already validated arithmetic ratio without reparsing its text.
    pub fn from_ratio(ratio: ExactRatio) -> Self {
        Self { ratio }
    }
    /// Exact arithmetic view; no decimal conversion.
    pub fn as_ratio(&self) -> &ExactRatio {
        &self.ratio
    }
    /// Canonical signed numeric binding of the numerator.
    pub fn numerator(&self) -> CanonicalInteger {
        let (negative, num, _) = self.ratio.parts();
        CanonicalInteger {
            negative,
            magnitude: num.clone(),
        }
    }
    /// Canonical positive numeric binding of the denominator.
    pub fn denominator(&self) -> CanonicalInteger {
        let (_, _, den) = self.ratio.parts();
        CanonicalInteger {
            negative: false,
            magnitude: den.clone(),
        }
    }
}
