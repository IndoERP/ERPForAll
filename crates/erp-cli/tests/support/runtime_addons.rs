//! Prepared private runtime addon trees for native CLI fixtures.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use erp_modules::{AddonIndex, AddonSourceKind, artifact_identity::ArtifactSnapshot};

/// Fixture preparation failure. Artifact admission retains the production error and its source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeAddonError {
    #[error("runtime addon fixture IO at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("runtime addon fixture directory is a symbolic link: {0}")]
    SymbolicLink(PathBuf),
    #[error("runtime addon fixture path is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("runtime addon fixture path requires a final directory name: {0}")]
    MissingName(PathBuf),
    #[error("runtime addon fixture discovered a non-folder source for {0}")]
    NonFolder(String),
    #[error("runtime addon fixture receipt differs after staging {0}")]
    ReceiptMismatch(String),
    #[error(transparent)]
    Artifact(#[from] erp_modules::Error),
}

fn io_at(path: &Path, source: io::Error) -> RuntimeAddonError {
    RuntimeAddonError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Resolve a caller-owned parent prefix, such as macOS's /var temporary-directory alias.
/// Do not resolve the final entry: a search directory or addon root that is a symlink refuses.
fn physical_path(path: &Path) -> Result<PathBuf, RuntimeAddonError> {
    let name = path
        .file_name()
        .ok_or_else(|| RuntimeAddonError::MissingName(path.to_owned()))?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        // A bare fixture directory name is relative to the process's current directory.
        Some(_) | None => Path::new("."),
    };
    Ok(fs::canonicalize(parent)
        .map_err(|error| io_at(parent, error))?
        .join(name))
}

fn physical_directory(path: &Path) -> Result<PathBuf, RuntimeAddonError> {
    let physical = physical_path(path)?;
    let metadata = fs::symlink_metadata(&physical).map_err(|error| io_at(&physical, error))?;
    if metadata.file_type().is_symlink() {
        return Err(RuntimeAddonError::SymbolicLink(physical));
    }
    if !metadata.is_dir() {
        return Err(RuntimeAddonError::NotDirectory(physical));
    }
    Ok(physical)
}

fn private_directory(path: &Path) -> Result<(), RuntimeAddonError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|error| io_at(path, error))
}

/// Stage every addon in a fixture's repository search directory as prepared runtime bytes.
///
/// Public production snapshots select the bytes, reject unsafe links and verify the original
/// files did not change during copying. Snapshot receipts must match after staging. In
/// particular, tooling descendants excluded by that production selector are never reopened or
/// copied here. The node digest and its whole-catalogue policy are deliberately unchanged.
/// The caller owns the source and the destination's parent; `destination` must not exist.
///
/// # Errors
/// Invalid discovery or artifact input, unsafe roots, source mutation, IO or receipt mismatch.
/// Partial output stays in the caller's private temporary tree and must never be activated
/// after an error; fixture callers use this result before launching any server.
pub fn stage_runtime_addons(source: &Path, destination: &Path) -> Result<(), RuntimeAddonError> {
    let source = physical_directory(source)?;
    // Refuse addon-entry links before discovery can read a manifest through one. Descendant
    // admission remains entirely owned by ArtifactSnapshot, including excluded-entry checks.
    for entry in fs::read_dir(&source).map_err(|error| io_at(&source, error))? {
        let entry = entry.map_err(|error| io_at(&source, error))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| io_at(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(RuntimeAddonError::SymbolicLink(path));
        }
    }
    let mut index = AddonIndex::new(true);
    index.add_dev_path(&source);
    let entries = index.discover_entries()?;
    let destination = physical_path(destination)?;
    private_directory(&destination)?;
    for (name, entry) in entries {
        let AddonSourceKind::DevFolder(root) = entry.source_kind() else {
            return Err(RuntimeAddonError::NonFolder(name.to_string()));
        };
        let root = physical_directory(root)?;
        let snapshot = ArtifactSnapshot::capture(name.as_str(), &root)?;
        let target = destination.join(name.as_str());
        private_directory(&target)?;
        for relative in snapshot.directories() {
            if !relative.is_empty() {
                private_directory(&target.join(relative))?;
            }
        }
        for (relative, bytes) in snapshot.files() {
            let path = target.join(relative);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path).map_err(|error| io_at(&path, error))?;
            file.write_all(bytes).map_err(|error| io_at(&path, error))?;
        }
        snapshot.verify_unchanged()?;
        let staged = ArtifactSnapshot::capture(name.as_str(), &target)?;
        if staged.digest() != snapshot.digest() {
            return Err(RuntimeAddonError::ReceiptMismatch(name.to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "fixture assertions"
)]
mod tests {
    use super::*;

    fn source_tree(root: &Path) -> PathBuf {
        let source = root.join("source");
        let addon = source.join("note");
        fs::create_dir_all(addon.join("static")).unwrap();
        fs::create_dir_all(addon.join("data")).unwrap();
        fs::write(
            addon.join("manifest.toml"),
            "name='note'\nversion='1.0.0'\n",
        )
        .unwrap();
        fs::write(
            addon.join("static/runtime.js"),
            b"export const value = 1;\n",
        )
        .unwrap();
        fs::write(addon.join("data/records.xml"), b"<odoo/>\n").unwrap();
        fs::write(addon.join("static/icon.bin"), [0, 255, 1, 254]).unwrap();
        source
    }

    #[test]
    fn prepared_runtime_addons_copy_admitted_bytes_and_ignore_tooling_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let source = source_tree(directory.path());
        let addon = source.join("note");
        let spare = source.join("spare");
        fs::create_dir(&spare).unwrap();
        fs::write(
            spare.join("manifest.toml"),
            "name='spare'\nversion='1.0.0'\n",
        )
        .unwrap();
        fs::write(spare.join("unlisted-runtime.txt"), "whole local catalogue").unwrap();
        let cache = addon.join("static/node_modules");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("tooling.js"), "not deployed").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(addon.join("data/records.xml"), cache.join("vitest")).unwrap();
        fs::write(addon.join(".DS_Store"), "desktop metadata").unwrap();
        let target = directory.path().join("runtime-addons");
        stage_runtime_addons(&source, &target).unwrap();
        assert_eq!(
            fs::read(target.join("note/static/runtime.js")).unwrap(),
            b"export const value = 1;\n"
        );
        assert_eq!(
            fs::read(target.join("note/static/icon.bin")).unwrap(),
            [0, 255, 1, 254]
        );
        assert_eq!(
            fs::read(target.join("note/data/records.xml")).unwrap(),
            b"<odoo/>\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("spare/unlisted-runtime.txt")).unwrap(),
            "whole local catalogue"
        );
        assert!(!target.join("note/static/node_modules").exists());
        assert!(!target.join("note/.DS_Store").exists());
        let source =
            ArtifactSnapshot::capture("note", &physical_directory(&source).unwrap().join("note"))
                .unwrap();
        let destination =
            ArtifactSnapshot::capture("note", &physical_directory(&target).unwrap().join("note"))
                .unwrap();
        assert_eq!(source.digest(), destination.digest());
        let mut index = AddonIndex::new(true);
        index.add_dev_path(&target);
        let identity = index.digest().unwrap();
        assert_eq!(identity.entries().len(), 2);
        assert_eq!(identity.entries()[0].name(), "note");
        assert_eq!(identity.entries()[1].name(), "spare");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(target.join("note/static/runtime.js"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepared_runtime_addons_refuse_link_at_excluded_entry_itself() {
        let directory = tempfile::tempdir().unwrap();
        let source = source_tree(directory.path());
        std::os::unix::fs::symlink(
            source.join("note/data"),
            source.join("note/static/node_modules"),
        )
        .unwrap();
        let error =
            stage_runtime_addons(&source, &directory.path().join("runtime-addons")).unwrap_err();
        match error {
            RuntimeAddonError::Artifact(erp_modules::Error::NodeDigest(
                erp_orm::node::NodeError::AddonSource { addon, message },
            )) => {
                assert_eq!(addon, "note");
                assert_eq!(message, "symbolic link in artifact path");
            }
            other => panic!("expected snapshot link refusal, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepared_runtime_addons_refuse_linked_addon_root() {
        let directory = tempfile::tempdir().unwrap();
        let source = source_tree(directory.path());
        let moved = directory.path().join("actual-note");
        fs::rename(source.join("note"), &moved).unwrap();
        std::os::unix::fs::symlink(&moved, source.join("note")).unwrap();
        let error =
            stage_runtime_addons(&source, &directory.path().join("runtime-addons")).unwrap_err();
        let expected = physical_directory(&source).unwrap().join("note");
        assert_eq!(
            error.to_string(),
            format!(
                "runtime addon fixture directory is a symbolic link: {}",
                expected.display()
            )
        );
        assert!(matches!(error, RuntimeAddonError::SymbolicLink(_)));
    }
}
