// SPDX-License-Identifier: MIT OR Apache-2.0
//! Local implementation of the existing ownership-marker port. The stable guard closes the
//! pause between the last lease check and publication; leases alone cannot close that race.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{self, AtFlags, FlockOperation, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use crate::owner::{finish_probe, invalid};
use crate::{
    BlobError, ContestState, Namespace, OWNER_MARKER_MAX_BYTES, OwnerMarker, OwnerState,
    OwnerVersion,
};

const LOCK: &str = ".owner.lock";
const GUARD: &str = ".owner.guard";
const LEASE_SECONDS: u64 = 30;
const LEASE_MAGIC: &[u8] = b"ERPLCK01";
const ENVELOPE_MAGIC: &[u8] = b"ERPLOC01";
const OWNER: &str = "_owner";
const CONTESTED: &str = "_contested";

fn busy(message: &'static str) -> BlobError {
    BlobError::Unavailable {
        retryable: true,
        message: message.to_owned(),
        source: None,
    }
}
fn io(what: &str, error: impl Into<std::io::Error>) -> BlobError {
    BlobError::from_io(what, "<local-owner>", error.into())
}
fn primitive(error: Errno) -> BlobError {
    if matches!(error, Errno::NOSYS | Errno::NOTSUP | Errno::INVAL) {
        BlobError::OwnerCapabilityMissing {
            capability: "local no-replace rename or advisory lock",
        }
    } else {
        io("perform an atomic owner operation", error)
    }
}

#[derive(Debug)]
pub(super) struct Directory(pub(super) File);
impl Directory {
    pub(super) fn open(root: &Path, ns: &Namespace) -> Result<Self, BlobError> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let root = File::from(
            fs::open(root, flags, Mode::empty()).map_err(|e| io("open the store directory", e))?,
        );
        match fs::mkdirat(&root, ns.as_str(), Mode::from_raw_mode(0o700)) {
            Ok(()) => root
                .sync_all()
                .map_err(|e| io("sync the store directory", e))?,
            Err(Errno::EXIST) => {}
            Err(error) => return Err(io("create the owner namespace", error)),
        }
        let fd = fs::openat(&root, ns.as_str(), flags, Mode::empty()).map_err(|error| {
            if matches!(error, Errno::LOOP | Errno::NOTDIR) {
                invalid("local namespace must be a directory without symlinks")
            } else {
                io("open the owner namespace", error)
            }
        })?;
        Ok(Self(File::from(fd)))
    }
    pub(super) fn file(&self, name: &str, flags: OFlags, mode: fs::RawMode) -> Result<File, Errno> {
        let fd = fs::openat(
            &self.0,
            name,
            flags | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::from_raw_mode(mode),
        )?;
        Ok(File::from(fd))
    }
    pub(super) fn read(&self, name: &str) -> Result<Option<Vec<u8>>, BlobError> {
        let file = match self.file(name, OFlags::RDONLY, 0) {
            Ok(file) => file,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP) => {
                return Err(invalid(
                    "local marker must be a regular file without symlinks",
                ));
            }
            Err(error) => return Err(io("open a local owner record", error)),
        };
        let meta = file
            .metadata()
            .map_err(|e| io("read local owner metadata", e))?;
        if !meta.is_file() {
            return Err(invalid(
                "local marker must be a regular file without symlinks",
            ));
        }
        // Read at most bound+1, including a concurrent in-place corruption of a trusted directory.
        let mut bytes = Vec::new();
        file.take((OWNER_MARKER_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| io("read a local owner record", e))?;
        Ok(Some(bytes))
    }
    pub(super) fn create(&self, name: &str, bytes: &[u8]) -> Result<(), BlobError> {
        let mut file = self
            .file(name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, 0o440)
            .map_err(|e| io("create a local owner record", e))?;
        file.write_all(bytes)
            .map_err(|e| io("write a local owner record", e))?;
        file.sync_all()
            .map_err(|e| io("sync a local owner record", e))?;
        self.sync()
    }
    pub(super) fn sync(&self) -> Result<(), BlobError> {
        self.0
            .sync_all()
            .map_err(|e| io("sync the owner namespace", e))
    }
    pub(super) fn unlink(&self, name: &str) -> Result<(), BlobError> {
        match fs::unlinkat(&self.0, name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()), // Removal is idempotent only for this operation's own artifact.
            Err(error) => Err(io("remove a local owner artifact", error)),
        }
    }
    pub(super) fn rename_no_replace(&self, from: &str, to: &str) -> Result<(), Errno> {
        fs::renameat_with(&self.0, from, &self.0, to, RenameFlags::NOREPLACE)
    }
    pub(super) fn try_guard(&self) -> Result<Option<Guard>, BlobError> {
        let file = self
            .file(GUARD, OFlags::RDWR | OFlags::CREATE, 0o600)
            .map_err(|e| io("open the local owner guard", e))?;
        if !file
            .metadata()
            .map_err(|e| io("read the owner guard metadata", e))?
            .is_file()
        {
            return Err(invalid("local owner guard must be a regular file"));
        }
        match fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Guard(file))),
            Err(Errno::WOULDBLOCK) => Ok(None),
            Err(error) => Err(primitive(error)),
        }
    }
    fn guard(&self) -> Result<Guard, BlobError> {
        self.try_guard()?
            .ok_or_else(|| busy("local owner guard is busy; retry"))
    }
}

// Closing the descriptor releases the kernel lock even on process death. Never unlink the guard.
#[derive(Debug)]
pub(super) struct Guard(File);
impl Guard {
    pub(super) fn unlock(self) -> Result<(), BlobError> {
        fs::flock(&self.0, FlockOperation::Unlock).map_err(primitive)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Lease {
    nonce: Namespace,
    operation: Namespace,
    expires: u64,
}
impl Lease {
    fn new(marker: &OwnerMarker, now: u64) -> Result<Self, BlobError> {
        Ok(Self {
            nonce: Namespace::random()?,
            operation: marker.operation().clone(),
            expires: now
                .checked_add(LEASE_SECONDS)
                .ok_or_else(|| invalid("local owner lease expiry overflow"))?,
        })
    }
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(88);
        bytes.extend_from_slice(LEASE_MAGIC);
        bytes.extend_from_slice(self.nonce.as_str().as_bytes());
        bytes.extend_from_slice(self.operation.as_str().as_bytes());
        bytes.extend_from_slice(&self.expires.to_le_bytes());
        bytes
    }
    fn decode(bytes: &[u8]) -> Result<Self, BlobError> {
        let bad = || invalid("expected bounded local owner lease");
        if bytes.len() != 88 || bytes.get(..8) != Some(LEASE_MAGIC) {
            return Err(bad());
        }
        let parse = |start, end| {
            Namespace::parse(
                std::str::from_utf8(bytes.get(start..end).ok_or_else(bad)?).map_err(|_| bad())?,
            )
            .map_err(|_| bad())
        };
        let expires = u64::from_le_bytes(
            bytes
                .get(80..88)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?,
        );
        if expires == 0 {
            return Err(bad());
        }
        Ok(Self {
            nonce: parse(8, 44)?,
            operation: parse(44, 80)?,
            expires,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AfterLockCreate,
    AfterLeaseSync,
    AfterAcquire,
    AfterMove,
    BeforeTempSync,
    BeforePublish,
    AfterValidation,
    AfterPublish,
    BeforeDirectorySync,
    BeforeRelease,
}
trait Hooks {
    fn now(&self) -> Result<u64, BlobError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| invalid("local owner clock precedes Unix epoch"))
    }
    fn at(&self, _phase: Phase, _dir: &Directory) -> Result<(), BlobError> {
        Ok(())
    }
}
struct System;
impl Hooks for System {}

fn read_lease(dir: &Directory) -> Result<Option<Lease>, BlobError> {
    dir.read(LOCK)?
        .map(|bytes| Lease::decode(&bytes))
        .transpose()
}
fn restore(dir: &Directory, moved: &str) -> Result<(), BlobError> {
    dir.rename_no_replace(moved, LOCK)
        .map_err(|error| BlobError::Unavailable {
            retryable: true,
            message: "local owner lease restoration failed; recovery file retained".to_owned(),
            source: Some(Box::new(error)),
        })?;
    dir.sync()
}
fn break_expired(dir: &Directory, expected: &Lease, hooks: &impl Hooks) -> Result<bool, BlobError> {
    let moved = format!(".owner.recovery-{}", Namespace::random()?);
    match dir.rename_no_replace(LOCK, &moved) {
        Ok(()) => {}
        Err(Errno::NOENT) => return Ok(false),
        Err(error) => return Err(primitive(error)),
    }
    let judged = (|| {
        hooks.at(Phase::AfterMove, dir)?;
        let bytes = dir
            .read(&moved)?
            .ok_or_else(|| busy("moved local owner lease disappeared; retry"))?;
        let actual = Lease::decode(&bytes)?;
        Ok(actual == *expected && actual.expires <= hooks.now()?)
    })();
    match judged {
        Ok(true) => {
            dir.unlink(&moved)?;
            dir.sync()?;
            Ok(true)
        }
        Ok(false) => {
            restore(dir, &moved)?;
            Ok(false)
        }
        Err(error) => finish(Err(error), restore(dir, &moved)),
    }
}
fn acquire(dir: &Directory, marker: &OwnerMarker, hooks: &impl Hooks) -> Result<Lease, BlobError> {
    let guard = dir.guard()?;
    let lease = Lease::new(marker, hooks.now()?)?;
    if let Some(existing) = read_lease(dir)?
        && (existing.expires > hooks.now()? || !break_expired(dir, &existing, hooks)?)
    {
        return Err(busy("local owner lease is busy; retry"));
    }
    let mut file = match dir.file(LOCK, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, 0o440) {
        Ok(file) => file,
        Err(Errno::EXIST) => return Err(busy("local owner lease is busy; retry")),
        Err(error) => return Err(io("create the local owner lease", error)),
    };
    hooks.at(Phase::AfterLockCreate, dir)?;
    file.write_all(&lease.encode())
        .map_err(|e| io("write the local owner lease", e))?;
    file.sync_all()
        .map_err(|e| io("sync the local owner lease", e))?;
    dir.sync()?;
    hooks.at(Phase::AfterLeaseSync, dir)?;
    guard.unlock()?;
    Ok(lease)
}
fn validate(dir: &Directory, lease: &Lease, hooks: &impl Hooks) -> Result<(), BlobError> {
    if read_lease(dir)?.as_ref() != Some(lease) || lease.expires <= hooks.now()? {
        return Err(busy("local owner lease was lost; retry"));
    }
    Ok(())
}
fn release_locked(dir: &Directory, lease: &Lease, hooks: &impl Hooks) -> Result<(), BlobError> {
    if read_lease(dir)?.as_ref() != Some(lease) {
        return Err(busy("local owner lease was lost; retry"));
    }
    hooks.at(Phase::BeforeRelease, dir)?;
    dir.unlink(LOCK)?;
    dir.sync()
}
fn release(dir: &Directory, lease: &Lease, hooks: &impl Hooks) -> Result<(), BlobError> {
    let guard = dir.guard()?;
    let result = release_locked(dir, lease, hooks);
    finish(result, guard.unlock())
}

#[derive(Debug, thiserror::Error)]
#[error("local owner operation and cleanup failed: {operation}; cleanup: {cleanup}")]
struct CleanupFailure {
    operation: BlobError,
    #[source]
    cleanup: BlobError,
}
fn finish<T>(
    operation: Result<T, BlobError>,
    cleanup: Result<(), BlobError>,
) -> Result<T, BlobError> {
    match (operation, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error),
        (Err(operation), Err(cleanup)) => Err(BlobError::Unavailable {
            retryable: true,
            message: "local owner operation and cleanup failed".to_owned(),
            source: Some(Box::new(CleanupFailure { operation, cleanup })),
        }),
    }
}
fn encode(marker: &OwnerMarker) -> Result<Vec<u8>, BlobError> {
    let nonce = Namespace::random()?;
    let mut bytes = Vec::with_capacity(152);
    bytes.extend_from_slice(ENVELOPE_MAGIC);
    bytes.extend_from_slice(nonce.as_str().as_bytes());
    bytes.extend_from_slice(&marker.encode());
    Ok(bytes)
}
fn decode(bytes: &[u8], ns: &Namespace) -> Result<OwnerState, BlobError> {
    let bad = || invalid("expected bounded local owner envelope");
    if bytes.len() != 152 || bytes.get(..8) != Some(ENVELOPE_MAGIC) {
        return Err(bad());
    }
    let nonce = Namespace::parse(
        std::str::from_utf8(bytes.get(8..44).ok_or_else(bad)?).map_err(|_| bad())?,
    )
    .map_err(|_| bad())?;
    let marker = OwnerMarker::decode(bytes.get(44..).ok_or_else(bad)?)?;
    let version = OwnerVersion::new(&format!("{ns}:{nonce}"), None)?;
    Ok(OwnerState::new(marker, version))
}
fn current(dir: &Directory, ns: &Namespace) -> Result<Option<OwnerState>, BlobError> {
    dir.read(OWNER)?.map(|bytes| decode(&bytes, ns)).transpose()
}
fn put(
    dir: &Directory,
    ns: &Namespace,
    marker: &OwnerMarker,
    expected: Option<&OwnerVersion>,
    hooks: &impl Hooks,
) -> Result<OwnerVersion, BlobError> {
    let lease = acquire(dir, marker, hooks)?;
    let temp = format!(".owner.temp-{}", lease.nonce);
    let work = (|| {
        hooks.at(Phase::AfterAcquire, dir)?;
        let bytes = encode(marker)?;
        let mut file = dir
            .file(&temp, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, 0o440)
            .map_err(|e| io("create the owner temporary file", e))?;
        file.write_all(&bytes)
            .map_err(|e| io("write the owner temporary file", e))?;
        hooks.at(Phase::BeforeTempSync, dir)?;
        file.sync_all()
            .map_err(|e| io("sync the owner temporary file", e))?;
        let guard = dir.guard()?;
        let publication = (|| {
            validate(dir, &lease, hooks)?;
            let found = current(dir, ns)?;
            if found.as_ref().map(OwnerState::version) != expected {
                return Err(BlobError::OwnerConflict);
            }
            hooks.at(Phase::AfterValidation, dir)?;
            hooks.at(Phase::BeforePublish, dir)?;
            if expected.is_none() {
                dir.rename_no_replace(&temp, OWNER).map_err(|e| {
                    if e == Errno::EXIST {
                        BlobError::OwnerConflict
                    } else {
                        primitive(e)
                    }
                })?;
            } else {
                fs::renameat(&dir.0, &temp, &dir.0, OWNER).map_err(primitive)?;
            }
            hooks.at(Phase::AfterPublish, dir)?;
            hooks.at(Phase::BeforeDirectorySync, dir)?;
            dir.sync()?;
            Ok(decode(&bytes, ns)?.version().clone())
        })();
        let released = match read_lease(dir) {
            Ok(Some(found)) if found == lease => release_locked(dir, &lease, hooks),
            // A reclaimed lease is the successor's responsibility, not a cleanup failure.
            Ok(Some(_)) | Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        finish(finish(publication, released), guard.unlock())
    })();
    // A resumed stale holder can remove its unique temporary file, never another holder's lock.
    let cleanup = dir.unlink(&temp).and_then(|()| dir.sync());
    let work = finish(work, cleanup);
    // Early failure before the guarded publication still owns a lease, unless it was reclaimed.
    // A missing or changed lease is preserved; it is not this call's cleanup responsibility.
    match read_lease(dir) {
        Ok(Some(found)) if found == lease => finish(work, release(dir, &lease, &System)),
        Ok(Some(_)) | Ok(None) => work,
        Err(error) => finish(work, Err(error)),
    }
}

pub(super) async fn owner(root: &Path, ns: &Namespace) -> Result<Option<OwnerState>, BlobError> {
    let root = root.to_owned();
    let ns = ns.clone();
    run(move || current(&Directory::open(&root, &ns)?, &ns)).await
}
pub(super) async fn put_owner(
    root: &Path,
    ns: &Namespace,
    marker: &OwnerMarker,
    expected: Option<&OwnerVersion>,
) -> Result<OwnerVersion, BlobError> {
    let root = root.to_owned();
    let ns = ns.clone();
    let marker = marker.clone();
    let expected = expected.cloned();
    run(move || {
        put(
            &Directory::open(&root, &ns)?,
            &ns,
            &marker,
            expected.as_ref(),
            &System,
        )
    })
    .await
}
fn contest_current(dir: &Directory, ns: &Namespace) -> Result<Option<ContestState>, BlobError> {
    dir.read(CONTESTED)?
        .map(|bytes| {
            if bytes.len() >= 44 && bytes.get(..8) == Some(b"ERPCAS01") {
                let nonce = Namespace::parse(
                    std::str::from_utf8(&bytes[8..44])
                        .map_err(|_| invalid("invalid local contest envelope"))?,
                )?;
                ContestState::decode(
                    &bytes[44..],
                    OwnerVersion::new(&format!("{ns}:{nonce}"), None)?,
                )
            } else {
                // Legacy active records cannot be replaced by old writers (create-only). New writes
                // always carry a random version, including identical losing markers after recovery.
                ContestState::decode(&bytes, OwnerVersion::new(&format!("{ns}:legacy"), None)?)
            }
        })
        .transpose()
}
fn contest_write(
    dir: &Directory,
    ns: &Namespace,
    marker: &OwnerMarker,
    resolved: bool,
) -> Result<ContestState, BlobError> {
    let nonce = Namespace::random()?;
    let mut bytes = b"ERPCAS01".to_vec();
    bytes.extend(nonce.as_str().as_bytes());
    bytes.extend(crate::owner::contest_bytes(marker, resolved));
    let temp = format!(".contest.temp-{nonce}");
    let work = (|| {
        dir.create(&temp, &bytes)?;
        fs::renameat(&dir.0, &temp, &dir.0, CONTESTED).map_err(primitive)?;
        dir.sync()?;
        Ok(ContestState::new(
            marker.clone(),
            OwnerVersion::new(&format!("{ns}:{nonce}"), None)?,
            resolved,
        ))
    })();
    finish(work, dir.unlink(&temp).and_then(|()| dir.sync()))
}
pub(super) async fn contest_state(
    root: &Path,
    ns: &Namespace,
) -> Result<Option<ContestState>, BlobError> {
    let root = root.to_owned();
    let ns = ns.clone();
    run(move || contest_current(&Directory::open(&root, &ns)?, &ns)).await
}
pub(super) async fn contested(
    root: &Path,
    ns: &Namespace,
) -> Result<Option<OwnerMarker>, BlobError> {
    Ok(contest_state(root, ns)
        .await?
        .filter(|s| !s.resolved())
        .map(|s| s.marker().clone()))
}
pub(super) async fn create_contested(
    root: &Path,
    ns: &Namespace,
    marker: &OwnerMarker,
) -> Result<(), BlobError> {
    let root = root.to_owned();
    let ns = ns.clone();
    let marker = marker.clone();
    run(move || {
        let dir = Directory::open(&root, &ns)?;
        let guard = dir.guard()?;
        let work = (|| {
            if contest_current(&dir, &ns)?.is_none_or(|s| s.resolved()) {
                contest_write(&dir, &ns, &marker, false)?;
            }
            Ok(())
        })();
        finish(work, guard.unlock())
    })
    .await
}
pub(super) async fn resolve_contested(
    root: &Path,
    ns: &Namespace,
    owner: &OwnerState,
    expected: Option<&ContestState>,
) -> Result<ContestState, BlobError> {
    let root = root.to_owned();
    let ns = ns.clone();
    let owner = owner.clone();
    let expected = expected.cloned();
    run(move || {
        let dir = Directory::open(&root, &ns)?;
        let guard = dir.guard()?;
        let work = (|| {
            if current(&dir, &ns)?.as_ref() != Some(&owner) {
                return Err(BlobError::OwnerConflict);
            }
            let found = contest_current(&dir, &ns)?;
            if let Some(s) = &found
                && s.resolved()
                && s.marker() == owner.marker()
            {
                dir.sync()?;
                return Ok(s.clone());
            }
            if found != expected {
                return Err(BlobError::OwnerConflict);
            }
            contest_write(&dir, &ns, owner.marker(), true)
        })();
        finish(work, guard.unlock())
    })
    .await
}
async fn run<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, BlobError> + Send + 'static,
) -> Result<T, BlobError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| BlobError::Unavailable {
            retryable: true,
            message: "the local owner task failed".to_owned(),
            source: Some(Box::new(error)),
        })?
}

pub(super) async fn probe(store: &super::LocalStore) -> Result<(), BlobError> {
    probe_namespace(store, Namespace::random()?).await
}

async fn probe_namespace(store: &super::LocalStore, ns: Namespace) -> Result<(), BlobError> {
    let root = store.root.clone();
    let private_ns = ns.clone();
    run(move || {
        let root = File::from(
            fs::open(
                &root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| io("open the probe root", e))?,
        );
        // Cleanup requires this exclusive mkdir to succeed.
        fs::mkdirat(&root, private_ns.as_str(), Mode::from_raw_mode(0o700)).map_err(|error| {
            if error == Errno::EXIST {
                BlobError::OwnerConflict
            } else {
                io("exclusively create the probe namespace", error)
            }
        })?;
        root.sync_all()
            .map_err(|e| io("sync the private probe namespace", e))
    })
    .await?;
    let result = async {
        let root = store.root.clone();
        let probe_ns = ns.clone();
        run(move || {
            let dir = Directory::open(&root, &probe_ns)?;
            let guard = dir.guard()?;
            match dir.try_guard()? {
                None => {}
                Some(other) => {
                    other.unlock()?;
                    return Err(BlobError::OwnerCapabilityMissing {
                        capability: "local exclusive advisory guard",
                    });
                }
            }
            guard.unlock()?;
            dir.create(".probe-a", b"a")?;
            dir.create(".probe-b", b"b")?;
            match dir.rename_no_replace(".probe-a", ".probe-b") {
                Err(Errno::EXIST) => {}
                Err(error) => return Err(primitive(error)),
                Ok(()) => {
                    return Err(BlobError::OwnerCapabilityMissing {
                        capability: "local no-replace rename",
                    });
                }
            }
            if dir.read(".probe-a")?.as_deref() != Some(b"a")
                || dir.read(".probe-b")?.as_deref() != Some(b"b")
            {
                return Err(BlobError::OwnerCapabilityMissing {
                    capability: "local no-replace rename",
                });
            }
            Ok(())
        })
        .await?;
        crate::owner::probe_contract(store, &ns).await
    }
    .await;
    let root = store.root.clone();
    let cleanup = run(move || {
        let dir = Directory::open(&root, &ns)?;
        for name in [OWNER, CONTESTED, ".probe-a", ".probe-b", GUARD] {
            dir.unlink(name)?;
        }
        dir.sync()?;
        // Only the fresh private namespace may be removed. Unexpected crash artifacts retain it.
        let root = File::from(
            fs::open(
                &root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| io("open the probe root", e))?,
        );
        fs::unlinkat(&root, ns.as_str(), AtFlags::REMOVEDIR)
            .map_err(|e| io("remove the private probe namespace", e))?;
        root.sync_all().map_err(|e| io("sync the probe root", e))
    })
    .await;
    finish_probe(result, cleanup)
}

#[cfg(test)]
#[path = "../../tests/support/local_owner.rs"]
mod tests;
