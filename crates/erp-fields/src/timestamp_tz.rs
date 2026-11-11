//! Explicit-offset, finite microsecond instants. This type does not change the
//! existing naive UTC [`crate::DateTime`] representation or parser.
use std::{fmt, str::FromStr};

use crate::{Error, Result};

const INVALID: &str =
    "invalid timestamptz: expected explicit-offset ISO timestamp with at most 6 fractional digits";
const RANGE: &str = "timestamptz outside supported finite instant range";

/// A bounded timezone-aware instant at whole-microsecond precision.
///
/// Accepted offsets normalize to UTC; the original offset/time zone is not stored.
/// The range starts at [`Self::MIN_UNIX_MICROS`] (the finite lower bound of the
/// existing PostgreSQL codec) and ends at pinned Jiff's [`jiff::Timestamp::MAX`]
/// rounded down to a microsecond. Full PostgreSQL timestamp extremes and infinity
/// are intentionally unsupported and fail explicitly.
///
/// Parsing requires bounded ASCII ISO date/time with `T`, explicit `Z` or a
/// numeric `±HH:MM` offset, and zero through six fractional digits. Unknown
/// `-00:00`, zone annotations, leap-second/24:00 normalization and greater input
/// precision are refused. No implicit conversion from naive Datetime exists.
///
/// ```
/// use erp_fields::TimestampTz;
/// let local = TimestampTz::parse("2024-03-01T01:00:00.123456+07:00")?;
/// assert_eq!(local.to_string(), "2024-02-29T18:00:00.123456Z");
/// assert_eq!(TimestampTz::from_unix_micros(local.unix_micros())?, local);
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimestampTz(jiff::Timestamp);

impl TimestampTz {
    /// Earliest supported instant, in Unix microseconds. Uses the unchanged
    /// current PostgreSQL timestamp codec's lower bound and epoch offset.
    pub const MIN_UNIX_MICROS: i64 = -211_813_488_000_000_000 + 946_684_800_000_000;

    /// An explicit epoch value, not an implicit default and not SQL NULL.
    pub const UNIX_EPOCH: Self = Self(jiff::Timestamp::UNIX_EPOCH);

    /// Latest supported whole-microsecond instant from the pinned time library.
    pub fn max_unix_micros() -> i64 {
        jiff::Timestamp::MAX.as_microsecond()
    }

    /// Construct an instant without rounding, saturation or a naive-time attach.
    ///
    /// # Errors
    /// `Validation` outside the documented finite range.
    pub fn from_unix_micros(micros: i64) -> Result<Self> {
        if micros < Self::MIN_UNIX_MICROS {
            return Err(Error::validation(RANGE));
        }
        jiff::Timestamp::from_microsecond(micros)
            .map(Self)
            .map_err(|_| Error::validation(RANGE))
    }

    /// Exact Unix microseconds; no floating-point conversion.
    pub fn unix_micros(self) -> i64 {
        self.0.as_microsecond()
    }

    /// Parse explicit-offset ISO text and normalize the instant to UTC.
    ///
    /// # Errors
    /// `Validation` for malformed, unknown/absent offset, extra precision,
    /// unsupported normalization or a value outside the finite range. Rejected
    /// input is not included in the error.
    pub fn parse(input: &str) -> Result<Self> {
        validate_shape(input)?;
        let timestamp = input
            .parse::<jiff::Timestamp>()
            .map_err(|_| Error::validation(INVALID))?;
        if timestamp.as_nanosecond() % 1000 != 0 {
            return Err(Error::validation(INVALID));
        }
        Self::from_unix_micros(timestamp.as_microsecond())
    }
}

fn pair(bytes: &[u8]) -> Option<u8> {
    if bytes.len() == 2 && bytes.iter().all(u8::is_ascii_digit) {
        Some((bytes[0] - b'0') * 10 + bytes[1] - b'0')
    } else {
        None
    }
}

fn validate_shape(input: &str) -> Result<()> {
    let invalid = || Error::validation(INVALID);
    if !(20..=35).contains(&input.len()) || !input.is_ascii() {
        return Err(invalid());
    }
    let bytes = input.as_bytes();
    let signed = matches!(bytes[0], b'+' | b'-');
    let year = if signed { 7 } else { 4 };
    let after_seconds = year + 15;
    if bytes.len() <= after_seconds {
        return Err(invalid());
    }
    let digits = if signed {
        &bytes[1..year]
    } else {
        &bytes[..year]
    };
    if !digits.iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    for (index, expected) in [
        (year, b'-'),
        (year + 3, b'-'),
        (year + 6, b'T'),
        (year + 9, b':'),
        (year + 12, b':'),
    ] {
        if bytes[index] != expected {
            return Err(invalid());
        }
    }
    let hour = pair(&bytes[year + 7..year + 9]).ok_or_else(invalid)?;
    let minute = pair(&bytes[year + 10..year + 12]).ok_or_else(invalid)?;
    let second = pair(&bytes[year + 13..year + 15]).ok_or_else(invalid)?;
    if hour > 23 || minute > 59 || second > 59 {
        return Err(invalid());
    }
    let mut offset = after_seconds;
    if bytes[offset] == b'.' {
        offset += 1;
        let start = offset;
        while offset < bytes.len() && bytes[offset].is_ascii_digit() {
            offset += 1;
        }
        if !(1..=6).contains(&(offset - start)) {
            return Err(invalid());
        }
    }
    let suffix = &bytes[offset..];
    if suffix == b"Z" {
        return Ok(());
    }
    if suffix.len() != 6
        || !matches!(suffix[0], b'+' | b'-')
        || suffix[3] != b':'
        || suffix == b"-00:00"
    {
        return Err(invalid());
    }
    if pair(&suffix[1..3]).ok_or_else(invalid)? > 23
        || pair(&suffix[4..6]).ok_or_else(invalid)? > 59
    {
        return Err(invalid());
    }
    Ok(())
}

impl fmt::Display for TimestampTz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for TimestampTz {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}
