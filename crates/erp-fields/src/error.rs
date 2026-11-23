//! The one error type shared by every ERPForAll surface.

use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

/// What went wrong, in the terms.
// An exhaustive `match` turns a newly added kind into a compile error at each of those
// places, so it cannot silently fall into a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// `erp.exceptions.UserError` (422).
    UserError,
    /// `erp.exceptions.ValidationError` (422): bad input values, constraint violations.
    Validation,
    /// `erp.exceptions.AccessError` (403).
    Access,
    /// `erp.exceptions.AccessDenied` (403): authentication.
    AccessDenied,
    /// `erp.exceptions.MissingError` (404): a record does not exist or was deleted.
    Missing,
    /// `erp.exceptions.LockError` (409).
    Lock,
    /// `erp.exceptions.RedirectWarning` (500).
    Redirect,
    /// Serialization failure or deadlock after retries (500).
    Concurrency,
    Programming,
    /// An internal invariant was broken (500); never the caller's or data's mistake.
    Internal,
    /// Any other database error; the SQLSTATE is kept in [`Error::sqlstate`].
    Database,
    /// A bounded server resource is used up (500), e.g. a database's intern quota or the
    /// process intern budget for runtime model declarations.
    ResourceExhausted,
    /// A record changed since the caller read it: the expected row version given with
    /// `expect_versions` is not the stored one. Unlike [`Self::Concurrency`]
    /// it is never retried by the unit of work: retrying cannot make a stale edit current, the
    /// caller must reload and apply the change again. JSON-2 answers it with status 409 and the
    /// error name `erp.conflict` (accepted's error policy, 2026-10-16).
    Conflict,
    /// `erp.http.session.SessionExpiredException` (403): the request's session is missing,
    /// expired, revoked, or no longer matches its user (JSON-RPC answers it
    /// with `error.code` 100, JSON-2 with 403, `http` routes with a redirect to the login page).
    SessionExpired,
}

impl ErrorKind {
    /// HTTP status used by JSON-2 for this kind.
    pub fn http_status(self) -> u16 {
        match self {
            Self::UserError | Self::Validation => 422,
            Self::Access | Self::AccessDenied | Self::SessionExpired => 403,
            Self::Missing => 404,
            Self::Lock | Self::Conflict => 409,
            Self::Redirect
            | Self::Concurrency
            | Self::Programming
            | Self::Internal
            | Self::Database
            | Self::ResourceExhausted => 500,
        }
    }

    /// Legacy source-compatible spelling; emitted names follow's ERP namespace.
    pub fn odoo_name(self) -> Option<&'static str> {
        self.erp_name()
    }

    /// ERP exception name reported in the native JSON-2 error body, when there is one.
    pub fn erp_name(self) -> Option<&'static str> {
        Some(match self {
            Self::UserError => "erp.exceptions.UserError",
            Self::Validation => "erp.exceptions.ValidationError",
            Self::Access => "erp.exceptions.AccessError",
            Self::AccessDenied => "erp.exceptions.AccessDenied",
            Self::Missing => "erp.exceptions.MissingError",
            Self::Lock => "erp.exceptions.LockError",
            Self::Redirect => "erp.exceptions.RedirectWarning",
            Self::SessionExpired => "erp.http.session.SessionExpiredException",
            Self::Concurrency
            | Self::Programming
            | Self::Internal
            | Self::Database
            | Self::ResourceExhausted
            | Self::Conflict => {
                return None;
            }
        })
    }
}

/// A condition an error carries besides its kind, set only by the code that detects it. It is never
/// derived from a SQLSTATE, so a database function that raises the same code (`RAISE ... USING
/// ERRCODE`) cannot produce it: match on it with [`Error::condition`] or the `is_*` helpers, never on
/// the message or the SQLSTATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Condition {
    /// The ORM the transaction ran on was replaced by a refresh after a registry change in another
    /// process: retry the request on the current ORM.
    OrmSuperseded,
    /// The cross-process signal generations read lower than the process had applied (the database
    /// was restored or signal rows were deleted): the process forces a full refresh.
    SignalRegression,
    /// A statement waited for a lock longer than the transaction's `lock_timeout` (`55P03`).
    LockTimeout,
    /// A statement ran longer than the transaction's `statement_timeout` (`57014`).
    StatementTimeout,
    /// The transaction outlived its `transaction_timeout` backstop (`25P04`): the server
    /// ended the session, so the connection was discarded.
    TransactionTimeout,
    /// The read pool could not give a connection: the read ran nowhere. Retry, or
    /// configure `ReadPoolFailure::Primary` to run such reads on the primary.
    ReadPoolUnavailable,
    /// A [`ErrorKind::Programming`] error that is a broken contract between layers (a load state
    /// the data loader did not write, a record layer's invariant), never the mistake of the data
    /// being written: the module system's demo savepoint does not absorb it as a demo failure
    /// (review F-1). Set only with [`Error::invariant`], on
    /// [`ErrorKind::Programming`]. Consumers check the marker before the kind so an incorrectly
    /// wrapped error still cannot be absorbed as a data failure.
    InvariantViolation,
    /// A bounded queue's wait expired (the hashing pool; kind
    /// [`ErrorKind::ResourceExhausted`]): the server answers 503 with `Retry-After` and the frozen
    /// overload body; a retry later may succeed.
    Overloaded,
}

#[derive(Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    /// SQLSTATE and constraint of the database error behind it (boxed: most errors have none,
    /// and every `Result` carries the error's size).
    db: Option<Box<DbDetails>>,
    condition: Option<Condition>,
    source: Option<Arc<dyn StdError + Send + Sync + 'static>>,
    context: Option<Box<crate::Value>>,
}

/// What a database error adds to an [`Error`].
#[derive(Clone, Debug, Default)]
struct DbDetails {
    sqlstate: Option<String>,
    /// The constraint or index a violation (`23xxx`) names, set by the ORM's error mapping.
    constraint: Option<String>,
    retry_lock_timeout: bool,
}

impl Error {
    /// A new error of `kind` with `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            db: None,
            condition: None,
            source: None,
            context: None,
        }
    }

    /// `ErrorKind::UserError`.
    pub fn user(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::UserError, message)
    }

    /// `ErrorKind::Validation`.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, message)
    }

    /// `ErrorKind::Access`.
    pub fn access(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Access, message)
    }

    /// `ErrorKind::Missing`.
    pub fn missing(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Missing, message)
    }

    /// `ErrorKind::Programming`.
    pub fn programming(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Programming, message)
    }

    /// `ErrorKind::Internal`: an internal invariant was broken.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// `ErrorKind::Programming` marked [`Condition::InvariantViolation`]: a contract between
    /// layers was broken, which is not the mistake of the data being written.
    ///
    /// ```
    /// use erp_fields::{Condition, Error, ErrorKind};
    /// let e = Error::invariant("the load state is not the loader's");
    /// assert_eq!(e.kind(), ErrorKind::Programming);
    /// assert!(e.is_invariant_violation());
    /// assert_eq!(e.condition(), Some(Condition::InvariantViolation));
    /// assert!(!Error::programming("x").is_invariant_violation());
    /// ```
    pub fn invariant(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Programming, message).with_condition(Condition::InvariantViolation)
    }

    /// `ErrorKind::Conflict`: a stale expected row version (never retried).
    ///
    /// ```
    /// use erp_fields::{Error, ErrorKind};
    /// let e = Error::conflict("changed by someone else");
    /// assert_eq!((e.kind(), e.kind().http_status()), (ErrorKind::Conflict, 409));
    /// ```
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    /// `ErrorKind::Lock`.
    pub fn lock(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Lock, message)
    }

    /// `ErrorKind::Database`.
    pub fn db(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Database, message)
    }

    /// A database error with its SQLSTATE.
    pub fn database(
        kind: ErrorKind,
        sqlstate: Option<String>,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            db: sqlstate.map(|sqlstate| {
                Box::new(DbDetails {
                    sqlstate: Some(sqlstate),
                    constraint: None,
                    retry_lock_timeout: false,
                })
            }),
            condition: None,
            source: Some(Arc::new(source)),
            context: None,
        }
    }

    /// Attach a source error.
    #[must_use]
    pub fn with_source(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.source = Some(Arc::new(source));
        self
    }

    /// The same error with a second error of the same operation named in its message:
    /// `"<message> (and <other's message>)"`. Kind, SQLSTATE, condition, context and source are
    /// kept, so a caller that inspects the source still finds the original cause.
    ///
    /// ```
    /// use std::error::Error as _;
    /// use erp_fields::{Error, ErrorKind};
    /// let cause = std::io::Error::other("connection reset");
    /// let e = Error::db("statement failed")
    ///     .with_source(cause)
    ///     .and_also(&Error::programming("a row was refused"));
    /// assert_eq!(e.kind(), ErrorKind::Database);
    /// assert_eq!(e.message(), "statement failed (and a row was refused)");
    /// assert!(e.source().unwrap().downcast_ref::<std::io::Error>().is_some());
    /// ```
    #[must_use]
    pub fn and_also(mut self, other: &Error) -> Self {
        self.message = format!("{} (and {})", self.message, other.message);
        self
    }

    /// Attach a SQLSTATE.
    #[must_use]
    pub fn with_sqlstate(mut self, sqlstate: Option<String>) -> Self {
        match (&mut self.db, sqlstate) {
            (Some(db), sqlstate) => db.sqlstate = sqlstate,
            (None, None) => {}
            (None, Some(sqlstate)) => {
                self.db = Some(Box::new(DbDetails {
                    sqlstate: Some(sqlstate),
                    constraint: None,
                    retry_lock_timeout: false,
                }));
            }
        }
        self
    }

    /// Opt a positively identified server lock timeout into a retry of the complete operation.
    /// Use this only at a statement whose contract requires a fresh transaction after a
    /// bounded lock wait. Ordinary lock refusals, including NOWAIT, keep their original policy.
    #[must_use]
    pub fn retry_whole_unit_on_lock_timeout(mut self) -> Self {
        if self.condition() == Some(Condition::LockTimeout)
            && self.sqlstate() == Some("55P03")
            && let Some(db) = self.db.as_mut()
        {
            db.retry_lock_timeout = true;
        }
        self
    }

    /// Whether the unit owner can retry this concurrency failure with a fresh transaction.
    /// A lock timeout joins serialization failures and deadlocks only for an opted-in statement.
    pub fn retries_transaction(&self) -> bool {
        (self.kind() == ErrorKind::Concurrency
            && matches!(self.sqlstate(), Some("40001" | "40P01")))
            || (self.condition() == Some(Condition::LockTimeout)
                && self.sqlstate() == Some("55P03")
                && self.db.as_ref().is_some_and(|db| db.retry_lock_timeout))
    }

    /// Mark the error with a [`Condition`] (only the code that detects the condition does this).
    /// This replaces the previous condition. Wrappers must preserve an invariant marker rather
    /// than attaching a different condition; construct invariant errors with [`Self::invariant`].
    #[must_use]
    pub fn with_condition(mut self, condition: Condition) -> Self {
        self.condition = Some(condition);
        self
    }

    /// The [`Condition`] the error carries, if any.
    pub fn condition(&self) -> Option<Condition> {
        self.condition
    }

    /// True when the transaction failed because its ORM was replaced ([`Condition::OrmSuperseded`]):
    /// the caller retries the request on the current ORM. A database error with the same SQLSTATE
    /// is not superseded.
    ///
    /// ```
    /// use erp_fields::{Condition, Error, ErrorKind};
    /// let raised = Error::new(ErrorKind::Concurrency, "x").with_sqlstate(Some("ERP01".to_owned()));
    /// assert!(!raised.is_superseded());
    /// assert!(raised.with_condition(Condition::OrmSuperseded).is_superseded());
    /// ```
    pub fn is_superseded(&self) -> bool {
        self.condition == Some(Condition::OrmSuperseded)
    }

    /// True when the signal generations went backwards ([`Condition::SignalRegression`]).
    pub fn is_signal_regression(&self) -> bool {
        self.condition == Some(Condition::SignalRegression)
    }

    /// True when the error is a broken contract between layers ([`Condition::InvariantViolation`],
    /// [`Error::invariant`]).
    pub fn is_invariant_violation(&self) -> bool {
        self.condition == Some(Condition::InvariantViolation)
    }

    /// True when an internal invariant was broken ([`ErrorKind::Internal`]).
    pub fn is_internal(&self) -> bool {
        self.kind == ErrorKind::Internal
    }

    /// True when the read pool was unreachable ([`Condition::ReadPoolUnavailable`]).
    pub fn is_read_pool_unavailable(&self) -> bool {
        self.condition == Some(Condition::ReadPoolUnavailable)
    }

    /// Attach a context dictionary for the client (the JSON-2 error body's `context`).
    #[must_use]
    pub fn with_context(mut self, context: crate::Value) -> Self {
        self.context = Some(Box::new(context));
        self
    }

    /// The context dictionary attached for the client, if any.
    pub fn context(&self) -> Option<&crate::Value> {
        self.context.as_deref()
    }

    /// Name the fields of the record this refusal is about, for a client that shows the message on
    /// those fields (the JSON-2 error body's `context.fields`). The names join any already given and
    /// any other context key is kept. A refusal about the record as a whole names none.
    #[must_use]
    pub fn with_fields<'a>(mut self, names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut context = match self.context.as_deref() {
            Some(crate::Value::Map(map)) => (**map).clone(),
            _ => crate::ValueMap::new(),
        };
        let mut fields: Vec<crate::Value> = match context.get(FIELDS_KEY) {
            Some(crate::Value::List(list)) => list.to_vec(),
            _ => Vec::new(),
        };
        for name in names {
            let value = crate::Value::str(name);
            if !fields.contains(&value) {
                fields.push(value);
            }
        }
        if fields.is_empty() {
            return self;
        }
        context.insert(FIELDS_KEY.to_owned(), crate::Value::List(fields.into()));
        self.context = Some(Box::new(crate::Value::Map(Arc::new(context))));
        self
    }

    /// The fields [`Error::with_fields`] named, in the order they were given.
    pub fn fields(&self) -> Vec<&str> {
        match self.context.as_deref() {
            Some(crate::Value::Map(map)) => match map.get(FIELDS_KEY) {
                Some(crate::Value::List(list)) => list
                    .iter()
                    .filter_map(|value| match value {
                        crate::Value::Str(name) => Some(name.as_ref()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// The kind.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// SQLSTATE of the database error behind this error, if any.
    pub fn sqlstate(&self) -> Option<&str> {
        self.db.as_ref().and_then(|db| db.sqlstate.as_deref())
    }

    /// Name the database constraint or index an integrity violation reports (the ORM's error
    /// mapping does this; callers match on [`Error::constraint`], never on the message).
    #[must_use]
    pub fn with_constraint(mut self, constraint: &str) -> Self {
        self.db.get_or_insert_default().constraint = Some(constraint.to_owned());
        self
    }

    /// The database constraint or index an integrity violation names (unique, check, exclusion
    /// and foreign-key violations mapped by the ORM), as PostgreSQL reports it.
    ///
    /// ```
    /// use erp_fields::Error;
    /// let e = Error::validation("Path to show in the URL must be unique!")
    ///     .with_constraint("ir_actions_path_unique");
    /// assert_eq!(e.constraint(), Some("ir_actions_path_unique"));
    /// assert_eq!(Error::validation("x").constraint(), None);
    /// ```
    pub fn constraint(&self) -> Option<&str> {
        self.db.as_ref().and_then(|db| db.constraint.as_deref())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Error");
        s.field("kind", &self.kind).field("message", &self.message);
        if let Some(state) = self.sqlstate() {
            s.field("sqlstate", &state);
        }
        if let Some(condition) = &self.condition {
            s.field("condition", condition);
        }
        if let Some(source) = &self.source {
            s.field("source", &source.to_string());
        }
        if let Some(context) = &self.context {
            s.field("context", &context.to_string());
        }
        if let Some(constraint) = self.constraint() {
            s.field("constraint", &constraint);
        }
        s.finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn StdError + 'static))
    }
}

/// The error context key naming the fields a refusal is about ([`Error::with_fields`]).
pub const FIELDS_KEY: &str = "fields";

/// Result alias using [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_json2_statuses() {
        assert_eq!(ErrorKind::Validation.http_status(), 422);
        assert_eq!(ErrorKind::Missing.http_status(), 404);
        assert_eq!(ErrorKind::Programming.http_status(), 500);
        assert_eq!(ErrorKind::Internal.http_status(), 500);
        assert_eq!(ErrorKind::Internal.odoo_name(), None);
        assert_eq!(
            ErrorKind::Validation.odoo_name(),
            Some("erp.exceptions.ValidationError")
        );
        assert_eq!(ErrorKind::Database.odoo_name(), None);
        assert_eq!(ErrorKind::ResourceExhausted.http_status(), 500);
        assert_eq!(ErrorKind::ResourceExhausted.odoo_name(), None);
        assert_eq!(ErrorKind::Conflict.http_status(), 409);
        assert_eq!(ErrorKind::Conflict.odoo_name(), None);
    }

    #[test]
    fn conditions_come_only_from_with_condition() {
        let raised = Error::new(ErrorKind::Concurrency, "raised by a function")
            .with_sqlstate(Some("ERP01".to_owned()));
        assert!(!raised.is_superseded());
        assert_eq!(raised.condition(), None);
        let superseded = raised.with_condition(Condition::OrmSuperseded);
        assert!(superseded.is_superseded());
        assert!(!superseded.is_signal_regression());
        let regression =
            Error::new(ErrorKind::Database, "r").with_condition(Condition::SignalRegression);
        assert!(regression.is_signal_regression());
        assert!(!regression.is_superseded());
        let unavailable =
            Error::new(ErrorKind::Database, "r").with_condition(Condition::ReadPoolUnavailable);
        assert!(unavailable.is_read_pool_unavailable());
        assert!(!unavailable.is_superseded());
        assert!(!regression.is_read_pool_unavailable());
    }

    #[test]
    fn bounded_timeout_requires_both_detection_and_statement_opt_in() {
        let unavailable =
            || Error::new(ErrorKind::Concurrency, "locked").with_sqlstate(Some("55P03".into()));
        assert!(
            !unavailable()
                .retry_whole_unit_on_lock_timeout()
                .retries_transaction()
        );
        let timeout = unavailable().with_condition(Condition::LockTimeout);
        assert!(!timeout.retries_transaction());
        let scoped = timeout.retry_whole_unit_on_lock_timeout();
        assert_eq!(scoped.sqlstate(), Some("55P03"));
        assert_eq!(scoped.condition(), Some(Condition::LockTimeout));
        assert!(scoped.retries_transaction());
    }

    #[test]
    fn named_fields_travel_in_the_context_and_keep_other_keys() {
        let plain = Error::validation("Wrong value");
        assert!(plain.fields().is_empty());
        assert!(plain.clone().with_fields([]).context().is_none());
        let blocked = Error::validation("Blocked").with_context(crate::Value::Map(Arc::new(
            crate::ValueMap::from([(
                "blocking_model".to_owned(),
                crate::Value::str("res.partner"),
            )]),
        )));
        let named = blocked
            .with_fields(["date_end"])
            .with_fields(["date_start", "date_end"]);
        assert_eq!(named.fields(), ["date_end", "date_start"]);
        let Some(crate::Value::Map(map)) = named.context() else {
            panic!("a context map");
        };
        assert_eq!(
            map.get("blocking_model"),
            Some(&crate::Value::str("res.partner"))
        );
        assert_eq!(named.message(), "Blocked");
    }

    #[test]
    fn display_is_the_message() {
        let e = Error::validation("Wrong value");
        assert_eq!(e.to_string(), "Wrong value");
        assert_eq!(e.kind(), ErrorKind::Validation);
    }
}
