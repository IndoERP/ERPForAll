//! Errors of the query AST: one struct carrying the JSON path of the offending element and a
//! reason with one variant per class of problem.

use std::fmt;

use erp_fields::json::JsonError;
use thiserror::Error;

use crate::DomainError;

/// The longest piece of a document echoed back in an error message, in characters.
pub(crate) const SHOWN_CHARS: usize = 64;

/// `text` cut to [`SHOWN_CHARS`] characters (a longer text ends with `…`), so an error never
/// repeats a whole oversized document.
pub(crate) fn shown(text: &str) -> String {
    let mut out: String = text.chars().take(SHOWN_CHARS).collect();
    if text.chars().nth(SHOWN_CHARS).is_some() {
        out.push('…');
    }
    out
}

/// A position in a document, written like a JSONPath: `$`, `$.domain`, `$.groupby[1].granularity`.
///
/// ```
/// use erp_domain::query::JsonPath;
///
/// let path = JsonPath::root().key("groupby").index(1).key("granularity");
/// assert_eq!(path.to_string(), "$.groupby[1].granularity");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JsonPath(String);

impl JsonPath {
    /// The document itself: `$`.
    pub fn root() -> Self {
        Self("$".to_owned())
    }

    /// The member `name` of the object at this path.
    #[must_use]
    pub fn key(&self, name: &str) -> Self {
        Self(format!("{}.{name}", self.0))
    }

    /// The element `index` of the array at this path.
    #[must_use]
    pub fn index(&self, index: usize) -> Self {
        Self(format!("{}[{index}]", self.0))
    }

    /// The path as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A bounded quantity of a query document (see [`crate::query::QueryLimits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Bound {
    /// Bytes of the input document (and of the canonical form).
    Bytes,
    /// Nesting depth of the JSON text.
    JsonDepth,
    /// Conditions of the domain, those of `any` sub-domains included (operators are not counted).
    DomainItems,
    /// Nesting depth of the parsed domain.
    DomainDepth,
    /// Entries of `fields`.
    Fields,
    /// Entries of `groupby`.
    GroupBy,
    /// Entries of `aggregates`.
    Aggregates,
    /// Terms of `order`.
    OrderTerms,
    /// Conditions of `having`.
    Having,
    /// Segments of one group-by path.
    PathSegments,
    /// `limit` of a `search_read` query (rows).
    Rows,
    /// `limit` of a `read_group` query (groups).
    Groups,
    /// `offset`.
    Offset,
}

impl Bound {
    /// The name used in messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bytes => "input bytes",
            Self::JsonDepth => "JSON depth",
            Self::DomainItems => "domain items",
            Self::DomainDepth => "domain depth",
            Self::Fields => "fields",
            Self::GroupBy => "group-bys",
            Self::Aggregates => "aggregates",
            Self::OrderTerms => "order terms",
            Self::Having => "having conditions",
            Self::PathSegments => "path segments",
            Self::Rows => "limit",
            Self::Groups => "limit",
            Self::Offset => "offset",
        }
    }
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a query document or AST is refused. Every message is stored once, here.
#[derive(Debug, Clone, PartialEq, Error)]
#[non_exhaustive]
pub enum QueryAstReason {
    /// The text is not JSON of the strict form (duplicate keys, nesting, bad UTF-8, ...).
    #[error("not JSON: {0}")]
    Json(#[source] JsonError),
    /// A quantity is above its bound.
    #[error("{bound} is {found}; at most {max} allowed")]
    TooLarge {
        /// What is bounded.
        bound: Bound,
        /// The bound.
        max: u64,
        /// The value found, as written (it may not fit an integer type).
        found: String,
    },
    /// A quantity is below its minimum.
    #[error("{bound} must be at least {min}")]
    TooSmall {
        /// What is bounded.
        bound: Bound,
        /// The minimum.
        min: u64,
        /// The value found, as written.
        found: String,
    },
    /// `version` is an integer other than the supported one.
    #[error("unsupported version {found}; supported: 1")]
    UnsupportedVersion {
        /// The version found, as written.
        found: String,
    },
    /// A key written twice in one object (a tree handed to `from_json` or a builder; the parser
    /// refuses it in text with [`QueryAstReason::Json`]).
    #[error("duplicate key '{0}'")]
    DuplicateKey(String),
    /// A required key is absent.
    #[error("missing key '{0}'")]
    MissingKey(&'static str),
    /// A key outside the closed set of its object.
    #[error("unknown key '{0}'")]
    UnknownKey(String),
    /// A value of the wrong JSON type.
    #[error("expected {expected}, found {found}")]
    WrongType {
        /// The type wanted.
        expected: &'static str,
        /// The type found.
        found: &'static str,
    },
    /// A name that is not an identifier of its kind.
    #[error("invalid {what} '{value}': {why}")]
    InvalidName {
        /// The kind of name.
        what: &'static str,
        /// The text found (cut when long).
        value: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// A string outside an enumeration.
    #[error("unknown {what} '{value}'; expected one of {expected}")]
    UnknownValue {
        /// The enumeration.
        what: &'static str,
        /// The text found (cut when long).
        value: String,
        /// The accepted values.
        expected: &'static str,
    },
    /// The same element twice.
    #[error("duplicate {what} '{value}'")]
    Duplicate {
        /// The kind of element.
        what: &'static str,
        /// Its name.
        value: String,
    },
    /// A context key a query AST does not store.
    #[error("context key '{key}' is not stored in a query AST: {why}")]
    RefusedContextKey {
        /// The key (cut when long).
        key: String,
        /// Why it is refused.
        why: &'static str,
    },
    /// The domain parser refused the domain.
    #[error("{0}")]
    Domain(#[source] DomainError),
    /// An `order` or `having` entry names no declared group-by or aggregate.
    #[error("'{0}' names no group-by or aggregate of this query")]
    DanglingReference(String),
    /// A feature a query AST does not carry.
    #[error("{what} is not supported in a query AST: {why}")]
    RefusedFeature {
        /// The feature.
        what: String,
        /// Why, naming the task that adds it when there is one.
        why: &'static str,
    },
    /// A `read_group` query with neither group-bys nor aggregates.
    #[error("a read_group query needs groupby or aggregates")]
    NothingToGroup,
    /// A host asked for a bound above its default, or zero.
    #[error("{bound} cannot be raised above {default} or set to 0 (got {found})")]
    LimitNotLowerable {
        /// The bound.
        bound: Bound,
        /// Its default, which is also its ceiling.
        default: u64,
        /// The value asked for.
        found: u64,
    },
}

/// A refused query document or AST: where, and why.
///
/// Converts into an [`erp_fields::Error`] of kind `Validation`, the inner reason as its source.
///
/// ```
/// use erp_domain::query::{QueryAst, QueryLimits};
///
/// let err = QueryAst::parse(br#"{"version": 2}"#, &QueryLimits::default()).unwrap_err();
/// assert_eq!(err.to_string(), "query AST $.version: unsupported version 2; supported: 1");
/// assert_eq!(err.at().as_str(), "$.version");
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct QueryAstError {
    at: JsonPath,
    reason: QueryAstReason,
}

impl QueryAstError {
    pub(crate) fn new(at: &JsonPath, reason: QueryAstReason) -> Self {
        Self {
            at: at.clone(),
            reason,
        }
    }

    /// Where in the document.
    pub fn at(&self) -> &JsonPath {
        &self.at
    }

    /// What is wrong.
    pub fn reason(&self) -> &QueryAstReason {
        &self.reason
    }
}

impl fmt::Display for QueryAstError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "query AST {}: {}", self.at, self.reason)
    }
}

/// `source()` is the inner [`JsonError`] or [`DomainError`] when the reason wraps one.
impl std::error::Error for QueryAstError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.reason {
            QueryAstReason::Json(inner) => Some(inner),
            QueryAstReason::Domain(inner) => Some(inner),
            _ => None,
        }
    }
}

/// `?` in ORM and addon code; the kind is `Validation` (HTTP 422), `Programming` for a limit
/// a host asked for above its ceiling.
impl From<QueryAstError> for erp_fields::Error {
    fn from(error: QueryAstError) -> Self {
        // a host asking for a limit above the ceiling is a mistake of the calling code, not of a
        // document
        let message = error.to_string();
        match error.reason {
            QueryAstReason::LimitNotLowerable { .. } => erp_fields::Error::programming(message),
            _ => erp_fields::Error::validation(message),
        }
        .with_source(error)
    }
}
