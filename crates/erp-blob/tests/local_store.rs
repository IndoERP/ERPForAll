//! Behavior only the directory store has: fingerprint, permissions, damaged state.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_blob::{AttachmentStore, LocalStore, Namespace};

mod common;
use common::*;

#[tokio::test]
async fn the_store_id_is_stable_and_another_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let first = LocalStore::open(dir.path().join("a")).await.unwrap();
    let again = LocalStore::open(dir.path().join("a")).await.unwrap();
    let other = LocalStore::open(dir.path().join("b")).await.unwrap();
    let fingerprint = first.fingerprint().unwrap();
    assert_eq!(again.fingerprint().unwrap(), fingerprint);
    assert_ne!(other.fingerprint().unwrap(), fingerprint);
    first.verify_fingerprint(&fingerprint).await.unwrap();
    let err = other.verify_fingerprint(&fingerprint).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "the attachment store is unavailable (not retryable): the store directory is not the one \
         this database registered (its .store-id differs)"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn a_damaged_store_id_fails_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    drop(LocalStore::open(&root).await.unwrap());
    let id_path = root.join(".store-id");
    // the file is 0440: replace it by name
    std::fs::remove_file(&id_path).unwrap();
    std::fs::write(&id_path, "not an id").unwrap();
    let err = LocalStore::open(&root).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "attachment store configuration: the store directory has a damaged .store-id"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn directories_are_0700_and_files_0440() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path().join("s")).await.unwrap();
    let ns = Namespace::random().unwrap();
    let staged = stage_bytes(&store, &ns, 1, b"secret").await;
    let key = key_of(&ns, &staged);
    store.promote(&staged, &key).await.unwrap();
    let file = store.root().join(key.path());
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o440);
    assert_eq!(mode(file.parent().unwrap()), 0o700);
    assert_eq!(mode(store.root()), 0o700);
    assert_eq!(mode(&store.root().join(".store-id")), 0o440);
}

#[tokio::test]
async fn promote_leaves_one_name_and_no_part_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path().join("s")).await.unwrap();
    let ns = Namespace::random().unwrap();
    let staged = stage_bytes(&store, &ns, 1, b"one name").await;
    let key = key_of(&ns, &staged);
    store.promote(&staged, &key).await.unwrap();
    let staging = store
        .list(&format!("{}/staging", ns.as_str()), None, 10)
        .await
        .unwrap();
    assert!(staging.is_empty(), "{staging:?}");
}

#[tokio::test]
async fn a_listing_prefix_cannot_leave_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path().join("s")).await.unwrap();
    for prefix in ["", "../x", "a/../b", "/etc"] {
        assert!(store.list(prefix, None, 10).await.is_err(), "{prefix:?}");
    }
}
