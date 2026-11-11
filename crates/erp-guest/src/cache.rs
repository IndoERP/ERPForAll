//! Values a guest keeps across calls, valid only under one `cache-epoch`.
//!
//! The host changes the transaction's `cache-epoch` whenever it writes or flushes. A guest that
//! keeps columns between calls holds them in an [`EpochCache`] and asks for them with the epoch of
//! the current call: under another epoch the cache drops everything it holds and answers empty, so
//! stale columns are never read.

use std::collections::BTreeMap;

/// Entries keyed by `K`, all read under the same `cache-epoch`.
#[derive(Clone, Debug)]
pub struct EpochCache<K, V> {
    epoch: Option<u64>,
    entries: BTreeMap<K, V>,
}

impl<K: Ord, V> Default for EpochCache<K, V> {
    fn default() -> Self {
        Self {
            epoch: None,
            entries: BTreeMap::new(),
        }
    }
}

impl<K: Ord, V> EpochCache<K, V> {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a call under `epoch`: when it differs from the epoch the entries were stored under,
    /// every entry is dropped. Returns whether entries were dropped.
    pub fn begin(&mut self, epoch: u64) -> bool {
        if self.epoch == Some(epoch) {
            return false;
        }
        let dropped = !self.entries.is_empty();
        self.entries.clear();
        self.epoch = Some(epoch);
        dropped
    }

    /// The entry for `key` under `epoch`, dropping the cache first when the epoch changed.
    pub fn get(&mut self, epoch: u64, key: &K) -> Option<&V> {
        self.begin(epoch);
        self.entries.get(key)
    }

    /// Store `value` for `key` under `epoch`, dropping the cache first when the epoch changed.
    pub fn insert(&mut self, epoch: u64, key: K, value: V) {
        self.begin(epoch);
        self.entries.insert(key, value);
    }

    /// The epoch the entries belong to, `None` before the first call.
    pub fn epoch(&self) -> Option<u64> {
        self.epoch
    }

    /// Number of entries held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
