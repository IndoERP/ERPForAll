// SPDX-License-Identifier: MIT OR Apache-2.0
//! External evidence contracts. Construction does not establish database provenance.
use super::SealIdentity;
use std::{fmt, io::ErrorKind};

/// A validated external seal anchor. Contains identifiers and a digest, never audit rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Anchor {
    identity: SealIdentity,
    seal_hash: [u8; 32],
    format_version: u16,
}
impl Anchor {
    /// Construct an anchor for a confirmed, independently validated seal.
    /// This constructor checks format support; callers establish the seal's provenance.
    /// # Errors
    /// Returns [`WitnessError::UnsupportedVersion`] unless the format is 1.
    pub fn new(
        identity: SealIdentity,
        seal_hash: [u8; 32],
        format_version: u16,
    ) -> Result<Self, WitnessError> {
        if format_version != 1 {
            return Err(WitnessError::UnsupportedVersion);
        }
        Ok(Self {
            identity,
            seal_hash,
            format_version,
        })
    }
    /// Incarnation, epoch and batch identity.
    pub fn identity(&self) -> SealIdentity {
        self.identity
    }
    /// SHA-256 seal digest in output byte order.
    pub fn seal_hash(&self) -> &[u8; 32] {
        &self.seal_hash
    }
    /// Accepted envelope format version.
    pub fn format_version(&self) -> u16 {
        self.format_version
    }
}

/// Inclusive positive SQL-bigint batch bounds, not a request to allocate their numerical width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AnchorRange {
    first: u64,
    last: u64,
}
impl AnchorRange {
    /// Validate inclusive bounds.
    /// # Errors
    /// Returns [`WitnessError::InvalidRange`] for zero, reversed or out-of-bigint bounds.
    pub fn new(first: u64, last: u64) -> Result<Self, WitnessError> {
        if first == 0 || first > last || last > i64::MAX as u64 {
            return Err(WitnessError::InvalidRange);
        }
        Ok(Self { first, last })
    }
    /// Whether a batch number lies in these inclusive bounds.
    pub fn contains(self, batch_no: u64) -> bool {
        (self.first..=self.last).contains(&batch_no)
    }
}

/// Synchronous operator-side witness. Never call from a request transaction.
/// Publish only after a confirmed database commit and validation of the stored seal.
/// Implementations bound input/output and validate existing evidence before returning results.
pub trait AuditWitness {
    /// Durably publish an anchor; an identical retry is idempotent.
    /// # Errors
    /// A conflict, unavailable/busy storage, invalid existing evidence or publication uncertainty.
    fn publish(&self, anchor: &Anchor) -> Result<(), WitnessError>;
    /// Fetch a bounded scoped set. Empty results do not establish external assurance.
    /// # Errors
    /// Invalid identity, unavailable/busy storage, limits or malformed/conflicting evidence.
    fn fetch(
        &self,
        incarnation: [u8; 16],
        epoch: u64,
        range: AnchorRange,
    ) -> Result<Vec<Anchor>, WitnessError>;
}

/// Closed operation labels prevent arbitrary storage text from entering diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WitnessOperation {
    /// Open the existing operator directory or witness file.
    Open,
    /// Inspect the admitted file and its directory entry.
    Inspect,
    /// Acquire the operating-system file lock.
    Lock,
    /// Read existing witness evidence.
    Read,
    /// Append a complete anchor record.
    Write,
    /// Synchronize witness contents.
    SyncFile,
    /// Synchronize the parent directory entry.
    SyncDirectory,
}
impl fmt::Display for WitnessOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::Inspect => "inspect",
            Self::Lock => "lock",
            Self::Read => "read",
            Self::Write => "write",
            Self::SyncFile => "file synchronization",
            Self::SyncDirectory => "directory synchronization",
        })
    }
}

/// Payload-free witness failure. I/O kinds preserve failure classification without paths or JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WitnessError {
    /// Unsupported seal format.
    UnsupportedVersion,
    /// Invalid nonnil incarnation or positive SQL-bigint epoch/batch.
    InvalidIdentity,
    /// Invalid inclusive batch bounds.
    InvalidRange,
    /// Different anchors use one identity.
    Conflict,
    /// Another cooperating publisher contends for creation or holds the file lock.
    Busy,
    /// The opened object is not an admitted regular, single-link file.
    InvalidFile,
    /// A complete line failed strict decoding. The line number is one-based.
    MalformedRecord {
        /// One-based physical line.
        line: u32,
    },
    /// The final record lacks a newline and must not be repaired automatically.
    TornRecord {
        /// One-based physical line.
        line: u32,
    },
    /// A named fixed safety bound was exceeded.
    Limit(&'static str),
    /// Storage failed before any publication was attempted.
    Storage {
        /// Failed bounded operation.
        operation: WitnessOperation,
        /// Operating-system failure classification.
        kind: ErrorKind,
    },
    /// An append or synchronization failed; evidence may already exist and must be reconciled.
    PublicationUncertain {
        /// Failed bounded operation.
        operation: WitnessOperation,
        /// Operating-system failure classification.
        kind: ErrorKind,
    },
    /// This platform does not implement the required file semantics.
    UnsupportedPlatform,
}
impl fmt::Display for WitnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("audit witness: ")?;
        match self {
            Self::UnsupportedVersion => f.write_str("unsupported format version"),
            Self::InvalidIdentity => f.write_str("invalid anchor identity"),
            Self::InvalidRange => f.write_str("invalid batch range"),
            Self::Conflict => f.write_str("conflicting anchor identity"),
            Self::Busy => f.write_str("storage is busy; retry publication or read"),
            Self::InvalidFile => f.write_str("requires an unchanged regular single-link file"),
            Self::MalformedRecord { line } => write!(f, "malformed record at line {line}"),
            Self::TornRecord { line } => write!(
                f,
                "torn record at line {line}; operator reconciliation required"
            ),
            Self::Limit(bound) => write!(f, "{bound} limit exceeded"),
            Self::Storage { operation, kind } => write!(f, "{operation} failed ({kind:?})"),
            Self::PublicationUncertain { operation, kind } => write!(
                f,
                "{operation} failed ({kind:?}); publication may be incomplete"
            ),
            Self::UnsupportedPlatform => {
                f.write_str("file witness is unsupported on this platform")
            }
        }
    }
}
impl std::error::Error for WitnessError {}
