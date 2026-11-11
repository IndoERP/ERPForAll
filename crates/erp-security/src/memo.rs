//! A bounded memo with least-recently-used eviction (DN §2.3, R-S10): a policy snapshot memoizes
//! its combined predicates per (model, operation, group set, shape) without growing without
//! bound. Eviction only costs a recompile.

use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use erp_fields::{Error, Result};
use rustc_hash::FxHashMap;

/// At most `capacity` entries; when full, the least recently used tenth (at least one entry) is
/// dropped before an insert.
#[derive(Debug)]
pub(crate) struct Lru<K, V> {
    capacity: usize,
    state: Mutex<State<K, V>>,
    evictions: AtomicU64,
}

#[derive(Debug)]
struct State<K, V> {
    map: FxHashMap<K, (V, u64)>,
    tick: u64,
}

impl<K: Hash + Eq, V: Clone> Lru<K, V> {
    /// An empty memo of `capacity` entries.
    ///
    /// # Errors
    ///
    /// `Programming` for a zero capacity (zero never means unlimited).
    pub(crate) fn new(capacity: usize, what: &str) -> Result<Self> {
        if capacity == 0 {
            return Err(Error::programming(format!(
                "the {what} memo capacity must be non-zero (zero never means unlimited)"
            )));
        }
        Ok(Self {
            capacity,
            state: Mutex::new(State {
                map: FxHashMap::default(),
                tick: 0,
            }),
            evictions: AtomicU64::new(0),
        })
    }

    /// The value of `key`, marking it used.
    pub(crate) fn get(&self, key: &K) -> Option<V> {
        // a poisoned lock still holds a consistent map: every update below is a single map call
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.tick = state.tick.wrapping_add(1);
        let tick = state.tick;
        state.map.get_mut(key).map(|(value, used)| {
            *used = tick;
            value.clone()
        })
    }

    /// Remember `value` for `key`.
    pub(crate) fn insert(&self, key: K, value: V) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.tick = state.tick.wrapping_add(1);
        let tick = state.tick;
        if !state.map.contains_key(&key) && state.map.len() >= self.capacity {
            let mut ticks: Vec<u64> = state.map.values().map(|(_, used)| *used).collect();
            let drop = (self.capacity / 10).max(1).min(ticks.len());
            let (_, cutoff, _) = ticks.select_nth_unstable(drop - 1);
            let cutoff = *cutoff;
            let before = state.map.len();
            state.map.retain(|_, (_, used)| *used > cutoff);
            let evicted = before - state.map.len();
            self.evictions.fetch_add(evicted as u64, Ordering::Relaxed);
        }
        state.map.insert(key, (value, tick));
    }

    /// Entries dropped so far (`access_memo_evictions{memo}`).
    pub(crate) fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    /// Entries held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_the_least_recently_used() {
        let memo: Lru<u32, u32> = Lru::new(10, "test").unwrap();
        for k in 0..10 {
            memo.insert(k, k * 10);
        }
        // touch 0..5 so 5..10 are the least recently used
        for k in 0..5 {
            assert_eq!(memo.get(&k), Some(k * 10));
        }
        memo.insert(10, 100);
        assert_eq!(memo.evictions(), 1);
        assert_eq!(memo.len(), 10);
        assert_eq!(memo.get(&5), None, "the least recently used went");
        for k in (0..5).chain(6..11) {
            assert!(memo.get(&k).is_some(), "{k}");
        }
        // replacing a key never evicts
        memo.insert(10, 101);
        assert_eq!(memo.evictions(), 1);
        assert_eq!(memo.get(&10), Some(101));
    }

    #[test]
    fn capacity_one_and_zero() {
        let memo: Lru<u32, u32> = Lru::new(1, "test").unwrap();
        memo.insert(1, 1);
        memo.insert(2, 2);
        assert_eq!((memo.get(&1), memo.get(&2)), (None, Some(2)));
        let err = Lru::<u32, u32>::new(0, "combined").unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "the combined memo capacity must be non-zero (zero never means unlimited)"
        );
    }
}
