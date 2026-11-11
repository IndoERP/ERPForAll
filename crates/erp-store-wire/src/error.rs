//! The error type of the store wire contract.
//!
//! Every refusal names a stable machine code ([`WireError::code`]) and, where the input has a
//! position, a path into the payload ([`WireError::path`]). The code and the path are what the
//! TypeScript decoder must reproduce; the message is for people.

use thiserror::Error;

/// A refusal by the encoder, the decoder, a validated type constructor or the host's limit setter.
///
/// Paths use the payload's own names: `records.<model>[<index>].<field>` while decoding,
/// `records.<model>[id=<id>].<field>` while encoding, `asof.xip[<index>]` for descriptors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum WireError {
    /// A value does not have the shape its schema kind needs.
    #[error("{path}: expected {expected}")]
    TypeMismatch {
        /// Where the value is.
        path: String,
        /// The schema kind or shape that was needed.
        expected: &'static str,
    },
    /// A value has the right shape but is outside its range (non-finite float, unsafe integer,
    /// counter above its cap, impossible calendar date).
    #[error("{path}: {reason}")]
    ValueOutOfRange {
        /// Where the value is.
        path: String,
        /// What is wrong with it.
        reason: String,
    },
    /// An id appears twice in one list or one model's record list.
    #[error("{path}: duplicate id {id}")]
    DuplicateId {
        /// Where the list is.
        path: String,
        /// The repeated id.
        id: u64,
    },
    /// A field is set twice on one record, or listed twice in a `written` marker.
    #[error("{path}: duplicate field")]
    DuplicateField {
        /// Where the second occurrence is.
        path: String,
    },
    /// An x2many operation lists the same id in `add` and `remove`.
    #[error("{path}: id {id} is in both add and remove")]
    AddRemoveOverlap {
        /// Where the operation is.
        path: String,
        /// The id in both lists.
        id: u64,
    },
    /// An x2many operation changes nothing.
    #[error("{path}: empty operation")]
    EmptyOperation {
        /// Where the operation is.
        path: String,
    },
    /// An operation is known but not allowed here (wrong kind, wrong payload type, answer-only
    /// field in a frame).
    #[error("{path}: operation not allowed: {reason}")]
    OperationNotAllowed {
        /// Where the operation is.
        path: String,
        /// Why it is refused.
        reason: String,
    },
    /// A counter increment of zero.
    #[error("{path}: zero counter delta")]
    ZeroDelta {
        /// Where the operation is.
        path: String,
    },
    /// A model name that the schema does not declare.
    #[error("{path}: unknown model {model}")]
    UnknownModel {
        /// Where the name is.
        path: String,
        /// The unknown name.
        model: String,
    },
    /// A field name that the model's schema does not declare.
    #[error("{path}: unknown field {field}")]
    UnknownField {
        /// Where the name is.
        path: String,
        /// The unknown name.
        field: String,
    },
    /// An operation key other than `add`, `remove` and `inc`.
    #[error("{path}: unknown operation {operation}")]
    UnknownOperation {
        /// Where the operation object is.
        path: String,
        /// The unknown key.
        operation: String,
    },
    /// A record id outside 1..2^53-1.
    #[error("{path}: bad record id {value}")]
    BadId {
        /// Where the id is.
        path: String,
        /// The offending value as written.
        value: String,
    },
    /// A record is marked deleted and also given fields.
    #[error("{path}: delete marker together with fields")]
    DeleteWithFields {
        /// Where the entry is.
        path: String,
    },
    /// A `written` marker that does not describe the payload.
    #[error("{path}: bad written marker: {reason}")]
    BadWritten {
        /// Where the marker is.
        path: String,
        /// Why it is refused.
        reason: String,
    },
    /// The payload is not well formed (missing or mistyped member, unknown member, bad JSON).
    #[error("{path}: malformed payload: {reason}")]
    Malformed {
        /// Where the problem is; empty for the whole document.
        path: String,
        /// What is wrong.
        reason: String,
    },
    /// The payload names a wire major this build does not implement.
    #[error("unsupported wire major {found}")]
    WireMajorUnsupported {
        /// The major found in the payload.
        found: u64,
    },
    /// A client announced a wire major the server does not speak.
    #[error("client wire major {client} is not supported (server speaks {supported})")]
    ClientOutdated {
        /// The client's major.
        client: u64,
        /// The only major the server speaks.
        supported: u32,
    },
    /// A snapshot descriptor, xid or cursor that breaks its invariants.
    #[error("{path}: bad descriptor: {reason}")]
    BadDescriptor {
        /// Where the problem is.
        path: String,
        /// Which invariant failed.
        reason: String,
    },
    /// A schema that breaks its rules.
    #[error("{path}: bad schema: {reason}")]
    BadSchema {
        /// Where the problem is.
        path: String,
        /// Which rule failed.
        reason: String,
    },
    /// A model, field or generation name outside the allowed characters or length.
    #[error("{path}: bad name: {reason}")]
    BadName {
        /// Where the name is.
        path: String,
        /// What is wrong with it.
        reason: String,
    },
    /// An input exceeds a named resource limit.
    #[error("{path}: limit {limit} exceeded (maximum {maximum})")]
    LimitExceeded {
        /// The limit's name, see [`Limit::name`](crate::Limit::name).
        limit: &'static str,
        /// Where the input is; empty for the whole document.
        path: String,
        /// The maximum in force.
        maximum: u64,
    },
    /// The host asked for a limit that is zero or above the compiled-in maximum.
    #[error("limit {limit}: {reason}")]
    BadLimit {
        /// The limit's name.
        limit: &'static str,
        /// Why the value is refused.
        reason: String,
    },
}

impl WireError {
    /// The stable machine code of this refusal (`snake_case`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TypeMismatch { .. } => "type_mismatch",
            Self::ValueOutOfRange { .. } => "value_out_of_range",
            Self::DuplicateId { .. } => "duplicate_id",
            Self::DuplicateField { .. } => "duplicate_field",
            Self::AddRemoveOverlap { .. } => "add_remove_overlap",
            Self::EmptyOperation { .. } => "empty_operation",
            Self::OperationNotAllowed { .. } => "operation_not_allowed",
            Self::ZeroDelta { .. } => "zero_delta",
            Self::UnknownModel { .. } => "unknown_model",
            Self::UnknownField { .. } => "unknown_field",
            Self::UnknownOperation { .. } => "unknown_operation",
            Self::BadId { .. } => "bad_id",
            Self::DeleteWithFields { .. } => "delete_with_fields",
            Self::BadWritten { .. } => "bad_written",
            Self::Malformed { .. } => "malformed",
            Self::WireMajorUnsupported { .. } => "wire_major_unsupported",
            Self::ClientOutdated { .. } => "client_outdated",
            Self::BadDescriptor { .. } => "bad_descriptor",
            Self::BadSchema { .. } => "bad_schema",
            Self::BadName { .. } => "bad_name",
            Self::LimitExceeded { .. } => "limit_exceeded",
            Self::BadLimit { .. } => "bad_limit",
        }
    }

    /// The position of the problem in the payload, when it has one. An empty path means the
    /// whole document and is reported as `Some("")`; errors with no document position
    /// (`client_outdated`, `wire_major_unsupported`, `bad_limit`) return `None`.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::TypeMismatch { path, .. }
            | Self::ValueOutOfRange { path, .. }
            | Self::DuplicateId { path, .. }
            | Self::DuplicateField { path }
            | Self::AddRemoveOverlap { path, .. }
            | Self::EmptyOperation { path }
            | Self::OperationNotAllowed { path, .. }
            | Self::ZeroDelta { path }
            | Self::UnknownModel { path, .. }
            | Self::UnknownField { path, .. }
            | Self::UnknownOperation { path, .. }
            | Self::BadId { path, .. }
            | Self::DeleteWithFields { path }
            | Self::BadWritten { path, .. }
            | Self::Malformed { path, .. }
            | Self::BadDescriptor { path, .. }
            | Self::BadSchema { path, .. }
            | Self::BadName { path, .. }
            | Self::LimitExceeded { path, .. } => Some(path),
            Self::WireMajorUnsupported { .. }
            | Self::ClientOutdated { .. }
            | Self::BadLimit { .. } => None,
        }
    }
}
