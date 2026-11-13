//! The thread API a client calls over RPC (
//! guide "Call the thread from the
//! client (JSON-2)").
//!
//! Six methods of the `mail.thread` mixin, `thread_post`, `thread_follow`, `thread_unfollow`,
//! `thread_set_subtypes`, `thread_followers` and `thread_subtypes`. Each validates its input (the
//! post is a closed DTO, [`PostWire`]), decides what the caller's standing allows, calls the Rust
//! method of the same job through the method chain (`message_post`, `follow`, `unfollow`,
//! `set_subtypes`, `followers`, so an addon's override applies) and answers with the records it
//! touched as a store payload ([`crate::store`]).
//!
//! A caller's standing is its group: internal (`base.group_user`), public (`base.group_public`) or
//! portal (anyone else). Public callers are refused on every call; portal callers post comments
//! only, address nobody, and act on their own partner only; internal callers get the access the
//! Rust methods check. The author of a post is always the caller's partner. Nothing here elevates
//! the caller except the privileged reads named in the `SUDO_SITES` table of
//! `crates/erp-test-models/tests/access_entry_points.rs`.

use std::collections::BTreeSet;

use erp_sdk::prelude::*;
use erp_sdk::rpc::{ArgError, ArgShape, FromRpc, json_type};
use erp_sdk::{Load, Operation, ValueMap};

use crate::MailThread;
use crate::followers::subscriptions;
use crate::mail_thread::{MAX_FOLLOWER_PAGE, MAX_IDS_PER_CALL, checked_ids};
use crate::message_attachments;
use crate::post::{
    Body, HTML_REFUSED, Mention, MentionTarget, PostRequest, PostSubtype, PostType, Posted,
};
use crate::store::{self, Row, StoreFields};

/// The keys of the `post` object, in the order the error message lists them.
const POST_KEYS: [&str; 11] = [
    "body",
    "subject",
    "message_type",
    "subtype",
    "parent_id",
    "partner_ids",
    "mentions",
    "silent",
    "client_uid",
    "attachment_ids",
    "staged_uploads",
];

/// The `post` argument of `thread_post`: a validated [`PostRequest`] parsed from a closed JSON
/// object. Unknown keys are refused naming the key; there is no key for the author, the sender
/// address, tracking values or any other column. `attachment_ids` names files the caller uploaded
/// for the composer or that are on the record already (see [`PostRequest::attachment_ids`]).
///
/// ```text
/// {"body": "Hello", "partner_ids": [7], "mentions": [{"start": 0, "end": 5, "partner_id": 7}],
///  "message_type": "comment", "subtype": "comment", "client_uid": "0f9c…", "attachment_ids": [12]}
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostWire {
    request: PostRequest,
}

impl PostWire {
    /// The validated request.
    pub fn into_request(self) -> PostRequest {
        self.request
    }

    /// The validated request, borrowed.
    pub fn request(&self) -> &PostRequest {
        &self.request
    }

    /// Parse the JSON object a client sent as `post`.
    ///
    /// # Errors
    /// `Validation` naming the key and the problem; see the error catalogue of the design note.
    pub fn parse(map: &ValueMap) -> Result<Self> {
        parse_post(map).map(|request| Self { request })
    }
}

impl FromRpc for PostWire {
    const EXPECTED: &'static str = "an object";
    const SHAPE: ArgShape = ArgShape::Open("a post request");

    fn from_rpc(value: Option<Value>) -> std::result::Result<Self, ArgError> {
        match value {
            None => Err(ArgError::Missing),
            Some(Value::Map(map)) => Self::parse(&map).map_err(ArgError::Invalid),
            Some(other) => Err(ArgError::Mismatch {
                got: json_type(&other),
            }),
        }
    }
}

fn parse_staged_uploads(value: &Value) -> Result<Vec<crate::post::StagedAttachment>> {
    let Value::List(files) = value else {
        return Err(validation(
            "The staged_uploads are a list of file descriptors.",
        ));
    };
    if files.len() > crate::post::MAX_ATTACHMENTS {
        return Err(validation(
            "At most 100 staged uploads are posted with one message.",
        ));
    }
    files
        .iter()
        .map(|value| {
            let Value::Map(file) = value else {
                return Err(validation("Each staged upload is a file descriptor."));
            };
            if let Some(key) = file
                .keys()
                .find(|k| !["name", "upload_id", "mimetype"].contains(&k.as_str()))
            {
                return Err(validation(format!(
                    "A staged upload has no key '{key}'; its keys are name, upload_id, mimetype."
                )));
            }
            let text = |key: &str| -> Result<String> {
                match present(file, key) {
                    Some(Value::Str(text)) => {
                        let bound = if key == "name" { 255 } else { 36 };
                        if text.len() > bound {
                            return Err(validation(format!(
                                "A staged upload '{key}' is at most {bound} bytes."
                            )));
                        }
                        Ok(text.to_string())
                    }
                    _ => Err(validation(format!("A staged upload needs text '{key}'."))),
                }
            };
            let mime = match present(file, "mimetype") {
                None => None,
                Some(Value::Str(text)) if text.len() <= 127 => Some(text.to_string()),
                Some(Value::Str(_)) => {
                    return Err(validation(
                        "A staged upload media type is at most 127 bytes.",
                    ));
                }
                Some(_) => return Err(validation("A staged upload mimetype is text.")),
            };
            crate::post::StagedAttachment::new(text("name")?, &text("upload_id")?, mime)
        })
        .collect()
}

fn upload_content(
    data: Option<String>,
    upload_id: Option<String>,
) -> Result<erp_addon_base::attachments::AttachmentContent> {
    use erp_addon_base::attachments::{AttachmentContent, UploadId};
    match (data, upload_id) {
        (Some(data), None) => Ok(AttachmentContent::Base64(data)),
        (None, Some(id)) => Ok(AttachmentContent::Upload(UploadId::parse(&id)?)),
        _ => Err(validation(
            "Give exactly one of data or upload_id for an attachment.",
        )),
    }
}

/// A key's value, with JSON `null` read as absent. JSON `false` is a value like any other and
/// reaches the typed parse of its key, which refuses it unless the key is a boolean.
fn present<'a>(map: &'a ValueMap, key: &str) -> Option<&'a Value> {
    map.get(key).filter(|value| !matches!(value, Value::False))
}

fn validation(message: impl Into<String>) -> Error {
    Error::validation(message)
}

fn parse_post(map: &ValueMap) -> Result<PostRequest> {
    if let Some(key) = map.keys().find(|key| !POST_KEYS.contains(&key.as_str())) {
        return Err(validation(format!(
            "The post has no key '{key}'; its keys are {}.",
            POST_KEYS.join(", ")
        )));
    }
    let body = match present(map, "body") {
        None => return Err(validation("A post needs a body.")),
        Some(value) => parse_body(value)?,
    };
    let mut request = PostRequest::new(body);
    request.message_type = match present(map, "message_type") {
        None => PostType::Comment,
        Some(Value::Str(text)) => {
            let parsed: PostType = text.parse()?;
            if !matches!(parsed, PostType::Comment | PostType::Notification) {
                return Err(validation(format!(
                    "A client posts a comment or a notification, not '{parsed}'; the other \
                     message types are made by the server."
                )));
            }
            parsed
        }
        Some(other) => {
            return Err(validation(format!(
                "The message_type is text, not {}.",
                json_type(other)
            )));
        }
    };
    request.subtype = match present(map, "subtype") {
        None => PostSubtype::Comment,
        Some(Value::Str(text)) => match text.as_ref() {
            "comment" => PostSubtype::Comment,
            "note" => PostSubtype::Note,
            xmlid if xmlid.contains('.') => PostSubtype::Xmlid(xmlid.to_owned()),
            other => {
                return Err(validation(format!(
                    "A subtype is comment, note, an external id 'module.name' or an id, not \
                     '{other}'."
                )));
            }
        },
        Some(Value::Int(id)) => PostSubtype::Id(*id),
        Some(other) => {
            return Err(validation(format!(
                "A subtype is comment, note, an external id 'module.name' or an id, not {}.",
                json_type(other)
            )));
        }
    };
    request.subject = match present(map, "subject") {
        None => None,
        Some(Value::Str(text)) => Some(text.to_string()),
        Some(other) => {
            return Err(validation(format!(
                "The subject is text, not {}.",
                json_type(other)
            )));
        }
    };
    request.parent_id = match present(map, "parent_id") {
        None => None,
        Some(Value::Int(id)) => Some(*id),
        Some(other) => {
            return Err(validation(format!(
                "The parent_id is an integer, not {}.",
                json_type(other)
            )));
        }
    };
    if let Some(value) = present(map, "partner_ids") {
        request.partner_ids = int_list("partner_ids", value)?;
    }
    if let Some(value) = present(map, "attachment_ids") {
        request.attachment_ids = int_list("attachment_ids", value)?;
    }
    if let Some(value) = present(map, "staged_uploads") {
        request.staged_uploads = parse_staged_uploads(value)?;
    }
    if let Some(value) = present(map, "mentions") {
        request.mentions = mentions(value)?;
    }
    request.silent = match present(map, "silent") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(other) => {
            return Err(validation(format!(
                "The silent flag is true or false, not {}.",
                json_type(other)
            )));
        }
    };
    request.client_uid = match present(map, "client_uid") {
        None => None,
        Some(Value::Str(text)) => Some(
            Uuid::parse(text)
                .map_err(|_| validation(format!("The client_uid must be a UUID, not '{text}'.")))?,
        ),
        Some(other) => {
            return Err(validation(format!(
                "The client_uid is a UUID string, not {}.",
                json_type(other)
            )));
        }
    };
    Ok(request)
}

fn parse_body(value: &Value) -> Result<Body> {
    match value {
        Value::Str(text) => Ok(Body::Text(text.to_string())),
        Value::Map(map) => match map.iter().collect::<Vec<_>>().as_slice() {
            [(key, Value::Str(text))] if key.as_str() == "text" => Ok(Body::Text(text.to_string())),
            [(key, _)] if key.as_str() == "html" => Err(validation(HTML_REFUSED)),
            _ => Err(validation(
                "A body object has exactly one key, text or html, holding text.",
            )),
        },
        other => Err(validation(format!(
            "The post body is text or an object {{\"text\": …}}, not {}.",
            json_type(other)
        ))),
    }
}

fn int_list(key: &str, value: &Value) -> Result<Vec<i64>> {
    let Value::List(items) = value else {
        return Err(validation(format!(
            "The {key} are a list of integers, not {}.",
            json_type(value)
        )));
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::Int(id) => Ok(*id),
            other => Err(validation(format!(
                "The {key} hold integers; element {index} is {}.",
                json_type(other)
            ))),
        })
        .collect()
}

fn mentions(value: &Value) -> Result<Vec<Mention>> {
    const KEYS: &str = "start, end and partner_id";
    let Value::List(items) = value else {
        return Err(validation(format!(
            "The mentions are a list of objects with {KEYS}, not {}.",
            json_type(value)
        )));
    };
    items
        .iter()
        .map(|item| {
            let Value::Map(map) = item else {
                return Err(validation(format!(
                    "A mention is an object with {KEYS}, not {}.",
                    json_type(item)
                )));
            };
            if let Some(key) = map
                .keys()
                .find(|key| !matches!(key.as_str(), "start" | "end" | "partner_id"))
            {
                return Err(validation(format!(
                    "A mention has the keys start, end and partner_id, not '{key}'."
                )));
            }
            let int = |key: &str| match map.get(key) {
                Some(Value::Int(n)) => Ok(*n),
                _ => Err(validation(format!("A mention needs {KEYS}, as integers."))),
            };
            let offset = |key: &str| -> Result<usize> {
                let n = int(key)?;
                usize::try_from(n).map_err(|_| {
                    validation(format!(
                        "A mention offset is a non-negative integer, not {n}."
                    ))
                })
            };
            Ok(Mention {
                start: offset("start")?,
                end: offset("end")?,
                target: MentionTarget::Partner(int("partner_id")?),
            })
        })
        .collect()
}

/// What a caller is, for the thread API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// A member of `base.group_user` (or the superuser).
    Internal,
    /// A user outside `base.group_user` and `base.group_public`.
    Portal,
    /// A member of `base.group_public`.
    Public,
}

pub(crate) async fn standing(tx: &mut Transaction, env: &Env) -> Result<Standing> {
    if env.su() {
        return Ok(Standing::Internal);
    }
    if tx.has_group(env, "base.group_public").await? {
        Ok(Standing::Public)
    } else if tx.has_group(env, "base.group_user").await? {
        Ok(Standing::Internal)
    } else {
        Ok(Standing::Portal)
    }
}

const PUBLIC_REFUSED: &str = "Public users cannot use the follower and subtype calls of a thread.";
const PORTAL_OWN_ONLY: &str =
    "Portal users can only follow, unfollow or set the subtypes of their own partner.";

/// The shape a portal caller may post (comment, subtype comment, nobody addressed).
fn portal_shape(request: &PostRequest) -> Result<()> {
    let refuse = |what: String| {
        Error::access(format!(
            "Portal users post comments only (message_type comment, subtype comment), not {what}."
        ))
    };
    if request.message_type != PostType::Comment {
        return Err(refuse(format!("message_type '{}'", request.message_type)));
    }
    match &request.subtype {
        PostSubtype::Comment => {}
        PostSubtype::Xmlid(xmlid) if xmlid == "mail.mt_comment" => {}
        PostSubtype::Note => return Err(refuse("subtype 'note'".to_owned())),
        PostSubtype::Xmlid(xmlid) => return Err(refuse(format!("subtype '{xmlid}'"))),
        PostSubtype::Id(id) => return Err(refuse(format!("subtype id {id}"))),
    }
    if !request.partner_ids.is_empty() || !request.mentions.is_empty() {
        return Err(Error::access(
            "Portal users cannot address or mention partners.",
        ));
    }
    if request.silent {
        return Err(Error::access("Portal users cannot post silently."));
    }
    Ok(())
}

/// The partners an internal caller addresses or mentions must be readable by them: a missing id
/// and an unreadable one are refused alike, so the message tells nothing about which exists.
async fn require_readable_partners(
    tx: &mut Transaction,
    env: &Env,
    request: &PostRequest,
) -> Result<()> {
    // a malformed or oversize list is refused here, not left to the post path
    let mut wanted: BTreeSet<i64> = crate::post::checked_partner_ids(&request.partner_ids)?
        .into_iter()
        .collect();
    for mention in &request.mentions {
        if let MentionTarget::Partner(id) = mention.target {
            if id <= 0 {
                return Err(Error::validation(format!(
                    "A partner id must be a positive integer, not {id}."
                )));
            }
            wanted.insert(id);
        }
    }
    if wanted.len() > MAX_IDS_PER_CALL {
        return Err(Error::validation(format!(
            "At most {MAX_IDS_PER_CALL} partners may be addressed or mentioned per post, not {}.",
            wanted.len()
        )));
    }
    // an empty list is legitimate: nobody is addressed
    if wanted.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = wanted.iter().copied().collect();
    let readable: BTreeSet<i64> = env
        .records("res.partner")?
        .browse(ids.clone())?
        .filtered_access(tx, Operation::Read)
        .await?
        .ids()
        .into_iter()
        .collect();
    let refused: Vec<String> = ids
        .iter()
        .filter(|id| !readable.contains(id))
        .map(ToString::to_string)
        .collect();
    if refused.is_empty() {
        return Ok(());
    }
    Err(Error::access(format!(
        "You may not address partner ids {}: they do not exist or you may not read them.",
        refused.join(", ")
    )))
}

/// The subtypes a portal caller names must be ones it can see: the subtype rule of its group
/// hides the internal ones, so the same message as a missing id follows.
async fn require_visible_subtypes(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let domain = Domain::condition("id", Operator::In, ids.to_vec())?
        & (Domain::condition("res_model", Operator::Eq, model)?
            | Domain::condition("res_model", Operator::Eq, false)?);
    let found: BTreeSet<i64> = env
        .records("mail.message.subtype")?
        .search(tx, &domain)
        .await?
        .ids()
        .into_iter()
        .collect();
    let bad: Vec<String> = ids
        .iter()
        .filter(|id| !found.contains(id))
        .map(ToString::to_string)
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(Error::validation(format!(
        "Unknown subtype ids for {model}: {}. A subtype must exist and have no model or the \
         model of the record.",
        bad.join(", ")
    )))
}

fn single(record: &Records) -> Result<i64> {
    record
        .ids()
        .first()
        .copied()
        .ok_or_else(|| Error::invariant("a single-record call has no record id"))
}

/// Read the rows of `records` with the raw load, keyed by id.
pub(crate) async fn rows_of(
    tx: &mut Transaction,
    records: &Records,
    fields: &[&str],
) -> Result<Vec<(i64, Row)>> {
    let rows = records.read_with(tx, fields, Load::Raw).await?;
    rows.into_iter()
        .map(|mut row| {
            let id = match row.remove("id") {
                Some(Value::Int(id)) => id,
                other => {
                    return Err(Error::invariant(format!(
                        "a read row of {} has no id: {other:?}",
                        records.model_name()
                    )));
                }
            };
            Ok((id, row))
        })
        .collect()
}

/// The follower rows of `partner_ids` on one record, read with privileges: the caller's access
/// to the record and the standing rules have been checked, and only the rows of the partners
/// the caller named are read (a portal caller acting on its own partner cannot read the
/// follower table).
async fn read_followers(tx: &mut Transaction, env: &Env, ids: Vec<i64>) -> Result<Vec<(i64, Row)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let followers = env.records("mail.followers")?.sudo(true).browse(ids)?;
    rows_of(tx, &followers, &store::FOLLOWER_FIELDS).await
}

fn answer_map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut out = ValueMap::default();
    for (key, value) in entries {
        out.insert(key.to_owned(), value);
    }
    Value::Map(Arc::new(out))
}

fn ids_value(ids: &[i64]) -> Value {
    Value::list(ids.iter().map(|id| Value::Int(*id)))
}

/// An answer with the follower rows `rows` as records and `deletes` as delete markers.
async fn follower_answer(
    tx: &mut Transaction,
    env: &Env,
    rows: Vec<(i64, Row)>,
    deletes: Vec<i64>,
) -> Result<Value> {
    let mut partners = StoreFields::new();
    store::stage_partners(
        tx,
        env,
        store::partner_refs(rows.iter().map(|(_, r)| r)),
        &mut partners,
    )
    .await?;
    let store = store::answer(tx, |builder, _wrote| {
        for (id, row) in &rows {
            store::follower_record(builder, *id, row)?;
        }
        store::apply_fields(builder, &partners)?;
        store::follower_deletes(builder, &deletes)
    })
    .await?;
    Ok(answer_map([("store", store)]))
}

/// The common start of the follower calls: one record, the standing, and for a portal caller
/// that it acts on its own partner only.
async fn follower_call_guard(
    tx: &mut Transaction,
    record: &Records,
    partner_ids: &[i64],
) -> Result<Standing> {
    let env = record.env().clone();
    let standing = standing(tx, &env).await?;
    match standing {
        Standing::Public => return Err(Error::access(PUBLIC_REFUSED)),
        Standing::Internal => {}
        Standing::Portal => {
            let id = single(record)?;
            let own = subscriptions(tx, record.model_name(), &[id], &[], env.uid())
                .await?
                .user_partner;
            let named: BTreeSet<i64> = partner_ids.iter().copied().collect();
            if own.is_none() || named != own.into_iter().collect::<BTreeSet<_>>() {
                return Err(Error::access(PORTAL_OWN_ONLY));
            }
        }
    }
    Ok(standing)
}

// The read calls that answer a store payload run on the primary (D-4, (e)):
// `Transaction::store_snapshot` refuses a read-pool transaction.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::rpc::PRIMARY_READS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static THREAD_FOLLOWERS_PRIMARY: erp_sdk::rpc::PrimaryRead = erp_sdk::rpc::PrimaryRead {
    addon: "mail",
    model: "mail.thread",
    method: "thread_followers",
};

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::rpc::PRIMARY_READS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static THREAD_SUBTYPES_PRIMARY: erp_sdk::rpc::PrimaryRead = erp_sdk::rpc::PrimaryRead {
    addon: "mail",
    model: "mail.thread",
    method: "thread_subtypes",
};

/// The client-callable thread API of the `mail.thread` mixin.
#[extend(MailThread)]
pub struct MailThreadRpc {}

#[methods(MailThread, addon = "mail")]
impl MailThreadRpc {
    /// Post a message on the record as the caller and answer with it. `post`
    /// is the closed object of [`PostWire`]. The caller needs the access `_mail_post_access`
    /// names; the author is the caller's partner; an author who may not write the record is not
    /// subscribed by the post. Answers `{message_id, replayed, notified_partner_ids, store}`:
    /// the message as a `mail.message` record of the store, `replayed` true when the
    /// `client_uid` was seen before, and the partners it reached (internal callers only: a portal
    /// caller is not told who follows the record).
    ///
    /// # Errors
    /// `Access` for a public caller, a portal caller who posts anything but a comment or addresses
    /// partners, a caller who may not read an addressed partner, and the record-level refusal of
    /// the post access; `Validation` as `message_post`, for anything but one saved record; the
    /// errors of the store answer.
    #[method]
    #[rpc(effect = write)]
    async fn thread_post(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        post: PostWire,
    ) -> Result<Value> {
        let record = this.as_records().ensure_one()?;
        let env = record.env().clone();
        let standing = standing(tx, &env).await?;
        let mut request = post.into_request();
        match standing {
            Standing::Public => return Err(Error::access("Public users cannot post on a thread.")),
            Standing::Portal => portal_shape(&request)?,
            Standing::Internal => require_readable_partners(tx, &env, &request).await?,
        }
        if !record.has_access(tx, Operation::Write).await? {
            request.options.skip_author_subscribe = true;
        }
        let files_named = !request.attachment_ids.is_empty() || !request.staged_uploads.is_empty();
        let posted: Posted = record.call(tx, "message_post", (request,)).await?;
        let message = env
            .records("mail.message")?
            .browse(vec![posted.message_id])?;
        let rows = rows_of(tx, &message, &store::MESSAGE_FIELDS).await?;
        let [(id, row)] = rows.as_slice() else {
            return Err(Error::invariant(format!(
                "the posted message {} cannot be read back by its author",
                posted.message_id
            )));
        };
        let model = record.model_name().to_owned();
        let replayed = posted.replayed;
        // the files posted with the message: a message this call created from a request without
        // files has none; any other (files named, or a replayed post) is asked
        let mut files = StoreFields::new();
        if replayed || files_named {
            message_attachments::stage_of_messages(tx, &[*id], &mut files).await?;
        } else {
            message_attachments::stage_none(*id, &mut files)?;
        }
        store::stage_partners(tx, &env, store::partner_refs([row]), &mut files).await?;
        let store = store::answer(tx, |builder, wrote| {
            let mut set = store::message_record(builder, *id, row, &model)?;
            store::apply_fields(builder, &files)?;
            set.push("attachment_ids");
            if wrote && !replayed {
                store::mark_message_written(builder, *id, &set)?;
            }
            Ok(())
        })
        .await?;
        let mut entries = vec![
            ("message_id", Value::Int(posted.message_id)),
            ("replayed", Value::Bool(posted.replayed)),
            ("store", store),
        ];
        if standing == Standing::Internal {
            entries.push((
                "notified_partner_ids",
                ids_value(&posted.notified_partner_ids),
            ));
        }
        Ok(answer_map(entries))
    }

    /// Upload one file for the composer of this record (exactly one of base64 `data` or `upload_id`; `mimetype` an optional
    /// hint): the file waits, its creator's alone, until `thread_post` names its id in
    /// `attachment_ids`. Answers `{attachment_id, name, mimetype, file_size, checksum}`.
    ///
    /// # Errors
    /// `Access` for a public caller or a caller who may not post on the record; `Validation` for a
    /// bad name, content above the inline threshold, or more than 50 waiting files; `UserError` for
    /// malformed base64.
    #[method]
    #[rpc(effect = write)]
    async fn thread_upload(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        name: String,
        data: Option<String>,
        mimetype: Option<String>,
        upload_id: Option<String>,
    ) -> Result<Value> {
        let record = this.as_records().ensure_one()?;
        message_attachments::upload(
            tx,
            record,
            name,
            mimetype,
            upload_content(data, upload_id)?,
            true,
        )
        .await
    }

    /// Attach one file (exactly one of base64 `data` or `upload_id`) to the record itself, for its attachments panel. Same
    /// answer and limits as `thread_upload`; the caller needs write access to the record.
    ///
    /// # Errors
    /// As `thread_upload`, with the record's write access in place of the post access.
    #[method]
    #[rpc(effect = write)]
    async fn thread_attach(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        name: String,
        data: Option<String>,
        mimetype: Option<String>,
        upload_id: Option<String>,
    ) -> Result<Value> {
        let record = this.as_records().ensure_one()?;
        message_attachments::upload(
            tx,
            record,
            name,
            mimetype,
            upload_content(data, upload_id)?,
            false,
        )
        .await
    }

    /// Make `partner_ids` follow the record: `follow` with its access rules
    /// (read for one's own partner, write for others), and for a portal caller only its
    /// own partner and only subtypes it can see. Answers `{store}` with the follower records of
    /// the named partners.
    ///
    /// # Errors
    /// `Access` for a public caller or a portal caller naming another partner; the errors of
    /// `follow`.
    #[method]
    #[rpc(effect = write)]
    async fn thread_follow(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        partner_ids: Vec<i64>,
        subtype_ids: Option<Vec<i64>>,
    ) -> Result<Value> {
        let record = this.as_records().ensure_one()?;
        let partner_ids = checked_ids("partner", partner_ids)?;
        let subtype_ids = match subtype_ids {
            Some(ids) => Some(checked_ids("subtype", ids)?),
            None => None,
        };
        let standing = follower_call_guard(tx, record, &partner_ids).await?;
        let env = record.env().clone();
        if standing == Standing::Portal {
            require_visible_subtypes(
                tx,
                &env,
                record.model_name(),
                subtype_ids.as_deref().unwrap_or_default(),
            )
            .await?;
        }
        record
            .call::<_, ()>(tx, "follow", (partner_ids.clone(), subtype_ids))
            .await?;
        let found = subscriptions(
            tx,
            record.model_name(),
            &[single(record)?],
            &partner_ids,
            env.uid(),
        )
        .await?;
        let rows = read_followers(tx, &env, found.followers).await?;
        follower_answer(tx, &env, rows, Vec::new()).await
    }

    /// Stop `partner_ids` following the record: `unfollow` with its access
    /// rules; a portal caller may name only its own partner. Answers `{store}` with a delete
    /// marker for each follower record removed.
    ///
    /// # Errors
    /// `Access` for a public caller or a portal caller naming another partner; the errors of
    /// `unfollow`.
    #[method]
    #[rpc(effect = write)]
    async fn thread_unfollow(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        partner_ids: Vec<i64>,
    ) -> Result<Value> {
        let record = this.as_records().ensure_one()?;
        let partner_ids = checked_ids("partner", partner_ids)?;
        follower_call_guard(tx, record, &partner_ids).await?;
        let env = record.env().clone();
        // the rows are gone after the call; their ids are revealed only if it succeeds
        let found = subscriptions(
            tx,
            record.model_name(),
            &[single(record)?],
            &partner_ids,
            env.uid(),
        )
        .await?;
        record.call::<_, ()>(tx, "unfollow", (partner_ids,)).await?;
        follower_answer(tx, &env, Vec::new(), found.followers).await
    }

    /// Replace the subtypes `partner_id` receives from the record:
    /// `set_subtypes` with its access rules; a portal caller may name only its own partner and
    /// only subtypes it can see. Answers `{store}` with the follower record.
    ///
    /// # Errors
    /// `Access` for a public caller or a portal caller naming another partner; the errors of
    /// `set_subtypes`.
    #[method]
    #[rpc(effect = write)]
    async fn thread_set_subtypes(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        partner_id: i64,
        subtype_ids: Vec<i64>,
    ) -> Result<Value> {
        let record = this.as_records().ensure_one()?;
        let partner = checked_ids("partner", vec![partner_id])?;
        let subtype_ids = checked_ids("subtype", subtype_ids)?;
        let standing = follower_call_guard(tx, record, &partner).await?;
        let env = record.env().clone();
        if standing == Standing::Portal {
            require_visible_subtypes(tx, &env, record.model_name(), &subtype_ids).await?;
        }
        record
            .call::<_, ()>(tx, "set_subtypes", (partner_id, subtype_ids))
            .await?;
        let found = subscriptions(
            tx,
            record.model_name(),
            &[single(record)?],
            &partner,
            env.uid(),
        )
        .await?;
        let rows = read_followers(tx, &env, found.followers).await?;
        follower_answer(tx, &env, rows, Vec::new()).await
    }

    /// One keyset page of the record's followers, oldest first: at most
    /// `limit` (1 to 100, default 100) after the follower id `after`. Internal callers only. Answers
    /// `{store, follower_ids, next_after}`: the follower records, their ids in order, and the id to
    /// pass as `after` for the next page (`null` on the last page).
    ///
    /// # Errors
    /// `Access` for a caller who is not internal and for a record the caller may not read;
    /// `Validation` for a limit out of range or a non-positive `after`.
    #[method]
    #[rpc(effect = read)]
    async fn thread_followers(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        after: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Value> {
        let record = this.as_records().ensure_one()?;
        let env = record.env().clone();
        if standing(tx, &env).await? != Standing::Internal {
            return Err(Error::access("Only internal users can list followers."));
        }
        let limit = match limit {
            None => MAX_FOLLOWER_PAGE,
            Some(n) => u32::try_from(n)
                .ok()
                .filter(|n| (1..=MAX_FOLLOWER_PAGE).contains(n))
                .ok_or_else(|| {
                    Error::validation(format!(
                        "A follower page holds 1 to {MAX_FOLLOWER_PAGE} followers, not {n}."
                    ))
                })?,
        };
        let page: Records = record.call(tx, "followers", (after, limit)).await?;
        let rows = rows_of(tx, &page, &store::FOLLOWER_FIELDS).await?;
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let next_after = if ids.len() == limit as usize {
            ids.last().map_or(Value::False, |id| Value::Int(*id))
        } else {
            Value::False
        };
        let mut partners = StoreFields::new();
        store::stage_partners(
            tx,
            &env,
            store::partner_refs(rows.iter().map(|(_, r)| r)),
            &mut partners,
        )
        .await?;
        let store = store::answer(tx, |builder, _wrote| {
            for (id, row) in &rows {
                store::follower_record(builder, *id, row)?;
            }
            store::apply_fields(builder, &partners)?;
            Ok(())
        })
        .await?;
        Ok(answer_map([
            ("store", store),
            ("follower_ids", ids_value(&ids)),
            ("next_after", next_after),
        ]))
    }

    /// Needs read access to the record. Answers `{store, subtype_ids}`.
    ///
    /// # Errors
    /// `Access` for a public caller or a record the caller may not read; `Invariant` when more
    /// subtypes exist than one answer holds.
    #[method]
    #[rpc(effect = read)]
    async fn thread_subtypes(this: &RecordSet<MailThread>, tx: &mut Transaction) -> Result<Value> {
        let record = this.as_records().ensure_one()?;
        let env = record.env().clone();
        if standing(tx, &env).await? == Standing::Public {
            return Err(Error::access(PUBLIC_REFUSED));
        }
        record.check_access(tx, Operation::Read).await?;
        let domain = (Domain::condition("res_model", Operator::Eq, record.model_name())?
            | Domain::condition("res_model", Operator::Eq, false)?)
            & Domain::condition("hidden", Operator::Eq, false)?;
        let options = SearchOptions {
            order: Some("sequence, id".to_owned()),
            limit: Some(MAX_IDS_PER_CALL as u64 + 1),
            ..SearchOptions::default()
        };
        let found = env
            .records("mail.message.subtype")?
            .search_with(tx, &domain, &options)
            .await?;
        if found.ids().len() > MAX_IDS_PER_CALL {
            return Err(Error::invariant(format!(
                "more than {MAX_IDS_PER_CALL} subtypes apply to {}",
                record.model_name()
            )));
        }
        let rows = rows_of(tx, &found, &store::SUBTYPE_FIELDS).await?;
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let store = store::answer(tx, |builder, _wrote| {
            for (id, row) in &rows {
                store::subtype_record(builder, *id, row)?;
            }
            Ok(())
        })
        .await?;
        Ok(answer_map([
            ("store", store),
            ("subtype_ids", ids_value(&ids)),
        ]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use erp_sdk::ErrorKind;

    fn map(json: erp_sdk::serde_json::Value) -> ValueMap {
        let Value::Map(map) = json_to_value(&json) else {
            panic!("an object")
        };
        Arc::unwrap_or_clone(map)
    }

    fn json_to_value(json: &erp_sdk::serde_json::Value) -> Value {
        use erp_sdk::serde_json::Value as J;
        match json {
            J::Null => Value::False,
            J::Bool(b) => Value::Bool(*b),
            J::Number(n) => Value::Int(n.as_i64().unwrap()),
            J::String(s) => Value::str(s),
            J::Array(items) => Value::list(items.iter().map(json_to_value)),
            J::Object(o) => Value::Map(Arc::new(
                o.iter()
                    .map(|(k, v)| (k.clone(), json_to_value(v)))
                    .collect(),
            )),
        }
    }

    #[test]
    fn the_dto_parses_every_key() {
        let post = PostWire::parse(&map(erp_sdk::serde_json::json!({
            "body": {"text": "Hi Ann"},
            "subject": "s",
            "message_type": "notification",
            "subtype": "note",
            "parent_id": 4,
            "partner_ids": [7, 8],
            "mentions": [{"start": 3, "end": 6, "partner_id": 7}],
            "silent": true,
            "client_uid": "0f9c1b1e-5d37-4a4b-9a62-3f1e2f7d9b10",
        })))
        .unwrap();
        let request = post.request();
        assert_eq!(request.body, Body::Text("Hi Ann".to_owned()));
        assert_eq!(request.message_type, PostType::Notification);
        assert_eq!(request.subtype, PostSubtype::Note);
        assert_eq!(request.parent_id, Some(4));
        assert_eq!(request.partner_ids, vec![7, 8]);
        assert_eq!(
            request.mentions,
            vec![Mention {
                start: 3,
                end: 6,
                target: MentionTarget::Partner(7)
            }]
        );
        assert!(request.silent);
        assert!(request.client_uid.is_some());
        // a plain string is the text, and the defaults are a comment under the comment subtype
        let post = PostWire::parse(&map(erp_sdk::serde_json::json!({"body": "x"}))).unwrap();
        assert_eq!(post.request().message_type, PostType::Comment);
        assert_eq!(post.request().subtype, PostSubtype::Comment);
    }

    #[test]
    fn the_dto_refuses_with_the_key_and_the_reason() {
        for (json, message) in [
            (
                erp_sdk::serde_json::json!({"body": "x", "author_id": 1}),
                "The post has no key 'author_id'; its keys are body, subject, message_type, \
                 subtype, parent_id, partner_ids, mentions, silent, client_uid, attachment_ids, \
                 staged_uploads.",
            ),
            (erp_sdk::serde_json::json!({}), "A post needs a body."),
            (
                erp_sdk::serde_json::json!({"body": 5}),
                "The post body is text or an object {\"text\": …}, not integer.",
            ),
            (
                erp_sdk::serde_json::json!({"body": {"html": "<b>x</b>"}}),
                HTML_REFUSED,
            ),
            (
                erp_sdk::serde_json::json!({"body": {"text": "a", "html": "b"}}),
                "A body object has exactly one key, text or html, holding text.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "message_type": "email"}),
                "A client posts a comment or a notification, not 'email'; the other message \
                 types are made by the server.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "message_type": "tracking"}),
                "A client posts a comment or a notification, not 'tracking'; the other message \
                 types are made by the server.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "message_type": "user_notification"}),
                "A user_notification is not posted on a thread; it is sent with message_notify, \
                 which is not implemented yet.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "message_type": "nope"}),
                "Unknown message type 'nope'; a post is one of email, comment, email_outgoing, \
                 notification, auto_comment, out_of_office, tracking.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "subtype": "mt_note"}),
                "A subtype is comment, note, an external id 'module.name' or an id, not \
                 'mt_note'.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "partner_ids": [1, "2"]}),
                "The partner_ids hold integers; element 1 is string.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "mentions": [{"start": 0, "end": 1, "partner_id": 1, "x": 2}]}),
                "A mention has the keys start, end and partner_id, not 'x'.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "mentions": [{"start": 0, "end": 1}]}),
                "A mention needs start, end and partner_id, as integers.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "mentions": [{"start": -1, "end": 1, "partner_id": 1}]}),
                "A mention offset is a non-negative integer, not -1.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "client_uid": "nope"}),
                "The client_uid must be a UUID, not 'nope'.",
            ),
            (
                erp_sdk::serde_json::json!({"body": "x", "silent": "yes"}),
                "The silent flag is true or false, not string.",
            ),
        ] {
            let error = PostWire::parse(&map(json)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation, "{message}");
            assert_eq!(error.message(), message);
        }
    }
}
