//! The addon digest of the node registry (design §8.4): every addon source
//! this process holds locally, so replicas with different addons never serve one database side by
//! side.

use std::path::Path;

use erp_orm::node::{
    AddonContent, AddonsDigest, Digest, MAX_TREE_DEPTH, MAX_TREE_ENTRIES, MAX_TREE_FILES,
    NodeError, TreeHasher,
};
use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};

use crate::discovery::{AddonIndex, AddonSourceKind};
use crate::error::Result;

impl AddonIndex {
    /// The digest of the whole local catalogue (not only the installed modules): per addon its
    /// name, manifest version, source kind and content — nothing for an embedded addon (the build
    /// digest covers it), the recorded SHA-256 of a package
    /// ([`add_package_with_sha256`](Self::add_package_with_sha256)), the content of a dev folder
    /// (every regular file: relative path, size and bytes; at most 20,000 files, 40,000 entries,
    /// 1 GiB and depth 32; each file with one hard link). Dev folders count only in `--dev` mode,
    /// as in [`discover_entries`](Self::discover_entries).
    ///
    /// # Errors
    ///
    /// The errors of [`discover_entries`](Self::discover_entries); [`crate::Error::NodeDigest`]
    /// with [`NodeError::AddonSource`] for a package without a recorded SHA-256, a symbolic link,
    /// a non-UTF-8 name, a special file or a file with another hard link in a dev folder, a
    /// folder over its bounds, or a file that cannot be read or changes while it is read.
    pub fn digest(&mut self) -> Result<AddonsDigest> {
        let entries = self.discover_entries()?;
        let mut builder = AddonsDigest::builder();
        for (name, entry) in &entries {
            let addon = name.as_str();
            let content = match entry.source_kind() {
                AddonSourceKind::Embedded => AddonContent::Embedded,
                AddonSourceKind::Package { .. } => match self.package_sha256(name) {
                    Some(sha256) => AddonContent::Package(sha256),
                    None => {
                        return Err(NodeError::AddonSource {
                            addon: addon.to_owned(),
                            message: format!(
                                "the package {} has no recorded SHA-256: register it with \
                                 AddonIndex::add_package_with_sha256",
                                entry.source_location()
                            ),
                        }
                        .into());
                    }
                },
                AddonSourceKind::DevFolder(path) => {
                    AddonContent::DevFolder(tree_digest(addon, path)?)
                }
            };
            builder = builder.addon(addon, &entry.manifest().version().to_string(), content)?;
        }
        Ok(builder.build()?)
    }
}

fn refuse(addon: &str, message: String) -> NodeError {
    NodeError::AddonSource {
        addon: addon.to_owned(),
        message,
    }
}

/// The content digest of a dev folder, walked by directory descriptor: every directory below the
/// root is opened relative to its parent with `O_NOFOLLOW | O_DIRECTORY` and every file with
/// `O_NOFOLLOW`, so an entry swapped for a symbolic link while the walk runs fails it instead of
/// leading outside the folder. Files are hashed in bytewise order of their relative paths: each
/// directory's entries are visited sorted by name, a directory keyed by its name and `/`.
fn tree_digest(addon: &str, root: &Path) -> std::result::Result<Digest, NodeError> {
    let dir = rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| refuse(addon, format!("cannot list .: {e}")))?;
    let mut walk = Walk {
        addon,
        hasher: TreeHasher::new(addon),
        files: 0,
        entries: 0,
    };
    walk.dir(&dir, "", 0)?;
    Ok(walk.hasher.finish())
}

/// One dev-folder walk and its counts.
struct Walk<'a> {
    addon: &'a str,
    hasher: TreeHasher,
    files: u64,
    entries: u64,
}

impl Walk<'_> {
    /// The entries of the directory `dir` at `prefix` (relative, `/` separators).
    fn dir(
        &mut self,
        dir: &OwnedFd,
        prefix: &str,
        depth: usize,
    ) -> std::result::Result<(), NodeError> {
        let addon = self.addon;
        if depth > MAX_TREE_DEPTH {
            return Err(refuse(
                addon,
                format!("the dev folder is nested deeper than {MAX_TREE_DEPTH} levels at {prefix}"),
            ));
        }
        let shown = if prefix.is_empty() { "." } else { prefix };
        let listed = |e: rustix::io::Errno| refuse(addon, format!("cannot list {shown}: {e}"));
        let mut names: Vec<(String, bool)> = Vec::new();
        for entry in Dir::read_from(dir).map_err(listed)? {
            let entry = entry.map_err(listed)?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let Ok(name) = std::str::from_utf8(name) else {
                return Err(refuse(
                    addon,
                    format!(
                        "{shown} holds a file whose name is not UTF-8: {:?}",
                        String::from_utf8_lossy(name)
                    ),
                ));
            };
            self.entries += 1;
            if self.entries > MAX_TREE_ENTRIES {
                return Err(refuse(
                    addon,
                    format!(
                        "the dev folder holds more than {MAX_TREE_ENTRIES} entries (files and \
                         directories)"
                    ),
                ));
            }
            let relative = join(prefix, name);
            let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|e| refuse(addon, format!("cannot read {relative}: {e}")))?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Symlink => {
                    return Err(refuse(
                        addon,
                        format!(
                            "{relative} is a symbolic link: dev folders may hold regular files \
                             only"
                        ),
                    ));
                }
                FileType::Directory => names.push((name.to_owned(), true)),
                FileType::RegularFile => names.push((name.to_owned(), false)),
                _ => {
                    return Err(refuse(
                        addon,
                        format!("{relative} is not a regular file or a directory"),
                    ));
                }
            }
        }
        // bytewise order of the relative paths below this directory
        names.sort_by_key(key);
        for (name, is_dir) in names {
            let relative = join(prefix, &name);
            if is_dir {
                let sub = rustix::fs::openat(
                    dir,
                    name.as_str(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| refuse(addon, format!("cannot list {relative}: {e}")))?;
                self.dir(&sub, &relative, depth + 1)?;
            } else {
                self.file(dir, &name, &relative)?;
            }
        }
        Ok(())
    }

    /// One regular file: opened without following a link, one hard link only, then hashed.
    fn file(
        &mut self,
        dir: &OwnedFd,
        name: &str,
        relative: &str,
    ) -> std::result::Result<(), NodeError> {
        let addon = self.addon;
        self.files += 1;
        if self.files > MAX_TREE_FILES {
            return Err(refuse(
                addon,
                format!("the dev folder holds more than {MAX_TREE_FILES} files"),
            ));
        }
        let fd = rustix::fs::openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| refuse(addon, format!("cannot open {relative}: {e}")))?;
        let stat = rustix::fs::fstat(&fd)
            .map_err(|e| refuse(addon, format!("cannot read {relative}: {e}")))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(refuse(
                addon,
                format!("{relative} is no longer a regular file"),
            ));
        }
        // a second link makes the file reachable (and changeable) from outside the folder
        #[allow(
            clippy::useless_conversion,
            reason = "st_nlink is u64 on Linux and u16 on macOS"
        )]
        let links = u64::from(stat.st_nlink);
        if links != 1 {
            return Err(refuse(
                addon,
                format!(
                    "{relative} has {links} hard links: dev folders may hold files with one link only"
                ),
            ));
        }
        let size = u64::try_from(stat.st_size)
            .map_err(|_| refuse(addon, format!("{relative} reports a negative size")))?;
        let mut file = std::fs::File::from(fd);
        self.hasher.file(relative, size, &mut file)
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// The sort key of an entry: a directory sorts as its name and `/`, where its files' paths
/// continue.
fn key((name, is_dir): &(String, bool)) -> Vec<u8> {
    let mut key = name.as_bytes().to_vec();
    if *is_dir {
        key.push(b'/');
    }
    key
}
