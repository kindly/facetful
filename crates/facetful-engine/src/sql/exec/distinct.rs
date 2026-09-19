//! Distinct-count structures: the last-seen filter, the (group, value) table, the Fx hasher.

// ---------------- aggregation ----------------

/// Drops a value identical to the one its group saw on the previous row it
/// kept. A pure filter in front of an exact accumulator: it can only skip a
/// genuine repeat, so it is correct under any row order, and only its hit rate
/// depends on the layout. Images are usually written in key order (a plant's
/// rows contiguous), and then it turns the per-row cost of a distinct count
/// into a per-(group, value) one.
///
/// It sits in front of the hash paths only. Guarding a bitmap set with it is a
/// loss even when it hits: the load-compare-store chain through one group's
/// slot is a longer dependency than the `or` it saves.
#[derive(Default)]
pub(super) struct LastSeen {
    pub(super) v: Vec<u64>,
    pub(super) seen: Vec<bool>,
}

impl LastSeen {
    pub(super) fn grow(&mut self, n: usize) {
        self.v.resize(n, 0);
        self.seen.resize(n, false);
    }
    #[inline]
    pub(super) fn repeat(&mut self, g: usize, v: u64) -> bool {
        if self.seen[g] && self.v[g] == v {
            return true;
        }
        self.v[g] = v;
        self.seen[g] = true;
        false
    }
}

/// Fold the high half down, then multiply: every input bit reaches the top
/// bits, which is where table indices are taken from (`>> shift`).
#[inline]
pub(crate) fn mix64(mut h: u64) -> u64 {
    h ^= h >> 32;
    h.wrapping_mul(0xD6E8_FEB8_6659_FD93)
}

/// `g == EMPTY` marks a free slot. `for_kept` already skips `u32::MAX` group
/// ids, so no live group can collide with the sentinel.
#[derive(Clone, Copy)]
pub(super) struct Slot {
    pub(super) v: u64,
    pub(super) g: u32,
}

pub(super) const EMPTY: u32 = u32::MAX;

/// Distinct `u64` values per group: ONE open-addressed table keyed by
/// (group, value) rather than a hash set per group — a single allocation, no
/// per-group indirection, linear probing, and a multiply-shift hash, since
/// SipHash's quality buys nothing on an integer key and costs more than the
/// probe it guards. Fronted by [`LastSeen`].
pub(super) struct DistinctU64 {
    pub(super) slots: Vec<Slot>,
    /// `slots.len() - 1`; probes wrap with it.
    pub(super) mask: usize,
    /// `64 - log2(slots.len())`; the index is the hash's top bits.
    pub(super) shift: u32,
    pub(super) used: usize,
    pub(super) counts: Vec<i64>,
    pub(super) last: LastSeen,
}

impl DistinctU64 {
    pub(super) fn new() -> DistinctU64 {
        DistinctU64 {
            slots: Vec::new(),
            mask: 0,
            shift: 0,
            used: 0,
            counts: Vec::new(),
            last: LastSeen::default(),
        }
    }
    pub(super) fn grow(&mut self, n: usize) {
        self.counts.resize(n, 0);
        self.last.grow(n);
    }
    /// A product bit depends only on input bits at or below it, so the index
    /// has to come from the TOP of the product, and the high half of `v` has
    /// to be folded down first — otherwise doubles such as 50.0 / 100.0 /
    /// 200.0, which differ only in exponent and top mantissa bits, all land
    /// in one probe chain. Only valid once `resize` has run.
    #[inline]
    pub(super) fn index(&self, g: u32, v: u64) -> usize {
        (mix64(v ^ (g as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> self.shift) as usize
    }
    /// Counts `v` for group `g` unless that group already holds it.
    #[inline]
    pub(super) fn insert(&mut self, g: usize, v: u64) {
        if !self.last.repeat(g, v) {
            self.insert_unfiltered(g as u32, v);
        }
    }
    /// The same insert without the last-seen filter, for bulk loads where the
    /// filter state belongs to the accumulator being replaced.
    pub(super) fn insert_unfiltered(&mut self, g: u32, v: u64) {
        if (self.used + 1) * 4 >= self.slots.len() * 3 {
            self.resize();
        }
        let mut i = self.index(g, v);
        loop {
            let slot = self.slots[i];
            if slot.g == EMPTY {
                self.slots[i] = Slot { v, g };
                self.used += 1;
                self.counts[g as usize] += 1;
                return;
            }
            if slot.g == g && slot.v == v {
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[cold]
    #[inline(never)]
    pub(super) fn resize(&mut self) {
        // 4x while small: rehashing dominates a mid-sized distinct count and
        // every reinsert is a cache miss, so reaching 64K slots in three hops
        // rather than six is a measured third of the work. Past that, 2x —
        // the 16-byte slot would otherwise sit at ~20% load on big counts.
        let len = self.slots.len();
        let cap = if len < 1 << 16 { (len * 4).max(1024) } else { len * 2 };
        let old = std::mem::replace(&mut self.slots, vec![Slot { v: 0, g: EMPTY }; cap]);
        self.mask = cap - 1;
        // u64, not usize: the hash is 64-bit on wasm32 too
        self.shift = (cap as u64).leading_zeros() + 1;
        for slot in old {
            if slot.g != EMPTY {
                let mut i = self.index(slot.g, slot.v);
                while self.slots[i].g != EMPTY {
                    i = (i + 1) & self.mask;
                }
                self.slots[i] = slot;
            }
        }
    }
}

/// Multiply-xor hashing for the text distinct set — same reasoning as
/// [`DistinctU64`], applied to the one distinct path that still needs a map.
#[derive(Default, Clone, Copy)]
pub(super) struct FxBuild;

#[derive(Default)]
pub(super) struct FxHasher {
    pub(super) h: u64,
}

impl FxHasher {
    #[inline]
    pub(super) fn add(&mut self, w: u64) {
        self.h = (self.h.rotate_left(5) ^ w).wrapping_mul(0x517C_C1B7_2722_0A95);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(buf));
        }
        self.add(bytes.len() as u64);
    }
    #[inline]
    fn write_u8(&mut self, b: u8) {
        self.add(b as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.h
    }
}

impl std::hash::BuildHasher for FxBuild {
    type Hasher = FxHasher;
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}
