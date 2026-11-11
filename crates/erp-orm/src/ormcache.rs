//! `ormcache`: named, registry-wide caches of method results with transaction layers (ENV §4.11).
//!
//! A transaction reads its own layers (innermost savepoint first), then the registry-wide cache.
//! Values added and invalidations made in a transaction become visible to others only when it
//! commits; a rolled-back savepoint discards its layer, a released one merges it into the layer
//! below. Each named cache has a generation, bumped when a committed transaction
//! invalidated it: values computed by a transaction that started reading the cache before that
//! invalidation are not published (they may be stale).
//! Cross-process signaling and
//! LRU sizes are not part of this slice.

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use erp_fields::{Result, Value};
use rustc_hash::FxHashMap;

use crate::Transaction;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OrmCacheKey {
    /// `_name` of the model.
    pub model: &'static str,
    /// Method name.
    pub method: &'static str,
    /// The values the method's key expressions evaluate to.
    pub args: Vec<Value>,
}

impl OrmCacheKey {
    /// A key.
    pub fn new(model: &'static str, method: &'static str, args: Vec<Value>) -> Self {
        Self {
            model,
            method,
            args,
        }
    }
}

#[derive(Clone)]
enum Cached {
    Value(Value),
    Typed(Arc<dyn Any + Send + Sync>),
}

impl fmt::Debug for Cached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(v) => f.debug_tuple("Value").field(v).finish(),
            Self::Typed(_) => f.write_str("Typed(..)"),
        }
    }
}

type Values = FxHashMap<OrmCacheKey, Cached>;

/// The error of reading a cached entry through the API of the other kind.
fn kind_mismatch(cache: &str, key: &OrmCacheKey, holds: &str, wanted: &str) -> erp_fields::Error {
    erp_fields::Error::programming(format!(
        "the ormcache entry {}.{} of cache '{cache}' holds {holds}, read as {wanted}: one method's \
         entries are read through one API",
        key.model, key.method
    ))
}

#[derive(Debug, Default)]
struct Named {
    values: Values,
    generation: u64,
}

#[derive(Debug, Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
}

/// The registry-wide caches.
#[derive(Debug, Default)]
pub(crate) struct SharedOrmCache {
    caches: RwLock<FxHashMap<String, Named>>,
    stats: RwLock<FxHashMap<(&'static str, &'static str), Arc<Counters>>>,
}

impl SharedOrmCache {
    fn get(&self, cache: &str, key: &OrmCacheKey) -> Option<Cached> {
        let caches = self.caches.read().unwrap_or_else(PoisonError::into_inner);
        caches.get(cache)?.values.get(key).cloned()
    }

    /// Drop every value of a named cache and move its generation, as a committed invalidation in
    /// this process does; values being computed against the old state are then not published.
    /// Used when another process invalidated it.
    pub(crate) fn clear(&self, cache: &str) {
        let mut caches = self.caches.write().unwrap_or_else(PoisonError::into_inner);
        let named = caches.entry(cache.to_owned()).or_default();
        named.values.clear();
        named.generation += 1;
    }

    /// [`clear`](Self::clear) every named cache (a full refresh after the signal generations went
    /// backwards).
    pub(crate) fn clear_all(&self) {
        let mut caches = self.caches.write().unwrap_or_else(PoisonError::into_inner);
        for named in caches.values_mut() {
            named.values.clear();
            named.generation += 1;
        }
    }

    fn generation(&self, cache: &str) -> u64 {
        let caches = self.caches.read().unwrap_or_else(PoisonError::into_inner);
        caches.get(cache).map_or(0, |n| n.generation)
    }

    fn counters(&self, model: &'static str, method: &'static str) -> Arc<Counters> {
        if let Some(c) = self
            .stats
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(model, method))
        {
            return Arc::clone(c);
        }
        let mut stats = self.stats.write().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(stats.entry((model, method)).or_default())
    }
}

#[derive(Debug, Default)]
struct Layer {
    values: Values,
    /// The cache was invalidated in this layer: nothing below is visible.
    cleared: bool,
}

/// A transaction's layers: index 0 is the transaction itself, then one per open savepoint.
#[derive(Debug, Default)]
pub(crate) struct OrmCacheLayers {
    layers: Vec<FxHashMap<String, Layer>>,
    /// Generation of each named cache when this transaction first used it.
    seen: FxHashMap<String, u64>,
}

impl OrmCacheLayers {
    fn top(&mut self) -> &mut FxHashMap<String, Layer> {
        if self.layers.is_empty() {
            self.layers.push(FxHashMap::default());
        }
        let last = self.layers.len() - 1;
        &mut self.layers[last]
    }

    fn touch(&mut self, cache: &str, shared: &SharedOrmCache) {
        if !self.seen.contains_key(cache) {
            self.seen.insert(cache.to_owned(), shared.generation(cache));
        }
    }

    /// Names of the caches this transaction invalidated (any layer), sorted and unique.
    pub(crate) fn cleared_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .layers
            .iter()
            .flat_map(|layer| layer.iter())
            .filter(|(_, l)| l.cleared)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// `None`: look in the shared cache; `Some(found)`: answered by a layer.
    fn lookup(&self, cache: &str, key: &OrmCacheKey) -> Option<Option<Cached>> {
        for layer in self.layers.iter().rev() {
            if let Some(l) = layer.get(cache) {
                if let Some(v) = l.values.get(key) {
                    return Some(Some(v.clone()));
                }
                if l.cleared {
                    return Some(None);
                }
            }
        }
        None
    }

    pub(crate) fn push(&mut self) {
        self.top();
        self.layers.push(FxHashMap::default());
    }

    pub(crate) fn release(&mut self) {
        let Some(top) = self.layers.pop() else { return };
        let below = self.top();
        for (name, layer) in top {
            if layer.cleared {
                below.insert(name, layer);
            } else {
                below.entry(name).or_default().values.extend(layer.values);
            }
        }
    }

    pub(crate) fn rollback(&mut self) {
        if self.layers.len() > 1 {
            self.layers.pop();
        }
    }

    /// Publish the transaction's values at commit. Layers of savepoints still open (commit
    /// refuses that case) are merged first rather than dropped. The values of a cache for which
    /// `stale` is true were computed on a snapshot older than a change this process has applied
    /// : they are not published, but an invalidation still is.
    fn publish(&mut self, shared: &SharedOrmCache, stale: impl Fn(&str) -> bool) {
        while self.layers.len() > 1 {
            self.release();
        }
        let base = self.layers.pop().unwrap_or_default();
        let seen = std::mem::take(&mut self.seen);
        let mut caches = shared
            .caches
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for (name, layer) in base {
            let is_stale = stale(&name);
            let named = caches.entry(name.clone()).or_default();
            if layer.cleared {
                named.values = if is_stale {
                    Values::default()
                } else {
                    layer.values
                };
                named.generation += 1;
            } else if !is_stale && seen.get(&name).is_none_or(|g| *g == named.generation) {
                named.values.extend(layer.values);
            }
            // else: another transaction invalidated this cache after we read it; our values may
            // be stale, so they are not published.
        }
    }

    pub(crate) fn reset(&mut self) {
        self.layers.clear();
        self.seen.clear();
    }
}

impl crate::Orm {
    /// The generation of the named cache `cache` in this process: it moves when a transaction of
    /// this process that invalidated the cache commits, and when the process applies a
    /// `cache:<name>` signal bump from another process. A cache kept outside the
    /// ormcache (`erp-security`'s auth settings and keys) stores the generation it was read at and
    /// is stale when it differs. 0 for a cache never used.
    pub fn ormcache_generation(&self, cache: &str) -> u64 {
        self.shared.ormcache.generation(cache)
    }
}

impl Transaction {
    /// [`crate::Orm::ormcache_generation`] of this transaction's ORM (the process-wide value, not
    /// this transaction's pending invalidations).
    pub fn ormcache_generation(&self, cache: &str) -> u64 {
        self.shared.ormcache.generation(cache)
    }

    /// `get_value`: the cached value of `key` in cache `cache`, without computing it
    /// .
    ///
    /// The transaction is opened first ([`Transaction::open`]): with signals, its opening flight
    /// reads the cache generations and drops what another process invalidated, so a lookup made
    /// before any statement cannot serve such a value (MN-4).
    ///
    /// # Errors
    ///
    /// The errors of opening the transaction.
    pub async fn ormcache_get(&mut self, cache: &str, key: &OrmCacheKey) -> Result<Option<Value>> {
        self.open().await?;
        match self.ormcache_lookup(cache, key) {
            None => Ok(None),
            Some(Cached::Value(v)) => Ok(Some(v)),
            Some(Cached::Typed(_)) => Err(kind_mismatch(cache, key, "a typed value", "a Value")),
        }
    }

    /// The lookup of [`Transaction::ormcache_get`] on an open transaction.
    fn ormcache_lookup(&mut self, cache: &str, key: &OrmCacheKey) -> Option<Cached> {
        let shared = Arc::clone(&self.shared);
        self.engine.ormcache.touch(cache, &shared.ormcache);
        match self.engine.ormcache.lookup(cache, key) {
            Some(found) => found,
            None => shared.ormcache.get(cache, key),
        }
    }

    /// `add_value`: store a value in this transaction's innermost layer. The
    /// transaction is opened first, so the generation the value is checked against at commit is
    /// the one of its snapshot (see [`Transaction::ormcache_get`]).
    ///
    /// # Errors
    ///
    /// The errors of opening the transaction.
    pub async fn ormcache_add(
        &mut self,
        cache: &str,
        key: OrmCacheKey,
        value: Value,
    ) -> Result<()> {
        self.open().await?;
        self.ormcache_insert(cache, key, Cached::Value(value));
        Ok(())
    }

    fn ormcache_insert(&mut self, cache: &str, key: OrmCacheKey, value: Cached) {
        let shared = Arc::clone(&self.shared);
        self.engine.ormcache.touch(cache, &shared.ormcache);
        self.engine
            .ormcache
            .top()
            .entry(cache.to_owned())
            .or_default()
            .values
            .insert(key, value);
    }

    /// The cached result of `key`, or the result of `compute` (then cached); counts hits and
    /// misses per (model, method). Opens the transaction first, as
    /// [`Transaction::ormcache_get`] does.
    ///
    /// # Errors
    ///
    /// The errors of opening the transaction and of `compute`.
    pub async fn ormcache<F>(&mut self, cache: &str, key: OrmCacheKey, compute: F) -> Result<Value>
    where
        F: AsyncFnOnce(&mut Transaction) -> Result<Value>,
    {
        self.open().await?;
        let counters = self.shared.ormcache.counters(key.model, key.method);
        match self.ormcache_lookup(cache, &key) {
            Some(Cached::Value(v)) => {
                counters.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(v);
            }
            Some(Cached::Typed(_)) => {
                return Err(kind_mismatch(cache, &key, "a typed value", "a Value"));
            }
            None => {}
        }
        counters.misses.fetch_add(1, Ordering::Relaxed);
        let value = compute(self).await?;
        self.ormcache_insert(cache, key, Cached::Value(value.clone()));
        Ok(value)
    }

    /// [`Transaction::ormcache`] for a typed value: the cached `Arc<T>`, or the result of
    /// `compute` (then cached). Same layers, generations and cross-process invalidation as the
    /// `Value` API; a hit clones the `Arc` (no allocation). Each `(model, method)` keeps to one
    /// API and one `T`.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # async fn demo(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let key = erp_orm::OrmCacheKey::new("x.model", "_lookup", vec![]);
    /// let table: Arc<Vec<i64>> = tx
    ///     .ormcache_arc("x_cache", key, async |_tx: &mut erp_orm::Transaction| Ok(vec![1, 2, 3]))
    ///     .await?;
    /// assert_eq!(table.len(), 3);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// The errors of opening the transaction and of `compute`; `Programming` when the entry holds
    /// a `Value` or a value of another type.
    pub async fn ormcache_arc<T, F>(
        &mut self,
        cache: &str,
        key: OrmCacheKey,
        compute: F,
    ) -> Result<Arc<T>>
    where
        T: Any + Send + Sync,
        F: AsyncFnOnce(&mut Transaction) -> Result<T>,
    {
        self.open().await?;
        let counters = self.shared.ormcache.counters(key.model, key.method);
        match self.ormcache_lookup(cache, &key) {
            Some(Cached::Typed(any)) => {
                return match any.downcast::<T>() {
                    Ok(value) => {
                        counters.hits.fetch_add(1, Ordering::Relaxed);
                        Ok(value)
                    }
                    Err(_) => Err(kind_mismatch(
                        cache,
                        &key,
                        "a value of another type",
                        std::any::type_name::<T>(),
                    )),
                };
            }
            Some(Cached::Value(_)) => {
                return Err(kind_mismatch(
                    cache,
                    &key,
                    "a Value",
                    std::any::type_name::<T>(),
                ));
            }
            None => {}
        }
        counters.misses.fetch_add(1, Ordering::Relaxed);
        let value = Arc::new(compute(self).await?);
        let shared: Arc<dyn Any + Send + Sync> = Arc::clone(&value) as _;
        self.ormcache_insert(cache, key, Cached::Typed(shared));
        Ok(value)
    }

    /// Invalidate a named cache for the rest of this transaction (and for everyone once it
    /// commits).
    pub fn invalidate_ormcache(&mut self, cache: &str) {
        let shared = Arc::clone(&self.shared);
        self.engine.ormcache.touch(cache, &shared.ormcache);
        self.engine.ormcache.top().insert(
            cache.to_owned(),
            Layer {
                values: Values::default(),
                cleared: true,
            },
        );
    }

    /// `(hits, misses)` of a cached method since the ORM started.
    pub fn ormcache_stats(&self, model: &'static str, method: &'static str) -> (u64, u64) {
        let c = self.shared.ormcache.counters(model, method);
        (
            c.hits.load(Ordering::Relaxed),
            c.misses.load(Ordering::Relaxed),
        )
    }

    /// Publish this transaction's ormcache values. With signals, a cache whose generation this
    /// transaction began at is older than the one the process has applied by now was invalidated
    /// elsewhere after this transaction's snapshot: its values are refused (review 1;
    /// the per-process generation alone misses it, because it is recorded at first use, not at
    /// transaction start). A refresh records a cache's generation before it clears the cache, so
    /// this also holds for a cache first used while the refresh runs. Every value is refused when
    /// a full refresh (after a regression) started since the transaction began.
    pub(crate) fn publish_ormcache(&mut self) {
        let shared = Arc::clone(&self.shared);
        let began = self.signal_state.began_at();
        let began_full_refreshes = self.signal_state.began_full_refreshes();
        let signal = shared.signal.as_deref();
        self.engine.ormcache.publish(&shared.ormcache, |name| {
            let (Some(signal), Some(began)) = (signal, began) else {
                // no signal, or the transaction never opened (no snapshot its values depend on)
                return false;
            };
            if began_full_refreshes != signal.full_refreshes() {
                return true;
            }
            match crate::signal::SignalKey::cache(name) {
                Ok(key) => began.get(&key) < signal.applied_generation(&key),
                // a name the signal cannot carry is never invalidated across processes
                Err(_) => false,
            }
        });
    }
}
