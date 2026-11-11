//! A growable bitset over cache slots.

/// Bits indexed by slot number; reading past the end is `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BitSet {
    words: Vec<u64>,
}

// The four single-bit operations run once per cached cell. `#[inline]` keeps them inlined in
// every caller whichever of the release profile's 16 codegen units the caller lands in: an
// unrelated change once moved `assign` out of line and cost BENCH-002 3 % CPU (PERF.STREAM-WINDOW).
impl BitSet {
    #[inline]
    pub(crate) fn get(&self, bit: usize) -> bool {
        self.words
            .get(bit / 64)
            .is_some_and(|w| w & (1u64 << (bit % 64)) != 0)
    }

    #[inline]
    pub(crate) fn set(&mut self, bit: usize) {
        let word = bit / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1u64 << (bit % 64);
    }

    #[inline]
    pub(crate) fn clear(&mut self, bit: usize) {
        if let Some(w) = self.words.get_mut(bit / 64) {
            *w &= !(1u64 << (bit % 64));
        }
    }

    #[inline]
    pub(crate) fn assign(&mut self, bit: usize, on: bool) {
        if on {
            self.set(bit);
        } else {
            self.clear(bit);
        }
    }

    /// Room for bits `0..bits` exactly (no amortised doubling: the caller knows the size).
    pub(crate) fn reserve(&mut self, bits: usize) {
        let words = bits.div_ceil(64);
        self.words
            .reserve_exact(words.saturating_sub(self.words.len()));
    }

    /// Number of bits the allocation can hold without growing.
    pub(crate) fn capacity_bits(&self) -> usize {
        self.words.capacity().saturating_mul(64)
    }

    /// Whether a bit at or past `bit` is set.
    pub(crate) fn any_from(&self, bit: usize) -> bool {
        let word = bit / 64;
        let Some(first) = self.words.get(word) else {
            return false;
        };
        // stops at the first word with a set bit
        first & !((1u64 << (bit % 64)) - 1) != 0
            || self
                .words
                .get(word + 1..)
                .is_some_and(|tail| tail.iter().any(|w| *w != 0))
    }

    /// Number of set bits at or past `bit`.
    pub(crate) fn count_from(&self, bit: usize) -> usize {
        let word = bit / 64;
        let Some(first) = self.words.get(word) else {
            return 0;
        };
        let head = (first & !((1u64 << (bit % 64)) - 1)).count_ones() as usize;
        let rest: usize = self
            .words
            .get(word + 1..)
            .map_or(0, |tail| tail.iter().map(|w| w.count_ones() as usize).sum());
        head + rest
    }

    /// Clear every bit at or past `bits`, keeping the allocation (a stream window's slots leave
    /// the cache, design note `perf-stream.md` §3.5).
    pub(crate) fn cut_at(&mut self, bits: usize) {
        let words = bits.div_ceil(64);
        if self.words.len() > words {
            self.words.truncate(words);
        }
        if !bits.is_multiple_of(64)
            && let Some(last) = self.words.get_mut(bits / 64)
        {
            *last &= (1u64 << (bits % 64)) - 1;
        }
    }

    pub(crate) fn any(&self) -> bool {
        self.words.iter().any(|w| *w != 0)
    }

    /// Set bits in increasing order.
    pub(crate) fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(i, &w)| {
            let mut word = w;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros() as usize;
                word &= word - 1;
                Some(i * 64 + bit)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_clear_iterate() {
        let mut b = BitSet::default();
        assert!(!b.get(1000));
        for i in [0, 63, 64, 130, 1000] {
            b.set(i);
        }
        assert!(b.get(64) && b.get(1000) && !b.get(65));
        b.clear(64);
        assert_eq!(b.ones().collect::<Vec<_>>(), [0, 63, 130, 1000]);
        assert!(b.any());
        for i in [0, 63, 130, 1000] {
            b.clear(i);
        }
        assert!(!b.any());
    }

    #[test]
    fn count_and_cut_from_a_bit() {
        let mut b = BitSet::default();
        for i in [0, 5, 63, 64, 70, 200] {
            b.set(i);
        }
        assert_eq!(b.count_from(0), 6);
        assert_eq!(b.count_from(5), 5);
        assert_eq!(b.count_from(6), 4);
        assert_eq!(b.count_from(64), 3);
        assert_eq!(b.count_from(201), 0);
        assert_eq!(b.count_from(10_000), 0);
        let capacity = b.capacity_bits();
        b.cut_at(64);
        assert_eq!(b.ones().collect::<Vec<_>>(), [0, 5, 63]);
        assert_eq!(b.capacity_bits(), capacity, "the allocation is kept");
        b.cut_at(6);
        assert_eq!(b.ones().collect::<Vec<_>>(), [0, 5]);
        assert!(!b.any_from(6));
        b.cut_at(0);
        assert!(!b.any());
        b.cut_at(1_000);
        assert!(!b.any());
    }
}
