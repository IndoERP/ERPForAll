//! `xid8`: a 64-bit PostgreSQL transaction id in its canonical wire form.

use std::fmt;

use crate::error::WireError;

/// A PostgreSQL `xid8` value, 1..=2^64-1.
///
/// On the wire it is a canonical decimal string: digits only, no sign, no leading zero. The
/// ordering is numeric. The inner value is private so that zero cannot exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Xid8(u64);

impl Xid8 {
    /// Wrap a number.
    ///
    /// # Errors
    /// [`WireError::BadDescriptor`] for zero.
    pub fn new(value: u64) -> Result<Self, WireError> {
        if value == 0 {
            return Err(bad("xid", ZERO));
        }
        Ok(Self(value))
    }

    /// The number.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Parse the canonical decimal form.
    ///
    /// # Errors
    /// [`WireError::BadDescriptor`] (path `xid`) for an empty string, a sign or other non-digit,
    /// a leading zero, zero, or a value above 2^64-1.
    pub fn parse(text: &str) -> Result<Self, WireError> {
        Self::parse_at(text, "xid")
    }

    pub(crate) fn parse_at(text: &str, path: &str) -> Result<Self, WireError> {
        match parse_decimal(text) {
            Ok(0) => Err(bad(path, ZERO)),
            Ok(value) => Ok(Self(value)),
            Err(reason) => Err(bad(path, reason)),
        }
    }
}

const ZERO: &str = "zero is not an xid8";

fn bad(path: &str, reason: &str) -> WireError {
    WireError::BadDescriptor {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Parse a canonical decimal `u64`: digits only, no sign, no leading zero (zero itself is
/// accepted here; callers that forbid it check the value).
pub(crate) fn parse_decimal(text: &str) -> Result<u64, &'static str> {
    if text.is_empty() {
        return Err("empty");
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err("not a decimal digit");
    }
    if text.len() > 1 && text.starts_with('0') {
        return Err("leading zero");
    }
    text.parse::<u64>().map_err(|_| "above 2^64-1")
}

impl fmt::Display for Xid8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
