//! `mail.scheduled.message`: a message composed now and posted on a record's thread later.
//!
//! `message_post` keeps its own post check at send time. The delivery
//! fields and creation are refused to every other path (a direct ORM create or write) unless a
//! server-only marker, set by those methods for the one call, is present. Delivery is one job per message on the durable queue
//! , due at the message's date and run as the creator, who is the only user that can
//! arm or re-arm it (the author of the post is the principal that posts).
//!
//! Sending goes through `message_post` of the record's thread, so every override of the post
//! path applies. Attachments (`ir.attachment`) and Cc recipients (`mail.message` has
//! no `partner_cc_ids` yet) are refused with errors that name what they wait for.
//!
//! [`schedule`]: MailScheduledMessage::schedule
//! [`edit`]: MailScheduledMessage::edit
//! [`send_now`]: MailScheduledMessage::send_now
//! [`cancel`]: MailScheduledMessage::cancel

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use erp_sdk::jobs::{
    EnqueueRequest, HandlerRef, JobKey, JobKindDecl, MAX_CANCEL_KEYS, OnDuplicate, StepArgs,
    StepOutcome, When,
};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::store_wire::{FieldKind, FieldSpec, FieldValue};
use erp_sdk::{ErrorKind, ModelKind, Operation, TimestampTz};

use crate::mail_thread::MAX_IDS_PER_CALL;
use crate::post::{Body, MAX_BODY_BYTES, PostRequest, PostSubtype};
use crate::store_fetch::{
    Audience, ParamDecl, SchemaFragment, SchemaPart, StoreTopic, TopicFuture, TopicInput,
    TopicOutput,
};

/// The model name.
pub const MODEL: &str = "mail.scheduled.message";
/// The delivery job kind.
pub const SEND_KIND: &str = "mail.scheduled_message_send";
/// The store topic that lists the scheduled messages of one record.
pub const TOPIC: &str = "thread.scheduled";
/// Most scheduled messages one record holds.
pub const MAX_SCHEDULED_PER_THREAD: usize = 100;
/// Most bytes of the stored failure reason.
pub const MAX_FAILURE_REASON_BYTES: usize = 512;
/// First key of the transaction-level advisory lock that serializes the schedulers of one record
/// (`pg_advisory_xact_lock(THREAD_LOCK_CLASS, hashtext('<model>:<res_id>'))`), so the per-record
/// bound holds across the cluster. The class is registered next to the other classes in
/// `erp_sql::locks` (`MAIL_SCHEDULED_THREAD_LOCK_CLASS`, a test pins that the two are equal); the
/// classes in use are 1701 and 1702 (partitions), 1703 (singleton jobs), 1704 (this) and
/// `0x45525046` (`ERPF`, the node and install locks).
pub const THREAD_LOCK_CLASS: i32 = 1704;
/// Longest model name a listing request accepts (bytes).
const MAX_MODEL_NAME_BYTES: usize = 128;
/// [`MAX_SCHEDULED_PER_THREAD`] as the `int4` the slot statement takes.
const MAX_SLOTS: i32 = 100;
const _: () = assert!(MAX_SLOTS as usize == MAX_SCHEDULED_PER_THREAD);

const PAST_DATE: &str = "A Scheduled Message cannot be scheduled in the past";
/// How far ahead a message may be scheduled: 365 days.
pub const MAX_AHEAD_DAYS: i64 = 365;
const MAX_AHEAD_MICROS: i64 = MAX_AHEAD_DAYS * 86_400 * 1_000_000;
/// The refusal of a date beyond [`MAX_AHEAD_DAYS`].
const TOO_FAR: &str = "A Scheduled Message cannot be scheduled more than 365 days ahead";
const INTERNAL_ONLY: &str = "Scheduled messages are for internal users.";
/// The refusal of a direct create (only `schedule` creates a row and arms its delivery).
const CREATE_REFUSED: &str = "A scheduled message is created with schedule(): a direct create \
                              would leave it without a delivery job.";
/// The refusal of a direct write of the delivery fields.
const DELIVERY_FIELDS_REFUSED: &str = "The delivery fields of a scheduled message (scheduled_date, \
                                       arm_seq, state, failure_reason) are set by edit, send now \
                                       and the delivery job only.";
/// The unique key on (model, res_id, slot), matched by SQLSTATE 23505 and this constraint name: a
/// concurrent scheduler took the slot.
const SLOT_KEY: &str = "_mail_scheduled_message_slot_uniq";
/// The refusal of a record the caller may not write; it names nothing of the row.
const RECORD_DENIED: &str =
    "You are not allowed to access the record this message is scheduled on.";

erp_sdk::jobs::register_job_kind!(
    SEND_JOB_KIND,
    JobKindDecl::new(
        "mail",
        SEND_KIND,
        HandlerRef::Step {
            model: MODEL,
            method: "send_due",
        },
    )
    .max_attempts(5)
);

/// The job kinds this addon registers, for `OrmBuilder::require_job_kinds` at the build site of a
/// binary that runs jobs. Calling it also keeps the registration in the binary.
pub fn job_kinds() -> &'static [erp_sdk::jobs::JobKindRequirement] {
    // the reference pulls the registration's object file into the binary
    std::hint::black_box(&SEND_JOB_KIND);
    &REQUIRED_KINDS
}

const REQUIRED_KINDS: [erp_sdk::jobs::JobKindRequirement; 1] =
    [erp_sdk::jobs::JobKindRequirement::new(
        SEND_KIND,
        "erp-addon-mail",
    )];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommentOption {
    /// Reply to everyone of the thread.
    ReplyAll,
    /// Forward.
    Forward,
}

impl CommentOption {
    fn key(self) -> &'static str {
        match self {
            Self::ReplyAll => "reply_all",
            Self::Forward => "forward",
        }
    }
}

/// A message to schedule on one record. Built with [`ScheduleRequest::new`], then fields set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScheduleRequest {
    /// Model of the record (must inherit `mail.thread`).
    pub model: String,
    /// The record.
    pub res_id: i64,
    /// Plain text body, escaped when sent.
    pub body: String,
    /// Subject line.
    pub subject: Option<String>,
    /// When to post (naive UTC); not before the database clock.
    pub scheduled_date: DateTime,
    /// Explicit recipients (To).
    pub partner_ids: Vec<i64>,
    /// Post as a note (`mail.mt_note`) instead of a message (`mail.mt_comment`).
    pub is_note: bool,
    /// The composer's reply-all or forward option.
    pub composition_comment_option: Option<CommentOption>,
    /// Make the recipients follow the record when sent (`PostOptions::autofollow_recipients`).
    pub autofollow_recipients: bool,
    /// Attachments: refused until `ir.attachment` exists.
    pub attachment_ids: Vec<i64>,
    /// Cc recipients: refused until `mail.message` has `partner_cc_ids`.
    pub partner_cc_ids: Vec<i64>,
}

impl ScheduleRequest {
    /// A message of `body` for `model` record `res_id` at `scheduled_date`, posted as a message
    /// to nobody in particular.
    pub fn new(
        model: impl Into<String>,
        res_id: i64,
        body: impl Into<String>,
        scheduled_date: DateTime,
    ) -> Self {
        Self {
            model: model.into(),
            res_id,
            body: body.into(),
            subject: None,
            scheduled_date,
            partner_ids: Vec::new(),
            is_note: false,
            composition_comment_option: None,
            autofollow_recipients: false,
            attachment_ids: Vec::new(),
            partner_cc_ids: Vec::new(),
        }
    }
}

/// The changes of an [`edit`](MailScheduledMessage::edit): only the fields that are `Some`.
/// There is no model or record: the target of a scheduled message never changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EditRequest {
    /// New subject.
    pub subject: Option<String>,
    /// New body.
    pub body: Option<String>,
    /// The new full set of recipients.
    pub partner_ids: Option<Vec<i64>>,
    /// Note or message.
    pub is_note: Option<bool>,
    /// New composer option.
    pub composition_comment_option: Option<CommentOption>,
    /// New follow-recipients flag.
    pub autofollow_recipients: Option<bool>,
    /// A new date: re-arms the delivery (also for a failed message).
    pub scheduled_date: Option<DateTime>,
}

impl EditRequest {
    /// No change.
    pub fn new() -> Self {
        Self::default()
    }

    fn is_empty(&self) -> bool {
        self.subject.is_none()
            && self.body.is_none()
            && self.partner_ids.is_none()
            && self.is_note.is_none()
            && self.composition_comment_option.is_none()
            && self.autofollow_recipients.is_none()
            && self.scheduled_date.is_none()
    }
}

/// A message scheduled for later. `model`/`res_id` point at the thread (no foreign key: deleting
/// the thread deletes its scheduled messages through the `mail.thread` `unlink` override).
#[model(
    name = "mail.scheduled.message",
    description = "Scheduled Message",
    order = "scheduled_date, id",
    rec_name = "subject",
    addon = "mail",
    company_access = unrestricted(reason = "scheduled messages are reached through their record's own access; the reference ERP's mail.scheduled.message has no company rule"),
    constraints = [(
        "_mail_scheduled_message_slot_range",
        "CHECK (slot BETWEEN 1 AND 100)",
        "A scheduled message slot is between 1 and 100."
    )],
    keys = [key(
        name = "_mail_scheduled_message_slot_uniq",
        fields = ["model", "res_id", "slot"],
        message = "A scheduled message slot of this record is taken."
    )]
)]
pub struct MailScheduledMessage {
    /// Subject line.
    #[field(char, string = "Subject")]
    pub subject: Option<Arc<str>>,
    /// Plain text body (HTML bodies wait for).
    #[field(text, string = "Contents")]
    pub body: Option<Arc<str>>,
    /// When the message is posted (naive UTC).
    #[field(datetime, required = true, string = "Scheduled Date")]
    pub scheduled_date: Option<DateTime>,
    /// The composer's reply-all or forward option.
    #[field(
        selection,
        selection = [("reply_all", "Reply-All"), ("forward", "Forward")],
        string = "Comment Options"
    )]
    pub composition_comment_option: Option<Arc<str>>,
    /// Model of the thread.
    #[field(char, required = true, string = "Related Document Model")]
    pub model: Option<Arc<str>>,
    /// Id of the thread record.
    #[field(
        many2one_reference,
        model_field = "model",
        required = true,
        string = "Related Document Id"
    )]
    pub res_id: i64,
    /// The creator's partner.
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        index = true,
        ondelete = "restrict",
        string = "Author"
    )]
    pub author_id: Option<RecId>,
    /// Explicit recipients (To).
    #[field(
        many2many,
        comodel = "res.partner",
        relation = "mail_scheduled_message_res_partner_rel",
        column1 = "mail_scheduled_message_id",
        column2 = "res_partner_id",
        string = "Recipients (To)"
    )]
    pub partner_ids: Vec<RecId>,
    /// Post as a note.
    #[field(boolean, string = "Is a note")]
    pub is_note: bool,
    /// Make the recipients follow the record when sent.
    #[field(boolean, string = "Follow Recipients")]
    pub autofollow_recipients: bool,
    /// `scheduled` until sent; `failed` when the send raised an error the user must see.
    #[field(
        selection,
        selection = [("scheduled", "Scheduled"), ("failed", "Failed")],
        required = true,
        default = "scheduled",
        string = "State"
    )]
    pub state: Option<Arc<str>>,
    /// Why the send failed (at most [`MAX_FAILURE_REASON_BYTES`] bytes).
    #[field(char, string = "Failure Reason")]
    pub failure_reason: Option<Arc<str>>,
    /// The delivery generation: bumped by every re-arm; the job of an older generation does
    /// nothing.
    #[field(integer, default = 1, required = true, string = "Delivery Generation")]
    pub arm_seq: i32,
    /// One of `1..=MAX_SCHEDULED_PER_THREAD`, unique per record: the bound of the record lives in
    /// this unique key, so two concurrent schedulers that read the same count cannot both take
    /// the last place (the database refuses the second, and the call is retried).
    #[field(integer, required = true, string = "Slot")]
    pub slot: i32,
}

/// The transactions inside a call of `schedule`, `edit` or the delivery job that writes the
/// protected fields of this model. A direct ORM create or write has none of them and is refused.
///
/// The marker is the address of the live `Transaction`: not a field, a context key or anything an
/// RPC, a client or an addon call can set; only this module enters it, around exactly the one
/// create or write it performs, never around a hook, a post or an enqueue. **Overrides of
/// `create` and `write` that run inside the window (an addon extending this model) see it set**,
/// as does anything such an override calls on the same transaction: the window is one ORM call
/// wide, so an override must not forward the transaction to code that creates other scheduled
/// messages. Each key has a counter, so a nested entry on the same transaction does not clear the
/// outer one when it ends.
static SERVER_CALLS: Mutex<BTreeMap<usize, u32>> = Mutex::new(BTreeMap::new());

struct ServerCall(usize);

impl ServerCall {
    fn enter(tx: &Transaction) -> Self {
        let key = std::ptr::from_ref(tx) as usize;
        *SERVER_CALLS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key)
            .or_insert(0) += 1;
        Self(key)
    }
}

impl Drop for ServerCall {
    fn drop(&mut self) {
        let mut calls = SERVER_CALLS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = calls.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                calls.remove(&self.0);
            }
        }
    }
}

fn server_call_active(tx: &Transaction) -> bool {
    SERVER_CALLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains_key(&(std::ptr::from_ref(tx) as usize))
}

/// What the row-loading statement found.
struct Draft {
    id: i64,
    model: String,
    res_id: i64,
    create_uid: Option<i64>,
    subject: Option<String>,
    body: String,
    is_note: bool,
    autofollow: bool,
    failed: bool,
    arm_seq: i32,
    partners: Vec<i64>,
}

const DRAFT_COLUMNS: &str = "m.model::text, m.res_id::int8, m.create_uid::int8, \
     m.subject::text, COALESCE(m.body, '')::text, m.is_note, m.autofollow_recipients, \
     (m.state = 'failed'), m.arm_seq::int4, \
     ARRAY(SELECT r.res_partner_id::int8 FROM mail_scheduled_message_res_partner_rel r \
     WHERE r.mail_scheduled_message_id = m.id ORDER BY 1)";

macro_rules! col {
    ($row:expr, $i:expr) => {
        $row.try_get($i).map_err(|error| {
            Error::db(format!(
                "unexpected scheduled message column {}: {error}",
                $i
            ))
        })?
    };
}

/// The row `id` (locked when `lock`), or `None` when it does not exist (any more).
async fn load_draft(tx: &mut Transaction, id: i64, lock: bool) -> Result<Option<Draft>> {
    tx.flush().await?;
    let sql = format!(
        "SELECT {DRAFT_COLUMNS} FROM mail_scheduled_message m WHERE m.id = $1::int8{}",
        if lock { " FOR UPDATE OF m" } else { "" }
    );
    let rows = tx.query(&sql, &[&id]).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(Draft {
        id,
        model: col!(row, 0),
        res_id: col!(row, 1),
        create_uid: col!(row, 2),
        subject: col!(row, 3),
        body: col!(row, 4),
        is_note: col!(row, 5),
        autofollow: col!(row, 6),
        failed: col!(row, 7),
        arm_seq: col!(row, 8),
        partners: col!(row, 9),
    }))
}

async fn require_draft(tx: &mut Transaction, id: i64, lock: bool) -> Result<Draft> {
    load_draft(tx, id, lock)
        .await?
        .ok_or_else(|| Error::missing(format!("The scheduled message {id} does not exist.")))
}

fn single(records: &RecordSet<MailScheduledMessage>) -> Result<i64> {
    match records.as_records().ids().as_slice() {
        [id] if *id > 0 => Ok(*id),
        ids => Err(Error::validation(format!(
            "This method works on exactly one saved scheduled message, not {}.",
            ids.len()
        ))),
    }
}

fn rec_ids(ids: &[i64]) -> Result<Vec<RecId>> {
    ids.iter()
        .map(|id| {
            RecId::from_real(*id)
                .ok_or_else(|| Error::invariant(format!("partner id {id} is not a real id")))
        })
        .collect()
}

/// The checks of a request that need no statement. Returns the deduplicated partner ids.
fn check_text(body: &str, subject: Option<&str>) -> Result<()> {
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::validation(format!(
            "A scheduled message body holds at most {MAX_BODY_BYTES} bytes, not {}.",
            body.len()
        )));
    }
    if let Some(subject) = subject
        && subject.len() > MAX_BODY_BYTES
    {
        return Err(Error::validation(format!(
            "A scheduled message subject holds at most {MAX_BODY_BYTES} bytes, not {}.",
            subject.len()
        )));
    }
    Ok(())
}

fn check_partners(ids: &[i64]) -> Result<Vec<i64>> {
    if ids.len() > MAX_IDS_PER_CALL {
        return Err(Error::validation(format!(
            "At most {MAX_IDS_PER_CALL} partner ids are accepted per scheduled message, not {}.",
            ids.len()
        )));
    }
    let mut set = BTreeSet::new();
    for id in ids {
        if *id <= 0 {
            return Err(Error::validation(format!(
                "A partner id must be a positive integer, not {id}."
            )));
        }
        set.insert(*id);
    }
    Ok(set.into_iter().collect())
}

fn check_schedule(request: &ScheduleRequest) -> Result<Vec<i64>> {
    if !request.attachment_ids.is_empty() {
        return Err(Error::validation(
            "Attachments on a scheduled message need ir.attachment (a later release), which is not \
             available yet.",
        ));
    }
    if !request.partner_cc_ids.is_empty() {
        return Err(Error::validation(
            "Cc recipients on a scheduled message need partner_cc_ids on mail.message (the \
             outgoing mail slice), which is not available yet.",
        ));
    }
    if request.res_id <= 0 {
        return Err(Error::validation(format!(
            "A message is scheduled on a saved record, not on {} {}.",
            request.model, request.res_id
        )));
    }
    check_text(&request.body, request.subject.as_deref())?;
    check_partners(&request.partner_ids)
}

/// `message_post` checks the post access of the record again when the message is sent.
async fn check_write_access(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    res_id: i64,
) -> Result<()> {
    let record = env.records(model)?.browse([res_id])?;
    record.check_access(tx, Operation::Write).await?;
    // a rule without a domain allows an id nobody has: a missing record is `Missing`
    if record.exists(tx).await?.is_empty() {
        return Err(Error::missing(format!(
            "The record {model}({res_id}) does not exist."
        )));
    }
    Ok(())
}

/// The four methods and the listing are for internal users (`base.group_user`).
async fn require_internal(tx: &mut Transaction, env: &Env) -> Result<()> {
    if tx.has_group(env, "base.group_user").await? {
        Ok(())
    } else {
        Err(Error::access(INTERNAL_ONLY))
    }
}

/// The same check for a stored row: a refusal says nothing of the row (fixed text).
async fn check_row_access(tx: &mut Transaction, env: &Env, draft: &Draft) -> Result<()> {
    match check_write_access(tx, env, &draft.model, draft.res_id).await {
        Err(error) if error.kind() == ErrorKind::Access => Err(Error::access(RECORD_DENIED)),
        other => other,
    }
}

/// Serialize the schedulers of one record for the rest of the transaction (the per-record bound,
/// cluster-wide, in PostgreSQL). The lock is released at commit or rollback.
async fn lock_thread(tx: &mut Transaction, model: &str, res_id: i64) -> Result<()> {
    tx.query(
        "SELECT pg_advisory_xact_lock($1::int4, hashtext($2::text))",
        &[&THREAD_LOCK_CLASS, &format!("{model}:{res_id}")],
    )
    .await?;
    Ok(())
}

/// The database clock in microseconds since the epoch.
async fn clock_micros(tx: &mut Transaction) -> Result<i64> {
    let rows = tx
        .query(
            "SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::int8",
            &[],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::invariant("the clock statement returned no row"))?;
    Ok(col!(row, 0))
}

/// What one statement says about the caller and the database clock.
struct Context {
    now_micros: i64,
    partner: Option<i64>,
    existing_on_record: i64,
    free_slot: Option<i32>,
    known_partners: BTreeSet<i64>,
}

async fn context(
    tx: &mut Transaction,
    uid: i64,
    model: &str,
    res_id: i64,
    partners: &[i64],
) -> Result<Context> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::int8, \
             (SELECT u.partner_id::int8 FROM res_users u WHERE u.id = $1::int8), \
             (SELECT count(*)::int8 FROM mail_scheduled_message m \
              WHERE m.model = $2::text AND m.res_id = $3::int8), \
             ARRAY(SELECT p.id::int8 FROM res_partner p WHERE p.id = ANY($4::int8[]) ORDER BY p.id), \
             (SELECT min(g)::int4 FROM generate_series(1, $5::int4) g WHERE NOT EXISTS \
              (SELECT 1 FROM mail_scheduled_message m WHERE m.model = $2::text \
               AND m.res_id = $3::int8 AND m.slot = g))",
            &[&uid, &model, &res_id, &partners, &MAX_SLOTS],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::invariant("the scheduling context statement returned no row"))?;
    let known: Vec<i64> = col!(row, 3);
    Ok(Context {
        now_micros: col!(row, 0),
        partner: col!(row, 1),
        existing_on_record: col!(row, 2),
        free_slot: col!(row, 4),
        known_partners: known.into_iter().collect(),
    })
}

fn check_partners_exist(wanted: &[i64], known: &BTreeSet<i64>) -> Result<()> {
    let unknown: Vec<String> = wanted
        .iter()
        .filter(|id| !known.contains(id))
        .map(ToString::to_string)
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(Error::validation(format!(
            "Unknown partner ids: {}.",
            unknown.join(", ")
        )))
    }
}

fn check_not_past(date: DateTime, now_micros: i64) -> Result<()> {
    if date.micros() < now_micros {
        return Err(Error::validation(PAST_DATE));
    }
    if date.micros() - now_micros > MAX_AHEAD_MICROS {
        return Err(Error::validation(TOO_FAR));
    }
    Ok(())
}

/// Enqueue the delivery job of generation `seq` at `date`, as the caller. The dedup key makes
/// enqueueing one generation twice a single job.
async fn arm(tx: &mut Transaction, env: &Env, id: i64, seq: i32, date: DateTime) -> Result<()> {
    let at = TimestampTz::from_unix_micros(date.micros())?;
    let request = EnqueueRequest::new(SEND_KIND)
        .payload(&json!({ "id": id, "seq": seq }))?
        .at(When::At(at))
        .dedup(delivery_key(id, seq)?, OnDuplicate::Keep);
    tx.enqueue(env, request).await?;
    Ok(())
}

/// Post the draft on its record through `message_post`. The post error is the caller's.
async fn post_draft(tx: &mut Transaction, env: &Env, draft: &Draft) -> Result<i64> {
    let record = env.records(&draft.model)?.browse([draft.res_id])?;
    let mut request = PostRequest::new(Body::Text(draft.body.clone()));
    request.subject.clone_from(&draft.subject);
    request.subtype = if draft.is_note {
        PostSubtype::Note
    } else {
        PostSubtype::Comment
    };
    request.partner_ids.clone_from(&draft.partners);
    request.options.autofollow_recipients = draft.autofollow;
    let posted: crate::post::Posted = record.call(tx, "message_post", (request,)).await?;
    // the hook sees the row through a privileged recordset: a user has no table access
    env.records(MODEL)?
        .sudo(true)
        .browse([draft.id])?
        .call::<_, ()>(tx, "_message_created_hook", (posted.message_id,))
        .await?;
    Ok(posted.message_id)
}

/// Delete the row (and its recipient rows). Privileged: the callers have checked the record
/// access, or are the creator's own job.
async fn delete_draft(tx: &mut Transaction, env: &Env, id: i64) -> Result<()> {
    env.records(MODEL)?
        .sudo(true)
        .browse([id])?
        .unlink(tx)
        .await
}

fn thread_model_error() -> Error {
    Error::validation("A message cannot be scheduled on a model that does not have a mail thread.")
}

/// Whether `model` is a regular model that inherits `mail.thread`.
fn require_thread_model(env: &Env, model: &str) -> Result<Records> {
    let records = env.records(model).map_err(|_| thread_model_error())?;
    let spec = records.spec();
    if spec.kind != ModelKind::Regular || !spec.inherits("mail.thread") {
        return Err(thread_model_error());
    }
    Ok(records)
}

/// The reason shown on a failed row, by the kind of error that stopped the send. Fixed texts: the
/// row is read by everyone who may write the record, so the raw error (which can name partners or
/// access rules of the creator) goes to the server log only.
fn failure_text(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Access => {
            "The sender no longer has the right to post on the record, or is no longer an internal user."
        }
        ErrorKind::Validation => {
            "The message no longer passes its checks (for example a recipient was removed)."
        }
        ErrorKind::Missing => "The record or a recipient of the message no longer exists.",
        _ => "The message was refused when it was sent.",
    }
}

/// The key of the delivery job of generation `seq` of row `id`.
fn delivery_key(id: i64, seq: i32) -> Result<JobKey> {
    JobKey::new(&format!("{id}:{seq}"))
}

/// Cancel the live delivery job of `draft` and check what that found. `env` is the creator's
/// (their own job), or a superuser env for a cancel by another user of the record.
///
/// - a `scheduled` row with one live job: cancelled, as expected; a `failed` row has none (its job
///   finished);
/// - a `scheduled` row whose job is a dead letter: nothing live to cancel, fine;
/// - a `scheduled` row where nothing was cancelled but a non-dead job with the key exists in this
///   snapshot: the cancel skipped a job another transaction holds (a claim in flight,
///   D17.2). That is a race, not a defect: `Concurrency` (40001), so the unit of work repeats the
///   whole call and finds the job claimed (then flagged) or finished;
/// - a `scheduled` row with no job at all (the dead letter expired, an administrator discarded
///   or cancelled the job): there is nothing to cancel, the call goes on (a `warn`), so the row
///   never gets stuck; `edit` re-arms it, `cancel` and `send_now` finish their work, and the
///   listing shows such a row as `failed` (3).
///
/// More than one cancelled is an `Invariant` error: a key has at most one live job.
async fn cancel_delivery(tx: &mut Transaction, env: &Env, draft: &Draft) -> Result<()> {
    let key = delivery_key(draft.id, draft.arm_seq)?;
    let count = tx.job_cancel_by_key(env, SEND_KIND, &key).await?;
    match (draft.failed, count) {
        (true, 0) | (false, 1) => Ok(()),
        (false, 0) => {
            let jobs = tx
                .query(
                    "SELECT (state = 'dead') FROM erp_job WHERE kind = $1 AND dedup_key = $2",
                    &[&SEND_KIND, &key.as_str()],
                )
                .await?;
            let mut live = false;
            for row in &jobs {
                let dead: bool = col!(row, 0);
                live |= !dead;
            }
            if live {
                return Err(Error::new(
                    ErrorKind::Concurrency,
                    "could not serialize access: the delivery job is held by a claim in flight",
                )
                .with_sqlstate(Some("40001".to_owned())));
            }
            if jobs.is_empty() {
                tracing::warn!(
                    target: "erp::mail::scheduled",
                    id = draft.id,
                    key = key.as_str(),
                    "a scheduled message has no delivery job (expired or discarded in the queue)"
                );
            }
            Ok(())
        }
        (_, many) => Err(Error::invariant(format!(
            "cancelling the delivery job {} of scheduled message {} cancelled {many}, not {}",
            key.as_str(),
            draft.id,
            u8::from(!draft.failed)
        ))),
    }
}

/// Cancel the live delivery jobs of every scheduled message of the records `ids` of `model`, in
/// the statements of one set-based cancel (the cascade of a deleted thread,.5). Runs
/// in superuser mode: the jobs are the creators', and the caller has already deleted, or is
/// deleting, the records under their own access. At most one live job per row is expected, so the
/// count may not exceed the number of rows; more is an `Invariant` error.
pub(crate) async fn cancel_for_records(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
) -> Result<()> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT m.id::int8, m.arm_seq::int4 FROM mail_scheduled_message m \
             WHERE m.model = $1::text AND m.res_id = ANY($2::int8[])",
            &[&model, &ids],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut keys = Vec::with_capacity(rows.len());
    for row in &rows {
        keys.push(delivery_key(col!(row, 0), col!(row, 1))?);
    }
    let env = env.sudo(true);
    // the jobs API takes at most MAX_CANCEL_KEYS keys per call: a thread holds at most 100 rows,
    // but one unlink can delete many threads, so the keys go in batches, each checked
    for batch in key_batches(&keys, MAX_CANCEL_KEYS) {
        let count = tx.job_cancel_by_keys(&env, SEND_KIND, batch).await?;
        if usize::try_from(count).unwrap_or(usize::MAX) > batch.len() {
            return Err(Error::invariant(format!(
                "cancelling the delivery jobs of {} scheduled messages cancelled {count}",
                batch.len()
            )));
        }
    }
    Ok(())
}

/// `keys` in batches of at most `max` (never an empty batch).
fn key_batches<T>(keys: &[T], max: usize) -> std::slice::Chunks<'_, T> {
    keys.chunks(max.max(1))
}

async fn validate_vals(tx: &mut Transaction, env: &Env, vals: &RawVals) -> Result<()> {
    if let Some(Cell::Str(model)) = vals.get("model") {
        require_thread_model(env, model)?;
    }
    if let Some(Cell::DateTime(date)) = vals.get("scheduled_date") {
        let now = clock_micros(tx).await?;
        check_not_past(*date, now)?;
    }
    Ok(())
}

#[methods(MailScheduledMessage, addon = "mail")]
impl MailScheduledMessage {
    /// The caller must be an internal user with write access to the record; the
    /// row is created for the caller (author, creator) and its delivery job is enqueued in the
    /// same transaction. Returns the new row's id.
    ///
    /// # Errors
    /// `Validation` for attachments or Cc recipients (they name what they wait for), a model that
    /// is not a mail thread, a past date, an unknown partner, over-long text, bad ids, and a record
    /// that already holds [`MAX_SCHEDULED_PER_THREAD`] scheduled messages; `Access` for a caller
    /// who is not an internal user or may not write the record; `Invariant` when the caller has no
    /// partner; `Concurrency` (40001, retried by the unit of work) when another transaction took
    /// the same place on the record.
    #[method]
    async fn schedule(
        this: &RecordSet<MailScheduledMessage>,
        tx: &mut Transaction,
        request: ScheduleRequest,
    ) -> Result<i64> {
        let env = this.env().clone();
        require_internal(tx, &env).await?;
        let partners = check_schedule(&request)?;
        require_thread_model(&env, &request.model)?;
        check_write_access(tx, &env, &request.model, request.res_id).await?;
        lock_thread(tx, &request.model, request.res_id).await?;
        let seen = context(tx, env.uid(), &request.model, request.res_id, &partners).await?;
        check_not_past(request.scheduled_date, seen.now_micros)?;
        check_partners_exist(&partners, &seen.known_partners)?;
        let author = seen.partner.ok_or_else(|| {
            Error::invariant(format!(
                "User {} has no partner (no res.users row, or its partner_id is empty): a \
                 scheduled message needs an author partner.",
                env.uid()
            ))
        })?;
        let Some(slot) = seen.free_slot else {
            return Err(Error::validation(format!(
                "At most {MAX_SCHEDULED_PER_THREAD} messages can be scheduled on one record, and \
                 this one has {}.",
                seen.existing_on_record
            )));
        };
        let mut vals = RawVals::new()
            .set_cell("model", Cell::Str(request.model.as_str().into()))
            .set_cell("res_id", Cell::Id(request.res_id))
            .set_cell("body", Cell::Str(request.body.as_str().into()))
            .set_cell("scheduled_date", Cell::DateTime(request.scheduled_date))
            .set_cell("author_id", Cell::Id(author))
            .set_cell("is_note", Cell::Bool(request.is_note))
            .set_cell("slot", Cell::Int(slot))
            .set_cell(
                "autofollow_recipients",
                Cell::Bool(request.autofollow_recipients),
            );
        if let Some(subject) = &request.subject {
            vals = vals.set_cell("subject", Cell::Str(subject.as_str().into()));
        }
        if let Some(option) = request.composition_comment_option {
            vals = vals.set_cell("composition_comment_option", Cell::Str(option.key().into()));
        }
        if !partners.is_empty() {
            vals = vals.set_commands("partner_ids", [Command::set(rec_ids(&partners)?)]);
        }
        let created = {
            let _server = ServerCall::enter(tx);
            env.records(MODEL)?.sudo(true).create(tx, vec![vals]).await
        };
        let created = created.map_err(|error| {
            // the unique key on (record, slot) is the bound's backstop under REPEATABLE READ: the
            // loser read a count that predates the winner's commit, so it repeats with a new one
            if error.sqlstate() == Some("23505")
                && error
                    .constraint()
                    .is_some_and(|name| name.contains(SLOT_KEY))
            {
                Error::new(
                    ErrorKind::Concurrency,
                    "could not serialize access: a concurrent scheduled message took the place",
                )
                .with_sqlstate(Some("40001".to_owned()))
            } else {
                error
            }
        })?;
        let id = created
            .ids()
            .first()
            .copied()
            .ok_or_else(|| Error::invariant("creating a scheduled message returned no id"))?;
        arm(tx, &env, id, 1, request.scheduled_date).await?;
        Ok(id)
    }

    /// Only the creator may, as an
    /// internal user who may still write the record. A new date re-arms the delivery (also for a
    /// failed message): the live job of the old generation is cancelled (exactly one is expected),
    /// the generation advances, the state returns to `scheduled` and a new job is enqueued. Text
    /// changes alone are read by the pending job when it runs.
    ///
    /// # Errors
    /// `Missing` for an unknown id; `Access` for a caller who is not an internal user, may not
    /// write the record or is not the creator; `Validation` for an empty change, a past date,
    /// unknown partners, over-long text and bad ids; `Invariant` when the job the row should have
    /// is missing.
    #[method]
    async fn edit(
        this: &RecordSet<MailScheduledMessage>,
        tx: &mut Transaction,
        patch: EditRequest,
    ) -> Result<()> {
        let id = single(this)?;
        let env = this.env().clone();
        require_internal(tx, &env).await?;
        let draft = require_draft(tx, id, true).await?;
        check_row_access(tx, &env, &draft).await?;
        if draft.create_uid != Some(env.uid()) {
            return Err(Error::access(
                "Only the author of a scheduled message can edit it.",
            ));
        }
        if patch.is_empty() {
            return Err(Error::validation(
                "An edit changes at least one field of the scheduled message.",
            ));
        }
        check_text(
            patch.body.as_deref().unwrap_or(""),
            patch.subject.as_deref(),
        )?;
        let partners = match &patch.partner_ids {
            Some(ids) => Some(check_partners(ids)?),
            None => None,
        };
        let wanted: &[i64] = partners.as_deref().unwrap_or(&[]);
        let seen = context(tx, env.uid(), &draft.model, draft.res_id, wanted).await?;
        check_partners_exist(wanted, &seen.known_partners)?;
        if let Some(date) = patch.scheduled_date {
            check_not_past(date, seen.now_micros)?;
        }
        let mut vals = RawVals::new();
        if let Some(subject) = &patch.subject {
            vals = vals.set_cell("subject", Cell::Str(subject.as_str().into()));
        }
        if let Some(body) = &patch.body {
            vals = vals.set_cell("body", Cell::Str(body.as_str().into()));
        }
        if let Some(ids) = &partners {
            vals = vals.set_commands("partner_ids", [Command::set(rec_ids(ids)?)]);
        }
        if let Some(is_note) = patch.is_note {
            vals = vals.set_cell("is_note", Cell::Bool(is_note));
        }
        if let Some(option) = patch.composition_comment_option {
            vals = vals.set_cell("composition_comment_option", Cell::Str(option.key().into()));
        }
        if let Some(flag) = patch.autofollow_recipients {
            vals = vals.set_cell("autofollow_recipients", Cell::Bool(flag));
        }
        let rearm = patch.scheduled_date.map(|date| (date, draft.arm_seq + 1));
        if let Some((date, seq)) = rearm {
            vals = vals
                .set_cell("scheduled_date", Cell::DateTime(date))
                .set_cell("arm_seq", Cell::Int(seq))
                .set_cell("state", Cell::Str("scheduled".into()))
                .set_cell("failure_reason", Cell::Null);
        }
        {
            let _server = ServerCall::enter(tx);
            env.records(MODEL)?
                .sudo(true)
                .browse([id])?
                .write(tx, vals)
                .await?;
        }
        if let Some((date, seq)) = rearm {
            cancel_delivery(tx, &env, &draft).await?;
            arm(tx, &env, id, seq, date).await?;
        }
        Ok(())
    }

    /// Returns the id of the posted message.
    #[method]
    async fn send_now(this: &RecordSet<MailScheduledMessage>, tx: &mut Transaction) -> Result<i64> {
        let id = single(this)?;
        let env = this.env().clone();
        require_internal(tx, &env).await?;
        let draft = require_draft(tx, id, true).await?;
        check_row_access(tx, &env, &draft).await?;
        if draft.create_uid != Some(env.uid()) {
            if env.su() || tx.has_group(&env, "base.group_erp_manager").await? {
                return Err(Error::access(
                    "Sending a message scheduled by another user needs the audited elevation of \
                     audit: posting as that user is not available yet.",
                ));
            }
            return Err(Error::user(
                "You are not allowed to send this scheduled message",
            ));
        }
        let message = post_draft(tx, &env, &draft).await?;
        delete_draft(tx, &env, id).await?;
        cancel_delivery(tx, &env, &draft).await?;
        Ok(message)
    }

    /// The row is deleted and its delivery job cancelled, whoever enqueued it
    /// (the cancel is server code here, a superuser-mode cancel by key).
    ///
    /// # Errors
    /// `Missing` for an unknown id; `Access` for a caller who is not an internal user or may not
    /// write the record; `Invariant` when the job the row should have is missing.
    #[method]
    async fn cancel(this: &RecordSet<MailScheduledMessage>, tx: &mut Transaction) -> Result<()> {
        let id = single(this)?;
        let env = this.env().clone();
        require_internal(tx, &env).await?;
        let draft = require_draft(tx, id, true).await?;
        check_row_access(tx, &env, &draft).await?;
        delete_draft(tx, &env, id).await?;
        cancel_delivery(tx, &env.sudo(true), &draft).await
    }

    /// One chunk of the delivery job (
    /// ): runs as the creator. A row that is gone, of another generation or
    /// already failed is left alone (outcomes `gone`, `superseded`, `already_failed`); otherwise
    /// the creator must still be an internal user with write access to the record, and the message
    /// is posted and the row deleted in this transaction. A send that fails with an `Access`,
    /// `Validation`, `UserError` or `Missing` error is rolled back and the row becomes `failed`
    /// with a fixed reason for that kind (the raw error goes to the server log); any other error
    /// is returned, so the queue retries and finally dead-letters the job.
    ///
    /// # Errors
    /// `Invariant` for a payload that is not `{id, seq}` or a job that runs as another user than
    /// the creator; database and concurrency errors of the post.
    #[method]
    async fn send_due(
        this: &RecordSet<MailScheduledMessage>,
        tx: &mut Transaction,
        args: StepArgs,
    ) -> Result<StepOutcome> {
        let (Some(id), Some(seq)) = (args.payload["id"].as_i64(), args.payload["seq"].as_i64())
        else {
            return Err(Error::invariant(
                "the scheduled message job payload is not {id, seq}",
            ));
        };
        let env = this.env().clone();
        let Some(draft) = load_draft(tx, id, true).await? else {
            return Ok(StepOutcome::done(json!({"outcome": "gone"}), 0));
        };
        if i64::from(draft.arm_seq) != seq {
            return Ok(StepOutcome::done(json!({"outcome": "superseded"}), 0));
        }
        if draft.failed {
            return Ok(StepOutcome::done(json!({"outcome": "already_failed"}), 0));
        }
        if draft.create_uid != Some(env.uid()) {
            return Err(Error::invariant(format!(
                "the scheduled message {id} was created by user {:?} but its job runs as user {}: \
                 a message is posted as its creator only",
                draft.create_uid,
                env.uid()
            )));
        }
        let sent = tx
            .savepoint(async |tx| {
                require_internal(tx, &env).await?;
                check_write_access(tx, &env, &draft.model, draft.res_id).await?;
                let message = post_draft(tx, &env, &draft).await?;
                delete_draft(tx, &env, id).await?;
                Ok(message)
            })
            .await;
        match sent {
            Ok(message) => Ok(StepOutcome::done(
                json!({"outcome": "sent", "message_id": message}),
                1,
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Access
                        | ErrorKind::Validation
                        | ErrorKind::UserError
                        | ErrorKind::Missing
                ) =>
            {
                tracing::warn!(
                    target: "erp::mail::scheduled",
                    id,
                    kind = ?error.kind(),
                    error = error.message(),
                    "a scheduled message could not be sent; it is kept as failed"
                );
                let reason = failure_text(error.kind());
                let _server = ServerCall::enter(tx);
                env.records(MODEL)?
                    .sudo(true)
                    .browse([id])?
                    .write(
                        tx,
                        RawVals::new()
                            .set_cell("state", Cell::Str("failed".into()))
                            .set_cell("failure_reason", Cell::Str(reason.into())),
                    )
                    .await?;
                Ok(StepOutcome::done(json!({"outcome": "failed"}), 1))
            }
            Err(error) => Err(error),
        }
    }

    /// **The recordset is the row, privileged (`sudo`)**: an override reads and
    /// writes with superuser rights and must not trust it for access decisions; a failure rolls
    /// the post back.
    ///
    /// # Errors
    /// Whatever an override raises.
    #[method]
    async fn _message_created_hook(
        _this: &RecordSet<MailScheduledMessage>,
        _tx: &mut Transaction,
        _message_id: i64,
    ) -> Result<()> {
        Ok(())
    }

    /// A scheduled message is created by [`schedule`](Self::schedule) only: any other create is
    /// refused (it would leave a row without a delivery job).
    ///
    /// # Errors
    /// `Validation` for a non-thread model or a past date; `UserError` otherwise, unless the call
    /// is the one `schedule` makes.
    #[method(override)]
    async fn create(
        this: &RecordSet<MailScheduledMessage>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if vals_list.is_empty() {
            return sup.call(tx, this.as_records(), (vals_list,)).await;
        }
        if !server_call_active(tx) {
            let env = this.env().clone();
            for vals in &vals_list {
                validate_vals(tx, &env, vals).await?;
            }
            return Err(Error::user(CREATE_REFUSED));
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// A new date is checked like on
    /// create.
    ///
    /// # Errors
    /// `UserError` when `model` or `res_id` is written, or a delivery field is written by any
    /// other path; `Validation` for a past date.
    #[method(override)]
    async fn write(
        this: &RecordSet<MailScheduledMessage>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("model") || vals.contains("res_id") {
            return Err(Error::user(
                "You are not allowed to change the target record of a scheduled message.",
            ));
        }
        let protected = [
            "scheduled_date",
            "arm_seq",
            "state",
            "failure_reason",
            "slot",
        ]
        .iter()
        .any(|field| vals.contains(field));
        if protected && !server_call_active(tx) {
            let env = this.env().clone();
            validate_vals(tx, &env, &vals).await?;
            return Err(Error::user(DELIVERY_FIELDS_REFUSED));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}

/// Parameters of the listing topic.
const PARAMS: &[ParamDecl] = &[
    ParamDecl::text("model", MAX_MODEL_NAME_BYTES),
    ParamDecl::int("res_id", 1, i64::MAX),
];

crate::store_topic!(
    /// The scheduled messages of one record.
    pub SCHEDULED_TOPIC = StoreTopic::new("mail", TOPIC, run_topic)
        .audience(Audience::Internal)
        .params(PARAMS)
        .access(access_topic)
        .statements(4)
);

crate::store_schema_part!(SCHEDULED_SCHEMA = SchemaPart::new("mail", scheduled_schema));

fn scheduled_schema(fragment: &mut SchemaFragment) {
    fragment.model("res.partner");
    for (field, kind) in [
        ("subject", FieldKind::String),
        ("body", FieldKind::String),
        ("scheduled_date", FieldKind::Datetime),
        ("composition_comment_option", FieldKind::Selection),
        ("is_note", FieldKind::Bool),
        ("state", FieldKind::Selection),
        ("failure_reason", FieldKind::String),
    ] {
        fragment.field(MODEL, field, FieldSpec::new(kind));
    }
    fragment.field(
        MODEL,
        "author_id",
        FieldSpec::new(FieldKind::Many2One).target("res.partner"),
    );
    fragment.field(
        MODEL,
        "partner_ids",
        FieldSpec::new(FieldKind::X2Many).target("res.partner"),
    );
}

fn access_topic<'a>(tx: &'a mut Transaction, input: &'a TopicInput<'a>) -> TopicFuture<'a> {
    Box::pin(async move {
        let model = input.params().text("model")?.to_owned();
        let res_id = input.params().int("res_id")?;
        let env = input.env().clone();
        require_thread_model(&env, &model).map_err(|_| Error::access(RECORD_DENIED))?;
        // an unreadable or missing record is the same denial: the slot never says which
        check_write_access(tx, &env, &model, res_id)
            .await
            .map_err(|error| match error.kind() {
                ErrorKind::Access | ErrorKind::Missing => Error::access(RECORD_DENIED),
                _ => error,
            })
    })
}

/// One listed row: the columns the client shows.
struct Listed {
    id: u64,
    subject: Option<String>,
    body: String,
    date: String,
    author: Option<u64>,
    partners: Vec<u64>,
    is_note: bool,
    state: String,
    reason: Option<String>,
    option: Option<String>,
}

/// The scheduled messages of one record, by date then id: at most the bound; more is a
/// `UserError`. A message whose current delivery job is a dead letter shows as failed.
async fn list_thread(tx: &mut Transaction, model: &str, res_id: i64) -> Result<Vec<Listed>> {
    let limit = i64::try_from(MAX_SCHEDULED_PER_THREAD + 1).unwrap_or(i64::MAX);
    let rows = tx
        .query(
            "SELECT m.id::int8, m.subject::text, COALESCE(m.body, '')::text, \
             to_char(m.scheduled_date, 'YYYY-MM-DD HH24:MI:SS'), m.author_id::int8, \
             ARRAY(SELECT r.res_partner_id::int8 FROM mail_scheduled_message_res_partner_rel r \
             WHERE r.mail_scheduled_message_id = m.id ORDER BY 1), m.is_note, \
             CASE WHEN m.state = 'failed' OR d.code IS NOT NULL OR NOT q.any_job THEN 'failed' \
             ELSE 'scheduled' END, \
             COALESCE(m.failure_reason, CASE WHEN NOT q.any_job THEN \
             'The message has no delivery job (it expired or was discarded in the queue). Edit it with a new date, send it now or cancel it.' \
             WHEN d.code IS NOT NULL THEN \
             CASE d.code WHEN 'principal_revoked' THEN \
             'The sender can no longer send: the account was archived or lost its access. Edit the message with a new date, or cancel it.' \
             ELSE 'The delivery job failed (' || d.code || '); an administrator can requeue it.' END END), \
             m.composition_comment_option::text \
             FROM mail_scheduled_message m \
             LEFT JOIN LATERAL (SELECT COALESCE(j.last_error_code, 'unknown') AS code \
             FROM erp_job j WHERE j.kind = $3::text \
             AND j.dedup_key = m.id::text || ':' || m.arm_seq::text AND j.state = 'dead' \
             LIMIT 1) d ON true \
             CROSS JOIN LATERAL (SELECT m.state = 'failed' OR EXISTS (SELECT 1 FROM erp_job a \
             WHERE a.kind = $3::text AND a.dedup_key = m.id::text || ':' || m.arm_seq::text) \
             AS any_job) q \
             WHERE m.model = $1::text AND m.res_id = $2::int8 \
             ORDER BY m.scheduled_date, m.id LIMIT $4::int8",
            &[&model, &res_id, &SEND_KIND, &limit],
        )
        .await?;
    if rows.len() > MAX_SCHEDULED_PER_THREAD {
        return Err(Error::user(format!(
            "Cannot list scheduled messages, too many are scheduled on this record (more than \
             {MAX_SCHEDULED_PER_THREAD})."
        )));
    }
    let to_u64 = |id: i64| {
        u64::try_from(id)
            .map_err(|_| Error::invariant(format!("a scheduled message id {id} is not positive")))
    };
    rows.iter()
        .map(|row| {
            let id: i64 = col!(row, 0);
            let author: Option<i64> = col!(row, 4);
            let partners: Vec<i64> = col!(row, 5);
            Ok(Listed {
                id: to_u64(id)?,
                subject: col!(row, 1),
                body: col!(row, 2),
                date: col!(row, 3),
                author: author.map(to_u64).transpose()?,
                partners: partners.into_iter().map(to_u64).collect::<Result<_>>()?,
                is_note: col!(row, 6),
                state: col!(row, 7),
                reason: col!(row, 8),
                option: col!(row, 9),
            })
        })
        .collect()
}

fn run_topic<'a>(
    tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let model = input.params().text("model")?.to_owned();
        let res_id = input.params().int("res_id")?;
        let listed = list_thread(tx, &model, res_id).await?;
        let mut ids = Vec::with_capacity(listed.len());
        for row in listed {
            ids.push(row.id);
            if let Some(subject) = row.subject {
                out.set(MODEL, row.id, "subject", FieldValue::String(subject));
            }
            out.set(MODEL, row.id, "body", FieldValue::String(row.body));
            out.set(
                MODEL,
                row.id,
                "scheduled_date",
                FieldValue::Datetime(row.date),
            );
            out.set(MODEL, row.id, "author_id", FieldValue::Many2One(row.author));
            out.set(
                MODEL,
                row.id,
                "partner_ids",
                FieldValue::X2ManyReplace(row.partners),
            );
            out.set(MODEL, row.id, "is_note", FieldValue::Bool(row.is_note));
            out.set(MODEL, row.id, "state", FieldValue::Selection(row.state));
            if let Some(reason) = row.reason {
                out.set(MODEL, row.id, "failure_reason", FieldValue::String(reason));
            }
            if let Some(option) = row.option {
                out.set(
                    MODEL,
                    row.id,
                    "composition_comment_option",
                    FieldValue::Selection(option),
                );
            }
        }
        out.data(json!({ "model": model, "res_id": res_id, "ids": ids }));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_text_fits_the_stored_reason() {
        for kind in [
            ErrorKind::Access,
            ErrorKind::Validation,
            ErrorKind::UserError,
            ErrorKind::Missing,
        ] {
            let text = failure_text(kind);
            assert!(
                !text.is_empty() && text.len() <= MAX_FAILURE_REASON_BYTES,
                "{text}"
            );
        }
    }

    #[test]
    fn keys_go_to_the_jobs_api_in_batches_within_its_bound() {
        let keys: Vec<u32> = (0..25).collect();
        let sizes: Vec<usize> = key_batches(&keys, 10).map(<[u32]>::len).collect();
        assert_eq!(sizes, [10, 10, 5]);
        assert_eq!(key_batches(&keys, 25).count(), 1);
        assert_eq!(key_batches::<u32>(&[], 10).count(), 0);
        // a bound of zero never loops on empty batches
        assert_eq!(key_batches(&keys, 0).count(), 25);
        let at_bound = vec![0_u8; 2 * MAX_CANCEL_KEYS + 1];
        assert!(key_batches(&at_bound, MAX_CANCEL_KEYS).all(|b| b.len() <= MAX_CANCEL_KEYS));
        assert_eq!(key_batches(&at_bound, MAX_CANCEL_KEYS).count(), 3);
    }

    #[test]
    fn a_date_equal_to_now_is_not_in_the_past() {
        let now = 1_700_000_000_000_000_i64;
        assert!(check_not_past(DateTime::from_micros(now), now).is_ok());
        let error = check_not_past(DateTime::from_micros(now - 1), now).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), PAST_DATE);
    }

    #[test]
    fn a_date_beyond_a_year_ahead_is_refused() {
        let now = 1_700_000_000_000_000_i64;
        assert!(check_not_past(DateTime::from_micros(now + MAX_AHEAD_MICROS), now).is_ok());
        let error =
            check_not_past(DateTime::from_micros(now + MAX_AHEAD_MICROS + 1), now).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), TOO_FAR);
    }

    #[test]
    fn an_empty_edit_is_detected() {
        assert!(EditRequest::new().is_empty());
        let mut patch = EditRequest::new();
        patch.is_note = Some(false);
        assert!(!patch.is_empty());
    }

    #[test]
    fn partner_ids_are_deduplicated_and_bounded() {
        assert_eq!(check_partners(&[3, 1, 3]).unwrap_or_default(), vec![1, 3]);
        assert!(check_partners(&[0]).is_err());
        assert!(check_partners(&vec![1; MAX_IDS_PER_CALL + 1]).is_err());
    }
}
