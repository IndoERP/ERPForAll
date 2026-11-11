//! Compiled-component cache keyed by `(sha256, WIT version, cpu)` (plan §8).
//!
//! The digest identifies the addon bytes, the contract version the host linked them against, and the
//! cpu fingerprint the machine code was compiled for (a precompiled artefact is only valid for the
//! engine configuration and cpu that produced it). The cache is per process; a persistent copy keyed
//! the same way belongs to the package store.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::error::{HostError, LimitKind};
use crate::limits::Limits;

/// Cache key of one compiled component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleKey {
    sha256: [u8; 32],
    wit_version: String,
    cpu: String,
}

/// `Hasher` over SHA-256, so the cpu fingerprint is stable across Rust versions (a persistent copy of
/// the cache,, uses the same key).
struct ShaHasher(Sha256);

impl std::hash::Hasher for ShaHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        let digest = self.0.clone().finalize();
        let mut head = [0u8; 8];
        head.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(head)
    }
}

impl ModuleKey {
    /// SHA-256 of the component bytes.
    #[must_use]
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// WIT contract version the component is linked against.
    #[must_use]
    pub fn wit_version(&self) -> &str {
        &self.wit_version
    }

    /// Fingerprint of the engine configuration and cpu the code is compiled for.
    #[must_use]
    pub fn cpu(&self) -> &str {
        &self.cpu
    }

    /// Key of `bytes` for `engine` under the host's current contract version.
    #[must_use]
    pub fn for_bytes(engine: &Engine, bytes: &[u8]) -> Self {
        Self::with_version(engine, bytes, crate::WIT_VERSION)
    }

    /// Key of `bytes` for an explicit contract version.
    #[must_use]
    pub fn with_version(engine: &Engine, bytes: &[u8], wit_version: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = ShaHasher(Sha256::new());
        engine.precompile_compatibility_hash().hash(&mut hasher);
        Self {
            sha256: Sha256::digest(bytes).into(),
            wit_version: wit_version.to_owned(),
            cpu: format!("{}-{:016x}", std::env::consts::ARCH, hasher.finish()),
        }
    }
}

/// Thread-safe cache of compiled components.
#[derive(Debug, Default)]
pub struct ModuleCache {
    entries: Mutex<HashMap<ModuleKey, Component>>,
    compilations: AtomicU64,
}

impl ModuleCache {
    /// The component for `key`, compiled from `bytes` on first use. `bytes` must be the bytes `key`
    /// was derived from; the key is recomputed and a mismatch is an error, so a caller cannot store
    /// other code under a trusted digest.
    pub fn get_or_compile(
        &self,
        engine: &Engine,
        key: &ModuleKey,
        bytes: &[u8],
    ) -> Result<Component, HostError> {
        if bytes.len() > Limits::MAX_PAYLOAD_BYTES {
            return Err(HostError::LimitExceeded {
                limit: LimitKind::PayloadBytes,
                value: bytes.len() as u64,
                max: Limits::MAX_PAYLOAD_BYTES as u64,
            });
        }
        let actual: [u8; 32] = Sha256::digest(bytes).into();
        if actual != key.sha256 {
            return Err(HostError::DigestMismatch);
        }
        // The lock is held while compiling: concurrent requests for the same new component compile it
        // once. Compilation is a rare, load-time event; a bounded or sharded cache is for.
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = entries.get(key) {
            return Ok(found.clone());
        }
        let component =
            Component::new(engine, bytes).map_err(|e| HostError::Compile(format!("{e:#}")))?;
        self.compilations.fetch_add(1, Ordering::Relaxed);
        entries.insert(key.clone(), component.clone());
        Ok(component)
    }

    /// Number of compilations performed (cache misses that succeeded).
    #[must_use]
    pub fn compilations(&self) -> u64 {
        self.compilations.load(Ordering::Relaxed)
    }
}
