// SPDX-License-Identifier: MIT OR Apache-2.0
//! Local primitives only: no database coordinator, peer admission or cleanup activation.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_blob::{
    AttachmentStore, BlobError, LocalStore, Namespace, OwnerIdentity, OwnerMarker, OwnerVersion,
};
use std::io::Write;
use std::process::{Command, Stdio};

fn marker(ns: &Namespace) -> OwnerMarker {
    OwnerMarker::new(
        OwnerIdentity::new(42, 16384, ns.clone()).unwrap(),
        1,
        Namespace::random().unwrap(),
        1234,
    )
    .unwrap()
}

#[tokio::test]
async fn local_owner_reopen_and_aba() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::random().unwrap();
    let first = marker(&ns);
    let store = LocalStore::open(dir.path()).await.unwrap();
    let v1 = store.put_owner(&ns, &first, None).await.unwrap();
    drop(store);
    let store = LocalStore::open(dir.path()).await.unwrap();
    let found = store.owner(&ns).await.unwrap().unwrap();
    assert_eq!(found.marker(), &first);
    assert_eq!(found.version(), &v1);
    let v2 = store.put_owner(&ns, &first, Some(&v1)).await.unwrap();
    assert_ne!(v1, v2);
    let error = store.put_owner(&ns, &first, Some(&v1)).await.unwrap_err();
    assert!(matches!(error, BlobError::OwnerConflict));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker changed or already exists"
    );
    assert_eq!(store.owner(&ns).await.unwrap().unwrap().version(), &v2);
}

#[tokio::test]
async fn local_owner_rejects_damaged_and_symlink_markers() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).await.unwrap();
    let ns = Namespace::random().unwrap();
    let base = dir.path().join(ns.as_str());
    std::fs::create_dir(&base).unwrap();
    for bytes in [vec![], vec![0; 257], vec![0; 100]] {
        std::fs::write(base.join("_owner"), bytes).unwrap();
        let error = store.owner(&ns).await.unwrap_err();
        assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
        assert_eq!(
            error.to_string(),
            "the attachment owner marker is invalid: expected bounded local owner envelope"
        );
    }
    std::fs::remove_file(base.join("_owner")).unwrap();
    let outside = dir.path().join("outside");
    std::fs::write(&outside, b"preserve").unwrap();
    symlink(&outside, base.join("_owner")).unwrap();
    let error = store.owner(&ns).await.unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: local marker must be a regular file without symlinks"
    );
    assert_eq!(std::fs::read(outside).unwrap(), b"preserve");
}

// Invoked only by this test binary's parent fixture; no environment hook enters library code.
#[test]
fn local_owner_child() {
    use std::io::Read;
    let Some(root) = std::env::var_os("ERP_LOCAL_OWNER_CHILD_ROOT") else {
        return;
    };
    let ns = Namespace::parse(&std::env::var("ERP_LOCAL_OWNER_CHILD_NS").unwrap()).unwrap();
    let version = OwnerVersion::new(
        &std::env::var("ERP_LOCAL_OWNER_CHILD_VERSION").unwrap(),
        None,
    )
    .unwrap();
    let mut start = [0];
    std::io::stdin().read_exact(&mut start).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let store = LocalStore::open(root).await.unwrap();
        let next = marker(&ns);
        match store.put_owner(&ns, &next, Some(&version)).await {
            Ok(_) => println!("OWNER_RESULT:OK:{}", next.operation()),
            Err(BlobError::OwnerConflict) => println!("OWNER_RESULT:CONFLICT"),
            Err(error @ BlobError::Unavailable { retryable: true, .. }) => {
                assert!(matches!(error.to_string().as_str(),
                    "the attachment store is unavailable (retryable): local owner guard is busy; retry"
                    | "the attachment store is unavailable (retryable): local owner lease is busy; retry"));
                println!("OWNER_RESULT:BUSY");
            }
            Err(error) => panic!("unexpected child error: {error}"),
        }
    });
}

#[tokio::test]
async fn local_owner_two_process_cas() {
    race_case(false).await;
}

#[tokio::test]
async fn local_owner_two_process_expired_breakers() {
    race_case(true).await;
}

async fn race_case(expired: bool) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).await.unwrap();
    let ns = Namespace::random().unwrap();
    let version = store.put_owner(&ns, &marker(&ns), None).await.unwrap();
    if expired {
        let mut lease = b"ERPLCK01".to_vec();
        lease.extend_from_slice(Namespace::random().unwrap().as_str().as_bytes());
        lease.extend_from_slice(Namespace::random().unwrap().as_str().as_bytes());
        lease.extend_from_slice(&1_u64.to_le_bytes());
        std::fs::write(dir.path().join(ns.as_str()).join(".owner.lock"), lease).unwrap();
    }
    let mut children = (0..2)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "local_owner_child", "--nocapture"])
                .env("ERP_LOCAL_OWNER_CHILD_ROOT", dir.path())
                .env("ERP_LOCAL_OWNER_CHILD_NS", ns.as_str())
                .env("ERP_LOCAL_OWNER_CHILD_VERSION", version.etag())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    for child in &mut children {
        child.stdin.take().unwrap().write_all(b"x").unwrap();
    }
    let mut winners = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let result = stdout
            .lines()
            .find_map(|line| line.strip_prefix("OWNER_RESULT:"))
            .expect(&stdout);
        if let Some(operation) = result.strip_prefix("OK:") {
            winners.push(operation.to_owned());
        } else {
            assert!(matches!(result, "CONFLICT" | "BUSY"), "{result}");
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(
        store
            .owner(&ns)
            .await
            .unwrap()
            .unwrap()
            .marker()
            .operation()
            .as_str(),
        winners[0]
    );
}

#[tokio::test]
async fn local_probe_preserves_existing_namespace_content() {
    let root = tempfile::tempdir().unwrap();
    let store = LocalStore::open(root.path()).await.unwrap();
    let ns = Namespace::random().unwrap();
    let first = marker(&ns);
    let version = store.put_owner(&ns, &first, None).await.unwrap();
    store.create_contested(&ns, &first).await.unwrap();
    let untouched = root.path().join(ns.as_str()).join("keep-content");
    std::fs::write(&untouched, b"application bytes").unwrap();
    store.probe_owner().await.unwrap();
    assert_eq!(std::fs::read(untouched).unwrap(), b"application bytes");
    assert_eq!(store.owner(&ns).await.unwrap().unwrap().version(), &version);
    assert_eq!(store.contested(&ns).await.unwrap(), Some(first));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2); // own namespace and .store-id
}
