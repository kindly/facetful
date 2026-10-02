//! The word index behind `search()` (docs/full-text-search.sv, "The word
//! index, built on load"): every folded word of the searched text columns,
//! and per row group the rows each word occurs in.
//!
//! Group-major, so it can be built one row group at a time as an image
//! streams in, and so a query reads one entry per group, the way the
//! executor already runs. Within a group a word's rows are a sorted `u16`
//! array, or a 65,536-bit bitmap once that is smaller (over 4,096 rows),
//! as Roaring does. Dictionary columns are indexed by value, not by row:
//! word -> the dictionary codes whose value holds it. A query turns those
//! codes into a lookup table and reads each group's codes, which keeps the
//! rows of a column like grantnav's Description Common (61K values, about 17
//! words each, over 820K rows) out of the build and out of the row lists.
//!
//! The vocabulary is discovered as the text is read; a last pass renumbers
//! the words into word order, so a prefix is a contiguous range of ids in
//! every group. (A words file prepared beforehand, with the vocabulary and
//! per-group counts for exact allocation, was tried on 3 October and built
//! slower: 1.71 s against 1.31 s natively on grantnav.)

use crate::format::read::ReadAt;
use crate::format::FormatError;
use crate::Table;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Rows above which a word's rows in a group are a bitmap, not an array:
/// 4,096 `u16`s are 8 KB, the size of the bitmap.
const BITMAP_ABOVE: usize = 4096;
const BITMAP_WORDS: usize = 65536 / 64;


/// FxHash (as in rustc): fast for short keys, and the keys are our own data.
#[derive(Default)]
struct Fx(u64);
impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut b = [0u8; 8];
            b[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(b) ^ ((rest.len() as u64) << 59));
        }
    }
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    fn write_u128(&mut self, i: u128) {
        self.add(i as u64);
        self.add((i >> 64) as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
impl Fx {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}
type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

/// Call `emit` with each folded word of `text`: letters and digits, lowercased
/// (Unicode), split on anything else. ASCII text takes a byte loop.
pub fn for_each_word(text: &[u8], buf: &mut Vec<u8>, mut emit: impl FnMut(&[u8])) {
    buf.clear();
    if text.is_ascii() {
        for &b in text {
            if b.is_ascii_alphanumeric() {
                buf.push(b.to_ascii_lowercase());
            } else if !buf.is_empty() {
                emit(buf);
                buf.clear();
            }
        }
    } else {
        let mut tmp = [0u8; 4];
        for c in String::from_utf8_lossy(text).chars() {
            if c.is_alphanumeric() {
                for l in c.to_lowercase() {
                    buf.extend_from_slice(l.encode_utf8(&mut tmp).as_bytes());
                }
            } else if !buf.is_empty() {
                emit(buf);
                buf.clear();
            }
        }
    }
    if !buf.is_empty() {
        emit(buf);
        buf.clear();
    }
}

/// One word's rows in one group: `len` rows of the group's array pool from
/// `start`, or (len == BITMAP) the bitmap at `start` in the bitmap pool.
#[derive(Clone, Copy)]
struct Entry {
    start: u32,
    len: u32,
}
const BITMAP: u32 = u32::MAX;

/// Buffers `from_pairs` reuses across groups: per word id a count, then a
/// position; the ids seen in the group; the rows bucketed by word.
#[derive(Default)]
struct Scratch {
    count: Vec<u32>,
    touched: Vec<u32>,
    rows: Vec<u16>,
    runs: Vec<(u32, usize, usize)>,
}

#[derive(Default)]
struct GroupIndex {
    /// word ids present in the group, ascending, parallel to `entries`
    ids: Vec<u32>,
    entries: Vec<Entry>,
    rows: Vec<u16>,
    bitmaps: Vec<u64>,
}

impl GroupIndex {
    /// From (word id << 16 | row) pairs, pushed in ascending row order with
    /// repeats allowed. A counting sort by word id, not a comparison sort:
    /// each word's rows land in push order, so ascending, and repeats are
    /// dropped in one pass. `scratch` is reused across groups.
    fn from_pairs(pairs: &[u64], scratch: &mut Scratch) -> GroupIndex {
        let Scratch { count, touched, rows, runs } = scratch;
        touched.clear();
        for &p in pairs {
            let id = (p >> 16) as usize;
            if id >= count.len() {
                count.resize(id + 1, 0);
            }
            if count[id] == 0 {
                touched.push(id as u32);
            }
            count[id] += 1;
        }
        touched.sort_unstable();
        // count[id] becomes the bucket's start; ends follow from the next start
        let mut pos = 0u32;
        for &id in touched.iter() {
            let n = count[id as usize];
            count[id as usize] = pos;
            pos += n;
        }
        rows.clear();
        rows.resize(pairs.len(), 0);
        for &p in pairs {
            let id = (p >> 16) as usize;
            rows[count[id] as usize] = p as u16;
            count[id] += 1;
        }
        // count[id] is now the bucket's end; walk the buckets in id order
        let (mut array_rows, mut bitmaps) = (0usize, 0usize);
        let mut start = 0usize;
        runs.clear();
        for &id in touched.iter() {
            let end = count[id as usize] as usize;
            count[id as usize] = 0; // reset for the next group
            let b = &mut rows[start..end];
            // rows arrive ascending (pairs are pushed row by row); drop the
            // repeats of a word within one row in the same pass
            let mut n = 1;
            let mut ordered = true;
            for k in 1..b.len() {
                let (prev, cur) = (b[n - 1], b[k]);
                if cur != prev {
                    ordered &= cur > prev;
                    b[n] = cur;
                    n += 1;
                }
            }
            if !ordered {
                // not reachable from `build`; kept so the list stays a set
                b.sort_unstable();
                n = 1;
                for k in 1..b.len() {
                    if b[k] != b[n - 1] {
                        b[n] = b[k];
                        n += 1;
                    }
                }
            }
            if n > BITMAP_ABOVE {
                bitmaps += 1;
            } else {
                array_rows += n;
            }
            runs.push((id, start, start + n));
            start = end;
        }
        let mut g = GroupIndex {
            ids: Vec::with_capacity(runs.len()),
            entries: Vec::with_capacity(runs.len()),
            rows: Vec::with_capacity(array_rows),
            bitmaps: vec![0; bitmaps * BITMAP_WORDS],
        };
        let mut next_bitmap = 0;
        for &(id, i, j) in runs.iter() {
            g.ids.push(id);
            if j - i > BITMAP_ABOVE {
                let base = next_bitmap * BITMAP_WORDS;
                for &r in &rows[i..j] {
                    g.bitmaps[base + r as usize / 64] |= 1 << (r % 64);
                }
                g.entries.push(Entry { start: next_bitmap as u32, len: BITMAP });
                next_bitmap += 1;
            } else {
                g.entries.push(Entry { start: g.rows.len() as u32, len: (j - i) as u32 });
                g.rows.extend_from_slice(&rows[i..j]);
            }
        }
        g
    }

    /// Give the words their final ids (`rank[old]`) and keep `ids` ascending.
    fn renumber(&mut self, rank: &[u32]) {
        let mut both: Vec<(u32, Entry)> = self.ids.iter().map(|&id| rank[id as usize]).zip(self.entries.iter().copied()).collect();
        both.sort_unstable_by_key(|&(id, _)| id);
        for (k, (id, e)) in both.into_iter().enumerate() {
            self.ids[k] = id;
            self.entries[k] = e;
        }
    }

    /// OR the rows of every word with an id in `ids` into `out`: in word
    /// order, a prefix is one contiguous run of this group's entries.
    fn mark_range(&self, ids: std::ops::Range<u32>, out: &mut [u8]) {
        let lo = self.ids.partition_point(|&id| id < ids.start);
        let hi = lo + self.ids[lo..].partition_point(|&id| id < ids.end);
        for k in lo..hi {
            self.mark_entry(self.entries[k], out);
        }
    }

    /// Rows of this group the row lists give `ids` (for the NULL correction
    /// of a nullable dictionary column, a rare path).
    fn plain_hits(&self, ids: std::ops::Range<u32>, rows: usize) -> Vec<u8> {
        let mut m = vec![0u8; rows];
        self.mark_range(ids, &mut m);
        m
    }

    /// (word, row) pairs held.
    fn pairs(&self) -> usize {
        self.entries
            .iter()
            .map(|e| {
                if e.len == BITMAP {
                    let s = e.start as usize * BITMAP_WORDS;
                    self.bitmaps[s..s + BITMAP_WORDS].iter().map(|b| b.count_ones() as usize).sum()
                } else {
                    e.len as usize
                }
            })
            .sum()
    }

    fn bytes(&self) -> usize {
        self.ids.len() * 4 + self.entries.len() * 8 + self.rows.len() * 2 + self.bitmaps.len() * 8
    }

    /// OR word `id`'s rows into `out` (one byte per row); false when absent.
    #[cfg(test)]
    fn mark(&self, id: u32, out: &mut [u8]) -> bool {
        let Ok(k) = self.ids.binary_search(&id) else { return false };
        self.mark_entry(self.entries[k], out);
        true
    }

    fn mark_entry(&self, e: Entry, out: &mut [u8]) {
        if e.len == BITMAP {
            let s = e.start as usize * BITMAP_WORDS;
            for (w, &bits) in self.bitmaps[s..s + BITMAP_WORDS].iter().enumerate() {
                let mut b = bits;
                while b != 0 {
                    let r = w * 64 + b.trailing_zeros() as usize;
                    if r < out.len() {
                        out[r] = 1;
                    }
                    b &= b - 1;
                }
            }
        } else {
            for &r in &self.rows[e.start as usize..(e.start + e.len) as usize] {
                out[r as usize] = 1;
            }
        }
    }
}

/// Seconds since `t`, natively; wasm32 has no clock in std, so 0 there.
#[cfg(not(target_arch = "wasm32"))]
fn since(t: &std::time::Instant) -> f64 {
    t.elapsed().as_secs_f64()
}
#[cfg(not(target_arch = "wasm32"))]
fn now() -> std::time::Instant {
    std::time::Instant::now()
}
#[cfg(target_arch = "wasm32")]
fn since(_: &()) -> f64 {
    0.0
}
#[cfg(target_arch = "wasm32")]
fn now() {}

/// What a build cost and holds.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexStats {
    pub words: usize,
    /// distinct (word, row) pairs held in row lists (the plain text columns)
    pub pairs: usize,
    /// (word, value) pairs of the dictionary columns
    pub value_pairs: usize,
    /// word entries stored as bitmaps
    pub bitmap_entries: usize,
    /// bytes held by the row lists and per-group directories
    pub list_bytes: usize,
    /// bytes held by the vocabulary (word text, ids, sort order)
    pub vocab_bytes: usize,
    /// native builds only: seconds spent tokenizing dictionaries, reading
    /// rows into (word, row) pairs, turning pairs into row lists, finishing
    pub phases: [f64; 4],
    /// wasm builds only: memory size in MB after the dictionaries, after
    /// half the groups, after all of them, at the end
    pub mem_mb: [u32; 4],
}

#[cfg(target_arch = "wasm32")]
fn mem_mb() -> u32 {
    (core::arch::wasm32::memory_size(0) * 65536 / 1_000_000) as u32
}
#[cfg(not(target_arch = "wasm32"))]
fn mem_mb() -> u32 {
    0
}

pub struct SearchIndex {
    /// the searched columns, ascending
    pub cols: Vec<usize>,
    /// word text by id; ids are in word order, so a prefix is an id range
    words: Words,
    /// rows each word occurs in (over the whole table), by id
    counts: Vec<u32>,
    groups: Vec<GroupIndex>,
    dicts: Vec<DictIndex>,
    phases: [f64; 4],
    mem: [u32; 4],
}

/// The vocabulary's text: every word's bytes in one buffer, in id order.
#[derive(Default)]
struct Words {
    bytes: Vec<u8>,
    /// word i is `bytes[ends[i - 1]..ends[i]]` (from 0 for the first)
    ends: Vec<u32>,
}
impl Words {
    fn len(&self) -> usize {
        self.ends.len()
    }
    #[inline]
    fn get(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[start..self.ends[i] as usize]
    }
    fn push(&mut self, w: &[u8]) {
        self.bytes.extend_from_slice(w);
        self.ends.push(self.bytes.len() as u32);
    }
    /// The first index in `lo..` whose word is not `before(word)`.
    fn partition_from(&self, lo: usize, before: impl Fn(&[u8]) -> bool) -> usize {
        let (mut a, mut b) = (lo, self.len());
        while a < b {
            let m = (a + b) / 2;
            if before(self.get(m)) {
                a = m + 1;
            } else {
                b = m;
            }
        }
        a
    }
}

/// One dictionary column: per word id, the codes whose value holds the word
/// (`codes[offsets[id]..offsets[id + 1]]`).
struct DictIndex {
    col: usize,
    values: usize,
    offsets: Vec<u32>,
    codes: Vec<u32>,
}

/// Per dictionary column of the index: which codes a query word admits, one
/// byte per code (0 or 1) so the per-row lookup is a branchless OR. `None`
/// when the word holds no value of that column.
pub struct ValueSets(Vec<(usize, Option<Vec<u8>>)>);

/// Interns words while building: text -> id. Words of up to 15 bytes are
/// keys by value (bytes plus length in one u128), so a lookup is an integer
/// hash and compare with no pointer to chase; longer words are boxed.
/// A short word's key in the map: 16 bytes stored unaligned (20-byte map
/// slots with the id, where a u128 key's alignment made them 32), hashed as
/// one u128.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ShortKey([u8; 16]);
impl std::hash::Hash for ShortKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        h.write_u128(u128::from_le_bytes(self.0));
    }
}

struct Vocab {
    short: FxMap<ShortKey, u32>,
    long: FxMap<Box<[u8]>, u32>,
    /// direct-mapped cache in front of `short`: word frequencies are skewed,
    /// so a few thousand slots (in cache, unlike the map) answer most lookups
    front: Vec<(u128, u32)>,
}

const FRONT_SLOTS: usize = 1 << 13;

impl Default for Vocab {
    fn default() -> Self {
        // key 0 never occurs (every short key carries its length, at least 1)
        Vocab { short: FxMap::default(), long: FxMap::default(), front: vec![(0, 0); FRONT_SLOTS] }
    }
}
impl Vocab {
    #[inline]
    fn front_slot(key: u128) -> usize {
        let h = ((key as u64) ^ ((key >> 64) as u64).rotate_left(29)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (h >> (64 - 13)) as usize
    }

    /// The id of a word, a new one if unseen.
    #[inline]
    fn key_id(&mut self, k: Key) -> u32 {
        let next = (self.short.len() + self.long.len()) as u32;
        match k {
            Key::Short(key) => {
                let slot = Self::front_slot(key);
                let f = self.front[slot];
                if f.0 == key {
                    return f.1;
                }
                let id = *self.short.entry(ShortKey(key.to_le_bytes())).or_insert(next);
                self.front[slot] = (key, id);
                id
            }
            Key::Long(w) => {
                if let Some(&id) = self.long.get(w) {
                    return id;
                }
                self.long.insert(w.into(), next);
                next
            }
        }
    }

    #[inline]
    fn id(&mut self, w: &[u8]) -> u32 {
        self.key_id(key_of(w))
    }

    /// The words in id order, as one arena.
    fn into_words(self) -> Words {
        let n = self.short.len() + self.long.len();
        // (start, len) per id into one buffer filled in map order
        let mut at = vec![(0u32, 0u8); n];
        let mut bytes = Vec::new();
        let mut long_at: Vec<(u32, u32, u32)> = Vec::with_capacity(self.long.len()); // id, start, len
        for (k, id) in self.short {
            let len = k.0[15] as usize;
            at[id as usize] = (bytes.len() as u32, len as u8);
            bytes.extend_from_slice(&k.0[..len]);
        }
        for (w, id) in self.long {
            long_at.push((id, bytes.len() as u32, w.len() as u32));
            bytes.extend_from_slice(&w);
        }
        let mut long_of = vec![u32::MAX; 0];
        if !long_at.is_empty() {
            long_of = vec![u32::MAX; n];
            for (k, &(id, _, _)) in long_at.iter().enumerate() {
                long_of[id as usize] = k as u32;
            }
        }
        let mut words = Words { bytes: Vec::with_capacity(bytes.len()), ends: Vec::with_capacity(n) };
        for (id, &(start, len)) in at.iter().enumerate() {
            match long_of.get(id).copied().filter(|&k| k != u32::MAX) {
                Some(k) => {
                    let (_, s, l) = long_at[k as usize];
                    words.push(&bytes[s as usize..(s + l) as usize]);
                }
                None => words.push(&bytes[start as usize..start as usize + len as usize]),
            }
        }
        words
    }
}

/// A plain text column's row offsets: raw LE bytes off the image, or copied.
#[derive(Clone, Copy)]
enum Offsets<'a> {
    Le(&'a [u8]),
    Words(&'a [u32]),
}
impl Offsets<'_> {
    #[inline]
    fn at(&self, i: usize) -> usize {
        match self {
            Offsets::Le(o) => u32::from_le_bytes(o[i * 4..i * 4 + 4].try_into().unwrap()) as usize,
            Offsets::Words(o) => o[i] as usize,
        }
    }
}

/// One bit per byte of an ASCII text, set for letters and digits, 64 bytes
/// per word. Written as plain compares so the compiler vectorizes it.
#[inline]
fn word_byte_mask(text: &[u8], masks: &mut Vec<u64>) {
    masks.clear();
    for chunk in text.chunks(64) {
        let mut m = 0u64;
        for (k, &b) in chunk.iter().enumerate() {
            let l = b | 0x20;
            let is = (l.wrapping_sub(b'a') < 26) | (b.wrapping_sub(b'0') < 10);
            m |= (is as u64) << k;
        }
        masks.push(m);
    }
}

/// The next position at or after `from` whose bit is `want`, or `len`.
#[inline]
fn next_bit(masks: &[u64], from: usize, want: bool, len: usize) -> usize {
    let mut w = from / 64;
    if w >= masks.len() {
        return len;
    }
    let flip = if want { 0 } else { u64::MAX };
    let mut cur = (masks[w] ^ flip) & (u64::MAX << (from % 64));
    loop {
        if cur != 0 {
            return (w * 64 + cur.trailing_zeros() as usize).min(len);
        }
        w += 1;
        if w >= masks.len() {
            return len;
        }
        cur = masks[w] ^ flip;
    }
}

/// A folded word as a hash key: up to 15 bytes by value (the bytes
/// little-endian, the length in the top byte), longer ones as bytes.
enum Key<'a> {
    Short(u128),
    Long(&'a [u8]),
}

#[inline]
fn key_of(w: &[u8]) -> Key<'_> {
    if w.len() <= 15 {
        let mut b = [0u8; 16];
        b[..w.len()].copy_from_slice(w);
        b[15] = w.len() as u8;
        Key::Short(u128::from_le_bytes(b))
    } else {
        Key::Long(w)
    }
}

/// Call `f` with the key of every word of one row's text. ASCII text finds
/// its words from a bitmask of letter-and-digit bytes, and a word of up to
/// 15 bytes becomes its key in one 16-byte load: masked to its length and
/// lowercased by setting bit 5 of every byte (which turns letters to
/// lowercase and leaves digits as they are). Other text goes through
/// `for_each_word`. Same words, same order, as `for_each_word`.
#[inline]
fn row_keys(text: &[u8], buf: &mut Vec<u8>, masks: &mut Vec<u64>, mut f: impl FnMut(Key)) {
    if !text.is_ascii() {
        for_each_word(text, buf, |w| f(key_of(w)));
        return;
    }
    let n = text.len();
    word_byte_mask(text, masks);
    let mut i = next_bit(masks, 0, true, n);
    while i < n {
        let end = next_bit(masks, i, false, n);
        let len = end - i;
        if len <= 15 {
            let raw = if i + 16 <= n {
                u128::from_le_bytes(text[i..i + 16].try_into().unwrap())
            } else {
                let mut b = [0u8; 16];
                b[..len].copy_from_slice(&text[i..end]);
                u128::from_le_bytes(b)
            };
            let keep = (1u128 << (8 * len)) - 1;
            let lower = 0x2020_2020_2020_2020_2020_2020_2020_2020u128 & keep;
            f(Key::Short((raw & keep) | lower | (len as u128) << 120));
        } else {
            buf.clear();
            buf.extend(text[i..end].iter().map(|b| b.to_ascii_lowercase()));
            f(Key::Long(buf));
        }
        i = next_bit(masks, end, true, n);
    }
}

/// Plain text columns of one group, row by row across columns: `f(row, key)`
/// for every word, so each word's rows come in ascending order.
fn each_row_word<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    plain: &[usize],
    buf: &mut Vec<u8>,
    masks: &mut Vec<u64>,
    mut f: impl FnMut(usize, Key),
) -> Result<(), FormatError> {
    let rows = table.catalog().groups[g].row_count as usize;
    let mut each = |segs: &[(Offsets, &[u8], Option<&[u8]>)]| {
        for r in 0..rows {
            for (off, blob, valid) in segs {
                if valid.is_none_or(|v| v[r / 8] >> (r % 8) & 1 == 1) {
                    row_keys(&blob[off.at(r)..off.at(r + 1)], buf, masks, |k| f(r, k));
                }
            }
        }
    };
    let done = table.with_text_columns(g, plain, |segs| {
        let segs: Vec<_> = segs.iter().map(|&(o, b, v)| (Offsets::Le(o), b, v)).collect();
        each(&segs);
    })?;
    if done.is_none() {
        let mut owned = Vec::new();
        for &c in plain {
            let (o, b) = table.texts_raw(g, c, rows)?;
            owned.push((o, b, table.validity(g, c)?));
        }
        let segs: Vec<_> = owned.iter().map(|(o, b, v)| (Offsets::Words(o), b.as_slice(), v.as_deref())).collect();
        each(&segs);
    }
    Ok(())
}

/// Invert one dictionary column: word -> the codes whose value holds it.
fn invert_dict(col: usize, per_value: &[Vec<u32>], n_words: usize) -> DictIndex {
    let mut offsets = vec![0u32; n_words + 1];
    for ids in per_value {
        for &id in ids {
            offsets[id as usize + 1] += 1;
        }
    }
    for k in 0..n_words {
        offsets[k + 1] += offsets[k];
    }
    let mut fill = offsets.clone();
    let mut codes = vec![0u32; offsets[n_words] as usize];
    for (code, ids) in per_value.iter().enumerate() {
        for &id in ids {
            codes[fill[id as usize] as usize] = code as u32;
            fill[id as usize] += 1;
        }
    }
    DictIndex { col, values: per_value.len(), offsets, codes }
}

impl SearchIndex {
    /// Index `cols` (text columns) of every row group.
    pub fn build<S: ReadAt>(table: &mut Table<S>, cols: &[usize]) -> Result<SearchIndex, FormatError> {
        let mut cols = cols.to_vec();
        cols.sort_unstable();
        cols.dedup();
        let mut vocab = Vocab::default();
        let mut buf = Vec::new();
        // dictionary columns: each distinct value's word ids, tokenized once
        let mut phases = [0.0f64; 4];
        let t = now();
        // per dictionary column: each value's word ids, and rows per value
        let mut dict_words: FxMap<usize, (Vec<Vec<u32>>, Vec<u32>)> = FxMap::default();
        for &c in &cols {
            if table.catalog().schema.columns[c].is_dict() {
                let dict = table.dictionary_rc(c)?;
                let per_value = dict
                    .iter()
                    .map(|s| {
                        let mut ids = Vec::new();
                        for_each_word(s.as_bytes(), &mut buf, |w| ids.push(vocab.id(w)));
                        ids.sort_unstable();
                        ids.dedup();
                        ids
                    })
                    .collect::<Vec<_>>();
                let n = per_value.len();
                dict_words.insert(c, (per_value, vec![0u32; n]));
            }
        }
        phases[0] = since(&t);
        let mut mem = [mem_mb(), 0, 0, 0];
        let mut groups = Vec::with_capacity(table.group_count());
        // about one word per 7 bytes of text: one buffer sized for the largest
        // group, allocated once (growing it, or regrowing it per group, left
        // holes in wasm's never-returned memory)
        let largest_text = (0..table.group_count())
            .map(|g| {
                cols.iter()
                    .filter(|c| !dict_words.contains_key(c))
                    .map(|&c| table.catalog().groups[g].cols[c].seg_lens[1] as usize)
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        let mut pairs: Vec<u64> = Vec::with_capacity(largest_text / 7);
        let mut scratch = Scratch::default();
        let mut masks = Vec::new();
        for g in 0..table.group_count() {
            let t = now();
            let rows = table.catalog().groups[g].row_count as usize;
            pairs.clear();
            let plain: Vec<usize> = cols.iter().copied().filter(|c| !dict_words.contains_key(c)).collect();
            for (&c, (_, value_rows)) in dict_words.iter_mut() {
                // rows per value, for the words' row counts
                let valid = table.validity(g, c)?;
                for (r, &code) in table.codes(g, c, rows)?.iter().enumerate() {
                    if valid.as_ref().is_none_or(|v| v[r / 8] >> (r % 8) & 1 == 1) {
                        value_rows[code as usize] += 1;
                    }
                }
            }
            // plain columns row by row, across columns: every word's rows then
            // arrive in ascending order, so `from_pairs` only drops repeats
            each_row_word(table, g, &plain, &mut buf, &mut masks, |r, k| {
                pairs.push((vocab.key_id(k) as u64) << 16 | r as u64)
            })?;
            phases[1] += since(&t);
            let t = now();
            groups.push(GroupIndex::from_pairs(&pairs, &mut scratch));
            phases[2] += since(&t);
            if g + 1 == table.group_count() / 2 {
                mem[1] = mem_mb();
            }
        }
        mem[2] = mem_mb();
        let t = now();
        drop(pairs);
        drop(scratch);
        // renumber the words into word order: a prefix becomes an id range
        let unsorted = vocab.into_words();
        let mut order: Vec<u32> = (0..unsorted.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| unsorted.get(a as usize).cmp(unsorted.get(b as usize)));
        let mut rank = vec![0u32; unsorted.len()];
        let mut words = Words { bytes: Vec::with_capacity(unsorted.bytes.len()), ends: Vec::with_capacity(unsorted.len()) };
        for (r, &id) in order.iter().enumerate() {
            rank[id as usize] = r as u32;
            words.push(unsorted.get(id as usize));
        }
        drop(order);
        drop(unsorted);
        for g in &mut groups {
            g.renumber(&rank);
        }
        let mut counts = vec![0u32; words.len()];
        // invert each dictionary column: word -> codes (CSR); a word's row
        // count adds its values' rows (a row holding the word in two columns
        // counts twice: the counts guide "too broad" and spell checking)
        let mut dicts = Vec::new();
        let mut dict_cols: Vec<usize> = dict_words.keys().copied().collect();
        dict_cols.sort_unstable();
        for c in dict_cols {
            let (mut per_value, value_rows) = dict_words.remove(&c).unwrap();
            for (code, ids) in per_value.iter_mut().enumerate() {
                for id in ids.iter_mut() {
                    *id = rank[*id as usize];
                    counts[*id as usize] += value_rows[code];
                }
            }
            dicts.push(invert_dict(c, &per_value, words.len()));
        }
        for g in &groups {
            for (k, &id) in g.ids.iter().enumerate() {
                let e = g.entries[k];
                counts[id as usize] += if e.len == BITMAP {
                    let s = e.start as usize * BITMAP_WORDS;
                    g.bitmaps[s..s + BITMAP_WORDS].iter().map(|b| b.count_ones()).sum()
                } else {
                    e.len
                };
            }
        }
        phases[3] = since(&t);
        mem[3] = mem_mb();
        Ok(SearchIndex { cols, words, counts, groups, dicts, phases, mem })
    }

    pub fn stats(&self) -> IndexStats {
        IndexStats {
            words: self.words.len(),
            pairs: self.groups.iter().map(|g| g.pairs()).sum(),
            value_pairs: self.dicts.iter().map(|d| d.codes.len()).sum(),
            bitmap_entries: self.groups.iter().map(|g| g.entries.iter().filter(|e| e.len == BITMAP).count()).sum(),
            list_bytes: self.groups.iter().map(|g| g.bytes()).sum::<usize>()
                + self.dicts.iter().map(|d| d.offsets.len() * 4 + d.codes.len() * 4).sum::<usize>(),
            vocab_bytes: self.words.bytes.len() + self.words.ends.len() * 4 + self.counts.len() * 4,
            phases: self.phases,
            mem_mb: self.mem,
        }
    }

    /// Ids of every word starting with `prefix` (folded): a range, since
    /// ids are in word order.
    pub fn prefix_ids(&self, prefix: &[u8]) -> std::ops::Range<u32> {
        let lo = self.words.partition_from(0, |w| w < prefix);
        let hi = self.words.partition_from(lo, |w| w.starts_with(prefix));
        lo as u32..hi as u32
    }

    /// The id of exactly `word`, if indexed.
    pub fn word_id(&self, word: &[u8]) -> Option<u32> {
        let k = self.words.partition_from(0, |w| w < word);
        (k < self.words.len() && self.words.get(k) == word).then_some(k as u32)
    }

    pub fn word(&self, id: u32) -> &[u8] {
        self.words.get(id as usize)
    }

    /// Rows (over the whole table) word `id` occurs in.
    pub fn rows_of(&self, id: u32) -> u32 {
        self.counts[id as usize]
    }

    /// The dictionary codes any of `ids` occurs in, per dictionary column.
    /// Once per query word; every group's lookup reuses it.
    pub fn value_sets(&self, ids: std::ops::Range<u32>) -> ValueSets {
        ValueSets(
            self.dicts
                .iter()
                .map(|d| {
                    let mut t: Option<Vec<u8>> = None;
                    // the range's codes are one contiguous run of `codes`
                    let run = d.offsets[ids.start as usize] as usize..d.offsets[ids.end as usize] as usize;
                    for &code in &d.codes[run] {
                        t.get_or_insert_with(|| vec![0u8; d.values])[code as usize] = 1;
                    }
                    (d.col, t)
                })
                .collect(),
        )
    }

    /// Mark (one byte per row, set to 1) the rows of group `g` holding any of
    /// `ids`: the plain columns from the row lists, the dictionary columns by
    /// reading the group's codes against `values` (from `value_sets(ids)`).
    pub fn mark_group<S: ReadAt>(
        &self,
        table: &mut Table<S>,
        g: usize,
        ids: std::ops::Range<u32>,
        values: &ValueSets,
        out: &mut [u8],
    ) -> Result<(), FormatError> {
        self.groups[g].mark_range(ids.clone(), out);
        let rows = out.len();
        for (c, t) in &values.0 {
            let Some(t) = t else { continue };
            let done = table.with_code_segments(g, *c, |codes, width, valid| {
                if width == 1 {
                    for (o, &code) in out.iter_mut().zip(&codes[..rows]) {
                        *o |= t[code as usize];
                    }
                } else {
                    for (o, c) in out.iter_mut().zip(codes[..rows * 2].chunks_exact(2)) {
                        *o |= t[u16::from_le_bytes([c[0], c[1]]) as usize];
                    }
                }
                valid.map(|v| v.to_vec())
            })?;
            let valid = match done {
                Some(v) => v,
                None => {
                    let codes = table.codes(g, *c, rows)?;
                    for (o, &code) in out.iter_mut().zip(&codes) {
                        *o |= t[code as usize];
                    }
                    table.validity(g, *c)?
                }
            };
            // NULL rows carry some code; a hit there is not a hit
            if let Some(v) = valid {
                let plain = self.groups[g].plain_hits(ids.clone(), rows);
                for (r, o) in out.iter_mut().enumerate() {
                    if v[r / 8] >> (r % 8) & 1 == 0 && plain[r] == 0 {
                        *o = 0;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{for_each_word, GroupIndex, BITMAP};

    #[test]
    fn arrays_and_bitmaps() {
        // word 7 in every 3rd row of a full group (21,846 rows: a bitmap), word 2 in three rows
        let mut pairs: Vec<u64> = (0..65536u64).step_by(3).map(|r| 7 << 16 | r).collect();
        pairs.extend([2u64 << 16 | 9, 2 << 16 | 1, 2 << 16 | 9, 2 << 16 | 65535]);
        let g = GroupIndex::from_pairs(&pairs, &mut super::Scratch::default());
        assert_eq!(g.ids, [2, 7]);
        assert_eq!(g.entries[0].len, 3);
        assert_eq!(g.entries[1].len, BITMAP);
        let mut out = vec![0u8; 65536];
        assert!(g.mark(7, &mut out));
        assert!(out.iter().enumerate().all(|(r, &b)| (b == 1) == (r % 3 == 0)));
        let mut out = vec![0u8; 65536];
        assert!(g.mark(2, &mut out) && !g.mark(5, &mut out));
        assert_eq!(out.iter().enumerate().filter(|(_, &b)| b == 1).map(|(r, _)| r).collect::<Vec<_>>(), [1, 9, 65535]);
    }

    fn words(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = Vec::new();
        for_each_word(s.as_bytes(), &mut buf, |w| out.push(String::from_utf8(w.to_vec()).unwrap()));
        out
    }

    #[test]
    fn folding_and_splitting() {
        assert_eq!(words("Mental-health, LONDON 2024!"), ["mental", "health", "london", "2024"]);
        assert_eq!(words("São Paulo ÉCOLE"), ["são", "paulo", "école"]);
        assert_eq!(words("  "), Vec::<String>::new());
        assert_eq!(words("a"), ["a"]);
    }
}
