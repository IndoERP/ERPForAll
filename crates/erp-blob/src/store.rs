//! The [`AttachmentStore`] trait and the types of its calls.

use std::future::Future;
use std::pin::Pin;

use tokio::io::AsyncRead;

use crate::key::{ContentKey, StagingKey};
use crate::stage::{StageLimits, StagedBlob};
use crate::{BlobError, ContestState, Namespace, OwnerMarker, OwnerState, OwnerVersion};

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
    /// Current local root/mount identity, read without a fingerprint cache.
    /// # Errors
    /// Explicitly unsupported for stores without a shared-local peer protocol.
    fn peer_binding(&self) -> Result<String, BlobError> {
        Err(BlobError::OwnerCapabilityMissing {
            capability: "shared-local peer binding",
        })
    }

    /// Check that the store still is the one the database registered.
    ///
    /// # Errors
    ///
    /// `Unavailable` (not retryable) when it is another store.
    fn verify_fingerprint<'a>(&'a self, expected: &'a str) -> BoxFuture<'a, Result<(), BlobError>>;

    /// Probe real owner create/CAS/conditional-check semantics using private temporary markers.
    /// Callers must run this before admitting destructive maintenance; flags alone are insufficient.
    ///
    /// # Errors
    /// Typed unsupported/missing-capability, marker, backend or probe-cleanup failure.
    fn probe_owner(&self) -> BoxFuture<'_, Result<(), BlobError>> {
        Box::pin(async { Err(BlobError::UnsupportedOwnerFence) })
    }

    /// Read the owner and its concurrency token atomically; absence is explicit.
    ///
    /// # Errors
    /// Unsupported capability, malformed marker/token or backend failure.
    fn owner<'a>(
        &'a self,
        _ns: &'a Namespace,
    ) -> BoxFuture<'a, Result<Option<OwnerState>, BlobError>> {
        Box::pin(async { Err(BlobError::UnsupportedOwnerFence) })
    }

    /// Create the owner when `expected` is absent; otherwise compare-and-swap its exact version.
    /// The caller must durably record the intended operation before issuing this put.
    ///
    /// # Errors
    /// `OwnerConflict` for a failed precondition, or unsupported/marker/backend failure.
    fn put_owner<'a>(
        &'a self,
        _ns: &'a Namespace,
        _marker: &'a OwnerMarker,
        _expected: Option<&'a OwnerVersion>,
    ) -> BoxFuture<'a, Result<OwnerVersion, BlobError>> {
        Box::pin(async { Err(BlobError::UnsupportedOwnerFence) })
    }

    /// Check the current owner version, without selecting a historical object version.
    /// This checks neither contest absence nor observation delay: the coordinator checks those too.
    ///
    /// # Errors
    /// `OwnerConflict` for absent/changed ownership; unsupported/marker/backend failure otherwise.
    fn check_owner<'a>(
        &'a self,
        _ns: &'a Namespace,
        _expected: &'a OwnerVersion,
    ) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async { Err(BlobError::UnsupportedOwnerFence) })
    }

    /// Read the losing operation in an active `_contested` record; an active contest prohibits deletion.
    /// A validated operator resolution receipt is returned as no active contest.
    ///
    /// # Errors
    /// Unsupported capability, malformed record or backend failure; none is contest absence.
    fn contested<'a>(
        &'a self,
        _ns: &'a Namespace,
    ) -> BoxFuture<'a, Result<Option<OwnerMarker>, BlobError>> {
        Box::pin(async { Err(BlobError::UnsupportedOwnerFence) })
    }

    /// Create an active contest without overwriting an earlier loser; an active marker is success.
    /// A prior resolution receipt is conditionally replaced with a fresh active contest.
    /// Only audited operator recovery may clear it; resolution requires the audited recovery port.
    ///
    /// # Errors
    /// Unsupported capability or backend failure.
    fn create_contested<'a>(
        &'a self,
        _ns: &'a Namespace,
        _loser: &'a OwnerMarker,
    ) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async { Err(BlobError::UnsupportedOwnerFence) })
    }

    /// Atomically read contest content and its exact version, including resolution receipts.
    /// # Errors
    /// Unsupported capability, malformed marker or backend failure.
    fn contest_state<'a>(
        &'a self,
        _ns: &'a Namespace,
    ) -> BoxFuture<'a, Result<Option<ContestState>, BlobError>> {
        Box::pin(async { Err(BlobError::UnsupportedOwnerFence) })
    }
    /// CAS a contest to a resolution receipt only after the exact chosen owner is established.
    /// Only the audited owner-side recovery coordinator may invoke this low-level port.
    /// Replaying this exact resolved operation is idempotent; another contest/version refuses.
    /// # Errors
    /// OwnerConflict, unsupported capability, malformed marker or backend failure. An error
    /// after publication may be ambiguous: inspect the persisted exact operation on retry.
    fn resolve_contested<'a>(
        &'a self,
        _ns: &'a Namespace,
        _owner: &'a OwnerState,
        _expected: Option<&'a ContestState>,
    ) -> BoxFuture<'a, Result<ContestState, BlobError>> {
        Box::pin(async { Err(BlobError::UnsupportedOwnerFence) })
    }

    /// Stream `body` into a staging object, hashing it on the way.
    ///
    /// # Errors
    ///
    /// `LimitExceeded` as soon as the stream passes `limits.max_bytes`; a partial staging object
    /// may remain, owned by the caller's durable intent until fenced cleanup;
    /// `Unavailable` or `PermissionDenied` for store failures.
    fn stage<'a>(
        &'a self,
        key: &'a StagingKey,
        body: BodyReader<'a>,
        limits: StageLimits,
    ) -> BoxFuture<'a, Result<StagedBlob, BlobError>>;

    /// Publish staged bytes at their content key, create-only, retaining the staging source.
    /// An existing key is accepted after its size is checked. The caller retains its durable
    /// upload intent; only fenced maintenance calls `delete_staged` after consume/expiry.
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
