// SPDX-License-Identifier: MIT OR Apache-2.0
//! Partial ownership-fence coverage: storage primitives only, no database/job admission.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_blob::{
    AttachmentStore, BlobError, LocalStore, MemoryStore, Namespace, ObjectBackend, Op,
    OwnerIdentity, OwnerMarker, OwnerVersion, When,
};
use futures_util::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, path::Path};

fn marker(generation: u64) -> OwnerMarker {
    OwnerMarker::new(
        OwnerIdentity::new(42, 16384, Namespace::random().unwrap()).unwrap(),
        generation,
        Namespace::random().unwrap(),
        1234,
    )
    .unwrap()
}

fn conflict(error: BlobError) {
    assert!(matches!(error, BlobError::OwnerConflict));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker changed or already exists"
    );
}

async fn contract(store: &dyn AttachmentStore) {
    store.probe_owner().await.unwrap();
    let ns = Namespace::random().unwrap();
    let foreign = Namespace::random().unwrap();
    let first = marker(1);
    let next = marker(2);
    assert!(store.owner(&ns).await.unwrap().is_none());
    assert!(store.contested(&ns).await.unwrap().is_none());
    let version = store.put_owner(&ns, &first, None).await.unwrap();
    let found = store.owner(&ns).await.unwrap().unwrap();
    assert_eq!(found.marker(), &first);
    assert_eq!(found.version(), &version);
    store.check_owner(&ns, &version).await.unwrap();
    conflict(store.put_owner(&ns, &next, None).await.unwrap_err());
    assert_eq!(store.owner(&ns).await.unwrap().unwrap().marker(), &first);
    let newer = store.put_owner(&ns, &next, Some(&version)).await.unwrap();
    assert_ne!(newer, version);
    conflict(store.check_owner(&ns, &version).await.unwrap_err());
    conflict(
        store
            .put_owner(&ns, &first, Some(&version))
            .await
            .unwrap_err(),
    );
    assert_eq!(store.owner(&ns).await.unwrap().unwrap().marker(), &next);
    store.check_owner(&ns, &newer).await.unwrap();
    conflict(store.check_owner(&foreign, &newer).await.unwrap_err());
    assert!(store.owner(&foreign).await.unwrap().is_none());
    store.create_contested(&ns, &first).await.unwrap();
    store.create_contested(&ns, &next).await.unwrap();
    assert_eq!(store.contested(&ns).await.unwrap(), Some(first));
    // Storage primitives do not pretend a conditional HEAD proves absent contest.
    // The coordinator must check both, in each destructive chunk.
    store.check_owner(&ns, &newer).await.unwrap();
}

#[tokio::test]
async fn memory_owner_contract() {
    let store = MemoryStore::new();
    store.probe_owner().await.unwrap();
    assert!(store.list("", None, 100).await.unwrap().is_empty());
    contract(&store).await;
}

#[tokio::test]
async fn object_owner_contract() {
    let inner = Arc::new(object_store::memory::InMemory::new());
    let store = ObjectBackend::open(inner.clone(), "tenant").await.unwrap();
    store.probe_owner().await.unwrap();
    assert_eq!(
        inner
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        1
    );
    contract(&store).await;
}

#[test]
fn marker_encoding_is_bounded_strict_and_round_trips() {
    let first = marker(u64::MAX);
    assert_eq!(OwnerMarker::decode(&first.encode()).unwrap(), first);
    assert_eq!(first.generation(), u64::MAX);
    assert_eq!(first.observed_at(), 1234);
    assert_eq!(first.identity().system_identifier(), 42);
    assert_eq!(first.identity().database_oid(), 16384);
    assert_eq!(first.identity().incarnation().as_str().len(), 36);
    assert_eq!(first.operation().as_str().len(), 36);
    for input in [vec![], vec![0; 257], b"v9|ignored".to_vec(), vec![255]] {
        let error = OwnerMarker::decode(&input).unwrap_err();
        assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
        assert_eq!(
            error.to_string(),
            "the attachment owner marker is invalid: expected bounded canonical v1 record"
        );
    }
    let mut extra = first.encode();
    extra.extend_from_slice(b"extra");
    let error = OwnerMarker::decode(&extra).unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: expected bounded canonical v1 record"
    );
    let error =
        OwnerMarker::new(first.identity().clone(), 0, first.operation().clone(), 0).unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: generation must be positive"
    );
    let error = OwnerIdentity::new(0, 1, Namespace::random().unwrap()).unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: database identity must be positive"
    );
}

#[test]
fn versions_reject_wildcards_lists_and_header_injection() {
    for value in ["", "*", "first,second", "x\r\ny", "x y", "é"] {
        let error = OwnerVersion::new(value, None).unwrap_err();
        assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
        assert_eq!(
            error.to_string(),
            "the attachment owner marker is invalid: expected one bounded opaque version token"
        );
    }
    let version = OwnerVersion::new("\"opaque\"", Some("version-1")).unwrap();
    assert_eq!(version.etag(), "\"opaque\"");
    assert_eq!(version.object_version(), Some("version-1"));
    for value in ["".to_owned(), "a".repeat(1025)] {
        let error = OwnerVersion::new("etag", Some(&value)).unwrap_err();
        assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
        assert_eq!(
            error.to_string(),
            "the attachment owner marker is invalid: expected one bounded opaque version token"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn local_owner_contract() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).await.unwrap();
    contract(&store).await;
}

#[tokio::test]
async fn lost_reply_keeps_the_owner_operation_recoverable() {
    for when in [When::Before, When::After] {
        let store = MemoryStore::new();
        let ns = Namespace::random().unwrap();
        let first = marker(1);
        store.faults().inject(Op::PutOwner, when, 0, 1);
        let error = store.put_owner(&ns, &first, None).await.unwrap_err();
        assert!(matches!(
            error,
            BlobError::Unavailable {
                retryable: true,
                ..
            }
        ));
        assert_eq!(
            error.to_string(),
            format!(
                "the attachment store is unavailable (retryable): injected fault in PutOwner ({when:?} its effect)"
            )
        );
        assert_eq!(
            store
                .owner(&ns)
                .await
                .unwrap()
                .map(|state| state.marker().clone()),
            if when == When::After {
                Some(first)
            } else {
                None
            }
        );
    }
}

#[tokio::test]
async fn failed_contest_creation_is_observable_and_retry_preserves_the_first_loser() {
    for when in [When::Before, When::After] {
        let store = MemoryStore::new();
        let ns = Namespace::random().unwrap();
        let first = marker(1);
        store.faults().inject(Op::CreateContested, when, 0, 1);
        let error = store.create_contested(&ns, &first).await.unwrap_err();
        assert!(matches!(
            error,
            BlobError::Unavailable {
                retryable: true,
                ..
            }
        ));
        assert_eq!(
            error.to_string(),
            format!(
                "the attachment store is unavailable (retryable): injected fault in CreateContested ({when:?} its effect)"
            )
        );
        assert_eq!(
            store.contested(&ns).await.unwrap(),
            if when == When::After {
                Some(first.clone())
            } else {
                None
            }
        );
        store.create_contested(&ns, &first).await.unwrap();
        store.create_contested(&ns, &marker(2)).await.unwrap();
        assert_eq!(store.contested(&ns).await.unwrap(), Some(first));
    }
}

#[tokio::test]
async fn probe_failures_and_cleanup_failures_are_not_capability_success() {
    let store = MemoryStore::new();
    assert!(store.capabilities().if_match_put);
    store.faults().inject(Op::PutOwner, When::After, 0, 1);
    store.faults().inject(Op::ProbeCleanup, When::Before, 0, 1);
    let error = store.probe_owner().await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "the attachment owner capability probe could not remove its private markers"
    );
    match error {
        BlobError::OwnerProbeCleanup {
            probe: Some(probe),
            cleanup,
        } => {
            assert!(matches!(
                *probe,
                BlobError::Unavailable {
                    retryable: true,
                    ..
                }
            ));
            assert_eq!(
                probe.to_string(),
                "the attachment store is unavailable (retryable): injected fault in PutOwner (After its effect)"
            );
            assert!(matches!(
                *cleanup,
                BlobError::Unavailable {
                    retryable: true,
                    ..
                }
            ));
            assert_eq!(
                cleanup.to_string(),
                "the attachment store is unavailable (retryable): injected fault in ProbeCleanup (Before its effect)"
            );
        }
        other => panic!("unexpected error: {other}"),
    }
    // Only that probe's markers remain; another probe cannot erase its predecessor's state.
    let remaining = store.list("", None, 100).await.unwrap();
    assert_eq!(remaining.len(), 1);
    store.probe_owner().await.unwrap();
    assert_eq!(store.list("", None, 100).await.unwrap(), remaining);
}

async fn concurrent_updates(store: Arc<dyn AttachmentStore>) {
    let ns = Namespace::random().unwrap();
    let initial = marker(1);
    let version = store.put_owner(&ns, &initial, None).await.unwrap();
    let mut workers = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        let ns = ns.clone();
        let version = version.clone();
        workers.push(tokio::spawn(async move {
            let next = marker(2);
            (
                next.clone(),
                store.put_owner(&ns, &next, Some(&version)).await,
            )
        }));
    }
    let mut winner = None;
    for worker in workers {
        let (next, result) = worker.await.unwrap();
        match result {
            Ok(version) => {
                assert!(winner.replace((next, version)).is_none());
            }
            Err(error) => conflict(error),
        }
    }
    let (next, version) = winner.unwrap();
    let found = store.owner(&ns).await.unwrap().unwrap();
    assert_eq!(found.marker(), &next);
    assert_eq!(found.version(), &version);
}

#[tokio::test]
async fn concurrent_memory_cas_has_one_winner() {
    concurrent_updates(Arc::new(MemoryStore::new())).await;
}

#[tokio::test]
async fn concurrent_object_cas_has_one_winner() {
    let inner = Arc::new(object_store::memory::InMemory::new());
    concurrent_updates(Arc::new(
        ObjectBackend::open(inner, "tenant").await.unwrap(),
    ))
    .await;
}

#[tokio::test]
async fn object_marker_reads_refuse_corruption_without_echoing_payload() {
    let inner = Arc::new(object_store::memory::InMemory::new());
    let store = ObjectBackend::open(inner.clone(), "tenant").await.unwrap();
    let ns = Namespace::random().unwrap();
    for suffix in ["_owner", "_contested"] {
        let path = Path::from(format!("tenant/{ns}/{suffix}"));
        inner
            .put(&path, PutPayload::from(vec![b'x'; 257]))
            .await
            .unwrap();
        let error = if suffix == "_owner" {
            store.owner(&ns).await.unwrap_err()
        } else {
            store.contested(&ns).await.unwrap_err()
        };
        assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
        assert_eq!(
            error.to_string(),
            "the attachment owner marker is invalid: expected bounded canonical v1 record"
        );
    }
}
