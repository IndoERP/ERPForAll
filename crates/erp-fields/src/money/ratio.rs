//! `ExactRatio`: a signed quotient of two bounded integers.
//!
//! `1/3` and `11/12` stay exact until a named quantization boundary ([`ExactRatio::quantize`]).
//! Numerator and denominator are at most [`MAX_BITS`] bits each; every operation that would exceed
//! the bound is refused with [`MoneyError::NumericLimit`] before it allocates the oversized
//! product, never rounded to fit.

use std::cmp::Ordering;
use std::fmt;

use rust_decimal::Decimal;

use super::error::MoneyError;
use super::nat::Nat;
use super::rounding::{Increment, RoundingMode};

/// The bound on the numerator and on the denominator of an [`ExactRatio`], in bits.
pub const MAX_BITS: u32 = 4096;

/// The largest coefficient of a boundary decimal: `2^96 - 1`.
const MAX_COEFFICIENT_BITS: u32 = 96;

/// A signed exact quotient, normalized (lowest terms, positive denominator, no negative zero).
///
/// Built from decimals ([`ExactRatio::from_decimal`], [`ExactRatio::quotient`]) and combined with
/// the checked operations. There is no float conversion and no public constructor from raw
/// parts.
///
/// ```
/// use erp_fields::money::{ExactRatio, Increment, RoundingMode};
/// use erp_fields::Decimal;
///
/// let eleven_twelfths = ExactRatio::quotient(Decimal::from(11), Decimal::from(12))?;
/// let wages = ExactRatio::from_decimal(Decimal::from(24_000_000))?;
/// let owed = wages.checked_mul(&eleven_twelfths)?;
/// // 24,000,000 x 11/12 = 22,000,000 exactly, before any rounding.
/// assert_eq!(owed.quantize(&Increment::unit(), RoundingMode::HalfAwayFromZero)?, Decimal::from(22_000_000));
/// // 1/3 never becomes a finite decimal until it is quantized.
/// let third = ExactRatio::quotient(Decimal::ONE, Decimal::from(3))?;
/// assert_eq!(third.to_string(), "1/3");
/// # Ok::<(), erp_fields::money::MoneyError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExactRatio {
    negative: bool,
    num: Nat,
    den: Nat,
}

impl ExactRatio {
    /// Zero.
    pub fn zero() -> Self {
        Self {
            negative: false,
            num: Nat::zero(),
            den: Nat::from_u128(1),
        }
    }

    /// An integer, exactly.
    pub fn from_i64(value: i64) -> Self {
        Self {
            negative: value < 0,
            num: Nat::from_u128(u128::from(value.unsigned_abs())),
            den: Nat::from_u128(1),
        }
    }

    /// A decimal, losslessly (`mantissa / 10^scale`, reduced).
    ///
    /// # Errors
    ///
    /// None for a valid [`Decimal`] (at most 96 bits over `10^28`, far inside [`MAX_BITS`]); the
    /// error type exists so no failure path is swallowed.
    pub fn from_decimal(value: Decimal) -> Result<Self, MoneyError> {
        let num = Nat::from_u128(value.mantissa().unsigned_abs());
        let den = Nat::pow10(value.scale());
        Self::normalized(value.is_sign_negative(), num, den)
    }

    /// `numerator / denominator`, exactly.
    ///
    /// # Errors
    ///
    /// [`MoneyError::DivisionByZero`] when `denominator` is zero.
    pub fn quotient(numerator: Decimal, denominator: Decimal) -> Result<Self, MoneyError> {
        Self::from_decimal(numerator)?.checked_div(&Self::from_decimal(denominator)?)
    }

    fn normalized(negative: bool, num: Nat, den: Nat) -> Result<Self, MoneyError> {
        if den.is_zero() {
            return Err(MoneyError::DivisionByZero);
        }
        if num.bits() > MAX_BITS || den.bits() > MAX_BITS {
            return Err(MoneyError::NumericLimit {
                operation: "normalize",
            });
        }
        if num.is_zero() {
            return Ok(Self::zero());
        }
        let g = num.gcd(&den);
        let (num, den) = if g.is_one() {
            (num, den)
        } else {
            match (num.divrem(&g), den.divrem(&g)) {
                (Some((n, _)), Some((d, _))) => (n, d),
                _ => return Err(MoneyError::DivisionByZero),
            }
        };
        Ok(Self { negative, num, den })
    }

    /// Whether the value is exactly zero.
    pub fn is_zero(&self) -> bool {
        self.num.is_zero()
    }

    /// `-1`, `0` or `1`.
    pub fn signum(&self) -> i8 {
        if self.num.is_zero() {
            0
        } else if self.negative {
            -1
        } else {
            1
        }
    }

    /// The additive inverse.
    #[must_use]
    pub fn negated(&self) -> Self {
        Self {
            negative: !self.negative && !self.num.is_zero(),
            num: self.num.clone(),
            den: self.den.clone(),
        }
    }

    /// The canonical numerator, with a leading `-` when negative.
    pub fn numerator_string(&self) -> String {
        let digits = self.num.to_decimal_string();
        if self.negative {
            format!("-{digits}")
        } else {
            digits
        }
    }

    /// The canonical (positive) denominator.
    pub fn denominator_string(&self) -> String {
        self.den.to_decimal_string()
    }

    /// `self + other`.
    ///
    /// # Errors
    ///
    /// [`MoneyError::NumericLimit`] when an intermediate or the result exceeds [`MAX_BITS`].
    pub fn checked_add(&self, other: &Self) -> Result<Self, MoneyError> {
        if other.is_zero() {
            return Ok(self.clone());
        }
        if self.is_zero() {
            return Ok(other.clone());
        }
        // a/b + c/d over lcm(b, d) = b * (d / g)
        let g = self.den.gcd(&other.den);
        let (d_over_g, b_over_g) = match (other.den.divrem(&g), self.den.divrem(&g)) {
            (Some((d, _)), Some((b, _))) => (d, b),
            _ => return Err(MoneyError::DivisionByZero),
        };
        let left = mul_bounded(&self.num, &d_over_g, "add")?;
        let right = mul_bounded(&other.num, &b_over_g, "add")?;
        let den = mul_bounded(&self.den, &d_over_g, "add")?;
        let (negative, num) = if self.negative == other.negative {
            (self.negative, left.add(&right))
        } else {
            match left.cmp(&right) {
                Ordering::Equal => return Ok(Self::zero()),
                Ordering::Greater => (
                    self.negative,
                    left.checked_sub(&right)
                        .ok_or(MoneyError::NumericLimit { operation: "add" })?,
                ),
                Ordering::Less => (
                    other.negative,
                    right
                        .checked_sub(&left)
                        .ok_or(MoneyError::NumericLimit { operation: "add" })?,
                ),
            }
        };
        Self::normalized(negative, num, den)
    }

    /// `self - other`.
    ///
    /// # Errors
    ///
    /// As [`ExactRatio::checked_add`].
    pub fn checked_sub(&self, other: &Self) -> Result<Self, MoneyError> {
        self.checked_add(&other.negated())
    }

    /// `self * other`, cancelling common factors across the operands first.
    ///
    /// # Errors
    ///
    /// [`MoneyError::NumericLimit`] when the product exceeds [`MAX_BITS`].
    pub fn checked_mul(&self, other: &Self) -> Result<Self, MoneyError> {
        if self.is_zero() || other.is_zero() {
            return Ok(Self::zero());
        }
        let g1 = self.num.gcd(&other.den);
        let g2 = other.num.gcd(&self.den);
        let (a, d, c, b) = match (
            self.num.divrem(&g1),
            other.den.divrem(&g1),
            other.num.divrem(&g2),
            self.den.divrem(&g2),
        ) {
            (Some((a, _)), Some((d, _)), Some((c, _)), Some((b, _))) => (a, d, c, b),
            _ => return Err(MoneyError::DivisionByZero),
        };
        let num = mul_bounded(&a, &c, "multiply")?;
        let den = mul_bounded(&b, &d, "multiply")?;
        // Already in lowest terms after cross-cancelling.
        Ok(Self {
            negative: self.negative != other.negative,
            num,
            den,
        })
    }

    /// `self / other`.
    ///
    /// # Errors
    ///
    /// [`MoneyError::DivisionByZero`] when `other` is zero; [`MoneyError::NumericLimit`] as for
    /// multiplication.
    pub fn checked_div(&self, other: &Self) -> Result<Self, MoneyError> {
        if other.is_zero() {
            return Err(MoneyError::DivisionByZero);
        }
        let reciprocal = Self {
            negative: other.negative,
            num: other.den.clone(),
            den: other.num.clone(),
        };
        self.checked_mul(&reciprocal)
    }

    /// `quantum * round(self / quantum, mode)`: the named rounding boundary.
    ///
    /// The result carries the increment's scale (`85.00` for a `0.01` increment). It is exact:
    /// the division is performed on integers and only the final integer is rounded.
    ///
    /// # Errors
    ///
    /// [`MoneyError::NumericLimit`] when the working values exceed [`MAX_BITS`] or the quantized
    /// result is outside the boundary decimal range (coefficient below 2^96).
    pub fn quantize(
        &self,
        increment: &Increment,
        mode: RoundingMode,
    ) -> Result<Decimal, MoneyError> {
        let limit = MoneyError::NumericLimit {
            operation: "quantize",
        };
        // self / q = (num * 10^s) / (den * m)
        let numer = mul_bounded(&self.num, &Nat::pow10(increment.scale()), "quantize")?;
        let denom = mul_bounded(&self.den, increment.mantissa_nat(), "quantize")?;
        let (quotient, remainder) = numer.divrem(&denom).ok_or(MoneyError::DivisionByZero)?;
        let inexact = !remainder.is_zero();
        let ord = remainder.add(&remainder).cmp(&denom);
        let up = match mode {
            RoundingMode::TowardZero => false,
            RoundingMode::AwayFromZero => inexact,
            RoundingMode::HalfAwayFromZero => ord != Ordering::Less,
            RoundingMode::HalfTowardZero => ord == Ordering::Greater,
            RoundingMode::HalfEven => {
                ord == Ordering::Greater || (ord == Ordering::Equal && quotient.is_odd())
            }
            RoundingMode::Ceiling => inexact && !self.negative,
            RoundingMode::Floor => inexact && self.negative,
        };
        let units = if up {
            quotient.add(&Nat::from_u128(1))
        } else {
            quotient
        };
        let magnitude = mul_bounded(&units, increment.mantissa_nat(), "quantize")?;
        if magnitude.bits() > MAX_COEFFICIENT_BITS {
            return Err(limit);
        }
        let magnitude =
            i128::try_from(magnitude.to_u128().ok_or(limit.clone())?).map_err(|_| limit.clone())?;
        let signed = if self.negative { -magnitude } else { magnitude };
        Decimal::try_from_i128_with_scale(signed, increment.scale()).map_err(|_| limit)
    }
}

impl PartialOrd for ExactRatio {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExactRatio {
    /// Exact comparison by cross-multiplication. The two products are temporaries of at most
    /// twice [`MAX_BITS`] bits and are never stored.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.signum(), other.signum()) {
            (a, b) if a != b => a.cmp(&b),
            (0, _) => Ordering::Equal,
            _ => {
                let magnitude = self.num.mul(&other.den).cmp(&other.num.mul(&self.den));
                if self.negative {
                    magnitude.reverse()
                } else {
                    magnitude
                }
            }
        }
    }
}

impl fmt::Display for ExactRatio {
    /// `n` for an integer, otherwise `n/d`; the same text as the canonical numerator and
    /// denominator strings.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den.is_one() {
            f.write_str(&self.numerator_string())
        } else {
            write!(
                f,
                "{}/{}",
                self.numerator_string(),
                self.denominator_string()
            )
        }
    }
}

/// `x * y`, refused before allocation when the product cannot fit [`MAX_BITS`] and checked again
/// after (a product has `bits(x) + bits(y) - 1` or `bits(x) + bits(y)` bits).
fn mul_bounded(x: &Nat, y: &Nat, operation: &'static str) -> Result<Nat, MoneyError> {
    if x.is_zero() || y.is_zero() {
        return Ok(Nat::zero());
    }
    if x.bits() + y.bits() - 1 > MAX_BITS {
        return Err(MoneyError::NumericLimit { operation });
    }
    let product = x.mul(y);
    if product.bits() > MAX_BITS {
        return Err(MoneyError::NumericLimit { operation });
    }
    Ok(product)
}
