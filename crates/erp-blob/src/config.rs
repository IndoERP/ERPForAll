//! Store configuration and the runtime product.
//!
//! Stores are server configuration, never tenant data: `[attachments] write_store = "<id>"` and
//! `[attachments.stores.<id>] kind = …`. Unknown keys, unknown kinds and kinds that are not built
//! yet fail with an error that names the setting or the task. [`AttachmentRuntime`] is the
//! opened, validated product a composition root attaches to the ORM generation
//! (`OrmBuilder::generation_attachment`).
//!
//! Several nodes need a store they all reach: `s3`, or `local` with `shared_filesystem = true`
//! on a path every node mounts. A node-local directory is refused for a second live node
//! (`AttachmentRuntime::unshared_local_stores`, checked against the node registry by the base
//! addon), and a node that mounts another directory than the one the database registered fails
//! its fingerprint check.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::BlobError;
use crate::local::LocalStore;
use crate::store::AttachmentStore;

/// The size limits and timings of the attachment subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AttachmentLimits {
    /// Blobs up to this many bytes live in the database row (`ir_blob_inline`); 0 turns it off.
    pub inline_max_bytes: usize,
    /// The most bytes of one attachment.
    pub max_file_bytes: u64,
    /// The longest attachment name in bytes.
    pub name_max_bytes: usize,
    /// How long an unreferenced blob is kept before the sweep removes its bytes.
    pub gc_grace_secs: u64,
    /// Optional soft database quota, including retained staging bytes.
    pub quota_bytes: Option<u64>,
    /// Hard database-wide staging reservation cap, across every node and subject.
    pub staging_max_bytes: u64,
    /// Pending uploads one subject may hold.
    pub pending_uploads_per_subject: u32,
    /// How long an upload may wait to be linked.
    pub upload_ttl_secs: u64,
    /// How often the store fingerprint is checked again.
    pub fingerprint_recheck_secs: u64,
    /// Streaming operations (an upload's stage, a content proof, a deep verification read) one
    /// node runs at once: a **per-node** admission bound, not a cluster-wide limit. The
    /// deployment's capacity is nodes × this value; a call beyond it fails at once with
    /// [`BlobError::Busy`] instead of queueing.
    pub stream_permits_per_node: u32,
}

impl Default for AttachmentLimits {
    fn default() -> Self {
        Self {
            inline_max_bytes: 2048,
            max_file_bytes: 128 * 1024 * 1024,
            name_max_bytes: 255,
            gc_grace_secs: 86_400,
            quota_bytes: None,
            staging_max_bytes: 4 * 1024 * 1024 * 1024,
            pending_uploads_per_subject: 100,
            upload_ttl_secs: 86_400,
            fingerprint_recheck_secs: 300,
            stream_permits_per_node: 32,
        }
    }
}

impl AttachmentLimits {
    /// The hard bounds of a configured value.
    pub const INLINE_MAX_BYTES_BOUND: usize = 64 * 1024;
    pub const MAX_FILE_BYTES_RANGE: (u64, u64) = (1024 * 1024, 2_147_483_647);
    /// The smallest and largest `gc_grace_secs` (floor of one hour, hard bound of 30 days).
    pub const GC_GRACE_RANGE: (u64, u64) = (3_600, 30 * 86_400);
    /// The smallest and largest `stream_permits_per_node`.
    pub const STREAM_PERMITS_RANGE: (u32, u32) = (1, 1024);

    /// A copy with another per-node stream bound, bypassing the configuration bounds (tests).
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_stream_permits_for_tests(mut self, permits: u32) -> Self {
        self.stream_permits_per_node = permits;
        self
    }

    /// A copy with another grace, outside the production floor. Only for tests that force the
    /// sweep to see an unreferenced blob at once.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_gc_grace_secs_for_tests(mut self, secs: u64) -> Self {
        self.gc_grace_secs = secs;
        self
    }

    /// A copy with another inline threshold, bypassing the configuration bounds (tests).
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_inline_max_bytes_for_tests(mut self, bytes: usize) -> Self {
        self.inline_max_bytes = bytes;
        self
    }

    /// A copy with another file limit, bypassing the configuration bounds (tests).
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_max_file_bytes_for_tests(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// A copy with another per-subject pending-upload cap, bypassing the bounds (tests).
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_pending_uploads_for_tests(mut self, count: u32) -> Self {
        self.pending_uploads_per_subject = count;
        self
    }
}

/// One configured store.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreConfig {
    /// A directory.
    Local {
        /// The directory.
        path: PathBuf,
        /// The operator states that every node mounts this path from shared storage. Without it
        /// the directory is node-local, and a second live node refuses to start serving
        /// attachments (the node registry shows it; a different `.store-id` shows it too).
        shared_filesystem: bool,
    },
    /// An S3-compatible bucket, shared by every node.
    S3(crate::object::S3Settings),
}

/// The validated configuration of the attachment subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AttachmentConfig {
    /// The store new content goes to.
    pub write_store: String,
    /// Every configured store, by identifier.
    pub stores: BTreeMap<String, StoreConfig>,
    /// Limits and timings.
    pub limits: AttachmentLimits,
}

#[derive(Deserialize)]
struct Document {
    attachments: Option<RawAttachments>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAttachments {
    write_store: Option<String>,
    stores: Option<BTreeMap<String, RawStore>>,
    inline_max_bytes: Option<usize>,
    max_file_bytes: Option<u64>,
    gc_grace_secs: Option<u64>,
    quota_bytes: Option<u64>,
    staging_max_bytes: Option<u64>,
    pending_uploads_per_subject: Option<u32>,
    upload_ttl_secs: Option<u64>,
    fingerprint_recheck_secs: Option<u64>,
    stream_permits_per_node: Option<u32>,
    encryption: Option<String>,
    scan: Option<RawScan>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScan {
    mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStore {
    kind: String,
    path: Option<PathBuf>,
    shared_filesystem: Option<bool>,
    bucket: Option<String>,
    region: Option<String>,
    endpoint: Option<String>,
    prefix: Option<String>,
    path_style: Option<bool>,
}

fn bounded<T: PartialOrd + std::fmt::Display + Copy>(
    name: &str,
    value: T,
    low: T,
    high: T,
) -> Result<T, BlobError> {
    if value < low || value > high {
        return Err(BlobError::config(format!(
            "attachments.{name} = {value} is outside {low} to {high}"
        )));
    }
    Ok(value)
}

impl AttachmentConfig {
    /// Parse the `[attachments]` table of a server configuration text. Other tables are ignored.
    ///
    /// # Errors
    ///
    /// `Configuration`, naming the key: a missing `attachments` table or `write_store`, an
    /// unknown key or kind, a value outside its bounds, `encryption = "on"`, a
    /// scan mode other than `off` (the scanner addon), the `s3` kind (`the BASE-B s3 follow-up`), the
    /// `postgres` kind (the BASE-B postgres-store follow-up).
    pub fn from_toml(text: &str) -> Result<Self, BlobError> {
        let document: Document = toml::from_str(text).map_err(|e| {
            let setting = e
                .span()
                .and_then(|span| text.get(..span.start))
                .and_then(|prefix| prefix.rsplit('\n').next())
                .and_then(|line| line.split_once('='))
                .map(|(key, _)| key.trim())
                .filter(|key| matches!(*key, "quota_bytes" | "staging_max_bytes"));
            let detail = setting.map_or_else(String::new, |key| format!(" (attachments.{key})"));
            BlobError::config(format!(
                "the [attachments] table is not valid: {}{detail}",
                e.message()
            ))
        })?;
        let raw = document.attachments.ok_or_else(|| {
            BlobError::config(
                "the [attachments] table is missing; set attachments.write_store and a store",
            )
        })?;
        if raw.encryption.as_deref().is_some_and(|e| e != "off") {
            return Err(BlobError::config(
                "attachments.encryption = \"on\" is not built yet: it needs a later release",
            ));
        }
        if raw
            .scan
            .as_ref()
            .and_then(|s| s.mode.as_deref())
            .is_some_and(|m| m != "off")
        {
            return Err(BlobError::config(
                "attachments.scan.mode other than \"off\" needs a content scanner addon",
            ));
        }
        let write_store = raw
            .write_store
            .ok_or_else(|| BlobError::config("attachments.write_store is not set"))?;
        let mut stores = BTreeMap::new();
        for (id, store) in raw.stores.unwrap_or_default() {
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(BlobError::config(format!(
                    "attachments.stores.{id}: a store id is 1 to 64 letters, digits, '_' or '-'"
                )));
            }
            let parsed = match store.kind.as_str() {
                "local" => {
                    let path = store.path.ok_or_else(|| {
                        BlobError::config(format!("attachments.stores.{id}.path is not set"))
                    })?;
                    if !path.is_absolute() {
                        return Err(BlobError::config(format!(
                            "attachments.stores.{id}.path must be an absolute directory"
                        )));
                    }
                    StoreConfig::Local {
                        path,
                        shared_filesystem: store.shared_filesystem == Some(true),
                    }
                }
                "s3" => {
                    let need = |value: Option<String>, key: &str| {
                        value.filter(|v| !v.is_empty()).ok_or_else(|| {
                            BlobError::config(format!("attachments.stores.{id}.{key} is not set"))
                        })
                    };
                    let endpoint = store.endpoint.filter(|e| !e.is_empty());
                    if let Some(endpoint) = &endpoint {
                        let lower = endpoint.to_ascii_lowercase();
                        let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
                            .iter()
                            .any(|p| lower.starts_with(p));
                        if !(lower.starts_with("https://") || loopback) {
                            return Err(BlobError::config(format!(
                                "attachments.stores.{id}.endpoint must be https; plain http is allowed only for a loopback address"
                            )));
                        }
                    }
                    StoreConfig::S3(crate::object::S3Settings {
                        bucket: need(store.bucket, "bucket")?,
                        region: need(store.region, "region")?,
                        endpoint,
                        prefix: store.prefix.unwrap_or_default(),
                        path_style: store.path_style.unwrap_or(false),
                    })
                }
                "postgres" => {
                    return Err(BlobError::config(format!(
                        "attachments.stores.{id}.kind = \"postgres\" is not built yet: it is the BASE-B postgres-store follow-up"
                    )));
                }
                "azure" | "gcs" => {
                    return Err(BlobError::config(format!(
                        "attachments.stores.{id}.kind = \"{}\" is not built yet: it is the task a later release",
                        store.kind
                    )));
                }
                other => {
                    return Err(BlobError::config(format!(
                        "attachments.stores.{id}.kind = \"{other}\" is unknown (local, s3, postgres)"
                    )));
                }
            };
            stores.insert(id, parsed);
        }
        if !stores.contains_key(&write_store) {
            return Err(BlobError::config(format!(
                "attachments.write_store = \"{write_store}\" names no store in attachments.stores"
            )));
        }
        let mut limits = AttachmentLimits::default();
        if let Some(v) = raw.inline_max_bytes {
            limits.inline_max_bytes = bounded(
                "inline_max_bytes",
                v,
                0,
                AttachmentLimits::INLINE_MAX_BYTES_BOUND,
            )?;
        }
        if let Some(v) = raw.max_file_bytes {
            let (low, high) = AttachmentLimits::MAX_FILE_BYTES_RANGE;
            limits.max_file_bytes = bounded("max_file_bytes", v, low, high)?;
        }
        if let Some(v) = raw.gc_grace_secs {
            let (low, high) = AttachmentLimits::GC_GRACE_RANGE;
            limits.gc_grace_secs = bounded("gc_grace_secs", v, low, high)?;
        }
        if let Some(v) = raw.quota_bytes {
            limits.quota_bytes = Some(bounded("quota_bytes", v, 1024 * 1024, i64::MAX as u64)?);
        }
        if let Some(v) = raw.staging_max_bytes {
            limits.staging_max_bytes =
                bounded("staging_max_bytes", v, 1024 * 1024, i64::MAX as u64)?;
        }
        if let Some(v) = raw.pending_uploads_per_subject {
            limits.pending_uploads_per_subject = bounded("pending_uploads_per_subject", v, 1, 100)?;
        }
        if let Some(v) = raw.upload_ttl_secs {
            limits.upload_ttl_secs = bounded("upload_ttl_secs", v, 60, 7 * 86_400)?;
        }
        if let Some(v) = raw.fingerprint_recheck_secs {
            limits.fingerprint_recheck_secs = bounded("fingerprint_recheck_secs", v, 10, 3_600)?;
        }
        if let Some(v) = raw.stream_permits_per_node {
            let (low, high) = AttachmentLimits::STREAM_PERMITS_RANGE;
            limits.stream_permits_per_node = bounded("stream_permits_per_node", v, low, high)?;
        }
        Ok(Self {
            write_store,
            stores,
            limits,
        })
    }
}

/// The opened stores of one ORM generation and the limits, attached by the composition root.
pub struct AttachmentRuntime {
    stores: BTreeMap<String, Arc<dyn AttachmentStore>>,
    write_store: String,
    limits: AttachmentLimits,
    /// When each store's fingerprint was last verified.
    verified: Mutex<BTreeMap<String, Instant>>,
    /// Local stores not declared as shared storage.
    unshared_local: Vec<String>,
    /// The per-node admission bound of streaming operations
    /// ([`AttachmentLimits::stream_permits_per_node`]).
    streams: Arc<tokio::sync::Semaphore>,
}

/// One admitted streaming operation of this node: held while the bytes pass, released when
/// dropped (also on an early error or a cancelled future).
#[derive(Debug)]
#[must_use = "the admission lasts as long as the permit is held"]
pub struct StreamPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

fn stream_semaphore(limits: &AttachmentLimits) -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(
        usize::try_from(limits.stream_permits_per_node).unwrap_or(usize::MAX),
    ))
}

impl std::fmt::Debug for AttachmentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentRuntime")
            .field("write_store", &self.write_store)
            .field("stores", &self.stores.keys().collect::<Vec<_>>())
            .field("limits", &self.limits)
            .finish()
    }
}

impl AttachmentRuntime {
    /// Open every configured store.
    ///
    /// # Errors
    ///
    /// The error of a store that cannot be opened (a directory that cannot be created or whose
    /// `.store-id` is damaged).
    pub async fn open(config: AttachmentConfig) -> Result<Self, BlobError> {
        let mut stores: BTreeMap<String, Arc<dyn AttachmentStore>> = BTreeMap::new();
        let mut unshared_local = Vec::new();
        for (id, store) in &config.stores {
            match store {
                StoreConfig::Local {
                    path,
                    shared_filesystem,
                } => {
                    stores.insert(id.clone(), Arc::new(LocalStore::open(path.clone()).await?));
                    if !shared_filesystem {
                        unshared_local.push(id.clone());
                    }
                }
                StoreConfig::S3(settings) => {
                    stores.insert(
                        id.clone(),
                        Arc::new(crate::object::ObjectBackend::s3(settings).await?),
                    );
                }
            }
        }
        Ok(Self {
            stores,
            write_store: config.write_store,
            streams: stream_semaphore(&config.limits),
            limits: config.limits,
            verified: Mutex::new(BTreeMap::new()),
            unshared_local,
        })
    }

    /// A runtime over one ready store (composition of an already opened store, and tests).
    pub fn single(
        store_key: impl Into<String>,
        store: Arc<dyn AttachmentStore>,
        limits: AttachmentLimits,
    ) -> Self {
        let key = store_key.into();
        let mut stores = BTreeMap::new();
        stores.insert(key.clone(), store);
        Self {
            stores,
            write_store: key,
            streams: stream_semaphore(&limits),
            limits,
            verified: Mutex::new(BTreeMap::new()),
            unshared_local: Vec::new(),
        }
    }

    /// The identifier of the store new content goes to.
    pub fn write_store_key(&self) -> &str {
        &self.write_store
    }

    /// The store new content goes to.
    ///
    /// # Errors
    ///
    /// `Configuration` if the runtime was built without it (cannot happen through `open` or
    /// `single`).
    pub fn write_store(&self) -> Result<&Arc<dyn AttachmentStore>, BlobError> {
        self.stores
            .get(&self.write_store)
            .ok_or_else(|| BlobError::config("the write store is not among the opened stores"))
    }

    /// A store by its configured identifier.
    pub fn store(&self, key: &str) -> Option<&Arc<dyn AttachmentStore>> {
        self.stores.get(key)
    }

    /// The identifiers of every opened store.
    pub fn store_keys(&self) -> impl Iterator<Item = &str> {
        self.stores.keys().map(String::as_str)
    }

    /// The limits.
    pub fn limits(&self) -> &AttachmentLimits {
        &self.limits
    }

    /// Admit one streaming operation on this node, or refuse at once when
    /// [`AttachmentLimits::stream_permits_per_node`] are all held. Never waits: a full node
    /// answers busy, it does not queue. The bound is per node (per runtime), not cluster-wide.
    ///
    /// # Errors
    ///
    /// [`BlobError::Busy`] (retryable) when every permit is held.
    pub fn try_stream_permit(&self) -> Result<StreamPermit, BlobError> {
        Arc::clone(&self.streams)
            .try_acquire_owned()
            .map(|permit| StreamPermit { _permit: permit })
            .map_err(|_| BlobError::Busy {
                permits: self.limits.stream_permits_per_node,
            })
    }

    /// Permits of [`Self::try_stream_permit`] free now (metrics and tests).
    pub fn available_stream_permits(&self) -> usize {
        self.streams.available_permits()
    }

    /// The local stores not declared as shared storage. A deployment with more than one live node
    /// must not have any: every node has to reach the same bytes.
    pub fn unshared_local_stores(&self) -> &[String] {
        &self.unshared_local
    }

    /// Check that store `store_key` still is the one the database registered (`expected` is the
    /// fingerprint of its `ir_blob_store` row). The store is asked at most every
    /// `fingerprint_recheck_secs`; a failed check is asked again at the next call.
    ///
    /// # Errors
    ///
    /// `Configuration` for an unknown store; `Unavailable` (not retryable) when the store is
    /// another directory or bucket.
    pub async fn verify_fingerprint(
        &self,
        store_key: &str,
        expected: &str,
    ) -> Result<(), BlobError> {
        let store = self.store(store_key).ok_or_else(|| {
            BlobError::config(format!(
                "the database uses store {store_key}, which is not configured"
            ))
        })?;
        let recheck = Duration::from_secs(self.limits.fingerprint_recheck_secs);
        let due = {
            let verified = self.verified.lock().unwrap_or_else(PoisonError::into_inner);
            verified
                .get(store_key)
                .is_none_or(|at| at.elapsed() >= recheck)
        };
        if !due {
            return Ok(());
        }
        store.verify_fingerprint(expected).await?;
        self.verified
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(store_key.to_owned(), Instant::now());
        Ok(())
    }
}
