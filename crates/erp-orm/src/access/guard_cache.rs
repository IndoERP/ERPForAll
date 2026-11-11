//! The resolved-plan cache.
//!
//! Resolving a [`GuardPlan`]'s symbolic group references to ids is a pure function of the plan and
//! one policy snapshot, so the result is cached per `(database, registry generation, snapshot,
//! plan digest)`. The cache is **not authoritative**: a wrong eviction costs a re-resolution, never
//! a wrong answer. It is bounded in bytes and in entries (both configurable within a range, and
//! rejected loudly outside it), evicts least-recently-used entries until a new one fits, counts
//! what it skips, and drops every older entry of a database as soon as a transaction pinned to a
//! newer registry generation or snapshot asks.
//!
//! A hit compares the complete plan, not only the digest. A plan too large for the whole budget is
//! resolved without being stored (`skipped`), and a lookup from an older generation than the
//! newest one seen for its database is resolved without being stored (`stale`), so an in-flight
//! transaction pinned to an old snapshot cannot repopulate what was just dropped.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use erp_fields::{Error, Result};
use rustc_hash::FxHashMap;

use super::guard::{GuardPlan, ResolvedPlan};
use crate::principal::SnapshotId;

/// Bounds of a [`ResolvedGuardCache`]. Chosen bounds, not measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedGuardCacheConfig {
    bytes: usize,
    entries: usize,
}

impl ResolvedGuardCacheConfig {
    /// Default byte budget: 4 MiB.
    pub const DEFAULT_BYTES: usize = 4 << 20;
    /// Smallest byte budget: 1 MiB.
    pub const MIN_BYTES: usize = 1 << 20;
    /// Largest byte budget: 64 MiB.
    pub const MAX_BYTES: usize = 64 << 20;
    /// Default entry cap.
    pub const DEFAULT_ENTRIES: usize = 4_096;
    /// Largest entry cap.
    pub const MAX_ENTRIES: usize = 65_536;

    /// A configuration; `bytes` from 1 MiB to 64 MiB, `entries` from 1 to 65,536.
    ///
    /// # Errors
    ///
    /// `Programming` naming the setting (`orm.resolved_guard_cache_bytes`,
    /// `orm.resolved_guard_cache_entries`) that is out of range.
    pub fn new(bytes: usize, entries: usize) -> Result<Self> {
        if !(Self::MIN_BYTES..=Self::MAX_BYTES).contains(&bytes) {
            return Err(Error::programming(format!(
                "orm.resolved_guard_cache_bytes is {bytes}: it must be from {} to {}",
                Self::MIN_BYTES,
                Self::MAX_BYTES
            )));
        }
        if !(1..=Self::MAX_ENTRIES).contains(&entries) {
            return Err(Error::programming(format!(
                "orm.resolved_guard_cache_entries is {entries}: it must be from 1 to {}",
                Self::MAX_ENTRIES
            )));
        }
        Ok(Self { bytes, entries })
    }

    /// The byte budget.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// The entry cap.
    #[must_use]
    pub const fn entries(&self) -> usize {
        self.entries
    }
}

impl Default for ResolvedGuardCacheConfig {
    fn default() -> Self {
        Self {
            bytes: Self::DEFAULT_BYTES,
            entries: Self::DEFAULT_ENTRIES,
        }
    }
}

/// What a [`ResolvedGuardCache`] counted since it was created, plus its current size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolvedGuardCacheStats {
    /// Lookups answered from an entry.
    pub hits: u64,
    /// Lookups that resolved and stored an entry.
    pub misses: u64,
    /// Plans resolved without being stored because one entry would not fit the byte budget.
    pub skipped: u64,
    /// Lookups resolved without being stored because their generation is older than the newest
    /// seen for the database.
    pub stale: u64,
    /// Entries dropped to make room.
    pub evictions: u64,
    /// Entries dropped because a newer generation of their database arrived.
    pub invalidated: u64,
    /// Entries now held.
    pub entries: usize,
    /// Bytes now charged.
    pub bytes: usize,
}

/// The identity of one cached resolution. Constructed by the engine from the transaction's pinned
/// state, never by a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct GuardCacheKey {
    database: u64,
    registry: u64,
    snapshot: SnapshotId,
    plan_hash: u64,
}

impl GuardCacheKey {
    pub(crate) const fn new(
        database: u64,
        registry: u64,
        snapshot: SnapshotId,
        plan_hash: u64,
    ) -> Self {
        Self {
            database,
            registry,
            snapshot,
            plan_hash,
        }
    }

    fn stamp(&self) -> (u64, SnapshotId) {
        (self.registry, self.snapshot)
    }
}

const NIL: usize = usize::MAX;

struct Node {
    key: GuardCacheKey,
    plan: Arc<GuardPlan>,
    resolved: Arc<ResolvedPlan>,
    bytes: usize,
    prev: usize,
    next: usize,
}

#[derive(Default)]
struct State {
    map: FxHashMap<GuardCacheKey, usize>,
    slots: Vec<Option<Node>>,
    free: Vec<usize>,
    /// Most recently used.
    head: usize,
    /// Least recently used.
    tail: usize,
    bytes: usize,
    /// Newest `(registry generation, snapshot)` seen per database.
    newest: FxHashMap<u64, (u64, SnapshotId)>,
}

impl State {
    fn new() -> Self {
        Self {
            head: NIL,
            tail: NIL,
            ..Self::default()
        }
    }

    fn node(&self, at: usize) -> Result<&Node> {
        self.slots
            .get(at)
            .and_then(Option::as_ref)
            .ok_or_else(|| Error::programming("resolved guard cache index is not a live entry"))
    }

    fn node_mut(&mut self, at: usize) -> Result<&mut Node> {
        self.slots
            .get_mut(at)
            .and_then(Option::as_mut)
            .ok_or_else(|| Error::programming("resolved guard cache index is not a live entry"))
    }

    fn unlink(&mut self, at: usize) -> Result<()> {
        let (prev, next) = {
            let n = self.node(at)?;
            (n.prev, n.next)
        };
        if prev == NIL {
            self.head = next;
        } else {
            self.node_mut(prev)?.next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.node_mut(next)?.prev = prev;
        }
        Ok(())
    }

    fn push_front(&mut self, at: usize) -> Result<()> {
        let old = self.head;
        {
            let n = self.node_mut(at)?;
            n.prev = NIL;
            n.next = old;
        }
        if old == NIL {
            self.tail = at;
        } else {
            self.node_mut(old)?.prev = at;
        }
        self.head = at;
        Ok(())
    }

    fn touch(&mut self, at: usize) -> Result<()> {
        if self.head != at {
            self.unlink(at)?;
            self.push_front(at)?;
        }
        Ok(())
    }

    fn remove(&mut self, at: usize) -> Result<()> {
        self.unlink(at)?;
        let node = self
            .slots
            .get_mut(at)
            .and_then(Option::take)
            .ok_or_else(|| Error::programming("resolved guard cache index is not a live entry"))?;
        self.map.remove(&node.key);
        self.bytes = self.bytes.saturating_sub(node.bytes);
        self.free.push(at);
        Ok(())
    }

    /// Bound `newest`: past `cap` databases, forget those with no cached entry (a forgotten
    /// database only loses the stale-lookup refusal, never a correct answer). What remains has an
    /// entry each, so it holds at most one more database than the cache has entries (`keep`, the one being
    /// looked up).
    fn prune_newest(&mut self, cap: usize, keep: u64) {
        if self.newest.len() <= cap {
            return;
        }
        let live: rustc_hash::FxHashSet<u64> = self
            .slots
            .iter()
            .flatten()
            .map(|n| n.key.database)
            .collect();
        self.newest.retain(|db, _| *db == keep || live.contains(db));
    }

    fn insert(&mut self, node: Node) -> Result<()> {
        let key = node.key;
        let bytes = node.bytes;
        let at = if let Some(at) = self.free.pop() {
            self.slots[at] = Some(node);
            at
        } else {
            self.slots.push(Some(node));
            self.slots.len() - 1
        };
        self.map.insert(key, at);
        self.bytes += bytes;
        self.push_front(at)
    }
}

/// A bounded LRU cache of resolved guard plans, shared by every transaction of an ORM lineage.
pub struct ResolvedGuardCache {
    config: ResolvedGuardCacheConfig,
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
    skipped: AtomicU64,
    stale: AtomicU64,
    evictions: AtomicU64,
    invalidated: AtomicU64,
}

impl fmt::Debug for ResolvedGuardCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedGuardCache")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ResolvedGuardCache {
    /// A cache of `config`'s size.
    #[must_use]
    pub fn new(config: ResolvedGuardCacheConfig) -> Self {
        Self {
            config,
            state: Mutex::new(State::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            invalidated: AtomicU64::new(0),
        }
    }

    /// A cache with bounds outside the configurable range (tests of the byte cap).
    #[cfg(test)]
    pub(crate) fn with_raw_bounds(bytes: usize, entries: usize) -> Self {
        Self::new(ResolvedGuardCacheConfig { bytes, entries })
    }

    /// Databases whose newest generation is remembered (tests of the bound).
    #[cfg(test)]
    pub(crate) fn tracked_databases(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .newest
            .len()
    }

    /// The configured bounds.
    #[must_use]
    pub fn config(&self) -> ResolvedGuardCacheConfig {
        self.config
    }

    /// The counters and the current size.
    #[must_use]
    pub fn stats(&self) -> ResolvedGuardCacheStats {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        ResolvedGuardCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            stale: self.stale.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            invalidated: self.invalidated.load(Ordering::Relaxed),
            entries: state.map.len(),
            bytes: state.bytes,
        }
    }

    /// The resolution of `plan` under `key`: from an entry whose plan equals `plan` completely,
    /// or `resolve`d (outside the lock) and stored when it is admitted.
    pub(crate) fn get_or_resolve(
        &self,
        key: GuardCacheKey,
        plan: &Arc<GuardPlan>,
        resolve: impl FnOnce(&GuardPlan) -> Result<ResolvedPlan>,
    ) -> Result<Arc<ResolvedPlan>> {
        let mut stale = false;
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            match state.newest.get(&key.database).copied() {
                Some(newest) if key.stamp() < newest => stale = true,
                Some(newest) if key.stamp() == newest => {}
                _ => {
                    // a newer generation of this database: everything older goes at once
                    let doomed: Vec<usize> = state
                        .slots
                        .iter()
                        .enumerate()
                        .filter_map(|(at, n)| {
                            n.as_ref()
                                .filter(|n| n.key.database == key.database)
                                .map(|_| at)
                        })
                        .collect();
                    for at in doomed {
                        state.remove(at)?;
                        self.invalidated.fetch_add(1, Ordering::Relaxed);
                    }
                    state.newest.insert(key.database, key.stamp());
                    state.prune_newest(self.config.entries, key.database);
                }
            }
            if !stale && let Some(&at) = state.map.get(&key) {
                let hit = {
                    let node = state.node(at)?;
                    (Arc::ptr_eq(&node.plan, plan) || *node.plan == **plan)
                        .then(|| Arc::clone(&node.resolved))
                };
                if let Some(resolved) = hit {
                    state.touch(at)?;
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(resolved);
                }
            }
        }
        let resolved = Arc::new(resolve(plan)?);
        if stale {
            self.stale.fetch_add(1, Ordering::Relaxed);
            return Ok(resolved);
        }
        let cost = plan
            .retained_bytes()
            .checked_add(resolved.bytes)
            .and_then(|b| b.checked_add(std::mem::size_of::<Node>()))
            .ok_or_else(|| Error::programming("resolved guard cache entry size overflows"))?;
        if cost > self.config.bytes {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(resolved);
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .newest
            .get(&key.database)
            .is_some_and(|n| key.stamp() < *n)
        {
            // a newer generation arrived while this one was resolving
            self.stale.fetch_add(1, Ordering::Relaxed);
            return Ok(resolved);
        }
        if let Some(&at) = state.map.get(&key) {
            // a concurrent lookup stored the same key (or a digest collision): replace it
            state.remove(at)?;
        }
        while !state.map.is_empty()
            && (state.map.len() >= self.config.entries
                || state.bytes.saturating_add(cost) > self.config.bytes)
        {
            let tail = state.tail;
            state.remove(tail)?;
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        state.insert(Node {
            key,
            plan: Arc::clone(plan),
            resolved: Arc::clone(&resolved),
            bytes: cost,
            prev: NIL,
            next: NIL,
        })?;
        self.misses.fetch_add(1, Ordering::Relaxed);
        Ok(resolved)
    }
}
