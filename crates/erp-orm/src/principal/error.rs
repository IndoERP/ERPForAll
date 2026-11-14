//! Errors of principal construction and validation.

use std::fmt;

use erp_fields::{Error, ErrorKind};

/// Which identifier an [`PrincipalError::InvalidId`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdKind {
    /// `res.users` (non-zero).
    User,
    /// `res.groups` (positive).
    Group,
    /// `res.company` (positive).
    Company,
    /// `res.users.apikeys` (positive).
    ApiKey,
    /// A link token (positive).
    Link,
}

impl IdKind {
    fn describe(self) -> (&'static str, &'static str) {
        match self {
            Self::User => ("user", "non-zero"),
            Self::Group => ("group", "positive"),
            Self::Company => ("company", "positive"),
            Self::ApiKey => ("API key", "positive"),
            Self::Link => ("link", "positive"),
        }
    }
}

/// A principal, scope or capability that cannot be built.
///
/// Converts into [`erp_fields::Error`] with the kind of [`PrincipalError::kind`] and the same
/// message, with this error as its source.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PrincipalError {
    /// An identifier out of its range.
    InvalidId {
        /// Which identifier.
        kind: IdKind,
        /// The refused value.
        value: i64,
    },
    /// A qualified XML id that does not parse.
    InvalidXmlId {
        /// The input (at most 64 characters of it).
        value: String,
        /// What is wrong, as a predicate ("has no module part").
        problem: &'static str,
    },
    UnauthorizedCompanies,
    /// A company is listed twice in the allowed companies or in the subject's companies.
    DuplicateCompany {
        /// The repeated company.
        company: i64,
    },
    /// [`crate::Principal::with_companies`] was given an empty list: ask for the subject's default
    /// order with [`crate::Principal::with_default_companies`].
    NoCompanies,
    /// A company is listed twice in a context's `allowed_company_ids` (the list of
    /// [`crate::Principal::with_companies`]): a `Validation` error with the text the RPC layer
    /// reports for the same mistake in a call's context.
    DuplicateContextCompany {
        /// The repeated company.
        company: i64,
    },
    /// The superuser id was given to a builder that makes a non-system principal.
    SuperuserAsUser,
    /// The superuser id was given to the caller entry (`PrincipalHint::caller`, A4,
    SuperuserAsCaller,
    /// A principal kind or feature that a later slice adds.
    Unsupported {
        /// What was asked for ("agent principals").
        feature: &'static str,
        /// The board task that adds it.
        task: &'static str,
    },
    /// An API-key scope that cannot be built.
    InvalidScope {
        /// What is wrong.
        problem: String,
    },
    /// An elevation scope that cannot be built.
    InvalidElevationScope {
        /// What is wrong.
        problem: String,
    },
    /// A capability name outside the allowed form.
    InvalidCapabilityName {
        /// The name (at most 64 characters of it).
        name: String,
        /// Module path of the `capability!` that declared it.
        declared_in: &'static str,
    },
    /// A [`crate::Capability`] value that is not a member of [`crate::CAPABILITIES`]: built by hand
    /// with the hidden `__declare`, or copied out of a `capability!` static.
    UnregisteredCapability {
        /// The name (at most 64 characters of it).
        name: String,
        /// Module path it claims to be declared in.
        declared_in: &'static str,
    },
    /// The process handed out every policy snapshot id (ids are never reused).
    SnapshotIdsExhausted,
    /// Two `capability!` declarations with one name.
    DuplicateCapability {
        /// The name.
        name: &'static str,
        /// Module path of the first declaration.
        first: &'static str,
        /// Module path of the second declaration.
        second: &'static str,
    },
}

impl PrincipalError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::UnauthorizedCompanies | Self::SuperuserAsCaller => ErrorKind::Access,
            Self::DuplicateContextCompany { .. } => ErrorKind::Validation,
            Self::SnapshotIdsExhausted => ErrorKind::ResourceExhausted,
            Self::InvalidId { .. }
            | Self::InvalidXmlId { .. }
            | Self::DuplicateCompany { .. }
            | Self::NoCompanies
            | Self::SuperuserAsUser
            | Self::Unsupported { .. }
            | Self::InvalidScope { .. }
            | Self::InvalidElevationScope { .. }
            | Self::InvalidCapabilityName { .. }
            | Self::UnregisteredCapability { .. }
            | Self::DuplicateCapability { .. } => ErrorKind::Programming,
        }
    }
}

impl fmt::Display for PrincipalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId { kind, value } => {
                let (what, range) = kind.describe();
                write!(f, "invalid {what} id {value}: ids are {range}")
            }
            Self::InvalidXmlId { value, problem } => {
                write!(f, "invalid XML id '{value}': it {problem}")
            }
            Self::UnauthorizedCompanies => {
                f.write_str("One of the selected companies does not exist or is not one of yours.")
            }
            Self::DuplicateCompany { company } => {
                write!(f, "company {company} is listed twice")
            }
            Self::NoCompanies => f.write_str(
                "with_companies needs at least one company: use with_default_companies for the \
                 subject's default order",
            ),
            Self::DuplicateContextCompany { company } => write!(
                f,
                "context key 'allowed_company_ids': company {company} is listed twice"
            ),
            Self::SuperuserAsUser => f.write_str(
                "user 1 is the superuser: use the System principal (PrincipalHint::system), \
                 not a user principal",
            ),
            Self::SuperuserAsCaller => f.write_str(
                "the superuser cannot be opened as a caller: core code opens the system principal \
                 itself",
            ),
            Self::Unsupported { feature, task } => {
                write!(f, "{feature} are not supported until {task}")
            }
            Self::InvalidScope { problem } => write!(f, "invalid API key scope: {problem}"),
            Self::SnapshotIdsExhausted => f.write_str(
                "policy snapshot ids are exhausted in this process (ids are never reused): \
                 restart the server",
            ),
            Self::InvalidElevationScope { problem } => {
                write!(f, "invalid elevation scope: {problem}")
            }
            Self::InvalidCapabilityName { name, declared_in } => write!(
                f,
                "invalid capability name '{name}' (declared in {declared_in}): use lowercase \
                 ASCII letters, digits and '_' in dot-separated parts, at most 64 bytes"
            ),
            Self::UnregisteredCapability { name, declared_in } => write!(
                f,
                "capability '{name}' (declared in {declared_in}) is not a registered capability: \
                 declare it with capability!, never build it by hand"
            ),
            Self::DuplicateCapability {
                name,
                first,
                second,
            } => write!(
                f,
                "capability '{name}' is declared twice (in {first} and in {second})"
            ),
        }
    }
}

impl std::error::Error for PrincipalError {}

impl From<PrincipalError> for Error {
    fn from(error: PrincipalError) -> Self {
        Error::new(error.kind(), error.to_string()).with_source(error)
    }
}
