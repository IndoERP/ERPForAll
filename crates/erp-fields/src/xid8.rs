//! Full-width PostgreSQL transaction identifiers, without xid32 wrapping or a
//! signed integer intermediary. This scalar is not transaction/commit authority.
use std::{fmt, str::FromStr};

use crate::{Error, Result};

/// A PostgreSQL `xid8`, preserving all unsigned 64 bits. Zero and other special
/// bit patterns are values, not claims that a transaction is active or committed.
/// Native outbox assignment comes from PostgreSQL, independently of this scalar.
///
/// Text is canonical unsigned decimal. There is no implicit `i64`, `f64` or xid32
/// conversion, arithmetic, generated id or default.
///
/// ```
/// use erp_fields::Xid8;
/// let id = Xid8::parse("18446744073709551615")?;
/// assert_eq!(id.get(), u64::MAX);
/// assert_eq!(id.to_string(), "18446744073709551615");
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Xid8(u64);

impl Xid8 {
    /// Preserve all bits, without declaring a transaction's validity or visibility.
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// The full unsigned value; never a wrapping xid32 or a signed cast.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Parse canonical unsigned decimal text, including the complete u64 range.
    ///
    /// # Errors
    /// `Validation` for empty, nondecimal, noncanonical or overflowing text. The
    /// rejected input is not included in the error.
    pub fn parse(value: &str) -> Result<Self> {
        const INVALID: &str = "invalid xid8: expected canonical unsigned 64-bit decimal text";
        if value.is_empty()
            || value.len() > 20
            || !value.as_bytes().iter().all(u8::is_ascii_digit)
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(Error::validation(INVALID));
        }
        value
            .parse::<u64>()
            .map(Self)
            .map_err(|_| Error::validation(INVALID))
    }
}

impl fmt::Display for Xid8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Xid8 {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}
