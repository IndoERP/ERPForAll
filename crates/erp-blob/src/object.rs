//! The `s3` store: any S3-compatible object store through `object_store`.
//!
//! There is no node-local state: every node of a deployment talks to the same bucket, so a blob
//! one node writes is read by any other, and the store survives the loss of any node. Staging is
//! a multipart upload in fixed parts (memory is one part, 8 MiB, whatever the file size); promote
//! is a server-side create-only copy followed by removal of the staging object; a content key is
//! never overwritten. The backend is generic over `object_store::ObjectStore`, so the same code
//! runs against an in-memory object store in the tests; the real S3 service is reached by
//! [`ObjectBackend::s3`].

use std::io;
use std::sync::Arc;

use futures_util::{StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{
    Error as OsError, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload,
};
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;

use crate::BlobError;
use crate::error::LimitKind;
use crate::key::{ContentKey, StagingKey, random_uuid_text};
use crate::stage::{Hasher, StageLimits, StagedBlob};
use crate::store::{
    AttachmentStore, BlobMeta, BlobRead, BodyReader, BoxFuture, ByteRange, Capabilities,
    DeleteReport, StoreKind,
};

/// The size of one multipart part: the store's memory per upload. Most S3-compatible services
/// need parts of at least 5 MiB except the last.
pub const PART_SIZE: usize = 8 * 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const MARKER: &str = ".store-id";

/// An S3-compatible bucket (or any `object_store` implementation) under a key prefix.
#[derive(Debug)]
pub struct ObjectBackend {
    inner: Arc<dyn ObjectStore>,
    prefix: String,
    fingerprint: String,
    part_size: usize,
}

/// What an `s3` store needs, apart from credentials (which only the environment or an instance
/// role supplies, never configuration).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct S3Settings {
    /// The bucket.
    pub bucket: String,
    /// The region (any text for services that ignore it).
    pub region: String,
    /// An endpoint URL for S3-compatible services (MinIO, Ceph, R2, Garage); AWS when `None`.
    pub endpoint: Option<String>,
    /// Keys live under this prefix (no leading or trailing slash); empty is the bucket root.
    pub prefix: String,
    /// Path-style requests (`https://host/bucket/key`), which most self-hosted services need.
    pub path_style: bool,
}

fn map_err(what: &str, key: &str, error: OsError) -> BlobError {
    match error {
        OsError::NotFound { .. } => BlobError::NotFound {
            key: key.to_owned(),
        },
        OsError::PermissionDenied { .. } | OsError::Unauthenticated { .. } => {
            BlobError::PermissionDenied {
                message: format!("{what} on {key}"),
                // the library's text can carry the request URL: it is not kept
                source: None,
            }
        }
        other => BlobError::Unavailable {
            retryable: true,
            message: format!("{what} on {key} failed ({})", variant_name(&other)),
            source: None,
        },
    }
}

fn variant_name(error: &OsError) -> &'static str {
    match error {
        OsError::Generic { .. } => "request error",
        OsError::AlreadyExists { .. } => "already exists",
        OsError::Precondition { .. } => "precondition failed",
        OsError::NotModified { .. } => "not modified",
        OsError::NotSupported { .. } => "not supported",
        _ => "store error",
    }
}

impl ObjectBackend {
    /// Open over `inner`: the objects live under `prefix`, and a `.store-id` marker object
    /// identifies the bucket and prefix (created if absent). Used with the real service through
    /// [`Self::s3`] and with `object_store`'s in-memory store in tests.
    ///
    /// # Errors
    ///
    /// `Configuration` for a bad prefix; `Unavailable` or `PermissionDenied` when the marker
    /// cannot be read or created.
    pub async fn open(inner: Arc<dyn ObjectStore>, prefix: &str) -> Result<Self, BlobError> {
        let ok = prefix.is_empty()
            || (!prefix.starts_with('/')
                && !prefix.ends_with('/')
                && !prefix
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                && prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.')));
        if !ok {
            return Err(BlobError::config(
                "an s3 prefix has no leading or trailing slash and only letters, digits, '-', '_', '.' and '/'",
            ));
        }
        let mut backend = Self {
            inner,
            prefix: prefix.to_owned(),
            fingerprint: String::new(),
            part_size: PART_SIZE,
        };
        backend.fingerprint = backend.load_or_create_marker().await?;
        Ok(backend)
    }

    /// Open the S3-compatible bucket described by `settings`, with credentials from the
    /// environment or the instance role (`AWS_*` variables); never from configuration. The
    /// copy-if-not-exists mechanism is the multipart one AWS S3 supports.
    ///
    /// # Errors
    ///
    /// `Configuration` for an endpoint that is not HTTPS (plain HTTP only for a loopback
    /// address) or malformed; the errors of [`Self::open`].
    pub async fn s3(settings: &S3Settings) -> Result<Self, BlobError> {
        use object_store::aws::{AmazonS3Builder, S3CopyIfNotExists};
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(&settings.bucket)
            .with_region(&settings.region)
            .with_virtual_hosted_style_request(!settings.path_style)
            .with_copy_if_not_exists(S3CopyIfNotExists::Multipart);
        if let Some(endpoint) = &settings.endpoint {
            let lower = endpoint.to_ascii_lowercase();
            let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
                .iter()
                .any(|p| lower.starts_with(p));
            if !(lower.starts_with("https://") || loopback) {
                return Err(BlobError::config(
                    "an s3 endpoint must be https; plain http is allowed only for a loopback address",
                ));
            }
            builder = builder.with_endpoint(endpoint).with_allow_http(loopback);
        }
        let store = builder.build().map_err(|e| {
            BlobError::config(format!(
                "the s3 store could not be built: {}",
                variant_name(&e)
            ))
        })?;
        Self::open(Arc::new(store), &settings.prefix).await
    }

    /// A copy with another part size (tests exercise the multipart path with small parts).
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_part_size_for_tests(mut self, bytes: usize) -> Self {
        self.part_size = bytes.max(1);
        self
    }

    fn path(&self, relative: &str) -> Path {
        if self.prefix.is_empty() {
            Path::from(relative)
        } else {
            Path::from(format!("{}/{relative}", self.prefix))
        }
    }

    fn relative(&self, full: &str) -> String {
        full.strip_prefix(&format!("{}/", self.prefix))
            .unwrap_or(full)
            .to_owned()
    }

    async fn load_or_create_marker(&self) -> Result<String, BlobError> {
        let path = self.path(MARKER);
        for _ in 0..3 {
            match self.inner.get(&path).await {
                Ok(found) => {
                    let bytes = found
                        .bytes()
                        .await
                        .map_err(|e| map_err("read the store id", MARKER, e))?;
                    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
                    return crate::key::Namespace::parse(&text)
                        .map(|_| text)
                        .map_err(|_| BlobError::config("the bucket has a damaged .store-id"));
                }
                Err(OsError::NotFound { .. }) => {
                    let nonce = random_uuid_text()?;
                    let options = PutOptions::from(PutMode::Create);
                    match self
                        .inner
                        .put_opts(&path, PutPayload::from(nonce.clone().into_bytes()), options)
                        .await
                    {
                        Ok(_) => return Ok(nonce),
                        // another node created it first: read it
                        Err(OsError::AlreadyExists { .. } | OsError::Precondition { .. }) => {}
                        Err(e) => return Err(map_err("create the store id", MARKER, e)),
                    }
                }
                Err(e) => return Err(map_err("read the store id", MARKER, e)),
            }
        }
        Err(BlobError::Unavailable {
            retryable: true,
            message: "the store id could not be created or read".to_owned(),
            source: None,
        })
    }
}

async fn size_check(
    inner: &dyn ObjectStore,
    path: &Path,
    relative: &str,
    expected: u64,
) -> Result<(), BlobError> {
    let meta = inner
        .head(path)
        .await
        .map_err(|e| map_err("read object metadata", relative, e))?;
    if meta.size == expected {
        Ok(())
    } else {
        Err(BlobError::Integrity {
            key: relative.to_owned(),
            expected: format!("{expected} bytes"),
            actual: format!("{} bytes", meta.size),
        })
    }
}

impl AttachmentStore for ObjectBackend {
    fn kind(&self) -> StoreKind {
        StoreKind::S3
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_reads: true,
            presign: false,
            create_only: true,
            if_match_put: true,
        }
    }

    fn fingerprint(&self) -> Option<String> {
        Some(self.fingerprint.clone())
    }

    fn verify_fingerprint<'a>(&'a self, expected: &'a str) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async move {
            let found = self.load_or_create_marker().await?;
            if found == expected {
                Ok(())
            } else {
                Err(BlobError::Unavailable {
                    retryable: false,
                    message: "the bucket prefix is not the store this database registered (its \
                              .store-id differs)"
                        .to_owned(),
                    source: None,
                })
            }
        })
    }

    fn stage<'a>(
        &'a self,
        key: &'a StagingKey,
        body: BodyReader<'a>,
        limits: StageLimits,
    ) -> BoxFuture<'a, Result<StagedBlob, BlobError>> {
        Box::pin(async move {
            let relative = key.path();
            let path = self.path(&relative);
            let mut hasher = Hasher::new();
            let mut part: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; READ_CHUNK];
            let mut upload: Option<Box<dyn object_store::MultipartUpload>> = None;
            let outcome: Result<(), BlobError> = async {
                loop {
                    let read = body
                        .read(&mut chunk)
                        .await
                        .map_err(|e| BlobError::from_io("read the upload body", &relative, e))?;
                    if read == 0 {
                        return Ok(());
                    }
                    let slice = chunk.get(..read).unwrap_or(&[]);
                    hasher.update(slice);
                    if hasher.size() > limits.max_bytes {
                        return Err(BlobError::LimitExceeded {
                            kind: LimitKind::ObjectBytes,
                            limit: limits.max_bytes,
                        });
                    }
                    part.extend_from_slice(slice);
                    if part.len() >= self.part_size {
                        if upload.is_none() {
                            upload = Some(
                                self.inner
                                    .put_multipart(&path)
                                    .await
                                    .map_err(|e| map_err("start an upload", &relative, e))?,
                            );
                        }
                        if let Some(active) = upload.as_mut() {
                            let payload = PutPayload::from(std::mem::take(&mut part));
                            active
                                .put_part(payload)
                                .await
                                .map_err(|e| map_err("upload a part", &relative, e))?;
                        }
                    }
                }
            }
            .await;
            if let Err(error) = outcome {
                if let Some(mut active) = upload {
                    let _ = active.abort().await;
                }
                return Err(error);
            }
            let finished = match upload {
                None => self
                    .inner
                    .put_opts(
                        &path,
                        PutPayload::from(part),
                        PutOptions::from(PutMode::Overwrite),
                    )
                    .await
                    .map(|_| ())
                    .map_err(|e| map_err("write a staging object", &relative, e)),
                Some(mut active) => {
                    let last = if part.is_empty() {
                        Ok(())
                    } else {
                        active
                            .put_part(PutPayload::from(part))
                            .await
                            .map_err(|e| map_err("upload a part", &relative, e))
                    };
                    match last {
                        Ok(()) => active
                            .complete()
                            .await
                            .map(|_| ())
                            .map_err(|e| map_err("complete an upload", &relative, e)),
                        Err(e) => {
                            let _ = active.abort().await;
                            Err(e)
                        }
                    }
                }
            };
            finished?;
            Ok(hasher.finish(key.clone()))
        })
    }

    fn promote<'a>(
        &'a self,
        staged: &'a StagedBlob,
        key: &'a ContentKey,
    ) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async move {
            if staged.key().namespace() != key.namespace() {
                return Err(BlobError::ForeignNamespace {
                    namespace: key.namespace().to_string(),
                });
            }
            let source_rel = staged.key().path();
            let source = self.path(&source_rel);
            let target_rel = key.path();
            let target = self.path(&target_rel);
            match self.inner.copy_if_not_exists(&source, &target).await {
                Ok(()) => {}
                Err(OsError::AlreadyExists { .. } | OsError::Precondition { .. }) => {
                    size_check(self.inner.as_ref(), &target, &target_rel, staged.size()).await?;
                }
                // the staging object is gone: a retry after a successful first promote
                Err(OsError::NotFound { .. }) => {
                    return match size_check(
                        self.inner.as_ref(),
                        &target,
                        &target_rel,
                        staged.size(),
                    )
                    .await
                    {
                        Err(BlobError::NotFound { .. }) => {
                            Err(BlobError::NotFound { key: source_rel })
                        }
                        other => other,
                    };
                }
                Err(e) => return Err(map_err("promote an object", &target_rel, e)),
            }
            match self.inner.delete(&source).await {
                Ok(()) | Err(OsError::NotFound { .. }) => Ok(()),
                Err(e) => Err(map_err("remove a staging object", &source_rel, e)),
            }
        })
    }

    fn get<'a>(
        &'a self,
        key: &'a ContentKey,
        range: Option<ByteRange>,
    ) -> BoxFuture<'a, Result<BlobRead, BlobError>> {
        Box::pin(async move {
            let relative = key.path();
            let path = self.path(&relative);
            let total = self
                .inner
                .head(&path)
                .await
                .map_err(|e| map_err("read object metadata", &relative, e))?
                .size;
            let mut options = GetOptions::new();
            if let Some(range) = range {
                let end = range.start.checked_add(range.len);
                match end {
                    Some(end) if range.len > 0 && end <= total => {
                        options = options.with_range(Some(range.start..end));
                    }
                    _ => return Err(BlobError::InvalidRange { total }),
                }
            }
            let result = self
                .inner
                .get_opts(&path, options)
                .await
                .map_err(|e| map_err("open an object", &relative, e))?;
            let stream = result
                .into_stream()
                .map_err(|e| io::Error::other(variant_name(&e)));
            Ok(BlobRead {
                total_size: total,
                reader: Box::pin(StreamReader::new(stream)),
            })
        })
    }

    fn head<'a>(&'a self, key: &'a ContentKey) -> BoxFuture<'a, Result<BlobMeta, BlobError>> {
        Box::pin(async move {
            let relative = key.path();
            let meta = self
                .inner
                .head(&self.path(&relative))
                .await
                .map_err(|e| map_err("read object metadata", &relative, e))?;
            Ok(BlobMeta { size: meta.size })
        })
    }

    fn delete_many<'a>(
        &'a self,
        keys: &'a [ContentKey],
    ) -> BoxFuture<'a, Result<DeleteReport, BlobError>> {
        Box::pin(async move {
            let mut report = DeleteReport::default();
            let paths: Vec<Result<Path, OsError>> =
                keys.iter().map(|k| Ok(self.path(&k.path()))).collect();
            let mut results = self
                .inner
                .delete_stream(futures_util::stream::iter(paths).boxed());
            while let Some(result) = results.next().await {
                match result {
                    Ok(_) => report.deleted += 1,
                    Err(OsError::NotFound { .. }) => report.already_absent += 1,
                    Err(e) => return Err(map_err("remove objects", "<batch>", e)),
                }
            }
            Ok(report)
        })
    }

    fn delete_staged<'a>(&'a self, key: &'a StagingKey) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async move {
            let relative = key.path();
            match self.inner.delete(&self.path(&relative)).await {
                Ok(()) | Err(OsError::NotFound { .. }) => Ok(()),
                Err(e) => Err(map_err("remove a staging object", &relative, e)),
            }
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, BlobError>> {
        Box::pin(async move {
            let clean = !prefix.is_empty()
                && prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
                && !prefix.split('/').any(|part| part == ".." || part == ".");
            if !clean {
                return Err(BlobError::LimitExceeded {
                    kind: LimitKind::NameBytes,
                    limit: 0,
                });
            }
            let start = self.path(prefix);
            let mut stream = match after {
                Some(after) => self.inner.list_with_offset(Some(&start), &self.path(after)),
                None => self.inner.list(Some(&start)),
            };
            let mut found: Vec<String> = Vec::new();
            while let Some(meta) = stream.next().await {
                let meta = meta.map_err(|e| map_err("list objects", prefix, e))?;
                found.push(self.relative(meta.location.as_ref()));
            }
            found.sort();
            if let Some(after) = after {
                found.retain(|name| name.as_str() > after);
            }
            found.truncate(limit);
            Ok(found)
        })
    }
}
