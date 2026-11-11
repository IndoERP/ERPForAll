//! The `memory` store: objects in process memory, with fault injection (tests only).
//!
//! It implements the same contract as the local store (create-only promote, `NotFound` for a
//! missing key, ranges, no empty content for an absent object) and adds [`Faults`]: a test makes
//! the next calls of an operation fail before or after their effect, which is how the garbage
//! collection race test places a crash after a promote, or after a `delete_many` and before the
//! database commit. Server configuration cannot build this kind.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::AsyncReadExt;

use crate::BlobError;
use crate::error::LimitKind;
use crate::key::{ContentKey, StagingKey};
use crate::stage::{Hasher, StageLimits, StagedBlob};
use crate::store::{
    AttachmentStore, BlobMeta, BlobRead, BodyReader, BoxFuture, ByteRange, Capabilities,
    DeleteReport, StoreKind,
};

/// A store operation a fault can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Op {
    /// `stage`.
    Stage,
    /// `promote`.
    Promote,
    /// `get`.
    Get,
    /// `head`.
    Head,
    /// `delete_many`.
    DeleteMany,
    /// `delete_staged`.
    DeleteStaged,
}

/// When a fault fires relative to the operation's effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum When {
    /// The call fails and has no effect.
    Before,
    /// The call has its effect, then reports a failure (a lost reply, or a crash after the effect).
    After,
}

#[derive(Debug, Clone, Copy)]
struct Fault {
    op: Op,
    when: When,
    skip: usize,
    remaining: usize,
}

/// Programmed failures of a [`MemoryStore`].
#[derive(Debug, Default)]
pub struct Faults {
    rules: Mutex<Vec<Fault>>,
}

impl Faults {
    /// Make the next `count` calls of `op` fail at `when`, after letting `skip` calls through.
    pub fn inject(&self, op: Op, when: When, skip: usize, count: usize) {
        self.rules
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Fault {
                op,
                when,
                skip,
                remaining: count,
            });
    }

    /// Remove every programmed failure.
    pub fn clear(&self) {
        self.rules
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn fire(&self, op: Op, when: When) -> bool {
        let mut rules = self.rules.lock().unwrap_or_else(PoisonError::into_inner);
        for rule in rules.iter_mut().filter(|r| r.op == op && r.when == when) {
            if rule.skip > 0 {
                rule.skip -= 1;
            } else if rule.remaining > 0 {
                rule.remaining -= 1;
                return true;
            }
        }
        false
    }
}

fn injected(op: Op, when: When) -> BlobError {
    BlobError::Unavailable {
        retryable: true,
        message: format!("injected fault in {op:?} ({when:?} its effect)"),
        source: None,
    }
}

#[derive(Debug, Default)]
struct Inner {
    content: BTreeMap<String, Arc<Vec<u8>>>,
    staging: BTreeMap<String, Arc<Vec<u8>>>,
}

/// An in-memory store.
#[derive(Debug)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
    faults: Faults,
    fingerprint: String,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            faults: Faults::default(),
            fingerprint: "memory".to_owned(),
        }
    }

    /// The programmed failures.
    pub fn faults(&self) -> &Faults {
        &self.faults
    }

    /// Every content key held, as paths.
    pub fn content_keys(&self) -> Vec<String> {
        self.lock().content.keys().cloned().collect()
    }

    /// Every staging key held, as paths.
    pub fn staging_keys(&self) -> Vec<String> {
        self.lock().staging.keys().cloned().collect()
    }

    /// The bytes under a content key, if any.
    pub fn content(&self, key: &ContentKey) -> Option<Vec<u8>> {
        self.lock().content.get(&key.path()).map(|b| b.to_vec())
    }

    /// Insert an object without staging (a test arranging a state, such as a stray key).
    pub fn put_content(&self, key: &ContentKey, bytes: Vec<u8>) {
        self.lock().content.insert(key.path(), Arc::new(bytes));
    }

    /// Replace the bytes under a key (a test simulating corruption).
    pub fn corrupt(&self, key: &ContentKey, bytes: Vec<u8>) {
        self.put_content(key, bytes);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl AttachmentStore for MemoryStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Memory
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
            if expected == self.fingerprint {
                Ok(())
            } else {
                Err(BlobError::Unavailable {
                    retryable: false,
                    message: "the store is not the one this database registered".to_owned(),
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
            if self.faults.fire(Op::Stage, When::Before) {
                return Err(injected(Op::Stage, When::Before));
            }
            let mut hasher = Hasher::new();
            let mut bytes = Vec::new();
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                let read = body
                    .read(&mut buffer)
                    .await
                    .map_err(|e| BlobError::from_io("read the upload body", &key.path(), e))?;
                if read == 0 {
                    break;
                }
                let chunk = buffer.get(..read).unwrap_or(&[]);
                hasher.update(chunk);
                if hasher.size() > limits.max_bytes {
                    return Err(BlobError::LimitExceeded {
                        kind: LimitKind::ObjectBytes,
                        limit: limits.max_bytes,
                    });
                }
                bytes.extend_from_slice(chunk);
            }
            self.lock().staging.insert(key.path(), Arc::new(bytes));
            if self.faults.fire(Op::Stage, When::After) {
                return Err(injected(Op::Stage, When::After));
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
            if self.faults.fire(Op::Promote, When::Before) {
                return Err(injected(Op::Promote, When::Before));
            }
            {
                let mut inner = self.lock();
                let source = staged.key().path();
                let target = key.path();
                let size = staged.size();
                let existing = inner.content.get(&target).map(|b| b.len());
                match existing {
                    Some(len) => {
                        if len as u64 != size {
                            return Err(BlobError::Integrity {
                                key: target,
                                expected: format!("{size} bytes"),
                                actual: format!("{len} bytes"),
                            });
                        }
                        inner.staging.remove(&source);
                    }
                    None => match inner.staging.remove(&source) {
                        Some(bytes) => {
                            inner.content.insert(target, bytes);
                        }
                        None => return Err(BlobError::NotFound { key: source }),
                    },
                }
            }
            if self.faults.fire(Op::Promote, When::After) {
                return Err(injected(Op::Promote, When::After));
            }
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        key: &'a ContentKey,
        range: Option<ByteRange>,
    ) -> BoxFuture<'a, Result<BlobRead, BlobError>> {
        Box::pin(async move {
            if self.faults.fire(Op::Get, When::Before) {
                return Err(injected(Op::Get, When::Before));
            }
            let bytes = self
                .lock()
                .content
                .get(&key.path())
                .cloned()
                .ok_or_else(|| BlobError::NotFound { key: key.path() })?;
            let total = bytes.len() as u64;
            let slice: Vec<u8> = match range {
                None => bytes.to_vec(),
                Some(range) => {
                    let end = range.start.checked_add(range.len);
                    match end {
                        Some(end) if range.len > 0 && end <= total => {
                            let start = usize::try_from(range.start)
                                .map_err(|_| BlobError::InvalidRange { total })?;
                            let end = usize::try_from(end)
                                .map_err(|_| BlobError::InvalidRange { total })?;
                            bytes.get(start..end).unwrap_or(&[]).to_vec()
                        }
                        _ => return Err(BlobError::InvalidRange { total }),
                    }
                }
            };
            Ok(BlobRead {
                total_size: total,
                reader: Box::pin(Cursor::new(slice)),
            })
        })
    }

    fn head<'a>(&'a self, key: &'a ContentKey) -> BoxFuture<'a, Result<BlobMeta, BlobError>> {
        Box::pin(async move {
            if self.faults.fire(Op::Head, When::Before) {
                return Err(injected(Op::Head, When::Before));
            }
            self.lock()
                .content
                .get(&key.path())
                .map(|b| BlobMeta {
                    size: b.len() as u64,
                })
                .ok_or_else(|| BlobError::NotFound { key: key.path() })
        })
    }

    fn delete_many<'a>(
        &'a self,
        keys: &'a [ContentKey],
    ) -> BoxFuture<'a, Result<DeleteReport, BlobError>> {
        Box::pin(async move {
            if self.faults.fire(Op::DeleteMany, When::Before) {
                return Err(injected(Op::DeleteMany, When::Before));
            }
            let mut report = DeleteReport::default();
            {
                let mut inner = self.lock();
                for key in keys {
                    if inner.content.remove(&key.path()).is_some() {
                        report.deleted += 1;
                    } else {
                        report.already_absent += 1;
                    }
                }
            }
            if self.faults.fire(Op::DeleteMany, When::After) {
                return Err(injected(Op::DeleteMany, When::After));
            }
            Ok(report)
        })
    }

    fn delete_staged<'a>(&'a self, key: &'a StagingKey) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async move {
            if self.faults.fire(Op::DeleteStaged, When::Before) {
                return Err(injected(Op::DeleteStaged, When::Before));
            }
            self.lock().staging.remove(&key.path());
            if self.faults.fire(Op::DeleteStaged, When::After) {
                return Err(injected(Op::DeleteStaged, When::After));
            }
            Ok(())
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, BlobError>> {
        Box::pin(async move {
            let inner = self.lock();
            let mut found: Vec<String> = inner
                .content
                .keys()
                .chain(inner.staging.keys())
                .filter(|k| k.starts_with(prefix))
                .filter(|k| after.is_none_or(|a| k.as_str() > a))
                .cloned()
                .collect();
            found.sort();
            found.truncate(limit);
            Ok(found)
        })
    }
}
