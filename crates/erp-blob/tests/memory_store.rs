//! The fault injection of the memory store, which the garbage collection race test relies on.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_blob::{AttachmentStore, BlobError, MemoryStore, Namespace, Op, When};

mod common;
use common::*;

#[tokio::test]
async fn a_fault_before_the_effect_changes_nothing() {
    let store = MemoryStore::new();
    let ns = Namespace::random().unwrap();
    let staged = stage_bytes(&store, &ns, 1, b"x").await;
    let key = key_of(&ns, &staged);
    store.faults().inject(Op::Promote, When::Before, 0, 1);
    let err = store.promote(&staged, &key).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "the attachment store is unavailable (retryable): injected fault in Promote (Before its effect)"
    );
    assert!(store.content(&key).is_none());
    // the next call goes through
    store.promote(&staged, &key).await.unwrap();
    assert_eq!(store.content(&key).unwrap(), b"x");
}

#[tokio::test]
async fn a_fault_after_the_effect_keeps_the_effect() {
    let store = MemoryStore::new();
    let ns = Namespace::random().unwrap();
    let staged = stage_bytes(&store, &ns, 1, b"y").await;
    let key = key_of(&ns, &staged);
    store.promote(&staged, &key).await.unwrap();
    store.faults().inject(Op::DeleteMany, When::After, 0, 1);
    let err = store
        .delete_many(std::slice::from_ref(&key))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        BlobError::Unavailable {
            retryable: true,
            ..
        }
    ));
    assert!(
        store.content(&key).is_none(),
        "the bytes are gone, the reply was lost"
    );
    // the retry reports the key already absent
    let report = store.delete_many(&[key]).await.unwrap();
    assert_eq!((report.deleted, report.already_absent), (0, 1));
}

#[tokio::test]
async fn skip_lets_calls_through_first() {
    let store = MemoryStore::new();
    let ns = Namespace::random().unwrap();
    store.faults().inject(Op::Stage, When::Before, 1, 1);
    let _first = stage_bytes(&store, &ns, 1, b"a").await;
    let key = erp_blob::StagingKey::new(ns, &upload_id(2)).unwrap();
    let mut body: &[u8] = b"b";
    assert!(
        store
            .stage(&key, &mut body, erp_blob::StageLimits::new(10))
            .await
            .is_err()
    );
}
