//! Errors of the job queue.
//!
//! Two families. The `Transaction` API (enqueue, admin actions) returns [`erp_fields::Error`]
//! with a specific [`ErrorKind`](erp_fields::ErrorKind) and a stable message, built by the
//! constructors at the end of this module so one failure mode has one text. A running job's
//! handler returns [`JobError`], which tells the runner what to do next (retry, dead-letter,
//! stop). Neither carries a payload or an SQL parameter value.

use std::fmt;
use std::time::Duration;

use erp_fields::{Error, ErrorKind};

use super::JobRef;

/// Longest error text stored on a job row (the column's `CHECK`).
pub const MAX_ERROR_BYTES: usize = 4096;
/// Longest error code stored on a job row.
pub const MAX_CODE_BYTES: usize = 64;

/// What a handler (or the runner on its behalf) reports when a run, or a chunk of it, ends in
/// anything but success.
///
/// A `Transient` failure is retried with backoff, a `Permanent` one dead-letters the job, `Yield`
/// gives the attempt back and reschedules, and the remaining variants stop the run without
/// counting against the job (`LeaseLost`, `Cancelled`) or end it (`PrincipalRevoked`,
/// `ItemFailuresExceeded`, `Payload`).
///
/// ```
/// use erp_orm::jobs::JobError;
/// let error = JobError::transient("smtp_busy", "the relay asked to retry later");
/// assert_eq!(error.to_string(), "transient job failure [smtp_busy]: the relay asked to retry later");
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum JobError {
    /// Worth retrying after a backoff.
    Transient {
        /// Stable short code stored on the row (at most 64 bytes).
        code: &'static str,
        /// What happened, without payload text (at most 4 KiB).
        message: Box<str>,
        /// The error that caused it, kept for logs.
        source: Option<Box<Error>>,
    },
    /// Retrying cannot help: the job becomes a dead letter.
    Permanent {
        /// Stable short code.
        code: &'static str,
        /// What happened.
        message: Box<str>,
        /// The error that caused it.
        source: Option<Box<Error>>,
    },
    /// A failure that belongs to the items of a chunk ([`JobContext::chunk_items`](super::JobContext::chunk_items)):
    /// the item is rolled back, recorded and counted while the others commit. Outside
    /// `chunk_items` it fails the job like [`JobError::Permanent`].
    Item {
        /// Stable short code stored with the item's failure row.
        code: &'static str,
        /// What happened to the item, without its data (at most 4 KiB).
        message: Box<str>,
        /// The error that caused it.
        source: Option<Box<Error>>,
    },
    /// Not a failure: run again after `after`, without consuming an attempt (a rate limit).
    Yield {
        /// Delay before the job is claimable again.
        after: Duration,
    },
    /// The lease or the epoch moved on: another worker owns the job, so nothing may be written.
    LeaseLost {
        /// The job.
        job: JobRef,
    },
    /// An operator cancelled the job; the run stops at the next fence.
    Cancelled {
        /// The job.
        job: JobRef,
    },
    /// The stored principal can no longer act (archived user, removed company access). The job
    /// dead-letters; nothing substitutes another principal.
    PrincipalRevoked {
        /// The job.
        job: JobRef,
        /// Why, from a fixed vocabulary.
        reason: &'static str,
    },
    /// More items failed than `jobs.max_item_failures_per_job` allows.
    ItemFailuresExceeded {
        /// The job.
        job: JobRef,
        /// The limit that was passed.
        limit: u32,
    },
    /// The payload or the progress of the job does not deserialize.
    Payload {
        /// The job.
        job: JobRef,
        /// What did not parse (never the payload text).
        message: Box<str>,
    },
}

impl JobError {
    /// A retryable failure.
    pub fn transient(code: &'static str, message: impl Into<String>) -> Self {
        Self::Transient {
            code,
            message: bounded(message.into()),
            source: None,
        }
    }

    /// A failure of one item of a chunk.
    pub fn item(code: &'static str, message: impl Into<String>) -> Self {
        Self::Item {
            code,
            message: bounded(message.into()),
            source: None,
        }
    }

    /// A failure retrying cannot fix.
    pub fn permanent(code: &'static str, message: impl Into<String>) -> Self {
        Self::Permanent {
            code,
            message: bounded(message.into()),
            source: None,
        }
    }

    /// The code and message the job row stores for this error (`None` for outcomes that are not
    /// failures: [`JobError::Yield`], [`JobError::LeaseLost`], [`JobError::Cancelled`]).
    pub fn stored(&self) -> Option<(&'static str, String)> {
        match self {
            Self::Transient { code, message, .. }
            | Self::Permanent { code, message, .. }
            | Self::Item { code, message, .. } => Some((code, message.to_string())),
            Self::PrincipalRevoked { reason, .. } => Some((
                "principal_revoked",
                format!("the stored principal can no longer act: {reason}"),
            )),
            Self::ItemFailuresExceeded { limit, .. } => Some((
                "item_failures_exceeded",
                format!("more than {limit} items failed (jobs.max_item_failures_per_job)"),
            )),
            Self::Payload { message, .. } => Some(("payload", message.to_string())),
            Self::Yield { .. } | Self::LeaseLost { .. } | Self::Cancelled { .. } => None,
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient { code, message, .. } => {
                write!(f, "transient job failure [{code}]: {message}")
            }
            Self::Permanent { code, message, .. } => {
                write!(f, "permanent job failure [{code}]: {message}")
            }
            Self::Item { code, message, .. } => {
                write!(f, "item failure [{code}]: {message}")
            }
            Self::Yield { after } => write!(f, "job yielded for {} ms", after.as_millis()),
            Self::LeaseLost { job } => write!(f, "the lease of job {} was lost", job.uuid()),
            Self::Cancelled { job } => write!(f, "job {} was cancelled", job.uuid()),
            Self::PrincipalRevoked { job, reason } => {
                write!(f, "job {} lost its principal: {reason}", job.uuid())
            }
            Self::ItemFailuresExceeded { job, limit } => write!(
                f,
                "job {} had more than {limit} failing items (jobs.max_item_failures_per_job)",
                job.uuid()
            ),
            Self::Payload { job, message } => {
                write!(f, "job {} has an unreadable payload: {message}", job.uuid())
            }
        }
    }
}

impl std::error::Error for JobError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transient { source, .. }
            | Self::Permanent { source, .. }
            | Self::Item { source, .. } => source
                .as_deref()
                .map(|e| e as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}

/// A database or ORM error inside a chunk: connection, lock, serialization and resource
/// failures are `Transient`; everything the data or the code decided (access, validation,
/// missing records, programming errors) is `Permanent`, because running it again gives the same
/// answer. A handler that knows better returns its own [`JobError`].
impl From<Error> for JobError {
    fn from(error: Error) -> Self {
        // a limit that fired is the load, not the data: retrying later may succeed
        let limit = match error.condition() {
            Some(erp_fields::Condition::LockTimeout) => Some("lock_timeout"),
            Some(erp_fields::Condition::StatementTimeout) => Some("statement_timeout"),
            Some(erp_fields::Condition::TransactionTimeout) => Some("transaction_timeout"),
            Some(erp_fields::Condition::OrmSuperseded) => Some("orm_superseded"),
            Some(erp_fields::Condition::ReadPoolUnavailable) => Some("read_pool_unavailable"),
            _ => None,
        };
        if let Some(code) = limit {
            let message = bounded(error.message().to_owned());
            return Self::Transient {
                code,
                message,
                source: Some(Box::new(error)),
            };
        }
        let (transient, code) = match error.kind() {
            ErrorKind::Concurrency => (true, "serialization"),
            ErrorKind::Lock => (true, "lock_timeout"),
            ErrorKind::ResourceExhausted => (true, "resource_exhausted"),
            ErrorKind::Database => (true, "database"),
            ErrorKind::Access | ErrorKind::AccessDenied | ErrorKind::SessionExpired => {
                (false, "access")
            }
            ErrorKind::Validation | ErrorKind::UserError | ErrorKind::Conflict => {
                (false, "validation")
            }
            ErrorKind::Missing => (false, "missing"),
            ErrorKind::Programming | ErrorKind::Internal | ErrorKind::Redirect => {
                (false, "internal")
            }
        };
        let message = bounded(error.message().to_owned());
        let source = Some(Box::new(error));
        if transient {
            Self::Transient {
                code,
                message,
                source,
            }
        } else {
            Self::Permanent {
                code,
                message,
                source,
            }
        }
    }
}

/// `text` cut to [`MAX_ERROR_BYTES`] at a character boundary.
pub(crate) fn bounded(text: String) -> Box<str> {
    if text.len() <= MAX_ERROR_BYTES {
        return text.into_boxed_str();
    }
    let mut end = MAX_ERROR_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}

/// `code` cut to [`MAX_CODE_BYTES`] at a character boundary (the column's `CHECK`).
pub(crate) fn bounded_code(code: &str) -> String {
    let mut end = code.len().min(MAX_CODE_BYTES);
    while !code.is_char_boundary(end) {
        end -= 1;
    }
    code[..end].to_owned()
}

// ----- enqueue and admin failures: one constructor per failure mode -----------------------------

pub(crate) fn unknown_kind(kind: &str) -> Error {
    Error::programming(format!(
        "job kind '{kind}' is not registered in this binary (register_job_kind!)"
    ))
}

pub(crate) fn payload_too_large(kind: &str, bytes: usize, max: usize) -> Error {
    Error::validation(format!(
        "the payload of a '{kind}' job is {bytes} bytes; the limit is {max} bytes \
         (jobs.payload_max_bytes)"
    ))
}

pub(crate) fn payload_unserializable(kind: &str) -> Error {
    Error::validation(format!(
        "the payload of a '{kind}' job could not be serialized to JSON"
    ))
}

pub(crate) fn invalid_key(reason: &str) -> Error {
    Error::validation(format!(
        "invalid job key: {reason} (a key is 1..=200 bytes without NUL or control characters)"
    ))
}

pub(crate) fn run_as_needs_audit() -> Error {
    Error::access(
        "running a job as another principal needs the 'jobs.enqueue_as' elevation, which is \
         recorded by the audit trail of a later release; that hook is not available yet, so the \
         enqueue is refused",
    )
}

pub(crate) fn unsupported_principal(kind: &str, task: &str) -> Error {
    Error::programming(format!(
        "a job cannot be enqueued by a {kind} principal until {task}"
    ))
}

pub(crate) fn public_principal(kind: &str) -> Error {
    Error::access(format!(
        "a {kind} principal cannot enqueue a job: work started by a public request runs under \
         the fixed principal of its kind"
    ))
}

pub(crate) fn invalid_priority(value: u8) -> Error {
    Error::validation(format!("job priority {value} is outside 0..=9"))
}

pub(crate) fn invalid_attempts(kind: &str, value: u32, max: u32) -> Error {
    Error::validation(format!(
        "max_attempts {value} of a '{kind}' job is outside 1..={max}"
    ))
}

pub(crate) fn batch_too_large(count: usize, max: usize) -> Error {
    Error::validation(format!(
        "enqueue_many takes at most {max} jobs per call (got {count}); split the batch"
    ))
}

pub(crate) fn needs_job_admin(call: &str) -> Error {
    Error::access(format!(
        "{call} needs the 'jobs.admin' capability (the system administrators group)"
    ))
}

pub(crate) fn job_state(call: &str, uuid: &str, found: &str, wanted: &str) -> Error {
    Error::user(format!("{call}: job {uuid} is {found}, not {wanted}"))
}

pub(crate) fn job_missing(call: &str, uuid: &str) -> Error {
    Error::missing(format!(
        "{call}: job {uuid} does not exist (it may have finished)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_messages_are_cut_at_a_character_boundary() {
        let text = "é".repeat(MAX_ERROR_BYTES);
        let cut = bounded(text);
        assert!(cut.len() <= MAX_ERROR_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn codes_are_cut_to_the_column_limit_at_a_character_boundary() {
        let cut = bounded_code(&"é".repeat(MAX_CODE_BYTES));
        assert!(cut.len() <= MAX_CODE_BYTES);
        assert_eq!(bounded_code("short"), "short");
    }

    #[test]
    fn database_errors_are_transient_and_data_errors_permanent() {
        let db = JobError::from(Error::new(ErrorKind::Concurrency, "deadlock"));
        assert!(matches!(
            db,
            JobError::Transient {
                code: "serialization",
                ..
            }
        ));
        let data = JobError::from(Error::validation("bad"));
        assert!(matches!(
            data,
            JobError::Permanent {
                code: "validation",
                ..
            }
        ));
    }
}
