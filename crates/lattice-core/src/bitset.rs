//! Fixed-capacity bitset — the mask primitive of the schema compiler
//! (dispatch masks, requirement read-sets, enabled-action index; ARCH §7).
//! Representation is `Vec<u64>` words; equality is byte equality, so
//! `PartialEq` doubles as the determinism check (SPEC-03 D-invariants).

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BitSet {
    words: Vec<u64>,
    len: usize,
}

impl BitSet {
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0u64; len.div_ceil(64)],
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn set(&mut self, idx: usize) {
        assert!(idx < self.len, "bit {idx} out of range [0, {})", self.len);
        self.words[idx / 64] |= 1u64 << (idx % 64);
    }

    pub fn unset(&mut self, idx: usize) {
        assert!(idx < self.len, "bit {idx} out of range [0, {})", self.len);
        self.words[idx / 64] &= !(1u64 << (idx % 64));
    }

    pub fn test(&self, idx: usize) -> bool {
        assert!(idx < self.len, "bit {idx} out of range [0, {})", self.len);
        (self.words[idx / 64] >> (idx % 64)) & 1 == 1
    }

    /// In-place union (masks accumulate requirement read-sets, effective
    /// attribute sets, writable sets).
    pub fn union_with(&mut self, other: &BitSet) {
        assert_eq!(self.len, other.len, "bitset lengths must match for union");
        for (w, o) in self.words.iter_mut().zip(other.words.iter()) {
            *w |= o;
        }
    }

    /// Keep only bits present in `other` (writable = effective ∩ lock).
    pub fn intersect_with(&mut self, other: &BitSet) {
        assert_eq!(
            self.len, other.len,
            "bitset lengths must match for intersect"
        );
        for (w, o) in self.words.iter_mut().zip(other.words.iter()) {
            *w &= o;
        }
    }

    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn is_clear(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_test_unset_roundtrip() {
        let mut b = BitSet::new(130);
        assert!(b.is_clear());
        b.set(0);
        b.set(64);
        b.set(129);
        assert!(b.test(0) && b.test(64) && b.test(129));
        assert!(!b.test(1));
        assert_eq!(b.count(), 3);
        b.unset(64);
        assert!(!b.test(64));
        assert_eq!(b.count(), 2);
    }

    #[test]
    fn union_intersect_and_equality() {
        let mut a = BitSet::new(10);
        let mut b = BitSet::new(10);
        a.set(1);
        a.set(2);
        b.set(2);
        b.set(3);
        a.union_with(&b);
        assert_eq!(a.count(), 3);
        assert!(a.test(1) && a.test(2) && a.test(3));
        a.intersect_with(&b);
        assert_eq!(a.count(), 2, "a∩b = {{2,3}}");
        assert!(a.test(2) && a.test(3));
        assert_eq!(a, b); // byte equality = determinism check
    }

    #[test]
    fn out_of_range_panics_as_invariant() {
        let b = BitSet::new(1);
        let result = std::panic::catch_unwind(|| b.test(1));
        assert!(result.is_err(), "range violations are invariant panics");
    }
}
