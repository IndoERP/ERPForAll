//! The JSON-2 error table and body renderer (design note §11): the single
//! owner of every JSON error body. `erp-server` maps its own rejections (401, 413, 415, 429, 503,
//! session expiry) into [`RpcFailure`] and renders them here.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use erp_fields::{Error, ErrorKind, Value};

use crate::idempotency::IdempotencyKey;
use crate::output::JsonWriter;

/// The only error type rendered into a JSON-2 error body.
#[derive(Debug)]
#[non_exhaustive]
pub enum RpcFailure {
    Domain(Error),
    /// A protocol error with a `werkzeug.exceptions.*` name.
    Protocol(ProtocolError),
    /// A transport or infrastructure error (`erp.*` names).
    Infra(InfraError),
}

impl From<Error> for RpcFailure {
    fn from(error: Error) -> Self {
        Self::Domain(error)
    }
}

impl From<ProtocolError> for RpcFailure {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl From<InfraError> for RpcFailure {
    fn from(error: InfraError) -> Self {
        Self::Infra(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    /// 400: a body that is not JSON, not an object, or a malformed `Idempotency-Key`.
    BadRequest(String),
    /// 401: missing or invalid credentials (AUTH's texts).
    Unauthorized(String),
    /// 404: unknown model, method or path.
    NotFound(String),
    /// 413: the body is over `limits.rpc_body_bytes` (SERVER).
    RequestEntityTooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// 422: an argument, `ids` or context binding error.
    UnprocessableEntity(String),
    /// 403 `erp.http.session.SessionExpiredException` (AUTH's text).
    SessionExpired(String),
}

/// Infrastructure and transport errors (`erp.*` names, design note §11.2).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InfraError {
    /// 409: a request with the same key is still running.
    IdempotencyInProgress {
        /// The key.
        key: IdempotencyKey,
        /// Seconds for `Retry-After`.
        retry_after: u32,
    },
    /// 409: replay refused because the caller's access changed since the original call.
    IdempotencyAccessChanged,
    /// 409: the original call committed but its stored result is past the replay window.
    IdempotencyResultExpired,
    /// 422: the key was used for a different request.
    IdempotencyMismatch {
        /// The key.
        key: IdempotencyKey,
    },
    /// 422: the key is not accepted on this call (a `read` effect, or a caller kind without a
    /// private key space).
    IdempotencyNotAllowed(IdempotencyRefusal),
    /// 422: the serialized result is over `limits.rpc_result_bytes`.
    ResultTooLarge {
        /// `<model>.<method>`.
        entry: String,
        /// The limit in bytes.
        limit: u64,
    },
    /// 429: a per-principal rate limit (AUTH).
    RateLimited {
        /// Seconds for `Retry-After`.
        retry_after: u32,
    },
    /// 500: `COMMIT` was sent and no answer arrived.
    CommitUnknown {
        /// `<model>.<method>`.
        entry: String,
        /// Whether the call carried an `Idempotency-Key`.
        keyed: bool,
    },
    NotImplemented {
        /// `<model>.<method>`.
        entry: String,
        /// The board task that adds it.
        task: String,
    },
    /// 503: admission queue full (SERVER).
    Overloaded {
        /// Seconds for `Retry-After`.
        retry_after: u32,
    },
    /// 503: the database is in maintenance (SERVER).
    Maintenance {
        /// The database.
        database: String,
        /// Why.
        reason: String,
    },
    /// 504: the request deadline passed.
    DeadlineExceeded {
        /// The deadline in seconds.
        secs: u64,
    },
    /// 503: the audit trail could not record the call. A call that changes
    /// data changed nothing; a read selected for audit has its result withheld.
    AuditUnavailable,
}

/// Why an `Idempotency-Key` is refused on a call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdempotencyRefusal {
    /// The method's effect is `read`.
    ReadEffect {
        /// `<model>.<method>`.
        entry: String,
    },
    /// The caller kind has no private key space (`public`, `link`).
    CallerKind {
        /// The kind's name.
        kind: &'static str,
    },
}

impl RpcFailure {
    /// The HTTP status.
    pub fn status(&self) -> u16 {
        match self {
            Self::Domain(e) => e.kind().http_status(),
            Self::Protocol(p) => match p {
                ProtocolError::BadRequest(_) => 400,
                ProtocolError::Unauthorized(_) => 401,
                ProtocolError::SessionExpired(_) => 403,
                ProtocolError::NotFound(_) => 404,
                ProtocolError::RequestEntityTooLarge { .. } => 413,
                ProtocolError::UnprocessableEntity(_) => 422,
            },
            Self::Infra(i) => match i {
                InfraError::IdempotencyInProgress { .. }
                | InfraError::IdempotencyAccessChanged
                | InfraError::IdempotencyResultExpired => 409,
                InfraError::IdempotencyMismatch { .. }
                | InfraError::IdempotencyNotAllowed(_)
                | InfraError::ResultTooLarge { .. } => 422,
                InfraError::RateLimited { .. } => 429,
                InfraError::CommitUnknown { .. } => 500,
                InfraError::NotImplemented { .. } => 501,
                InfraError::Overloaded { .. }
                | InfraError::Maintenance { .. }
                | InfraError::AuditUnavailable => 503,
                InfraError::DeadlineExceeded { .. } => 504,
            },
        }
    }

    /// The `name` of the body.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Domain(e) => domain_name(e),
            Self::Protocol(p) => match p {
                ProtocolError::BadRequest(_) => "werkzeug.exceptions.BadRequest",
                ProtocolError::Unauthorized(_) => "werkzeug.exceptions.Unauthorized",
                ProtocolError::SessionExpired(_) => "erp.http.session.SessionExpiredException",
                ProtocolError::NotFound(_) => "werkzeug.exceptions.NotFound",
                ProtocolError::RequestEntityTooLarge { .. } => {
                    "werkzeug.exceptions.RequestEntityTooLarge"
                }
                ProtocolError::UnprocessableEntity(_) => "werkzeug.exceptions.UnprocessableEntity",
            },
            Self::Infra(i) => match i {
                InfraError::IdempotencyInProgress { .. } => "erp.idempotency_in_progress",
                InfraError::IdempotencyAccessChanged => "erp.idempotency_access_changed",
                InfraError::IdempotencyResultExpired => "erp.idempotency_result_expired",
                InfraError::IdempotencyMismatch { .. } => "erp.idempotency_mismatch",
                InfraError::IdempotencyNotAllowed(_) => "erp.idempotency_not_allowed",
                InfraError::ResultTooLarge { .. } => "erp.result_too_large",
                InfraError::RateLimited { .. } => "erp.rate_limited",
                InfraError::CommitUnknown { .. } => "erp.commit_unknown",
                InfraError::NotImplemented { .. } => "erp.not_implemented",
                InfraError::Overloaded { .. } => "erp.overloaded",
                InfraError::Maintenance { .. } => "erp.maintenance",
                InfraError::DeadlineExceeded { .. } => "erp.deadline_exceeded",
                InfraError::AuditUnavailable => "erp.audit_unavailable",
            },
        }
    }

    /// The `message` of the body (concealed for `Database` errors: only the SQLSTATE).
    pub fn message(&self) -> String {
        match self {
            Self::Domain(e) => domain_message(e),
            Self::Protocol(p) => match p {
                ProtocolError::BadRequest(m)
                | ProtocolError::Unauthorized(m)
                | ProtocolError::SessionExpired(m)
                | ProtocolError::NotFound(m)
                | ProtocolError::UnprocessableEntity(m) => m.clone(),
                ProtocolError::RequestEntityTooLarge { limit } => format!(
                    "The request body is larger than the server limit of {limit} bytes \
                     (limits.rpc_body_bytes)."
                ),
            },
            Self::Infra(i) => infra_message(i),
        }
    }

    /// `Retry-After` seconds, when the error carries one.
    pub fn retry_after(&self) -> Option<u32> {
        match self {
            Self::Infra(
                InfraError::IdempotencyInProgress { retry_after, .. }
                | InfraError::RateLimited { retry_after }
                | InfraError::Overloaded { retry_after },
            ) => Some(*retry_after),
            _ => None,
        }
    }

    /// The ORM error, for domain failures.
    pub fn domain_error(&self) -> Option<&Error> {
        match self {
            Self::Domain(e) => Some(e),
            _ => None,
        }
    }

    pub(crate) fn body(&self, debug: &str, timestamp: i64, request_id: Option<&str>) -> Vec<u8> {
        let message = self.message();
        let mut w = JsonWriter::unbounded();
        w.begin_object();
        w.key("name");
        w.string(self.name());
        w.key("message");
        w.string(&message);
        w.key("arguments");
        w.begin_array();
        w.string(&message);
        match self {
            Self::Protocol(ProtocolError::SessionExpired(_)) | Self::Infra(_) => {}
            Self::Protocol(_) => w.int(i64::from(self.status())),
            Self::Domain(e) if e.kind() == ErrorKind::Database => match e.sqlstate() {
                Some(state) => w.string(state),
                None => w.null(),
            },
            Self::Domain(_) => {}
        }
        w.end_array();
        w.key("timestamp");
        w.int(timestamp);
        w.key("context");
        match self.context_json() {
            Some(json) => w.raw(&json),
            None => w.empty_object(),
        }
        // The ordinary error context has a smaller, lossy cap. Keep the typed
        // sanitizer report separate so strict refusals never truncate diagnostics.
        if let Some((error, report)) = self.strict_html_report() {
            let identity = |key: &str| match error.context() {
                Some(Value::Map(map)) => match map.get(key) {
                    Some(Value::Str(text)) => Some(text.as_ref()),
                    _ => None,
                },
                _ => None,
            };
            w.key("html_reports");
            w.begin_array();
            crate::output::html_report(
                &mut w,
                identity("html_model"),
                identity("html_field"),
                identity("html_language"),
                report,
            );
            w.end_array();
        }
        w.key("debug");
        w.string(debug);
        if let Some(id) = request_id {
            w.key("request_id");
            w.string(id);
        }
        w.end_object();
        w.into_bytes()
    }
}

impl RpcFailure {
    /// Field failures may pass through argument wrappers. Follow the same bounded
    /// source depth as debug rendering and keep the nearest field error's identity.
    fn strict_html_report(&self) -> Option<(&Error, &erp_fields::html::RemovalReport)> {
        let Self::Domain(error) = self else {
            return None;
        };
        let mut identity = error;
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
        for _ in 0..16 {
            let cause = source?;
            if let Some(inner) = cause.downcast_ref::<Error>() {
                identity = inner;
            }
            if let Some(erp_fields::html::HtmlSanitizeError::DisallowedContent { report }) =
                cause.downcast_ref::<erp_fields::html::HtmlSanitizeError>()
            {
                return Some((identity, report));
            }
            source = cause.source();
        }
        None
    }

    fn context_json(&self) -> Option<Vec<u8>> {
        let Self::Domain(e) = self else {
            return None;
        };
        let context @ Value::Map(_) = e.context()? else {
            return None;
        };
        // bounded: a context is a small dictionary of names. One that cannot be written (a
        // non-finite float, over 64 KiB) is left out and logged, rather than failing the error
        // path (design note §21, "Errors")
        match crate::output::serialize(context, CONTEXT_BYTES) {
            Ok(json) => Some(json),
            Err(error) => {
                tracing::warn!(
                    target: "erp::rpc",
                    ?error,
                    name = self.name(),
                    "error context left out of the body: it cannot be written as JSON"
                );
                None
            }
        }
    }
}

impl fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.name(), self.status(), self.message())
    }
}

impl std::error::Error for RpcFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Domain(e) => Some(e),
            _ => None,
        }
    }
}

fn domain_name(e: &Error) -> &'static str {
    match e.kind() {
        ErrorKind::UserError => "erp.exceptions.UserError",
        ErrorKind::Validation => "erp.exceptions.ValidationError",
        ErrorKind::Access => "erp.exceptions.AccessError",
        ErrorKind::AccessDenied => "erp.exceptions.AccessDenied",
        // a failed session check; maps it per route kind (§4.3)
        ErrorKind::SessionExpired => "erp.http.session.SessionExpiredException",
        ErrorKind::Missing => "erp.exceptions.MissingError",
        ErrorKind::Lock => "erp.exceptions.LockError",
        ErrorKind::Redirect => "erp.exceptions.RedirectWarning",
        ErrorKind::Concurrency => match e.sqlstate() {
            Some("40001") => "psycopg2.errors.SerializationFailure",
            Some("40P01") => "psycopg2.errors.DeadlockDetected",
            _ => "erp.exceptions.ConcurrencyError",
        },
        ErrorKind::Programming => "erp.programming",
        ErrorKind::Internal => "erp.internal",
        ErrorKind::Database => "erp.database",
        ErrorKind::ResourceExhausted => "erp.resource_exhausted",
        ErrorKind::Conflict => "erp.conflict",
    }
}

fn domain_message(e: &Error) -> String {
    match e.kind() {
        ErrorKind::Concurrency => match e.sqlstate() {
            Some("40001") => "could not serialize access due to concurrent update".to_owned(),
            Some("40P01") => "deadlock detected".to_owned(),
            _ => e.message().to_owned(),
        },
        // a lock refused by the database (`55P03`): PostgreSQL's text can name the relation;
        // the ORM's own lock errors carry no SQLSTATE and keep their text
        ErrorKind::Lock if e.sqlstate().is_some() => "Cannot grab a lock on records".to_owned(),
        // PostgreSQL's message and detail can quote row values: logged, never sent
        ErrorKind::Database => match e.sqlstate() {
            Some(state) => format!("A database error occurred (SQLSTATE {state})."),
            None => "A database error occurred.".to_owned(),
        },
        ErrorKind::UserError
        | ErrorKind::Validation
        | ErrorKind::Access
        | ErrorKind::AccessDenied
        | ErrorKind::SessionExpired
        | ErrorKind::Missing
        | ErrorKind::Lock
        | ErrorKind::Redirect
        | ErrorKind::Programming
        | ErrorKind::Internal
        | ErrorKind::ResourceExhausted
        // the record, its id and the expected and current versions of a record the caller writes
        | ErrorKind::Conflict => e.message().to_owned(),
    }
}

fn infra_message(i: &InfraError) -> String {
    match i {
        InfraError::IdempotencyInProgress { key, retry_after } => format!(
            "A request with Idempotency-Key {key} is still running; retry after {retry_after} \
             seconds."
        ),
        InfraError::IdempotencyAccessChanged => "idempotency replay refused: access changed since \
                                                 the original call; the original call committed"
            .to_owned(),
        InfraError::IdempotencyResultExpired => {
            "the original call committed; its result is no longer available".to_owned()
        }
        InfraError::IdempotencyMismatch { key } => format!(
            "Idempotency-Key {key} was already used for a different request; use a new key."
        ),
        InfraError::IdempotencyNotAllowed(IdempotencyRefusal::ReadEffect { entry }) => format!(
            "Idempotency-Key is accepted only on calls that change data; {entry} has effect \
             'read'."
        ),
        InfraError::IdempotencyNotAllowed(IdempotencyRefusal::CallerKind { kind }) => format!(
            "Idempotency-Key is not accepted from {kind} callers: they have no private key space."
        ),
        InfraError::ResultTooLarge { entry, limit } => format!(
            "The result of {entry} is larger than the server limit of {limit} bytes \
             (limits.rpc_result_bytes); ask for fewer records or fields, or set bin_size."
        ),
        InfraError::RateLimited { retry_after } => {
            format!("Rate limit exceeded; retry after {retry_after} seconds.")
        }
        InfraError::CommitUnknown { entry, keyed } => {
            let first = format!(
                "The database connection was lost while committing {entry}; the change may or \
                 may not have been applied."
            );
            if *keyed {
                format!("{first} Retry with the same Idempotency-Key to get the outcome.")
            } else {
                format!("{first} Check the data before retrying.")
            }
        }
        InfraError::NotImplemented { entry, task } => {
            format!("{entry} is not supported until {task}.")
        }
        InfraError::Overloaded { retry_after } => {
            format!("Server busy; retry after {retry_after} seconds.")
        }
        InfraError::Maintenance { database, reason } => {
            format!("Database {database} is under maintenance ({reason}). Retry later.")
        }
        InfraError::DeadlineExceeded { secs } => {
            format!("The request exceeded its deadline of {secs} s (limits.rpc_deadline_secs).")
        }
        InfraError::AuditUnavailable => "The audit trail could not record this call: a call that \
                                         changes data changed nothing, and a result selected for \
                                         audit is withheld. Retry later or tell an administrator."
            .to_owned(),
    }
}

/// An error's message for `debug`: a `Database` error's text is PostgreSQL's (it can quote
/// values), so only the concealed form is shown.
fn concealed(e: &Error) -> String {
    match e.kind() {
        ErrorKind::Database => domain_message(e),
        _ => e.message().to_owned(),
    }
}

/// Largest error `context` written into a body.
const CONTEXT_BYTES: usize = 64 * 1024;

/// The body's `timestamp` (whole Unix seconds). Only error bodies use it; idempotency expiry runs
/// on the database's `now()`.
///
/// # Errors
///
/// `Programming` when the host clock is before 1970 or past the `i64` range.
pub(crate) fn unix_seconds() -> erp_fields::Result<i64> {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::programming(format!("the host clock is before 1970: {e}")))?;
    i64::try_from(since.as_secs())
        .map_err(|_| Error::programming("the host clock is past the 64-bit range"))
}

/// What `debug` may show when both the operator switch and the capability allow it (design note
/// §11.3): the kind, SQLSTATE, correlation id, the error chain (database errors by SQLSTATE and
/// primary message only, never their detail lines), the failed statement's template and the
/// span. Never bound values, credentials, cookies, paths or backtraces.
#[derive(Debug, Default)]
pub(crate) struct DebugInfo {
    pub(crate) correlation: Option<String>,
    pub(crate) entry: Option<String>,
    pub(crate) statement: Option<String>,
}

impl DebugInfo {
    pub(crate) fn render(&self, failure: &RpcFailure) -> String {
        let mut lines = vec![format!("kind: {} ({})", failure.name(), failure.status())];
        if let Some(e) = failure.domain_error() {
            lines.push(format!("error kind: {:?}", e.kind()));
            if let Some(state) = e.sqlstate() {
                lines.push(format!("sqlstate: {state}"));
            }
            // the server's own texts can quote bound values (`invalid input syntax for type
            // integer: "…"`): database errors show their SQLSTATE and object names only
            lines.push(format!("error: {}", concealed(e)));
            let mut source = std::error::Error::source(e);
            let mut depth = 0;
            while let Some(s) = source {
                depth += 1;
                if depth > 16 {
                    lines.push("caused by: …".to_owned());
                    break;
                }
                let text = if let Some(pg) = s.downcast_ref::<tokio_postgres::Error>() {
                    match pg.as_db_error() {
                        Some(db) => {
                            let mut text = format!("SQLSTATE {}", db.code().code());
                            for (what, name) in [
                                ("table", db.table()),
                                ("column", db.column()),
                                ("constraint", db.constraint()),
                            ] {
                                if let Some(name) = name {
                                    text.push_str(&format!(", {what} {name}"));
                                }
                            }
                            text
                        }
                        None if pg.is_closed() => "database connection closed".to_owned(),
                        None => "database client error".to_owned(),
                    }
                } else if let Some(inner) = s.downcast_ref::<Error>() {
                    concealed(inner)
                } else {
                    s.to_string()
                };
                lines.push(format!("caused by: {text}"));
                if s.is::<tokio_postgres::Error>() {
                    // below it is the server's own error text: never shown
                    break;
                }
                source = s.source();
            }
        }
        if let Some(statement) = &self.statement {
            lines.push(format!("statement: {statement}"));
        }
        if let Some(entry) = &self.entry {
            lines.push(format!("span: rpc.call {entry}"));
        }
        if let Some(correlation) = &self.correlation {
            lines.push(format!("correlation: {correlation}"));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests;
