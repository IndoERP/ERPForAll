//! The node registry's addon digest (`AddonIndex::digest`, design §8.4).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use erp_modules::{AddonIndex, Error, Manifest};
use erp_orm::node::{AddonContent, MAX_TREE_FILES, NodeError};
use tempfile::tempdir;

fn manifest(name: &str, version: &str) -> Manifest {
    Manifest::from_toml(
        name,
        &format!("name = \"{name}\"\nversion = \"{version}\"\n"),
    )
    .unwrap()
}

fn dev_addon(root: &Path, name: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("data")).unwrap();
    fs::write(
        dir.join("manifest.toml"),
        format!("name = \"{name}\"\nversion = \"1.0.0\"\n"),
    )
    .unwrap();
    fs::write(dir.join("data/records.xml"), b"<odoo><record/></odoo>\n").unwrap();
    dir
}

fn index(dev_root: &Path, base_version: &str) -> AddonIndex {
    let mut idx = AddonIndex::new(true);
    idx.add_embedded(manifest("base", base_version), "embedded:base")
        .unwrap();
    idx.add_package_with_sha256(
        manifest("pkg_a", "1.0.0"),
        "package:pkg_a.erpaddon",
        erp_orm::node::Digest::from_bytes([7; 32]),
    )
    .unwrap();
    idx.add_dev_path(dev_root);
    idx
}

fn addon_source_message(err: Error) -> String {
    match err {
        Error::NodeDigest(NodeError::AddonSource { message, .. }) => message,
        other => panic!("expected an addon source error, got {other:?}"),
    }
}

#[test]
fn addon_digest_covers_every_local_source_by_content() {
    let dir = tempdir().unwrap();
    let dev = dev_addon(dir.path(), "dev_mod");
    let digest = index(dir.path(), "1.0.0").digest().unwrap();
    // the whole catalogue (nothing is installed here), sorted by name, each with its kind
    let entries: Vec<(&str, &str, AddonContent)> = digest
        .entries()
        .iter()
        .map(|e| (e.name(), e.version(), e.content()))
        .collect();
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries.iter().map(|e| e.0).collect::<Vec<_>>(),
        ["base", "dev_mod", "pkg_a"]
    );
    assert_eq!(entries[0].2, AddonContent::Embedded);
    assert!(matches!(entries[1].2, AddonContent::DevFolder(_)));
    assert_eq!(
        entries[2].2,
        AddonContent::Package(erp_orm::node::Digest::from_bytes([7; 32]))
    );
    // the same sources give the same digest
    assert_eq!(index(dir.path(), "1.0.0").digest().unwrap(), digest);
    // a manifest version is part of it
    assert_ne!(
        index(dir.path(), "1.0.1").digest().unwrap().digest(),
        digest.digest()
    );

    // touching a file's mtime changes nothing; one changed byte does
    let file = dev.join("data/records.xml");
    let later = SystemTime::now() + Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(
        index(dir.path(), "1.0.0").digest().unwrap().digest(),
        digest.digest()
    );
    fs::write(&file, b"<odoo><record/></odoX>\n").unwrap();
    let edited = index(dir.path(), "1.0.0").digest().unwrap();
    assert_ne!(edited.digest(), digest.digest());

    // a package without a recorded SHA-256 is refused
    let mut unrecorded = AddonIndex::new(false);
    unrecorded
        .add_package(manifest("pkg_b", "1.0.0"), "package:pkg_b.erpaddon")
        .unwrap();
    let message = addon_source_message(unrecorded.digest().unwrap_err());
    assert_eq!(
        message,
        "the package package:pkg_b.erpaddon has no recorded SHA-256: register it with \
         AddonIndex::add_package_with_sha256"
    );

    // a symbolic link inside a dev folder is refused
    #[cfg(unix)]
    {
        let linked = tempdir().unwrap();
        let dev = dev_addon(linked.path(), "linked_mod");
        std::os::unix::fs::symlink("/etc/hosts", dev.join("data/hosts")).unwrap();
        let message = addon_source_message(index(linked.path(), "1.0.0").digest().unwrap_err());
        assert_eq!(
            message,
            "data/hosts is a symbolic link: dev folders may hold regular files only"
        );
    }

    // a file with another hard link (reachable from outside the folder) is refused
    #[cfg(unix)]
    {
        let linked = tempdir().unwrap();
        let dev = dev_addon(linked.path(), "hard_mod");
        fs::hard_link(
            dev.join("data/records.xml"),
            linked.path().join("outside.xml"),
        )
        .unwrap();
        let message = addon_source_message(index(linked.path(), "1.0.0").digest().unwrap_err());
        assert_eq!(
            message,
            "data/records.xml has 2 hard links: dev folders may hold files with one link only"
        );
    }

    // a folder over the entry bound (directories count too) is refused
    let wide = tempdir().unwrap();
    let dev = dev_addon(wide.path(), "wide_mod");
    let dirs = dev.join("dirs");
    fs::create_dir(&dirs).unwrap();
    for i in 0..40_000 {
        fs::create_dir(dirs.join(format!("d{i}"))).unwrap();
    }
    let message = addon_source_message(index(wide.path(), "1.0.0").digest().unwrap_err());
    assert_eq!(
        message,
        "the dev folder holds more than 40000 entries (files and directories)"
    );

    // a folder over the file-count bound is refused
    let big = tempdir().unwrap();
    let dev = dev_addon(big.path(), "big_mod");
    let many = dev.join("many");
    fs::create_dir(&many).unwrap();
    // manifest.toml and data/records.xml are two files already
    for i in 0..MAX_TREE_FILES {
        fs::write(many.join(format!("f{i}")), b"").unwrap();
    }
    let message = addon_source_message(index(big.path(), "1.0.0").digest().unwrap_err());
    assert_eq!(
        message,
        format!("the dev folder holds more than {MAX_TREE_FILES} files")
    );
}

/// Not a spec case: the readiness lock never collides with the module system's locks.
#[test]
fn readiness_lock_is_distinct_from_the_module_locks() {
    use erp_modules::system::{LOCK_CLASS, OPERATION_LOCK, POST_COMMIT_LOCK};
    use erp_orm::node::READINESS_LOCK;
    assert_eq!(erp_orm::node::LOCK_CLASS, LOCK_CLASS);
    assert_eq!(READINESS_LOCK.0, LOCK_CLASS);
    assert_ne!(READINESS_LOCK, OPERATION_LOCK);
    assert_ne!(READINESS_LOCK, POST_COMMIT_LOCK);
}
