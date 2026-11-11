//! The append-only audit trail (to 9,).
//!
//! Every mutating call, typed auth lifecycle change and audited elevation leaves **one** row in
//! `erp_event.ir_audit_entry`, written by one typed INSERT in the commit preamble of the
//! transaction it describes. The row commits with the change or not at all: a failed audit write
//! rolls the business change back, and a store that is not ready refuses the recording call before
//! any statement. There is no recording, in-memory or no-op sink in any build.
//!
//! Insert-only is enforced by PostgreSQL privileges, not by ORM overrides: the runtime role holds
//! `INSERT` on the columns it supplies and nothing else
//! ([`Transaction::grant_event_runtime_role`](crate::Transaction::grant_event_runtime_role)).
//!
//! What a row never holds: a key, digest, display prefix, key name, password, hash, token, cookie,
//! login text, network address or provider assertion. None of the constructors in this module
//! accepts such a value, so none can reach a row by construction.
//!
//! # Recording from an addon
//!
//! Core authentication code records typed lifecycle events in the transaction that makes the
//! change:
//!
//! ```no_run
//! use erp_orm::audit::{ApiKeyRevoked, AuthEvent, KeyRouteScope, RevokeCause};
//! use erp_orm::{ApiKeyId, KeyKind, Transaction, UserId};
//!
//! # fn demo(tx: &mut Transaction) -> erp_fields::Result<()> {
//! let event = ApiKeyRevoked::new(
//!     ApiKeyId::new(91).map_err(erp_fields::Error::from)?,
//!     UserId::new(7).map_err(erp_fields::Error::from)?,
//!     KeyKind::User,
//!     KeyRouteScope::Rpc,
//!     RevokeCause::Owner,
//! );
//! tx.audit_auth(AuthEvent::ApiKeyRevoked(event))?;
//! # Ok(()) }
//! ```
//!
//! There is no free-text or generic JSON event API: a new kind of event is added to
//! [`AuthEvent`] by the slice that owns it, with its own validated payload.
//!
//! # Contract for elevation
//!
//! Opening an elevation records an observation through the crate-private hook
//! `Transaction::record_elevation` before the elevated body runs; an error aborts the block. The
//! hook only buffers (no network, no second connection), deduplicates by binding and keeps the
//! evidence even when a savepoint rolls back.

mod buffer;
mod elevation;
mod event;
mod persist;
mod schema;
mod store;

pub use buffer::AttemptEvidence;
pub(crate) use buffer::AuditBuffer;
pub use elevation::{ElevationReason, SuperuserAction};
pub use event::{
    ApiKeyIssued, ApiKeyRevoked, ApiKeysCollected, AuthEvent, IdentityLink, KeyRouteScope,
    LinkedBy, PasswordChanged, RevokeCause, SecretRotated, SessionCause, SessionRevoked,
    UserSessionsRevoked,
};
pub use persist::AuditAttempt;
pub(crate) use schema::AUDIT_TABLE;
pub use store::AuditStoreState;
pub(crate) use store::StoreCell;

use std::fmt;
use std::sync::Arc;

use erp_fields::{Error, ErrorKind, Result};

use crate::rpc::Effect;

/// Days ahead of today the audit table keeps partitions for. Every runtime grant
/// publication and `erp audit maintain` refresh the window; the number is a safety margin until
/// keeps it daily.
pub const AUDIT_PREMAKE_DAYS: u32 = 31;

/// Largest encoded row: a conservative estimate of the bytes of the parameters sent (text and
/// array lengths exactly, fixed-width columns rounded up).
pub const MAX_ROW_BYTES: usize = 1024 * 1024;
/// Most entries in the `events` array of one row.
pub const MAX_EVENTS: usize = 512;
/// Most distinct elevation entries in one transaction attempt.
pub const MAX_ELEVATION_ENTRIES: usize = 128;
/// Most companies in one row.
pub const MAX_COMPANIES: usize = 256;
/// Most API key ids in one `ApiKeysCollected` event (the expired-key collector's batch size).
pub const MAX_COLLECTED_KEYS: usize = 1_000;
/// Deepest nesting of a structured value in a row.
pub const MAX_DEPTH: usize = 16;
/// Longest model or method name (bytes).
pub const MAX_NAME_BYTES: usize = 256;

/// The `schema_version` column of every row this build writes.
pub(crate) const SCHEMA_VERSION: i16 = 1;

/// Why an audit operation was refused. Reachable as the source of the [`Error`] returned
/// (`error.source()` and `downcast_ref`), named in its message as `(<code>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditError {
    /// The audit store is not ready ([`AuditStoreState`]); nothing was written or attempted.
    Unavailable(String),
    /// A lifecycle change or audited elevation in a read-only transaction.
    ReadOnlyTransaction,
    /// A second call summary in one transaction attempt.
    HeaderTwice,
    /// The buffer was frozen by the commit preamble; nothing more can be recorded.
    Frozen,
    /// A bound of the row was exceeded; nothing was truncated.
    CaptureLimit(String),
    /// A value the row cannot hold (empty or NUL names, non-positive ids).
    Invalid(String),
    /// The INSERT failed or did not write exactly one row. `cause` is the database error behind
    /// it, reachable as the next link of the source chain (`None` for a wrong row count).
    Insertion {
        /// What failed, named by SQLSTATE and never by row content.
        what: String,
        /// The error the database returned.
        cause: Option<InsertionCause>,
    },
    /// An elevation entry that PRINCIPAL or the base runner built wrongly.
    Elevation(String),
}

impl AuditError {
    /// The stable name of the problem, as it appears in the error message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "unavailable",
            Self::ReadOnlyTransaction => "read-only-transaction",
            Self::HeaderTwice => "header-twice",
            Self::Frozen => "frozen",
            Self::CaptureLimit(_) => "capture-limit",
            Self::Invalid(_) => "invalid",
            Self::Insertion { .. } => "insertion",
            Self::Elevation(_) => "elevation",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::Unavailable(_) | Self::Insertion { .. } => ErrorKind::Internal,
            Self::CaptureLimit(_) => ErrorKind::ResourceExhausted,
            Self::ReadOnlyTransaction | Self::HeaderTwice | Self::Frozen => ErrorKind::Programming,
            Self::Invalid(_) | Self::Elevation(_) => ErrorKind::Programming,
        }
    }
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a later release ({}): ", self.code())?;
        match self {
            Self::Unavailable(why) => write!(
                f,
                "the audit store is not ready ({why}); nothing was recorded, fix the store (run \
                 `erp audit maintain` or the runtime grant publication as the owner role)"
            ),
            Self::ReadOnlyTransaction => f.write_str(
                "lifecycle changes, audited superuser runs and call summaries need a write \
                 transaction",
            ),
            Self::HeaderTwice => {
                f.write_str("the call summary was already set for this transaction attempt")
            }
            Self::Frozen => f.write_str("the audit buffer is frozen for commit"),
            Self::CaptureLimit(what) => write!(f, "CaptureLimit: {what}"),
            Self::Invalid(what) | Self::Insertion { what, .. } | Self::Elevation(what) => {
                f.write_str(what)
            }
        }
    }
}

impl std::error::Error for AuditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Insertion {
                cause: Some(cause), ..
            } => Some(cause.0.as_ref()),
            _ => None,
        }
    }
}

/// The database error behind [`AuditError::Insertion`]. Two causes are equal only when they are the
/// same error value (an error is not comparable by content).
#[derive(Debug, Clone)]
pub struct InsertionCause(Arc<Error>);

impl PartialEq for InsertionCause {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for InsertionCause {}

impl From<AuditError> for Error {
    fn from(problem: AuditError) -> Self {
        Error::new(problem.kind(), problem.to_string()).with_source(problem)
    }
}

pub(crate) fn limit(what: impl Into<String>) -> AuditError {
    AuditError::CaptureLimit(what.into())
}

/// Where a row came from (`origin` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditOrigin {
    /// A JSON-2 call.
    Rpc,
    /// A server action run.
    ServerAction,
    /// A background job.
    Job,
    /// Authentication.
    Auth,
    /// Module lifecycle.
    Lifecycle,
    /// An operator command.
    Cli,
    /// Anything else in the core.
    Internal,
}

impl AuditOrigin {
    /// The `origin` column text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rpc => "rpc",
            Self::ServerAction => "server_action",
            Self::Job => "job",
            Self::Auth => "auth",
            Self::Lifecycle => "lifecycle",
            Self::Cli => "cli",
            Self::Internal => "internal",
        }
    }
}

/// How a call ended (`outcome` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditOutcome {
    /// Committed with its change.
    Committed,
    /// Refused by access control.
    Denied,
    /// Failed and rolled back.
    Failed,
    /// Answered from the idempotency store.
    Replayed,
    /// `COMMIT` may or may not have been applied.
    CommitUnknown,
    /// A read that policy selected for audit.
    ReadOk,
}

impl AuditOutcome {
    /// The `outcome` column text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Denied => "denied",
            Self::Failed => "failed",
            Self::Replayed => "replayed",
            Self::CommitUnknown => "commit_unknown",
            Self::ReadOk => "read_ok",
        }
    }
}

/// A bounded class of error (`error_class` column); never an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorClass {
    /// Access refused.
    Access,
    /// Input or constraint violation.
    Validation,
    /// A record is missing.
    Missing,
    /// Serialization failure, conflict or lock.
    Concurrency,
    /// A bounded resource.
    Limit,
    /// A dependency is unavailable.
    Unavailable,
    /// API misuse.
    Programming,
    /// Anything else.
    Internal,
}

impl ErrorClass {
    /// The `error_class` column text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Validation => "validation",
            Self::Missing => "missing",
            Self::Concurrency => "concurrency",
            Self::Limit => "limit",
            Self::Unavailable => "unavailable",
            Self::Programming => "programming",
            Self::Internal => "internal",
        }
    }

    /// The class of an error kind.
    pub fn of(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::Access | ErrorKind::AccessDenied | ErrorKind::SessionExpired => Self::Access,
            ErrorKind::UserError | ErrorKind::Validation | ErrorKind::Redirect => Self::Validation,
            ErrorKind::Missing => Self::Missing,
            ErrorKind::Lock | ErrorKind::Concurrency | ErrorKind::Conflict => Self::Concurrency,
            ErrorKind::ResourceExhausted => Self::Limit,
            ErrorKind::Programming => Self::Programming,
            ErrorKind::Internal | ErrorKind::Database => Self::Internal,
        }
    }
}

/// The ids a call targeted, as the `target_ids` column holds them: ascending runs of the caller's
/// order as `[first,last]`, single ids as numbers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TargetIds(Vec<(i64, i64)>);

impl TargetIds {
    /// No target.
    pub fn none() -> Self {
        Self(Vec::new())
    }

    /// Compress `ids` into runs (`last = previous + 1`), keeping the caller's order.
    ///
    /// # Errors
    ///
    /// `Programming` ([`AuditError::Invalid`]) for an id that is not positive.
    pub fn from_ids(ids: &[i64]) -> Result<Self> {
        let mut runs: Vec<(i64, i64)> = Vec::new();
        for &id in ids {
            if id <= 0 {
                return Err(
                    AuditError::Invalid(format!("target ids must be positive, got {id}")).into(),
                );
            }
            match runs.last_mut() {
                Some((_, last)) if last.checked_add(1) == Some(id) => *last = id,
                _ => runs.push((id, id)),
            }
        }
        Ok(Self(runs))
    }

    /// The runs `(first, last)`.
    pub fn runs(&self) -> &[(i64, i64)] {
        &self.0
    }

    pub(crate) fn to_json(&self) -> String {
        let mut out = String::from("[");
        for (i, (first, last)) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            if first == last {
                out.push_str(&first.to_string());
            } else {
                out.push_str(&format!("[{first},{last}]"));
            }
        }
        out.push(']');
        out
    }
}

/// The SHA-256 digest of the redacted canonical arguments of a call and the version of the rules
/// that produced it. Constructed by the RPC layer only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgsDigest {
    sha256: [u8; 32],
    redaction_version: u16,
}

impl ArgsDigest {
    /// A digest of the redacted projection (RPC layer).
    pub fn new(sha256: [u8; 32], redaction_version: u16) -> Self {
        Self {
            sha256,
            redaction_version,
        }
    }
}

/// The call summary of one transaction attempt (`model`, `method`, `effect`, targets, digest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditCall {
    pub(crate) model: Option<Arc<str>>,
    pub(crate) method: Arc<str>,
    pub(crate) effect: Effect,
    pub(crate) targets: TargetIds,
    pub(crate) args: Option<ArgsDigest>,
    /// The 1-based transaction attempt of the call (retries).
    pub(crate) attempt: u16,
}

impl AuditCall {
    /// A call of `method` on `model` (`None` for non-model origins) with its declared `effect`
    /// (recorded by its name, `Effect::as_str`).
    ///
    /// # Errors
    ///
    /// `Programming` ([`AuditError::Invalid`]) for an empty name, a name over
    /// [`MAX_NAME_BYTES`] or a NUL byte.
    pub fn new(
        model: Option<Arc<str>>,
        method: Arc<str>,
        effect: Effect,
        targets: TargetIds,
        args: Option<ArgsDigest>,
    ) -> Result<Self> {
        for (what, name) in model
            .iter()
            .map(|m| ("model", m))
            .chain(std::iter::once(("method", &method)))
        {
            if name.is_empty() || name.len() > MAX_NAME_BYTES || name.contains('\0') {
                return Err(AuditError::Invalid(format!(
                    "the {what} name must be 1 to {MAX_NAME_BYTES} bytes without NUL"
                ))
                .into());
            }
        }
        Ok(Self {
            model,
            method,
            effect,
            targets,
            args,
            attempt: 1,
        })
    }

    /// The 1-based transaction attempt number. A retried call keeps the evidence of its last
    /// attempt only: an attempt that is retried (serialization failure, deadlock, superseded ORM)
    /// leaves no row of its own, and the row written carries the number of the attempt that
    /// committed or finally failed.
    #[must_use]
    pub fn attempt(mut self, attempt: u16) -> Self {
        self.attempt = attempt.max(1);
        self
    }
}

#[cfg(test)]
mod tests;
