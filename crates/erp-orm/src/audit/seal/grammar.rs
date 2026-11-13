// SPDX-License-Identifier: MIT OR Apache-2.0
//! Format-1 value grammar. Compound values carry their validated depth, not recursive trees.
use std::fmt;

pub(super) const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_ITEMS: usize = 65_536;
const MAX_DEPTH: u8 = 16;

/// A format-1 refusal. Messages contain no row values, column names or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecError {
    /// A canonical encoding safety limit was exceeded.
    Limit(&'static str),
    /// An object or row contains the same name twice.
    DuplicateName,
    /// A decimal token is malformed or nonfinite.
    InvalidNumber,
    /// The supplied row does not contain exactly the schema-1 columns.
    ColumnSet,
    /// Empty batches cannot be sealed.
    EmptyBatch,
    /// A cursor is negative or the upper bound does not follow the lower bound.
    InvalidCursor,
    /// An incarnation is nil or an epoch/batch number is outside the positive bigint range.
    InvalidIdentity,
    /// A genesis seal has a predecessor or a subsequent seal lacks one.
    InvalidPredecessor,
}
impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("audit seal codec: ")?;
        match self {
            Self::Limit(limit) => write!(f, "{limit} limit exceeded"),
            Self::DuplicateName => f.write_str("duplicate name"),
            Self::InvalidNumber => f.write_str("invalid decimal number"),
            Self::ColumnSet => f.write_str("columns do not match audit schema 1"),
            Self::EmptyBatch => f.write_str("empty batch"),
            Self::InvalidCursor => f.write_str("invalid cursor"),
            Self::InvalidIdentity => f.write_str("invalid seal identity"),
            Self::InvalidPredecessor => f.write_str("invalid predecessor hash"),
        }
    }
}
impl std::error::Error for CodecError {}

/// A validated canonical value. No public deserializer or raw-byte constructor bypasses validation.
///
/// Limits: 8 MiB encoded bytes, 65,536 members and 16 nested containers. Input collections must
/// already be bounded by the caller. SQL NULL uses `null`; absent versioned fields use `missing`.
/// The type does not redact data or assert that a value matches its PostgreSQL column type.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CanonicalValue {
    bytes: Vec<u8>,
    depth: u8,
}
impl fmt::Debug for CanonicalValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CanonicalValue")
            .field("encoded_bytes", &self.bytes.len())
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}
impl CanonicalValue {
    fn scalar(bytes: Vec<u8>) -> Self {
        Self { bytes, depth: 0 }
    }
    /// Missing field in a versioned row, distinct from SQL NULL (tag 0).
    pub fn missing() -> Self {
        Self::scalar(vec![0])
    }
    /// SQL NULL or a structured null value (tag 1).
    pub fn null() -> Self {
        Self::scalar(vec![1])
    }
    /// Boolean (tag 2).
    pub fn boolean(value: bool) -> Self {
        Self::scalar(vec![2, u8::from(value)])
    }
    /// Signed int2/int4/int8, encoded as canonical decimal (tag 3).
    pub fn integer(value: i64) -> Self {
        Self::decimal_scalar(3, value.to_string())
    }
    /// xid8, encoded with the integer tag and unsigned decimal, without narrowing to i64.
    pub fn xid(value: u64) -> Self {
        Self::decimal_scalar(3, value.to_string())
    }
    /// Exact decimal, with optional exponent; no whitespace, leading plus or nonfinite values.
    ///
    /// # Errors
    /// `InvalidNumber` for malformed tokens; `Limit` if input or normalized output exceeds 4,096
    /// bytes. Conversion uses decimal digits only and never an f64.
    pub fn numeric(value: &str) -> Result<Self, CodecError> {
        Ok(Self::decimal_scalar(4, super::number::normalize(value)?))
    }
    /// Stored UTF-8 bytes, without Unicode normalization (tag 5).
    ///
    /// # Errors
    /// `Limit` if tag, length and payload exceed 8 MiB.
    pub fn text(value: &str) -> Result<Self, CodecError> {
        Self::variable(5, value.as_bytes())
    }
    /// UTC microseconds since 1970-01-01, including negative timestamps (tag 6).
    pub fn timestamp_micros(value: i64) -> Self {
        Self::decimal_scalar(6, value.to_string())
    }
    /// The 16 UUID bytes in network order, including nil if stored (tag 7).
    pub fn uuid(value: [u8; 16]) -> Self {
        let mut bytes = vec![7];
        bytes.extend_from_slice(&value);
        Self::scalar(bytes)
    }
    /// bytea or an existing stored digest (tag 8).
    ///
    /// # Errors
    /// `Limit` if tag, length and payload exceed 8 MiB.
    pub fn bytes(value: &[u8]) -> Result<Self, CodecError> {
        Self::variable(8, value)
    }
    /// Ordered array (tag 9). Child order is preserved, including empty arrays.
    ///
    /// # Errors
    /// `Limit` for excessive depth, members or total encoded bytes.
    pub fn array(values: &[Self]) -> Result<Self, CodecError> {
        let count = item_count(values.len())?;
        let mut size = 5;
        let mut depth = 1;
        for value in values {
            size = add_size(size, value.bytes.len())?;
            depth = depth.max(value.depth + 1);
        }
        check_depth(depth)?;
        let mut bytes = Vec::with_capacity(size);
        bytes.push(9);
        bytes.extend_from_slice(&count.to_be_bytes());
        for value in values {
            bytes.extend_from_slice(&value.bytes);
        }
        Ok(Self { bytes, depth })
    }
    /// Structured value (tag 10), sorting names bytewise and rejecting duplicates.
    ///
    /// # Errors
    /// `DuplicateName`, or `Limit` for excessive depth, members or total bytes.
    pub fn object(values: &[(&str, Self)]) -> Result<Self, CodecError> {
        let sorted = sorted_members(values)?;
        let mut depth = 1;
        for (_, value) in &sorted {
            depth = depth.max(value.depth + 1);
        }
        check_depth(depth)?;
        let mut bytes = vec![10];
        encode_members(&mut bytes, &sorted)?;
        Ok(Self { bytes, depth })
    }
    /// Canonical bytes. These may contain sensitive input: never log them.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn decimal_scalar(tag: u8, value: String) -> Self {
        // Callers supply i64/u64 decimal or a normalized, <=4,096-byte numeric value.
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
        Self::scalar(bytes)
    }
    fn variable(tag: u8, value: &[u8]) -> Result<Self, CodecError> {
        let size = add_size(5, value.len())?;
        let mut bytes = Vec::with_capacity(size);
        bytes.push(tag);
        put_lp(&mut bytes, value)?;
        Ok(Self::scalar(bytes))
    }
}
fn check_depth(depth: u8) -> Result<(), CodecError> {
    if depth > MAX_DEPTH {
        return Err(CodecError::Limit("depth"));
    }
    Ok(())
}
pub(super) fn item_count(n: usize) -> Result<u32, CodecError> {
    if n > MAX_ITEMS {
        return Err(CodecError::Limit("items"));
    }
    u32::try_from(n).map_err(|_| CodecError::Limit("items"))
}
pub(super) fn add_size(a: usize, b: usize) -> Result<usize, CodecError> {
    a.checked_add(b)
        .filter(|&n| n <= MAX_BYTES)
        .ok_or(CodecError::Limit("encoded bytes"))
}
pub(super) fn put_lp(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CodecError> {
    let n = u32::try_from(bytes.len()).map_err(|_| CodecError::Limit("encoded bytes"))?;
    add_size(add_size(out.len(), 4)?, bytes.len())?;
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}
pub(super) fn sorted_members<'a>(
    values: &'a [(&'a str, CanonicalValue)],
) -> Result<Vec<(&'a str, &'a CanonicalValue)>, CodecError> {
    item_count(values.len())?;
    let mut size = 5;
    for (name, value) in values {
        size = add_size(add_size(add_size(size, 4)?, name.len())?, value.bytes.len())?;
    }
    let mut sorted: Vec<_> = values.iter().map(|(k, v)| (*k, v)).collect();
    sorted.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    if sorted.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(CodecError::DuplicateName);
    }
    Ok(sorted)
}
pub(super) fn encode_members(
    out: &mut Vec<u8>,
    sorted: &[(&str, &CanonicalValue)],
) -> Result<(), CodecError> {
    let mut size = add_size(out.len(), 4)?;
    for (name, value) in sorted {
        size = add_size(add_size(add_size(size, 4)?, name.len())?, value.bytes.len())?;
    }
    out.reserve(size - out.len());
    out.extend_from_slice(&item_count(sorted.len())?.to_be_bytes());
    for (name, value) in sorted {
        put_lp(out, name.as_bytes())?;
        out.extend_from_slice(&value.bytes);
    }
    Ok(())
}
