// SPDX-License-Identifier: MIT OR Apache-2.0
//! Durable cooperating-process file witness. No database or request-path I/O.
use erp_orm::audit::seal::{Anchor, AnchorRange, AuditWitness, SealIdentity, WitnessError};
use std::path::{Path, PathBuf};

/// An operator-selected JSONL witness in an existing trusted directory.
///
/// Operations open fresh descriptors, lock across processes, validate the entire bounded file,
/// and refuse torn records. Identical publication retries synchronize without appending. The
/// operator must not replace/rotate the file while it is in use. All cooperating nodes must use
/// the same storage with working advisory locks and file/directory synchronization.
/// Limits: 1,024 bytes/record, 16 MiB/file and 65,536 physical records. Reads do not create files.
#[derive(Debug)]
#[non_exhaustive]
pub struct FileWitness {
    path: PathBuf,
}
impl FileWitness {
    /// Select a file. Parent admission occurs on each operation; no directories are created.
    /// # Errors
    /// [`WitnessError::InvalidFile`] if the path has no ordinary file name.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, WitnessError> {
        let path = path.as_ref();
        if path.file_name().is_none() {
            return Err(WitnessError::InvalidFile);
        }
        Ok(Self {
            path: path.to_owned(),
        })
    }
    /// Read all independent anchors, including incarnations/epochs absent from the database.
    /// Identical duplicates collapse, and identity order is deterministic.
    /// # Errors
    /// Missing/unavailable storage, contention, unsafe files, invalid records, conflicts or limits.
    pub fn inventory(&self) -> Result<Vec<Anchor>, WitnessError> {
        #[cfg(unix)]
        {
            let opened = unix::Opened::open(&self.path, false)?;
            Ok(unix::scan(&opened.file)?.anchors.into_values().collect())
        }
        #[cfg(not(unix))]
        {
            Err(WitnessError::UnsupportedPlatform)
        }
    }
}
impl AuditWitness for FileWitness {
    fn publish(&self, anchor: &Anchor) -> Result<(), WitnessError> {
        #[cfg(unix)]
        {
            unix::publish(&self.path, anchor)
        }
        #[cfg(not(unix))]
        {
            let _ = anchor;
            Err(WitnessError::UnsupportedPlatform)
        }
    }
    fn fetch(
        &self,
        incarnation: [u8; 16],
        epoch: u64,
        range: AnchorRange,
    ) -> Result<Vec<Anchor>, WitnessError> {
        SealIdentity::new(incarnation, epoch, 1).map_err(|_| WitnessError::InvalidIdentity)?;
        Ok(self
            .inventory()?
            .into_iter()
            .filter(|anchor| {
                let id = anchor.identity();
                *id.incarnation() == incarnation
                    && id.epoch() == epoch
                    && range.contains(id.batch_no())
            })
            .collect())
    }
}

#[cfg(unix)]
mod unix {
    use super::super::witness_format::{self, MAX_FILE_BYTES, MAX_RECORD_BYTES, MAX_RECORDS};
    use super::*;
    use erp_orm::audit::seal::WitnessOperation as Op;
    use rustix::fs::{FlockOperation, Mode, OFlags, flock, open, openat};
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::os::unix::fs::MetadataExt;

    type Key = ([u8; 16], u64, u64);
    fn key(anchor: &Anchor) -> Key {
        let id = anchor.identity();
        (*id.incarnation(), id.epoch(), id.batch_no())
    }
    fn storage(operation: Op, error: impl Into<io::Error>) -> WitnessError {
        WitnessError::Storage {
            operation,
            kind: error.into().kind(),
        }
    }
    fn uncertain(operation: Op, error: io::Error) -> WitnessError {
        WitnessError::PublicationUncertain {
            operation,
            kind: error.kind(),
        }
    }
    fn open_data_file(directory: &File, name: &OsStr, write: bool) -> Result<File, WitnessError> {
        let access = if write {
            OFlags::RDWR | OFlags::APPEND
        } else {
            OFlags::RDONLY
        };
        let flags = access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        match openat(directory, name, flags, Mode::empty()) {
            Ok(fd) => Ok(File::from(fd)),
            Err(rustix::io::Errno::NOENT) if write => {
                // The macOS concurrent-publisher regression reported ENOENT; follow-up probes
                // found both directory and target. Separate lookup from exclusive creation:
                // a definite creation collision is contention, never an empty witness.
                match openat(
                    directory,
                    name,
                    flags | OFlags::CREATE | OFlags::EXCL,
                    Mode::RUSR | Mode::WUSR,
                ) {
                    Ok(fd) => Ok(File::from(fd)),
                    Err(rustix::io::Errno::EXIST) => Err(WitnessError::Busy),
                    Err(error) => Err(storage(Op::Open, error)),
                }
            }
            Err(error) => Err(storage(Op::Open, error)),
        }
    }
    pub(super) struct Opened {
        pub file: File,
        directory: File,
        name: OsString,
    }
    impl Opened {
        pub(super) fn open(path: &Path, write: bool) -> Result<Self, WitnessError> {
            let name = path
                .file_name()
                .ok_or(WitnessError::InvalidFile)?
                .to_owned();
            let parent = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                Some(_) | None => Path::new("."), // Bare operator names are relative to cwd.
            };
            let directory = File::from(
                open(
                    parent,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| storage(Op::Open, e))?,
            );
            let file = open_data_file(&directory, &name, write)?;
            let metadata = file.metadata().map_err(|e| storage(Op::Inspect, e))?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(WitnessError::InvalidFile);
            }
            let lock = if write {
                FlockOperation::NonBlockingLockExclusive
            } else {
                FlockOperation::NonBlockingLockShared
            };
            flock(&file, lock).map_err(|e| {
                if e == rustix::io::Errno::WOULDBLOCK {
                    WitnessError::Busy
                } else {
                    storage(Op::Lock, e)
                }
            })?;
            let opened = Self {
                file,
                directory,
                name,
            };
            opened.check_identity()?;
            Ok(opened)
        }
        fn check_identity(&self) -> Result<(), WitnessError> {
            let metadata = self.file.metadata().map_err(|e| storage(Op::Inspect, e))?;
            let named_file = File::from(
                openat(
                    &self.directory,
                    &self.name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| storage(Op::Inspect, e))?,
            );
            let named = named_file.metadata().map_err(|e| storage(Op::Inspect, e))?;
            if metadata.nlink() != 1
                || metadata.dev() != named.dev()
                || metadata.ino() != named.ino()
            {
                return Err(WitnessError::InvalidFile);
            }
            Ok(())
        }
    }
    pub(super) struct Inventory {
        pub anchors: BTreeMap<Key, Anchor>,
        bytes: u64,
        records: u32,
    }
    pub(super) fn scan(file: &File) -> Result<Inventory, WitnessError> {
        if file.metadata().map_err(|e| storage(Op::Inspect, e))?.len() > MAX_FILE_BYTES {
            return Err(WitnessError::Limit("file bytes"));
        }
        let mut reader = BufReader::with_capacity(MAX_RECORD_BYTES, file);
        let mut inventory = Inventory {
            anchors: BTreeMap::new(),
            bytes: 0,
            records: 0,
        };
        let mut line = Vec::with_capacity(MAX_RECORD_BYTES + 1);
        loop {
            line.clear();
            let size = (&mut reader)
                .take((MAX_RECORD_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)
                .map_err(|e| storage(Op::Read, e))?;
            if size == 0 {
                break;
            }
            inventory.bytes = inventory
                .bytes
                .checked_add(size as u64)
                .ok_or(WitnessError::Limit("file bytes"))?;
            if inventory.bytes > MAX_FILE_BYTES {
                return Err(WitnessError::Limit("file bytes"));
            }
            inventory.records = inventory
                .records
                .checked_add(1)
                .ok_or(WitnessError::Limit("physical records"))?;
            if inventory.records > MAX_RECORDS {
                return Err(WitnessError::Limit("physical records"));
            }
            if size > MAX_RECORD_BYTES {
                return Err(WitnessError::Limit("record bytes"));
            }
            if line.last() != Some(&b'\n') {
                return Err(WitnessError::TornRecord {
                    line: inventory.records,
                });
            }
            let anchor = witness_format::decode(&line, inventory.records)?;
            if let Some(previous) = inventory.anchors.insert(key(&anchor), anchor)
                && previous != anchor
            {
                return Err(WitnessError::Conflict);
            }
        }
        Ok(inventory)
    }

    // A private fault boundary enables deterministic write/fsync failures without an alternate store.
    trait PublicationIo {
        fn append(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
        fn sync_file(&mut self, file: &File) -> io::Result<()>;
        fn sync_directory(&mut self, directory: &File) -> io::Result<()>;
    }
    struct DurableIo;
    impl PublicationIo for DurableIo {
        fn append(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
            file.write_all(bytes)
        }
        fn sync_file(&mut self, file: &File) -> io::Result<()> {
            file.sync_all()
        }
        fn sync_directory(&mut self, directory: &File) -> io::Result<()> {
            directory.sync_all()
        }
    }
    pub(super) fn publish(path: &Path, anchor: &Anchor) -> Result<(), WitnessError> {
        publish_with(path, anchor, &mut DurableIo)
    }
    fn publish_with(
        path: &Path,
        anchor: &Anchor,
        io: &mut impl PublicationIo,
    ) -> Result<(), WitnessError> {
        let bytes = witness_format::encode(anchor)?;
        let mut opened = Opened::open(path, true)?;
        let inventory = scan(&opened.file)?;
        match inventory.anchors.get(&key(anchor)) {
            Some(previous) if previous != anchor => return Err(WitnessError::Conflict),
            Some(_) => {} // Identical retries still sync below, reconciling prior sync uncertainty.
            None => {
                if inventory.records >= MAX_RECORDS {
                    return Err(WitnessError::Limit("physical records"));
                }
                if inventory
                    .bytes
                    .checked_add(bytes.len() as u64)
                    .ok_or(WitnessError::Limit("file bytes"))?
                    > MAX_FILE_BYTES
                {
                    return Err(WitnessError::Limit("file bytes"));
                }
                opened.check_identity()?;
                io.append(&mut opened.file, &bytes)
                    .map_err(|e| uncertain(Op::Write, e))?;
            }
        }
        io.sync_file(&opened.file)
            .map_err(|e| uncertain(Op::SyncFile, e))?;
        io.sync_directory(&opened.directory)
            .map_err(|e| uncertain(Op::SyncDirectory, e))?;
        opened.check_identity().map_err(|error| match error {
            WitnessError::Storage { kind, .. } => WitnessError::PublicationUncertain {
                operation: Op::Inspect,
                kind,
            },
            _ => WitnessError::PublicationUncertain {
                operation: Op::Inspect,
                kind: io::ErrorKind::InvalidData,
            },
        })?;
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]
        use super::*;
        struct FaultIo {
            fail: Option<Op>,
            short: bool,
            events: Vec<Op>,
        }
        impl PublicationIo for FaultIo {
            fn append(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
                self.events.push(Op::Write);
                if self.short {
                    file.write_all(&bytes[..7])?;
                }
                if self.fail == Some(Op::Write) {
                    return Err(io::Error::other("private-injected-detail"));
                }
                file.write_all(bytes)
            }
            fn sync_file(&mut self, file: &File) -> io::Result<()> {
                self.events.push(Op::SyncFile);
                if self.fail == Some(Op::SyncFile) {
                    return Err(io::Error::other("private-injected-detail"));
                }
                file.sync_all()
            }
            fn sync_directory(&mut self, directory: &File) -> io::Result<()> {
                self.events.push(Op::SyncDirectory);
                if self.fail == Some(Op::SyncDirectory) {
                    return Err(io::Error::other("private-injected-detail"));
                }
                directory.sync_all()
            }
        }
        fn anchor() -> Anchor {
            Anchor::new(SealIdentity::new([1; 16], 1, 1).unwrap(), [9; 32], 1).unwrap()
        }

        #[test]
        fn witness_io_failures_are_explicit_and_retries_sync_without_duplicates() {
            for fail in [Op::Write, Op::SyncFile, Op::SyncDirectory] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("anchors");
                let mut fault = FaultIo {
                    fail: Some(fail),
                    short: false,
                    events: Vec::new(),
                };
                let error = publish_with(&path, &anchor(), &mut fault).unwrap_err();
                assert_eq!(
                    error,
                    WitnessError::PublicationUncertain {
                        operation: fail,
                        kind: io::ErrorKind::Other
                    }
                );
                assert_eq!(
                    error.to_string(),
                    format!("audit witness: {fail} failed (Other); publication may be incomplete")
                );
                assert!(!format!("{error:?}").contains("private-injected-detail"));
                let mut retry = FaultIo {
                    fail: None,
                    short: false,
                    events: Vec::new(),
                };
                publish_with(&path, &anchor(), &mut retry).unwrap();
                let expected = if fail == Op::Write {
                    vec![Op::Write, Op::SyncFile, Op::SyncDirectory]
                } else {
                    vec![Op::SyncFile, Op::SyncDirectory]
                };
                assert_eq!(retry.events, expected);
                assert_eq!(
                    FileWitness::new(&path).unwrap().inventory().unwrap(),
                    [anchor()]
                );
            }
        }
        #[test]
        fn witness_partial_write_is_not_silently_repaired() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("anchors");
            let mut fault = FaultIo {
                fail: Some(Op::Write),
                short: true,
                events: Vec::new(),
            };
            assert!(matches!(
                publish_with(&path, &anchor(), &mut fault),
                Err(WitnessError::PublicationUncertain {
                    operation: Op::Write,
                    ..
                })
            ));
            let bytes = std::fs::read(&path).unwrap();
            let error = publish(&path, &anchor()).unwrap_err();
            assert_eq!(error, WitnessError::TornRecord { line: 1 });
            assert_eq!(
                error.to_string(),
                "audit witness: torn record at line 1; operator reconciliation required"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        #[test]
        fn witness_replacement_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("anchors");
            let opened = Opened::open(&path, true).unwrap();
            std::fs::rename(&path, dir.path().join("old")).unwrap();
            std::fs::write(&path, []).unwrap();
            assert_eq!(
                opened.check_identity().unwrap_err(),
                WitnessError::InvalidFile
            );
        }
        #[test]
        fn witness_read_and_open_errors_preserve_operation_and_kind() {
            let dir = tempfile::tempdir().unwrap();
            let error = FileWitness::new(dir.path().join("missing"))
                .unwrap()
                .inventory()
                .unwrap_err();
            assert_eq!(
                error,
                WitnessError::Storage {
                    operation: Op::Open,
                    kind: io::ErrorKind::NotFound
                }
            );
            assert_eq!(error.to_string(), "audit witness: open failed (NotFound)");
            let path = dir.path().join("write-only");
            std::fs::write(&path, []).unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            let error = match scan(&file) {
                Err(error) => error,
                Ok(_) => panic!("read on write-only file succeeded"),
            };
            assert!(matches!(
                error,
                WitnessError::Storage {
                    operation: Op::Read,
                    ..
                }
            ));
        }
    }
}
