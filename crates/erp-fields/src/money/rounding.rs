//! Rounding modes and positive increments.

use std::cmp::Ordering;

use rust_decimal::Decimal;

use super::error::MoneyError;
use super::nat::Nat;
use super::ratio::ExactRatio;

/// How a quotient is turned into an integer number of increments. Explicit everywhere: there is
/// no default mode inside the primitives.
///
/// | Mode | Behaviour, increment 1 |
/// |---|---|
/// | `HalfEven` | `2.5 -> 2`, `3.5 -> 4`, `-2.5 -> -2` |
/// | `Ceiling` / `Floor` | toward positive / negative infinity |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RoundingMode {
    /// Ties away from zero.
    HalfAwayFromZero,
    /// Ties toward zero.
    HalfTowardZero,
    /// Ties to the even neighbour (banker's rounding).
    HalfEven,
    /// Any non-zero fraction moves away from zero.
    AwayFromZero,
    /// The fraction is discarded.
    TowardZero,
    /// Toward positive infinity.
    Ceiling,
    /// Toward negative infinity.
    Floor,
}

/// A strictly positive rounding increment with at most 28 decimals, normalized (`1.000000` is
/// `1`, `0.050` is `0.05`). The scale of an increment is the scale of every amount quantized to
/// it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Increment {
    value: Decimal,
    mantissa: Nat,
    scale: u32,
}

impl Increment {
    /// An increment from a decimal.
    ///
    /// # Errors
    ///
    /// [`MoneyError::InvalidIncrement`] when `value` is zero or negative.
    pub fn new(value: Decimal) -> Result<Self, MoneyError> {
        if value.is_sign_negative() || value.is_zero() {
            return Err(MoneyError::InvalidIncrement);
        }
        let value = value.normalize();
        Ok(Self {
            mantissa: Nat::from_u128(value.mantissa().unsigned_abs()),
            scale: value.scale(),
            value,
        })
    }

    /// The increment `1` (whole units).
    pub fn unit() -> Self {
        Self {
            value: Decimal::ONE,
            mantissa: Nat::from_u128(1),
            scale: 0,
        }
    }

    /// The increment `10^-places`.
    ///
    /// # Errors
    ///
    /// [`MoneyError::InvalidIncrement`] for more than 28 places.
    pub fn from_places(places: u32) -> Result<Self, MoneyError> {
        if places > 28 {
            return Err(MoneyError::InvalidIncrement);
        }
        Ok(Self {
            value: Decimal::new(1, places),
            mantissa: Nat::from_u128(1),
            scale: places,
        })
    }

    /// Number of decimals of amounts quantized to this increment.
    pub fn scale(&self) -> u32 {
        self.scale
    }

    pub(super) fn mantissa_nat(&self) -> &Nat {
        &self.mantissa
    }

    /// The increment as a decimal (normalized).
    pub fn as_decimal(&self) -> Decimal {
        self.value
    }

    /// `amount` rounded to a multiple of this increment.
    ///
    /// # Errors
    ///
    /// [`MoneyError::NumericLimit`] when the result is outside the boundary decimal range.
    pub fn quantize(&self, amount: Decimal, mode: RoundingMode) -> Result<Decimal, MoneyError> {
        ExactRatio::from_decimal(amount)?.quantize(self, mode)
    }

    /// # Errors
    ///
    /// As [`Increment::quantize`].
    pub fn is_zero(&self, amount: Decimal, mode: RoundingMode) -> Result<bool, MoneyError> {
        Ok(self.quantize(amount, mode)?.is_zero())
    }

    /// # Errors
    ///
    /// As [`Increment::quantize`].
    pub fn compare(
        &self,
        left: Decimal,
        right: Decimal,
        mode: RoundingMode,
    ) -> Result<Ordering, MoneyError> {
        Ok(self.quantize(left, mode)?.cmp(&self.quantize(right, mode)?))
    }

    /// Whether `amount` is an exact multiple of this increment (stored amounts are validated with
    /// this before they are written, D2).
    ///
    /// # Errors
    ///
    /// [`MoneyError::NumericLimit`] only for working values beyond the bound.
    pub fn contains(&self, amount: Decimal) -> Result<bool, MoneyError> {
        let exact = ExactRatio::from_decimal(amount)?;
        let down = exact.quantize(self, RoundingMode::TowardZero)?;
        Ok(down == amount)
    }
}
