//! Savepoint-scoped id sets of a transaction.
//!
//! A crate that needs "which ids did this transaction already handle" across savepoints keeps its
//! ids in a set keyed by a private marker type ([`super::Transaction::mark_scoped_id`],
//! [`super::Transaction::scoped_ids`]). Ids recorded inside a savepoint are forgotten when it rolls back,
//! promoted to the enclosing level when it is released, and all sets are cleared at transaction end.
//! The `ir.ui.view` root tracking (addons/base, `tree.rs`) is the first user: it avoids rewriting a
//! tree root on every check while a savepoint (such as a batch import) is open.

use rustc_hash::FxHashMap;

/// Ids recorded in a transaction, mapped to the savepoint depth where they were first recorded.
#[derive(Debug, Default, Clone)]
pub(crate) struct ScopedIds {
    /// Map from id to the savepoint depth at which it was first touched.
    ids: FxHashMap<i64, usize>,
}

impl ScopedIds {
    /// Record `id` as touched at savepoint depth `depth`.
    ///
    /// If `id` was already touched at an earlier (shallower) depth, that earlier depth is preserved.
    pub(crate) fn touch(&mut self, id: i64, depth: usize) {
        self.ids.entry(id).or_insert(depth);
    }

    /// Whether `id` has been recorded.
    #[cfg(test)]
    #[inline]
    pub(crate) fn is_touched(&self, id: i64) -> bool {
        self.ids.contains_key(&id)
    }

    /// Sorted list of all ids recorded so far.
    pub(crate) fn touched_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.ids.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Savepoint rollback to `depth`: discard ids recorded at deeper levels (> depth).
    pub(crate) fn rolled_back_to(&mut self, depth: usize) {
        self.ids.retain(|_, d| *d <= depth);
    }

    /// Savepoint release to `depth`: promote ids recorded at deeper levels to `depth`.
    pub(crate) fn released_to(&mut self, depth: usize) {
        for d in self.ids.values_mut() {
            *d = (*d).min(depth);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_tracking_across_savepoints() {
        let mut vr = ScopedIds::default();
        // Top level (depth 0) touches root 1
        vr.touch(1, 0);
        assert!(vr.is_touched(1));
        assert_eq!(vr.touched_ids(), vec![1]);

        // Savepoint 1 (depth 1) touches root 2 and touches root 1 again
        vr.touch(2, 1);
        vr.touch(1, 1); // should keep depth 0
        assert!(vr.is_touched(1));
        assert!(vr.is_touched(2));
        assert_eq!(vr.touched_ids(), vec![1, 2]);

        // Savepoint 1 rolls back to depth 0
        vr.rolled_back_to(0);
        assert!(vr.is_touched(1));
        assert!(!vr.is_touched(2));
        assert_eq!(vr.touched_ids(), vec![1]);

        // Savepoint 2 (depth 1) touches root 3, then releases to depth 0
        vr.touch(3, 1);
        vr.released_to(0);
        assert!(vr.is_touched(1));
        assert!(vr.is_touched(3));

        // Another rollback to depth 0 does not discard root 3 because it was released
        vr.rolled_back_to(0);
        assert!(vr.is_touched(1));
        assert!(vr.is_touched(3));
    }

    /// A release into the enclosing savepoint keeps the entry at that level: a later rollback of
    /// the enclosing savepoint forgets it.
    #[test]
    fn released_ids_are_forgotten_when_the_outer_savepoint_rolls_back() {
        let mut ids = ScopedIds::default();
        ids.touch(7, 2);
        ids.released_to(1);
        // the inner savepoint is gone but the outer one (depth 1) is still open
        ids.rolled_back_to(1);
        assert!(ids.is_touched(7));
        ids.rolled_back_to(0);
        assert!(!ids.is_touched(7));
        assert_eq!(ids.touched_ids(), Vec::<i64>::new());
    }

    /// Two independent sets (one per marker key) do not see each other's ids.
    #[test]
    fn independent_sets_do_not_share_ids() {
        let mut a = ScopedIds::default();
        let b = ScopedIds::default();
        a.touch(1, 0);
        assert!(a.is_touched(1));
        assert!(!b.is_touched(1));
        assert!(b.touched_ids().is_empty());
    }
}
