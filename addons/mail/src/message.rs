//! `mail.message`: one row per message of a record thread or a channel.
//!
//! Storage and §4.1a; the post
//! path (`client_uid`, `silent`) is (`crate::post`). Tracking and the search vector are
//! later slices; the attachment link (`attachment_ids`) is
//! . Read access to messages is
//! : the `thread_access` rule operator declared below and the rows of
//! `security/ir.access.csv`.

use erp_sdk::DefaultContext;
use erp_sdk::prelude::*;

erp_sdk::rule_operator!(
    /// `thread_access` (Amendment 1): in the rows of `security/ir.access.csv`,
    /// `('res_id', 'thread_access', 'read')` stands for "the principal may read the document
    /// `(model, res_id)`", for the documents of every concrete model that inherits `mail.thread`.
    /// `erp-security` expands it per model when it builds the predicate; RPC and search domains
    /// refuse the spelling like any unknown operator. Read only for now: posting and writing map
    /// to the document's own operations in a later slice.
    pub THREAD_ACCESS = "thread_access" {
        addon: "mail",
        model: "mail.message",
        field: "res_id",
        inherits: "mail.thread",
        operations: [Read],
    }
);

/// A message. `model`/`res_id` point at the thread (no foreign key: deleting a thread deletes its
/// messages through the `mail.thread` `unlink` override).
#[model(
    name = "mail.message",
    description = "Message",
    order = "id desc",
    rec_name = "subject",
    addon = "mail",
    // chatter pages read a thread newest first (§4.1); a typed index has no DESC key
    indexes = [("_model_res_id_id_desc", "(model, res_id, id DESC)")],
    // the post path's idempotency key; a plain UNIQUE, so
    // the many messages without a client uid never conflict
    keys = [key(
        name = "_mail_message_client_uid_uniq",
        fields = ["client_uid"],
        message = "A message with this client_uid exists already.",
        nulls_distinct = true
    )]
)]
pub struct MailMessage {
    /// Subject line.
    #[field(char)]
    pub subject: Option<Arc<str>>,
    /// Message date.
    #[field(datetime, default_fn = now)]
    pub date: Option<DateTime>,
    /// Sanitized HTML body.
    #[field(html, default = "", string = "Contents")]
    pub body: Option<Arc<str>>,
    /// Message replied to.
    #[field(
        many2one,
        comodel = "mail.message",
        index = "btree_not_null",
        ondelete = "set null",
        string = "Parent Message"
    )]
    pub parent_id: Option<RecId>,
    /// Replies.
    #[field(one2many, comodel = "mail.message", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
    /// Model of the thread.
    #[field(char, string = "Related Document Model")]
    pub model: Option<Arc<str>>,
    /// Id of the thread record.
    #[field(
        many2one_reference,
        model_field = "model",
        string = "Related Document ID"
    )]
    pub res_id: i64,
    /// What produced the message.
    #[field(
        selection,
        selection = [
            ("email", "Incoming Email"),
            ("comment", "Comment"),
            ("email_outgoing", "Outgoing Email"),
            ("notification", "System notification"),
            ("auto_comment", "Automated Targeted Notification"),
            ("out_of_office", "Out-of-office Message"),
            ("user_notification", "User Specific Notification"),
            ("tracking", "Tracking")
        ],
        required = true,
        default = "comment",
        string = "Type"
    )]
    pub message_type: Option<Arc<str>>,
    /// Category of the message.
    #[field(
        many2one,
        comodel = "mail.message.subtype",
        ondelete = "set null",
        index = true
    )]
    pub subtype_id: Option<RecId>,
    /// Hidden from portal and public users whatever the subtype.
    #[field(boolean, string = "Employee Only")]
    pub is_internal: bool,
    /// Sender address when no partner matched.
    #[field(char, string = "From")]
    pub email_from: Option<Arc<str>>,
    /// Author.
    #[field(many2one, comodel = "res.partner", index = true, ondelete = "set null")]
    pub author_id: Option<RecId>,
    /// Explicit recipients (To), archived partners included.
    #[field(
        many2many,
        comodel = "res.partner",
        relation = "mail_message_res_partner_rel",
        column1 = "mail_message_id",
        column2 = "res_partner_id",
        context = [("active_test", false)],
        string = "Recipients (To)"
    )]
    pub partner_ids: Vec<RecId>,
    /// Everyone the message reached through any notification channel (§4.1a).
    #[field(
        many2many,
        comodel = "res.partner",
        relation = "mail_message_notified_rel",
        column1 = "message_id",
        column2 = "partner_id",
        context = [("active_test", false)],
        copy = false,
        string = "Notified Partners"
    )]
    pub notified_partner_ids: Vec<RecId>,
    /// Partners who bookmarked the message.
    #[field(
        many2many,
        comodel = "res.partner",
        relation = "mail_message_res_partner_bookmarked_rel",
        column1 = "mail_message_id",
        column2 = "res_partner_id",
        string = "Bookmarked By"
    )]
    pub bookmarked_partner_ids: Vec<RecId>,
    /// The files belong to the
    /// record the message is on (`ir.attachment` `res_model`/`res_id`); this link says which
    /// message they came with. The thread topics list those that still belong to that record.
    #[field(
        many2many,
        comodel = "ir.attachment",
        relation = "message_attachment_rel",
        column1 = "message_id",
        column2 = "attachment_id",
        string = "Attachments"
    )]
    pub attachment_ids: Vec<RecId>,
    /// RFC 5322 Message-Id; not unique (one inbound mail routed to several threads).
    #[field(
        char,
        index = true,
        readonly = true,
        copy = false,
        string = "Message-Id"
    )]
    pub message_id: Option<Arc<str>>,
    /// Reply-To address.
    #[field(char, string = "Reply-To")]
    pub reply_to: Option<Arc<str>>,
    /// Id the posting client chose for the post, so a retried post returns the stored message
    /// instead of posting twice (`message_post`).
    #[field(uuid, readonly = true, copy = false, string = "Client UID")]
    pub client_uid: Option<Uuid>,
    /// Type of the activity a done-activity message records.
    #[field(
        many2one,
        comodel = "mail.activity.type",
        ondelete = "set null",
        index = "btree_not_null",
        readonly = true,
        copy = false,
        string = "Activity Type"
    )]
    pub mail_activity_type_id: Option<RecId>,
    /// Posted without push or out-of-focus alert (5); stored for the push job.
    #[field(boolean, readonly = true, copy = false)]
    pub silent: bool,
}

fn now(_ctx: &DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}
