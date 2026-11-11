//! The `local` store: a directory.
//!
//! Directories are `0700` and files `0440`. A staged file is written to a `.part` name, synced
//! and renamed; promotion hard-links it to the content path (which fails if the key exists, so a
//! key is never overwritten) and then unlinks the staging name. The directory carries a random
//! `.store-id`; the database's store row holds the same value, so a node that mounts another
//! directory is refused instead of treating every blob as missing.

use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};

use tokio::fs::{self, DirBuilder, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::BlobError;
use crate::error::LimitKind;
use crate::key::{ContentKey, StagingKey, random_uuid_text};
use crate::stage::{Hasher, StageLimits, StagedBlob};
use crate::store::{
    AttachmentStore, BlobMeta, BlobRead, BodyReader, BoxFuture, ByteRange, Capabilities,
    DeleteReport, StoreKind,
};

const STORE_ID_FILE: &str = ".store-id";
const READ_CHUNK: usize = 64 * 1024;

/// A directory used as an attachment store.
#[derive(Debug)]
pub struct LocalStore {
    root: PathBuf,
    fingerprint: String,
}

async fn make_dirs(path: &Path) -> Result<(), BlobError> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(path)
        .await
        .map_err(|e| BlobError::from_io("create a directory", "<store>", e))
}

async fn create_new_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o440);
    let file = options.open(path).await?;
    // `mode` is masked by the process umask (a CI or service umask of 077 gives 0400): set the
    // documented 0440 explicitly on the file this call just created.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o440))
            .await?;
    }
    Ok(file)
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

impl LocalStore {
    /// Open the store at `root`, creating the directory (`0700`) and its `.store-id` when they
    /// do not exist.
    ///
    /// # Errors
    ///
    /// `Unavailable` or `PermissionDenied` when the directory cannot be created or read;
    /// `Configuration` when `.store-id` exists but is not an identifier.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, BlobError> {
        let root = root.into();
        make_dirs(&root).await?;
        let id_path = root.join(STORE_ID_FILE);
        let fingerprint = loop {
            match fs::read_to_string(&id_path).await {
                Ok(text) => break Self::check_id(text.trim())?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let nonce = random_uuid_text()?;
                    match create_new_file(&id_path).await {
                        Ok(mut file) => {
                            file.write_all(nonce.as_bytes()).await.map_err(|e| {
                                BlobError::from_io("write the store id", STORE_ID_FILE, e)
                            })?;
                            file.sync_all().await.map_err(|e| {
                                BlobError::from_io("sync the store id", STORE_ID_FILE, e)
                            })?;
                            break nonce;
                        }
                        // another process created it between our read and create: read again
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => {
                            return Err(BlobError::from_io(
                                "create the store id",
                                STORE_ID_FILE,
                                e,
                            ));
                        }
                    }
                }
                Err(error) => {
                    return Err(BlobError::from_io(
                        "read the store id",
                        STORE_ID_FILE,
                        error,
                    ));
                }
            }
        };
        Ok(Self { root, fingerprint })
    }

    fn check_id(text: &str) -> Result<String, BlobError> {
        crate::key::Namespace::parse(text)
            .map(|_| text.to_owned())
            .map_err(|_| BlobError::config("the store directory has a damaged .store-id"))
    }

    /// The directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn full(&self, relative: &str) -> PathBuf {
        // keys are built from validated parts, so this never leaves the root
        self.root.join(relative)
    }

    async fn ensure_parent(&self, path: &Path) -> Result<(), BlobError> {
        match path.parent() {
            Some(parent) => make_dirs(parent).await,
            None => Ok(()),
        }
    }
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, base, out)?;
        } else if let Ok(relative) = path.strip_prefix(base) {
            let parts: Vec<String> = relative
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            out.push(parts.join("/"));
        }
    }
    Ok(())
}

impl AttachmentStore for LocalStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Local
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_reads: true,
            presign: false,
            create_only: true,
            if_match_put: false,
        }
    }

    fn fingerprint(&self) -> Option<String> {
        Some(self.fingerprint.clone())
    }

    fn verify_fingerprint<'a>(&'a self, expected: &'a str) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async move {
            let found = fs::read_to_string(self.root.join(STORE_ID_FILE))
                .await
                .map_err(|e| BlobError::from_io("read the store id", STORE_ID_FILE, e))?;
            if found.trim() == expected {
                Ok(())
            } else {
                Err(BlobError::Unavailable {
                    retryable: false,
                    message: "the store directory is not the one this database registered \
                              (its .store-id differs)"
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
            let path = self.full(&relative);
            let part = part_path(&path);
            self.ensure_parent(&path).await?;
            let mut file = create_new_file(&part)
                .await
                .map_err(|e| BlobError::from_io("create a staging file", &relative, e))?;
            let outcome = copy_hashed(body, &mut file, limits, &relative).await;
            let hasher = match outcome {
                Ok(hasher) => hasher,
                Err(error) => {
                    drop(file);
                    let _ = fs::remove_file(&part).await;
                    return Err(error);
                }
            };
            if let Err(error) = file.sync_all().await {
                drop(file);
                let _ = fs::remove_file(&part).await;
                return Err(BlobError::from_io("sync a staging file", &relative, error));
            }
            drop(file);
            if let Err(error) = fs::rename(&part, &path).await {
                let _ = fs::remove_file(&part).await;
                return Err(BlobError::from_io("name a staging file", &relative, error));
            }
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
            let source = self.full(&source_rel);
            let target_rel = key.path();
            let target = self.full(&target_rel);
            self.ensure_parent(&target).await?;
            match fs::hard_link(&source, &target).await {
                Ok(()) => unlink_if_present(&source, &source_rel).await,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    check_size(&target, &target_rel, staged.size()).await?;
                    unlink_if_present(&source, &source_rel).await
                }
                // the staging name is gone: a retry after a successful first promote
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    match check_size(&target, &target_rel, staged.size()).await {
                        Err(BlobError::NotFound { .. }) => {
                            Err(BlobError::NotFound { key: source_rel })
                        }
                        other => other,
                    }
                }
                Err(e) => Err(BlobError::from_io("promote an object", &target_rel, e)),
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
            let mut file = fs::File::open(self.full(&relative))
                .await
                .map_err(|e| BlobError::from_io("open an object", &relative, e))?;
            let total = file
                .metadata()
                .await
                .map_err(|e| BlobError::from_io("read object metadata", &relative, e))?
                .len();
            match range {
                None => Ok(BlobRead {
                    total_size: total,
                    reader: Box::pin(file),
                }),
                Some(range) => {
                    let end = range.start.checked_add(range.len);
                    if range.len == 0 || end.is_none_or(|end| end > total) {
                        return Err(BlobError::InvalidRange { total });
                    }
                    file.seek(SeekFrom::Start(range.start))
                        .await
                        .map_err(|e| BlobError::from_io("seek in an object", &relative, e))?;
                    Ok(BlobRead {
                        total_size: total,
                        reader: Box::pin(file.take(range.len)),
                    })
                }
            }
        })
    }

    fn head<'a>(&'a self, key: &'a ContentKey) -> BoxFuture<'a, Result<BlobMeta, BlobError>> {
        Box::pin(async move {
            let relative = key.path();
            let meta = fs::metadata(self.full(&relative))
                .await
                .map_err(|e| BlobError::from_io("read object metadata", &relative, e))?;
            Ok(BlobMeta { size: meta.len() })
        })
    }

    fn delete_many<'a>(
        &'a self,
        keys: &'a [ContentKey],
    ) -> BoxFuture<'a, Result<DeleteReport, BlobError>> {
        Box::pin(async move {
            let mut report = DeleteReport::default();
            for key in keys {
                let relative = key.path();
                match fs::remove_file(self.full(&relative)).await {
                    Ok(()) => report.deleted += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        report.already_absent += 1;
                    }
                    Err(e) => return Err(BlobError::from_io("remove an object", &relative, e)),
                }
            }
            Ok(report)
        })
    }

    fn delete_staged<'a>(&'a self, key: &'a StagingKey) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async move {
            let relative = key.path();
            let path = self.full(&relative);
            unlink_if_present(&part_path(&path), &relative).await?;
            unlink_if_present(&path, &relative).await
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
            let root = self.root.clone();
            let start = root.join(prefix);
            let mut found = tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                walk(&start, &root, &mut out).map(|()| out)
            })
            .await
            .map_err(|e| BlobError::Unavailable {
                retryable: true,
                message: "the listing task failed".to_owned(),
                source: Some(Box::new(e)),
            })?
            .map_err(|e| BlobError::from_io("list objects", prefix, e))?;
            found.sort();
            if let Some(after) = after {
                found.retain(|name| name.as_str() > after);
            }
            found.truncate(limit);
            Ok(found)
        })
    }
}

async fn copy_hashed(
    body: BodyReader<'_>,
    file: &mut fs::File,
    limits: StageLimits,
    relative: &str,
) -> Result<Hasher, BlobError> {
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        let read = body
            .read(&mut buffer)
            .await
            .map_err(|e| BlobError::from_io("read the upload body", relative, e))?;
        if read == 0 {
            return Ok(hasher);
        }
        let chunk = buffer.get(..read).unwrap_or(&[]);
        hasher.update(chunk);
        if hasher.size() > limits.max_bytes {
            return Err(BlobError::LimitExceeded {
                kind: LimitKind::ObjectBytes,
                limit: limits.max_bytes,
            });
        }
        file.write_all(chunk)
            .await
            .map_err(|e| BlobError::from_io("write a staging file", relative, e))?;
    }
}

async fn unlink_if_present(path: &Path, relative: &str) -> Result<(), BlobError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(BlobError::from_io("remove a file", relative, e)),
    }
}

async fn check_size(path: &Path, relative: &str, expected: u64) -> Result<(), BlobError> {
    let meta = fs::metadata(path)
        .await
        .map_err(|e| BlobError::from_io("read object metadata", relative, e))?;
    if meta.len() == expected {
        Ok(())
    } else {
        Err(BlobError::Integrity {
            key: relative.to_owned(),
            expected: format!("{expected} bytes"),
            actual: format!("{} bytes", meta.len()),
        })
    }
}
