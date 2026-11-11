//! The error of every store call.
//!
//! Messages never carry a credential, a signed URL or a path outside the store root; the OS or
//! driver error that caused a failure is the [`std::error::Error::source`].

use std::error::Error as StdError;

/// Which limit a [`BlobError::LimitExceeded`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitKind {
    /// The size of one object.
    ObjectBytes,
    /// A name, key or identifier length.
    NameBytes,
}

impl LimitKind {
    /// The text of the limit in messages.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ObjectBytes => "object size in bytes",
            Self::NameBytes => "name length in bytes",
        }
    }
}

/// A store call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlobError {
    /// The key is not in the store. Never turned into empty content.
    #[error("the store has no object for key {key}")]
    NotFound {
        /// The content or staging key, relative to the store root.
        key: String,
    },
    /// The bytes under a key do not have the expected digest or size.
    #[error("object {key} failed its integrity check: expected {expected}, found {actual}")]
    Integrity {
        /// The key.
        key: String,
        /// What was expected (a lowercase hex SHA-256, or a size).
        expected: String,
        /// What was found.
        actual: String,
    },
    /// The store cannot serve the call now.
    #[error("the attachment store is unavailable ({}): {message}", if *.retryable { "retryable" } else { "not retryable" })]
    Unavailable {
        /// Whether the same call may succeed later.
        retryable: bool,
        /// What failed, without paths outside the store root.
        message: String,
        /// The cause.
        #[source]
        source: Option<Box<dyn StdError + Send + Sync>>,
    },
    /// The store refused the process.
    #[error("the attachment store refused access: {message}")]
    PermissionDenied {
        /// What was refused.
        message: String,
        /// The cause.
        #[source]
        source: Option<Box<dyn StdError + Send + Sync>>,
    },
    /// A size or length limit was passed.
    #[error("{} exceeds the limit of {limit}", .kind.as_str())]
    LimitExceeded {
        /// Which limit.
        kind: LimitKind,
        /// Its value.
        limit: u64,
    },
    /// A range does not fit the object.
    #[error("the byte range is outside the object of {total} bytes")]
    InvalidRange {
        /// The object's size.
        total: u64,
    },
    /// A row names a namespace this database does not own.
    #[error("namespace {namespace} is not owned by this database")]
    ForeignNamespace {
        /// The namespace as text.
        namespace: String,
    },
    /// Every streaming permit of this node is held (`attachments.stream_permits_per_node`, a
    /// per-node bound): retry shortly, or on another node.
    #[error(
        "every attachment stream of this node is in use (attachments.stream_permits_per_node = {permits}, per node): retry shortly"
    )]
    Busy {
        /// The configured per-node bound.
        permits: u32,
    },
    /// The store configuration is wrong, or names a store kind that is not built yet.
    #[error("attachment store configuration: {message}")]
    Configuration {
        /// What is wrong, naming the setting or the task that adds the feature.
        message: String,
    },
}

impl BlobError {
    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::Configuration {
            message: message.into(),
        }
    }

    /// Map an OS error of `what` (a short operation name) on the store at `key`.
    pub(crate) fn from_io(what: &str, key: &str, error: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match error.kind() {
            ErrorKind::NotFound => Self::NotFound {
                key: key.to_owned(),
            },
            ErrorKind::PermissionDenied => Self::PermissionDenied {
                message: format!("{what} on {key}"),
                source: Some(Box::new(error)),
            },
            _ => Self::Unavailable {
                retryable: true,
                message: format!("{what} on {key} failed"),
                source: Some(Box::new(error)),
            },
        }
    }

    /// True when the same call may succeed later.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Unavailable {
                retryable: true,
                ..
            } | Self::Busy { .. }
        )
    }
}
