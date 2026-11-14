// SPDX-License-Identifier: MIT OR Apache-2.0
//! Wide finite grids and their explicit quantization boundary.
use super::fraction::{CanonicalFraction, CanonicalInteger, ExactNumericError, parse_nat};
use super::nat::Nat;
use super::{ExactRatio, Increment, MAX_BITS};
use std::{cmp::Ordering, fmt};

/// Frozen coefficient bound of cost-scale-v1; changing policy requires a new policy identity.
pub const GRID_COEFFICIENT_BITS: u32 = 256;
/// Largest finite decimal scale admitted by the primitive (also the currency increment bound).
pub const MAX_GRID_SCALE: u32 = 28;
const GRID_DIGITS: usize = (GRID_COEFFICIENT_BITS as usize * 30103) / 100000 + 1;
/// Strict input bound, derived from coefficient digits, scale and sign/separator.
const GRID_TEXT_BYTES: usize = GRID_DIGITS + MAX_GRID_SCALE as usize + 3;

/// A signed integer coefficient below 2^256 times 10^-scale, with scale 0..28.
/// No float or Decimal narrowing is performed. The owning policy chooses the scale explicitly.
/// SQL decoding returns the minimal scale; call [`Self::rescale`] with the frozen row policy.
///
/// ```
/// use erp_fields::money::{CanonicalFraction, GridValue};
/// let third = CanonicalFraction::parse("1", "3")?;
/// let result = GridValue::quantize(third.as_ratio(), 18)?;
/// assert_eq!(result.value().to_string(), "0.333333333333333333");
/// assert_eq!(result.remainder().numerator().to_string(), "1");
/// assert_eq!(result.remainder().denominator().to_string(), "3000000000000000000");
/// # Ok::<(), erp_fields::money::ExactNumericError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct GridValue {
    coefficient: CanonicalInteger,
    scale: u32,
}
impl GridValue {
    /// Parse a strict finite decimal on an explicitly chosen grid; trailing zeroes are exact.
    /// # Errors
    /// InvalidInput for malformed text, negative zero, unsupported scale or nonzero finer digits;
    /// NumericLimit when the coefficient at the requested scale would exceed 256 bits.
    pub fn parse(text: &str, scale: u32) -> Result<Self, ExactNumericError> {
        if scale > MAX_GRID_SCALE || text.len() > GRID_TEXT_BYTES {
            return Err(ExactNumericError::InvalidInput);
        }
        let (negative, body) = match text.strip_prefix('-') {
            Some(body) => (true, body),
            None => (false, text),
        };
        let (whole, fraction) = match body.split_once('.') {
            Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
            Some(_) => return Err(ExactNumericError::InvalidInput),
            None => (body, ""),
        };
        if whole.is_empty()
            || !whole
                .bytes()
                .chain(fraction.bytes())
                .all(|c| c.is_ascii_digit())
            || (whole.len() > 1 && whole.starts_with('0'))
        {
            return Err(ExactNumericError::InvalidInput);
        }
        let fraction = fraction.trim_end_matches('0');
        if fraction.len() > scale as usize {
            return Err(ExactNumericError::InvalidInput);
        }
        let mut digits = format!("{whole}{fraction}");
        digits.extend(std::iter::repeat_n('0', scale as usize - fraction.len()));
        let magnitude = parse_nat(&digits, GRID_COEFFICIENT_BITS)?;
        if negative && magnitude.is_zero() {
            return Err(ExactNumericError::InvalidInput);
        }
        Ok(Self {
            coefficient: CanonicalInteger {
                negative,
                magnitude,
            },
            scale,
        })
    }
    /// Requested policy grid scale; this is separate from display formatting.
    pub fn scale(&self) -> u32 {
        self.scale
    }
    /// Exact signed coefficient, suitable for diagnostics and independent arithmetic checks.
    pub fn coefficient(&self) -> &CanonicalInteger {
        &self.coefficient
    }
    /// Convert to the bounded arithmetic ratio without a decimal boundary.
    /// # Errors
    /// NumericLimit if the arithmetic representation cannot be admitted.
    pub fn to_ratio(&self) -> Result<ExactRatio, ExactNumericError> {
        ExactRatio::normalized(
            self.coefficient.negative,
            self.coefficient.magnitude.clone(),
            Nat::pow10(self.scale),
        )
        .map_err(|_| ExactNumericError::NumericLimit)
    }
    /// Change grids exactly; never round to fit a coarser scale.
    /// # Errors
    /// As [`Self::parse`].
    pub fn rescale(&self, scale: u32) -> Result<Self, ExactNumericError> {
        Self::parse(&self.to_string(), scale)
    }
    /// Checked addition, retaining this grid. The other operand must rescale exactly to it.
    /// # Errors
    /// InvalidInput for an inexact scale change; NumericLimit for overflow.
    pub fn checked_add(&self, other: &Self) -> Result<Self, ExactNumericError> {
        let other = other.rescale(self.scale)?;
        let sum = self
            .to_ratio()?
            .checked_add(&other.to_ratio()?)
            .map_err(|_| ExactNumericError::NumericLimit)?;
        let result = Self::quantize(&sum, self.scale)?;
        Ok(result.value)
    }
    /// Checked subtraction, retaining this grid.
    /// # Errors
    /// As [`Self::checked_add`].
    pub fn checked_sub(&self, other: &Self) -> Result<Self, ExactNumericError> {
        let mut other = other.clone();
        if !other.coefficient.magnitude.is_zero() {
            other.coefficient.negative = !other.coefficient.negative;
        }
        self.checked_add(&other)
    }
    /// Quantize exactly once using HalfAwayFromZero, retaining the signed precision remainder.
    /// # Errors
    /// InvalidInput for scale outside 0..28; NumericLimit for operand/work/result overflow,
    /// including an unrepresentable remainder. No partial result is returned.
    pub fn quantize(input: &ExactRatio, scale: u32) -> Result<QuantizedGrid, ExactNumericError> {
        if scale > MAX_GRID_SCALE {
            return Err(ExactNumericError::InvalidInput);
        }
        let (negative, num, den) = input.parts();
        let factor = Nat::pow10(scale);
        let g = factor.gcd(den);
        let (factor, _) = factor.divrem(&g).ok_or(ExactNumericError::NumericLimit)?;
        let (den, _) = den.divrem(&g).ok_or(ExactNumericError::NumericLimit)?;
        // A product can have bits(a)+bits(b)-1 or bits(a)+bits(b) bits. In the ambiguous
        // case its allocation has at most one carry limb; the admitted result is checked again.
        if !num.is_zero() && num.bits() + factor.bits() - 1 > MAX_BITS {
            return Err(ExactNumericError::NumericLimit);
        }
        let scaled = num.mul(&factor);
        if scaled.bits() > MAX_BITS {
            return Err(ExactNumericError::NumericLimit);
        }
        let (mut units, remainder) = scaled.divrem(&den).ok_or(ExactNumericError::NumericLimit)?;
        let complement = den
            .checked_sub(&remainder)
            .ok_or(ExactNumericError::NumericLimit)?;
        if !remainder.is_zero() && remainder.cmp(&complement) != Ordering::Less {
            if units.bits() > GRID_COEFFICIENT_BITS {
                return Err(ExactNumericError::NumericLimit);
            }
            units = units.add(&Nat::from_u128(1));
        }
        if units.bits() > GRID_COEFFICIENT_BITS {
            return Err(ExactNumericError::NumericLimit);
        }
        let coefficient = CanonicalInteger {
            negative: negative && !units.is_zero(),
            magnitude: units,
        };
        let value = Self { coefficient, scale };
        let remainder = input
            .checked_sub(&value.to_ratio()?)
            .map_err(|_| ExactNumericError::NumericLimit)?;
        Ok(QuantizedGrid {
            value,
            remainder: CanonicalFraction::from_ratio(remainder),
        })
    }
    /// Admit a stored signed carry against its frozen normalized currency increment.
    /// # Errors
    /// InvalidEncoding for finer scale or a carry beyond half an increment; NumericLimit
    /// when the common-grid coefficient exceeds 256 bits. Both tie endpoints are admitted.
    pub fn validate_carry(&self, increment: &Increment) -> Result<Self, ExactNumericError> {
        let carry = self
            .rescale(18.max(increment.scale()))
            .map_err(|e| match e {
                ExactNumericError::InvalidInput => ExactNumericError::InvalidEncoding,
                other => other,
            })?;
        let two = ExactRatio::from_i64(2);
        let doubled = carry
            .to_ratio()?
            .checked_mul(&two)
            .map_err(|_| ExactNumericError::NumericLimit)?;
        let magnitude = if doubled.signum() < 0 {
            doubled.negated()
        } else {
            doubled
        };
        let bound = ExactRatio::from_decimal(increment.as_decimal())
            .map_err(|_| ExactNumericError::NumericLimit)?;
        if magnitude > bound {
            return Err(ExactNumericError::InvalidEncoding);
        }
        Ok(carry)
    }
}
impl fmt::Display for GridValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut digits = self.coefficient.magnitude.to_decimal_string();
        if self.coefficient.magnitude.is_zero() {
            return f.write_str("0");
        }
        if self.scale > 0 {
            if digits.len() <= self.scale as usize {
                digits = format!(
                    "{}{}",
                    "0".repeat(self.scale as usize + 1 - digits.len()),
                    digits
                );
            }
            digits.insert(digits.len() - self.scale as usize, '.');
            let trimmed = digits.trim_end_matches('0').trim_end_matches('.').len();
            digits.truncate(trimmed);
        }
        if self.coefficient.negative {
            f.write_str("-")?;
        }
        f.write_str(&digits)
    }
}

/// A grid result and the exact independent per-source precision difference.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuantizedGrid {
    value: GridValue,
    remainder: CanonicalFraction,
}
impl QuantizedGrid {
    /// The finite grid value.
    pub fn value(&self) -> &GridValue {
        &self.value
    }
    /// `input - value`; never silently discarded or recursively accumulated into state.
    pub fn remainder(&self) -> &CanonicalFraction {
        &self.remainder
    }
}
