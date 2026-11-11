//! Errors raised while building or optimizing a domain.
//!
//! # Error kind and provenance
//!
//! The kind of an error depends on where the domain came from, not on what is wrong
//! with it (orchestrator decision of 2026-10-15, extending to domains):
//!
//! - **External input** — anything entering through [`crate::Domain::from_json`],
//!   [`crate::Domain::from_value`], [`crate::Domain::from_list`] (and `from_list_internal`) or
//!   [`crate::Value::from_json`]: JSON-2 bodies, data files, Luau. Errors are
//!   [`ErrorKind::Validation`] (HTTP 422). A domain parsed from external input stays tagged, so
//!   [`crate::Domain::optimize`] also reports `Validation` for it (an unknown field sent by a
//!   client is the client's error).
//! - **Rust code** — the typed builder ([`crate::Domain::field`], [`crate::Domain::condition`],
//!   `&`, `|`, `!`), [`crate::Domain::to_json`], [`crate::parse_date`]. Errors are
//!   [`ErrorKind::Programming`] (HTTP 500, never shown as a user message).

use std::fmt;
use std::sync::Arc;

use thiserror::Error;

/// The error kind of a [`DomainError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Validation,
    /// The domain was built by Rust code: API misuse, HTTP 500.
    Programming,
}

/// An invalid domain, condition or domain value: what is wrong ([`ErrorReason`]) and whose fault
/// it is ([`ErrorKind`], see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainError {
    kind: ErrorKind,
    // Keep the diagnostic payload out of enclosing error enums' inline representation.
    reason: Box<ErrorReason>,
    context: Option<Arc<str>>,
}

impl fmt::Display for DomainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(context) = &self.context {
            write!(formatter, "{context}: ")?;
        }
        fmt::Display::fmt(&self.reason, formatter)
    }
}

impl std::error::Error for DomainError {}

impl DomainError {
    /// Construct an error, using `kind` unless an internal optimization reason carries the
    /// failing node's more specific provenance. The exposed reason remains the underlying cause.
    pub fn new(mut kind: ErrorKind, mut reason: ErrorReason) -> Self {
        let mut context = None;
        while let ErrorReason::Provenance {
            kind: inner_kind,
            context: inner_context,
            reason: inner,
        } = reason
        {
            kind = inner_kind;
            context = inner_context;
            reason = *inner;
        }
        DomainError {
            kind,
            reason: Box::new(reason),
            context,
        }
    }

    /// Whether the input or the calling code is at fault.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// What is wrong.
    pub fn reason(&self) -> &ErrorReason {
        self.reason.as_ref()
    }

    /// The condition reason, when this error is about one condition.
    pub fn condition_reason(&self) -> Option<&ConditionError> {
        match self.reason.as_ref() {
            ErrorReason::Condition { reason, .. } => Some(reason),
            _ => None,
        }
    }

    pub fn python_class(&self) -> &'static str {
        self.reason.python_class()
    }

    pub(crate) fn programming(reason: ErrorReason) -> Self {
        Self::new(ErrorKind::Programming, reason)
    }

    pub(crate) fn validation(reason: ErrorReason) -> Self {
        Self::new(ErrorKind::Validation, reason)
    }
}

/// The ERPForAll error of the same kind and message, so `?` works in ORM and addon code.
impl From<DomainError> for erp_fields::Error {
    fn from(error: DomainError) -> Self {
        let message = error.to_string();
        match error.kind {
            ErrorKind::Validation => erp_fields::Error::validation(message),
            ErrorKind::Programming => erp_fields::Error::programming(message),
        }
        .with_source(error)
    }
}

/// What is wrong with a domain.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ErrorReason {
    /// Internal transport of the failing node's origin during recursive optimization.
    /// Removed by `DomainError::new`, so `DomainError::reason` keeps its original shape.
    #[doc(hidden)]
    #[error("{reason}")]
    Provenance {
        /// Classification belonging to the failing node.
        kind: ErrorKind,
        /// Optional framework provider context.
        context: Option<Arc<str>>,
        /// Unmodified underlying error reason.
        reason: Box<ErrorReason>,
    },

    #[error("Domain() malformed domain {0}")]
    Malformed(String),

    #[error("Domain() invalid item in domain: {0}")]
    InvalidItem(String),

    #[error("Domain() invalid argument type for domain: {0}")]
    InvalidArgumentType(String),

    #[error("Domain() invalid arguments: {0}")]
    InvalidArguments(String),

    #[error("{reason} in condition {condition}")]
    Condition {
        /// What is wrong with the condition.
        reason: ConditionError,
        /// The condition, rendered as a Python tuple `('field', 'op', value)`.
        condition: String,
    },

    #[error("unsupported value in domain: {0}")]
    UnsupportedValue(String),

    #[error("Invalid field expression {0}")]
    InvalidFieldExpression(String),

    #[error("Failed to cast {0} into a date")]
    NotADate(String),

    #[error("Failed to cast {0} into a datetime")]
    NotADatetime(String),

    /// A string that looks like an ISO date (starts with digits and a dash) is not valid ISO 8601
    /// (Python `fromisoformat`, `ValueError`).
    #[error("Invalid isoformat string: {0}")]
    InvalidIsoFormat(String),

    /// An ISO date or datetime has a component out of range (Python `ValueError`).
    #[error("{0}")]
    DateOutOfRange(&'static str),

    #[error("expecting only datetimes with no timezone: {0}")]
    TimezoneNotAllowed(String),

    #[error("Empty date value")]
    EmptyDateValue,

    #[error("Invalid term {term} in expression date: {value}")]
    InvalidDateTerm {
        /// The offending term, Python-quoted.
        term: String,
        /// The whole relative date string, Python-quoted.
        value: String,
    },

    /// Date arithmetic left the supported range (years 1–9999) (Python `OverflowError`).
    #[error("date value out of range")]
    DateOverflow,

    #[error("Domain.optimize: too many loops")]
    TooManyLoops,

    /// The whole optimization exceeded its step budget (ERPForAll guard against pathological
    /// input; reported as `RecursionError` like the per-node limit).
    #[error("Domain.optimize: step budget of {0} exceeded")]
    StepBudgetExceeded(usize),

    /// A domain or value nests deeper than [`crate::MAX_DEPTH`].
    #[error("Domain() nesting deeper than {0} levels")]
    TooDeep(usize),

    /// A list-syntax domain has more than [`crate::MAX_ITEMS`] items (ERPForAll guard that bounds
    /// parsing and optimization work; reported as `ValueError`).
    #[error("Domain() has more than {0} items")]
    TooManyItems(usize),

    /// A [`crate::UserPath`] that cannot be built (ERPForAll principal parameters).
    #[error("invalid user path: {0}")]
    InvalidUserPath(String),
}

impl ErrorReason {
    pub fn python_class(&self) -> &'static str {
        match self {
            Self::Provenance { reason, .. } => reason.python_class(),
            Self::InvalidArgumentType(_)
            | Self::InvalidArguments(_)
            | Self::UnsupportedValue(_) => "TypeError",
            Self::Condition {
                reason:
                    ConditionError::EmptyFieldName
                    | ConditionError::LikeWithSql
                    | ConditionError::PatternNotString
                    | ConditionError::RelationalInequalityWithString,
                ..
            } => "TypeError",
            Self::DateOverflow => "OverflowError",
            Self::TooManyLoops | Self::StepBudgetExceeded(_) | Self::TooDeep(_) => "RecursionError",
            _ => "ValueError",
        }
    }
}

/// The reason a condition is invalid.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConditionError {
    #[error("Empty field name")]
    EmptyFieldName,

    #[error("Invalid operator")]
    InvalidOperator,

    #[error("Invalid field {model}.{field}")]
    InvalidField {
        /// Technical name of the model.
        model: String,
        /// Name of the missing field.
        field: String,
    },

    #[error("Cannot use 'any' with non-relational fields")]
    AnyOnNonRelational,

    #[error("Cannot determine the comodel relation")]
    UnknownComodel,

    #[error("Use Domain.custom instead of SQL")]
    LikeWithSql,

    #[error("The pattern to match must be a string")]
    PatternNotString,

    #[error("Inequality not supported for relational field using a string")]
    RelationalInequalityWithString,

    #[error("Cannot compare {field} to {value_type} which is not a collection of length 1")]
    BooleanNotCollection {
        /// The field expression, Python-quoted.
        field: String,
        /// The Python type of the value, e.g. `<class 'bool'>`.
        value_type: String,
    },

    /// A principal parameter where the condition needs a literal value (ERPForAll,:
    /// access rules compile principal values to parameters, so a rewrite that would need the value
    /// refuses the condition instead of guessing).
    #[error("The principal parameter {param} cannot be used here: {needs}")]
    ParamNotAllowed {
        /// The parameter, as written in a rule (`user.id`).
        param: String,
        /// What the condition needs instead.
        needs: &'static str,
    },
}
