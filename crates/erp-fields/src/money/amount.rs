//! Currency-tagged amounts (H41).

use std::fmt;

use rust_decimal::Decimal;

use super::error::MoneyError;
use super::rounding::{Increment, RoundingMode};

/// A validated currency code: exactly three ASCII letters or digits (`res.currency.name` is a
/// three-character code; ISO 4217 codes are letters, local codes may carry digits).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CurrencyCode([u8; 3]);

impl CurrencyCode {
    /// Validates `code`.
    ///
    /// # Errors
    ///
    /// [`MoneyError::InvalidCurrencyCode`].
    pub fn new(code: &str) -> Result<Self, MoneyError> {
        match code.as_bytes() {
            [a, b, c] if [a, b, c].iter().all(|x| x.is_ascii_alphanumeric()) => {
                Ok(Self([*a, *b, *c]))
            }
            _ => Err(MoneyError::InvalidCurrencyCode),
        }
    }

    /// The code's three ASCII bytes.
    pub fn as_bytes(&self) -> &[u8; 3] {
        &self.0
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|b| fmt::Write::write_char(f, char::from(*b)))
    }
}

/// A decimal amount in one currency. Arithmetic is checked and refuses to mix currencies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Amount {
    value: Decimal,
    currency: CurrencyCode,
}

impl Amount {
    /// An amount of `currency`.
    pub fn new(value: Decimal, currency: CurrencyCode) -> Self {
        Self { value, currency }
    }

    /// The numeric value.
    pub fn value(&self) -> Decimal {
        self.value
    }

    /// The currency.
    pub fn currency(&self) -> CurrencyCode {
        self.currency
    }

    /// `self + other`.
    ///
    /// # Errors
    ///
    /// [`MoneyError::CurrencyMismatch`], or [`MoneyError::NumericLimit`] on overflow.
    pub fn checked_add(&self, other: &Amount) -> Result<Amount, MoneyError> {
        self.same_currency(other)?;
        let value = self
            .value
            .checked_add(other.value)
            .ok_or(MoneyError::NumericLimit { operation: "add" })?;
        Ok(Amount::new(value, self.currency))
    }

    /// `self - other`.
    ///
    /// # Errors
    ///
    /// As [`Amount::checked_add`].
    pub fn checked_sub(&self, other: &Amount) -> Result<Amount, MoneyError> {
        self.same_currency(other)?;
        let value = self
            .value
            .checked_sub(other.value)
            .ok_or(MoneyError::NumericLimit {
                operation: "subtract",
            })?;
        Ok(Amount::new(value, self.currency))
    }

    /// The amount rounded to `increment`.
    ///
    /// # Errors
    ///
    /// As [`Increment::quantize`].
    pub fn quantize(
        &self,
        increment: &Increment,
        mode: RoundingMode,
    ) -> Result<Amount, MoneyError> {
        Ok(Amount::new(
            increment.quantize(self.value, mode)?,
            self.currency,
        ))
    }

    fn same_currency(&self, other: &Amount) -> Result<(), MoneyError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch)
        }
    }
}
