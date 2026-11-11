//! The attachment blob store (hook H15, layer L5).
//!
//! Attachment bytes never live in a business table.
//! It has no edge to any other `erp-*` crate; the
//! `base` addon's `attachments` module holds the metadata, the link protocol and the garbage
//! collection on top of it.
//!
//! The kind `s3` ([`ObjectBackend`]) is the store every node of a scaled-out deployment shares;
//! `local` is valid for several nodes only on a shared path (`shared_filesystem = true`), and a
//! node whose directory is not the one the database registered refuses to serve. The kind
//! `postgres` (the BASE-B postgres-store follow-up) is not built yet: configuring it is a
//! [`BlobError::Configuration`] naming the task.
//!
//! ```
//! use erp_blob::{AttachmentStore, ContentKey, MemoryStore, Namespace, StageLimits, StagingKey};
//!
//! # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
//! let store = MemoryStore::new();
//! let ns = Namespace::random()?;
//! let staging = StagingKey::new(ns.clone(), "018f4f6e-7a3c-7c3e-8a9b-0123456789ab")?;
//! let mut body: &[u8] = b"hello";
//! let staged = store.stage(&staging, &mut body, StageLimits::new(1024)).await?;
//! let key = ContentKey::new(ns, *staged.sha256());
//! store.promote(&staged, &key).await?;
//! assert_eq!(store.head(&key).await?.size, 5);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod config;
mod detect;
mod error;
mod key;
mod local;
mod memory;
mod object;
mod stage;
mod store;

pub use config::{
    AttachmentConfig, AttachmentLimits, AttachmentRuntime, StoreConfig, StreamPermit,
};
pub use detect::{
    ContentClass, DetectError, Detected, MAX_MEDIA_TYPE_BYTES, detect, is_active_type,
    normalize_declared, resolve,
};
pub use error::{BlobError, LimitKind};
pub use key::{ContentKey, Namespace, StagingKey, from_hex, to_hex};
pub use local::LocalStore;
pub use memory::{Faults, MemoryStore, Op, When};
pub use object::{ObjectBackend, PART_SIZE, S3Settings};
pub use stage::{DETECT_BYTES, DIGEST_READ_BYTES, StageLimits, StagedBlob, digest_stream, digests};
pub use store::{
    AttachmentStore, BlobMeta, BlobRead, BlobReader, BodyReader, BoxFuture, ByteRange,
    Capabilities, DeleteReport, StoreKind,
};
