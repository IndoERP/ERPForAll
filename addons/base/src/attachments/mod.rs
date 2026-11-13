//! Attachments: `ir.attachment` over a content-addressed blob store (hook H15).
//!
//! The store itself is [`erp_sdk::blob`]. This module holds what lives in the database: the
//! metadata model ([`IrAttachment`]), the tables ([`schema`]), the checked read of the content
//! route ([`content`]), the write protocol that stages an
//! upload and links it inside the business transaction, the explicit parent-derived access check,
//! and the garbage collection, cleanup and verification jobs. Addons use the functions of
//! [`service`]: [`create`], [`create_unique`], [`copy`], [`reattach`], [`adopt`], [`for_record`], [`open`],
//! [`unlink_for_record`] and the [`UNLINK_HOOK`] the composition root registers.

mod access;
mod cleanup;
pub mod content;
pub mod gc;
mod link;
mod model;
pub mod ownership;
pub mod peer_probe;

mod pending;
mod quota;
pub mod recovery;
mod runtime;
pub mod schema;
mod service;
mod topology;
mod types;
mod upload;

pub use model::IrAttachment;
pub use pending::{PendingLimitExceeded, create_pending};
pub use service::{
    AdoptScope, UNLINK_HOOK, adopt, copy, create, create_unique, for_record, open, reattach,
    unlink_for_record, write_content,
};
pub use topology::verify_topology;
pub use types::{
    AttachmentContent, AttachmentMeta, BlobHandle, ListOptions, MAX_BATCH, NAME_MAX_BYTES,
    NewAttachment, ParentRef, URL_MAX_BYTES, UploadId,
};
pub use upload::{StageRequest, StagedUpload, Uploader};

pub(crate) use schema::{REQUIREMENT, keep_registration};
