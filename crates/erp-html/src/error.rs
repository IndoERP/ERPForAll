//! The error type of the sanitiser.
//!
//! Messages are fixed text plus numbers (offsets, sizes, limits). They never contain the input,
//! so an error can be logged or shown without leaking the markup that caused it.

use std::fmt;
use std::str::Utf8Error;

use crate::report::RemovalReport;

/// Why the parser could not produce a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseFailure {
    /// Sanitising the sanitised output a second time changed it, so the result is not stable.
    ReparseUnstable,
}

impl fmt::Display for ParseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseFailure::ReparseUnstable => {
                f.write_str("the sanitised output changed when parsed again")
            }
        }
    }
}

impl std::error::Error for ParseFailure {}

/// A sanitising call failed. Nothing is returned as a partial or lossy success.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HtmlSanitizeError {
    /// Byte input was not valid UTF-8.
    InvalidUtf8 {
        /// The decoder's error, which holds the offset of the first bad byte.
        source: Utf8Error,
    },
    /// The input contains a NUL byte.
    NulByte {
        /// Byte offset of the first NUL.
        offset: u64,
    },
    /// The input is longer than the input limit.
    InputTooLarge {
        /// Input length in bytes.
        len: u64,
        /// The limit that was exceeded.
        limit: u64,
    },
    /// Elements nest deeper than the depth limit.
    DepthExceeded {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// The document has more nodes than the node limit.
    NodeLimit {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// The call used more work units than the work limit.
    WorkLimit {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// The call needed more working memory than the memory limit.
    MemoryLimit {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// The serialised output is longer than the output limit. The output is capped by the smaller
    /// of `output_bytes` and `input_bytes` (it must itself be acceptable input), so `limit` is
    /// whichever of the two was exceeded; text-led input has `input_bytes - 7` bytes of room for
    /// the `<p></p>` wrapper.
    OutputTooLarge {
        /// The limit that was exceeded: `min(output_bytes, input_bytes)`.
        limit: u64,
    },
    /// Strict mode found content the policy removes. Carries the same aggregated report that
    /// default cleaning returns; nothing is stored.
    DisallowedContent {
        /// What the policy would have removed.
        report: RemovalReport,
    },
    /// The aggregated removal report is larger than the report limit. The report
    /// is not truncated.
    ReportTooLarge {
        /// The report byte limit that was exceeded.
        limit: u64,
    },
    /// The parser failed; see [`ParseFailure`].
    ParseFailed(ParseFailure),
    /// The sanitiser found an inconsistency inside itself (an allocation that failed, a broken
    /// budget account, a handle that names no node). It is a bug or an exhausted allocator, never
    /// a limit the input exceeded. The reason is fixed text and never holds input.
    Internal {
        /// Fixed description of the fault.
        reason: &'static str,
    },
}

impl fmt::Display for HtmlSanitizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HtmlSanitizeError::InvalidUtf8 { source } => {
                write!(
                    f,
                    "input is not valid UTF-8 (valid up to byte {})",
                    source.valid_up_to()
                )
            }
            HtmlSanitizeError::NulByte { offset } => {
                write!(f, "input contains a NUL byte at offset {offset}")
            }
            HtmlSanitizeError::InputTooLarge { len, limit } => {
                write!(f, "input of {len} bytes exceeds the limit of {limit} bytes")
            }
            HtmlSanitizeError::DepthExceeded { limit } => {
                write!(f, "element nesting exceeds the depth limit of {limit}")
            }
            HtmlSanitizeError::NodeLimit { limit } => {
                write!(f, "document exceeds the node limit of {limit}")
            }
            HtmlSanitizeError::WorkLimit { limit } => {
                write!(f, "sanitising exceeds the work limit of {limit} units")
            }
            HtmlSanitizeError::MemoryLimit { limit } => {
                write!(
                    f,
                    "sanitising exceeds the working memory limit of {limit} bytes"
                )
            }
            HtmlSanitizeError::OutputTooLarge { limit } => {
                write!(
                    f,
                    "output exceeds the limit of {limit} bytes (output is capped by min(output_bytes, input_bytes))"
                )
            }
            HtmlSanitizeError::DisallowedContent { report } => write!(
                f,
                "strict mode: the policy removes {} item(s) in {} group(s)",
                report.total_count(),
                report.entries().len()
            ),
            HtmlSanitizeError::ReportTooLarge { limit } => {
                write!(f, "removal report exceeds the limit of {limit} bytes")
            }
            HtmlSanitizeError::ParseFailed(failure) => write!(f, "HTML parsing failed: {failure}"),
            HtmlSanitizeError::Internal { reason } => {
                write!(f, "internal sanitiser error: {reason}")
            }
        }
    }
}

impl std::error::Error for HtmlSanitizeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HtmlSanitizeError::InvalidUtf8 { source } => Some(source),
            HtmlSanitizeError::ParseFailed(failure) => Some(failure),
            HtmlSanitizeError::NulByte { .. }
            | HtmlSanitizeError::InputTooLarge { .. }
            | HtmlSanitizeError::DepthExceeded { .. }
            | HtmlSanitizeError::NodeLimit { .. }
            | HtmlSanitizeError::WorkLimit { .. }
            | HtmlSanitizeError::MemoryLimit { .. }
            | HtmlSanitizeError::OutputTooLarge { .. }
            | HtmlSanitizeError::DisallowedContent { .. }
            | HtmlSanitizeError::ReportTooLarge { .. }
            | HtmlSanitizeError::Internal { .. } => None,
        }
    }
}
