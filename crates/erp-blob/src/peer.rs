// SPDX-License-Identifier: MIT OR Apache-2.0
//! Two-participant local-filesystem probe, distinct from ownership and deletion authority.
use crate::BlobError;
/// Fixed number of version-1 peer phases.
pub const PEER_STEPS: u8 = 16;
/// Protocol identity; changing phase semantics requires a new version.
pub const PEER_PROTOCOL: i16 = 1;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use crate::local::peer::LocalPeer;

pub(crate) fn refused(reason: &'static str) -> BlobError {
    BlobError::PeerProbe { reason }
}

/// Verify both complete primitive transcripts. This alone is not durable operator admission.
/// # Errors
/// `PeerProbe` for missing phases, multiple create winners, inconsistent readback or failed checks.
pub fn verify_transcript(a: &[u8], b: &[u8]) -> Result<(), BlobError> {
    if a.len() != usize::from(PEER_STEPS) || b.len() != usize::from(PEER_STEPS) {
        return Err(refused("two complete transcripts are required"));
    }
    if !matches!((a[2], b[2]), (1, 0) | (0, 1)) {
        return Err(refused(
            "exclusive creation did not have exactly one winner",
        ));
    }
    let winner = if a[2] == 1 { 1 } else { 2 };
    if a[3] != winner || b[3] != winner {
        return Err(refused("race readback disagrees"));
    }
    for i in 0..usize::from(PEER_STEPS) {
        if i != 2 && i != 3 && (a[i] != 1 || b[i] != 1) {
            return Err(refused("a required primitive check failed"));
        }
    }
    Ok(())
}

/// One actual local primitive result; only the live probe can construct it.
#[derive(Debug)]
#[non_exhaustive]
pub struct PeerObservation {
    pub(crate) session: crate::Namespace,
    pub(crate) slot: u8,
    pub(crate) phase: u8,
    pub(crate) value: u8,
    pub(crate) mount: String,
}
impl PeerObservation {
    /// Private probe session identity.
    pub fn session(&self) -> &crate::Namespace {
        &self.session
    }
    /// Participant index, 0 or 1.
    pub fn slot(&self) -> u8 {
        self.slot
    }
    /// Completed phase index.
    pub fn phase(&self) -> u8 {
        self.phase
    }
    /// Bounded primitive result used in the transcript.
    pub fn value(&self) -> u8 {
        self.value
    }
    /// Locally observed root/mount binding.
    pub fn mount(&self) -> &str {
        &self.mount
    }
}
/// Proof that the completed creator removed its own private artifacts.
#[derive(Debug)]
#[non_exhaustive]
pub struct PeerCleanup {
    pub(crate) session: crate::Namespace,
    pub(crate) mount: String,
}
impl PeerCleanup {
    /// Current root/mount binding observed after cleanup.
    pub fn mount(&self) -> &str {
        &self.mount
    }
    /// The cleaned private session.
    pub fn session(&self) -> &crate::Namespace {
        &self.session
    }
}
