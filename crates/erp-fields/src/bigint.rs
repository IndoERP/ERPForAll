// SPDX-License-Identifier: MIT OR Apache-2.0
//! Exact signed 64-bit scalar, separate from record identifiers.
use crate::{Error, Result};
use std::{fmt, str::FromStr};

/// A signed PostgreSQL int8 scalar. JSON-2 uses canonical decimal strings.
///
/// ```
/// use erp_fields::BigInt;
/// let value = BigInt::parse("9223372036854775807")?;
/// assert_eq!(value.get(), i64::MAX);
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct BigInt(i64);
impl BigInt {
    /// Preserve every signed bit, including zero.
    pub const fn from_i64(value: i64) -> Self {
        Self(value)
    }
    /// The exact signed value.
    pub const fn get(self) -> i64 {
        self.0
    }
    /// Parse canonical signed decimal text, bounded before parsing.
    ///
    /// # Errors
    /// Validation for noncanonical text or values outside i64.
    pub fn parse(value: &str) -> Result<Self> {
        let invalid =
            || Error::validation("invalid bigint: expected canonical signed 64-bit decimal text");
        if value.is_empty() || value.len() > 20 {
            return Err(invalid());
        }
        // An absent minus keeps the original grammar input; shape checks follow.
        let digits = value.strip_prefix('-').unwrap_or(value);
        if digits.is_empty()
            || !digits.as_bytes().iter().all(u8::is_ascii_digit)
            || (digits.len() > 1 && digits.starts_with('0'))
            || value == "-0"
        {
            return Err(invalid());
        }
        value.parse::<i64>().map(Self).map_err(|_| invalid())
    }
}
impl fmt::Display for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for BigInt {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}
