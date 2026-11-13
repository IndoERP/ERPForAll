// SPDX-License-Identifier: MIT OR Apache-2.0
//! Descriptor-relative operations only in an exclusively allocated probe namespace.
use super::{
    LocalStore,
    owner::{Directory, Guard},
};
use crate::peer::{PEER_STEPS, PeerCleanup, PeerObservation, refused};
use crate::{AttachmentStore, BlobError, Namespace, OwnerIdentity, OwnerMarker, OwnerVersion};
use rustix::fs::{self, AtFlags, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Write, sync::Arc};

/// One live participant. Dropping it releases any held advisory guard; it never removes real data.
#[derive(Debug)]
#[non_exhaustive]
pub struct LocalPeer {
    store: Arc<LocalStore>,
    ns: Namespace,
    dir: Directory,
    slot: u8,
    next: u8,
    guard: Option<Guard>,
    version: Option<OwnerVersion>,
}
impl LocalPeer {
    /// Exclusively allocate a private namespace for participant A.
    /// # Errors
    /// `PeerProbe` for reused namespaces; typed filesystem failures otherwise.
    pub fn create(store: Arc<LocalStore>, ns: Namespace) -> Result<Self, BlobError> {
        let root = fs::open(
            &store.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io(e.into()))?;
        match fs::mkdirat(&root, ns.as_str(), Mode::from_raw_mode(0o700)) {
            Ok(()) => File::from(root).sync_all().map_err(io)?,
            Err(Errno::EXIST) => return Err(refused("probe namespace already exists")),
            Err(e) => return Err(io(e.into())),
        }
        Self::open(store, ns, 0)
    }
    /// Join an existing private namespace as participant B. Never creates missing directories.
    /// # Errors
    /// `PeerProbe` if the namespace is missing or unsafe; typed filesystem failures otherwise.
    pub fn join(store: Arc<LocalStore>, ns: Namespace) -> Result<Self, BlobError> {
        Self::open(store, ns, 1)
    }
    fn open(store: Arc<LocalStore>, ns: Namespace, slot: u8) -> Result<Self, BlobError> {
        let meta = std::fs::symlink_metadata(store.root.join(ns.as_str())).map_err(io)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(refused("unsafe probe namespace"));
        }
        let dir = Directory::open(&store.root, &ns)?;
        Ok(Self {
            store,
            ns,
            dir,
            slot,
            next: 0,
            guard: None,
            version: None,
        })
    }
    /// Execute exactly the next phase. The caller rendezvous with its peer between phases.
    /// # Errors
    /// Typed primitive failures, wrong phase, or disagreement with the peer's published bytes.
    pub async fn step(&mut self, phase: u8) -> Result<PeerObservation, BlobError> {
        if phase != self.next || phase >= PEER_STEPS {
            return Err(refused("unexpected local phase"));
        }
        let own = if self.slot == 0 { ".peer-a" } else { ".peer-b" };
        let other = if self.slot == 0 { ".peer-b" } else { ".peer-a" };
        let mut result = 1;
        match phase {
            0 => self.dir.create(own, self.ns.as_str().as_bytes())?,
            1 => {
                if self.dir.read(other)?.as_deref() != Some(self.ns.as_str().as_bytes()) {
                    return Err(refused("peer challenge is missing or differs"));
                }
            }
            2 => {
                result = u8::from(self.exclusive(".race", &[self.slot + 1])?);
            }
            3 => {
                let bytes = self
                    .dir
                    .read(".race")?
                    .ok_or_else(|| refused("race winner is missing"))?;
                if bytes.len() != 1 || !matches!(bytes[0], 1 | 2) {
                    return Err(refused("invalid race winner"));
                }
                result = bytes[0];
            }
            4 if self.slot == 0 => {
                self.hold()?;
            }
            5 if self.slot == 1 => {
                self.excluded()?;
                if self.exclusive(".peer-a", b"overwrite")? {
                    return Err(refused("held exclusive create succeeded"));
                }
            }
            6 if self.slot == 0 => self.release()?,
            7 if self.slot == 1 => self.hold()?,
            8 if self.slot == 0 => {
                self.excluded()?;
                if self.exclusive(".peer-b", b"overwrite")? {
                    return Err(refused("held exclusive create succeeded"));
                }
            }
            9 if self.slot == 1 => self.release()?,
            10 if self.slot == 0 => {
                self.version = Some(
                    self.store
                        .put_owner(&self.ns, &self.marker(1)?, None)
                        .await?,
                );
            }
            11 if self.slot == 1 => {
                let found = self
                    .store
                    .owner(&self.ns)
                    .await?
                    .ok_or_else(|| refused("peer owner publication is missing"))?;
                if found.marker() != &self.marker(1)? {
                    return Err(refused("peer owner publication differs"));
                }
                self.version = Some(found.version().clone());
            }
            12 if self.slot == 1 => {
                self.store
                    .put_owner(&self.ns, &self.marker(2)?, self.version.as_ref())
                    .await?;
            }
            13 if self.slot == 0 => match self
                .store
                .put_owner(&self.ns, &self.marker(2)?, self.version.as_ref())
                .await
            {
                Err(BlobError::OwnerConflict) => {}
                Err(e) => return Err(e),
                Ok(_) => return Err(refused("stale conditional write succeeded")),
            },
            14 => {
                let (a, b) = if self.slot == 0 {
                    (".rename-a0", ".rename-b0")
                } else {
                    (".rename-a1", ".rename-b1")
                };
                self.dir.create(a, b"a")?;
                self.dir.create(b, b"b")?;
                match self.dir.rename_no_replace(a, b) {
                    Err(Errno::EXIST) => {}
                    Err(e) => return Err(io(e.into())),
                    Ok(()) => return Err(refused("no-replace rename overwrote its target")),
                };
                if self.dir.read(a)?.as_deref() != Some(b"a")
                    || self.dir.read(b)?.as_deref() != Some(b"b")
                {
                    return Err(refused("no-replace rename changed bytes"));
                }
            }
            15 => {
                let found = self
                    .store
                    .owner(&self.ns)
                    .await?
                    .ok_or_else(|| refused("final owner publication is missing"))?;
                if found.marker() != &self.marker(2)?
                    || self.dir.read(own)?.as_deref() != Some(self.ns.as_str().as_bytes())
                {
                    return Err(refused("final publication differs"));
                }
            }
            // The opposite participant deliberately waits while its peer holds/checks a primitive.
            4..=13 => {}
            _ => return Err(refused("unknown phase")),
        }
        self.next += 1;
        Ok(PeerObservation {
            session: self.ns.clone(),
            slot: self.slot,
            phase,
            value: result,
            mount: self.store.peer_binding()?,
        })
    }
    fn marker(&self, generation: u64) -> Result<OwnerMarker, BlobError> {
        OwnerMarker::new(
            OwnerIdentity::new(1, 1, self.ns.clone())?,
            generation,
            self.ns.clone(),
            0,
        )
    }
    fn hold(&mut self) -> Result<(), BlobError> {
        self.guard = Some(
            self.dir
                .try_guard()?
                .ok_or_else(|| refused("released peer guard is unavailable"))?,
        );
        Ok(())
    }
    fn excluded(&self) -> Result<(), BlobError> {
        match self.dir.try_guard()? {
            None => Ok(()),
            Some(g) => {
                g.unlock()?;
                Err(refused(
                    "peer advisory guard did not exclude a second holder",
                ))
            }
        }
    }
    fn release(&mut self) -> Result<(), BlobError> {
        self.guard
            .take()
            .ok_or_else(|| refused("held guard is missing"))?
            .unlock()
    }
    fn exclusive(&self, name: &str, bytes: &[u8]) -> Result<bool, BlobError> {
        match self
            .dir
            .file(name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, 0o440)
        {
            Ok(mut file) => {
                file.write_all(bytes).map_err(io)?;
                file.sync_all().map_err(io)?;
                self.dir.sync()?;
                Ok(true)
            }
            Err(Errno::EXIST) => Ok(false),
            Err(e) => Err(io(e.into())),
        }
    }
    /// Remove only this completed attempt's known artifacts. The peer must have finished first.
    /// # Errors
    /// Incomplete protocol, filesystem failure or unexpected artifacts; failures never admit evidence.
    pub fn cleanup(self) -> Result<PeerCleanup, BlobError> {
        if self.slot != 0 || self.next != PEER_STEPS {
            return Err(refused("only a completed creator can clean the probe"));
        }
        for name in [
            ".peer-a",
            ".peer-b",
            ".race",
            ".rename-a0",
            ".rename-b0",
            ".rename-a1",
            ".rename-b1",
            "_owner",
            ".owner.guard",
        ] {
            self.dir.unlink(name)?;
        }
        self.dir.sync()?;
        let root = fs::open(
            &self.store.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io(e.into()))?;
        fs::unlinkat(&root, self.ns.as_str(), AtFlags::REMOVEDIR).map_err(|e| io(e.into()))?;
        File::from(root).sync_all().map_err(io)?;
        Ok(PeerCleanup {
            mount: self.store.peer_binding()?,
            session: self.ns,
        })
    }
}
fn io(e: std::io::Error) -> BlobError {
    BlobError::from_io("perform peer probe operation", "<peer>", e)
}

impl LocalStore {
    /// Digest the current local mount/root binding, without exposing its path in evidence.
    /// # Errors
    /// Unreadable, changed or unsafe root identity.
    pub fn peer_binding(&self) -> Result<String, BlobError> {
        use std::os::unix::fs::MetadataExt;
        let root = fs::open(
            &self.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io(e.into()))?;
        let dir = Directory(File::from(root));
        let meta = dir.0.metadata().map_err(io)?;
        if dir.read(".store-id")?.as_deref() != Some(self.fingerprint.as_bytes()) {
            return Err(refused("physical store fingerprint changed"));
        }
        let mut hash = Sha256::new();
        hash.update(
            std::fs::canonicalize(&self.root)
                .map_err(io)?
                .as_os_str()
                .as_encoded_bytes(),
        );
        hash.update(meta.dev().to_le_bytes());
        hash.update(meta.ino().to_le_bytes());
        hash.update(self.fingerprint.as_bytes());
        Ok(crate::key::to_hex(&hash.finalize()))
    }
}
