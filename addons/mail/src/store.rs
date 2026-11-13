//! The store payloads of the thread RPC answers (
//! guide).
//!
//! A thread call answers with the records it touched in the generic store wire: the schema below
//! names the models and fields those answers use, [`answer`] takes the snapshot descriptor of the
//! answering transaction (after its last write, producer obligation A1) and encodes the records
//! with the canonical encoder of `erp-store-wire`. The schema is the registry's ((f)):
//! this file contributes the thread models as a schema part, and the generation token is the
//! registry's, so a thread answer and a `store_fetch` answer carry the same one.
//!
//! The thread models are named in the schema so a client can hold a chatter's record; a message and a
//! follower still carry the thread as `res_model` (string) and `res_id` (int), not as the polymorphic
//! reference.2.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use erp_sdk::store_wire::{
    AnswerBuilder, FieldKind, FieldSpec, FieldValue as Wire, StoreSchema, WireError,
};
use erp_sdk::{Env, Error, Result, Transaction, Value, serde_json};

use crate::store_fetch::{SchemaFragment, SchemaPart, TopicRegistry};

/// The fields of a `mail.message` record in a store answer.
pub(crate) const MESSAGE_FIELDS: [&str; 10] = [
    "body",
    "subject",
    "message_type",
    "subtype_id",
    "author_id",
    "parent_id",
    "date",
    "is_internal",
    "partner_ids",
    "res_id",
];
/// The columns a message page reads: [`MESSAGE_FIELDS`] without the recipients (a page lists many
/// messages and the recipients are a many2many read of their own), and the thread model.
pub(crate) const MESSAGE_PAGE_FIELDS: [&str; 10] = [
    "body",
    "subject",
    "message_type",
    "subtype_id",
    "author_id",
    "parent_id",
    "date",
    "is_internal",
    "res_id",
    "model",
];
/// The fields of a `mail.followers` record in a store answer.
pub(crate) const FOLLOWER_FIELDS: [&str; 4] = ["res_model", "res_id", "partner_id", "subtype_ids"];
/// The fields of a `mail.message.subtype` record in a store answer.
pub(crate) const SUBTYPE_FIELDS: [&str; 2] = ["name", "internal"];

/// `ir.attachment` in a store answer: `name`, `mimetype`, `file_size`, `checksum`, `type` and `url`
/// (see [`thread_schema`]) - metadata only, never content. No access token and no route are
/// carried: `ir.attachment` has none until.
pub(crate) const ATTACHMENT: &str = "ir.attachment";
pub(crate) const MESSAGE: &str = "mail.message";
pub(crate) const FOLLOWERS: &str = "mail.followers";
pub(crate) const SUBTYPE: &str = "mail.message.subtype";
pub(crate) const PARTNER: &str = "res.partner";

crate::store_schema_part!(THREAD_SCHEMA = SchemaPart::new("mail", thread_schema));

/// The models and fields the thread answers use, as a part of the one store schema ((f):
/// the `TopicRegistry` owns the schema and its generation; a thread answer and a `store_fetch`
/// answer carry the same one).
fn thread_schema(f: &mut SchemaFragment) {
    // the partner records the answers carry for the authors, recipients and followers they name
    f.field(PARTNER, "name", FieldSpec::new(FieldKind::String));
    // every thread model of the repository, so the client can hold their chatter records.
    // shortcut: only `mail` may contribute a part until installed-only schema filtering exists (
    // (c).4), so the product bridge's two models are named here (no fields, so no record data); they
    // move to a `product_mail` part with task.
    f.model("res.users")
        .model("res.company")
        .model("product.template")
        .model("product.product");
    f.field(SUBTYPE, "name", FieldSpec::new(FieldKind::String));
    f.field(SUBTYPE, "internal", FieldSpec::new(FieldKind::Bool));
    f.field(MESSAGE, "body", FieldSpec::new(FieldKind::Html));
    f.field(MESSAGE, "subject", FieldSpec::new(FieldKind::String));
    f.field(
        MESSAGE,
        "message_type",
        FieldSpec::new(FieldKind::Selection),
    );
    f.field(
        MESSAGE,
        "subtype_id",
        FieldSpec::new(FieldKind::Many2One).target(SUBTYPE),
    );
    f.field(
        MESSAGE,
        "author_id",
        FieldSpec::new(FieldKind::Many2One).target(PARTNER),
    );
    f.field(
        MESSAGE,
        "parent_id",
        FieldSpec::new(FieldKind::Many2One).target(MESSAGE),
    );
    f.field(MESSAGE, "date", FieldSpec::new(FieldKind::Datetime));
    f.field(MESSAGE, "is_internal", FieldSpec::new(FieldKind::Bool));
    f.field(
        MESSAGE,
        "partner_ids",
        FieldSpec::new(FieldKind::X2Many).target(PARTNER),
    );
    f.field(
        MESSAGE,
        "attachment_ids",
        FieldSpec::new(FieldKind::X2Many).target(ATTACHMENT),
    );
    f.field(ATTACHMENT, "name", FieldSpec::new(FieldKind::String));
    f.field(ATTACHMENT, "mimetype", FieldSpec::new(FieldKind::String));
    f.field(ATTACHMENT, "file_size", FieldSpec::new(FieldKind::Int));
    f.field(ATTACHMENT, "checksum", FieldSpec::new(FieldKind::String));
    f.field(ATTACHMENT, "type", FieldSpec::new(FieldKind::Selection));
    f.field(ATTACHMENT, "url", FieldSpec::new(FieldKind::String));
    f.field(MESSAGE, "res_model", FieldSpec::new(FieldKind::String));
    f.field(MESSAGE, "res_id", FieldSpec::new(FieldKind::Int));
    f.field(FOLLOWERS, "res_model", FieldSpec::new(FieldKind::String));
    f.field(FOLLOWERS, "res_id", FieldSpec::new(FieldKind::Int));
    f.field(
        FOLLOWERS,
        "partner_id",
        FieldSpec::new(FieldKind::Many2One).target(PARTNER),
    );
    f.field(
        FOLLOWERS,
        "subtype_ids",
        FieldSpec::new(FieldKind::X2Many).target(SUBTYPE),
    );
}

/// The store schema the thread answers are encoded against: the registry's schema, the one a
/// `store_fetch` answer uses (a client fetches it once per generation).
///
/// # Errors
/// `Programming` when the linked registry or its schema is invalid (a defect, never a default).
pub fn schema() -> Result<&'static StoreSchema> {
    Ok(TopicRegistry::linked()?.schema())
}

fn wire(error: WireError) -> Error {
    Error::internal(format!("the store payload cannot be built: {error}")).with_source(error)
}

/// A positive record id out of a read value.
fn id_of(value: &Value, what: &str) -> Result<u64> {
    match value {
        Value::Int(id) if *id > 0 => Ok(*id as u64),
        other => Err(Error::invariant(format!(
            "{what} is not a record id: {other:?}"
        ))),
    }
}

/// A many2one read raw: an id, or `False` for none.
fn many2one(value: &Value, what: &str) -> Result<Option<u64>> {
    match value {
        Value::False => Ok(None),
        other => id_of(other, what).map(Some),
    }
}

/// An x2many read: the list of ids.
fn id_list(value: &Value, what: &str) -> Result<Vec<u64>> {
    match value {
        Value::List(items) => items.iter().map(|item| id_of(item, what)).collect(),
        other => Err(Error::invariant(format!(
            "{what} is not an id list: {other:?}"
        ))),
    }
}

/// Text of a read value; `False` (an unset char) reads as empty text.
fn text_of(value: &Value, what: &str) -> Result<String> {
    match value {
        Value::Str(text) => Ok(text.to_string()),
        Value::False => Ok(String::new()),
        other => Err(Error::invariant(format!("{what} is not text: {other:?}"))),
    }
}

fn bool_of(value: &Value, what: &str) -> Result<bool> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::False => Ok(false),
        other => Err(Error::invariant(format!(
            "{what} is not a boolean: {other:?}"
        ))),
    }
}

/// One record as `read` with the raw load returns it.
pub(crate) type Row = BTreeMap<String, Value>;

fn cell<'a>(row: &'a Row, field: &str, model: &str) -> Result<&'a Value> {
    row.get(field)
        .ok_or_else(|| Error::invariant(format!("the read of {model} has no field {field}")))
}

/// The fields of a message record, in schema order, as the store carries them; `recipients` adds
/// `partner_ids`, which the row must then hold.
///
/// # Errors
/// `Invariant` for a value of an unexpected shape.
pub(crate) fn message_entries(
    id: i64,
    row: &Row,
    model: &str,
    recipients: bool,
) -> Result<(u64, Vec<(&'static str, Wire)>)> {
    let uid = u64::try_from(id)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| Error::invariant(format!("message id {id} is not a record id")))?;
    let get = |field: &str| cell(row, field, MESSAGE);
    let mut set: Vec<(&'static str, Wire)> = vec![
        ("body", Wire::Html(text_of(get("body")?, "message body")?)),
        (
            "subject",
            Wire::String(text_of(get("subject")?, "message subject")?),
        ),
        (
            "message_type",
            Wire::Selection(text_of(get("message_type")?, "message type")?),
        ),
        (
            "subtype_id",
            Wire::Many2One(many2one(get("subtype_id")?, "message subtype")?),
        ),
        (
            "author_id",
            Wire::Many2One(many2one(get("author_id")?, "message author")?),
        ),
        (
            "parent_id",
            Wire::Many2One(many2one(get("parent_id")?, "message parent")?),
        ),
    ];
    match get("date")? {
        Value::DateTime(date) => set.push(("date", Wire::Datetime(date.to_string()))),
        Value::False => {}
        other => {
            return Err(Error::invariant(format!(
                "a message date is neither a datetime nor false: {other:?}"
            )));
        }
    }
    set.push((
        "is_internal",
        Wire::Bool(bool_of(get("is_internal")?, "message is_internal")?),
    ));
    if recipients {
        set.push((
            "partner_ids",
            Wire::X2ManyReplace(id_list(get("partner_ids")?, "message partners")?),
        ));
    }
    set.push(("res_model", Wire::String(model.to_owned())));
    set.push((
        "res_id",
        Wire::Int(
            i64::try_from(id_of(get("res_id")?, "message record id")?).map_err(|_| {
                Error::invariant("a message record id does not fit a signed integer")
            })?,
        ),
    ));
    Ok((uid, set))
}

/// The fields of a message record set in `builder`; returns the names set, for `mark_written`.
///
/// # Errors
/// `Invariant` for a value of an unexpected shape; the wire's refusals as `Internal`.
pub(crate) fn message_record(
    builder: &mut AnswerBuilder<'_>,
    id: i64,
    row: &Row,
    model: &str,
) -> Result<Vec<&'static str>> {
    let (uid, entries) = message_entries(id, row, model, true)?;
    let mut set = Vec::with_capacity(entries.len());
    for (field, value) in entries {
        builder.set(MESSAGE, uid, field, value).map_err(wire)?;
        set.push(field);
    }
    Ok(set)
}

/// The fields of a follower record set in `builder`.
///
/// # Errors
/// As [`message_record`].
pub(crate) fn follower_record(builder: &mut AnswerBuilder<'_>, id: i64, row: &Row) -> Result<()> {
    let uid = u64::try_from(id)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| Error::invariant(format!("follower id {id} is not a record id")))?;
    let get = |field: &str| cell(row, field, FOLLOWERS);
    builder
        .set(
            FOLLOWERS,
            uid,
            "res_model",
            Wire::String(text_of(get("res_model")?, "follower model")?),
        )
        .map_err(wire)?;
    builder
        .set(
            FOLLOWERS,
            uid,
            "res_id",
            Wire::Int(
                i64::try_from(id_of(get("res_id")?, "follower record id")?).map_err(|_| {
                    Error::invariant("a follower record id does not fit a signed integer")
                })?,
            ),
        )
        .map_err(wire)?;
    builder
        .set(
            FOLLOWERS,
            uid,
            "partner_id",
            Wire::Many2One(many2one(get("partner_id")?, "follower partner")?),
        )
        .map_err(wire)?;
    builder
        .set(
            FOLLOWERS,
            uid,
            "subtype_ids",
            Wire::X2ManyReplace(id_list(get("subtype_ids")?, "follower subtypes")?),
        )
        .map_err(wire)
}

/// The fields of a subtype record, as the store carries them.
///
/// # Errors
/// As [`message_entries`].
pub(crate) fn subtype_entries(id: i64, row: &Row) -> Result<(u64, Vec<(&'static str, Wire)>)> {
    let uid = u64::try_from(id)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| Error::invariant(format!("subtype id {id} is not a record id")))?;
    let get = |field: &str| cell(row, field, SUBTYPE);
    Ok((
        uid,
        vec![
            ("name", Wire::String(text_of(get("name")?, "subtype name")?)),
            (
                "internal",
                Wire::Bool(bool_of(get("internal")?, "subtype internal")?),
            ),
        ],
    ))
}

/// The fields of a subtype record set in `builder`.
///
/// # Errors
/// As [`message_record`].
pub(crate) fn subtype_record(builder: &mut AnswerBuilder<'_>, id: i64, row: &Row) -> Result<()> {
    let uid = u64::try_from(id)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| Error::invariant(format!("subtype id {id} is not a record id")))?;
    let get = |field: &str| cell(row, field, SUBTYPE);
    builder
        .set(
            SUBTYPE,
            uid,
            "name",
            Wire::String(text_of(get("name")?, "subtype name")?),
        )
        .map_err(wire)?;
    builder
        .set(
            SUBTYPE,
            uid,
            "internal",
            Wire::Bool(bool_of(get("internal")?, "subtype internal")?),
        )
        .map_err(wire)
}

/// Set every staged value of `fields` in `builder`.
///
/// # Errors
/// The wire's refusals as `Internal`.
pub(crate) fn apply_fields(builder: &mut AnswerBuilder<'_>, fields: &StoreFields) -> Result<()> {
    for (model, id, field, value) in fields.iter() {
        builder.set(model, id, field, value.clone()).map_err(wire)?;
    }
    Ok(())
}

/// Delete markers of follower records.
///
/// # Errors
/// The wire's refusals as `Internal`.
pub(crate) fn follower_deletes(builder: &mut AnswerBuilder<'_>, ids: &[i64]) -> Result<()> {
    for id in ids {
        let uid = u64::try_from(*id)
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| Error::invariant(format!("follower id {id} is not a record id")))?;
        builder.delete(FOLLOWERS, uid).map_err(wire)?;
    }
    Ok(())
}

/// Mark the fields of a message this transaction wrote.
///
/// # Errors
/// The wire's refusals as `Internal`.
pub(crate) fn mark_message_written(
    builder: &mut AnswerBuilder<'_>,
    id: i64,
    fields: &[&str],
) -> Result<()> {
    let uid = u64::try_from(id)
        .map_err(|_| Error::invariant(format!("message id {id} is not a record id")))?;
    for field in fields {
        builder.mark_written(MESSAGE, uid, field).map_err(wire)?;
    }
    Ok(())
}

/// The distinct partner ids the rows name (`author_id`, `partner_ids`, `partner_id`).
pub(crate) fn partner_refs<'a>(rows: impl IntoIterator<Item = &'a Row>) -> BTreeSet<i64> {
    let mut ids = BTreeSet::new();
    for row in rows {
        for field in ["author_id", "partner_id", "partner_ids"] {
            match row.get(field) {
                Some(Value::Int(id)) => {
                    ids.insert(*id);
                }
                Some(Value::List(items)) => {
                    ids.extend(items.iter().filter_map(|v| match v {
                        Value::Int(id) => Some(*id),
                        _ => None,
                    }));
                }
                _ => {}
            }
        }
    }
    ids
}

/// Stage the `name` of the partners `ids` the caller may read, from one rule-checked statement
/// and one read for all of them (never one per message). A partner the caller may not read is
/// left out: the client then shows its placeholder.
///
/// # Errors
/// `Invariant` for a value of an unexpected shape; the errors of the access check and the read.
pub(crate) async fn stage_partners(
    tx: &mut Transaction,
    env: &Env,
    ids: BTreeSet<i64>,
    fields: &mut StoreFields,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let readable = env
        .records(PARTNER)?
        .browse(ids)?
        .filtered_access(tx, erp_sdk::Operation::Read)
        .await?;
    if readable.is_empty() {
        return Ok(());
    }
    for (id, row) in crate::thread_rpc::rows_of(tx, &readable, &["name"]).await? {
        let uid = id_of(&Value::Int(id), "partner id")?;
        let name = text_of(cell(&row, "name", PARTNER)?, "partner name")?;
        fields.set(PARTNER, uid, "name", Wire::String(name));
    }
    Ok(())
}

/// Build a store answer: take the snapshot descriptor of `tx` (after its last write, so the
/// descriptor covers it), let `fill` add the records, encode.
///
/// `fill` receives the answer's builder and whether the transaction wrote anything (its own xid
/// is in the descriptor). The result is the payload as a JSON value, ready for an RPC answer.
///
/// # Errors
/// `Programming` for a transaction that is not REPEATABLE READ or ran on the read pool
/// (`Transaction::store_snapshot`), the errors of `fill`, `Internal` when the payload cannot be
/// built.
pub(crate) async fn answer(
    tx: &mut Transaction,
    fill: impl FnOnce(&mut AnswerBuilder<'_>, bool) -> Result<()>,
) -> Result<Value> {
    let schema = schema()?;
    let asof = tx.store_snapshot().await?;
    let wrote = asof.own().is_some();
    let mut builder = AnswerBuilder::new(schema, asof);
    fill(&mut builder, wrote)?;
    let bytes = builder.encode().map_err(wire)?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        Error::internal(format!("the store payload is not JSON: {error}")).with_source(error)
    })?;
    Ok(Value::Json(Arc::new(json)))
}

/// Store fields staged by the dispatched `_to_store` methods (`mail.message` and `mail.thread`):
/// the value of every field of every record, replaced when a later override sets it again.
///
/// An override adds fields (or changes one it knows) of the records the base method staged; a
/// field the store schema does not declare is refused by the runner when the topic's answer is
/// built, never dropped ("Change what a thread shows").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreFields {
    entries: BTreeMap<(String, u64, String), Wire>,
}

impl StoreFields {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `field` of record `id` of `model`, replacing an earlier value.
    pub fn set(&mut self, model: &str, id: u64, field: &str, value: Wire) {
        self.entries
            .insert((model.to_owned(), id, field.to_owned()), value);
    }

    /// The value staged for `field` of record `id` of `model`.
    #[must_use]
    pub fn get(&self, model: &str, id: u64, field: &str) -> Option<&Wire> {
        self.entries.get(&(model.to_owned(), id, field.to_owned()))
    }

    /// How many field values are staged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The staged values as `(model, id, field, value)`, ordered by model, id and field.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64, &str, &Wire)> {
        self.entries
            .iter()
            .map(|((model, id, field), value)| (model.as_str(), *id, field.as_str(), value))
    }

    /// Stage every value into a topic's output.
    pub fn apply(self, out: &mut crate::store_fetch::TopicOutput) {
        for ((model, id, field), value) in self.entries {
            out.set(&model, id, &field, value);
        }
    }

    pub(crate) fn extend(&mut self, model: &str, id: u64, entries: Vec<(&'static str, Wire)>) {
        for (field, value) in entries {
            self.set(model, id, field, value);
        }
    }
}
