//! The `thread` and `thread.messages` store topics (design note
//! , spec cases to, guide
//! "Add chatter to a model").
//!
//! `thread.messages` is a keyset page of a record thread's messages
//! (`before`, `after` or `around` a message id, newest first, at most 100); `thread` is the
//! chatter's first paint: the facts about the thread (what the caller may do, the default
//! subject), optionally its first message page and its follower panel.
//!
//! Messages are listed through the caller's own `mail.message` read rules (the `thread_access`
//! operator of `security/ir.access.csv`); a thread the caller may not read is denied before any
//! message is looked at.
//! Nothing here writes; marking needaction messages read is a separate call (4).

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::Operation;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::store_wire::FieldValue as Wire;

use crate::message_attachments::{self, MAX_PANEL_ATTACHMENTS};
use crate::store::{self, FOLLOWERS, MESSAGE, SUBTYPE, StoreFields};
use crate::store_fetch::{
    Audience, ParamDecl, StoreTopic, TopicFuture, TopicInput, TopicOutput, TopicParams,
};
use crate::thread_rpc::{Standing, rows_of, standing};
use crate::{MailMessage, MailThread};

/// Most messages one page holds.
pub const MAX_MESSAGE_PAGE: i64 = 100;
pub const DEFAULT_MESSAGE_PAGE: i64 = 30;
/// Longest thread model name a topic accepts.
const MAX_MODEL_BYTES: usize = 128;
const SUBJECT_WIDTH: usize = 100;

const PUBLIC_DENIED: &str = "Public users cannot load a thread.";
const THREAD_DENIED: &str =
    "You are not allowed to read this thread: it does not exist or you may not read it.";

/// The panels of the chatter's request list (SV-04) that
/// no slice implements yet, and the slice or area that adds each. A caller asking for one gets a
/// `Validation` error naming it, never a payload without it.
const UNAVAILABLE_PANELS: [(&str, &str); 5] = [
    ("activities", "the mail.activity slice"),
    ("scheduled_messages", "the scheduled-messages slice"),
    ("suggested_recipients", "the suggested-recipients slice"),
    (
        "contact_fields",
        "the chatter contact-fields slice (gap G-20)",
    ),
    ("has_pinned_messages", "the message-pin slice (gap G-20)"),
];

const THREAD_PARAMS: &[ParamDecl] = &[
    ParamDecl::text("model", MAX_MODEL_BYTES),
    ParamDecl::int("id", 1, i64::MAX),
    ParamDecl::boolean("messages").optional(),
    ParamDecl::int("limit", 1, MAX_MESSAGE_PAGE).optional(),
    ParamDecl::boolean("followers").optional(),
    ParamDecl::int("followers_limit", 1, MAX_FOLLOWER_LIMIT).optional(),
    ParamDecl::boolean("default_subject").optional(),
    ParamDecl::boolean("attachments").optional(),
    ParamDecl::int("attachments_limit", 1, MAX_PANEL_ATTACHMENTS).optional(),
    ParamDecl::boolean("activities").optional(),
    ParamDecl::boolean("scheduled_messages").optional(),
    ParamDecl::boolean("suggested_recipients").optional(),
    ParamDecl::boolean("contact_fields").optional(),
    ParamDecl::boolean("has_pinned_messages").optional(),
];

const MESSAGES_PARAMS: &[ParamDecl] = &[
    ParamDecl::text("model", MAX_MODEL_BYTES),
    ParamDecl::int("id", 1, i64::MAX),
    ParamDecl::int("before", 1, i64::MAX).optional(),
    ParamDecl::int("after", 1, i64::MAX).optional(),
    ParamDecl::int("around", 1, i64::MAX).optional(),
    ParamDecl::int("limit", 1, MAX_MESSAGE_PAGE).optional(),
];

/// Most followers the `thread` topic's follower panel holds.
const MAX_FOLLOWER_LIMIT: i64 = 100;

crate::store_topic!(
    /// `thread`: the chatter's first paint of one record thread (facts, first page, followers).
    /// `limit`, `followers_limit` and `attachments_limit` are ignored when `messages`,
    /// `followers` or `attachments` is off.
    pub THREAD_TOPIC = StoreTopic::new("mail", "thread", run_thread)
        .audience(Audience::LoggedIn)
        .params(THREAD_PARAMS)
        .access(check_thread)
        .needs_groups()
        .statements(11)
);

crate::store_topic!(
    /// `thread.messages`: one keyset page of a record thread's messages.
    pub THREAD_MESSAGES_TOPIC = StoreTopic::new("mail", "thread.messages", run_messages)
        .audience(Audience::LoggedIn)
        .params(MESSAGES_PARAMS)
        .access(check_messages)
        .needs_groups()
        .statements(6)
        // `around` runs two searches, one per side of the cursor
        .statements_plus(&[("around", 1)])
);

// ----- the dispatched extension points ------------------------------------------------------

/// What the thread facts of `mail.thread._store_thread_fields` carry: the panels the caller requested and
/// the facts set so far. A fact is set once: an override that sets a name the base (or another
/// override) set is a `Programming` error, never a silent replacement.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadFacts {
    requested: BTreeSet<String>,
    values: BTreeMap<String, Json>,
}

impl ThreadFacts {
    /// Facts for a request that asked for `requested`.
    #[must_use]
    pub fn new(requested: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            requested: requested.into_iter().map(Into::into).collect(),
            values: BTreeMap::new(),
        }
    }

    /// Whether the caller asked for `panel` (a parameter of the `thread` topic).
    #[must_use]
    pub fn requested(&self, panel: &str) -> bool {
        self.requested.contains(panel)
    }

    /// Set the fact `name`.
    ///
    /// # Errors
    /// `Programming` when `name` is set already.
    pub fn set(&mut self, name: &str, value: Json) -> Result<()> {
        if self.values.insert(name.to_owned(), value).is_some() {
            return Err(Error::programming(format!(
                "the thread fact '{name}' is set twice; an override adds facts, it does not \
                 replace the base ones"
            )));
        }
        Ok(())
    }

    /// The fact `name`, if set.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Json> {
        self.values.get(name)
    }

    fn into_json(self) -> Json {
        Json::Object(self.values.into_iter().collect())
    }
}

/// The store entry of messages (dispatched `mail.message._to_store`).
#[extend(MailMessage)]
pub struct MailMessageStore {}

#[methods(MailMessage, addon = "mail")]
impl MailMessageStore {
    /// Stage the store entry of these messages: the message fields of the wire schema (the
    /// recipients `partner_ids` excluded: a page does not carry them) and the subtypes the
    /// messages name (`name`, `internal`). An override calls `sup` and adds or adjusts fields of
    /// the returned set; a field the store schema does not declare fails the topic that carries
    /// it.
    ///
    /// The files posted with the messages come with them: each message's `attachment_ids` and one
    /// `ir.attachment` record per file (metadata only), from one statement
    /// ([`crate::message_attachments`]).
    ///
    /// The recordset is trusted: the caller has found it through its own `mail.message` read
    /// rules (the topics do), and the columns are read with privileges, because those rules
    /// depend on the thread documents and cannot be remembered, so a read as the caller would
    /// ask the database to decide again. A message the caller sees never names a subtype the
    /// caller may not see (the portal and public rows require a non-internal subtype).
    ///
    /// # Errors
    /// `Invariant` for a value of an unexpected shape.
    #[method]
    async fn _to_store(
        this: &RecordSet<MailMessage>,
        tx: &mut Transaction,
        mut fields: StoreFields,
    ) -> Result<StoreFields> {
        let records = this.as_records();
        if records.is_empty() {
            return Ok(fields);
        }
        let trusted = records.sudo(true);
        let rows = rows_of(tx, &trusted, &store::MESSAGE_PAGE_FIELDS).await?;
        let message_ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        message_attachments::stage_of_messages(tx, &message_ids, &mut fields).await?;
        let mut subtypes = BTreeSet::new();
        store::stage_partners(
            tx,
            this.as_records().env(),
            store::partner_refs(rows.iter().map(|(_, r)| r)),
            &mut fields,
        )
        .await?;
        for (id, row) in &rows {
            let model = match row.get("model") {
                Some(Value::Str(model)) => model.to_string(),
                other => {
                    return Err(Error::invariant(format!(
                        "message {id} has no thread model: {other:?}"
                    )));
                }
            };
            let (uid, entries) = store::message_entries(*id, row, &model, false)?;
            if let Some(Value::Int(subtype)) = row.get("subtype_id") {
                subtypes.insert(*subtype);
            }
            fields.extend(MESSAGE, uid, entries);
        }
        if !subtypes.is_empty() {
            let found = trusted.env().records(SUBTYPE)?.browse(subtypes)?;
            for (id, row) in rows_of(tx, &found, &store::SUBTYPE_FIELDS).await? {
                let (uid, entries) = store::subtype_entries(id, &row)?;
                fields.extend(SUBTYPE, uid, entries);
            }
        }
        Ok(fields)
    }
}

#[extend(MailThread)]
pub struct MailThreadStore {}

#[methods(MailThread, addon = "mail")]
impl MailThreadStore {
    /// Set the facts about one record for the caller: `can_write` and `can_post` (whether the
    /// caller's rules allow the write, and the operation `_mail_post_access` names, on the record)
    /// and, when `default_subject` is requested, the default subject of a post. An override
    /// calls `sup` and adds facts with [`ThreadFacts::set`].
    ///
    /// # Errors
    /// `Validation` for anything but one record.
    #[method]
    async fn _store_thread_fields(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        mut facts: ThreadFacts,
    ) -> Result<ThreadFacts> {
        let record = this.as_records().ensure_one()?;
        let can_write = !record
            .filtered_access(tx, Operation::Write)
            .await?
            .is_empty();
        facts.set("can_write", json!(can_write))?;
        let post: Operation = record.call(tx, "_mail_post_access", ()).await?;
        let can_post = !record.filtered_access(tx, post).await?.is_empty();
        facts.set("can_post", json!(can_post))?;
        if facts.requested("default_subject") {
            let rows = rows_of(tx, record, &["display_name"]).await?;
            let name = match rows.first().and_then(|(_, row)| row.get("display_name")) {
                Some(Value::Str(name)) => name.to_string(),
                Some(Value::False) => String::new(),
                other => {
                    return Err(Error::invariant(format!(
                        "the display name of a thread is not text: {other:?}"
                    )));
                }
            };
            facts.set("default_subject", json!(shorten(&name, SUBJECT_WIDTH)))?;
        }
        Ok(facts)
    }

    /// A model
    /// overrides it to show more or fewer.
    #[method]
    async fn _get_customer_portal_message_types(
        _this: &RecordSet<MailThread>,
        _tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        Ok(["auto_comment", "comment", "email", "email_outgoing"]
            .into_iter()
            .map(str::to_owned)
            .collect())
    }
}

/// Python's `textwrap.shorten(text, width, placeholder="...")`: the words of `text` joined by one
/// space, cut at a word boundary with `...` appended when the text is wider than `width`.
fn shorten(text: &str, width: usize) -> String {
    const PLACEHOLDER: &str = "...";
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= width {
        return joined;
    }
    let budget = width.saturating_sub(PLACEHOLDER.len());
    let mut out = String::new();
    let mut used = 0;
    for word in joined.split(' ') {
        let add = word.chars().count() + usize::from(!out.is_empty());
        if used + add > budget {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        used += add;
    }
    out.push_str(PLACEHOLDER);
    out
}

// ----- parameters and access ------------------------------------------------------------------

fn thread_ref(params: &TopicParams) -> Result<(&str, i64)> {
    Ok((params.text("model")?, params.int("id")?))
}

/// The thread model must be a concrete model that inherits `mail.thread`.
fn require_thread_model(env: &Env, model: &str) -> Result<()> {
    let ok = env
        .registry()
        .model(model)
        .is_some_and(|spec| !spec.is_abstract() && spec.inherits("mail.thread"));
    if ok {
        return Ok(());
    }
    Err(Error::validation(format!(
        "'{}' is not a model with a message thread: it does not exist, is not installed or does \
         not inherit mail.thread.",
        crate::store_fetch::bounded(model)
    )))
}

/// The thread's read access, the part both topics share: the model must be a thread model, a
/// public caller is refused, and the caller must be allowed to read the record. A missing
/// record and an unreadable one are refused alike, with one statement and one message.
async fn require_readable_thread(tx: &mut Transaction, input: &TopicInput<'_>) -> Result<()> {
    let env = input.env();
    let (model, id) = thread_ref(input.params())?;
    require_thread_model(env, model)?;
    if standing(tx, env).await? == Standing::Public {
        return Err(Error::access(PUBLIC_DENIED));
    }
    let record = env.records(model)?.browse([id])?;
    if record
        .filtered_access(tx, Operation::Read)
        .await?
        .is_empty()
    {
        return Err(Error::access(THREAD_DENIED));
    }
    Ok(())
}

fn check_messages<'a>(tx: &'a mut Transaction, input: &'a TopicInput<'a>) -> TopicFuture<'a> {
    Box::pin(async move {
        cursor_of(input.params())?;
        require_readable_thread(tx, input).await
    })
}

fn check_thread<'a>(tx: &'a mut Transaction, input: &'a TopicInput<'a>) -> TopicFuture<'a> {
    Box::pin(async move {
        for (panel, owner) in UNAVAILABLE_PANELS {
            if input.params().opt_boolean(panel)?.unwrap_or(false) {
                return Err(Error::validation(format!(
                    "thread: the panel '{panel}' is not available yet; it comes with {owner}."
                )));
            }
        }
        require_readable_thread(tx, input).await
    })
}

// ----- the message page -----------------------------------------------------------------------

/// Where a page starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cursor {
    /// The newest messages.
    Latest,
    /// The messages older than the id.
    Before(i64),
    /// The messages newer than the id.
    After(i64),
    /// The messages around the id: it and the older ones, then the newer ones.
    Around(i64),
}

fn cursor_of(params: &TopicParams) -> Result<Cursor> {
    let before = params.opt_int("before")?;
    let after = params.opt_int("after")?;
    let around = params.opt_int("around")?;
    match (before, after, around) {
        (None, None, None) => Ok(Cursor::Latest),
        (Some(id), None, None) => Ok(Cursor::Before(id)),
        (None, Some(id), None) => Ok(Cursor::After(id)),
        (None, None, Some(id)) => Ok(Cursor::Around(id)),
        _ => Err(Error::validation(
            "thread.messages: pass at most one of before, after and around.",
        )),
    }
}

/// Polls, the fourth term, do not exist
/// here yet.
fn empty_messages() -> Result<Domain> {
    let empty_body = Domain::condition("body", Operator::Eq, false)?
        | Domain::condition("body", Operator::Eq, "")?
        | Domain::condition(
            "body",
            Operator::EqLike,
            "<span class=\"o-mail-Message-edited\"%></span>",
        )?;
    Ok(Domain::condition("attachment_ids", Operator::Eq, false)?
        & empty_body
        & Domain::condition("subtype_id.description", Operator::Eq, false)?)
}

/// One page: the ids newest first and what lies beyond it.
#[derive(Debug, PartialEq, Eq)]
struct Page {
    ids: Vec<i64>,
    has_more_before: bool,
    has_more_after: bool,
}

impl Page {
    fn document(&self) -> Json {
        json!({
            "message_ids": self.ids,
            "has_more_before": self.has_more_before,
            "has_more_after": self.has_more_after,
        })
    }
}

/// The columns the page search fetches with the ids, so the read of the same fields costs no
/// statement of its own.
const PAGE_COLUMNS: [&str; 10] = [
    "body",
    "subject",
    "message_type",
    "subtype_id",
    "author_id",
    "parent_id",
    "date",
    "is_internal",
    "model",
    "res_id",
];

async fn search_ids(
    tx: &mut Transaction,
    messages: &Records,
    domain: &Domain,
    order: &str,
    limit: usize,
) -> Result<Vec<i64>> {
    let options = SearchOptions::new()
        .order(order)
        .limit(limit as u64)
        .fetch_fields(PAGE_COLUMNS);
    Ok(messages.search_with(tx, domain, &options).await?.ids())
}

/// Load one page of the messages of `(model, id)` as the caller may read them: one search for a
/// `Latest`, `Before` or `After` page and two for an `Around` page, each asking one message more
/// than the page holds to learn whether the thread goes on.
///
/// `has_more_after` of a `Before` page and `has_more_before` of an `After` page are true: the
/// cursor message, which the caller holds, lies on that side (a message deleted since shows as an
/// empty page on the next call in that direction).
async fn load_page(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    id: i64,
    cursor: Cursor,
    limit: usize,
) -> Result<Page> {
    let messages = env.records(MESSAGE)?;
    let mut domain = Domain::condition("model", Operator::Eq, model)?
        & Domain::condition("res_id", Operator::Eq, id)?
        & Domain::condition("message_type", Operator::Ne, "user_notification")?;
    if standing(tx, env).await? != Standing::Internal {
        let thread = env.records(model)?.browse([id])?;
        let types: Vec<String> = thread
            .call(tx, "_get_customer_portal_message_types", ())
            .await?;
        domain =
            domain & Domain::condition("message_type", Operator::In, types)? & !empty_messages()?;
    }
    let before = |bound: Operator, id: i64| -> Result<Domain> {
        Ok(domain.clone() & Domain::condition("id", bound, id)?)
    };
    match cursor {
        Cursor::Latest | Cursor::Before(_) => {
            let scoped = match cursor {
                Cursor::Before(bound) => before(Operator::Lt, bound)?,
                _ => domain.clone(),
            };
            let mut ids = search_ids(tx, &messages, &scoped, "id desc", limit + 1).await?;
            let more = ids.len() > limit;
            ids.truncate(limit);
            Ok(Page {
                ids,
                has_more_before: more,
                has_more_after: matches!(cursor, Cursor::Before(_)),
            })
        }
        Cursor::After(bound) => {
            let scoped = before(Operator::Gt, bound)?;
            let mut ids = search_ids(tx, &messages, &scoped, "id asc", limit + 1).await?;
            let more = ids.len() > limit;
            ids.truncate(limit);
            ids.reverse();
            Ok(Page {
                ids,
                has_more_before: true,
                has_more_after: more,
            })
        }
        Cursor::Around(bound) => {
            let newer_room = limit / 2;
            let older_room = limit - newer_room;
            let older = before(Operator::Le, bound)?;
            let mut older_ids =
                search_ids(tx, &messages, &older, "id desc", older_room + 1).await?;
            let newer = before(Operator::Gt, bound)?;
            let mut newer_ids = search_ids(tx, &messages, &newer, "id asc", newer_room + 1).await?;
            let has_more_before = older_ids.len() > older_room;
            let has_more_after = newer_ids.len() > newer_room;
            older_ids.truncate(older_room);
            newer_ids.truncate(newer_room);
            newer_ids.reverse();
            newer_ids.extend(older_ids);
            Ok(Page {
                ids: newer_ids,
                has_more_before,
                has_more_after,
            })
        }
    }
}

/// Stage the store entries of the page's messages through the dispatched `_to_store`.
async fn stage_messages(
    tx: &mut Transaction,
    env: &Env,
    ids: &[i64],
    out: &mut TopicOutput,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let messages = env.records(MESSAGE)?.browse(ids.iter().copied())?;
    let fields: StoreFields = messages
        .call(tx, "_to_store", (StoreFields::new(),))
        .await?;
    fields.apply(out);
    Ok(())
}

fn limit_of(params: &TopicParams) -> Result<usize> {
    let limit = params.opt_int("limit")?.unwrap_or(DEFAULT_MESSAGE_PAGE);
    usize::try_from(limit).map_err(|_| Error::invariant("a validated page limit is positive"))
}

fn run_messages<'a>(
    tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let env = input.env();
        let (model, id) = thread_ref(input.params())?;
        let cursor = cursor_of(input.params())?;
        let page = load_page(tx, env, model, id, cursor, limit_of(input.params())?).await?;
        stage_messages(tx, env, &page.ids, out).await?;
        let mut document = page.document();
        if let Some(map) = document.as_object_mut() {
            map.insert("model".to_owned(), json!(model));
            map.insert("id".to_owned(), json!(id));
        }
        out.data(document);
        Ok(())
    })
}

// ----- the thread -----------------------------------------------------------------------------

/// The follower panel read in one statement: the first page (oldest first), the caller's own
/// follower row wherever it lies, and the follower count. The thread's read access and the
/// caller's internal standing were checked first, as for `thread_followers`.
const FOLLOWER_PANEL: &str = "\
WITH me AS (SELECT u.partner_id FROM res_users u WHERE u.id = $3::int8), \
page AS (SELECT f.id FROM mail_followers f WHERE f.res_model = $1 AND f.res_id = $2::int8 \
         ORDER BY f.id LIMIT $4::int8), \
own AS (SELECT f.id FROM mail_followers f JOIN me ON f.partner_id = me.partner_id \
        WHERE f.res_model = $1 AND f.res_id = $2::int8), \
want AS (SELECT id FROM page UNION SELECT id FROM own) \
SELECT f.id::int8, f.partner_id::int8, \
       ARRAY(SELECT r.mail_message_subtype_id::int8 FROM mail_followers_mail_message_subtype_rel r \
             WHERE r.mail_followers_id = f.id ORDER BY 1), \
       (SELECT count(*) FROM mail_followers c WHERE c.res_model = $1 AND c.res_id = $2::int8)::int8, \
       f.id IN (SELECT id FROM own), \
       f.id IN (SELECT id FROM page) \
FROM mail_followers f WHERE f.id IN (SELECT id FROM want) ORDER BY f.id";

/// The follower panel; `limit` is the page size (the caller's `followers_limit`).
async fn followers_panel(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    id: i64,
    limit: i64,
    out: &mut TopicOutput,
) -> Result<Json> {
    if standing(tx, env).await? != Standing::Internal {
        return Err(Error::access("Only internal users can list followers."));
    }
    let uid = env.uid();
    let rows = tx
        .query(FOLLOWER_PANEL, &[&model, &id, &uid, &limit])
        .await?;
    fn bad(what: &str, error: impl std::error::Error + Send + Sync + 'static) -> Error {
        Error::internal(format!("the follower panel row has no readable {what}")).with_source(error)
    }
    let mut count = 0_i64;
    let mut page_ids: Vec<i64> = Vec::new();
    let mut own: Option<i64> = None;
    let mut fields = StoreFields::new();
    let mut partners = BTreeSet::new();
    for row in &rows {
        let fid: i64 = row.try_get(0).map_err(|e| bad("id", e))?;
        let partner: Option<i64> = row.try_get(1).map_err(|e| bad("partner", e))?;
        let subtypes: Vec<i64> = row.try_get(2).map_err(|e| bad("subtypes", e))?;
        count = row.try_get(3).map_err(|e| bad("count", e))?;
        let is_own: bool = row.try_get(4).map_err(|e| bad("own flag", e))?;
        let in_page: bool = row.try_get(5).map_err(|e| bad("page flag", e))?;
        let record = u64::try_from(fid)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::invariant(format!("follower id {fid} is not a record id")))?;
        partners.extend(partner);
        let partner = partner
            .map(|p| {
                u64::try_from(p)
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| Error::invariant(format!("follower partner {p} is not an id")))
            })
            .transpose()?;
        let subtype_ids = subtypes
            .iter()
            .map(|s| {
                u64::try_from(*s)
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| Error::invariant(format!("subtype id {s} is not a record id")))
            })
            .collect::<Result<Vec<u64>>>()?;
        fields.set(
            FOLLOWERS,
            record,
            "res_model",
            Wire::String(model.to_owned()),
        );
        fields.set(FOLLOWERS, record, "res_id", Wire::Int(id));
        fields.set(FOLLOWERS, record, "partner_id", Wire::Many2One(partner));
        fields.set(
            FOLLOWERS,
            record,
            "subtype_ids",
            Wire::X2ManyReplace(subtype_ids),
        );
        if is_own {
            own = Some(fid);
        }
        if in_page {
            page_ids.push(fid);
        }
    }
    store::stage_partners(tx, env, partners, &mut fields).await?;
    fields.apply(out);
    // more followers exist than the page holds
    let more = usize::try_from(count).is_ok_and(|all| all > page_ids.len());
    let next_after = match page_ids.last() {
        Some(last) if more => json!(last),
        _ => Json::Null,
    };
    Ok(json!({
        "count": count,
        "follower_ids": page_ids,
        "self_follower_id": own,
        "next_after": next_after,
    }))
}

fn flag(params: &TopicParams, name: &str, default: bool) -> Result<bool> {
    Ok(params.opt_boolean(name)?.unwrap_or(default))
}

fn run_thread<'a>(
    tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let env = input.env();
        let params = input.params();
        let (model, id) = thread_ref(params)?;
        let mut requested = Vec::new();
        for panel in ["messages", "followers", "default_subject", "attachments"] {
            if flag(params, panel, panel == "messages")? {
                requested.push(panel);
            }
        }
        let record = env.records(model)?.browse([id])?;
        let mut facts: ThreadFacts = record
            .call(
                tx,
                "_store_thread_fields",
                (ThreadFacts::new(requested.iter().copied()),),
            )
            .await?;
        facts.set("model", json!(model))?;
        facts.set("id", json!(id))?;
        let mut document = json!({});
        if flag(params, "attachments", false)? {
            // the thread read was checked by the access check: its files follow
            let limit = params
                .opt_int("attachments_limit")?
                .unwrap_or(MAX_PANEL_ATTACHMENTS);
            let mut fields = StoreFields::new();
            let (panel, count) =
                message_attachments::stage_of_record(tx, model, id, limit, &mut fields).await?;
            fields.apply(out);
            facts.set("attachment_count", json!(count))?;
            document["attachments"] = panel;
        }
        if flag(params, "messages", true)? {
            let page = load_page(tx, env, model, id, Cursor::Latest, limit_of(params)?).await?;
            stage_messages(tx, env, &page.ids, out).await?;
            document["messages"] = page.document();
        }
        if flag(params, "followers", false)? {
            let limit = params
                .opt_int("followers_limit")?
                .unwrap_or(MAX_FOLLOWER_LIMIT);
            document["followers"] = followers_panel(tx, env, model, id, limit, out).await?;
        }
        document["thread"] = facts.into_json();
        out.data(document);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python's `textwrap.shorten`, the cases of its documentation and the edges.
    #[test]
    fn shorten_cuts_at_a_word_and_collapses_spaces() {
        assert_eq!(shorten("Hello  world!", 12), "Hello world!");
        assert_eq!(shorten("Hello  world!", 11), "Hello...");
        assert_eq!(shorten("  a \n b  ", 100), "a b");
        assert_eq!(shorten("", 100), "");
        assert_eq!(shorten("abcdefghij", 5), "...");
        let long = "word ".repeat(40);
        let cut = shorten(&long, 100);
        assert!(cut.ends_with("..."));
        assert!(cut.chars().count() <= 100, "{cut}");
    }

    #[test]
    fn a_thread_fact_is_set_once() {
        let mut facts = ThreadFacts::new(["messages"]);
        facts.set("can_post", json!(true)).unwrap();
        let error = facts.set("can_post", json!(false)).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "the thread fact 'can_post' is set twice; an override adds facts, it does not \
             replace the base ones"
        );
        assert!(facts.requested("messages"));
        assert!(!facts.requested("followers"));
    }
}
