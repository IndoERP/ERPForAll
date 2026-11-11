//! Bounded, immutable addon files admitted for one lifecycle operation.
//!
//! Content receipts are portable across releases. Filesystem stamps and the pinned root descriptor
//! detect replacement within one operation and are never serialized or persisted. Consumers must
//! read [`ArtifactSnapshot::files`] rather than reopen paths after admission.
//!
//! Detection limit: within-operation mutation is detected from filesystem stamps (device, inode,
//! length, link count, mtime, ctime) compared around each read. A rewrite that keeps the length and
//! lands inside one filesystem timestamp tick leaves the stamp unchanged; coarse-tick filesystems
//! (common on Linux) cannot distinguish it, and the bytes read afterwards are the admitted bytes.
//! The canonical digest always covers the bytes actually read. A rewrite that finishes before the
//! read yields a consistent snapshot of the new content; a write racing the read itself in the same
//! tick and at the same length may yield mixed bytes that stamps cannot detect. The limit covers
//! mutation during a single capture only: [`ArtifactSnapshot::verify_unchanged`] recaptures and
//! compares the content digest as well, so a later rewrite is reported.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use erp_orm::node::NodeError;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};

use crate::{ModuleName, Result};

/// Aggregate admitted file bytes. This is an input safety bound, not a performance measurement.
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum number of regular files in an artifact.
pub const MAX_ARTIFACT_FILES: usize = 20_000;
/// Maximum number of directory entries, including directories, in an artifact.
pub const MAX_ARTIFACT_ENTRIES: usize = 40_000;
/// Maximum directory nesting below the artifact root.
pub const MAX_ARTIFACT_DEPTH: usize = 32;
/// Maximum aggregate relative-path bytes retained while walking an artifact.
pub const MAX_ARTIFACT_PATH_BYTES: usize = 1024 * 1024;
/// Maximum bytes in one relative path or the configured root path.
pub const MAX_ARTIFACT_PATH_LENGTH: usize = 4096;
/// Maximum duration of each filesystem capture, checked between bounded filesystem operations.
pub const ARTIFACT_CAPTURE_DEADLINE: Duration = Duration::from_secs(30);

/// Immutable file bytes and transient filesystem admission for one addon operation.
///
/// The file map is bounded by [`MAX_ARTIFACT_BYTES`]. Verification temporarily captures a second
/// bounded map; it does not replace the admitted bytes. The descriptor pins the original root until
/// this value is dropped. This type makes no claim about which native code was linked into a binary;
/// the caller must separately bind linked-code provenance to this receipt.
#[derive(Debug)]
#[non_exhaustive]
pub struct ArtifactSnapshot {
    addon: String,
    root_path: PathBuf,
    root: File,
    ancestors: Vec<(u64, u64)>,
    stamps: BTreeMap<String, Stamp>,
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeSet<String>,
    digest: String,
}

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    length: u64,
    links: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            links: metadata.nlink(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    fn identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }
}

fn refuse(addon: &str, message: impl Into<String>) -> crate::Error {
    NodeError::AddonSource {
        addon: addon.to_owned(),
        message: message.into(),
    }
    .into()
}

fn changed(addon: &str) -> crate::Error {
    refuse(addon, "artifact changed during lifecycle operation")
}

fn metadata(addon: &str, file: &File) -> Result<Metadata> {
    file.metadata().map_err(|error| {
        refuse(
            addon,
            format!("cannot inspect artifact descriptor: {error}"),
        )
    })
}

impl ArtifactSnapshot {
    /// Capture runtime/source files below `root` through pinned directory descriptors.
    /// Release-excluded tooling/metadata entries (`node_modules`, `target`, `.git`,
    /// `__pycache__`, `.DS_Store`) are never traversed or available to lifecycle consumers.
    ///
    /// Neither the configured path nor any admitted descendant may contain a symbolic link.
    /// A link at an excluded entry itself still refuses. Files with
    /// multiple hard links, special files, non-UTF-8 names, parent traversal and over-bound inputs
    /// refuse. Relative roots resolve against the current working directory without following
    /// symbolic links. Intentional deployment pointer resolution belongs to the trusted caller.
    ///
    /// The canonical receipt is SHA-256 over `erp-addon-artifact-v1\0`, the addon UTF-8 byte length
    /// and name, file count, then sorted relative paths. Each file contributes its UTF-8 path length,
    /// path, byte size and raw SHA-256. Integer lengths/counts are unsigned 64-bit big-endian. The
    /// external representation is `addon-v1:` followed by 64 lowercase hexadecimal characters.
    ///
    /// # Errors
    /// Invalid module names return [`crate::Error::InvalidModuleName`]. Filesystem, mutation and
    /// safety-limit failures return [`crate::Error::NodeDigest`] with [`NodeError::AddonSource`].
    ///
    /// ```no_run
    /// use erp_modules::artifact_identity::ArtifactSnapshot;
    /// let source = ArtifactSnapshot::capture("note", std::path::Path::new("addons/note"))?;
    /// let content = source.files().get("manifest.toml");
    /// source.verify_unchanged()?;
    /// # Ok::<(), erp_modules::Error>(())
    /// ```
    pub fn capture(addon: &str, root: &Path) -> Result<Self> {
        ModuleName::validate(addon)?;
        let started = Instant::now();
        let root_path = absolute_path(addon, root)?;
        let (root, ancestors) = open_root(addon, &root_path)?;
        let mut walk = Walk {
            addon,
            started,
            entries: 0,
            path_bytes: 0,
            bytes: 0,
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
            stamps: BTreeMap::new(),
            #[cfg(test)]
            after_file_metadata: None,
        };
        walk.directory(&root, "", 0)?;
        let digest = content_digest(addon, &walk.files)?;
        Ok(Self {
            addon: addon.to_owned(),
            root_path,
            root,
            ancestors,
            stamps: walk.stamps,
            files: walk.files,
            directories: walk.directories,
            digest,
        })
    }

    /// The admitted physical root, for diagnostic paths only; consumers still use snapshot bytes.
    pub(crate) fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// The portable versioned per-addon file-content receipt; it contains no inode or timestamp.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The exact immutable bytes hashed during admission, keyed by sorted relative UTF-8 path.
    pub fn files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }

    /// The admitted relative directory paths, including the root as an empty string.
    ///
    /// Empty directories affect within-operation verification but not the portable content hash.
    pub fn directories(&self) -> &BTreeSet<String> {
        &self.directories
    }

    /// Revalidate paths, content and transient stamps against this operation's admitted snapshot.
    ///
    /// A replacement with identical bytes still refuses within this operation. A separately
    /// captured copy with identical content has an equal portable digest across operations.
    ///
    /// # Errors
    /// As [`Self::capture`], including a changed root or file set. No receipt or captured file bytes
    /// are updated on failure.
    pub fn verify_unchanged(&self) -> Result<()> {
        let pinned = Stamp::from_metadata(&metadata(&self.addon, &self.root)?);
        if self.stamps.get("") != Some(&pinned) {
            return Err(changed(&self.addon));
        }
        let current = Self::capture(&self.addon, &self.root_path)?;
        if self.ancestors != current.ancestors
            || self.stamps != current.stamps
            || self.digest != current.digest
        {
            return Err(changed(&self.addon));
        }
        Ok(())
    }
}

fn absolute_path(addon: &str, root: &Path) -> Result<PathBuf> {
    if root.components().any(|part| part == Component::ParentDir) {
        return Err(refuse(
            addon,
            "artifact path must not contain parent components",
        ));
    }
    let path = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| refuse(addon, format!("cannot resolve artifact root: {error}")))?
            .join(root)
    };
    if path.as_os_str().len() > MAX_ARTIFACT_PATH_LENGTH {
        return Err(refuse(addon, "artifact path exceeds 4096 bytes"));
    }
    Ok(path)
}

fn open_root(addon: &str, path: &Path) -> Result<(File, Vec<(u64, u64)>)> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = File::from(
        rustix::fs::open("/", flags, Mode::empty())
            .map_err(|error| refuse(addon, format!("cannot open artifact root: {error}")))?,
    );
    let mut ancestors = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                let stat = rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|error| {
                        refuse(addon, format!("cannot inspect artifact path: {error}"))
                    })?;
                if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
                    return Err(refuse(addon, "symbolic link in artifact path"));
                }
                let next = File::from(
                    rustix::fs::openat(&directory, name, flags, Mode::empty()).map_err(
                        |error| refuse(addon, format!("cannot open artifact path: {error}")),
                    )?,
                );
                let stamp = Stamp::from_metadata(&metadata(addon, &next)?);
                if stamp.identity() != stat_identity(&stat) {
                    return Err(changed(addon));
                }
                ancestors.push(stamp.identity());
                directory = next;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(refuse(addon, "unsupported artifact root component"));
            }
        }
    }
    Ok((directory, ancestors))
}

#[allow(
    clippy::useless_conversion,
    clippy::unnecessary_cast,
    reason = "stat fields differ in width on macOS and Linux"
)]
fn stat_identity(stat: &rustix::fs::Stat) -> (u64, u64) {
    // std::os::unix::fs::MetadataExt::dev uses the same device-bit conversion on Darwin,
    // where dev_t is signed. This compares kernel identities, not arithmetic input.
    (stat.st_dev as u64, u64::from(stat.st_ino))
}

struct Walk<'a> {
    addon: &'a str,
    started: Instant,
    entries: usize,
    path_bytes: usize,
    bytes: u64,
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeSet<String>,
    stamps: BTreeMap<String, Stamp>,
    #[cfg(test)]
    after_file_metadata: Option<Box<dyn FnOnce()>>,
}

impl Walk<'_> {
    fn check_deadline(&self) -> Result<()> {
        if self.started.elapsed() > ARTIFACT_CAPTURE_DEADLINE {
            return Err(refuse(self.addon, "artifact capture deadline exceeded"));
        }
        Ok(())
    }

    fn directory(&mut self, directory: &File, prefix: &str, depth: usize) -> Result<()> {
        self.check_deadline()?;
        if depth > MAX_ARTIFACT_DEPTH {
            return Err(refuse(self.addon, "artifact depth exceeds 32"));
        }
        let initial = Stamp::from_metadata(&metadata(self.addon, directory)?);
        let mut names = Vec::new();
        let listing = Dir::read_from(directory).map_err(|error| {
            refuse(
                self.addon,
                format!("cannot list artifact directory: {error}"),
            )
        })?;
        for entry in listing {
            self.check_deadline()?;
            let entry = entry.map_err(|error| {
                refuse(
                    self.addon,
                    format!("cannot read artifact directory entry: {error}"),
                )
            })?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            self.entries = self
                .entries
                .checked_add(1)
                .ok_or_else(|| refuse(self.addon, "artifact entry count overflow"))?;
            if self.entries > MAX_ARTIFACT_ENTRIES {
                return Err(refuse(self.addon, "artifact entries exceed 40000"));
            }
            let name = std::str::from_utf8(name)
                .map_err(|_| refuse(self.addon, "artifact path is not UTF-8"))?;
            let relative = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            self.path_bytes = self
                .path_bytes
                .checked_add(relative.len())
                .ok_or_else(|| refuse(self.addon, "artifact path byte count overflow"))?;
            if relative.len() > MAX_ARTIFACT_PATH_LENGTH
                || self.path_bytes > MAX_ARTIFACT_PATH_BYTES
            {
                return Err(refuse(
                    self.addon,
                    "artifact relative paths exceed byte limits",
                ));
            }
            let stat = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(
                |error| {
                    refuse(
                        self.addon,
                        format!("cannot inspect artifact entry: {error}"),
                    )
                },
            )?;
            let kind = FileType::from_raw_mode(stat.st_mode);
            match kind {
                FileType::Symlink => {
                    return Err(refuse(self.addon, "symbolic link in artifact path"));
                }
                FileType::Directory | FileType::RegularFile => {}
                _ => {
                    return Err(refuse(
                        self.addon,
                        "artifact entry is not a regular file or directory",
                    ));
                }
            }
            if crate::artifact_paths::excluded(name) {
                // Match release preparation. The entry's own type was admitted above, but
                // excluded tooling/metadata bytes and descendants are never traversed.
                continue;
            }
            names.push((name.to_owned(), relative, kind, stat_identity(&stat)));
        }
        names.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, relative, kind, expected) in names {
            self.check_deadline()?;
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let flags = if kind == FileType::Directory {
                flags | OFlags::DIRECTORY
            } else {
                flags
            };
            let mut file = File::from(
                rustix::fs::openat(directory, name.as_str(), flags, Mode::empty()).map_err(
                    |error| refuse(self.addon, format!("cannot open artifact entry: {error}")),
                )?,
            );
            let meta = metadata(self.addon, &file)?;
            let initial_file = Stamp::from_metadata(&meta);
            if initial_file.identity() != expected {
                return Err(changed(self.addon));
            }
            if kind == FileType::Directory {
                self.directory(&file, &relative, depth + 1)?;
            } else {
                if !meta.is_file() {
                    return Err(refuse(
                        self.addon,
                        "artifact entry is not a regular file or directory",
                    ));
                }
                if meta.nlink() != 1 {
                    return Err(refuse(self.addon, "artifact file has multiple hard links"));
                }
                if self.files.len() >= MAX_ARTIFACT_FILES {
                    return Err(refuse(self.addon, "artifact files exceed 20000"));
                }
                self.bytes = self
                    .bytes
                    .checked_add(meta.len())
                    .ok_or_else(|| refuse(self.addon, "artifact byte count overflow"))?;
                if self.bytes > MAX_ARTIFACT_BYTES {
                    return Err(refuse(self.addon, "artifact bytes exceed 67108864"));
                }
                #[cfg(test)]
                if let Some(action) = self.after_file_metadata.take() {
                    action();
                }
                let cap = meta
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| refuse(self.addon, "artifact file size overflow"))?;
                let capacity = usize::try_from(cap)
                    .map_err(|_| refuse(self.addon, "artifact file size overflow"))?;
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(capacity).map_err(|error| {
                    refuse(
                        self.addon,
                        format!("cannot reserve artifact bytes: {error}"),
                    )
                })?;
                (&mut file)
                    .take(cap)
                    .read_to_end(&mut bytes)
                    .map_err(|error| {
                        refuse(self.addon, format!("cannot read artifact file: {error}"))
                    })?;
                self.check_deadline()?;
                if u64::try_from(bytes.len())
                    .map_err(|_| refuse(self.addon, "artifact file size overflow"))?
                    != meta.len()
                    || Stamp::from_metadata(&metadata(self.addon, &file)?) != initial_file
                {
                    return Err(changed(self.addon));
                }
                self.files.insert(relative.clone(), bytes);
                self.stamps.insert(relative, initial_file);
            }
        }
        if Stamp::from_metadata(&metadata(self.addon, directory)?) != initial {
            return Err(changed(self.addon));
        }
        self.directories.insert(prefix.to_owned());
        self.stamps.insert(prefix.to_owned(), initial);
        Ok(())
    }
}

fn content_digest(addon: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"erp-addon-artifact-v1\0");
    digest.update(
        u64::try_from(addon.len())
            .map_err(|_| refuse(addon, "artifact canonical length overflow"))?
            .to_be_bytes(),
    );
    digest.update(addon.as_bytes());
    digest.update(
        u64::try_from(files.len())
            .map_err(|_| refuse(addon, "artifact canonical length overflow"))?
            .to_be_bytes(),
    );
    for (path, bytes) in files {
        digest.update(
            u64::try_from(path.len())
                .map_err(|_| refuse(addon, "artifact canonical length overflow"))?
                .to_be_bytes(),
        );
        digest.update(path.as_bytes());
        digest.update(
            u64::try_from(bytes.len())
                .map_err(|_| refuse(addon, "artifact canonical length overflow"))?
                .to_be_bytes(),
        );
        digest.update(Sha256::digest(bytes));
    }
    let mut result = String::from("addon-v1:");
    for byte in digest.finalize() {
        use std::fmt::Write;
        write!(result, "{byte:02x}")
            .map_err(|error| refuse(addon, format!("cannot encode artifact digest: {error}")))?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]
    use super::*;

    #[test]
    fn mutation_after_open_is_detected_deterministically() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().canonicalize().unwrap();
        let path = root_path.join("records.xml");
        std::fs::write(&path, b"<erp/>\n").unwrap();
        let (root, _) = open_root("note", &root_path).unwrap();
        let mut walk = Walk {
            addon: "note",
            started: Instant::now(),
            entries: 0,
            path_bytes: 0,
            bytes: 0,
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
            stamps: BTreeMap::new(),
            // A different length is detected on every filesystem; a same-length rewrite inside one
            // timestamp tick is not (see the module's detection limit).
            after_file_metadata: Some(Box::new(move || {
                std::fs::write(path, b"<bad-longer/>\n").unwrap();
            })),
        };
        let error = walk.directory(&root, "", 0).unwrap_err();
        match error {
            crate::Error::NodeDigest(NodeError::AddonSource { addon, message }) => {
                assert_eq!(addon, "note");
                assert_eq!(message, "artifact changed during lifecycle operation");
            }
            other => panic!("wrong error kind: {other:?}"),
        }
        assert!(walk.files.is_empty());
    }

    #[test]
    fn same_length_rewrite_with_changed_mtime_is_detected() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().canonicalize().unwrap();
        let path = root_path.join("records.xml");
        std::fs::write(&path, b"<erp/>\n").unwrap();
        let (root, _) = open_root("note", &root_path).unwrap();
        let mut walk = Walk {
            addon: "note",
            started: Instant::now(),
            entries: 0,
            path_bytes: 0,
            bytes: 0,
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
            stamps: BTreeMap::new(),
            // Set the modification time explicitly so the outcome does not depend on the
            // filesystem's timestamp granularity.
            after_file_metadata: Some(Box::new(move || {
                std::fs::write(&path, b"<bad/>\n").unwrap();
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                let later = file.metadata().unwrap().modified().unwrap() + Duration::from_secs(5);
                file.set_modified(later).unwrap();
            })),
        };
        let error = walk.directory(&root, "", 0).unwrap_err();
        match error {
            crate::Error::NodeDigest(NodeError::AddonSource { addon, message }) => {
                assert_eq!(addon, "note");
                assert_eq!(message, "artifact changed during lifecycle operation");
            }
            other => panic!("wrong error kind: {other:?}"),
        }
        assert!(walk.files.is_empty());
    }
}
