//! Streaming staging: SHA-256, SHA-1 and the size computed while the bytes pass, and the first
//! 8 KiB kept for content detection.

use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::key::StagingKey;

/// How many leading bytes are kept for content detection.
pub const DETECT_BYTES: usize = 8192;

/// The size limit of one staging call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageLimits {
    /// The most bytes a stream may carry; one more is `LimitExceeded`.
    pub max_bytes: u64,
}

impl StageLimits {
    /// A limit of `max_bytes`.
    pub const fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

/// What a finished staging call knows about the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedBlob {
    key: StagingKey,
    size: u64,
    sha256: [u8; 32],
    sha1: [u8; 20],
    head: Vec<u8>,
}

impl StagedBlob {
    /// Rebuild what a finished staging call returned from the values its caller stored (the
    /// `ir_blob_upload` row): the head is not kept, so detection is not repeated.
    pub fn from_parts(key: StagingKey, size: u64, sha256: [u8; 32], sha1: [u8; 20]) -> Self {
        Self {
            key,
            size,
            sha256,
            sha1,
            head: Vec::new(),
        }
    }

    /// The staging key the bytes are under.
    pub fn key(&self) -> &StagingKey {
        &self.key
    }

    /// The size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The SHA-256 of the bytes: the content address.
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    pub fn sha1(&self) -> &[u8; 20] {
        &self.sha1
    }

    /// The first [`DETECT_BYTES`] bytes (fewer for a small object).
    pub fn head(&self) -> &[u8] {
        &self.head
    }
}

/// Hashes and counts bytes as they stream by.
pub(crate) struct Hasher {
    sha256: Sha256,
    sha1: Sha1,
    size: u64,
    head: Vec<u8>,
}

impl Hasher {
    pub(crate) fn new() -> Self {
        Self {
            sha256: Sha256::new(),
            sha1: Sha1::new(),
            size: 0,
            head: Vec::new(),
        }
    }

    pub(crate) fn update(&mut self, chunk: &[u8]) {
        self.sha256.update(chunk);
        self.sha1.update(chunk);
        self.size = self.size.saturating_add(chunk.len() as u64);
        let room = DETECT_BYTES.saturating_sub(self.head.len());
        if room > 0 {
            self.head
                .extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or(&[]));
        }
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn finish(self, key: StagingKey) -> StagedBlob {
        StagedBlob {
            key,
            size: self.size,
            sha256: self.sha256.finalize().into(),
            sha1: self.sha1.finalize().into(),
            head: self.head,
        }
    }
}

/// The SHA-256 and SHA-1 of `bytes` in one pass.
pub fn digests(bytes: &[u8]) -> ([u8; 32], [u8; 20]) {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    (
        hasher.sha256.finalize().into(),
        hasher.sha1.finalize().into(),
    )
}

/// The read buffer of [`digest_stream`]: proving an object's bytes holds at most this much of it
/// in memory, whatever its size.
pub const DIGEST_READ_BYTES: usize = 64 * 1024;

/// The SHA-256 and the size of everything `reader` yields, read in [`DIGEST_READ_BYTES`] chunks
/// into one reused buffer (memory bounded by the buffer, not by the object).
///
/// # Errors
///
/// The reader's.
pub async fn digest_stream<R>(reader: &mut R) -> std::io::Result<([u8; 32], u64)>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; DIGEST_READ_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or(&[]));
    }
    let size = hasher.size;
    Ok((hasher.sha256.finalize().into(), size))
}
