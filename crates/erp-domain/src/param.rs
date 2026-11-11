//! Principal parameters (§3.1): leaves
//! of a compiled access domain that stand for a value of the principal a statement runs for.
//!
//! A compiled access domain never holds a principal's values (the uid, its companies, the current
//! time): those are [`Value::Param`](crate::Value::Param) leaves, so one compiled predicate (and
//! one memoized SQL text) serves every principal with the same groups and shape. `erp-sql` lowers
//! them to statement parameters and sub-selects; `erp-orm` binds them per statement from the
//! principal binding. Only Rust code builds them: no external input format (JSON, list syntax from
//! clients, data files) can produce a parameter.
//!
//! # Semantics
//!
//! | Parameter | Stands for | Shape |
//! |---|---|---|
//! | [`PrincipalParam::Uid`] | the subject's user id (`user.id`) | scalar, never `False` |
//! | [`PrincipalParam::CompanyId`] | the current company (`company_id`) | scalar, never `False` |
//! | [`PrincipalParam::CompanyIds`] | the allowed companies (`company_ids`) | collection |
//! | [`PrincipalParam::Now`] | the transaction's start time, as a date or a timestamp | scalar |
//! | [`PrincipalParam::UserPath`] | a field path from the subject's `res.users` row; its last part is a terminal: `id` (`user.partner_id.id`: one id, `False` when a many2one on the path is empty) or `ids` (`user.employee_id.ids`: the ids reached, a collection, empty when a link is); a path ending on a many2one is that id, on an x2many the ids | scalar or collection |
//! | [`PrincipalParam::GrantIds`] | the record ids a link grant gives for (model, operation) | collection |
//!
//! Rewrites that
//! need the values (set intersections, truthiness tests, `like` patterns) refuse or skip
//! parameters; the comparison is decided where the values are known, in SQL.

use std::fmt;
use std::sync::Arc;

use crate::error::{DomainError, ErrorReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Operation {
    /// Read.
    Read,
    /// Write.
    Write,
    /// Create.
    Create,
    /// Delete.
    Unlink,
}

impl Operation {
    pub const ALL: [Operation; 4] = [Self::Read, Self::Write, Self::Create, Self::Unlink];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Create => "create",
            Self::Unlink => "unlink",
        }
    }

    /// ```
    /// use erp_domain::Operation;
    /// assert_eq!(Operation::parse("write"), Some(Operation::Write));
    /// assert_eq!(Operation::parse("blabla"), None);
    /// ```
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.as_str() == name)
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How "now" compares (R-S13): typed by the field it is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NowKind {
    /// The UTC date of the transaction's start (`time.strftime('%Y-%m-%d')`,
    /// `fields.Date.today()`); compared with date fields.
    Date,
    /// The transaction's start, a naive UTC timestamp (`datetime.now()`); compared with datetime
    /// fields.
    Timestamp,
}

/// Most fields a [`UserPath`] may cross.
pub const MAX_USER_PATH_LEN: usize = 16;

/// Most bytes of one field name in a [`UserPath`].
pub const MAX_PATH_FIELD_LEN: usize = 63;

/// A path of field names starting at the subject's `res.users` row (`partner_id`,
/// `commercial_partner_id`): validated, non-empty, at most [`MAX_USER_PATH_LEN`] fields of the
/// identifier form `[a-z_][a-z0-9_]*` (at most [`MAX_PATH_FIELD_LEN`] bytes each). Whether a field
/// exists is checked against the registry when the parameter is lowered to SQL.
///
/// ```
/// use erp_domain::UserPath;
/// let path = UserPath::parse("partner_id.commercial_partner_id")?;
/// assert_eq!(
///     path.fields().iter().map(|f| &**f).collect::<Vec<_>>(),
///     ["partner_id", "commercial_partner_id"]
/// );
/// assert_eq!(path.to_string(), "partner_id.commercial_partner_id");
/// assert!(UserPath::parse("partner_id..id").is_err());
/// # Ok::<(), erp_domain::DomainError>(())
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct UserPath(Arc<[Box<str>]>);

impl UserPath {
    /// The path of `fields`, in order.
    ///
    /// # Errors
    ///
    /// A `Programming` [`DomainError`] ([`ErrorReason::InvalidUserPath`]) when the path is empty,
    /// longer than [`MAX_USER_PATH_LEN`] fields, or a field name is not an identifier.
    pub fn new<S: AsRef<str>>(fields: impl IntoIterator<Item = S>) -> Result<Self, DomainError> {
        let mut out: Vec<Box<str>> = Vec::new();
        for field in fields {
            let field = field.as_ref();
            if out.len() == MAX_USER_PATH_LEN {
                return Err(invalid_path(format!(
                    "it crosses more than {MAX_USER_PATH_LEN} fields"
                )));
            }
            if !is_identifier(field) {
                return Err(invalid_path(format!(
                    "{} is not a field name",
                    crate::value::py_str_repr(&clip_name(field))
                )));
            }
            out.push(Box::from(field));
        }
        if out.is_empty() {
            return Err(invalid_path("it is empty".to_owned()));
        }
        Ok(Self(out.into()))
    }

    /// The path written with dots (`partner_id.commercial_partner_id`).
    ///
    /// # Errors
    ///
    /// As [`UserPath::new`].
    pub fn parse(dotted: &str) -> Result<Self, DomainError> {
        Self::new(dotted.split('.'))
    }

    /// The field names, in order.
    pub fn fields(&self) -> &[Box<str>] {
        &self.0
    }
}

impl fmt::Display for UserPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, field) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(field)?;
        }
        Ok(())
    }
}

impl fmt::Debug for UserPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UserPath({self})")
    }
}

fn is_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_PATH_FIELD_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn clip_name(name: &str) -> String {
    name.chars().take(MAX_PATH_FIELD_LEN + 1).collect()
}

fn invalid_path(problem: String) -> DomainError {
    DomainError::programming(ErrorReason::InvalidUserPath(problem))
}

/// A leaf of a compiled access domain standing for a value of the principal (see the module
/// documentation for the semantics of each).
///
/// ```
/// use erp_domain::{Domain, Operator, PrincipalParam, Value};
/// let mine = Domain::condition("user_id", Operator::Eq, Value::Param(PrincipalParam::Uid))?;
/// assert_eq!(format!("{mine:?}"), "[('user_id', '=', user.id)]");
/// # Ok::<(), erp_domain::DomainError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrincipalParam {
    /// The subject's user id (`user.id`).
    Uid,
    /// The current company (`company_id`, the first allowed company).
    CompanyId,
    /// The allowed companies (`company_ids`).
    CompanyIds,
    /// The transaction's start time.
    Now(NowKind),
    /// A field path from the subject's user row (`user.partner_id.commercial_partner_id.id`).
    UserPath(UserPath),
    /// The record ids of `model` a link grant gives for `op` (binds them).
    GrantIds {
        /// The model the ids belong to.
        model: Arc<str>,
        /// The operation.
        op: Operation,
    },
}

impl PrincipalParam {
    /// Whether the parameter stands for several values (an `in` collection).
    ///
    /// `UserPath` answers `None`: whether it crosses an x2many field depends on the registry.
    pub fn is_collection(&self) -> Option<bool> {
        match self {
            Self::Uid | Self::CompanyId | Self::Now(_) => Some(false),
            Self::CompanyIds | Self::GrantIds { .. } => Some(true),
            Self::UserPath(_) => None,
        }
    }

    /// The parameter as written in an access rule (`user.id`, `company_ids`, …), for messages and
    /// explanations.
    pub fn symbol(&self) -> String {
        match self {
            Self::Uid => "user.id".to_owned(),
            Self::CompanyId => "company_id".to_owned(),
            Self::CompanyIds => "company_ids".to_owned(),
            Self::Now(NowKind::Date) => "context_today()".to_owned(),
            Self::Now(NowKind::Timestamp) => "datetime.datetime.now()".to_owned(),
            Self::UserPath(path) => format!("user.{path}"),
            Self::GrantIds { model, op } => format!(
                "grant_ids({}, {})",
                crate::value::py_str_repr(model),
                crate::value::py_str_repr(op.as_str())
            ),
        }
    }
}

impl fmt::Display for PrincipalParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.symbol())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_round_trip_by_name() {
        for op in Operation::ALL {
            assert_eq!(Operation::parse(op.as_str()), Some(op));
        }
        assert_eq!(Operation::parse("Read"), None, "exact match");
        assert_eq!(Operation::parse(""), None);
    }

    #[test]
    fn user_paths_are_validated() {
        let ok = UserPath::new(["partner_id", "commercial_partner_id", "id"]).unwrap();
        assert_eq!(ok.to_string(), "partner_id.commercial_partner_id.id");
        let message = |r: Result<UserPath, DomainError>| r.unwrap_err().to_string();
        assert_eq!(
            message(UserPath::new(Vec::<&str>::new())),
            "invalid user path: it is empty"
        );
        assert_eq!(
            message(UserPath::parse("partner_id..id")),
            "invalid user path: '' is not a field name"
        );
        assert_eq!(
            message(UserPath::parse("Partner_id")),
            "invalid user path: 'Partner_id' is not a field name"
        );
        assert_eq!(
            message(UserPath::parse("partner_id.naïve")),
            "invalid user path: 'naïve' is not a field name"
        );
        let deep = vec!["a"; MAX_USER_PATH_LEN + 1];
        assert_eq!(
            message(UserPath::new(deep)),
            "invalid user path: it crosses more than 16 fields"
        );
        let long = "a".repeat(MAX_PATH_FIELD_LEN + 1);
        let err = UserPath::new([long.as_str()]).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            format!("invalid user path: '{long}' is not a field name")
        );
        assert!(UserPath::new(vec!["a"; MAX_USER_PATH_LEN]).is_ok());
    }

    #[test]
    fn symbols_name_the_rule_variables() {
        let path = UserPath::parse("partner_id.id").unwrap();
        let cases = [
            (PrincipalParam::Uid, "user.id"),
            (PrincipalParam::CompanyId, "company_id"),
            (PrincipalParam::CompanyIds, "company_ids"),
            (PrincipalParam::Now(NowKind::Date), "context_today()"),
            (
                PrincipalParam::Now(NowKind::Timestamp),
                "datetime.datetime.now()",
            ),
            (PrincipalParam::UserPath(path), "user.partner_id.id"),
            (
                PrincipalParam::GrantIds {
                    model: Arc::from("sale.order"),
                    op: Operation::Read,
                },
                "grant_ids('sale.order', 'read')",
            ),
        ];
        for (param, symbol) in cases {
            assert_eq!(param.symbol(), symbol);
        }
        assert_eq!(PrincipalParam::CompanyIds.is_collection(), Some(true));
        assert_eq!(PrincipalParam::Uid.is_collection(), Some(false));
    }
}
