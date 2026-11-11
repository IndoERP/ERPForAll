//! A purpose-neutral PostgreSQL UUID value. Correlation, execution and event
//! identities impose their own policy; this scalar preserves all 128 bits.
use std::{fmt, str::FromStr};

use crate::{Error, Result};

/// A native UUID, stored as its sixteen bytes. Every 128-bit value, including nil,
/// is valid for this general-purpose scalar; it grants no authority.
///
/// Text parsing requires canonical lowercase hexadecimal and `8-4-4-4-12` grouping.
/// No version, variant, time or correlation policy is inferred from a stored UUID.
///
/// ```
/// use erp_fields::Uuid;
/// let id = Uuid::parse("01234567-89ab-cdef-0123-456789abcdef")?;
/// assert_eq!(Uuid::from_bytes(*id.as_bytes()), id);
/// assert_eq!(id.to_string(), "01234567-89ab-cdef-0123-456789abcdef");
/// assert!(Uuid::NIL.is_nil());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uuid([u8; 16]);

impl Uuid {
    /// The nil UUID. Nil is a value, never an implicit default or SQL NULL.
    pub const NIL: Self = Self([0; 16]);

    /// Preserve exactly these bytes without imposing application identity policy.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The native sixteen bytes, in UUID network order.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Whether every bit is zero. This does not test SQL nullability.
    pub const fn is_nil(self) -> bool {
        u128::from_be_bytes(self.0) == 0
    }

    /// Parse canonical lowercase UUID text, without silently normalizing input.
    ///
    /// # Errors
    /// `Validation` for wrong length/grouping, nonhex input or uppercase text.
    /// The error does not include the rejected input.
    pub fn parse(value: &str) -> Result<Self> {
        const INVALID: &str =
            "invalid UUID: expected canonical lowercase 8-4-4-4-12 hexadecimal text";
        let raw = value.as_bytes();
        if raw.len() != 36 {
            return Err(Error::validation(INVALID));
        }
        let mut bytes = [0; 16];
        let mut nibble = 0;
        for (index, &byte) in raw.iter().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) {
                if byte != b'-' {
                    return Err(Error::validation(INVALID));
                }
                continue;
            }
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => return Err(Error::validation(INVALID)),
            };
            if nibble % 2 == 0 {
                bytes[nibble / 2] = value << 4;
            } else {
                bytes[nibble / 2] |= value;
            }
            nibble += 1;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Uuid {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}
