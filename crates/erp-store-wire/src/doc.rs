//! Plain data documents of the wire contract: the shapes the TypeScript side reads.
//!
//! These types carry no validation. They describe, field for field, the JSON the generated
//! TypeScript declarations (`web/packages/store-wire/src/generated`) and the golden fixture file
//! (`web/packages/store-wire/tests/generated-fixtures.json`) are made of. Every value in a fixture
//! comes from the validated types of this crate converted through the `From` impls below, never
//! from a typed literal. With the `schema-gen` feature the types also derive the declarations;
//! production builds never enable it.
//!
//! Integers that the wire keeps below 2^53 are declared `number`; transaction ids stay decimal
//! strings.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::decode::{DecodedField, DecodedPayload, DecodedRecord, Outcome};
use crate::error::WireError;
use crate::limits::Limit;
use crate::payload::{FieldValue, RecordRef};
use crate::schema::{FieldKind, FieldSchema, StoreSchema, SyncClass};
use crate::snapshot::SnapshotDescriptor;

/// Derive the TypeScript declaration into its own file when the generator feature is on.
macro_rules! wire_doc {
    ($file:literal, $item:item) => {
        #[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
        #[cfg_attr(feature = "schema-gen", ts(export_to = $file))]
        $item
    };
}

wire_doc! {
    "FieldKindDoc.d.ts",
    /// The wire name of a field kind (`FieldKind::name`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "lowercase")]
    pub enum FieldKindDoc {
        /// `bool`.
        Bool,
        /// `int`.
        Int,
        /// `float`.
        Float,
        /// `string`.
        String,
        /// `html`.
        Html,
        /// `date`.
        Date,
        /// `datetime`.
        Datetime,
        /// `selection`.
        Selection,
        /// `many2one`.
        Many2one,
        /// `reference`.
        Reference,
        /// `x2many`.
        X2many,
        /// `counter`.
        Counter,
    }
}

impl From<FieldKind> for FieldKindDoc {
    fn from(kind: FieldKind) -> Self {
        match kind {
            FieldKind::Bool => Self::Bool,
            FieldKind::Int => Self::Int,
            FieldKind::Float => Self::Float,
            FieldKind::String => Self::String,
            FieldKind::Html => Self::Html,
            FieldKind::Date => Self::Date,
            FieldKind::Datetime => Self::Datetime,
            FieldKind::Selection => Self::Selection,
            FieldKind::Many2One => Self::Many2one,
            FieldKind::Reference => Self::Reference,
            FieldKind::X2Many => Self::X2many,
            FieldKind::Counter => Self::Counter,
        }
    }
}

wire_doc! {
    "SyncClassDoc.d.ts",
    /// How a field's values reach the client (`SyncClass`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SyncClassDoc {
        /// Absolute values.
        State,
        /// Integer deltas.
        Counter,
        /// Values only from answers.
        AnswerOnly,
    }
}

impl From<SyncClass> for SyncClassDoc {
    fn from(sync: SyncClass) -> Self {
        match sync {
            SyncClass::State => Self::State,
            SyncClass::Counter => Self::Counter,
            SyncClass::AnswerOnly => Self::AnswerOnly,
        }
    }
}

wire_doc! {
    "ErrorCodeDoc.d.ts",
    /// The stable machine code of a refusal (`WireError::code`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ErrorCodeDoc {
        /// `type_mismatch`.
        TypeMismatch,
        /// `value_out_of_range`.
        ValueOutOfRange,
        /// `duplicate_id`.
        DuplicateId,
        /// `duplicate_field`.
        DuplicateField,
        /// `add_remove_overlap`.
        AddRemoveOverlap,
        /// `empty_operation`.
        EmptyOperation,
        /// `operation_not_allowed`.
        OperationNotAllowed,
        /// `zero_delta`.
        ZeroDelta,
        /// `unknown_model`.
        UnknownModel,
        /// `unknown_field`.
        UnknownField,
        /// `unknown_operation`.
        UnknownOperation,
        /// `bad_id`.
        BadId,
        /// `delete_with_fields`.
        DeleteWithFields,
        /// `bad_written`.
        BadWritten,
        /// `malformed`.
        Malformed,
        /// `wire_major_unsupported`.
        WireMajorUnsupported,
        /// `client_outdated`.
        ClientOutdated,
        /// `bad_descriptor`.
        BadDescriptor,
        /// `bad_schema`.
        BadSchema,
        /// `bad_name`.
        BadName,
        /// `limit_exceeded`.
        LimitExceeded,
        /// `bad_limit`.
        BadLimit,
    }
}

impl From<&WireError> for ErrorCodeDoc {
    fn from(error: &WireError) -> Self {
        // Exhaustive on purpose: a new refusal fails to compile until the declaration lists it.
        match error {
            WireError::TypeMismatch { .. } => Self::TypeMismatch,
            WireError::ValueOutOfRange { .. } => Self::ValueOutOfRange,
            WireError::DuplicateId { .. } => Self::DuplicateId,
            WireError::DuplicateField { .. } => Self::DuplicateField,
            WireError::AddRemoveOverlap { .. } => Self::AddRemoveOverlap,
            WireError::EmptyOperation { .. } => Self::EmptyOperation,
            WireError::OperationNotAllowed { .. } => Self::OperationNotAllowed,
            WireError::ZeroDelta { .. } => Self::ZeroDelta,
            WireError::UnknownModel { .. } => Self::UnknownModel,
            WireError::UnknownField { .. } => Self::UnknownField,
            WireError::UnknownOperation { .. } => Self::UnknownOperation,
            WireError::BadId { .. } => Self::BadId,
            WireError::DeleteWithFields { .. } => Self::DeleteWithFields,
            WireError::BadWritten { .. } => Self::BadWritten,
            WireError::Malformed { .. } => Self::Malformed,
            WireError::WireMajorUnsupported { .. } => Self::WireMajorUnsupported,
            WireError::ClientOutdated { .. } => Self::ClientOutdated,
            WireError::BadDescriptor { .. } => Self::BadDescriptor,
            WireError::BadSchema { .. } => Self::BadSchema,
            WireError::BadName { .. } => Self::BadName,
            WireError::LimitExceeded { .. } => Self::LimitExceeded,
            WireError::BadLimit { .. } => Self::BadLimit,
        }
    }
}

wire_doc! {
    "LimitDoc.d.ts",
    /// The name of a bounded quantity (`Limit::name`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum LimitDoc {
        /// `bytes`.
        Bytes,
        /// `depth`.
        Depth,
        /// `models`.
        Models,
        /// `records`.
        Records,
        /// `ids_per_list`.
        IdsPerList,
        /// `xip`.
        Xip,
        /// `string_bytes`.
        StringBytes,
        /// `schema_token_bytes`.
        SchemaTokenBytes,
        /// `schema_fields`.
        SchemaFields,
    }
}

impl From<Limit> for LimitDoc {
    fn from(limit: Limit) -> Self {
        match limit {
            Limit::Bytes => Self::Bytes,
            Limit::Depth => Self::Depth,
            Limit::Models => Self::Models,
            Limit::Records => Self::Records,
            Limit::IdsPerList => Self::IdsPerList,
            Limit::Xip => Self::Xip,
            Limit::StringBytes => Self::StringBytes,
            Limit::SchemaTokenBytes => Self::SchemaTokenBytes,
            Limit::SchemaFields => Self::SchemaFields,
        }
    }
}

wire_doc! {
    "LimitValueDoc.d.ts",
    /// One limit and its value: a compiled-in maximum, or a lowered value for a fixture case.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct LimitValueDoc {
        /// Which limit.
        pub limit: LimitDoc,
        /// Its value.
        #[cfg_attr(feature = "schema-gen", ts(type = "number"))]
        pub value: u64,
    }
}

wire_doc! {
    "RefusalDoc.d.ts",
    /// What a refusal reports: the code, the path into the input and, for a limit, its name.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct RefusalDoc {
        /// The stable machine code.
        pub code: ErrorCodeDoc,
        /// The path into the input, when the refusal has a position.
        pub path: Option<String>,
        /// The limit that was exceeded, for `limit_exceeded`.
        pub limit: Option<LimitDoc>,
    }
}

impl From<&WireError> for RefusalDoc {
    fn from(error: &WireError) -> Self {
        let limit = match error {
            WireError::LimitExceeded { limit, .. } | WireError::BadLimit { limit, .. } => {
                Limit::ALL
                    .into_iter()
                    .find(|candidate| candidate.name() == *limit)
                    .map(LimitDoc::from)
            }
            _ => None,
        };
        Self {
            code: error.into(),
            path: error.path().map(str::to_owned),
            limit,
        }
    }
}

wire_doc! {
    "AsofDoc.d.ts",
    /// A snapshot descriptor as it travels in `asof` (ids are decimal strings).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct AsofDoc {
        /// The transaction's own xid, when it has written.
        pub own: Option<String>,
        /// In-progress xids, strictly ascending, inside `[xmin, xmax)`.
        pub xip: Vec<String>,
        /// First xid not yet assigned.
        pub xmax: String,
        /// Lowest xid still in progress.
        pub xmin: String,
    }
}

impl From<&SnapshotDescriptor> for AsofDoc {
    fn from(asof: &SnapshotDescriptor) -> Self {
        Self {
            own: asof.own().map(|xid| xid.to_string()),
            xip: asof.xip().iter().map(ToString::to_string).collect(),
            xmax: asof.xmax().to_string(),
            xmin: asof.xmin().to_string(),
        }
    }
}

wire_doc! {
    "CursorDoc.d.ts",
    /// A bus frame cursor: decimal strings, ordered numerically by `xid` then `id`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct CursorDoc {
        /// The event id.
        pub id: String,
        /// The emitting transaction's xid.
        pub xid: String,
    }
}

impl From<&crate::cursor::BusCursor> for CursorDoc {
    fn from(cursor: &crate::cursor::BusCursor) -> Self {
        Self {
            id: cursor.id().to_string(),
            xid: cursor.xid().to_string(),
        }
    }
}

wire_doc! {
    "RecordRefDoc.d.ts",
    /// A `{model, id}` pair.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct RecordRefDoc {
        /// The record id.
        #[cfg_attr(feature = "schema-gen", ts(type = "number"))]
        pub id: u64,
        /// The model.
        pub model: String,
    }
}

impl From<&RecordRef> for RecordRefDoc {
    fn from(reference: &RecordRef) -> Self {
        Self {
            id: reference.id(),
            model: reference.model().to_string(),
        }
    }
}

wire_doc! {
    "ValueDoc.d.ts",
    /// A decoded field value, tagged by the variant the schema kind names.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    #[serde(tag = "kind", content = "value", rename_all = "snake_case")]
    pub enum ValueDoc {
        /// `bool`.
        Bool(bool),
        /// `int`.
        Int(#[cfg_attr(feature = "schema-gen", ts(type = "number"))] i64),
        /// `float`.
        Float(f64),
        /// `string`.
        String(String),
        /// `html`.
        Html(String),
        /// `date`.
        Date(String),
        /// `datetime`.
        Datetime(String),
        /// `selection`.
        Selection(String),
        /// `many2one`: an id or null.
        Many2one(#[cfg_attr(feature = "schema-gen", ts(type = "number | null"))] Option<u64>),
        /// `reference`: a pair or null.
        Reference(Option<RecordRefDoc>),
        /// `x2many` replace: the full id set.
        X2manyReplace(#[cfg_attr(feature = "schema-gen", ts(type = "Array<number>"))] Vec<u64>),
        /// `x2many` add/remove.
        X2manyDelta {
            /// Ids added.
            #[cfg_attr(feature = "schema-gen", ts(type = "Array<number>"))]
            add: Vec<u64>,
            /// Ids removed.
            #[cfg_attr(feature = "schema-gen", ts(type = "Array<number>"))]
            remove: Vec<u64>,
        },
        /// `counter` in an answer: the absolute value.
        Counter(#[cfg_attr(feature = "schema-gen", ts(type = "number"))] i64),
        /// `counter` in a frame: a non-zero delta.
        Inc(#[cfg_attr(feature = "schema-gen", ts(type = "number"))] i64),
    }
}

impl From<&FieldValue> for ValueDoc {
    fn from(value: &FieldValue) -> Self {
        match value {
            FieldValue::Bool(v) => Self::Bool(*v),
            FieldValue::Int(v) => Self::Int(*v),
            FieldValue::Float(v) => Self::Float(*v),
            FieldValue::String(v) => Self::String(v.clone()),
            FieldValue::Html(v) => Self::Html(v.clone()),
            FieldValue::Date(v) => Self::Date(v.clone()),
            FieldValue::Datetime(v) => Self::Datetime(v.clone()),
            FieldValue::Selection(v) => Self::Selection(v.clone()),
            FieldValue::Many2One(v) => Self::Many2one(*v),
            FieldValue::Reference(v) => Self::Reference(v.as_ref().map(RecordRefDoc::from)),
            FieldValue::X2ManyReplace(ids) => Self::X2manyReplace(ids.clone()),
            FieldValue::X2ManyDelta { add, remove } => Self::X2manyDelta {
                add: add.clone(),
                remove: remove.clone(),
            },
            FieldValue::Counter(v) => Self::Counter(*v),
            FieldValue::Inc(v) => Self::Inc(*v),
        }
    }
}

wire_doc! {
    "FieldEntryDoc.d.ts",
    /// One decoded field of a record entry.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct FieldEntryDoc {
        /// The field name.
        pub name: String,
        /// The value.
        pub value: ValueDoc,
        /// Whether the payload's `written` marker names this field.
        pub written: bool,
    }
}

impl From<&DecodedField> for FieldEntryDoc {
    fn from(field: &DecodedField) -> Self {
        Self {
            name: field.name().to_string(),
            value: field.value().into(),
            written: field.written(),
        }
    }
}

wire_doc! {
    "RecordDoc.d.ts",
    /// One decoded record entry that sets fields.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct RecordDoc {
        /// The record id.
        #[cfg_attr(feature = "schema-gen", ts(type = "number"))]
        pub id: u64,
        /// The model.
        pub model: String,
        /// The fields, ascending by name.
        pub fields: Vec<FieldEntryDoc>,
    }
}

impl From<&DecodedRecord> for RecordDoc {
    fn from(record: &DecodedRecord) -> Self {
        Self {
            id: record.id(),
            model: record.model().to_string(),
            fields: record.fields().iter().map(FieldEntryDoc::from).collect(),
        }
    }
}

wire_doc! {
    "PayloadDoc.d.ts",
    /// A decoded answer or frame in canonical order: upserts by (model, id), then deletes.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct PayloadDoc {
        /// The snapshot descriptor.
        pub asof: AsofDoc,
        /// The bus cursor of a frame; null for an answer.
        pub cursor: Option<CursorDoc>,
        /// The records deleted by the payload.
        pub deletes: Vec<RecordRefDoc>,
        /// The schema generation of the payload.
        pub generation: String,
        /// The record entries that set fields.
        pub upserts: Vec<RecordDoc>,
    }
}

impl From<&DecodedPayload> for PayloadDoc {
    fn from(payload: &DecodedPayload) -> Self {
        Self {
            asof: payload.asof().into(),
            cursor: payload.cursor().map(CursorDoc::from),
            deletes: payload.deletes().iter().map(RecordRefDoc::from).collect(),
            generation: payload.generation().to_string(),
            upserts: payload.upserts().iter().map(RecordDoc::from).collect(),
        }
    }
}

wire_doc! {
    "SchemaFieldDoc.d.ts",
    /// One field of a decoded schema.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct SchemaFieldDoc {
        /// The saturation cap of a counter.
        #[cfg_attr(feature = "schema-gen", ts(type = "number | null"))]
        pub cap: Option<u64>,
        /// The kind.
        pub kind: FieldKindDoc,
        /// How values reach the client.
        pub sync: SyncClassDoc,
        /// The target model of a many2one or x2many.
        pub target: Option<String>,
    }
}

impl From<&FieldSchema> for SchemaFieldDoc {
    fn from(field: &FieldSchema) -> Self {
        Self {
            cap: field.cap(),
            kind: field.kind().into(),
            sync: field.sync().into(),
            target: field.target().map(ToString::to_string),
        }
    }
}

wire_doc! {
    "SchemaModelDoc.d.ts",
    /// One model of a decoded schema.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct SchemaModelDoc {
        /// Its fields by name.
        pub fields: BTreeMap<String, SchemaFieldDoc>,
    }
}

wire_doc! {
    "SchemaDoc.d.ts",
    /// A decoded store schema.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct SchemaDoc {
        /// The schema generation token.
        pub generation: String,
        /// Its models by name.
        pub models: BTreeMap<String, SchemaModelDoc>,
    }
}

impl From<&StoreSchema> for SchemaDoc {
    fn from(schema: &StoreSchema) -> Self {
        Self {
            generation: schema.generation().to_string(),
            models: schema
                .models()
                .map(|(name, model)| {
                    let fields = model
                        .fields()
                        .map(|(field, spec)| (field.to_string(), SchemaFieldDoc::from(spec)))
                        .collect();
                    (name.to_string(), SchemaModelDoc { fields })
                })
                .collect(),
        }
    }
}

wire_doc! {
    "PayloadOutcomeDoc.d.ts",
    /// The result of decoding one answer or frame.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    #[serde(tag = "outcome", rename_all = "snake_case")]
    pub enum PayloadOutcomeDoc {
        /// The payload decoded.
        Decoded {
            /// Its canonical form.
            payload: PayloadDoc,
        },
        /// The payload names a generation the decoder does not hold.
        NeedsSchema {
            /// The generation to fetch.
            generation: String,
        },
        /// The payload was refused.
        Refused {
            /// The refusal.
            refusal: RefusalDoc,
        },
    }
}

impl PayloadOutcomeDoc {
    /// The outcome of a decode call.
    #[must_use]
    pub fn of(result: &Result<Outcome<DecodedPayload>, WireError>) -> Self {
        match result {
            Ok(Outcome::Decoded(payload)) => Self::Decoded {
                payload: payload.into(),
            },
            Ok(Outcome::NeedsSchema { generation }) => Self::NeedsSchema {
                generation: generation.to_string(),
            },
            Err(error) => Self::Refused {
                refusal: error.into(),
            },
        }
    }
}

wire_doc! {
    "SchemaOutcomeDoc.d.ts",
    /// The result of decoding one schema document.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    #[serde(tag = "outcome", rename_all = "snake_case")]
    pub enum SchemaOutcomeDoc {
        /// The schema decoded.
        Decoded {
            /// Its canonical form.
            schema: SchemaDoc,
        },
        /// The schema was refused.
        Refused {
            /// The refusal.
            refusal: RefusalDoc,
        },
    }
}

impl SchemaOutcomeDoc {
    /// The outcome of a schema decode.
    #[must_use]
    pub fn of(result: &Result<StoreSchema, WireError>) -> Self {
        match result {
            Ok(schema) => Self::Decoded {
                schema: schema.into(),
            },
            Err(error) => Self::Refused {
                refusal: error.into(),
            },
        }
    }
}

wire_doc! {
    "PayloadKindDoc.d.ts",
    /// Whether a fixture payload is decoded as an answer or as a bus frame.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PayloadKindDoc {
        /// An RPC answer.
        Answer,
        /// A bus store frame.
        Frame,
    }
}

wire_doc! {
    "PayloadCaseDoc.d.ts",
    /// One payload fixture: the bytes, the decoder's setup and the Rust result.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct PayloadCaseDoc {
        /// Answer or frame.
        pub kind: PayloadKindDoc,
        /// Limits lowered below their maximum for this case.
        pub limits: Vec<LimitValueDoc>,
        /// Case name, unique in the file.
        pub name: String,
        /// What Rust decoded or refused.
        pub outcome: PayloadOutcomeDoc,
        /// Key into `schemas` of the schema the decoder holds; null when it holds none.
        pub schema: Option<String>,
        /// The payload text.
        pub wire: String,
    }
}

wire_doc! {
    "SchemaCaseDoc.d.ts",
    /// One schema fixture: the bytes and the Rust result.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct SchemaCaseDoc {
        /// Limits lowered below their maximum for this case.
        pub limits: Vec<LimitValueDoc>,
        /// Case name, unique in the file.
        pub name: String,
        /// What Rust decoded or refused.
        pub outcome: SchemaOutcomeDoc,
        /// The schema document text.
        pub wire: String,
    }
}

wire_doc! {
    "ProbeDoc.d.ts",
    /// One reflection probe: whether the descriptor reflects `xid`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct ProbeDoc {
        /// Whether `reflects(xid)` is true.
        pub reflects: bool,
        /// The probed xid.
        pub xid: String,
    }
}

wire_doc! {
    "DescriptorOutcomeDoc.d.ts",
    /// The result of building a descriptor from its wire strings.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    #[serde(tag = "outcome", rename_all = "snake_case")]
    pub enum DescriptorOutcomeDoc {
        /// The descriptor is valid.
        Valid {
            /// Its canonical form.
            asof: AsofDoc,
            /// `xmax - len(xip)`, a decimal string.
            completed_count: String,
            /// Reflection of each probe.
            probes: Vec<ProbeDoc>,
        },
        /// The descriptor was refused.
        Refused {
            /// The refusal.
            refusal: RefusalDoc,
        },
    }
}

wire_doc! {
    "DescriptorCaseDoc.d.ts",
    /// One descriptor fixture.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct DescriptorCaseDoc {
        /// Case name, unique in the file.
        pub name: String,
        /// The result.
        pub outcome: DescriptorOutcomeDoc,
        /// The input as written on the wire.
        pub wire: AsofDoc,
    }
}

wire_doc! {
    "CursorOutcomeDoc.d.ts",
    /// The result of building a cursor from its wire strings.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    #[serde(tag = "outcome", rename_all = "snake_case")]
    pub enum CursorOutcomeDoc {
        /// The cursor is valid.
        Valid {
            /// Its canonical form.
            cursor: CursorDoc,
        },
        /// The cursor was refused.
        Refused {
            /// The refusal.
            refusal: RefusalDoc,
        },
    }
}

wire_doc! {
    "CursorCaseDoc.d.ts",
    /// One cursor fixture.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct CursorCaseDoc {
        /// Case name, unique in the file.
        pub name: String,
        /// The result.
        pub outcome: CursorOutcomeDoc,
        /// The input as written on the wire.
        pub wire: CursorDoc,
    }
}

wire_doc! {
    "CursorOrderDoc.d.ts",
    /// Cursors in an arbitrary order and the same cursors in Rust's numeric order.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct CursorOrderDoc {
        /// Input order.
        pub shuffled: Vec<CursorDoc>,
        /// Ascending by `(xid, id)` as numbers.
        pub sorted: Vec<CursorDoc>,
    }
}

wire_doc! {
    "ReadOrderDoc.d.ts",
    /// Descriptors and the order of their finished counts, which orders reads.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct ReadOrderDoc {
        /// Indices into `descriptors`, ascending by finished count (stable on ties).
        pub ascending: Vec<u32>,
        /// The finished count of each descriptor, decimal strings.
        pub counts: Vec<String>,
        /// The descriptors.
        pub descriptors: Vec<AsofDoc>,
    }
}

wire_doc! {
    "ClientMajorCaseDoc.d.ts",
    /// One client wire major and whether the server accepts it.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct ClientMajorCaseDoc {
        /// The accepted major, when accepted.
        pub accepted: Option<u32>,
        /// The major the client sent.
        #[cfg_attr(feature = "schema-gen", ts(type = "number"))]
        pub client: u64,
        /// The refusal, when refused.
        pub refusal: Option<RefusalDoc>,
    }
}

wire_doc! {
    "FixtureFileDoc.d.ts",
    /// The whole golden fixture file written by the `store_wire_fixtures` example.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct FixtureFileDoc {
        /// Client wire majors.
        pub client_majors: Vec<ClientMajorCaseDoc>,
        /// Cursor cases.
        pub cursor_cases: Vec<CursorCaseDoc>,
        /// Cursor ordering vector.
        pub cursor_order: CursorOrderDoc,
        /// Descriptor cases with reflection vectors.
        pub descriptor_cases: Vec<DescriptorCaseDoc>,
        /// Fixture file format version.
        pub format: u32,
        /// Every limit with its compiled-in maximum.
        pub limits: Vec<LimitValueDoc>,
        /// Payload cases.
        pub payload_cases: Vec<PayloadCaseDoc>,
        /// Finished-count ordering vector.
        pub read_order: ReadOrderDoc,
        /// Schema cases.
        pub schema_cases: Vec<SchemaCaseDoc>,
        /// Encoded schema documents by key.
        pub schemas: BTreeMap<String, String>,
        /// The wire major the file was written for.
        pub wire: u32,
    }
}
