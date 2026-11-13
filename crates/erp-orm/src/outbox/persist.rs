//! One names-only, ordered final INSERT. No schema repair or side effects occur here.

use super::{
    Envelope, MAX_ENVELOPE_BYTES, MAX_ENVELOPES, MAX_TRANSACTION_BYTES, Queued, SCHEMA_VERSION,
};
use crate::Transaction;
use crate::event_schema::{Column, OUTBOX_COLUMNS};
use erp_fields::{Error, ErrorKind, Result};
use std::io::{self, Write};
use std::sync::LazyLock;

/// The one supplied column the envelope carries as pre-encoded JSON text: the recordset reads it as
/// `text` and the select casts it to `jsonb`. Every other column is read with its catalog type; a
/// test refuses any other `jsonb` column, so a new one cannot take this path unnoticed.
const PRE_ENCODED_JSON: &str = "record_ids";

/// The capture INSERT, built once from [`OUTBOX_COLUMNS`] so the table definition is its only
/// column source: the supplied columns are those without a database default, in definition order.
static INSERT: LazyLock<String> = LazyLock::new(|| {
    let supplied: Vec<&Column> = OUTBOX_COLUMNS
        .iter()
        .filter(|column| column.default.is_none())
        .collect();
    let names: Vec<&str> = supplied.iter().map(|column| column.name).collect();
    let select: Vec<String> = supplied
        .iter()
        .map(|column| {
            if column.name == PRE_ENCODED_JSON {
                format!("{}::jsonb", column.name)
            } else {
                column.name.to_owned()
            }
        })
        .collect();
    let definition: Vec<String> = supplied
        .iter()
        .map(|column| {
            let ty = if column.name == PRE_ENCODED_JSON {
                "text"
            } else {
                column.ty
            };
            format!("{} {ty}", column.name)
        })
        .collect();
    format!(
        "INSERT INTO {} ({}) SELECT {} FROM pg_catalog.jsonb_to_recordset($1::text::jsonb) \
         AS e({}) ORDER BY tx_ordinal",
        crate::event_schema::OUTBOX.qualified(),
        names.join(", "),
        select.join(", "),
        definition.join(", ")
    )
});

/// The generated capture INSERT.
fn insert_statement() -> &'static str {
    INSERT.as_str()
}

#[derive(serde::Serialize)]
struct Row<'a> {
    schema_version: i16,
    kind: &'static str,
    model: &'a str,
    record_ids: &'a str,
    field_names: &'a [String],
    cascade_models: &'a [String],
    subject: &'a str,
    principal_kind: &'static str,
    actor_kind: &'static str,
    actor_id: i64,
    delegating_user_id: Option<i64>,
    company_ids: &'a [i64],
    correlation_id: Option<&'a str>,
    operation_id: &'a str,
    causation_id: Option<&'a str>,
    tx_ordinal: i64,
}

impl<'a> Row<'a> {
    fn new(event: &'a Envelope, tx_ordinal: i64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: event.kind().as_str(),
            model: event.model(),
            record_ids: event.record_ids(),
            field_names: event.field_names(),
            cascade_models: event.cascade_models(),
            subject: event.subject(),
            principal_kind: event.principal_kind(),
            actor_kind: event.actor_kind(),
            actor_id: event.actor_id(),
            delegating_user_id: event.delegating_user_id(),
            company_ids: event.company_ids(),
            correlation_id: event.correlation_id().map(crate::CorrelationId::as_str),
            operation_id: event.operation_id().as_str(),
            causation_id: event.causation_id().map(crate::CorrelationId::as_str),
            tx_ordinal,
        }
    }
}

/// Count the same serialized row used by persistence without allocating an encoded copy.
/// Admission reserves the largest possible ordinal so every later position fits this charge.
pub(super) fn encoded_envelope_bytes(event: &Envelope) -> Result<usize> {
    let mut counter = EnvelopeCounter {
        bytes: 0,
        exceeded: false,
    };
    // MAX_ENVELOPES is the hard 4096 bound; converting this constant is lossless.
    let row = Row::new(event, MAX_ENVELOPES as i64);
    if let Err(error) = serde_json::to_writer(&mut counter, &row) {
        if counter.exceeded {
            return Err(super::limit("event exceeds 65536 bytes"));
        }
        return Err(
            Error::new(ErrorKind::Internal, "outbox: envelope size encoding failed")
                .with_source(error),
        );
    }
    Ok(counter.bytes)
}

struct EnvelopeCounter {
    bytes: usize,
    exceeded: bool,
}
impl Write for EnvelopeCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self
            .bytes
            .checked_add(bytes.len())
            .filter(|len| *len <= MAX_ENVELOPE_BYTES)
        else {
            self.exceeded = true;
            return Err(io::Error::other("outbox envelope byte limit"));
        };
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn encode(events: &[Queued]) -> Result<String> {
    let rows: Vec<Row<'_>> = events
        .iter()
        .enumerate()
        .map(|(index, queued)| {
            // The private buffer is hard-limited to 4096 envelopes, so this cannot overflow i64.
            Row::new(queued.envelope(), index as i64 + 1)
        })
        .collect();
    let mut encoded = BoundedJson {
        bytes: Vec::new(),
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut encoded, &rows) {
        if encoded.exceeded {
            return Err(super::limit("encoded transaction exceeds 8388608 bytes"));
        }
        return Err(
            Error::new(ErrorKind::Internal, "outbox: event batch encoding failed")
                .with_source(error),
        );
    }
    String::from_utf8(encoded.bytes).map_err(|error| {
        Error::new(
            ErrorKind::Internal,
            "outbox: event batch encoding produced invalid UTF-8",
        )
        .with_source(error)
    })
}

pub(crate) async fn persist_encoded(tx: &mut Transaction, payload: &str) -> Result<u64> {
    tx.execute(insert_statement(), &[&payload])
        .await
        .map_err(insertion_error)
}

/// The message of an outbox INSERT that no partition accepts.
const MISSING_PARTITION: &str = "outbox: no partition of erp_event.ir_event_outbox covers the \
    transaction's day; the event partition upkeep has not run (it runs at server start and hourly; \
    `erp audit maintain` refreshes both event tables)";

/// An INSERT failure. PostgreSQL's partition-routing failure (no partition covers the
/// transaction's day) becomes a `Concurrency` error naming the upkeep, as the audit store's
/// admission does, with the database error kept as its source and no SQLSTATE, so the unit of
/// work never retries it as a serialization failure. Everything else, `40001` and `40P01`
/// included, passes unchanged.
fn insertion_error(error: Error) -> Error {
    if crate::event_schema::is_partition_miss(&error, crate::event_schema::OUTBOX.table) {
        Error::new(ErrorKind::Concurrency, MISSING_PARTITION).with_source(error)
    } else {
        error
    }
}

/// Refuse before growing past the transport cap, including JSON escape expansion.
struct BoundedJson {
    bytes: Vec<u8>,
    exceeded: bool,
}
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|len| len > MAX_TRANSACTION_BYTES)
        {
            self.exceeded = true;
            return Err(io::Error::other("outbox JSON byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_error(kind: ErrorKind, state: &str, message: &str) -> Error {
        Error::new(kind, message).with_sqlstate(Some(state.to_owned()))
    }

    /// The columns a capture supplies: those without a database default, in definition order.
    fn supplied() -> Vec<&'static crate::event_schema::Column> {
        OUTBOX_COLUMNS
            .iter()
            .filter(|c| c.default.is_none())
            .collect()
    }

    /// The INSERT is generated from `OUTBOX_COLUMNS`: every column without a default appears, in
    /// order, in the column list, the select list and the recordset definition with its catalog
    /// type; `record_ids` is text in the recordset and cast to `jsonb` in the select; no generated
    /// column appears.
    #[test]
    fn the_insert_is_generated_from_the_outbox_columns() {
        let sql = insert_statement();
        let columns = supplied();
        assert!(!columns.is_empty());
        let names: Vec<&str> = columns.iter().map(|c| c.name).collect();
        let select: Vec<String> = columns
            .iter()
            .map(|c| {
                if c.name == "record_ids" {
                    "record_ids::jsonb".to_owned()
                } else {
                    c.name.to_owned()
                }
            })
            .collect();
        let definition: Vec<String> = columns
            .iter()
            .map(|c| {
                let ty = if c.name == "record_ids" { "text" } else { c.ty };
                format!("{} {ty}", c.name)
            })
            .collect();
        assert!(
            sql.starts_with(&format!(
                "INSERT INTO \"erp_event\".\"ir_event_outbox\" ({})",
                names.join(", ")
            )),
            "{sql}"
        );
        assert!(
            sql.contains(&format!("SELECT {} FROM", select.join(", "))),
            "{sql}"
        );
        assert!(
            sql.contains(&format!(
                "pg_catalog.jsonb_to_recordset($1::text::jsonb) AS e({})",
                definition.join(", ")
            )),
            "{sql}"
        );
        assert!(sql.ends_with("ORDER BY tx_ordinal"), "{sql}");
        assert_eq!(sql.matches("record_ids::jsonb").count(), 1, "{sql}");
        // the column list names no generated column (a substring test would match `id` inside
        // `record_ids`, so compare the listed names)
        let listed: Vec<&str> = sql
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(list, _)| list.split(", ").collect())
            .expect("the INSERT names its columns");
        assert_eq!(listed, names, "{sql}");
        for generated in OUTBOX_COLUMNS.iter().filter(|c| c.default.is_some()) {
            assert!(
                !listed.contains(&generated.name),
                "generated column {} must not be supplied: {sql}",
                generated.name
            );
        }
        // the pre-encoded JSON column is the only jsonb column
        let jsonb: Vec<&str> = OUTBOX_COLUMNS
            .iter()
            .filter(|c| c.ty == "jsonb")
            .map(|c| c.name)
            .collect();
        assert_eq!(jsonb, [PRE_ENCODED_JSON]);
    }

    /// The serialized keys of a `Row` are exactly the supplied-column set, so a renamed field on a
    /// nullable column cannot store NULL silently.
    #[test]
    fn row_keys_equal_the_supplied_columns() {
        let envelope = Envelope::new(super::super::EnvelopeParts {
            kind: super::super::Kind::Create,
            model: "res.partner",
            record_ids: vec![super::super::IdEntry::Single(1)],
            field_names: Vec::new(),
            cascade_models: Vec::new(),
            subject: Some("2".into()),
            actor_id: 2,
            actor_kind: super::super::capture::ActorKind::User,
            principal_kind: crate::PrincipalKind::User,
            operation_id: crate::OperationIdentity::generate()
                .expect("operation identity")
                .operation()
                .clone(),
            delegating_user_id: None,
            company_ids: vec![1],
            correlation_id: None,
            causation_id: None,
        })
        .expect("envelope");
        let value = serde_json::to_value(Row::new(&envelope, 1)).expect("row serializes");
        let object = value.as_object().expect("a row is a JSON object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected: Vec<&str> = supplied().iter().map(|c| c.name).collect();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    /// A routing failure of the outbox INSERT becomes the typed error naming the upkeep, with the
    /// database error kept as its source and no SQLSTATE (it is never retried as a serialization
    /// failure); every other failure passes unchanged.
    #[test]
    fn a_missing_partition_names_the_upkeep_and_other_failures_pass() {
        let miss = insertion_error(db_error(
            ErrorKind::Validation,
            "23514",
            "no partition of relation \"ir_event_outbox\" found for row",
        ));
        assert_eq!(miss.kind(), ErrorKind::Concurrency);
        assert_eq!(
            miss.message(),
            "outbox: no partition of erp_event.ir_event_outbox covers the transaction's day; \
             the event partition upkeep has not run (it runs at server start and hourly; `erp \
             audit maintain` refreshes both event tables)"
        );
        assert_eq!(miss.sqlstate(), None);
        let source = std::error::Error::source(&miss).expect("the database error");
        assert!(
            source.to_string().contains("no partition of relation"),
            "{source}"
        );
        for (kind, state) in [
            (ErrorKind::Concurrency, "40001"),
            (ErrorKind::Concurrency, "40P01"),
            (ErrorKind::Database, "42501"),
        ] {
            let error = insertion_error(db_error(kind, state, "other failure"));
            assert_eq!(error.kind(), kind);
            assert_eq!(error.sqlstate(), Some(state));
            assert_eq!(error.message(), "other failure");
        }
        let check = insertion_error(
            db_error(
                ErrorKind::Validation,
                "23514",
                "new row violates check constraint",
            )
            .with_constraint("ir_event_outbox_check"),
        );
        assert_eq!(check.kind(), ErrorKind::Validation);
        assert_eq!(check.message(), "new row violates check constraint");
    }
}
