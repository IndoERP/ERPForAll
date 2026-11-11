//! The strict decoder (spec §4.2, §4.4 payload order).

use std::collections::BTreeSet;

use crate::cursor::BusCursor;
use crate::error::WireError;
use crate::json::{Json, parse};
use crate::limits::{Limit, WireLimits};
use crate::payload::{FieldValue, RecordRef};
use crate::schema::{
    FieldKind, FieldName, FieldSchema, ModelName, ModelSchema, SchemaBuilder, StoreSchema,
    SyncClass, SyncDecl, make_field,
};
use crate::snapshot::SnapshotDescriptor;
use crate::value::{
    MAX_SAFE, check_date, check_datetime, check_id, check_safe_int, check_string, normalise_ids,
};
use crate::version::{SchemaGeneration, WireMajor};

/// The result of a decode that did not fail.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Outcome<T> {
    /// The payload decoded.
    Decoded(T),
    /// The payload names a schema generation the decoder does not hold; no operations yielded.
    NeedsSchema {
        /// The generation to fetch.
        generation: SchemaGeneration,
    },
}

/// One decoded field.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedField {
    name: FieldName,
    value: FieldValue,
    written: bool,
}

impl DecodedField {
    /// The field name.
    #[must_use]
    pub fn name(&self) -> &FieldName {
        &self.name
    }

    /// The value, of the variant the schema kind names.
    #[must_use]
    pub fn value(&self) -> &FieldValue {
        &self.value
    }

    /// Whether the payload's `written` marker names this field.
    #[must_use]
    pub fn written(&self) -> bool {
        self.written
    }
}

/// One decoded record entry that sets fields.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedRecord {
    model: ModelName,
    id: u64,
    fields: Vec<DecodedField>,
}

impl DecodedRecord {
    /// The model.
    #[must_use]
    pub fn model(&self) -> &ModelName {
        &self.model
    }

    /// The record id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The fields in ascending name order.
    #[must_use]
    pub fn fields(&self) -> &[DecodedField] {
        &self.fields
    }
}

/// A decoded answer or frame in canonical order: upserts by (model, id), then deletes by
/// (model, id). A consumer applies the upserts before the deletes.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedPayload {
    generation: SchemaGeneration,
    asof: SnapshotDescriptor,
    cursor: Option<BusCursor>,
    upserts: Vec<DecodedRecord>,
    deletes: Vec<RecordRef>,
}

impl DecodedPayload {
    /// The schema generation of the payload.
    #[must_use]
    pub fn generation(&self) -> &SchemaGeneration {
        &self.generation
    }

    /// The snapshot descriptor.
    #[must_use]
    pub fn asof(&self) -> &SnapshotDescriptor {
        &self.asof
    }

    /// The bus cursor of a frame; `None` for an answer.
    #[must_use]
    pub fn cursor(&self) -> Option<&BusCursor> {
        self.cursor.as_ref()
    }

    /// The record entries that set fields.
    #[must_use]
    pub fn upserts(&self) -> &[DecodedRecord] {
        &self.upserts
    }

    /// The records deleted by the payload.
    #[must_use]
    pub fn deletes(&self) -> &[RecordRef] {
        &self.deletes
    }
}

/// Decodes payloads against the schema it holds, under its limits.
#[derive(Debug, Clone)]
pub struct Decoder {
    schema: Option<StoreSchema>,
    limits: WireLimits,
}

impl Decoder {
    /// A decoder with no schema yet.
    #[must_use]
    pub fn new(limits: WireLimits) -> Self {
        Self {
            schema: None,
            limits,
        }
    }

    /// Hold `schema`.
    #[must_use]
    pub fn with_schema(mut self, schema: StoreSchema) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Replace the held schema.
    pub fn set_schema(&mut self, schema: StoreSchema) {
        self.schema = Some(schema);
    }

    /// Decode a schema payload.
    ///
    /// # Errors
    /// [`WireError::WireMajorUnsupported`], [`WireError::Malformed`], [`WireError::BadSchema`],
    /// [`WireError::BadName`] or [`WireError::LimitExceeded`].
    pub fn decode_schema(&self, bytes: &[u8]) -> Result<StoreSchema, WireError> {
        let doc = parse(bytes, &self.limits)?;
        let generation = header(&doc, &self.limits)?;
        known_members(&doc, "", &["models", "schema", "wire"])?;
        let models = member(&doc, "", "models")?;
        let Json::Object(models) = models else {
            return Err(malformed("models", "expected an object"));
        };
        self.limits.check(Limit::Models, models.len(), "models")?;
        let mut builder = SchemaBuilder::new(generation);
        let mut total_fields = 0_usize;
        for (model, body) in models {
            let mpath = format!("models.{model}");
            let name = ModelName::new_at(model, &mpath)?;
            known_schema_members(body, &mpath, &["fields"])?;
            let fields_path = format!("{mpath}.fields");
            let Json::Object(fields) = member(body, &mpath, "fields")? else {
                return Err(malformed(&fields_path, "expected an object"));
            };
            total_fields = total_fields.saturating_add(fields.len());
            self.limits
                .check(Limit::SchemaFields, total_fields, &fields_path)?;
            let mut schema = ModelSchema::default();
            for (field, spec) in fields {
                let fpath = format!("{fields_path}.{field}");
                let field_name = FieldName::new_at(field, &fpath)?;
                schema.insert(field_name, decode_field(spec, &fpath)?);
            }
            builder.insert_model(name, schema);
        }
        builder.finish()
    }

    /// Decode an answer payload.
    ///
    /// # Errors
    /// Any refusal of the decode contract, see [`WireError`].
    pub fn decode_answer(&self, bytes: &[u8]) -> Result<Outcome<DecodedPayload>, WireError> {
        self.decode_payload(bytes, false)
    }

    /// Decode a bus frame.
    ///
    /// # Errors
    /// Any refusal of the decode contract, see [`WireError`].
    pub fn decode_frame(&self, bytes: &[u8]) -> Result<Outcome<DecodedPayload>, WireError> {
        self.decode_payload(bytes, true)
    }

    fn decode_payload(
        &self,
        bytes: &[u8],
        frame: bool,
    ) -> Result<Outcome<DecodedPayload>, WireError> {
        let doc = parse(bytes, &self.limits)?;
        let generation = header(&doc, &self.limits)?;
        let Some(schema) = self
            .schema
            .as_ref()
            .filter(|s| *s.generation() == generation)
        else {
            return Ok(Outcome::NeedsSchema { generation });
        };
        let allowed: &[&str] = if frame {
            &["asof", "cursor", "records", "schema", "wire"]
        } else {
            &["asof", "records", "schema", "wire", "written"]
        };
        known_members(&doc, "", allowed)?;
        let asof = decode_asof(member(&doc, "", "asof")?, &self.limits)?;
        let cursor = if frame {
            let cursor = decode_cursor(member(&doc, "", "cursor")?)?;
            if asof.own() != Some(cursor.xid()) {
                return Err(WireError::BadDescriptor {
                    path: "asof.own".to_owned(),
                    reason: "own xid must equal the cursor xid".to_owned(),
                });
            }
            Some(cursor)
        } else {
            None
        };
        let (mut upserts, mut deletes) =
            self.decode_records(schema, member(&doc, "", "records")?, frame)?;
        if let Some(written) = doc.get("written") {
            apply_written(&mut upserts, written, asof.own().is_some())?;
        }
        upserts.sort_by(|a, b| (&a.model, a.id).cmp(&(&b.model, b.id)));
        deletes.sort();
        Ok(Outcome::Decoded(DecodedPayload {
            generation,
            asof,
            cursor,
            upserts,
            deletes,
        }))
    }

    fn decode_records(
        &self,
        schema: &StoreSchema,
        records: &Json,
        frame: bool,
    ) -> Result<(Vec<DecodedRecord>, Vec<RecordRef>), WireError> {
        let Json::Object(models) = records else {
            return Err(malformed("records", "expected an object"));
        };
        self.limits.check(Limit::Models, models.len(), "records")?;
        let mut total = 0_usize;
        for (_, list) in models {
            if let Json::Array(list) = list {
                total = total.saturating_add(list.len());
            }
        }
        self.limits.check(Limit::Records, total, "records")?;
        let (mut upserts, mut deletes) = (Vec::new(), Vec::new());
        for (model, list) in models {
            let mpath = format!("records.{model}");
            let model_schema = schema.model(model).ok_or_else(|| WireError::UnknownModel {
                path: mpath.clone(),
                model: model.clone(),
            })?;
            let model_name = ModelName::new_at(model, &mpath)?;
            let Json::Array(list) = list else {
                return Err(malformed(&mpath, "expected an array"));
            };
            let mut seen = BTreeSet::new();
            for (i, entry) in list.iter().enumerate() {
                let epath = format!("{mpath}[{i}]");
                let Json::Object(members) = entry else {
                    return Err(malformed(&epath, "expected an object"));
                };
                let id = decode_id(member(entry, &epath, "id")?, &format!("{epath}.id"))?;
                if !seen.insert(id) {
                    return Err(WireError::DuplicateId {
                        path: format!("{epath}.id"),
                        id,
                    });
                }
                if let Some(marker) = entry.get("delete") {
                    if *marker != Json::Bool(true) {
                        return Err(malformed(&format!("{epath}.delete"), "expected true"));
                    }
                    if members.len() > 2 {
                        return Err(WireError::DeleteWithFields { path: epath });
                    }
                    deletes.push(RecordRef::from_parts(model_name.clone(), id));
                    continue;
                }
                let mut fields = Vec::with_capacity(members.len().saturating_sub(1));
                for (key, value) in members.iter().filter(|(k, _)| k != "id") {
                    let fpath = format!("{epath}.{key}");
                    let Some(field_schema) = model_schema.field(key) else {
                        return Err(WireError::UnknownField {
                            path: fpath,
                            field: key.clone(),
                        });
                    };
                    let decoded =
                        decode_value(schema, &self.limits, frame, field_schema, value, &fpath)?;
                    fields.push(DecodedField {
                        name: FieldName::new_at(key, &fpath)?,
                        value: decoded,
                        written: false,
                    });
                }
                fields.sort_by(|a, b| a.name.cmp(&b.name));
                upserts.push(DecodedRecord {
                    model: model_name.clone(),
                    id,
                    fields,
                });
            }
        }
        Ok((upserts, deletes))
    }
}

fn malformed(path: &str, reason: &str) -> WireError {
    WireError::Malformed {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

fn member<'a>(doc: &'a Json, base: &str, key: &str) -> Result<&'a Json, WireError> {
    doc.get(key).ok_or_else(|| {
        let path = if base.is_empty() {
            key.to_owned()
        } else {
            format!("{base}.{key}")
        };
        malformed(&path, "missing member")
    })
}

fn known_members(doc: &Json, base: &str, allowed: &[&str]) -> Result<(), WireError> {
    let Json::Object(members) = doc else {
        return Err(malformed(base, "expected an object"));
    };
    for (key, _) in members {
        if !allowed.contains(&key.as_str()) {
            let path = if base.is_empty() {
                key.clone()
            } else {
                format!("{base}.{key}")
            };
            return Err(malformed(&path, "unknown member"));
        }
    }
    Ok(())
}

/// Like [`known_members`] for the schema, whose unknown members are schema errors.
fn known_schema_members(doc: &Json, base: &str, allowed: &[&str]) -> Result<(), WireError> {
    let Json::Object(members) = doc else {
        return Err(WireError::BadSchema {
            path: base.to_owned(),
            reason: "expected an object".to_owned(),
        });
    };
    for (key, _) in members {
        if !allowed.contains(&key.as_str()) {
            return Err(WireError::BadSchema {
                path: format!("{base}.{key}"),
                reason: "unknown member".to_owned(),
            });
        }
    }
    Ok(())
}

/// Check `wire` first (a foreign major is refused before any content error), then `schema`.
fn header(doc: &Json, limits: &WireLimits) -> Result<SchemaGeneration, WireError> {
    if !matches!(doc, Json::Object(_)) {
        return Err(malformed("", "expected an object"));
    }
    let Json::Number(wire) = member(doc, "", "wire")? else {
        return Err(malformed("wire", "expected an integer"));
    };
    let Some(wire) = wire.as_u64() else {
        return Err(malformed("wire", "expected an integer"));
    };
    if wire != u64::from(WireMajor::CURRENT.get()) {
        return Err(WireError::WireMajorUnsupported { found: wire });
    }
    let Json::String(token) = member(doc, "", "schema")? else {
        return Err(malformed("schema", "expected a string"));
    };
    SchemaGeneration::new_with_limits(token, limits)
}

fn string_of<'a>(value: &'a Json, path: &str) -> Result<&'a str, WireError> {
    match value {
        Json::String(s) => Ok(s),
        _ => Err(malformed(path, "expected a string")),
    }
}

fn decode_asof(value: &Json, limits: &WireLimits) -> Result<SnapshotDescriptor, WireError> {
    known_members(value, "asof", &["own", "xip", "xmax", "xmin"])?;
    let xmin = string_of(member(value, "asof", "xmin")?, "asof.xmin")?;
    let xmax = string_of(member(value, "asof", "xmax")?, "asof.xmax")?;
    let Json::Array(list) = member(value, "asof", "xip")? else {
        return Err(malformed("asof.xip", "expected an array"));
    };
    limits.check(Limit::Xip, list.len(), "asof.xip")?;
    let mut xip = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        xip.push(string_of(item, &format!("asof.xip[{i}]"))?);
    }
    let own = match member(value, "asof", "own")? {
        Json::Null => None,
        other => Some(string_of(other, "asof.own")?),
    };
    SnapshotDescriptor::from_wire_strings(xmin, xmax, &xip, own, limits)
}

fn decode_cursor(value: &Json) -> Result<BusCursor, WireError> {
    known_members(value, "cursor", &["id", "xid"])?;
    let xid = string_of(member(value, "cursor", "xid")?, "cursor.xid")?;
    let id = string_of(member(value, "cursor", "id")?, "cursor.id")?;
    BusCursor::from_wire_strings(xid, id)
}

fn decode_id(value: &Json, path: &str) -> Result<u64, WireError> {
    let Json::Number(n) = value else {
        return Err(malformed(path, "expected an integer"));
    };
    match n.as_u64() {
        Some(id) => check_id(id, &|| path.to_owned()).map(|()| id),
        None => Err(WireError::BadId {
            path: path.to_owned(),
            value: n.to_string(),
        }),
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

fn decode_field(spec: &Json, path: &str) -> Result<FieldSchema, WireError> {
    let bad = |p: &str, reason: &str| WireError::BadSchema {
        path: p.to_owned(),
        reason: reason.to_owned(),
    };
    known_schema_members(spec, path, &["cap", "kind", "sync", "target"])?;
    let kind = match spec.get("kind") {
        Some(Json::String(name)) => FieldKind::from_name(name)
            .ok_or_else(|| bad(&format!("{path}.kind"), &format!("unknown kind {name}")))?,
        Some(_) => return Err(bad(&format!("{path}.kind"), "kind must be a string")),
        None => return Err(bad(&format!("{path}.kind"), "kind is required")),
    };
    let target = match spec.get("target") {
        None => None,
        Some(Json::String(t)) => Some(t.as_str()),
        Some(_) => return Err(bad(&format!("{path}.target"), "target must be a string")),
    };
    let cap = match spec.get("cap") {
        None => None,
        Some(Json::Number(n)) => Some(
            n.as_u64()
                .ok_or_else(|| bad(&format!("{path}.cap"), "cap must be 1..=2^53-1"))?,
        ),
        Some(_) => return Err(bad(&format!("{path}.cap"), "cap must be an integer")),
    };
    let sync = match spec.get("sync") {
        None => SyncDecl::Unset,
        Some(Json::String(s)) if s == "state" => SyncDecl::State,
        Some(Json::String(s)) if s == "answer_only" => SyncDecl::AnswerOnly,
        Some(_) => {
            return Err(bad(
                &format!("{path}.sync"),
                "sync must be state or answer_only",
            ));
        }
    };
    make_field(path, kind, target, sync, cap)
}

const OPS: [&str; 3] = ["add", "remove", "inc"];

/// Refuse an operation object on a kind that is not given its own handling: an unknown key is
/// `unknown_operation`; a known one is `operation_not_allowed` for this kind.
fn refuse_object(members: &[(String, Json)], kind: FieldKind, path: &str) -> WireError {
    if let Some((key, _)) = members.iter().find(|(k, _)| !OPS.contains(&k.as_str())) {
        return WireError::UnknownOperation {
            path: path.to_owned(),
            operation: key.clone(),
        };
    }
    match members.first() {
        Some((key, _)) if key == "inc" => {
            not_allowed(path, "inc is only allowed on a counter field")
        }
        Some(_) => not_allowed(path, "add and remove are only allowed on an x2many field"),
        None => mismatch(path, kind.name()),
    }
}

fn decode_id_list(
    value: &Json,
    limits: &WireLimits,
    path: &str,
    non_empty: bool,
) -> Result<Vec<u64>, WireError> {
    let Json::Array(items) = value else {
        return Err(mismatch(path, "x2many"));
    };
    limits.check(Limit::IdsPerList, items.len(), path)?;
    if non_empty && items.is_empty() {
        return Err(WireError::EmptyOperation {
            path: path.to_owned(),
        });
    }
    let mut ids = Vec::with_capacity(items.len());
    for (j, item) in items.iter().enumerate() {
        ids.push(decode_id(item, &format!("{path}[{j}]"))?);
    }
    normalise_ids(&ids, limits, &|| path.to_owned())
}

fn decode_value(
    schema: &StoreSchema,
    limits: &WireLimits,
    frame: bool,
    field: &FieldSchema,
    value: &Json,
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
    // Operation objects first, for the kinds that do not take an object as their plain value.
    if let Json::Object(members) = value {
        match kind {
            FieldKind::X2Many => return decode_x2many_ops(members, limits, path),
            FieldKind::Counter => return decode_counter_object(members, frame, path),
            FieldKind::Reference => {
                if members.iter().any(|(k, _)| OPS.contains(&k.as_str())) {
                    return Err(refuse_object(members, kind, path));
                }
            }
            _ => return Err(refuse_object(members, kind, path)),
        }
    }
    match (kind, value) {
        (FieldKind::Bool, Json::Bool(b)) => Ok(FieldValue::Bool(*b)),
        (FieldKind::Int, Json::Number(n)) => match (n.as_i64(), n.is_u64()) {
            (Some(n), _) => check_safe_int(n, &at).map(|()| FieldValue::Int(n)),
            (None, true) => Err(WireError::ValueOutOfRange {
                path: at(),
                reason: "integer is outside the safe range +-(2^53-1)".to_owned(),
            }),
            (None, false) => Err(mismatch(path, "int")),
        },
        (FieldKind::Float, Json::Number(n)) => n
            .as_f64()
            .filter(|f| f.is_finite())
            .map(FieldValue::Float)
            .ok_or_else(|| mismatch(path, "float")),
        (FieldKind::String, Json::String(s)) => {
            check_string(s, limits, &at).map(|()| FieldValue::String(s.clone()))
        }
        (FieldKind::Html, Json::String(s)) => {
            check_string(s, limits, &at).map(|()| FieldValue::Html(s.clone()))
        }
        (FieldKind::Selection, Json::String(s)) => {
            check_string(s, limits, &at).map(|()| FieldValue::Selection(s.clone()))
        }
        (FieldKind::Date, Json::String(s)) => {
            check_date(s, &at).map(|()| FieldValue::Date(s.clone()))
        }
        (FieldKind::Datetime, Json::String(s)) => {
            check_datetime(s, &at).map(|()| FieldValue::Datetime(s.clone()))
        }
        (FieldKind::Many2One, Json::Null) => Ok(FieldValue::Many2One(None)),
        (FieldKind::Many2One, Json::Number(_)) => {
            decode_id(value, path).map(|id| FieldValue::Many2One(Some(id)))
        }
        (FieldKind::Reference, Json::Null) => Ok(FieldValue::Reference(None)),
        (FieldKind::Reference, Json::Object(members)) => {
            if members.len() != 2 || value.get("model").is_none() || value.get("id").is_none() {
                return Err(mismatch(path, "reference"));
            }
            let Some(Json::String(model)) = value.get("model") else {
                return Err(mismatch(path, "reference"));
            };
            if schema.model(model).is_none() {
                return Err(WireError::UnknownModel {
                    path: format!("{path}.model"),
                    model: model.clone(),
                });
            }
            let id = decode_id(member(value, path, "id")?, &format!("{path}.id"))?;
            let model = ModelName::new_at(model, &format!("{path}.model"))?;
            Ok(FieldValue::Reference(Some(RecordRef::from_parts(
                model, id,
            ))))
        }
        (FieldKind::X2Many, Json::Array(_)) => {
            if frame {
                return Err(not_allowed(
                    path,
                    "x2many replace is not allowed in a frame",
                ));
            }
            decode_id_list(value, limits, path, false).map(FieldValue::X2ManyReplace)
        }
        (FieldKind::Counter, Json::Number(n)) => {
            if frame {
                return Err(not_allowed(path, "a frame carries counters as inc deltas"));
            }
            let n = n.as_i64().ok_or_else(|| mismatch(path, "counter"))?;
            check_safe_int(n, &at)?;
            if let (Some(cap), Ok(unsigned)) = (field.cap(), u64::try_from(n))
                && unsigned > cap
            {
                return Err(WireError::ValueOutOfRange {
                    path: at(),
                    reason: format!("counter value is above its cap {cap}"),
                });
            }
            Ok(FieldValue::Counter(n))
        }
        (kind, _) => Err(mismatch(path, kind.name())),
    }
}

fn decode_counter_object(
    members: &[(String, Json)],
    frame: bool,
    path: &str,
) -> Result<FieldValue, WireError> {
    if let Some((key, _)) = members.iter().find(|(k, _)| !OPS.contains(&k.as_str())) {
        return Err(WireError::UnknownOperation {
            path: path.to_owned(),
            operation: key.clone(),
        });
    }
    if members.iter().any(|(k, _)| k != "inc") {
        return Err(not_allowed(
            path,
            "add and remove are only allowed on an x2many field",
        ));
    }
    if !frame {
        return Err(not_allowed(path, "inc is only allowed in a bus frame"));
    }
    let [(_, Json::Number(n))] = members else {
        return Err(mismatch(path, "counter"));
    };
    let n = n.as_i64().ok_or_else(|| mismatch(path, "counter"))?;
    if n == 0 {
        return Err(WireError::ZeroDelta {
            path: path.to_owned(),
        });
    }
    check_safe_int(n, &|| path.to_owned())?;
    Ok(FieldValue::Inc(n))
}

fn decode_x2many_ops(
    members: &[(String, Json)],
    limits: &WireLimits,
    path: &str,
) -> Result<FieldValue, WireError> {
    if let Some((key, _)) = members.iter().find(|(k, _)| !OPS.contains(&k.as_str())) {
        return Err(WireError::UnknownOperation {
            path: path.to_owned(),
            operation: key.clone(),
        });
    }
    if members.iter().any(|(k, _)| k == "inc") {
        return Err(not_allowed(path, "inc is only allowed on a counter field"));
    }
    if members.is_empty() {
        return Err(WireError::EmptyOperation {
            path: path.to_owned(),
        });
    }
    let list = |key: &str| -> Result<Vec<u64>, WireError> {
        match members.iter().find(|(k, _)| k == key) {
            Some((_, v)) => decode_id_list(v, limits, &format!("{path}.{key}"), true),
            None => Ok(Vec::new()),
        }
    };
    let (add, remove) = (list("add")?, list("remove")?);
    if let Some(id) = add.iter().find(|id| remove.binary_search(id).is_ok()) {
        return Err(WireError::AddRemoveOverlap {
            path: path.to_owned(),
            id: *id,
        });
    }
    Ok(FieldValue::X2ManyDelta { add, remove })
}

/// Mark the fields named by a `written` member, refusing a marker that does not describe the
/// payload.
fn apply_written(
    upserts: &mut [DecodedRecord],
    written: &Json,
    has_own: bool,
) -> Result<(), WireError> {
    let bad = |path: &str, reason: &str| WireError::BadWritten {
        path: path.to_owned(),
        reason: reason.to_owned(),
    };
    if !has_own {
        return Err(bad("written", "answer has no own xid"));
    }
    let Json::Object(models) = written else {
        return Err(malformed("written", "expected an object"));
    };
    for (model, ids) in models {
        let mpath = format!("written.{model}");
        if !upserts.iter().any(|r| r.model.as_str() == model) {
            return Err(bad(&mpath, "model is not in the payload"));
        }
        let Json::Object(ids) = ids else {
            return Err(malformed(&mpath, "expected an object"));
        };
        for (id_text, names) in ids {
            let ipath = format!("{mpath}.{id_text}");
            let id = crate::xid::parse_decimal(id_text)
                .ok()
                .filter(|id| *id >= 1 && *id <= MAX_SAFE as u64)
                .ok_or_else(|| bad(&ipath, "not a record id"))?;
            let Some(record) = upserts
                .iter_mut()
                .find(|r| r.model.as_str() == model && r.id == id)
            else {
                return Err(bad(&ipath, "record is not in the payload"));
            };
            let Json::Array(names) = names else {
                return Err(malformed(&ipath, "expected an array"));
            };
            let mut seen = BTreeSet::new();
            for (j, name) in names.iter().enumerate() {
                let npath = format!("{ipath}[{j}]");
                let name = string_of(name, &npath)?;
                if !seen.insert(name) {
                    return Err(WireError::DuplicateField { path: npath });
                }
                let Some(field) = record.fields.iter_mut().find(|f| f.name.as_str() == name) else {
                    return Err(bad(&npath, "field is not set by the entry"));
                };
                if matches!(field.value, FieldValue::Counter(_) | FieldValue::Inc(_)) {
                    return Err(bad(&npath, "counter fields cannot be written markers"));
                }
                field.written = true;
            }
        }
    }
    Ok(())
}
