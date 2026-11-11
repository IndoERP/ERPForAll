//! The [`AttachmentStore`] trait and the types of its calls.

use std::future::Future;
use std::pin::Pin;

use tokio::io::AsyncRead;

use crate::BlobError;
use crate::key::{ContentKey, StagingKey};
use crate::stage::{StageLimits, StagedBlob};

/// A boxed future, so the trait is object safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A byte stream out of the store.
pub type BlobReader = Pin<Box<dyn AsyncRead + Send>>;

/// The kind of a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreKind {
    /// A directory on a filesystem.
    Local,
    /// Process memory, for tests; cannot be built from server configuration.
    Memory,
    /// An S3-compatible bucket: shared by every node.
    S3,
}

/// What a store can do (probed at startup, checked by the features that
/// need a capability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// Ranged reads.
    pub range_reads: bool,
    /// Presigned download URLs.
    pub presign: bool,
    /// `promote` never overwrites an existing key.
    pub create_only: bool,
    /// Conditional put with `If-Match` (the owner fence).
    pub if_match_put: bool,
}

/// One inclusive-start byte range of a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// The first byte.
    pub start: u64,
    /// How many bytes, at least one.
    pub len: u64,
}

impl ByteRange {
    /// `len` bytes from `start`.
    pub const fn new(start: u64, len: u64) -> Self {
        Self { start, len }
    }
}

/// The metadata of an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobMeta {
    /// The size in bytes.
    pub size: u64,
}

/// A read: the whole object's size and the bytes asked for.
pub struct BlobRead {
    /// The size of the whole object, not of the range.
    pub total_size: u64,
    /// The bytes (the range, when one was asked for).
    pub reader: BlobReader,
}

impl std::fmt::Debug for BlobRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobRead")
            .field("total_size", &self.total_size)
            .finish_non_exhaustive()
    }
}

/// What a `delete_many` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeleteReport {
    /// Keys removed by this call.
    pub deleted: usize,
    /// Keys that were already absent (not an error: the next sweep finishes a lost commit).
    pub already_absent: usize,
}

/// The body of an upload.
pub type BodyReader<'a> = &'a mut (dyn AsyncRead + Send + Unpin);

/// Where attachment bytes live. Content keys are never overwritten; a missing key is an error.
pub trait AttachmentStore: Send + Sync + 'static {
    /// The kind.
    fn kind(&self) -> StoreKind;

    /// What the store can do.
    fn capabilities(&self) -> Capabilities;

    /// A fingerprint that identifies this store's backing directory or bucket, written in the
    /// database's store row and checked before use, or `None` when the kind has none.
    fn fingerprint(&self) -> Option<String>;

    /// Check that the store still is the one the database registered.
    ///
    /// # Errors
    ///
    /// `Unavailable` (not retryable) when it is another store.
    fn verify_fingerprint<'a>(&'a self, expected: &'a str) -> BoxFuture<'a, Result<(), BlobError>>;

    /// Stream `body` into a staging object, hashing it on the way.
    ///
    /// # Errors
    ///
    /// `LimitExceeded` as soon as the stream passes `limits.max_bytes` (nothing is kept);
    /// `Unavailable` or `PermissionDenied` for store failures.
    fn stage<'a>(
        &'a self,
        key: &'a StagingKey,
        body: BodyReader<'a>,
        limits: StageLimits,
    ) -> BoxFuture<'a, Result<StagedBlob, BlobError>>;

    /// Move staged bytes to their content key, create-only. An existing key is accepted: the
    /// bytes under a key are identical by construction.
    ///
    /// # Errors
    ///
    /// `NotFound` when the staging object is gone and the key is absent; `Integrity` when the
    /// existing key has another size.
    fn promote<'a>(
        &'a self,
        staged: &'a StagedBlob,
        key: &'a ContentKey,
    ) -> BoxFuture<'a, Result<(), BlobError>>;

    /// Read an object, or a range of it.
    ///
    /// # Errors
    ///
    /// `NotFound` for a missing key (never empty content); `InvalidRange` for a range outside
    /// the object.
    fn get<'a>(
        &'a self,
        key: &'a ContentKey,
        range: Option<ByteRange>,
    ) -> BoxFuture<'a, Result<BlobRead, BlobError>>;

    /// The metadata of an object.
    ///
    /// # Errors
    ///
    /// `NotFound` for a missing key.
    fn head<'a>(&'a self, key: &'a ContentKey) -> BoxFuture<'a, Result<BlobMeta, BlobError>>;

    /// Remove content keys. An absent key is counted, not an error.
    ///
    /// # Errors
    ///
    /// `Unavailable` or `PermissionDenied`; keys removed before the failure stay removed.
    fn delete_many<'a>(
        &'a self,
        keys: &'a [ContentKey],
    ) -> BoxFuture<'a, Result<DeleteReport, BlobError>>;

    /// Remove a staging object. An absent object is not an error.
    ///
    /// # Errors
    ///
    /// `Unavailable` or `PermissionDenied`.
    fn delete_staged<'a>(&'a self, key: &'a StagingKey) -> BoxFuture<'a, Result<(), BlobError>>;

    /// List keys under `prefix` after `after`, at most `limit`, in order (verification and import
    /// roles only; the cost grows with the store).
    ///
    /// # Errors
    ///
    /// `Unavailable` or `PermissionDenied`.
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, BlobError>>;
}
