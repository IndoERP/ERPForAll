// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_blob::{
    AttachmentStore, BlobError, LocalStore, MemoryStore, Namespace, ObjectBackend, OwnerIdentity,
    OwnerMarker,
};
use std::sync::Arc;
fn marker(g: u64) -> OwnerMarker {
    OwnerMarker::new(
        OwnerIdentity::new(42, 12, Namespace::random().unwrap()).unwrap(),
        g,
        Namespace::random().unwrap(),
        100,
    )
    .unwrap()
}
fn conflict(e: BlobError) {
    assert!(matches!(e, BlobError::OwnerConflict));
    assert_eq!(
        e.to_string(),
        "the attachment owner marker changed or already exists"
    );
}
async fn contract(store: &dyn AttachmentStore) {
    let ns = Namespace::random().unwrap();
    let old = marker(1);
    let version = store.put_owner(&ns, &old, None).await.unwrap();
    store.create_contested(&ns, &old).await.unwrap();
    let contest = store.contest_state(&ns).await.unwrap().unwrap();
    assert!(!contest.resolved());
    let chosen = marker(2);
    store.put_owner(&ns, &chosen, Some(&version)).await.unwrap();
    let owner = store.owner(&ns).await.unwrap().unwrap();
    let receipt = store
        .resolve_contested(&ns, &owner, Some(&contest))
        .await
        .unwrap();
    assert!(receipt.resolved());
    assert_eq!(receipt.marker(), &chosen);
    assert_eq!(store.contested(&ns).await.unwrap(), None);
    assert_eq!(
        store
            .resolve_contested(&ns, &owner, Some(&contest))
            .await
            .unwrap(),
        receipt
    );
    store.create_contested(&ns, &old).await.unwrap();
    let fresh = store.contest_state(&ns).await.unwrap().unwrap();
    assert_ne!(fresh.version(), contest.version());
    conflict(
        store
            .resolve_contested(&ns, &owner, Some(&contest))
            .await
            .unwrap_err(),
    );
    assert_eq!(store.contested(&ns).await.unwrap(), Some(old));
    let third = marker(3);
    store
        .put_owner(&ns, &third, Some(owner.version()))
        .await
        .unwrap();
    conflict(
        store
            .resolve_contested(&ns, &owner, Some(&fresh))
            .await
            .unwrap_err(),
    );
    assert!(!store.contest_state(&ns).await.unwrap().unwrap().resolved());
}
#[tokio::test]
async fn memory_recovery_contract() {
    contract(&MemoryStore::new()).await;
}
#[tokio::test]
async fn object_recovery_contract() {
    contract(
        &ObjectBackend::open(Arc::new(object_store::memory::InMemory::new()), "recovery")
            .await
            .unwrap(),
    )
    .await;
}

#[tokio::test]
async fn object_contest_republication_changes_content() {
    use object_store::{ObjectStoreExt, path::Path};
    let inner = Arc::new(object_store::memory::InMemory::new());
    let store = ObjectBackend::open(inner.clone(), "recovery")
        .await
        .unwrap();
    let ns = Namespace::random().unwrap();
    let chosen = marker(1);
    store.put_owner(&ns, &chosen, None).await.unwrap();
    store.create_contested(&ns, &chosen).await.unwrap();
    let path = Path::from(format!("recovery/{ns}/_contested"));
    let before = inner.get(&path).await.unwrap().bytes().await.unwrap();
    let contest = store.contest_state(&ns).await.unwrap().unwrap();
    let owner = store.owner(&ns).await.unwrap().unwrap();
    store
        .resolve_contested(&ns, &owner, Some(&contest))
        .await
        .unwrap();
    store.create_contested(&ns, &chosen).await.unwrap();
    let after = inner.get(&path).await.unwrap().bytes().await.unwrap();
    assert_ne!(
        before, after,
        "republishing the same loser must invalidate content-based validators"
    );
    conflict(
        store
            .resolve_contested(&ns, &owner, Some(&contest))
            .await
            .unwrap_err(),
    );
}
#[tokio::test]
async fn object_contest_envelope_validation_preserves_legacy_reads() {
    use object_store::{ObjectStoreExt, PutPayload, path::Path};
    let inner = Arc::new(object_store::memory::InMemory::new());
    let store = ObjectBackend::open(inner.clone(), "recovery")
        .await
        .unwrap();
    let ns = Namespace::random().unwrap();
    let chosen = marker(1);
    let path = Path::from(format!("recovery/{ns}/_contested"));
    inner
        .put(&path, PutPayload::from(chosen.encode()))
        .await
        .unwrap();
    let legacy = store.contest_state(&ns).await.unwrap().unwrap();
    assert_eq!(legacy.marker(), &chosen);
    assert!(!legacy.resolved());
    let mut malformed = b"ERPCAS01".to_vec();
    malformed.extend([b'x'; 36]);
    malformed.extend(chosen.encode());
    for bytes in [b"ERPCAS01".to_vec(), malformed] {
        inner.put(&path, PutPayload::from(bytes)).await.unwrap();
        let e = store.contest_state(&ns).await.unwrap_err();
        assert!(matches!(e, BlobError::InvalidOwnerMarker { .. }));
        assert_eq!(
            e.to_string(),
            "the attachment owner marker is invalid: invalid object contest envelope"
        );
    }
}

#[tokio::test]
async fn local_recovery_contract() {
    let dir = tempfile::tempdir().unwrap();
    contract(&LocalStore::open(dir.path()).await.unwrap()).await;
}

// This hook exists only in the test process, never in the library.
#[test]
fn resolution_child() {
    let Some(root) = std::env::var_os("ERP_RECOVERY_CHILD_ROOT") else {
        return;
    };
    let ns = Namespace::parse(&std::env::var("ERP_RECOVERY_CHILD_NS").unwrap()).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let store = LocalStore::open(root).await.unwrap();
        let owner = store.owner(&ns).await.unwrap().unwrap();
        let contest = store.contest_state(&ns).await.unwrap().unwrap();
        store
            .resolve_contested(&ns, &owner, Some(&contest))
            .await
            .unwrap();
    });
    std::process::exit(73); // Lose the external reply after durable publication, without test unwinding.
}
#[tokio::test]
async fn local_resolution_process_exit_preserves_recovery_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::random().unwrap();
    let store = LocalStore::open(dir.path()).await.unwrap();
    let chosen = marker(1);
    store.put_owner(&ns, &chosen, None).await.unwrap();
    store.create_contested(&ns, &chosen).await.unwrap();
    let before = store.contest_state(&ns).await.unwrap().unwrap();
    let owner = store.owner(&ns).await.unwrap().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resolution_child", "--nocapture"])
        .env("ERP_RECOVERY_CHILD_ROOT", dir.path())
        .env("ERP_RECOVERY_CHILD_NS", ns.as_str())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    let reopened = LocalStore::open(dir.path()).await.unwrap();
    let receipt = reopened.contest_state(&ns).await.unwrap().unwrap();
    assert!(receipt.resolved());
    assert_eq!(
        reopened
            .resolve_contested(&ns, &owner, Some(&before))
            .await
            .unwrap(),
        receipt
    );
    assert!(dir.path().join(ns.as_str()).join(".owner.guard").exists());
}

#[test]
fn resolution_codec_rejects_unknown_truncated_and_oversized_records() {
    use erp_blob::{ContestState, OwnerVersion};
    for bytes in [
        vec![],
        vec![0; 257],
        b"ERPRSL99".to_vec(),
        b"ERPRSL01".to_vec(),
    ] {
        let e =
            ContestState::decode(&bytes, OwnerVersion::new("opaque", None).unwrap()).unwrap_err();
        assert!(matches!(e, BlobError::InvalidOwnerMarker { .. }));
        assert_eq!(
            e.to_string(),
            "the attachment owner marker is invalid: expected bounded contest or resolution record"
        );
    }
}
