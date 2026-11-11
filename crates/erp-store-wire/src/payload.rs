//! The canonical encoder: answer and frame builders (spec §4.2).

use std::collections::{BTreeMap, BTreeSet};

use crate::cursor::BusCursor;
use crate::error::WireError;
use crate::limits::{Limit, WireLimits};
use crate::schema::{FieldKind, FieldName, FieldSchema, ModelName, StoreSchema, SyncClass};
use crate::snapshot::SnapshotDescriptor;
use crate::value::{
    check_date, check_datetime, check_float, check_id, check_safe_int, check_string, normalise_ids,
    write_float, write_ids, write_string,
};
use crate::version::WireMajor;

/// A `{model, id}` pair: a polymorphic reference value, a decoded delete marker.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordRef {
    model: ModelName,
    id: u64,
}

impl RecordRef {
    /// Build a pair; the id must be 1..2^53-1.
    ///
    /// # Errors
    /// [`WireError::BadName`] or [`WireError::BadId`].
    pub fn new(model: &str, id: u64) -> Result<Self, WireError> {
        let model = ModelName::new(model)?;
        check_id(id, &|| "id".to_owned())?;
        Ok(Self { model, id })
    }

    pub(crate) fn from_parts(model: ModelName, id: u64) -> Self {
        Self { model, id }
    }

    /// The model.
    #[must_use]
    pub fn model(&self) -> &ModelName {
        &self.model
    }

    /// The id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }
}

/// The value of one field in a payload, by schema kind. A builder checks the variant against the
/// field's kind; the decoder produces the variant its schema kind names.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FieldValue {
    /// `bool`.
    Bool(bool),
    /// `int`.
    Int(i64),
    /// `float`.
    Float(f64),
    /// `string`.
    String(String),
    /// `html`: plain text typed as markup by the schema.
    Html(String),
    /// `date`: `YYYY-MM-DD`.
    Date(String),
    /// `datetime`: `YYYY-MM-DD HH:MM:SS`.
    Datetime(String),
    /// `selection`.
    Selection(String),
    /// `many2one`: an id or null.
    Many2One(Option<u64>),
    /// `reference`: a pair or null.
    Reference(Option<RecordRef>),
    /// `x2many` replace (answers): the full id set.
    X2ManyReplace(Vec<u64>),
    /// `x2many` add/remove (frames, and answers if wanted): at least one list is not empty.
    X2ManyDelta {
        /// Ids added.
        add: Vec<u64>,
        /// Ids removed.
        remove: Vec<u64>,
    },
    /// `counter` in an answer: the absolute value.
    Counter(i64),
    /// `counter` in a frame: a non-zero delta.
    Inc(i64),
}

macro_rules! builder_api {
    ($name:ident, $frame:expr) => {
        impl<'s> $name<'s> {
            /// Use the host's limits instead of the defaults.
            #[must_use]
            pub fn with_limits(mut self, limits: WireLimits) -> Self {
                self.limits = limits;
                self
            }

            /// Set one field of one record. Sets for one record merge into one entry.
            ///
            /// # Errors
            /// Any value refusal: [`WireError::UnknownModel`], [`WireError::UnknownField`],
            /// [`WireError::TypeMismatch`], [`WireError::ValueOutOfRange`], [`WireError::BadId`],
            /// [`WireError::DuplicateId`], [`WireError::EmptyOperation`],
            /// [`WireError::AddRemoveOverlap`], [`WireError::ZeroDelta`],
            /// [`WireError::OperationNotAllowed`], [`WireError::DuplicateField`],
            /// [`WireError::DeleteWithFields`] or [`WireError::LimitExceeded`].
            pub fn set(
                &mut self,
                model: &str,
                id: u64,
                field: &str,
                value: FieldValue,
            ) -> Result<(), WireError> {
                self.core
                    .set(self.schema, &self.limits, $frame, model, id, field, value)
            }

            /// Mark a record deleted. Marking it twice is the same fact.
            ///
            /// # Errors
            /// [`WireError::UnknownModel`], [`WireError::BadId`] or
            /// [`WireError::DeleteWithFields`].
            pub fn delete(&mut self, model: &str, id: u64) -> Result<(), WireError> {
                self.core.delete(self.schema, &self.limits, model, id)
            }

            /// Encode the canonical bytes.
            ///
            /// # Errors
            /// [`WireError::BadWritten`] for a marker that does not describe the payload, or
            /// [`WireError::LimitExceeded`] for oversize output.
            pub fn encode(&self) -> Result<Vec<u8>, WireError> {
                let mut out = String::from("{\"asof\":");
                self.asof.write_json(&mut out);
                $name::write_cursor(self, &mut out);
                out.push_str(",\"records\":");
                self.core.write_records(&mut out)?;
                out.push_str(",\"schema\":");
                write_string(&mut out, self.schema.generation().as_str());
                out.push_str(&format!(",\"wire\":{}", WireMajor::CURRENT.get()));
                self.core.write_written(&mut out)?;
                out.push('}');
                self.limits.check(Limit::Bytes, out.len(), "")?;
                Ok(out.into_bytes())
            }
        }
    };
}

/// Builds an answer payload (the store member of an RPC result).
#[derive(Debug)]
pub struct AnswerBuilder<'s> {
    schema: &'s StoreSchema,
    asof: SnapshotDescriptor,
    limits: WireLimits,
    core: Core,
}

impl<'s> AnswerBuilder<'s> {
    /// Start an answer under the descriptor of the transaction that read it.
    #[must_use]
    pub fn new(schema: &'s StoreSchema, asof: SnapshotDescriptor) -> Self {
        Self {
            schema,
            asof,
            limits: WireLimits::default(),
            core: Core::default(),
        }
    }

    /// Mark a field of a record in this payload as written by the answering transaction. Allowed
    /// only when the descriptor has an own xid.
    ///
    /// # Errors
    /// [`WireError::BadWritten`] when the descriptor has no own xid or the field is a counter.
    pub fn mark_written(&mut self, model: &str, id: u64, field: &str) -> Result<(), WireError> {
        if self.asof.own().is_none() {
            return Err(WireError::BadWritten {
                path: "written".to_owned(),
                reason: "answer has no own xid".to_owned(),
            });
        }
        let field_schema = lookup_field(self.schema, model, id, field)?;
        if field_schema.kind() == FieldKind::Counter {
            return Err(WireError::BadWritten {
                path: format!("written.{model}.{id}.{field}"),
                reason: "counter fields cannot be written markers".to_owned(),
            });
        }
        let (model_name, field_name) = (ModelName::new(model)?, FieldName::new(field)?);
        if !self
            .core
            .written
            .entry((model_name, id))
            .or_default()
            .insert(field_name)
        {
            return Err(WireError::DuplicateField {
                path: format!("written.{model}.{id}.{field}"),
            });
        }
        Ok(())
    }

    /// An answer has no cursor member.
    fn write_cursor(&self, _out: &mut String) {}
}

builder_api!(AnswerBuilder, false);

/// Builds a bus store frame.
#[derive(Debug)]
pub struct FrameBuilder<'s> {
    schema: &'s StoreSchema,
    cursor: BusCursor,
    asof: SnapshotDescriptor,
    limits: WireLimits,
    core: Core,
}

impl<'s> FrameBuilder<'s> {
    /// Start a frame at `cursor` under the emitting transaction's descriptor.
    ///
    /// # Errors
    /// [`WireError::BadDescriptor`] (path `asof.own`) unless the descriptor's own xid equals the
    /// cursor's xid.
    pub fn new(
        schema: &'s StoreSchema,
        cursor: BusCursor,
        asof: SnapshotDescriptor,
    ) -> Result<Self, WireError> {
        if asof.own() != Some(cursor.xid()) {
            return Err(WireError::BadDescriptor {
                path: "asof.own".to_owned(),
                reason: "own xid must equal the cursor xid".to_owned(),
            });
        }
        Ok(Self {
            schema,
            cursor,
            asof,
            limits: WireLimits::default(),
            core: Core::default(),
        })
    }

    fn write_cursor(&self, out: &mut String) {
        out.push_str(&format!(
            ",\"cursor\":{{\"id\":\"{}\",\"xid\":\"{}\"}}",
            self.cursor.id(),
            self.cursor.xid()
        ));
    }
}

builder_api!(FrameBuilder, true);

/// What one record entry holds.
#[derive(Debug)]
enum Entry {
    Delete,
    Fields(BTreeMap<FieldName, FieldValue>),
}

/// The records and written markers collected by a builder.
#[derive(Debug, Default)]
struct Core {
    models: BTreeMap<ModelName, BTreeMap<u64, Entry>>,
    written: BTreeMap<(ModelName, u64), BTreeSet<FieldName>>,
    record_count: usize,
}

fn record_path(model: &str, id: u64) -> String {
    format!("records.{model}[id={id}]")
}

fn lookup_field<'a>(
    schema: &'a StoreSchema,
    model: &str,
    id: u64,
    field: &str,
) -> Result<&'a FieldSchema, WireError> {
    let model_schema = schema.model(model).ok_or_else(|| WireError::UnknownModel {
        path: format!("records.{model}"),
        model: model.to_owned(),
    })?;
    model_schema
        .field(field)
        .ok_or_else(|| WireError::UnknownField {
            path: format!("{}.{field}", record_path(model, id)),
            field: field.to_owned(),
        })
}

impl Core {
    /// Make room for a new record entry, or refuse by limit.
    fn admit(&mut self, limits: &WireLimits, model: &ModelName, id: u64) -> Result<(), WireError> {
        let known = self.models.get(model).is_some_and(|m| m.contains_key(&id));
        if known {
            return Ok(());
        }
        if !self.models.contains_key(model) {
            limits.check(Limit::Models, self.models.len() + 1, "records")?;
        }
        limits.check(Limit::Records, self.record_count + 1, "records")?;
        self.record_count += 1;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn set(
        &mut self,
        schema: &StoreSchema,
        limits: &WireLimits,
        frame: bool,
        model: &str,
        id: u64,
        field: &str,
        value: FieldValue,
    ) -> Result<(), WireError> {
        let rec = record_path(model, id);
        if schema.model(model).is_none() {
            return Err(WireError::UnknownModel {
                path: format!("records.{model}"),
                model: model.to_owned(),
            });
        }
        check_id(id, &|| rec.clone())?;
        let field_schema = lookup_field(schema, model, id, field)?;
        let path = format!("{rec}.{field}");
        let value = validate(schema, limits, frame, field_schema, value, &path)?;
        let (model_name, field_name) = (ModelName::new(model)?, FieldName::new(field)?);
        if let Some(Entry::Delete) = self.models.get(&model_name).and_then(|m| m.get(&id)) {
            return Err(WireError::DeleteWithFields { path: rec });
        }
        if let Some(Entry::Fields(fields)) = self.models.get(&model_name).and_then(|m| m.get(&id))
            && fields.contains_key(&field_name)
        {
            return Err(WireError::DuplicateField { path });
        }
        self.admit(limits, &model_name, id)?;
        let entry = self
            .models
            .entry(model_name)
            .or_default()
            .entry(id)
            .or_insert_with(|| Entry::Fields(BTreeMap::new()));
        if let Entry::Fields(fields) = entry {
            fields.insert(field_name, value);
        }
        Ok(())
    }

    fn delete(
        &mut self,
        schema: &StoreSchema,
        limits: &WireLimits,
        model: &str,
        id: u64,
    ) -> Result<(), WireError> {
        let rec = record_path(model, id);
        if schema.model(model).is_none() {
            return Err(WireError::UnknownModel {
                path: format!("records.{model}"),
                model: model.to_owned(),
            });
        }
        check_id(id, &|| rec.clone())?;
        let model_name = ModelName::new(model)?;
        match self.models.get(&model_name).and_then(|m| m.get(&id)) {
            Some(Entry::Fields(_)) => return Err(WireError::DeleteWithFields { path: rec }),
            Some(Entry::Delete) => return Ok(()),
            None => {}
        }
        self.admit(limits, &model_name, id)?;
        self.models
            .entry(model_name)
            .or_default()
            .insert(id, Entry::Delete);
        Ok(())
    }

    fn write_records(&self, out: &mut String) -> Result<(), WireError> {
        out.push('{');
        for (i, (model, entries)) in self.models.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_string(out, model.as_str());
            out.push_str(":[");
            for (j, (id, entry)) in entries.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                match entry {
                    Entry::Delete => out.push_str(&format!("{{\"delete\":true,\"id\":{id}}}")),
                    Entry::Fields(fields) => {
                        // Keys ascending, with "id" among them ("id" is a reserved field name).
                        let mut members: Vec<(&str, Option<&FieldValue>)> = vec![("id", None)];
                        members.extend(fields.iter().map(|(n, v)| (n.as_str(), Some(v))));
                        members.sort_by(|a, b| a.0.cmp(b.0));
                        out.push('{');
                        for (k, (name, value)) in members.into_iter().enumerate() {
                            if k > 0 {
                                out.push(',');
                            }
                            write_string(out, name);
                            out.push(':');
                            match value {
                                None => out.push_str(&id.to_string()),
                                Some(v) => {
                                    let path =
                                        || format!("{}.{name}", record_path(model.as_str(), *id));
                                    write_value(out, v, &path)?;
                                }
                            }
                        }
                        out.push('}');
                    }
                }
            }
            out.push(']');
        }
        out.push('}');
        Ok(())
    }

    fn write_written(&self, out: &mut String) -> Result<(), WireError> {
        if self.written.is_empty() {
            return Ok(());
        }
        out.push_str(",\"written\":{");
        let mut current: Option<&ModelName> = None;
        for ((model, id), names) in &self.written {
            let set = match self.models.get(model).and_then(|m| m.get(id)) {
                Some(Entry::Fields(fields)) => fields,
                _ => {
                    return Err(WireError::BadWritten {
                        path: format!("written.{model}.{id}"),
                        reason: "record is not in the payload".to_owned(),
                    });
                }
            };
            if let Some(name) = names.iter().find(|n| !set.contains_key(*n)) {
                return Err(WireError::BadWritten {
                    path: format!("written.{model}.{id}.{name}"),
                    reason: "field is not set by the entry".to_owned(),
                });
            }
            if current != Some(model) {
                if current.is_some() {
                    out.push_str("},");
                }
                write_string(out, model.as_str());
                out.push_str(":{");
                current = Some(model);
            } else {
                out.push(',');
            }
            out.push_str(&format!("\"{id}\":["));
            for (k, name) in names.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                write_string(out, name.as_str());
            }
            out.push(']');
        }
        out.push_str("}}");
        Ok(())
    }
}

fn mismatch(path: &str, expected: &'static str) -> WireError {
    WireError::TypeMismatch {
        path: path.to_owned(),
        expected,
    }
}

fn not_allowed(path: &str, reason: &str) -> WireError {
    WireError::OperationNotAllowed {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Check `value` against the field's kind, sync class and the payload mode; returns the
/// canonical form (id lists ascending).
fn validate(
    schema: &StoreSchema,
    limits: &WireLimits,
    frame: bool,
    field: &FieldSchema,
    value: FieldValue,
    path: &str,
) -> Result<FieldValue, WireError> {
    let at = || path.to_owned();
    if frame && field.sync() == SyncClass::AnswerOnly {
        return Err(not_allowed(
            path,
            "answer_only field is not allowed in a frame",
        ));
    }
    let kind = field.kind();
    match (kind, value) {
        (FieldKind::Bool, v @ FieldValue::Bool(_)) => Ok(v),
        (FieldKind::Int, FieldValue::Int(n)) => check_safe_int(n, &at).map(|()| FieldValue::Int(n)),
        (FieldKind::Float, FieldValue::Float(f)) => {
            check_float(f, &at).map(|()| FieldValue::Float(f))
        }
        (FieldKind::String, FieldValue::String(s)) => {
            check_string(&s, limits, &at).map(|()| FieldValue::String(s))
        }
        (FieldKind::Html, FieldValue::Html(s)) => {
            check_string(&s, limits, &at).map(|()| FieldValue::Html(s))
        }
        (FieldKind::Selection, FieldValue::Selection(s)) => {
            check_string(&s, limits, &at).map(|()| FieldValue::Selection(s))
        }
        (FieldKind::Date, FieldValue::Date(s)) => check_date(&s, &at).map(|()| FieldValue::Date(s)),
        (FieldKind::Datetime, FieldValue::Datetime(s)) => {
            check_datetime(&s, &at).map(|()| FieldValue::Datetime(s))
        }
        (FieldKind::Many2One, FieldValue::Many2One(id)) => {
            if let Some(id) = id {
                check_id(id, &at)?;
            }
            Ok(FieldValue::Many2One(id))
        }
        (FieldKind::Reference, FieldValue::Reference(pair)) => {
            if let Some(pair) = &pair
                && schema.model(pair.model().as_str()).is_none()
            {
                return Err(WireError::UnknownModel {
                    path: format!("{path}.model"),
                    model: pair.model().as_str().to_owned(),
                });
            }
            Ok(FieldValue::Reference(pair))
        }
        (FieldKind::X2Many, FieldValue::X2ManyReplace(ids)) => {
            if frame {
                return Err(not_allowed(
                    path,
                    "x2many replace is not allowed in a frame",
                ));
            }
            normalise_ids(&ids, limits, &at).map(FieldValue::X2ManyReplace)
        }
        (FieldKind::X2Many, FieldValue::X2ManyDelta { add, remove }) => {
            if add.is_empty() && remove.is_empty() {
                return Err(WireError::EmptyOperation { path: at() });
            }
            let add = normalise_ids(&add, limits, &|| format!("{path}.add"))?;
            let remove = normalise_ids(&remove, limits, &|| format!("{path}.remove"))?;
            if let Some(id) = add.iter().find(|id| remove.binary_search(id).is_ok()) {
                return Err(WireError::AddRemoveOverlap {
                    path: at(),
                    id: *id,
                });
            }
            Ok(FieldValue::X2ManyDelta { add, remove })
        }
        (FieldKind::Counter, FieldValue::Counter(n)) => {
            if frame {
                return Err(not_allowed(path, "a frame carries counters as inc deltas"));
            }
            check_safe_int(n, &at)?;
            if let Some(cap) = field.cap()
                && u64::try_from(n).is_ok_and(|n| n > cap)
            {
                return Err(WireError::ValueOutOfRange {
                    path: at(),
                    reason: format!("counter value is above its cap {cap}"),
                });
            }
            Ok(FieldValue::Counter(n))
        }
        (FieldKind::Counter, FieldValue::Inc(n)) => {
            if !frame {
                return Err(not_allowed(path, "inc is only allowed in a bus frame"));
            }
            if n == 0 {
                return Err(WireError::ZeroDelta { path: at() });
            }
            check_safe_int(n, &at).map(|()| FieldValue::Inc(n))
        }
        (_, FieldValue::Inc(_)) => Err(not_allowed(path, "inc is only allowed on a counter field")),
        (_, FieldValue::X2ManyDelta { .. }) => Err(not_allowed(
            path,
            "add and remove are only allowed on an x2many field",
        )),
        (kind, _) => Err(mismatch(path, kind.name())),
    }
}

fn write_value(
    out: &mut String,
    value: &FieldValue,
    path: &dyn Fn() -> String,
) -> Result<(), WireError> {
    match value {
        FieldValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        FieldValue::Int(n) | FieldValue::Counter(n) => out.push_str(&n.to_string()),
        FieldValue::Float(f) => write_float(out, *f, path)?,
        FieldValue::String(s)
        | FieldValue::Html(s)
        | FieldValue::Date(s)
        | FieldValue::Datetime(s)
        | FieldValue::Selection(s) => write_string(out, s),
        FieldValue::Many2One(id) => match id {
            Some(id) => out.push_str(&id.to_string()),
            None => out.push_str("null"),
        },
        FieldValue::Reference(pair) => match pair {
            Some(pair) => {
                out.push_str(&format!("{{\"id\":{},\"model\":", pair.id()));
                write_string(out, pair.model().as_str());
                out.push('}');
            }
            None => out.push_str("null"),
        },
        FieldValue::X2ManyReplace(ids) => write_ids(out, ids),
        FieldValue::X2ManyDelta { add, remove } => {
            out.push('{');
            if !add.is_empty() {
                out.push_str("\"add\":");
                write_ids(out, add);
            }
            if !remove.is_empty() {
                if !add.is_empty() {
                    out.push(',');
                }
                out.push_str("\"remove\":");
                write_ids(out, remove);
            }
            out.push('}');
        }
        FieldValue::Inc(n) => out.push_str(&format!("{{\"inc\":{n}}}")),
    }
    Ok(())
}
