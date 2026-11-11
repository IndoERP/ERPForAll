//! Private transactional event capture and ordered final persistence.
//! Create, scalar/stored-compute writes and supported root unlinks are captured here.
//! Unsupported indirect deletion effects, upsert, relational commands and delivery remain
//! explicit implementation boundaries.

use erp_fields::{Error, ErrorKind, Result};

use crate::correlation::{CorrelationId, OperationId};

mod capture;
mod pending;
mod persist;
pub(crate) use capture::{CaptureContext, actor_details};
pub(crate) use pending::Pending;
pub(crate) use persist::{encode, persist_encoded};

pub(crate) const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_TRANSACTION_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_ENVELOPES: usize = 4096;
pub(crate) const MAX_ID_ENTRIES: usize = 256;

/// One actual inserted id. Reserved-block range compression is deferred to S9.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum IdEntry {
    Single(i64),
}

/// What happened to the records. The text is the `kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Create,
    Write,
    Unlink,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Write => "write",
            Self::Unlink => "unlink",
        }
    }
}

/// The `schema_version` column of every envelope this build writes.
pub(crate) const SCHEMA_VERSION: i16 = 1;
const MAX_CHANGED_NAMES: usize = 128;
pub(crate) const MAX_CASCADE_MODELS: usize = 64;
const MAX_COMPANIES: usize = 256;
const MAX_NAME_BYTES: usize = 256;
const MAX_SUBJECT_BYTES: usize = 1024;

/// The input of [`Envelope::new`]: names and ids only, never a field value.
#[derive(Debug)]
pub(crate) struct EnvelopeParts<'a> {
    pub(crate) kind: Kind,
    pub(crate) model: &'a str,
    pub(crate) record_ids: Vec<IdEntry>,
    pub(crate) field_names: Vec<String>,
    pub(crate) cascade_models: Vec<String>,
    pub(crate) subject: Option<String>,
    pub(crate) actor_id: i64,
    pub(crate) actor_kind: capture::ActorKind,
    pub(crate) principal_kind: crate::PrincipalKind,
    pub(crate) operation_id: OperationId,
    pub(crate) delegating_user_id: Option<i64>,
    pub(crate) company_ids: Vec<i64>,
    pub(crate) correlation_id: Option<CorrelationId>,
    pub(crate) causation_id: Option<CorrelationId>,
}

/// One validated capture, shaped as the supplied columns of `erp_event.ir_event_outbox`
/// (`event_schema::COLUMNS`). Construction enforces every bound, so a value that exists fits
/// [`MAX_ENVELOPE_BYTES`].
#[derive(Debug)]
pub(crate) struct Envelope {
    kind: Kind,
    model: Box<str>,
    record_ids: Box<str>,
    field_names: Vec<String>,
    cascade_models: Vec<String>,
    subject: String,
    actor_id: i64,
    actor_kind: capture::ActorKind,
    principal_kind: crate::PrincipalKind,
    operation_id: OperationId,
    delegating_user_id: Option<i64>,
    company_ids: Vec<i64>,
    correlation_id: Option<CorrelationId>,
    causation_id: Option<CorrelationId>,
    encoded_bytes: usize,
    capture_bytes: usize,
}

impl Envelope {
    /// # Errors
    /// `Programming` for an empty model, no record ids, an unsupported cascade on a non-unlink
    /// event, or a malformed name; `ResourceExhausted` (CaptureLimit:`) when a list
    /// exceeds its bound or the encoded envelope exceeds [`MAX_ENVELOPE_BYTES`].
    pub(crate) fn new(parts: EnvelopeParts<'_>) -> Result<Self> {
        let EnvelopeParts {
            kind,
            model,
            record_ids,
            field_names,
            cascade_models,
            subject,
            actor_id,
            actor_kind,
            principal_kind,
            operation_id,
            delegating_user_id,
            company_ids,
            correlation_id,
            causation_id,
        } = parts;
        if model.is_empty() || record_ids.is_empty() || actor_id <= 0 {
            return Err(Error::programming(
                "outbox: an event needs a model, record ids and an actor",
            ));
        }
        if kind != Kind::Unlink && !cascade_models.is_empty() {
            return Err(Error::programming(
                "outbox: only unlink events carry cascade models",
            ));
        }
        if record_ids.len() > MAX_ID_ENTRIES {
            return Err(limit("event exceeds 256 id entries"));
        }
        if field_names.len() > MAX_CHANGED_NAMES {
            return Err(limit("event exceeds 128 changed field names"));
        }
        if cascade_models.len() > MAX_CASCADE_MODELS {
            return Err(limit("event exceeds 64 cascade models"));
        }
        if company_ids.len() > MAX_COMPANIES {
            return Err(limit("event exceeds 256 company ids"));
        }
        for name in std::iter::once(model)
            .chain(field_names.iter().map(String::as_str))
            .chain(cascade_models.iter().map(String::as_str))
        {
            if name.is_empty() || name.len() > MAX_NAME_BYTES || name.contains('\0') {
                return Err(Error::programming(
                    "outbox: model and field names must be 1-256 bytes without NUL",
                ));
            }
        }
        let subject =
            subject.ok_or_else(|| Error::programming("outbox: an event requires a subject"))?;
        if subject.len() > MAX_SUBJECT_BYTES || subject.contains('\0') {
            return Err(limit("event subject exceeds 1024 bytes or contains NUL"));
        }
        let record_ids = serde_json::to_string(&record_ids).map_err(|error| {
            Error::new(ErrorKind::Internal, "outbox: record id encoding failed").with_source(error)
        })?;
        let mut envelope = Self {
            kind,
            model: model.into(),
            record_ids: record_ids.into(),
            field_names,
            cascade_models,
            subject,
            actor_id,
            actor_kind,
            principal_kind,
            operation_id,
            delegating_user_id,
            company_ids,
            correlation_id,
            causation_id,
            encoded_bytes: 0,
            capture_bytes: 0,
        };
        envelope.encoded_bytes = persist::encoded_envelope_bytes(&envelope)?;
        envelope.capture_bytes = envelope.capture_charge()?;
        Ok(envelope)
    }

    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }
    pub(crate) fn model(&self) -> &str {
        &self.model
    }
    /// The `record_ids` jsonb text: an array of actual ids; reserved ranges are deferred to S9.
    pub(crate) fn record_ids(&self) -> &str {
        &self.record_ids
    }
    pub(crate) fn field_names(&self) -> &[String] {
        &self.field_names
    }
    pub(crate) fn cascade_models(&self) -> &[String] {
        &self.cascade_models
    }
    pub(crate) fn subject(&self) -> &str {
        &self.subject
    }
    pub(crate) fn actor_id(&self) -> i64 {
        self.actor_id
    }
    pub(crate) fn actor_kind(&self) -> &'static str {
        self.actor_kind.as_str()
    }
    pub(crate) fn principal_kind(&self) -> &'static str {
        self.principal_kind.as_str()
    }
    pub(crate) fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }
    pub(crate) fn delegating_user_id(&self) -> Option<i64> {
        self.delegating_user_id
    }
    pub(crate) fn company_ids(&self) -> &[i64] {
        &self.company_ids
    }
    pub(crate) fn correlation_id(&self) -> Option<&CorrelationId> {
        self.correlation_id.as_ref()
    }
    pub(crate) fn causation_id(&self) -> Option<&CorrelationId> {
        self.causation_id.as_ref()
    }

    fn charged_bytes(&self) -> usize {
        self.capture_bytes
    }

    /// The serialized row covers the raw string payload, including UUID boxes. Charge the
    /// additional heap metadata and spare capacity conservatively, without copying it.
    fn capture_charge(&self) -> Result<usize> {
        let overflow = || limit("event memory accounting overflow");
        let add = |total: usize, extra: usize| total.checked_add(extra).ok_or_else(overflow);
        let mut bytes = add(self.encoded_bytes, std::mem::size_of::<Queued>())?;
        for names in [&self.field_names, &self.cascade_models] {
            let slots = names
                .capacity()
                .checked_mul(std::mem::size_of::<String>())
                .ok_or_else(overflow)?;
            bytes = add(bytes, slots)?;
            for name in names {
                let spare = name
                    .capacity()
                    .checked_sub(name.len())
                    .ok_or_else(overflow)?;
                bytes = add(bytes, spare)?;
            }
        }
        // Short decimal ids can use fewer JSON bytes than their in-memory i64 slots. Charging
        // the full vector also includes unused capacity; any double accounting is conservative.
        let companies = self
            .company_ids
            .capacity()
            .checked_mul(std::mem::size_of::<i64>())
            .ok_or_else(overflow)?;
        bytes = add(bytes, companies)?;
        let subject_spare = self
            .subject
            .capacity()
            .checked_sub(self.subject.len())
            .ok_or_else(overflow)?;
        add(bytes, subject_spare)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BufferLimits {
    pub(crate) max_envelopes: usize,
    pub(crate) max_bytes: usize,
}

impl Default for BufferLimits {
    fn default() -> Self {
        Self {
            max_envelopes: MAX_ENVELOPES,
            max_bytes: MAX_TRANSACTION_BYTES,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Queued {
    envelope: Envelope,
    depth: usize,
}

impl Queued {
    pub(crate) fn envelope(&self) -> &Envelope {
        &self.envelope
    }
}

#[derive(Debug)]
pub(crate) struct Buffer {
    events: Vec<Queued>,
    bytes: usize,
    limits: BufferLimits,
    frozen: bool,
}

impl Buffer {
    pub(crate) fn new(limits: BufferLimits) -> Result<Self> {
        if !(1..=MAX_ENVELOPES).contains(&limits.max_envelopes)
            || !(MAX_ENVELOPE_BYTES..=MAX_TRANSACTION_BYTES).contains(&limits.max_bytes)
        {
            return Err(Error::programming("outbox: invalid capture buffer limits"));
        }
        Ok(Self {
            events: Vec::new(),
            bytes: 0,
            limits,
            frozen: false,
        })
    }

    pub(crate) fn push(&mut self, envelope: Envelope, depth: usize) -> Result<()> {
        if self.frozen {
            return Err(Error::programming("outbox: capture is frozen for commit"));
        }
        if self.events.len() >= self.limits.max_envelopes {
            return Err(limit(&format!(
                "transaction exceeds {} envelopes",
                self.limits.max_envelopes
            )));
        }
        let bytes = self
            .bytes
            .checked_add(envelope.charged_bytes())
            .ok_or_else(|| limit("transaction byte accounting overflow"))?;
        if bytes > self.limits.max_bytes {
            return Err(limit(&format!(
                "transaction exceeds {} bytes",
                self.limits.max_bytes
            )));
        }
        self.events.push(Queued { envelope, depth });
        self.bytes = bytes;
        Ok(())
    }

    pub(crate) fn events(&self) -> &[Queued] {
        &self.events
    }

    pub(crate) fn rolled_back_to(&mut self, depth: usize) {
        self.events.retain(|event| event.depth <= depth);
        // Every retained event was charged under the same hard aggregate bound.
        self.bytes = self
            .events
            .iter()
            .map(|event| event.envelope.charged_bytes())
            .sum();
    }

    pub(crate) fn released_to(&mut self, depth: usize) {
        for event in &mut self.events {
            event.depth = event.depth.min(depth);
        }
    }

    pub(crate) fn freeze(&mut self) -> Result<()> {
        if self.frozen {
            return Err(Error::programming("outbox: capture is frozen for commit"));
        }
        self.frozen = true;
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        self.events.clear();
        self.bytes = 0;
        self.frozen = false;
    }
}

fn limit(message: &str) -> Error {
    Error::new(
        ErrorKind::ResourceExhausted,
        format!("outbox CaptureLimit: {message}"),
    )
}

#[cfg(test)]
mod tests;
