//! The value set behind `x IN (select k …)`, a single-key `EXISTS`, and a
//! long literal `IN (…)` list: built once, then every row is one lookup —
//! not a per-row scan of the list, and not a joined copy of the table.
//!
//! A set rides the expression tree as a leaf (`Expr::Set` / `Bound::Set`),
//! the second argument of `in_set` or `exists_set`. Its identity is a digest
//! of its members, so a WHERE conjunct over it keys the mask cache by what
//! it contains: the same set reached by another subquery shares the mask,
//! and a table reloaded under the same name can never serve a stale one.
//!
//! Equality follows `Val::eq_sql`: numbers compare as f64, text as bytes.
//!
//! Size (2026-09-28): the set path costs ~11 KB gzipped of wasm (278 → 291
//! KB). If the budget tightens, these are extras past the core (this module,
//! `in_set` and its kernel, the rewrite in `expand_in_expr`, `key_set_of`,
//! `strip_own_alias`) and can go, each re-measured on the IATI search batch:
//! - long literal `IN (…)` lists bound as sets (`literal_set` in binder.rs);
//! - single-key `EXISTS` as `exists_set` (the join path handles it);
//! - the raw-segment integer sweep in `where_mask` (`int_in_set`, filter.rs);
//! - row-group pruning by the set's min/max (`walk_and`, filter.rs);
//! - per-type kernels in `in_set_vec` (raw text blob, per-code dict table)
//!   and the dense bitmap here, both over a generic per-row lookup;
//! - fusing `IN` statements in `run_batch_with` (they still share the mask
//!   through the cache when run one by one).
//! Removed one at a time these measured 0.3-1.7 KB each; most of the cost
//! is code they share. A caller-supplied row mask (planned with full-text
//! search) would cover search without SQL `IN` at all.

use super::binder::Ty;
use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};

/// Members as a bitmap when every one is an integer and the range is at most
/// this many bits, or 64 per member (a dense id domain: 505K activity ids
/// are 64 KB).
const DENSE_BITS: i64 = 1 << 20;

pub struct KeySet {
    /// the members' type; Null when there is no non-NULL member
    ty: Ty,
    has_null: bool,
    /// numeric members as f64 bits (bools as 0 / 1)
    nums: HashSet<u64, Mix>,
    /// the numeric members as bits over `lo..`, when they are all integers
    /// in a compact range
    dense: Option<(i64, Vec<u64>)>,
    range: Option<(f64, f64)>,
    texts: HashSet<Box<[u8]>, Mix>,
    len: usize,
    digest: (u64, u64),
}

impl KeySet {
    /// `nums` for Int / Float / Bool / Date / Timestamp members, `texts` for
    /// Text; `ty` the members' type (Null when both are empty).
    pub fn new(ty: Ty, mut nums: Vec<f64>, mut texts: Vec<Vec<u8>>, has_null: bool) -> KeySet {
        let mut bits: Vec<u64> = nums.iter().map(|x| x.to_bits()).collect();
        bits.sort_unstable();
        bits.dedup();
        texts.sort_unstable();
        texts.dedup();
        // canonical digest: two independent 64-bit hashes over the sorted members
        let (mut h1, mut h2) = (Digest::new(0x243F_6A88_85A3_08D3), Digest::new(0x1319_8A2E_0370_7344));
        for h in [&mut h1, &mut h2] {
            h.word(ty as u64);
            h.word(has_null as u64);
            h.word(bits.len() as u64);
            for &b in &bits {
                h.word(b);
            }
            h.word(texts.len() as u64);
            for t in &texts {
                h.bytes(t);
            }
        }
        nums.retain(|x| !x.is_nan());
        let range = nums.iter().fold(None, |acc: Option<(f64, f64)>, &x| match acc {
            None => Some((x, x)),
            Some((lo, hi)) => Some((lo.min(x), hi.max(x))),
        });
        let exact = |x: f64| x.fract() == 0.0 && x.abs() < 9e15 && x.to_bits() != (-0.0f64).to_bits();
        let dense = match range {
            Some((lo, hi)) if nums.iter().all(|&x| exact(x)) => {
                let (lo, hi) = (lo as i64, hi as i64);
                let span = hi - lo + 1;
                if span <= DENSE_BITS.max(64 * bits.len() as i64) {
                    let mut words = vec![0u64; (span as usize).div_ceil(64)];
                    for &x in &nums {
                        let o = (x as i64 - lo) as usize;
                        words[o / 64] |= 1 << (o % 64);
                    }
                    Some((lo, words))
                } else {
                    None
                }
            }
            _ => None,
        };
        let len = bits.len() + texts.len();
        KeySet {
            ty,
            has_null,
            nums: if dense.is_some() { HashSet::default() } else { bits.into_iter().collect() },
            dense,
            range,
            texts: texts.into_iter().map(Vec::into_boxed_slice).collect(),
            len,
            digest: (h1.finish(), h2.finish()),
        }
    }

    pub fn ty(&self) -> Ty {
        self.ty
    }
    /// Whether a NULL is among the members: a miss is then NULL, not FALSE
    /// (so `NOT IN` is never TRUE).
    pub fn has_null(&self) -> bool {
        self.has_null
    }
    /// The number of distinct non-NULL members.
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Smallest and largest numeric member, for row-group pruning.
    pub fn range(&self) -> Option<(f64, f64)> {
        self.range
    }

    #[inline]
    pub fn contains_i64(&self, x: i64) -> bool {
        match &self.dense {
            Some((lo, words)) => {
                // no wrap: out-of-range values fail the bounds test either way
                let o = x.wrapping_sub(*lo) as u64;
                o < words.len() as u64 * 64 && words[(o / 64) as usize] >> (o % 64) & 1 != 0
            }
            None => self.nums.contains(&(x as f64).to_bits()),
        }
    }
    #[inline]
    pub fn contains_f64(&self, x: f64) -> bool {
        match &self.dense {
            // the dense members are exact integers other than -0.0
            Some(_) => x.fract() == 0.0 && x.abs() < 9e15 && x.to_bits() != (-0.0f64).to_bits() && self.contains_i64(x as i64),
            None => self.nums.contains(&x.to_bits()),
        }
    }
    #[inline]
    pub fn contains_bytes(&self, s: &[u8]) -> bool {
        self.texts.contains(s)
    }
}

impl std::fmt::Debug for KeySet {
    // the mask cache keys a conjunct on its Debug form: the digest, never the members
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeySet({}, {}, {}, {:016x}{:016x})", self.ty.name(), self.len, self.has_null, self.digest.0, self.digest.1)
    }
}

impl PartialEq for KeySet {
    fn eq(&self, other: &Self) -> bool {
        (self.ty, self.has_null, self.len, self.digest) == (other.ty, other.has_null, other.len, other.digest)
    }
}

/// A seeded multiply-xorshift stream hash for the digest.
struct Digest(u64);

impl Digest {
    fn new(seed: u64) -> Digest {
        Digest(seed)
    }
    fn word(&mut self, w: u64) {
        self.0 = (self.0 ^ w).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) ^ self.0.rotate_right(17);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.word(b.len() as u64);
        for c in b.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.word(u64::from_le_bytes(w));
        }
    }
    fn finish(&self) -> u64 {
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 33;
        h
    }
}

/// Multiply-fold hashing for the member lookups: SipHash's quality buys
/// nothing here and costs more than the probe.
#[derive(Default, Clone, Copy)]
struct Mix;

#[derive(Default)]
struct MixHasher(u64);

impl Hasher for MixHasher {
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.write_u64(u64::from_le_bytes(w));
        }
    }
    #[inline]
    fn write_u64(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517C_C1B7_2722_0A95);
    }
    #[inline]
    fn finish(&self) -> u64 {
        let h = self.0 ^ (self.0 >> 32);
        h.wrapping_mul(0xD6E8_FEB8_6659_FD93) ^ (h >> 29)
    }
}

impl BuildHasher for Mix {
    type Hasher = MixHasher;
    fn build_hasher(&self) -> MixHasher {
        MixHasher::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_compare_as_sql_does() {
        let s = KeySet::new(Ty::Int, vec![3.0, 1.0, 3.0, 500_000.0], vec![], false);
        assert_eq!(s.len(), 3);
        assert!(s.dense.is_some());
        assert!(s.contains_i64(1) && s.contains_i64(500_000) && !s.contains_i64(2) && !s.contains_i64(-1));
        assert!(s.contains_f64(3.0) && !s.contains_f64(3.5));
        assert!(!s.contains_i64(i64::MIN) && !s.contains_i64(i64::MAX));
        // sparse: a hash of f64 bits
        let sparse = KeySet::new(Ty::Float, vec![1.5, 1e18, -7.0], vec![], false);
        assert!(sparse.dense.is_none());
        assert!(sparse.contains_f64(1.5) && sparse.contains_i64(-7) && sparse.contains_i64(1_000_000_000_000_000_000));
        assert!(!sparse.contains_f64(2.5));
        assert_eq!(sparse.range(), Some((-7.0, 1e18)));
    }

    #[test]
    fn identity_is_the_members() {
        let a = KeySet::new(Ty::Text, vec![], vec![b"b".to_vec(), b"a".to_vec()], false);
        let b = KeySet::new(Ty::Text, vec![], vec![b"a".to_vec(), b"b".to_vec(), b"a".to_vec()], false);
        let c = KeySet::new(Ty::Text, vec![], vec![b"a".to_vec(), b"b".to_vec()], true);
        assert_eq!(a, b);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_ne!(a, c);
        assert!(a.contains_bytes(b"a") && !a.contains_bytes(b"c"));
        assert!(KeySet::new(Ty::Null, vec![], vec![], false).is_empty());
    }
}
