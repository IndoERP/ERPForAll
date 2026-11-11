//! One names-only, ordered final INSERT. No schema repair or side effects occur here.

use super::{
    Envelope, MAX_ENVELOPE_BYTES, MAX_ENVELOPES, MAX_TRANSACTION_BYTES, Queued, SCHEMA_VERSION,
};
use crate::Transaction;
use erp_fields::{Error, ErrorKind, Result};
use std::io::{self, Write};

const INSERT: &str = r#"INSERT INTO "erp_event"."ir_event_outbox"
(schema_version, kind, model, record_ids, field_names, cascade_models, subject,
 principal_kind, actor_kind, actor_id, delegating_user_id, company_ids,
 correlation_id, operation_id, causation_id, tx_ordinal)
SELECT schema_version, kind, model, record_ids::jsonb, field_names, cascade_models,
 subject, principal_kind, actor_kind, actor_id, delegating_user_id, company_ids,
 correlation_id, operation_id, causation_id, tx_ordinal
FROM pg_catalog.jsonb_to_recordset($1::text::jsonb) AS e(
 schema_version smallint, kind text, model text, record_ids text,
 field_names text[], cascade_models text[], subject text,
 principal_kind text, actor_kind text, actor_id bigint, delegating_user_id bigint,
 company_ids bigint[], correlation_id uuid, operation_id uuid, causation_id uuid,
 tx_ordinal bigint)
ORDER BY tx_ordinal"#;

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
    tx.execute(INSERT, &[&payload]).await
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
