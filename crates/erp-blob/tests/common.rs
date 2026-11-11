//! Shared helpers of the store contract tests.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_blob::{AttachmentStore, ContentKey, Namespace, StageLimits, StagedBlob, StagingKey};
use tokio::io::AsyncReadExt;

pub const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
pub const SHA1_ABC: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";

pub fn upload_id(n: u8) -> String {
    format!("018f4f6e-7a3c-7c3e-8a9b-0123456789{n:02x}")
}

pub async fn stage_bytes(
    store: &dyn AttachmentStore,
    ns: &Namespace,
    n: u8,
    bytes: &[u8],
) -> StagedBlob {
    let key = StagingKey::new(ns.clone(), &upload_id(n)).unwrap();
    let mut body = bytes;
    store
        .stage(&key, &mut body, StageLimits::new(1 << 20))
        .await
        .unwrap()
}

pub fn key_of(ns: &Namespace, staged: &StagedBlob) -> ContentKey {
    ContentKey::new(ns.clone(), *staged.sha256())
}

pub async fn read_all(store: &dyn AttachmentStore, key: &ContentKey) -> Vec<u8> {
    let mut read = store.get(key, None).await.unwrap();
    let mut out = Vec::new();
    read.reader.read_to_end(&mut out).await.unwrap();
    out
}
