//! Discuss storage: messages, message subtypes, followers and the `mail.thread` mixin,
//! and the record-thread post
//! path, and the thread API a client
//! calls over RPC.

erp_sdk::addon!(name = "mail", depends = ["base"]);

pub mod activity;
pub(crate) mod activity_date;
pub mod activity_menu;
pub mod activity_mixin;
pub mod activity_type;
pub mod followers;
pub mod mail_thread;
pub mod message;
pub mod message_attachments;
pub mod post;
pub mod scheduled;
pub mod store;
pub mod store_fetch;
pub mod subtype;
pub mod thread_load;
pub mod thread_rpc;

mod res_company;
mod res_partner;
mod res_users;

pub use activity::{MailActivity, ScheduleRequest};
pub use activity_menu::{
    ActivityGroup, ActivityGroups, MAX_OPEN_RECORDS, OpenState, activity_group_action,
    activity_groups,
};
pub use activity_mixin::{ActivityPanel, MailActivityMixin, PanelActivity};
pub use activity_type::MailActivityType;
pub use followers::MailFollowers;
pub use mail_thread::MailThread;
pub use message::{MailMessage, THREAD_ACCESS};
pub use post::{
    Body, COMPOSER_MODEL, MAX_ATTACHMENTS, MAX_BODY_BYTES, MAX_MENTIONS, Mention, MentionTarget,
    PostOptions, PostRequest, PostSubtype, PostType, Posted, StagedAttachment,
};
// `activity::ScheduleRequest` (activities) keeps the crate-root name; the scheduled-message
// request is exported as `ScheduledMessageRequest` (train rename: both branches exported
// `ScheduleRequest`).
pub use scheduled::ScheduleRequest as ScheduledMessageRequest;
pub use scheduled::{
    CommentOption, EditRequest, MAX_FAILURE_REASON_BYTES, MAX_SCHEDULED_PER_THREAD,
    MailScheduledMessage, job_kinds,
};
pub use store::StoreFields;
pub use subtype::{DefaultSubtypes, MailMessageSubtype};
pub use thread_load::{
    DEFAULT_MESSAGE_PAGE, MAX_MESSAGE_PAGE, MailMessageStore, MailThreadStore, ThreadFacts,
};
pub use thread_rpc::{MailThreadRpc, PostWire};
