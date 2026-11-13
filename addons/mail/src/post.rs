//! The record-thread post path: `message_post` and `_message_log_batch` of `mail.thread`.
//!
//! Our own implementation.
//!
//! A post is: the access check on the record (`_mail_post_access`), one context statement (subtype,
//! author, parent, `client_uid` replay, partner existence, a fresh Message-Id), one recipient
//! statement (followers on the subtype, explicit and mentioned partners), and the ORM `create` of the
//! message with its recipient and notified-partner rows. The author's auto-subscription adds a
//! follower `create`. Attachments the caller names by id (`attachment_ids`) are claimed for the
//! record through the attachment service and linked to the message (
//! ); inbox rows, email jobs, bus events and the outbox
//! are later slices and are not written here.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use erp_addon_base::attachments::{
    self, AdoptScope, AttachmentContent, NewAttachment, ParentRef, UploadId,
};

use crate::followers::{ExistingPolicy, SubtypeChoice, insert_followers};
use crate::mail_thread::MAX_IDS_PER_CALL;

/// Most bytes of a post's body text (and of its subject).
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// Bytes the HTML field's cleaning adds around a body that starts with text: `<p>` and `</p>`.
const WRAP_BYTES: usize = 7;

/// The largest stored body: the store payload's string limit, which a message body must fit to be
/// answered (`erp_store_wire::Limit::StringBytes`).
fn max_stored_body_bytes() -> usize {
    usize::try_from(erp_sdk::store_wire::Limit::StringBytes.maximum()).unwrap_or(usize::MAX)
}

/// Most mentions one post carries.
pub const MAX_MENTIONS: usize = 100;

/// Most attachment ids one post names.
pub const MAX_ATTACHMENTS: usize = 100;
/// `mail.scheduled.message` joins it with the scheduled-messages
/// slice.
pub const COMPOSER_MODEL: &str = "mail.compose.message";

/// The refusal of an HTML body, shared by the Rust path and the RPC DTO.
pub(crate) const HTML_REFUSED: &str = "An HTML body is refused until HTML fields are sanitized (a later release); post the text with its mentions instead.";

/// The body of a post.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Body {
    /// Plain text: escaped by the server, mentions rendered as anchors.
    Text(String),
    /// HTML: refused until HTML fields are sanitized.
    Html(String),
}

/// What a mention points at (6). Only partner mentions are accepted on record threads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MentionTarget {
    /// A partner (`res.partner` id).
    Partner(i64),
    /// A channel (`discuss.channel` id).
    Channel(i64),
    /// A role.
    Role(i64),
    /// Everyone of a channel.
    Everyone,
    /// The members of a channel who are online.
    Here,
}

/// A mention: the bytes `start..end` of the text body, which the server renders as an anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// First byte of the mention in the text body.
    pub start: usize,
    /// Byte after the mention.
    pub end: usize,
    /// What is mentioned.
    pub target: MentionTarget,
}

/// The subtype a message is posted under.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PostSubtype {
    #[default]
    Note,
    /// `mail.mt_comment`.
    Comment,
    /// A subtype by id: no model or the record's model.
    Id(i64),
    /// A subtype by external id `module.name`.
    Xmlid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PostOptions {
    pub autofollow_recipients: bool,
    pub skip_author_subscribe: bool,
}

impl PostOptions {
    /// No option set.
    pub fn new() -> Self {
        Self::default()
    }
}

/// A staged file to adopt within a post. Content facts are resolved by the attachment linker.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StagedAttachment {
    name: String,
    upload_id: UploadId,
    mimetype: Option<String>,
}
impl StagedAttachment {
    /// Validate bounded file metadata and the upload id's canonical shape.
    ///
    /// # Errors
    /// Validation for an invalid name, media type or upload id.
    pub fn new(name: String, upload_id: &str, mimetype: Option<String>) -> Result<Self> {
        if name.is_empty()
            || name.len() > attachments::NAME_MAX_BYTES
            || name.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n'))
        {
            return Err(Error::validation(
                "A staged attachment name is 1 to 255 bytes without CR, LF or NUL.",
            ));
        }
        if mimetype.as_ref().is_some_and(|m| m.len() > 127) {
            return Err(Error::validation(
                "A staged attachment media type is at most 127 bytes.",
            ));
        }
        Ok(Self {
            name,
            upload_id: UploadId::parse(upload_id)?,
            mimetype,
        })
    }
    fn item(&self) -> NewAttachment {
        let mut item = NewAttachment::new(
            self.name.clone(),
            AttachmentContent::Upload(self.upload_id.clone()),
        )
        .pending_in(COMPOSER_MODEL);
        item.declared_mimetype.clone_from(&self.mimetype);
        item
    }
}

/// A post on one record (`message_post`). Built with [`PostRequest::new`], then fields set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PostRequest {
    /// The body.
    pub body: Body,
    /// Subject line.
    pub subject: Option<String>,
    pub message_type: PostType,
    /// The subtype; `mail.mt_note` by default.
    pub subtype: PostSubtype,
    /// The message replied to (a message of the same record).
    pub parent_id: Option<i64>,
    /// At most [`MAX_ATTACHMENTS`]; any other id refuses the post with `Access` (see
    /// [`erp_addon_base::attachments::adopt`]). Ignored when a `client_uid` replay returns the
    /// stored message.
    pub attachment_ids: Vec<i64>,
    /// Ready uploads to adopt atomically after the idempotency lookup. Combined with attachment
    /// ids, at most MAX_ATTACHMENTS; duplicate staged ids are refused. Ignored on replay.
    pub staged_uploads: Vec<StagedAttachment>,
    /// Explicit recipients (To).
    pub partner_ids: Vec<i64>,
    /// Structured mentions of the text body.
    pub mentions: Vec<Mention>,
    /// No push and no out-of-focus alert (5).
    pub silent: bool,
    /// The client's id for the post: a retry with the same id returns the stored message.
    pub client_uid: Option<Uuid>,
    /// The activity type a done-activity message records (`mail.message.mail_activity_type_id`).
    pub mail_activity_type_id: Option<i64>,
    /// Context options.
    pub options: PostOptions,
}

impl PostRequest {
    /// A `notification` post of `body` under `mail.mt_note`, without recipients.
    pub fn new(body: Body) -> Self {
        Self {
            body,
            subject: None,
            message_type: PostType::Notification,
            subtype: PostSubtype::Note,
            parent_id: None,
            attachment_ids: Vec::new(),
            staged_uploads: Vec::new(),
            partner_ids: Vec::new(),
            mentions: Vec::new(),
            silent: false,
            client_uid: None,
            mail_activity_type_id: None,
            options: PostOptions::default(),
        }
    }
}

/// The result of a post.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Posted {
    /// The message.
    pub message_id: i64,
    /// True when the `client_uid` was seen before and the stored message is returned.
    pub replayed: bool,
    /// The partners the message reached, in id order.
    pub notified_partner_ids: Vec<i64>,
    /// Whether the message is silent (for the later push job).
    pub silent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PostType {
    /// An incoming email.
    Email,
    /// A comment by a user.
    Comment,
    /// An outgoing email.
    EmailOutgoing,
    Notification,
    /// An automated comment.
    AutoComment,
    /// An out-of-office reply.
    OutOfOffice,
    /// A tracking message.
    Tracking,
}

impl PostType {
    /// Every type a post accepts, in the selection's order.
    pub const ALL: [PostType; 7] = [
        PostType::Email,
        PostType::Comment,
        PostType::EmailOutgoing,
        PostType::Notification,
        PostType::AutoComment,
        PostType::OutOfOffice,
        PostType::Tracking,
    ];

    /// The selection value stored in `mail.message.message_type`.
    pub fn as_str(self) -> &'static str {
        match self {
            PostType::Email => "email",
            PostType::Comment => "comment",
            PostType::EmailOutgoing => "email_outgoing",
            PostType::Notification => "notification",
            PostType::AutoComment => "auto_comment",
            PostType::OutOfOffice => "out_of_office",
            PostType::Tracking => "tracking",
        }
    }

    fn subscribes_author(self) -> bool {
        match self {
            PostType::Notification | PostType::AutoComment | PostType::OutOfOffice => false,
            PostType::Email | PostType::Comment | PostType::EmailOutgoing | PostType::Tracking => {
                true
            }
        }
    }
}

impl std::fmt::Display for PostType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PostType {
    type Err = Error;

    /// Parses a selection value. `user_notification` and unknown values are `Validation` errors.
    fn from_str(value: &str) -> Result<Self> {
        if value == "user_notification" {
            return Err(Error::validation(
                "A user_notification is not posted on a thread; it is sent with message_notify, \
                 which is not implemented yet.",
            ));
        }
        PostType::ALL
            .into_iter()
            .find(|t| t.as_str() == value)
            .ok_or_else(|| {
                let names: Vec<&str> = PostType::ALL.iter().map(|t| t.as_str()).collect();
                Error::validation(format!(
                    "Unknown message type '{value}'; a post is one of {}.",
                    names.join(", ")
                ))
            })
    }
}

/// Host part of a Message-Id when the database name is not a dot-atom (decision R6).
const FALLBACK_HOST: &str = "erpforall.invalid";

/// A validated request: the checks that need no statement.
struct Checked {
    body: String,
    /// attachment ids, deduplicated, in id order
    attachments: Vec<i64>,
    /// explicit recipients and mentioned partners, deduplicated, in id order
    recipients: Vec<i64>,
    subtype_id: Option<i64>,
    subtype_xmlid: Option<(String, String)>,
}

fn invalid_id(what: &str, id: i64) -> Error {
    Error::validation(format!("A {what} id must be a positive integer, not {id}."))
}

/// `ids` deduplicated in ascending order; every id positive, at most [`MAX_IDS_PER_CALL`].
pub(crate) fn checked_partner_ids(ids: &[i64]) -> Result<Vec<i64>> {
    if ids.len() > MAX_IDS_PER_CALL {
        return Err(Error::validation(format!(
            "At most {MAX_IDS_PER_CALL} partner ids are accepted per post, not {}.",
            ids.len()
        )));
    }
    let mut set = BTreeSet::new();
    for id in ids {
        if *id <= 0 {
            return Err(invalid_id("partner", *id));
        }
        set.insert(*id);
    }
    Ok(set.into_iter().collect())
}

/// `ids` deduplicated in ascending order; every id positive, at most [`MAX_ATTACHMENTS`].
fn checked_attachment_ids(ids: &[i64]) -> Result<Vec<i64>> {
    if ids.len() > MAX_ATTACHMENTS {
        return Err(Error::validation(format!(
            "At most {MAX_ATTACHMENTS} attachments are posted with one message, not {}.",
            ids.len()
        )));
    }
    let mut set = BTreeSet::new();
    for id in ids {
        if *id <= 0 {
            return Err(Error::validation(format!(
                "An attachment id must be a positive integer, not {id}."
            )));
        }
        set.insert(*id);
    }
    Ok(set.into_iter().collect())
}

/// HTML escaping of text (`&`, `<`, `>`, `"`, `'`).
fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
}

/// `text` escaped, with each line break (`\r\n`, `\r` or `\n`, one break each) written as `<br>`:
/// the text segments of a body, never the text of a mention anchor.
fn escape_text_into(out: &mut String, text: &str) {
    let mut chars = text.chars().peekable();
    let mut run = String::new();
    while let Some(c) = chars.next() {
        if c == '\r' || c == '\n' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            escape_into(out, &run);
            run.clear();
            out.push_str("<br>");
        } else {
            run.push(c);
        }
    }
    escape_into(out, &run);
}

/// `text` escaped, with each mention's bytes wrapped in an anchor to the partner. Returns the HTML
/// and the mentioned partners. The anchor is built by the server only; nothing of the text can
/// open a tag.
///
/// # Errors
/// `Validation` for more than [`MAX_MENTIONS`] mentions, a span that is empty, out of the text,
/// not on character boundaries or overlapping another, a non-positive partner id, and a mention of
/// anything but a partner (channel, role, `everyone` and `here` mentions come with
/// ).
pub(crate) fn render_text(text: &str, mentions: &[Mention]) -> Result<(String, Vec<i64>)> {
    if mentions.len() > MAX_MENTIONS {
        return Err(Error::validation(format!(
            "At most {MAX_MENTIONS} mentions are accepted per post, not {}.",
            mentions.len()
        )));
    }
    let mut spans: Vec<(usize, usize, i64)> = Vec::with_capacity(mentions.len());
    for mention in mentions {
        let partner = match mention.target {
            MentionTarget::Partner(id) if id > 0 => id,
            MentionTarget::Partner(id) => return Err(invalid_id("partner", id)),
            _ => {
                return Err(Error::validation(
                    "Only partner mentions are accepted on a record thread; channel, role, \
                     everyone and here mentions come with a later release.",
                ));
            }
        };
        let (start, end) = (mention.start, mention.end);
        if start >= end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return Err(Error::validation(format!(
                "The mention {start}..{end} is not a non-empty span of whole characters of the \
                 {}-byte text.",
                text.len()
            )));
        }
        spans.push((start, end, partner));
    }
    spans.sort_unstable();
    for pair in spans.windows(2) {
        if let [(a_start, a_end, _), (b_start, b_end, _)] = pair
            && b_start < a_end
        {
            return Err(Error::validation(format!(
                "The mentions {a_start}..{a_end} and {b_start}..{b_end} overlap."
            )));
        }
    }
    let mut html = String::with_capacity(text.len() + spans.len() * 96);
    let mut at = 0;
    let mut partners = BTreeSet::new();
    for (start, end, partner) in spans {
        escape_text_into(&mut html, &text[at..start]);
        html.push_str("<a class=\"o_mail_redirect\" data-oe-model=\"res.partner\" data-oe-id=\"");
        html.push_str(&partner.to_string());
        html.push_str("\">");
        escape_into(&mut html, &text[start..end]);
        html.push_str("</a>");
        partners.insert(partner);
        at = end;
    }
    escape_text_into(&mut html, &text[at..]);
    Ok((html, partners.into_iter().collect()))
}

/// Whether `host` is an RFC 5322 dot-atom (decision R6).
fn is_dot_atom(host: &str) -> bool {
    const SPECIALS: &str = "!#$%&'*+-/=?^_`{|}~";
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|atom| {
            !atom.is_empty()
                && atom
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || SPECIALS.contains(c))
        })
}

fn message_id(uuid: &str, res_id: i64, model: &str, database: &str) -> String {
    let host = if is_dot_atom(database) {
        database
    } else {
        FALLBACK_HOST
    };
    format!("<{uuid}-{res_id}-{model}@{host}>")
}

/// The checks of a request that need no statement.
fn check_request(request: &PostRequest) -> Result<Checked> {
    let text = match &request.body {
        Body::Text(text) => text,
        Body::Html(_) => {
            return Err(Error::validation(HTML_REFUSED));
        }
    };
    if text.len() > MAX_BODY_BYTES {
        return Err(Error::validation(format!(
            "A post body holds at most {MAX_BODY_BYTES} bytes, not {}.",
            text.len()
        )));
    }
    if let Some(subject) = &request.subject
        && subject.len() > MAX_BODY_BYTES
    {
        return Err(Error::validation(format!(
            "A post subject holds at most {MAX_BODY_BYTES} bytes, not {}.",
            subject.len()
        )));
    }
    if let Some(parent) = request.parent_id
        && parent <= 0
    {
        return Err(invalid_id("parent message", parent));
    }
    if request.client_uid.is_some_and(Uuid::is_nil) {
        return Err(Error::validation("A client_uid must not be the nil UUID."));
    }
    let (body, mentioned) = render_text(text, &request.mentions)?;
    // The limit counts the stored form: the body after escaping and line breaks, and the `<p>` and
    // `</p>` the HTML field's cleaning adds around a body that starts with text (rule R1,
    // `crates/erp-html/src/normalize.rs` `wrap_leading_text`). A body that starts with a tag is not
    // wrapped, so the bound is exact for it and at most 7 bytes high for a body of whitespace
    // text before a tag. The store payload refuses a string over its own limit, so a larger body
    // would be stored and then fail to be answered.
    let stored = body.len() + if body.starts_with('<') { 0 } else { WRAP_BYTES };
    if stored > max_stored_body_bytes() {
        return Err(Error::validation(format!(
            "A post body holds at most {MAX_BODY_BYTES} bytes, not {stored}."
        )));
    }
    let partners = checked_partner_ids(&request.partner_ids)?;
    let total = request
        .attachment_ids
        .len()
        .checked_add(request.staged_uploads.len())
        .ok_or_else(|| Error::validation("Attachment count exceeds the supported range."))?;
    if total > MAX_ATTACHMENTS {
        return Err(Error::validation(format!(
            "At most {MAX_ATTACHMENTS} attachments are posted with one message, not {total}."
        )));
    }
    let mut staged = BTreeSet::new();
    for file in &request.staged_uploads {
        if !staged.insert(file.upload_id.as_str()) {
            return Err(UploadId::not_usable_error());
        }
    }
    let attachments = checked_attachment_ids(&request.attachment_ids)?;
    let mut recipients: BTreeSet<i64> = partners.iter().copied().collect();
    recipients.extend(mentioned);
    if recipients.len() > MAX_IDS_PER_CALL {
        return Err(Error::validation(format!(
            "At most {MAX_IDS_PER_CALL} partners are reached explicitly per post, not {}.",
            recipients.len()
        )));
    }
    let (subtype_id, subtype_xmlid) = match &request.subtype {
        PostSubtype::Note => (None, Some(("mail".to_owned(), "mt_note".to_owned()))),
        PostSubtype::Comment => (None, Some(("mail".to_owned(), "mt_comment".to_owned()))),
        PostSubtype::Id(id) if *id > 0 => (Some(*id), None),
        PostSubtype::Id(id) => return Err(invalid_id("subtype", *id)),
        PostSubtype::Xmlid(xmlid) => match xmlid.split_once('.') {
            Some((module, name))
                if !module.is_empty() && !name.is_empty() && !name.contains('.') =>
            {
                (None, Some((module.to_owned(), name.to_owned())))
            }
            _ => {
                return Err(Error::validation(format!(
                    "A subtype external id is 'module.name', not '{xmlid}'."
                )));
            }
        },
    };
    Ok(Checked {
        body,
        attachments,
        recipients: recipients.into_iter().collect(),
        subtype_id,
        subtype_xmlid,
    })
}

/// What the context statement found.
struct PostContext {
    subtype: Option<(i64, bool)>,
    comment_subtype: Option<i64>,
    user_partner: Option<i64>,
    user_active: bool,
    user_internal: bool,
    author_follows: bool,
    default_subtypes: Vec<i64>,
    parent: Option<i64>,
    replay: Option<Replay>,
    existing_partners: Vec<i64>,
    uuid: String,
    database: String,
}

/// The stored message of a `client_uid`.
struct Replay {
    id: i64,
    create_uid: Option<i64>,
    model: Option<String>,
    res_id: Option<i64>,
    silent: bool,
    notified: Vec<i64>,
}

/// A column of a context row, decoded, or a database error naming the column.
macro_rules! col {
    ($row:expr, $i:expr) => {
        $row.try_get($i)
            .map_err(|error| Error::db(format!("unexpected post context column {}: {error}", $i)))?
    };
}

/// The `ir.model.data` model must be in the registry: subtypes are found by external id.
fn require_model_data(env: &Env) -> Result<()> {
    if env.records("ir.model.data").is_err() {
        return Err(Error::programming(
            "message_post finds subtypes by external id and needs the ir.model.data model in the \
             registry",
        ));
    }
    Ok(())
}

/// The context statement (§5.1 statement 2 of the slice note): subtype, author, parent,
/// `client_uid` replay, partner existence and a fresh uuid, after pending writes are flushed.
#[allow(clippy::too_many_arguments, reason = "one statement's parameters")]
async fn post_context(
    tx: &mut Transaction,
    model: &str,
    res_id: i64,
    uid: i64,
    checked: &Checked,
    parent: Option<i64>,
    flat: bool,
    client_uid: Option<Uuid>,
) -> Result<PostContext> {
    tx.flush().await?;
    let (module, name) = match &checked.subtype_xmlid {
        Some((module, name)) => (Some(module.as_str()), Some(name.as_str())),
        None => (None, None),
    };
    let rows = tx
        .query(
            "WITH me AS (SELECT u.partner_id::int8 AS partner, u.active, u.share FROM res_users u \
             WHERE u.id = $6::int8), \
             chosen AS (SELECT s.id::int8 AS id, s.internal FROM mail_message_subtype s \
             WHERE s.id = COALESCE($3::int8, (SELECT d.res_id::int8 FROM ir_model_data d \
             WHERE d.module = $4::text AND d.name = $5::text AND d.model = 'mail.message.subtype')) \
             AND (s.res_model IS NULL OR s.res_model = $1::text)), \
             hit AS (SELECT m.id::int8 AS id, m.create_uid::int8 AS create_uid, m.model::text AS model, \
             m.res_id::int8 AS res_id, m.silent FROM mail_message m \
             WHERE $9::uuid IS NOT NULL AND m.client_uid = $9::uuid) \
             SELECT (SELECT id FROM chosen), (SELECT internal FROM chosen), \
             (SELECT d.res_id::int8 FROM ir_model_data d WHERE d.module = 'mail' \
             AND d.name = 'mt_comment' AND d.model = 'mail.message.subtype'), \
             (SELECT partner FROM me), (SELECT active FROM me), (SELECT share FROM me), \
             EXISTS (SELECT 1 FROM mail_followers f JOIN me ON f.partner_id = me.partner \
             WHERE f.res_model = $1::text AND f.res_id = $2::int8), \
             ARRAY(SELECT s.id::int8 FROM mail_message_subtype s WHERE s.\"default\" \
             AND (s.res_model IS NULL OR s.res_model = $1::text) ORDER BY s.sequence, s.id), \
             COALESCE((SELECT m.id::int8 FROM mail_message m WHERE m.id = $7::int8 \
             AND m.model = $1::text AND m.res_id = $2::int8 \
             AND (NOT COALESCE((SELECT share FROM me), false) OR (NOT COALESCE(m.is_internal, false) \
             AND m.message_type <> 'tracking' AND NOT EXISTS \
             (SELECT 1 FROM mail_message_subtype x WHERE x.id = m.subtype_id AND x.internal)))), \
             (SELECT t.id FROM (SELECT m.id::int8 AS id, m.message_type, m.date, m.create_date \
             FROM mail_message m WHERE $8::bool AND m.model = $1::text AND m.res_id = $2::int8 \
             AND m.message_type <> 'user_notification' \
             AND (NOT COALESCE((SELECT share FROM me), false) OR (NOT COALESCE(m.is_internal, false) \
             AND m.message_type <> 'tracking' AND NOT EXISTS \
             (SELECT 1 FROM mail_message_subtype x WHERE x.id = m.subtype_id AND x.internal))) \
             ORDER BY m.date DESC, m.id DESC LIMIT 200) t \
             ORDER BY (t.message_type IN ('comment', 'email')) DESC, \
             COALESCE(t.date, t.create_date) DESC NULLS LAST, t.id DESC LIMIT 1)), \
             (SELECT id FROM hit), (SELECT create_uid FROM hit), (SELECT model FROM hit), \
             (SELECT res_id FROM hit), (SELECT silent FROM hit), \
             ARRAY(SELECT r.partner_id::int8 FROM mail_message_notified_rel r JOIN hit \
             ON r.message_id = hit.id ORDER BY r.partner_id), \
             ARRAY(SELECT p.id::int8 FROM res_partner p WHERE p.id = ANY($10::int8[]) ORDER BY p.id), \
             gen_random_uuid()::text, current_database()::text",
            &[
                &model,
                &res_id,
                &checked.subtype_id,
                &module,
                &name,
                &uid,
                &parent,
                &flat,
                &client_uid,
                &checked.recipients,
            ],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::invariant("the post context statement returned no row"))?;
    let subtype_id: Option<i64> = col!(row, 0);
    let subtype_internal: Option<bool> = col!(row, 1);
    let subtype = match (subtype_id, subtype_internal) {
        (Some(id), internal) => Some((id, internal.unwrap_or(false))),
        (None, _) => None,
    };
    let hit_id: Option<i64> = col!(row, 9);
    let replay = match hit_id {
        Some(id) => Some(Replay {
            id,
            create_uid: col!(row, 10),
            model: col!(row, 11),
            res_id: col!(row, 12),
            silent: {
                let silent: Option<bool> = col!(row, 13);
                silent.unwrap_or(false)
            },
            notified: col!(row, 14),
        }),
        None => None,
    };
    let user_active: Option<bool> = col!(row, 4);
    let user_share: Option<bool> = col!(row, 5);
    Ok(PostContext {
        subtype,
        comment_subtype: col!(row, 2),
        user_partner: col!(row, 3),
        user_active: user_active == Some(true),
        user_internal: user_active == Some(true) && user_share == Some(false),
        author_follows: col!(row, 6),
        default_subtypes: col!(row, 7),
        parent: col!(row, 8),
        replay,
        existing_partners: col!(row, 15),
        uuid: col!(row, 16),
        database: col!(row, 17),
    })
}

async fn recipients(
    tx: &mut Transaction,
    model: &str,
    res_id: i64,
    subtype: (i64, bool),
    explicit: &[i64],
    author: Option<i64>,
) -> Result<Vec<i64>> {
    let rows = tx
        .query(
            "SELECT p.id::int8 FROM res_partner p WHERE p.id IN (\
             SELECT f.partner_id FROM mail_followers f \
             JOIN mail_followers_mail_message_subtype_rel r ON r.mail_followers_id = f.id \
             AND r.mail_message_subtype_id = $3::int8 \
             WHERE f.res_model = $1::text AND f.res_id = $2::int8 \
             AND (NOT $4::bool OR EXISTS (SELECT 1 FROM res_users u WHERE u.partner_id = f.partner_id \
             AND u.active AND u.share = false)) \
             UNION ALL SELECT unnest($5::int8[])) \
             AND p.active AND ($6::int8 IS NULL OR p.id <> $6::int8) ORDER BY p.id",
            &[&model, &res_id, &subtype.0, &subtype.1, &explicit, &author],
        )
        .await?;
    rows.iter()
        .map(|row| {
            row.try_get::<_, i64>(0)
                .map_err(|error| Error::db(format!("unexpected recipient id: {error}")))
        })
        .collect()
}

fn rec_ids(ids: &[i64], what: &str) -> Result<Vec<RecId>> {
    ids.iter()
        .map(|id| {
            RecId::from_real(*id)
                .ok_or_else(|| Error::invariant(format!("{what} id {id} is not a real id")))
        })
        .collect()
}

/// The message row of a post. Privileged: `mail.message` is not writable by every user who may
/// post (message access is a later slice); the caller has checked access to the record.
async fn create_message(tx: &mut Transaction, env: &Env, vals: RawVals) -> Result<i64> {
    let created = env
        .records("mail.message")?
        .sudo(true)
        .create(tx, vec![vals])
        .await?;
    created
        .ids()
        .first()
        .copied()
        .ok_or_else(|| Error::invariant("creating a message returned no id"))
}

/// Make the author follow the record with the model's default subtypes.
/// Privileged: follower rows are not writable by every user who may post; the caller has checked
/// access to the record and that the author is an active internal user who does not follow yet.
async fn subscribe_author(
    tx: &mut Transaction,
    record: &Records,
    author: i64,
    default_subtypes: &[i64],
) -> Result<()> {
    let res_id = single_id(record)?;
    record
        .env()
        .records("mail.followers")?
        .sudo(true)
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("res_model", Cell::Str(record.model_name().into()))
                    .set_cell("res_id", Cell::Id(res_id))
                    .set_cell("partner_id", Cell::Id(author))
                    .set_commands(
                        "subtype_ids",
                        [Command::set(rec_ids(default_subtypes, "subtype")?)],
                    ),
            ],
        )
        .await?;
    Ok(())
}

/// A message has an author: a user without a `res.users` row or without a partner cannot post.
fn missing_author(uid: i64) -> Error {
    let gap = if uid == erp_sdk::SUPERUSER_ID.get() {
        " The superuser's partner link comes with a later release."
    } else {
        ""
    };
    Error::invariant(format!(
        "User {uid} has no partner (no res.users row, or its partner_id is empty): a message \
         needs an author partner.{gap}"
    ))
}

fn single_id(record: &Records) -> Result<i64> {
    match record.ids().as_slice() {
        [id] if *id > 0 => Ok(*id),
        [id] => Err(Error::validation(format!(
            "A message is posted on a saved record, not on {} {id}.",
            record.model_name()
        ))),
        ids => Err(Error::validation(format!(
            "A message is posted on exactly one record, not {} (the reference ERP's message_post needs a \
             singleton).",
            ids.len()
        ))),
    }
}

/// `mail.thread` `message_post` (spec §4.2): see the module documentation and
/// [`crate::MailThread`]'s method.
pub(crate) async fn message_post(
    tx: &mut Transaction,
    record: &Records,
    request: PostRequest,
) -> Result<Posted> {
    if request.staged_uploads.is_empty() {
        return message_post_inner(tx, record, request).await;
    }
    tx.savepoint(async |tx| message_post_inner(tx, record, request).await)
        .await
}

async fn message_post_inner(
    tx: &mut Transaction,
    record: &Records,
    request: PostRequest,
) -> Result<Posted> {
    let res_id = single_id(record)?;
    let mut checked = check_request(&request)?;
    let env = record.env().clone();
    require_model_data(&env)?;
    let model = record.model_name().to_owned();
    let operation: Operation = record.call(tx, "_mail_post_access", ()).await?;
    record.check_access(tx, operation).await?;
    let flat: bool = record.call(tx, "_mail_flat_thread", ()).await?;
    let context = post_context(
        tx,
        &model,
        res_id,
        env.uid(),
        &checked,
        request.parent_id,
        flat,
        request.client_uid,
    )
    .await?;
    if let Some(replay) = &context.replay {
        let same = replay.create_uid == Some(env.uid())
            && replay.model.as_deref() == Some(model.as_str())
            && replay.res_id == Some(res_id);
        if !same {
            return Err(Error::validation(
                "This client_uid was used for another post; choose a new one.",
            ));
        }
        return Ok(Posted {
            message_id: replay.id,
            replayed: true,
            notified_partner_ids: replay.notified.clone(),
            silent: replay.silent,
        });
    }
    let subtype = match (context.subtype, &request.subtype) {
        (Some(subtype), _) => subtype,
        (None, PostSubtype::Id(id)) => {
            return Err(Error::validation(format!(
                "Unknown subtype id {id} for {model}: a subtype must exist and have no model or \
                 the model of the record."
            )));
        }
        (None, PostSubtype::Xmlid(xmlid)) => {
            return Err(Error::validation(format!(
                "Unknown subtype '{xmlid}' for {model}: a subtype must exist and have no model or \
                 the model of the record."
            )));
        }
        (None, _) => {
            return Err(Error::invariant(
                "the mail subtypes mt_note and mt_comment are missing: the mail addon's data is \
                 not loaded",
            ));
        }
    };
    let author = context
        .user_partner
        .ok_or_else(|| missing_author(env.uid()))?;
    let existing: BTreeSet<i64> = context.existing_partners.iter().copied().collect();
    let unknown: Vec<String> = checked
        .recipients
        .iter()
        .filter(|id| !existing.contains(id))
        .map(ToString::to_string)
        .collect();
    if !unknown.is_empty() {
        return Err(Error::validation(format!(
            "Unknown partner ids: {}.",
            unknown.join(", ")
        )));
    }
    if request.options.autofollow_recipients && !checked.recipients.is_empty() {
        record
            .call::<_, ()>(tx, "follow", (checked.recipients.clone(), None::<Vec<i64>>))
            .await?;
    }
    if !request.staged_uploads.is_empty() {
        let created = attachments::create(
            tx,
            request
                .staged_uploads
                .iter()
                .map(StagedAttachment::item)
                .collect(),
        )
        .await?;
        checked
            .attachments
            .extend(created.iter().map(|id| id.get()));
        checked.attachments.sort_unstable();
    }
    attachments::adopt(
        tx,
        &checked.attachments,
        ParentRef::new(model.as_str(), res_id),
        &[COMPOSER_MODEL],
        if context.user_internal {
            AdoptScope::ComposerAndRecord
        } else {
            AdoptScope::ComposerOnly
        },
    )
    .await?;
    tx.flush().await?;
    let notified = recipients(
        tx,
        &model,
        res_id,
        subtype,
        &checked.recipients,
        Some(author),
    )
    .await?;
    let mut vals = RawVals::new()
        .set_cell("model", Cell::Str(model.as_str().into()))
        .set_cell("res_id", Cell::Id(res_id))
        .set_cell("body", Cell::Str(checked.body.as_str().into()))
        .set_cell(
            "message_type",
            Cell::Str(request.message_type.as_str().into()),
        )
        .set_cell("subtype_id", Cell::Id(subtype.0))
        .set_cell(
            "message_id",
            Cell::Str(message_id(&context.uuid, res_id, &model, &context.database).into()),
        )
        .set_cell("silent", Cell::Bool(request.silent));
    if let Some(subject) = &request.subject {
        vals = vals.set_cell("subject", Cell::Str(subject.as_str().into()));
    }
    if let Some(parent) = context.parent {
        vals = vals.set_cell("parent_id", Cell::Id(parent));
    }
    vals = vals.set_cell("author_id", Cell::Id(author));
    if let Some(uid) = request.client_uid {
        vals = vals.set_cell("client_uid", Cell::Uuid(uid));
    }
    if let Some(type_id) = request.mail_activity_type_id {
        vals = vals.set_cell("mail_activity_type_id", Cell::Id(type_id));
    }
    if !checked.recipients.is_empty() {
        vals = vals.set_commands(
            "partner_ids",
            [Command::set(rec_ids(&checked.recipients, "partner")?)],
        );
    }
    if !checked.attachments.is_empty() {
        vals = vals.set_commands(
            "attachment_ids",
            [Command::set(rec_ids(&checked.attachments, "attachment")?)],
        );
    }
    if !notified.is_empty() {
        vals = vals.set_commands(
            "notified_partner_ids",
            [Command::set(rec_ids(&notified, "partner")?)],
        );
    }
    let message = create_message(tx, &env, vals).await?;
    let author_subscribe = !request.options.skip_author_subscribe
        && request.message_type.subscribes_author()
        && context.comment_subtype == Some(subtype.0)
        && context.user_active
        && context.user_internal;
    if author_subscribe {
        if request.options.autofollow_recipients {
            // the followers changed after the context statement: the rows decide
            insert_followers(
                tx,
                record,
                &[author],
                &SubtypeChoice::Explicit(context.default_subtypes.clone()),
                ExistingPolicy::Skip,
            )
            .await?;
        } else if !context.author_follows {
            subscribe_author(tx, record, author, &context.default_subtypes).await?;
        }
    }
    record
        .call::<_, ()>(tx, "_message_post_after_hook", (message,))
        .await?;
    Ok(Posted {
        message_id: message,
        replayed: false,
        notified_partner_ids: notified,
        silent: request.silent,
    })
}

/// `mail.thread` `_message_log_batch`: one internal note per record of `records`
/// with its body from `bodies` (escaped text; an empty body for a record without one), subtype
/// `mail.mt_note`, authored by the current user's partner, created in one ORM call and notifying
/// nobody.
///
/// # Errors
/// `Validation` for an unsaved (non-positive) or repeated id in `records`, a body key that is not
/// a record of `records`, more than [`MAX_IDS_PER_CALL`] records or a body over
/// [`MAX_BODY_BYTES`]; `Invariant` when `mail.mt_note` is missing or the user has no partner.
pub(crate) async fn message_log_batch(
    tx: &mut Transaction,
    records: &Records,
    bodies: BTreeMap<i64, String>,
) -> Result<Vec<i64>> {
    // `ids()` skips new records and maps new-with-origin to the origin: the raw ids decide
    let unsaved: Vec<String> = records
        .rec_ids()
        .iter()
        .filter(|id| id.as_real().is_none())
        .map(ToString::to_string)
        .collect();
    if !unsaved.is_empty() {
        return Err(Error::validation(format!(
            "Only saved {} records are logged; unsaved ids: {}.",
            records.model_name(),
            unsaved.join(", ")
        )));
    }
    let ids = records.ids();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > MAX_IDS_PER_CALL {
        return Err(Error::validation(format!(
            "At most {MAX_IDS_PER_CALL} records are logged per call, not {}.",
            ids.len()
        )));
    }
    let own: BTreeSet<i64> = ids.iter().copied().collect();
    if own.len() != ids.len() {
        let mut seen = BTreeSet::new();
        let duplicated: BTreeSet<String> = ids
            .iter()
            .filter(|id| !seen.insert(**id))
            .map(ToString::to_string)
            .collect();
        return Err(Error::validation(format!(
            "A record is logged once per call; repeated {} ids: {}.",
            records.model_name(),
            duplicated.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    if let Some(stray) = bodies.keys().find(|id| !own.contains(id)) {
        return Err(Error::validation(format!(
            "A log body is given for {} {stray}, which is not one of the records logged.",
            records.model_name()
        )));
    }
    for body in bodies.values() {
        if body.len() > MAX_BODY_BYTES {
            return Err(Error::validation(format!(
                "A log body holds at most {MAX_BODY_BYTES} bytes, not {}.",
                body.len()
            )));
        }
    }
    let env = records.env().clone();
    require_model_data(&env)?;
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT (SELECT d.res_id::int8 FROM ir_model_data d WHERE d.module = 'mail' \
             AND d.name = 'mt_note' AND d.model = 'mail.message.subtype'), \
             (SELECT u.partner_id::int8 FROM res_users u WHERE u.id = $1::int8), \
             gen_random_uuid()::text, current_database()::text",
            &[&env.uid()],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::invariant("the log context statement returned no row"))?;
    let note: Option<i64> = col!(row, 0);
    let note = note.ok_or_else(|| {
        Error::invariant("the mail subtype mt_note is missing: the mail addon's data is not loaded")
    })?;
    let author: Option<i64> = col!(row, 1);
    let author = author.ok_or_else(|| missing_author(env.uid()))?;
    let uuid: String = col!(row, 2);
    let database: String = col!(row, 3);
    let host = if is_dot_atom(&database) {
        database.as_str()
    } else {
        FALLBACK_HOST
    };
    let message_id = format!("<{uuid}-log@{host}>");
    let mut vals = Vec::with_capacity(ids.len());
    for id in &ids {
        let mut body = String::new();
        if let Some(text) = bodies.get(id) {
            escape_into(&mut body, text);
        }
        let row = RawVals::new()
            .set_cell("model", Cell::Str(records.model_name().into()))
            .set_cell("res_id", Cell::Id(*id))
            .set_cell("body", Cell::Str(body.into()))
            .set_cell("message_type", Cell::Str("notification".into()))
            .set_cell("is_internal", Cell::Bool(true))
            .set_cell("subtype_id", Cell::Id(note))
            .set_cell("message_id", Cell::Str(message_id.as_str().into()))
            .set_cell("author_id", Cell::Id(author));
        vals.push(row);
    }
    log_messages(tx, &env, vals).await
}

/// The rows of a log batch. Privileged: see [`message_log_batch`].
async fn log_messages(tx: &mut Transaction, env: &Env, vals: Vec<RawVals>) -> Result<Vec<i64>> {
    Ok(env
        .records("mail.message")?
        .sudo(true)
        .create(tx, vals)
        .await?
        .ids())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use erp_sdk::ErrorKind;

    fn partner(start: usize, end: usize, id: i64) -> Mention {
        Mention {
            start,
            end,
            target: MentionTarget::Partner(id),
        }
    }

    #[test]
    fn text_is_escaped_and_mentions_become_anchors() {
        let text = "Hi @Ann <b>&'\"";
        let (html, partners) = render_text(text, &[partner(3, 7, 9)]).unwrap();
        assert_eq!(
            html,
            "Hi <a class=\"o_mail_redirect\" data-oe-model=\"res.partner\" data-oe-id=\"9\">@Ann</a> \
             &lt;b&gt;&amp;&#39;&#34;"
        );
        assert_eq!(partners, vec![9]);
    }

    #[test]
    fn mention_spans_are_validated() {
        for (mentions, message) in [
            (
                vec![partner(2, 2, 1)],
                "The mention 2..2 is not a non-empty span of whole characters of the 5-byte text.",
            ),
            (
                vec![partner(0, 9, 1)],
                "The mention 0..9 is not a non-empty span of whole characters of the 5-byte text.",
            ),
            (
                vec![partner(0, 3, 1), partner(2, 4, 2)],
                "The mentions 0..3 and 2..4 overlap.",
            ),
            (
                vec![partner(0, 1, 0)],
                "A partner id must be a positive integer, not 0.",
            ),
        ] {
            let error = render_text("hello", &mentions).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(error.message(), message);
        }
        // not on a character boundary: 'é' is two bytes
        let error = render_text("é", &[partner(0, 1, 1)]).unwrap_err();
        assert_eq!(
            error.message(),
            "The mention 0..1 is not a non-empty span of whole characters of the 2-byte text."
        );
        let error = render_text(
            "@all",
            &[Mention {
                start: 0,
                end: 4,
                target: MentionTarget::Everyone,
            }],
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Only partner mentions are accepted on a record thread; channel, role, everyone and \
             here mentions come with a later release."
        );
        let many: Vec<Mention> = (0..=MAX_MENTIONS).map(|i| partner(i, i + 1, 1)).collect();
        let error = render_text(&"x".repeat(200), &many).unwrap_err();
        assert_eq!(
            error.message(),
            "At most 100 mentions are accepted per post, not 101."
        );
    }

    #[test]
    fn message_id_host_is_the_database_when_it_is_a_dot_atom() {
        assert_eq!(
            message_id("u", 7, "project.task", "erp_db"),
            "<u-7-project.task@erp_db>"
        );
        assert_eq!(
            message_id("u", 7, "project.task", "bad name"),
            "<u-7-project.task@erpforall.invalid>"
        );
        assert!(!is_dot_atom("a..b"));
        assert!(!is_dot_atom(""));
        assert!(is_dot_atom("a.b-c"));
    }
}
