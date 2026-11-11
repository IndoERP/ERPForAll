//! `auto_sequence` counters (design §3.4).
//!
//! So one counter numbers one top-level operation and the sub-records
//! nested in it, depth first, then restarts for the next top-level operation. A skipped record
//! never reaches the step and does not advance the counter.
//!
//! A [`Counters`] value lives in the session. [`Counters::open`] starts a tree when the loader
//! meets a top-level `<record>`; the records of the tree take numbers when their batch is
//! written ([`Counters::next`]), in document order. [`Counters::close`] marks the tree finished;
//! its counter is dropped by the next [`Counters::retain_open`], which the batch calls after a
//! flush (a leaf record of a closed tree can still be waiting in the window until then).

use std::collections::HashMap;

/// The step of the counter.
const STEP: i64 = 10;

#[derive(Debug, Clone, Copy)]
struct Counter {
    /// `None`: the container has no `auto_sequence`.
    value: Option<i64>,
    open: bool,
}

/// The counters of the trees the session is loading.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    next_tree: u64,
    trees: HashMap<u64, Counter>,
}

impl Counters {
    /// Start a tree; `auto_sequence` is the flag of the container of its top-level operation.
    pub(crate) fn open(&mut self, auto_sequence: bool) -> u64 {
        self.next_tree += 1;
        let tree = self.next_tree;
        self.trees.insert(
            tree,
            Counter {
                value: auto_sequence.then_some(0),
                open: true,
            },
        );
        tree
    }

    /// The tree's top-level operation has been processed.
    pub(crate) fn close(&mut self, tree: u64) {
        if let Some(counter) = self.trees.get_mut(&tree) {
            counter.open = false;
        }
    }

    /// Drop the counters of closed trees.
    pub(crate) fn retain_open(&mut self) {
        self.trees.retain(|_, counter| counter.open);
    }

    /// The number for the next record of `tree` that needs one: `None` when the tree's container
    /// has no `auto_sequence`.
    pub(crate) fn next(&mut self, tree: u64) -> Option<i64> {
        let counter = self.trees.get_mut(&tree)?;
        let value = counter.value.as_mut()?;
        *value += STEP;
        Some(*value)
    }

    /// Forget every tree (the window is discarded).
    pub(crate) fn clear(&mut self) {
        self.trees.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_counts_in_tens_and_a_new_tree_restarts() {
        let mut counters = Counters::default();
        let first = counters.open(true);
        assert_eq!(counters.next(first), Some(10));
        assert_eq!(counters.next(first), Some(20));
        let second = counters.open(true);
        assert_eq!(counters.next(second), Some(10));
        assert_eq!(counters.next(first), Some(30));
    }

    #[test]
    fn a_tree_without_auto_sequence_numbers_nothing() {
        let mut counters = Counters::default();
        let tree = counters.open(false);
        assert_eq!(counters.next(tree), None);
    }

    #[test]
    fn a_closed_tree_is_dropped_after_the_next_flush_only() {
        let mut counters = Counters::default();
        let tree = counters.open(true);
        counters.close(tree);
        assert_eq!(counters.next(tree), Some(10));
        counters.retain_open();
        assert_eq!(counters.next(tree), None);
    }
}
