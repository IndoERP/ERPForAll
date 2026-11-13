// SPDX-License-Identifier: MIT OR Apache-2.0
//! Payload-free refusals for owner-side batch construction.
use super::CodecError;
use erp_fields::Error;
use std::fmt;

/// Why an owner-side batch could not be constructed. No variant carries row contents.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SealerProblem {
    /// Invalid row-count or stored-byte admission limits.
    Options,
    /// A row uses an unsupported audit schema version.
    RowSchema,
    /// A persisted value cannot be mapped without loss.
    RowValue,
    /// A stored byte estimate is zero or negative.
    EncodedBytes,
    /// No row can fit the requested stored-byte budget.
    FirstRow,
    /// A single transport payload exceeds the fixed safety bound.
    RowPayload,
    /// Decoding exceeded an independent fixed resource bound.
    DecodeLimit,
    /// Duplicate, reversed or invalid ordered cursors.
    Cursor,
    /// Positive SQL-bigint batch numbers are exhausted.
    BatchOverflow,
    /// The insertion returned something other than exactly one seal.
    Insertion,
    /// The canonical codec refused a value or envelope.
    Codec(CodecError),
}
impl fmt::Display for SealerProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a later release: ")?;
        match self {
            Self::Options => {
                f.write_str("limits require 1..=65536 rows and 1..=8388608 stored bytes")
            }
            Self::RowSchema => f.write_str("unsupported persisted audit schema version"),
            Self::RowValue => f.write_str("persisted audit value has an unsupported shape or type"),
            Self::EncodedBytes => f.write_str("persisted encoded_bytes must be positive"),
            Self::FirstRow => f.write_str("first row exceeds the batch byte limit"),
            Self::RowPayload => f.write_str("projected row payload exceeds the safety limit"),
            Self::DecodeLimit => f.write_str("persisted value decoding exceeds the safety limit"),
            Self::Cursor => {
                f.write_str("persisted audit cursors are invalid or not strictly increasing")
            }
            Self::BatchOverflow => {
                f.write_str("batch number exhausted SQL bigint; owner investigation is required")
            }
            Self::Insertion => f.write_str("seal insertion did not return exactly one row"),
            Self::Codec(error) => write!(f, "{error}"),
        }
    }
}
impl std::error::Error for SealerProblem {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codec(e) => Some(e),
            _ => None,
        }
    }
}
impl From<SealerProblem> for Error {
    fn from(value: SealerProblem) -> Self {
        Error::programming(value.to_string()).with_source(value)
    }
}
pub(super) fn codec(error: CodecError) -> Error {
    SealerProblem::Codec(error).into()
}
