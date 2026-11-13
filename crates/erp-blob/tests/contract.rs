//! The behavior every store kind must have: one body of checks, run against
//! the local and the memory store.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_blob::{
    AttachmentStore, BlobError, ByteRange, ContentKey, Namespace, StageLimits, StagingKey, to_hex,
};
use tokio::io::AsyncReadExt;

mod common;
use common::*;

/// `reports_absent`: the store can tell an absent key from a removed one (S3 deletes are
/// idempotent and cannot, so every key counts as removed).
async fn contract(store: &dyn AttachmentStore, reports_absent: bool) {
    let ns = Namespace::random().unwrap();

    // stage: digests, size and the detection head come from one pass
    let staged = stage_bytes(store, &ns, 1, b"abc").await;
    assert_eq!(to_hex(staged.sha256()), SHA256_ABC);
    assert_eq!(to_hex(staged.sha1()), SHA1_ABC);
    assert_eq!(staged.size(), 3);
    assert_eq!(staged.head(), b"abc");

    // a missing key is NotFound with its full message, never empty content
    let key = key_of(&ns, &staged);
    let missing = store.get(&key, None).await.unwrap_err();
    assert!(matches!(missing, BlobError::NotFound { .. }));
    assert_eq!(
        missing.to_string(),
        format!("the store has no object for key {}", key.path())
    );
    assert!(matches!(
        store.head(&key).await.unwrap_err(),
        BlobError::NotFound { .. }
    ));

    // promote makes the bytes readable; promoting again (a retry) is accepted
    store.promote(&staged, &key).await.unwrap();
    assert_eq!(read_all(store, &key).await, b"abc");
    assert_eq!(store.head(&key).await.unwrap().size, 3);
    store.promote(&staged, &key).await.unwrap();

    // an existing key is never overwritten: other bytes under the same key are an integrity error
    let other = stage_bytes(store, &ns, 2, b"abcd").await;
    let err = store.promote(&other, &key).await.unwrap_err();
    assert!(matches!(err, BlobError::Integrity { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        format!(
            "object {} failed its integrity check: expected 4 bytes, found 3 bytes",
            key.path()
        )
    );
    assert_eq!(read_all(store, &key).await, b"abc");

    // Publication must retain the durable intent's staging source for fenced maintenance.
    let staged_names = store
        .list(&format!("{}/staging", ns.as_str()), None, 100)
        .await
        .unwrap();
    assert!(
        staged_names.contains(&staged.key().path()),
        "promotion removed its staging source: {staged_names:?}"
    );

    // another namespace is refused
    let foreign = Namespace::random().unwrap();
    let other_ns_key = ContentKey::new(foreign.clone(), *other.sha256());
    let err = store.promote(&other, &other_ns_key).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("namespace {foreign} is not owned by this database")
    );

    // ranges
    let mut range = store.get(&key, Some(ByteRange::new(1, 2))).await.unwrap();
    assert_eq!(range.total_size, 3);
    let mut out = Vec::new();
    range.reader.read_to_end(&mut out).await.unwrap();
    assert_eq!(out, b"bc");
    for bad in [
        ByteRange::new(2, 2),
        ByteRange::new(3, 1),
        ByteRange::new(0, 0),
    ] {
        let err = store.get(&key, Some(bad)).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "the byte range is outside the object of 3 bytes"
        );
    }

    // The size limit stops the stream. Its durable intent owns any retained partial.
    let limited = StagingKey::new(ns.clone(), &upload_id(3)).unwrap();
    let mut body: &[u8] = &[7u8; 100];
    let err = store
        .stage(&limited, &mut body, StageLimits::new(99))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "object size in bytes exceeds the limit of 99"
    );
    let staging_prefix = format!("{}/staging", ns.as_str());
    let left = store.list(&staging_prefix, None, 100).await.unwrap();
    if store.kind() == erp_blob::StoreKind::Local {
        assert!(left.iter().any(|name| name.contains(&upload_id(3))));
    }
    store.delete_staged(&limited).await.unwrap();
    let left = store.list(&staging_prefix, None, 100).await.unwrap();
    assert!(
        left.iter().all(|name| !name.contains(&upload_id(3))),
        "{left:?}"
    );

    // delete_many counts removed and absent keys; an absent key is not an error
    let report = store
        .delete_many(&[key.clone(), key.clone()])
        .await
        .unwrap();
    if reports_absent {
        assert_eq!((report.deleted, report.already_absent), (1, 1));
    } else {
        assert_eq!(report.deleted + report.already_absent, 2);
    }
    assert!(matches!(
        store.get(&key, None).await.unwrap_err(),
        BlobError::NotFound { .. }
    ));

    // a staging object left by a failed link is removed, and removing it twice is fine
    let leftover = stage_bytes(store, &ns, 4, b"left over").await;
    store.delete_staged(leftover.key()).await.unwrap();
    store.delete_staged(leftover.key()).await.unwrap();
    // promoting after the staging object and the key are both gone is NotFound, naming the staging key
    let gone = ContentKey::new(ns.clone(), *leftover.sha256());
    let err = store.promote(&leftover, &gone).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("the store has no object for key {}", leftover.key().path())
    );

    // list: ordered, after, limit
    let mut keys = Vec::new();
    for n in 10..14u8 {
        let staged = stage_bytes(store, &ns, n, &[n]).await;
        let key = key_of(&ns, &staged);
        store.promote(&staged, &key).await.unwrap();
        keys.push(key.path());
    }
    keys.sort();
    let prefix = format!("{}/sha256", ns.as_str());
    let all = store.list(&prefix, None, 100).await.unwrap();
    assert_eq!(all, keys);
    let page = store.list(&prefix, Some(&keys[0]), 2).await.unwrap();
    assert_eq!(page, keys[1..3]);
}

/// Counts the largest buffer a reader is asked to fill, so a test sees the memory bound of a
/// streaming consumer.
struct CountingReader<R> {
    inner: R,
    largest_request: usize,
    reads: usize,
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = &mut *self;
        this.largest_request = this.largest_request.max(buf.remaining());
        this.reads += 1;
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

/// A stored object larger than one read buffer is proved by streaming: the digest and size match,
/// and no read asks for more than `DIGEST_READ_BYTES`.
async fn proves_large_objects_by_streaming(store: &dyn AttachmentStore) {
    let ns = Namespace::random().unwrap();
    let bytes: Vec<u8> = (0..(3 * erp_blob::DIGEST_READ_BYTES + 123))
        .map(|i| (i % 251) as u8)
        .collect();
    let staged = stage_bytes(store, &ns, 9, &bytes).await;
    let key = key_of(&ns, &staged);
    store.promote(&staged, &key).await.unwrap();
    let read = store.get(&key, None).await.unwrap();
    let mut counting = CountingReader {
        inner: read.reader,
        largest_request: 0,
        reads: 0,
    };
    let (sha256, size) = erp_blob::digest_stream(&mut counting).await.unwrap();
    assert_eq!(sha256, erp_blob::digests(&bytes).0);
    assert_eq!(sha256, *staged.sha256());
    assert_eq!(size, bytes.len() as u64);
    assert!(counting.reads >= 4, "{} reads", counting.reads);
    assert_eq!(counting.largest_request, erp_blob::DIGEST_READ_BYTES);
}

#[tokio::test]
async fn local_and_memory_stores_prove_large_objects_by_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let local = erp_blob::LocalStore::open(dir.path().join("store"))
        .await
        .unwrap();
    proves_large_objects_by_streaming(&local).await;
    proves_large_objects_by_streaming(&erp_blob::MemoryStore::new()).await;
}

#[tokio::test]
async fn local_store_follows_the_contract() {
    let dir = tempfile::tempdir().unwrap();
    let store = erp_blob::LocalStore::open(dir.path().join("store"))
        .await
        .unwrap();
    contract(&store, true).await;
}

#[tokio::test]
async fn memory_store_follows_the_contract() {
    contract(&erp_blob::MemoryStore::new(), true).await;
}

#[tokio::test]
async fn object_backend_follows_the_contract() {
    // the S3 backend over object_store's in-memory implementation
    let inner = std::sync::Arc::new(object_store::memory::InMemory::new());
    let store = erp_blob::ObjectBackend::open(inner, "erp/test")
        .await
        .unwrap();
    contract(&store, false).await;
}

#[tokio::test]
async fn object_backend_streams_in_parts_and_nodes_share_one_bucket() {
    use erp_blob::{AttachmentStore, ObjectBackend};
    let inner: std::sync::Arc<dyn object_store::ObjectStore> =
        std::sync::Arc::new(object_store::memory::InMemory::new());
    // two backends over the same bucket and prefix are two nodes: one writes, the other reads
    let node_a = ObjectBackend::open(std::sync::Arc::clone(&inner), "shared")
        .await
        .unwrap()
        .with_part_size_for_tests(1024);
    let node_b = ObjectBackend::open(inner, "shared").await.unwrap();
    assert_eq!(node_a.fingerprint(), node_b.fingerprint());
    let ns = erp_blob::Namespace::random().unwrap();
    // 10,000 bytes cross the 1 KiB part size ten times: the multipart path, memory of one part
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let key = erp_blob::StagingKey::new(ns.clone(), &upload_id(1)).unwrap();
    let mut body: &[u8] = &payload;
    let staged = node_a
        .stage(&key, &mut body, erp_blob::StageLimits::new(1 << 20))
        .await
        .unwrap();
    assert_eq!(staged.size(), 10_000);
    let content = erp_blob::ContentKey::new(ns, *staged.sha256());
    node_a.promote(&staged, &content).await.unwrap();
    assert_eq!(
        read_all(&node_b, &content).await,
        payload,
        "another node reads what this node wrote"
    );
    // The multipart upload that hits the size limit is aborted; successful promotion retained
    // its earlier source until the cleaner explicitly removes it.
    node_a.delete_staged(staged.key()).await.unwrap();
    let key2 = erp_blob::StagingKey::new(content.namespace().clone(), &upload_id(2)).unwrap();
    let mut body: &[u8] = &payload;
    let err = node_a
        .stage(&key2, &mut body, erp_blob::StageLimits::new(5_000))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "object size in bytes exceeds the limit of 5000"
    );
    let left = node_b
        .list(&format!("{}/staging", content.namespace()), None, 10)
        .await
        .unwrap();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_bucket_prefix_that_is_not_the_registered_one_is_refused() {
    use erp_blob::{AttachmentStore, ObjectBackend};
    let inner: std::sync::Arc<dyn object_store::ObjectStore> =
        std::sync::Arc::new(object_store::memory::InMemory::new());
    let first = ObjectBackend::open(std::sync::Arc::clone(&inner), "one")
        .await
        .unwrap();
    let other = ObjectBackend::open(inner, "two").await.unwrap();
    let err = other
        .verify_fingerprint(&first.fingerprint().unwrap())
        .await
        .unwrap_err();
    assert!(!err.is_retryable());
    assert!(
        ObjectBackend::open(
            std::sync::Arc::new(object_store::memory::InMemory::new()),
            "/bad"
        )
        .await
        .is_err()
    );
}
