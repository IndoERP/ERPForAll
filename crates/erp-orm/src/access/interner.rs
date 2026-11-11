//! Group-set interning per snapshot (DN §2.3, R-S10).

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use erp_fields::{Error, ErrorKind, Result};
use rustc_hash::FxHashMap;

use crate::principal::{GroupSet, GroupSetId};

/// A group set as one snapshot interned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InternedGroups {
    id: GroupSetId,
    share: bool,
}

impl InternedGroups {
    /// The interned `id`; `share` when the set is of a share user type (portal, public).
    pub fn new(id: GroupSetId, share: bool) -> Self {
        Self { id, share }
    }

    /// The id (meaningful only with the snapshot's [`crate::SnapshotId`]).
    pub fn id(self) -> GroupSetId {
        self.id
    }

    /// Whether the set is of a share user type.
    pub fn share(self) -> bool {
        self.share
    }
}

/// Interns group sets into [`GroupSetId`]s for one snapshot: equal sets get equal ids while they
/// stay interned, an id never names two sets (ids are never reused), and the table is bounded:
/// when it holds `capacity` sets it is emptied, so later sets get fresh ids and the memo entries
/// keyed by the old ones can never be hit again (eviction costs a recompile, nothing else).
///
/// ```
/// use erp_orm::{GroupId, GroupSet, GroupSetInterner};
/// let interner = GroupSetInterner::new(2)?;
/// let a = GroupSet::new([GroupId::new(1)?]);
/// let b = GroupSet::new([GroupId::new(2)?]);
/// let id_a = interner.intern(&a)?;
/// assert_eq!(interner.intern(&a)?, id_a);
/// assert_ne!(interner.intern(&b)?, id_a);
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug)]
pub struct GroupSetInterner {
    capacity: usize,
    state: Mutex<State>,
    evictions: AtomicU64,
}

#[derive(Debug, Default)]
struct State {
    last: u64,
    ids: FxHashMap<GroupSet, GroupSetId>,
}

impl GroupSetInterner {
    /// An interner holding at most `capacity` sets.
    ///
    /// # Errors
    ///
    /// `Programming` for a zero capacity (zero never means unlimited).
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(Error::programming(
                "GroupSetInterner::new: the capacity must be non-zero (zero never means \
                 unlimited)",
            ));
        }
        Ok(Self {
            capacity,
            state: Mutex::new(State::default()),
            evictions: AtomicU64::new(0),
        })
    }

    /// The id of `set`.
    ///
    /// # Errors
    ///
    /// `ResourceExhausted` after 2^64 − 1 ids (never reused).
    pub fn intern(&self, set: &GroupSet) -> Result<GroupSetId> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(id) = state.ids.get(set) {
            return Ok(*id);
        }
        if state.ids.len() >= self.capacity {
            state.ids.clear();
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        let next = state
            .last
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::ResourceExhausted,
                    "group set ids of this policy snapshot are exhausted (ids are never reused)",
                )
            })?;
        state.last = next.get();
        let id = GroupSetId::new(next);
        state.ids.insert(set.clone(), id);
        Ok(id)
    }

    /// How many times the table was emptied (`access_memo_evictions{memo="interner"}`).
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::GroupId;

    fn set(ids: &[i64]) -> GroupSet {
        GroupSet::new(ids.iter().map(|i| GroupId::new(*i).unwrap()))
    }

    #[test]
    fn ids_are_never_reused_across_evictions() {
        let interner = GroupSetInterner::new(2).unwrap();
        let a = interner.intern(&set(&[1])).unwrap();
        let b = interner.intern(&set(&[2])).unwrap();
        assert_eq!(interner.intern(&set(&[1])).unwrap(), a);
        let c = interner.intern(&set(&[3])).unwrap();
        assert_eq!(interner.evictions(), 1);
        let a2 = interner.intern(&set(&[1])).unwrap();
        let ids = [a, b, c, a2];
        for (i, x) in ids.iter().enumerate() {
            for y in &ids[i + 1..] {
                assert_ne!(x, y, "an id is never handed out twice");
            }
        }
        let empty = interner.intern(&GroupSet::default()).unwrap();
        assert!(!ids.contains(&empty));
    }

    #[test]
    fn zero_capacity_is_refused() {
        let err = GroupSetInterner::new(0).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "GroupSetInterner::new: the capacity must be non-zero (zero never means unlimited)"
        );
    }
}
