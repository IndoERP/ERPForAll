//! Errors of the exact money primitives.

use std::fmt;

use crate::Error;

/// Why an exact money operation was refused. Every variant is a user-visible, stable message
/// (`Display`); callers match on the variant, never on the text.
///
/// The set follows the kernel errors that the H41 primitives can raise. The
/// evaluation-budget error (`BudgetExceeded`) belongs to the tax kernel that owns a budget, and
/// `UnsupportedFeature` to the slices that name what they defer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MoneyError {
    /// A decimal string, or a value built from parts, is outside the boundary decimal contract
    /// (strict lexeme, at most 64 bytes, coefficient below 2^96, scale at most 28).
    InvalidDecimal {
        /// What was wrong, in one short phrase.
        reason: &'static str,
    },
    /// A rounding increment is zero or negative, or has more than 28 fractional digits.
    InvalidIncrement,
    /// A division by an exact zero.
    DivisionByZero,
    /// A value or an intermediate result exceeds a numeric bound: 4096 bits per numerator or
    /// denominator, or the boundary decimal range for a quantized result.
    NumericLimit {
        /// The operation that was refused.
        operation: &'static str,
    },
    /// Two amounts of different currencies were combined.
    CurrencyMismatch,
    /// A currency code is not three ASCII letters or digits.
    InvalidCurrencyCode,
    /// A conversion rate is zero or negative.
    InvalidRate,
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDecimal { reason } => write!(f, "Invalid decimal amount: {reason}."),
            Self::InvalidIncrement => f.write_str(
                "The rounding increment must be greater than 0 and have at most 28 decimals.",
            ),
            Self::DivisionByZero => f.write_str("Division by zero in a monetary calculation."),
            Self::NumericLimit { operation } => write!(
                f,
                "The monetary value is out of the supported range ({operation})."
            ),
            Self::CurrencyMismatch => {
                f.write_str("Amounts in different currencies cannot be combined.")
            }
            Self::InvalidCurrencyCode => {
                f.write_str("A currency code must be three ASCII letters or digits.")
            }
            Self::InvalidRate => f.write_str("A currency rate must be strictly positive."),
        }
    }
}

impl std::error::Error for MoneyError {}

impl From<MoneyError> for Error {
    /// All money errors are caller or data mistakes, reported as validation errors.
    fn from(value: MoneyError) -> Self {
        Error::validation(value.to_string()).with_source(value)
    }
}
