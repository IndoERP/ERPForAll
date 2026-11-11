//! A fixed-size bit set over group indices (the group graph's closures, `allowing_groups`).

/// A set of indices below a fixed size, one bit each.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BitSet {
    words: Box<[u64]>,
}

impl BitSet {
    /// The empty set over `len` indices.
    pub(crate) fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)].into_boxed_slice(),
        }
    }

    /// The full set over `len` indices.
    pub(crate) fn full(len: usize) -> Self {
        let mut set = Self::new(len);
        for i in 0..len {
            set.insert(i);
        }
        set
    }

    /// Add `i` (must be below the size the set was made for).
    pub(crate) fn insert(&mut self, i: usize) {
        if let Some(word) = self.words.get_mut(i / 64) {
            *word |= 1 << (i % 64);
        }
    }

    /// Whether `i` is in the set.
    #[cfg(test)]
    pub(crate) fn contains(&self, i: usize) -> bool {
        self.words
            .get(i / 64)
            .is_some_and(|word| word & (1 << (i % 64)) != 0)
    }

    /// `self ∪= other` (same size).
    pub(crate) fn union_with(&mut self, other: &BitSet) {
        for (a, b) in self.words.iter_mut().zip(other.words.iter()) {
            *a |= b;
        }
    }

    /// `self ∩= other` (same size).
    pub(crate) fn intersect_with(&mut self, other: &BitSet) {
        for (a, b) in self.words.iter_mut().zip(other.words.iter()) {
            *a &= b;
        }
    }

    /// `self −= other` (same size).
    pub(crate) fn difference_with(&mut self, other: &BitSet) {
        for (a, b) in self.words.iter_mut().zip(other.words.iter()) {
            *a &= !b;
        }
    }

    /// Whether no index is in the set.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// The indices, ascending.
    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, word)| {
            let mut bits = *word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let tz = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(w * 64 + tz)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_operations() {
        let mut a = BitSet::new(130);
        for i in [0, 63, 64, 129] {
            a.insert(i);
        }
        assert!(a.contains(129) && a.contains(64) && !a.contains(1));
        assert_eq!(a.iter().collect::<Vec<_>>(), [0, 63, 64, 129]);
        let mut b = BitSet::new(130);
        b.insert(64);
        b.insert(5);
        let mut u = a.clone();
        u.union_with(&b);
        assert_eq!(u.iter().collect::<Vec<_>>(), [0, 5, 63, 64, 129]);
        let mut i = a.clone();
        i.intersect_with(&b);
        assert_eq!(i.iter().collect::<Vec<_>>(), [64]);
        let mut d = a.clone();
        d.difference_with(&b);
        assert_eq!(d.iter().collect::<Vec<_>>(), [0, 63, 129]);
        assert!(BitSet::new(0).is_empty());
        assert_eq!(BitSet::full(3).iter().collect::<Vec<_>>(), [0, 1, 2]);
        // out of range is ignored by insert and absent for contains
        let mut small = BitSet::new(1);
        small.insert(200);
        assert!(!small.contains(200));
    }
}
