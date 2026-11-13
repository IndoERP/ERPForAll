//! Database errors to ERPForAll errors (CRUD §4.9).
//!
//! Serialization failures and deadlocks are `Concurrency` (the unit of work retries them),
//! `55P03` is `Lock`. Otherwise an error whose SQLSTATE and CONSTRAINT diagnostic are a condition
//! an addon declared (contract §6) is `Validation` with the declared message, never the
//! server's text; anything else is `Database` with its SQLSTATE.

use erp_fields::{Error, ErrorKind};
use erp_registry::{Registry, TableObjectKind};
use tokio_postgres::error::SqlState;

pub(crate) const PREFIX: &str = "The operation cannot be completed: ";

pub(crate) fn map_pg_error(registry: &Registry, err: tokio_postgres::Error) -> Error {
    let Some(db) = err.as_db_error() else {
        // Not a server error: the connection closed, or the statement failed on the client (a
        // parameter that could not be encoded, a decoding error).
        let message = if err.is_closed() {
            format!("database connection closed: {err}")
        } else {
            format!("statement failed in the database client: {err}")
        };
        return Error::new(ErrorKind::Database, message).with_source(err);
    };
    let code = db.code().clone();
    let violated = db.constraint().map(str::to_owned);
    let model = db.table().and_then(|t| registry.model_by_table(t));
    let state = Some(code.code().to_owned());
    // an insert beyond the partition window of a partitioned model (design §7.6)
    if let Some(message) = crate::partition::missing_partition_message(model, db) {
        return Error::database(ErrorKind::Database, state, message, err);
    }
    let (kind, message) = if code == SqlState::NOT_NULL_VIOLATION {
        let column = db.column().unwrap_or_default();
        let message = match model {
            Some(m) => {
                let label = m
                    .field(column)
                    .map_or_else(|| column.to_owned(), |f| f.string.clone());
                format!(
                    "{PREFIX}Missing required field '{label}' ({column}) for model '{}' ({})",
                    m.description, m.name
                )
            }
            None => format!("{PREFIX}{}", db.message()),
        };
        (ErrorKind::Validation, message)
    } else if code == SqlState::CHECK_VIOLATION
        && let Some(message) = model
            .zip(db.constraint())
            .and_then(|(m, c)| crate::exclusion::period_check_message(m, c))
    {
        // the period check of a typed exclusion: the field-named text (design §10)
        (ErrorKind::Validation, message)
    } else if code == SqlState::CHECK_VIOLATION
        || code == SqlState::UNIQUE_VIOLATION
        || code == SqlState::EXCLUSION_VIOLATION
    {
        let declared = model
            .zip(db.constraint())
            .and_then(|(m, c)| m.table_object(c));
        let message = match declared.map(|o| o.kind.message()) {
            Some(text) if !text.is_empty() => format!("{PREFIX}{text}"),
            // PostgreSQL's detail line quotes the row's values (`Key (login)=(…) already
            // exists`): it stays in the error's source (logs), never in the message a client sees
            // (design note §11.3)
            _ if code == SqlState::UNIQUE_VIOLATION => format!("{PREFIX}{}", db.message()),
            _ => format!("{PREFIX}{}", db.message()),
        };
        (ErrorKind::Validation, message)
    } else if code == SqlState::STRING_DATA_RIGHT_TRUNCATION {
        // Backstop for: the converter already checks `Char(size)`.
        let message = match model {
            Some(m) => format!(
                "{PREFIX}A value is too long for a field of '{}' ({}): {}",
                m.description,
                m.name,
                db.message()
            ),
            None => format!("{PREFIX}{}", db.message()),
        };
        (ErrorKind::Validation, message)
    } else if code == SqlState::FOREIGN_KEY_VIOLATION || code == SqlState::RESTRICT_VIOLATION {
        // the error names the referencing table; its constraint names the column (CRUD-032)
        let constraint = db.constraint().unwrap_or_default();
        let message = match model {
            Some(m) => {
                let field = m
                    .fields
                    .iter()
                    .find(|f| erp_sql::ddl::foreign_key_name(&m.table, f.name) == constraint);
                let culprit = match field {
                    Some(f) => format!("'{}' ({})", f.string, f.name),
                    None => format!("'{constraint}'"),
                };
                format!(
                    "{PREFIX}Another model is using the record you are trying to delete.\n\n\
                     The troublemaker is: '{}' ({})\nThanks to the following constraint: \
                     {culprit}\nHow about archiving the record instead?",
                    m.description, m.name,
                )
            }
            None => format!("{PREFIX}{}", db.message()),
        };
        // the referencing model, for callers that explain the block (web_unlink)
        if let Some(m) = model {
            let error = Error::database(ErrorKind::Validation, state, message, err);
            let error = match &violated {
                Some(c) => error.with_constraint(c),
                None => error,
            };
            return error.with_context(erp_fields::Value::Map(std::sync::Arc::new(
                erp_fields::ValueMap::from([(
                    "blocking_model".to_owned(),
                    erp_fields::Value::str(m.name),
                )]),
            )));
        }
        (ErrorKind::Validation, message)
    } else if code == SqlState::T_R_SERIALIZATION_FAILURE || code == SqlState::T_R_DEADLOCK_DETECTED
    {
        (ErrorKind::Concurrency, db.message().to_owned())
    } else if code == SqlState::LOCK_NOT_AVAILABLE {
        // the server's text tells a lock timeout from NOWAIT (schema steps read it); the RPC
        // error body shows a fixed text instead (erp-rpc, design note §21)
        (ErrorKind::Lock, db.message().to_owned())
    } else {
        // only here: the standard states above keep their mapping whatever an addon declares
        if let Some(identifier) = violated.as_deref()
            && let Some((id, declared)) = registry.sql_objects().condition(code.code(), identifier)
        {
            return declared_condition(id, declared, identifier, err);
        }
        (ErrorKind::Database, db.message().to_owned())
    };
    let error = Error::database(kind, state, message, err);
    // integrity violations name their constraint (callers match on it, never on the text)
    match violated {
        Some(c) if code.code().starts_with("23") => error.with_constraint(&c),
        _ => error,
    }
}

/// The `Validation` error of a declared SQL condition: the declaration's message verbatim (never
/// the server's message, DETAIL, HINT or CONTEXT), the identifier as the constraint, and a context
/// naming the qualified identity. Every value is declared static data.
fn declared_condition(
    id: erp_registry::SqlObjectId,
    declared: &erp_registry::SqlCondition,
    identifier: &str,
    err: tokio_postgres::Error,
) -> Error {
    let state = declared.sqlstate();
    let context = erp_fields::ValueMap::from([
        (
            "sql_condition".to_owned(),
            erp_fields::Value::str(id.to_string()),
        ),
        (
            "sql_condition_identifier".to_owned(),
            erp_fields::Value::str(identifier),
        ),
        ("sqlstate".to_owned(), erp_fields::Value::str(state)),
    ]);
    Error::database(
        ErrorKind::Validation,
        Some(state.to_owned()),
        declared.message(),
        err,
    )
    .with_constraint(identifier)
    .with_context(erp_fields::Value::Map(std::sync::Arc::new(context)))
}

/// The `Validation` error of a table object with a declared `message`, in the text
/// [`map_pg_error`] gives its violation, for a conflict the ORM finds itself: the upsert's write
/// pre-check raises it for a key whose existing row the caller may not read, so the
/// caller learns what a `create` of that key would tell, and no id.
pub(crate) fn declared_violation(message: &str) -> Error {
    Error::validation(format!("{PREFIX}{message}"))
}

/// The user message of a table object (empty for plain indexes).
pub(crate) trait TableObjectMessage {
    fn message(&self) -> &'static str;
}

impl TableObjectMessage for TableObjectKind {
    fn message(&self) -> &'static str {
        match self {
            TableObjectKind::Constraint { message, .. }
            | TableObjectKind::UniqueIndex { message, .. } => message,
            // a constraint violation never names a trigger (they raise nothing)
            TableObjectKind::Index { .. }
            | TableObjectKind::TypedIndex(_)
            | TableObjectKind::SignalTrigger(_)
            | TableObjectKind::FollowDelete(_) => "",
            // a business key's declared message
            TableObjectKind::Key(key) => key.message,
            // typed objects: the declared message, never the server's DETAIL
            TableObjectKind::Exclusion(decl) => decl.message,
            TableObjectKind::TemporalKey(decl) => decl.message,
        }
    }
}
