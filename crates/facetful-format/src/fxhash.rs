//! The hash maps of the engine: multiply-fold hashing (FxHash) with a mixing
//! finish. std's default SipHash and its random keys buy nothing here (the
//! keys are the data's own) and cost code and time. The finish matters:
//! a multiply alone leaves the low bits, which pick the bucket, depending
//! only on the key's low bits, and keys that differ in their high bits only
//! (round doubles, packed ids) would share a few buckets; it mixes fully.

use std::hash::{BuildHasher, Hasher};

#[derive(Default, Clone, Copy)]
pub struct FxBuild;

#[derive(Default)]
pub struct FxHasher(u64);

pub type HashMap<K, V> = std::collections::HashMap<K, V, FxBuild>;
pub type HashSet<K> = std::collections::HashSet<K, FxBuild>;

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517C_C1B7_2722_0A95);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(w));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    /// MurmurHash3's 64-bit finalizer: every bit reaches every other, so
    /// the low bits vary even when the keys' low bits never do (a rotate
    /// alone falls short for round doubles, whose low 40-odd bits are 0).
    #[inline]
    fn finish(&self) -> u64 {
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 33;
        h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
        h ^ (h >> 33)
    }
}

impl BuildHasher for FxBuild {
    type Hasher = FxHasher;
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys that differ only in their high bits still spread over buckets:
    /// round doubles, ids packed above 32 bits, and plain small integers.
    #[test]
    fn high_bit_keys_spread() {
        let spread = |keys: &mut dyn Iterator<Item = u64>| keys.map(|k| FxBuild.hash_one(k) & 127).collect::<HashSet<u64>>().len();
        assert!(spread(&mut (0..1000u64).map(|i| (i as f64 * 1024.0).to_bits())) > 100);
        assert!(spread(&mut (0..1000u64).map(|i| (i as f64).to_bits())) > 100);
        assert!(spread(&mut (0..1000u64).map(|i| i << 40)) > 100);
        assert!(spread(&mut (0..1000u64)) > 100);
    }
}
