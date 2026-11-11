//! Installed addon artifact admission. No database or compiled-code provenance is claimed here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_modules::{Error, artifact_identity::ArtifactSnapshot};
use erp_orm::node::NodeError;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("note");
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(
        root.join("manifest.toml"),
        b"name = 'note'\nversion = '1.0.0'\n",
    )
    .unwrap();
    fs::write(root.join("data/records.xml"), b"<erp/>\n").unwrap();
    (temp, root)
}

fn message(error: Error) -> String {
    match error {
        Error::NodeDigest(NodeError::AddonSource { addon, message }) => {
            assert_eq!(addon, "note");
            message
        }
        other => panic!("wrong error kind: {other:?}"),
    }
}

#[test]
fn canonical_digest_uses_file_receipts_and_snapshot_bytes() {
    let (_temp, root) = fixture();
    let snapshot = ArtifactSnapshot::capture("note", &root).unwrap();
    let mut canonical = Sha256::new();
    canonical.update(b"erp-addon-artifact-v1\0");
    canonical.update(4_u64.to_be_bytes());
    canonical.update(b"note");
    canonical.update(2_u64.to_be_bytes());
    for (path, bytes) in snapshot.files() {
        canonical.update(u64::try_from(path.len()).unwrap().to_be_bytes());
        canonical.update(path.as_bytes());
        canonical.update(u64::try_from(bytes.len()).unwrap().to_be_bytes());
        canonical.update(Sha256::digest(bytes));
    }
    assert_eq!(
        snapshot.digest(),
        format!(
            "addon-v1:{}",
            canonical
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    );
    assert_eq!(snapshot.files()["data/records.xml"], b"<erp/>\n");
    assert_eq!(
        snapshot
            .directories()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["", "data"]
    );
    snapshot.verify_unchanged().unwrap();
    fs::write(root.join("data/records.xml"), b"<bad/>\n").unwrap();
    assert_eq!(snapshot.files()["data/records.xml"], b"<erp/>\n");
    assert_eq!(
        message(snapshot.verify_unchanged().unwrap_err()),
        "artifact changed during lifecycle operation"
    );
    assert_ne!(
        snapshot.digest(),
        ArtifactSnapshot::capture("note", &root).unwrap().digest()
    );
}

#[test]
fn copied_tree_has_equal_digest_but_in_operation_replacement_refuses() {
    let (_temp, root) = fixture();
    let snapshot = ArtifactSnapshot::capture("note", &root).unwrap();
    let copied = root.with_file_name("copy");
    fs::create_dir_all(copied.join("data")).unwrap();
    for (path, bytes) in snapshot.files() {
        fs::write(copied.join(path), bytes).unwrap();
    }
    assert_eq!(
        snapshot.digest(),
        ArtifactSnapshot::capture("note", &copied).unwrap().digest()
    );
    let saved = root.with_file_name("saved");
    fs::rename(&root, &saved).unwrap();
    fs::rename(&copied, &root).unwrap();
    assert_eq!(
        message(snapshot.verify_unchanged().unwrap_err()),
        "artifact changed during lifecycle operation"
    );
}

#[cfg(unix)]
fn symlink(from: impl AsRef<Path>, to: impl AsRef<Path>) {
    std::os::unix::fs::symlink(from, to).unwrap();
}

#[cfg(unix)]
#[test]
fn rejects_symlink_root_ancestors_and_descendants() {
    let (_temp, root) = fixture();
    let linked = root.with_file_name("linked");
    symlink(&root, &linked);
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &linked).unwrap_err()),
        "symbolic link in artifact path"
    );
    let parent = root.parent().unwrap();
    let linked_parent = parent.join("parent-link");
    symlink(parent, &linked_parent);
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &linked_parent.join("note")).unwrap_err()),
        "symbolic link in artifact path"
    );
    symlink("records.xml", root.join("data/link.xml"));
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &root).unwrap_err()),
        "symbolic link in artifact path"
    );
}

#[test]
fn rejects_hard_link_alias() {
    let (_temp, root) = fixture();
    fs::hard_link(root.join("data/records.xml"), root.join("alias.xml")).unwrap();
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &root).unwrap_err()),
        "artifact file has multiple hard links"
    );
}

#[test]
fn replacing_file_with_identical_bytes_is_refused_within_operation() {
    let (_temp, root) = fixture();
    let snapshot = ArtifactSnapshot::capture("note", &root).unwrap();
    let path = root.join("data/records.xml");
    fs::write(
        root.join("replacement"),
        &snapshot.files()["data/records.xml"],
    )
    .unwrap();
    fs::rename(root.join("replacement"), &path).unwrap();
    assert_eq!(
        message(snapshot.verify_unchanged().unwrap_err()),
        "artifact changed during lifecycle operation"
    );
}

#[test]
fn changed_file_set_refuses_verification() {
    let (_temp, root) = fixture();
    let snapshot = ArtifactSnapshot::capture("note", &root).unwrap();
    fs::write(root.join("extra"), b"").unwrap();
    assert_eq!(
        message(snapshot.verify_unchanged().unwrap_err()),
        "artifact changed during lifecycle operation"
    );
    fs::remove_file(root.join("extra")).unwrap();
    fs::remove_file(root.join("data/records.xml")).unwrap();
    assert_eq!(
        message(snapshot.verify_unchanged().unwrap_err()),
        "artifact changed during lifecycle operation"
    );
}

#[test]
fn invalid_input_refuses() {
    let (_temp, root) = fixture();
    let invalid = ArtifactSnapshot::capture("../note", &root).unwrap_err();
    assert!(matches!(&invalid, Error::InvalidModuleName { .. }));
    assert_eq!(
        invalid.to_string(),
        "Invalid module name '../note': must contain only ASCII alphanumeric characters or underscores"
    );
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &root.join("..")).unwrap_err()),
        "artifact path must not contain parent components"
    );
}

#[test]
fn deep_tree_refuses_at_next_level() {
    let (_temp, root) = fixture();
    let mut nested = root.clone();
    for _ in 0..32 {
        nested = nested.join("d");
        fs::create_dir(&nested).unwrap();
    }
    ArtifactSnapshot::capture("note", &root).unwrap();
    fs::create_dir(nested.join("d")).unwrap();
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &root).unwrap_err()),
        "artifact depth exceeds 32"
    );
}

#[test]
fn byte_limit_refuses_before_unbounded_read() {
    let (_temp, root) = fixture();
    let occupied: u64 = ArtifactSnapshot::capture("note", &root)
        .unwrap()
        .files()
        .values()
        .map(|bytes| u64::try_from(bytes.len()).unwrap())
        .sum();
    let large = fs::File::create(root.join("large")).unwrap();
    let accepted_size = erp_modules::artifact_identity::MAX_ARTIFACT_BYTES - occupied;
    large.set_len(accepted_size).unwrap();
    let accepted = ArtifactSnapshot::capture("note", &root).unwrap();
    assert_eq!(
        accepted
            .files()
            .values()
            .map(|bytes| u64::try_from(bytes.len()).unwrap())
            .sum::<u64>(),
        erp_modules::artifact_identity::MAX_ARTIFACT_BYTES
    );
    drop(accepted);
    large.set_len(accepted_size + 1).unwrap();
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &root).unwrap_err()),
        "artifact bytes exceed 67108864"
    );
}

#[cfg(unix)]
#[test]
fn empty_directory_change_and_symlink_swap_refuse() {
    let (_temp, root) = fixture();
    let snapshot = ArtifactSnapshot::capture("note", &root).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    assert_eq!(
        message(snapshot.verify_unchanged().unwrap_err()),
        "artifact changed during lifecycle operation"
    );
    let snapshot = ArtifactSnapshot::capture("note", &root).unwrap();
    let path = root.join("data/records.xml");
    fs::remove_file(&path).unwrap();
    symlink("../manifest.toml", &path);
    assert_eq!(
        message(snapshot.verify_unchanged().unwrap_err()),
        "symbolic link in artifact path"
    );
}

#[test]
fn file_count_bound_and_next_entry() {
    let (_temp, root) = fixture();
    for index in 0..erp_modules::artifact_identity::MAX_ARTIFACT_FILES - 2 {
        fs::write(root.join(format!("f{index:05}")), b"").unwrap();
    }
    let accepted = ArtifactSnapshot::capture("note", &root).unwrap();
    assert_eq!(
        accepted.files().len(),
        erp_modules::artifact_identity::MAX_ARTIFACT_FILES
    );
    fs::write(root.join("one-more"), b"").unwrap();
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &root).unwrap_err()),
        "artifact files exceed 20000"
    );
}

#[test]
fn entry_count_bound_and_next_entry() {
    let (_temp, root) = fixture();
    // The fixture already has data/, data/records.xml and manifest.toml.
    for index in 0..erp_modules::artifact_identity::MAX_ARTIFACT_ENTRIES - 3 {
        fs::create_dir(root.join(format!("d{index:05}"))).unwrap();
    }
    let accepted = ArtifactSnapshot::capture("note", &root).unwrap();
    assert_eq!(
        accepted.files().len() + accepted.directories().len() - 1,
        erp_modules::artifact_identity::MAX_ARTIFACT_ENTRIES
    );
    fs::create_dir(root.join("one-more")).unwrap();
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &root).unwrap_err()),
        "artifact entries exceed 40000"
    );
}

#[test]
fn tooling_cache_is_excluded_without_following_its_links() {
    let (_temp, root) = fixture();
    let before = ArtifactSnapshot::capture("note", &root)
        .unwrap()
        .digest()
        .to_owned();
    let cache = root.join("static/node_modules");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("tooling.js"), "not a runtime artifact").unwrap();
    symlink(root.join("data/records.xml"), cache.join("linked-tool"));
    fs::create_dir_all(root.join("target")).unwrap();
    fs::write(root.join("target/build-output"), "not shipped").unwrap();
    fs::create_dir_all(root.join("__pycache__")).unwrap();
    fs::write(root.join("__pycache__/module.pyc"), "not shipped").unwrap();
    fs::write(root.join(".git"), "worktree metadata").unwrap();
    fs::write(root.join(".DS_Store"), "desktop metadata").unwrap();
    let snapshot = ArtifactSnapshot::capture("note", &root).unwrap();
    assert_eq!(snapshot.digest(), before);
    assert!(
        !snapshot
            .files()
            .keys()
            .any(|path| path.contains("node_modules"))
    );
    fs::write(cache.join("another.js"), "cache mutation").unwrap();
    snapshot.verify_unchanged().unwrap();
    fs::remove_dir_all(&cache).unwrap();
    symlink(root.join("data"), &cache);
    assert_eq!(
        message(ArtifactSnapshot::capture("note", &root).unwrap_err()),
        "symbolic link in artifact path"
    );
}
