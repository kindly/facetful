//! The word index behind `search()` (docs/full-text-search.sv, "The word
//! index, built on load"): every folded word of the searched text columns,
//! and per row group the rows each word occurs in.
//!
//! Group-major, so it can be built one row group at a time as an image
//! streams in, and so a query reads one entry per group, the way the
//! executor already runs. Within a group a word has a row list per plain
//! field (so ranking and field scoping know which field a row matched in),
//! each a sorted `u16` array, or a 65,536-bit bitmap once that is smaller
//! (over 4,096 rows), as Roaring does. Dictionary columns are indexed by value, not by row:
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

/// Call `emit` with each word of `text`, folded: the one tokenizer of the
/// index, the queries and the scan.
///
/// - A word is a run of letters, digits and combining marks (Unicode).
/// - Lowercased (Unicode), and accents folded: a letter whose canonical
///   decomposition is a letter plus accents in U+0300-036F becomes that
///   letter (`café` and `cafe`, `İ` and `i`), and such accents written
///   separately are dropped. Other marks stay (Devanagari, Thai vowel signs).
/// - An apostrophe (`'` or `’`) after a letter or digit: a possessive `'s`
///   ending the word goes (`children's` is `children`, `90's` is `90`);
///   between two letters or two digits the apostrophe goes and the word runs
///   on (`o'brien` is `obrien`), as UAX #29 keeps such words whole. Elsewhere
///   it separates, like every other character (`mental-health`, `U.K.`).
pub fn for_each_word(text: &[u8], buf: &mut Vec<u8>, mut emit: impl FnMut(&[u8])) {
    buf.clear();
    if text.is_ascii() {
        let mut i = 0;
        while i < text.len() {
            let b = text[i];
            if b.is_ascii_alphanumeric() {
                buf.push(b.to_ascii_lowercase());
            } else if b == b'\'' && !buf.is_empty() && text.get(i + 1).is_some_and(|c| c.is_ascii_alphanumeric()) {
                let (prev, next) = (*buf.last().unwrap(), text[i + 1]);
                if matches!(next, b's' | b'S') && !text.get(i + 2).is_some_and(|c| c.is_ascii_alphanumeric()) {
                    i += 1; // a possessive 's: the word ends before it
                    emit(buf);
                    buf.clear();
                } else if prev.is_ascii_digit() != next.is_ascii_digit() {
                    emit(buf); // a letter and a digit: two words
                    buf.clear();
                }
            } else if !buf.is_empty() {
                emit(buf);
                buf.clear();
            }
            i += 1;
        }
    } else {
        let mut tmp = [0u8; 4];
        let s = String::from_utf8_lossy(text);
        // ASCII characters (most, even here) skip the table lookups
        let in_word = |c: char| if c.is_ascii() { c.is_ascii_alphanumeric() } else { c.is_alphanumeric() || is_mark(c) };
        let mut chars = s.chars().peekable();
        // whether the word's last letter or digit was a digit
        let mut last_digit = false;
        while let Some(c) = chars.next() {
            if (0x300..0x370).contains(&(c as u32)) {
                continue; // an accent written separately: dropped, the word runs on
            }
            if c.is_ascii_alphanumeric() {
                buf.push(c.to_ascii_lowercase() as u8);
                last_digit = c.is_ascii_digit();
            } else if in_word(c) {
                for l in fold(c).to_lowercase() {
                    buf.extend_from_slice(fold(l).encode_utf8(&mut tmp).as_bytes());
                }
                if c.is_alphanumeric() {
                    last_digit = c.is_numeric();
                }
            } else if (c == '\'' || c == '\u{2019}') && !buf.is_empty() && chars.peek().is_some_and(|&n| n.is_alphanumeric()) {
                let mut ahead = chars.clone();
                let next = ahead.next().unwrap();
                if matches!(next, 's' | 'S') && !ahead.next().is_some_and(in_word) {
                    chars.next(); // a possessive 's: the word ends before it
                    emit(buf);
                    buf.clear();
                } else if last_digit != next.is_numeric() {
                    emit(buf); // a letter and a digit: two words
                    buf.clear();
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

/// A letter's base letter when its canonical decomposition is that letter
/// plus accents in U+0300-036F (`é` is `e`); itself otherwise.
fn fold(c: char) -> char {
    if (c as u32) < 0xC0 {
        return c; // nothing below À folds
    }
    let t = &crate::unicode_tables::FOLD;
    match t.binary_search_by_key(&(c as u32), |&(k, _)| k) {
        Ok(i) => char::from_u32(t[i].1).unwrap_or(c),
        Err(_) => c,
    }
}

/// A combining mark (general category M): part of the word it follows.
fn is_mark(c: char) -> bool {
    let t = &crate::unicode_tables::MARKS;
    let c = c as u32;
    let i = t.partition_point(|&(lo, _)| lo <= c);
    i > 0 && c <= t[i - 1].1
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

    /// Give the lists their final ids (`to(old)`) and keep `ids` ascending.
    fn renumber(&mut self, to: impl Fn(u32) -> u32) {
        let mut both: Vec<(u32, Entry)> = self.ids.iter().map(|&id| to(id)).zip(self.entries.iter().copied()).collect();
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

    /// Calls `f` with each row of one list.
    #[inline]
    fn each_row(&self, e: Entry, mut f: impl FnMut(usize)) {
        if e.len == BITMAP {
            let s = e.start as usize * BITMAP_WORDS;
            for (w, &bits) in self.bitmaps[s..s + BITMAP_WORDS].iter().enumerate() {
                let mut b = bits;
                while b != 0 {
                    f(w * 64 + b.trailing_zeros() as usize);
                    b &= b - 1;
                }
            }
        } else {
            for &r in &self.rows[e.start as usize..(e.start + e.len) as usize] {
                f(r as usize);
            }
        }
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
    /// (word, field, row) entries held in row lists (the plain text columns;
    /// a word in two fields of a row counts twice)
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
    /// the plain text columns among them: field `f` of a row list is `plain[f]`
    pub plain: Vec<usize>,
    /// row lists per word: one per plain field, word `w`'s for field `f` at
    /// list id `w * fields + f`, so a word range is a list range too
    fields: u32,
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
pub struct ValueSets(pub(crate) Vec<(usize, Option<Vec<u8>>)>);

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
    let alnum = |j: usize| j < n && text[j].is_ascii_alphanumeric();
    let mut i = next_bit(masks, 0, true, n);
    while i < n {
        let mut end = next_bit(masks, i, false, n);
        if alnum(end + 1) && text[end] == b'\'' {
            // the word may run on past an apostrophe, or end in a possessive
            // 's: read as `for_each_word` reads it
            buf.clear();
            buf.extend(text[i..end].iter().map(|b| b.to_ascii_lowercase()));
            let mut resume = None;
            while alnum(end + 1) && text[end] == b'\'' {
                let (prev, next) = (*buf.last().unwrap(), text[end + 1]);
                if matches!(next, b's' | b'S') && !alnum(end + 2) {
                    resume = Some(end + 2); // a possessive 's
                    break;
                }
                if prev.is_ascii_digit() != next.is_ascii_digit() {
                    resume = Some(end + 1); // a letter and a digit: two words
                    break;
                }
                let run = next_bit(masks, end + 1, false, n);
                buf.extend(text[end + 1..run].iter().map(|b| b.to_ascii_lowercase()));
                end = run;
            }
            f(key_of(buf));
            i = next_bit(masks, resume.unwrap_or(end), true, n);
            continue;
        }
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

/// Plain text columns of one group, row by row across columns: `f(row,
/// field, key)` for every word (field: the column's place in `plain`), so
/// each word's rows come in ascending order.
fn each_row_word<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    plain: &[usize],
    buf: &mut Vec<u8>,
    masks: &mut Vec<u64>,
    mut f: impl FnMut(usize, usize, Key),
) -> Result<(), FormatError> {
    let rows = table.catalog().groups[g].row_count as usize;
    let mut each = |segs: &[(Offsets, &[u8], Option<&[u8]>)]| {
        for r in 0..rows {
            for (field, (off, blob, valid)) in segs.iter().enumerate() {
                if valid.is_none_or(|v| v[r / 8] >> (r % 8) & 1 == 1) {
                    row_keys(&blob[off.at(r)..off.at(r + 1)], buf, masks, |k| f(r, field, k));
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

/// A build in progress: `new` reads the dictionary columns, `add_group`
/// indexes one row group (in order), `finish` renumbers the words and
/// returns the index. Between groups the caller is free to do other work:
/// the worker answers queries, or (streaming) waits for the next group.
pub struct IndexBuilder {
    cols: Vec<usize>,
    plain: Vec<usize>,
    vocab: Vocab,
    /// per dictionary column: each value's word ids, and rows per value
    dict_words: FxMap<usize, (Vec<Vec<u32>>, Vec<u32>)>,
    groups: Vec<GroupIndex>,
    pairs: Vec<u64>,
    scratch: Scratch,
    buf: Vec<u8>,
    masks: Vec<u64>,
    phases: [f64; 4],
    mem: [u32; 4],
}

impl IndexBuilder {
    /// Start indexing `cols` (text columns): the dictionary columns are
    /// tokenized here, once per distinct value.
    pub fn new<S: ReadAt>(table: &mut Table<S>, cols: &[usize]) -> Result<IndexBuilder, FormatError> {
        let mut cols = cols.to_vec();
        cols.sort_unstable();
        cols.dedup();
        check_table(table.catalog(), &cols)?;
        let t = now();
        let mut vocab = Vocab::default();
        let mut buf = Vec::new();
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
        let plain: Vec<usize> = cols.iter().copied().filter(|c| !dict_words.contains_key(c)).collect();
        // about one word per 7 bytes of text: one buffer sized for the largest
        // group, allocated once (growing it, or regrowing it per group, left
        // holes in wasm's never-returned memory)
        let largest_text = (0..table.group_count())
            .map(|g| plain.iter().map(|&c| table.catalog().groups[g].cols[c].seg_lens[1] as usize).sum::<usize>())
            .max()
            .unwrap_or(0);
        let mut phases = [0.0f64; 4];
        phases[0] = since(&t);
        Ok(IndexBuilder {
            cols,
            plain,
            vocab,
            dict_words,
            groups: Vec::with_capacity(table.group_count()),
            pairs: Vec::with_capacity(largest_text / 7),
            scratch: Scratch::default(),
            buf,
            masks: Vec::new(),
            phases,
            mem: [mem_mb(), 0, 0, 0],
        })
    }

    /// Row groups indexed so far; the next `add_group` takes this one.
    pub fn groups_done(&self) -> usize {
        self.groups.len()
    }

    /// Index the next row group.
    pub fn add_group<S: ReadAt>(&mut self, table: &mut Table<S>) -> Result<(), FormatError> {
        let g = self.groups.len();
        let t = now();
        if table.catalog().groups[g].row_count > MAX_GROUP_ROWS {
            return Err(TOO_BIG); // a group that arrived after the build began
        }
        let rows = table.catalog().groups[g].row_count as usize;
        for (&c, (_, value_rows)) in self.dict_words.iter_mut() {
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
        let IndexBuilder { vocab, pairs, buf, masks, .. } = self;
        pairs.clear();
        // a build that starts before the groups are known (an image still
        // arriving) sizes the buffer here, with room to spare, not by doubling
        let text: usize = self.plain.iter().map(|&c| table.catalog().groups[g].cols[c].seg_lens[1] as usize).sum();
        if pairs.capacity() < text / 7 {
            pairs.reserve_exact(text / 7 * 5 / 4);
        }
        let fields = self.plain.len().max(1) as u64;
        each_row_word(table, g, &self.plain, buf, masks, |r, field, k| {
            pairs.push((vocab.key_id(k) as u64 * fields + field as u64) << 16 | r as u64)
        })?;
        self.phases[1] += since(&t);
        let t = now();
        self.groups.push(GroupIndex::from_pairs(&self.pairs, &mut self.scratch));
        self.phases[2] += since(&t);
        if g + 1 == table.group_count() / 2 {
            self.mem[1] = mem_mb();
        }
        Ok(())
    }

    /// Renumber the words into word order (a prefix becomes an id range),
    /// invert the dictionary columns, and count each word's rows.
    pub fn finish(self) -> SearchIndex {
        let IndexBuilder { cols, plain, vocab, mut dict_words, mut groups, mut phases, mut mem, .. } = self;
        let fields = plain.len().max(1) as u32;
        mem[2] = mem_mb();
        let t = now();
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
            g.renumber(|list| rank[(list / fields) as usize] * fields + list % fields);
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
            for (k, &list) in g.ids.iter().enumerate() {
                let e = g.entries[k];
                counts[(list / fields) as usize] += if e.len == BITMAP {
                    let s = e.start as usize * BITMAP_WORDS;
                    g.bitmaps[s..s + BITMAP_WORDS].iter().map(|b| b.count_ones()).sum()
                } else {
                    e.len
                };
            }
        }
        phases[3] = since(&t);
        mem[3] = mem_mb();
        SearchIndex { cols, plain, fields, words, counts, groups, dicts, phases, mem }
    }
}

/// A range of word ids (ids are in word order, so a prefix is one).
pub type Ids = std::ops::Range<u32>;

/// Edits a word of `n` bytes may be off by and still count as a typo of
/// another: none under 4 (`cat` would find `car`, `hat`, `cut`), 1 up to 7,
/// 2 from 8. Algolia's defaults; bytes, so before accent folding a
/// non-ASCII letter costs 2.
pub fn max_edits(n: usize) -> usize {
    match n {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// Optimal-string-alignment distance (Damerau-Levenshtein where a swap of
/// two neighbouring letters is one edit) if it is at most `k`.
pub fn osa_within(a: &[u8], b: &[u8], k: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > k {
        return None;
    }
    let m = b.len();
    let (mut pp, mut p, mut c) = (vec![0usize; m + 1], (0..=m).collect::<Vec<_>>(), vec![0usize; m + 1]);
    for i in 1..=a.len() {
        c[0] = i;
        let mut best = i;
        for j in 1..=m {
            let cost = (a[i - 1] != b[j - 1]) as usize;
            let mut v = (p[j] + 1).min(c[j - 1] + 1).min(p[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(pp[j - 2] + 1);
            }
            c[j] = v;
            best = best.min(v);
        }
        if best > k {
            return None;
        }
        std::mem::swap(&mut pp, &mut p);
        std::mem::swap(&mut p, &mut c);
    }
    (p[m] <= k).then_some(p[m])
}

/// Every string one edit (delete, swap, substitute, insert) from ASCII word
/// `w`, over letters and digits, sorted and without duplicates or `w`. Each
/// is one binary search away in the vocabulary, which beats measuring the
/// distance to every word (grantnav: 4 to 14 ms a word natively, 35 ms for
/// a prefix).
fn edits1(w: &[u8]) -> Vec<Vec<u8>> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let n = w.len();
    let mut out = Vec::with_capacity(n * 2 * ALPHABET.len() + 2 * n);
    for i in 0..n {
        let mut d = w.to_vec();
        d.remove(i);
        out.push(d);
        if i + 1 < n {
            let mut s = w.to_vec();
            s.swap(i, i + 1);
            out.push(s);
        }
        for &c in ALPHABET {
            if c != w[i] {
                let mut s = w.to_vec();
                s[i] = c;
                out.push(s);
            }
        }
    }
    for i in 0..=n {
        for &c in ALPHABET {
            let mut s = w.to_vec();
            s.insert(i, c);
            out.push(s);
        }
    }
    out.sort_unstable();
    out.dedup();
    out.retain(|v| v != w);
    out
}

// ---- English inflections and UK/US spellings (query-side, ASCII) ----

/// Words whose final `s` is not a plural ending (`news` is not `new`s).
const NOT_PLURAL: &[&str] = &["news", "series", "species", "means", "lens", "gas", "bias", "atlas", "canvas", "always"];

fn is_vowel(c: u8) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u')
}

/// `plann` -> `plan`: a doubled final consonant undone (from `planning`).
fn undouble(s: &[u8]) -> Option<Vec<u8>> {
    let n = s.len();
    (n >= 3 && s[n - 1] == s[n - 2] && !is_vowel(s[n - 1]) && !matches!(s[n - 1], b'l' | b's' | b'z' | b'f')).then(|| s[..n - 1].to_vec())
}

/// Ends consonant, vowel, consonant (not w/x/y): `plan` doubles to `planned`.
fn cvc(s: &[u8]) -> bool {
    let n = s.len();
    n >= 3 && !is_vowel(s[n - 1]) && is_vowel(s[n - 2]) && !is_vowel(s[n - 3]) && !matches!(s[n - 1], b'w' | b'x' | b'y')
}

/// Candidate bases of a word: itself, and what plural, `-ed` and `-ing`
/// endings strip back to. Not checked against any vocabulary.
fn bases(w: &[u8]) -> Vec<Vec<u8>> {
    let n = w.len();
    let mut out = vec![w.to_vec()];
    let plural_ok = !NOT_PLURAL.iter().any(|x| x.as_bytes() == w)
        && !w.ends_with(b"ss")
        && !w.ends_with(b"us")
        && !w.ends_with(b"is")
        && !w.ends_with(b"ics");
    if plural_ok && n > 3 && w.ends_with(b"s") {
        out.push(w[..n - 1].to_vec());
        if n > 4 && w.ends_with(b"es") {
            out.push(w[..n - 2].to_vec());
        }
        if n > 4 && w.ends_with(b"ies") {
            out.push([&w[..n - 3], b"y"].concat());
        }
    }
    // `-ed` from 4 letters (`used` -> `use`) but not `-eed` (`feed` is not
    // `fee`); `-ing` from 6 (`thing` is not `the`)
    for (suffix, min) in [(&b"ed"[..], 3), (&b"ing"[..], 5)] {
        if n > min && w.ends_with(suffix) && !w.ends_with(b"eed") {
            let stem = &w[..n - suffix.len()];
            if stem.len() >= 3 {
                out.push(stem.to_vec()); // `used` is not `us`
            }
            out.push([stem, b"e"].concat()); // used -> use, using -> use
            out.extend(undouble(stem));
        }
    }
    out
}

/// The plural, `-ed` and `-ing` forms of a base (no `-er`: `funder` and
/// `heather` are other words, not forms of `fund` and `heath`).
fn forms_of(b: &[u8]) -> Vec<Vec<u8>> {
    let n = b.len();
    if n < 3 {
        return vec![b.to_vec()];
    }
    let mut out = vec![b.to_vec(), [b, b"s"].concat(), [b, b"ed"].concat(), [b, b"ing"].concat()];
    // `-es` after a hissing end only (`boxes`, `churches`; `plan` is not `planes`)
    if [&b"s"[..], b"x", b"z", b"ch", b"sh"].iter().any(|e| b.ends_with(e)) {
        out.push([b, b"es"].concat());
    }
    if b[n - 1] == b'y' && !is_vowel(b[n - 2]) {
        out.push([&b[..n - 1], b"ies"].concat());
        out.push([&b[..n - 1], b"ied"].concat());
    }
    if b[n - 1] == b'e' {
        out.push([b, b"d"].concat());
        out.push([&b[..n - 1], b"ing"].concat());
    }
    if cvc(b) {
        let last = [b[n - 1]];
        out.push([b, &last, b"ed"].concat());
        out.push([b, &last, b"ing"].concat());
    }
    out
}

/// Every inflected form of the word's bases that `is_word` accepts (a stem
/// that is not a word itself, like `traine` from `training`, would make
/// `trainees`), the word itself excluded.
fn inflections(w: &[u8], is_word: impl Fn(&[u8]) -> bool) -> Vec<Vec<u8>> {
    if !w.is_ascii() || w.len() < 3 {
        return Vec::new();
    }
    let mut out: Vec<Vec<u8>> = bases(w).iter().filter(|b| b.as_slice() == w || is_word(b)).flat_map(|b| forms_of(b)).collect();
    out.sort_unstable();
    out.dedup();
    out.retain(|f| f != w);
    out
}

/// UK/US spelling pairs, as word endings swapped either way.
const SPELLINGS: &[(&str, &str)] = &[
    ("ise", "ize"), ("ised", "ized"), ("ises", "izes"), ("ising", "izing"),
    ("isation", "ization"), ("isations", "izations"), ("iser", "izer"), ("isers", "izers"),
    ("our", "or"), ("ours", "ors"), ("oured", "ored"), ("ouring", "oring"), ("ourite", "orite"), ("ourites", "orites"), ("ourable", "orable"),
    ("tre", "ter"), ("tres", "ters"), ("bre", "ber"), ("bres", "bers"),
    ("mme", "m"), ("mmes", "ms"),
    ("lled", "led"), ("lling", "ling"), ("ller", "ler"), ("llers", "lers"),
    ("ogue", "og"), ("ogues", "ogs"),
    ("ence", "ense"), ("ences", "enses"),
];

/// The other spellings of a word (`organisation` -> `organization`). Not
/// checked against any vocabulary.
fn spelling_variants(w: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if !w.is_ascii() || w.len() < 5 {
        return out;
    }
    for &(uk, us) in SPELLINGS {
        for (from, to) in [(uk, us), (us, uk)] {
            if w.ends_with(from.as_bytes()) && w.len() > from.len() + 1 {
                out.push([&w[..w.len() - from.len()], to.as_bytes()].concat());
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out.retain(|v| v != w);
    out
}

/// An inflected form counts only with at least this many rows, and at
/// least 1/`FORM_SHARE` of the word's: the data's own misspellings
/// (`charitys`, 8 rows beside `charity`'s 31,756) and other words that look
/// like forms (`planed`, 9 beside `plan`'s 29,006) are words too.
const MIN_FORM_ROWS: u64 = 5;
const FORM_SHARE: u64 = 1000;

/// One word is the other plus `s` or `es` (`news`/`new`): not a typo of it.
pub fn plural_pair(a: &[u8], b: &[u8]) -> bool {
    let (s, l) = if a.len() < b.len() { (a, b) } else { (b, a) };
    l.starts_with(s) && matches!(&l[s.len()..], b"s" | b"es")
}

/// Corrections kept per word: the most common few…
const CORRECTIONS: usize = 5;
/// …each with at least 1/this of the best one's rows (`sprot` keeps `sport`,
/// not `spot`, `prot`, `sprout`, `sprog`).
const KEEP_BELOW_BEST: u64 = 100;
/// A finished word that exists is still corrected when a neighbour has this
/// many times its rows: `helth` (2 rows) beside `health` (120,730),
/// `enviromental` (39) beside `environmental` (38,091). A real word like
/// `heath` (2,258) beside `health` (53 times) is left alone, for
/// did-you-mean.
const TYPO_RATIO: u64 = 200;
/// …and only a word this rare: `news` (754 rows) beside `new` (152,461) is
/// a word, not a typo.
const TYPO_MAX_ROWS: u64 = 100;

/// How a query word matches the index: the id ranges of its accepted
/// forms, and what each beyond the word itself is (with its rows).
#[derive(Debug, Clone, Default)]
pub struct Forms {
    pub ranges: Vec<Ids>,
    /// rows the word as typed has (for a prefix, summed over its words)
    pub own_rows: u64,
    /// other spellings (UK/US), matched as the word itself
    pub variants: Vec<(Vec<u8>, u64)>,
    /// corrected words (or, for a word being typed, corrected prefixes)
    pub corrections: Vec<(Vec<u8>, u64)>,
    /// plural, `-ed` and `-ing` forms of the word, its variants and corrections
    pub inflections: Vec<(Vec<u8>, u64)>,
    /// the ranges that count as an exact match, for ranking: the whole word
    /// as typed (for the word being typed, not the longer words its prefix
    /// reaches), its other spellings, and their inflections
    pub exact: Vec<Ids>,
    /// the ranges that count as a typo, for ranking: corrections and their
    /// inflections
    pub typo: Vec<Ids>,
}

impl Forms {
    /// Matches more than the word as typed.
    pub fn expanded(&self) -> bool {
        !(self.variants.is_empty() && self.corrections.is_empty() && self.inflections.is_empty())
    }
}

/// Version of a saved index (`SearchIndex::to_bytes`): bump it whenever the
/// tokenizer, the word rules or the layout change, so an index saved by
/// another build is rebuilt instead of misread.
pub const SAVED_VERSION: u32 = 2; // 2: accent folding, apostrophes, marks in words
const SAVED_MAGIC: &[u8; 4] = b"FCTX";

/// What a column is, for checking a saved index against a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColKind {
    /// plain text: in the row lists
    Plain,
    /// dictionary text with this many values
    Dict(usize),
    /// not text, or not a column
    Other,
}

/// Little-endian writer for `to_bytes`: arrays as a u64 length, then items.
struct Out(Vec<u8>);
impl Out {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn len(&mut self, n: usize) {
        self.u64(n as u64);
    }
    fn u8s(&mut self, v: &[u8]) {
        self.len(v.len());
        self.0.extend_from_slice(v);
    }
    fn u16s(&mut self, v: &[u16]) {
        self.len(v.len());
        v.iter().for_each(|x| self.0.extend_from_slice(&x.to_le_bytes()));
    }
    fn u32s(&mut self, v: &[u32]) {
        self.len(v.len());
        v.iter().for_each(|x| self.0.extend_from_slice(&x.to_le_bytes()));
    }
    fn u64s(&mut self, v: &[u64]) {
        self.len(v.len());
        v.iter().for_each(|x| self.0.extend_from_slice(&x.to_le_bytes()));
    }
}

/// Reader for `from_bytes`: every read is bounds-checked.
struct In<'a>(&'a [u8]);
impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        if n > self.0.len() {
            return Err("saved index cut short");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn len(&mut self, width: usize) -> Result<usize, &'static str> {
        let n = self.u64()? as usize;
        if n.checked_mul(width).is_none_or(|b| b > self.0.len()) {
            return Err("saved index cut short");
        }
        Ok(n)
    }
    fn u8s(&mut self) -> Result<Vec<u8>, &'static str> {
        let n = self.len(1)?;
        Ok(self.take(n)?.to_vec())
    }
    fn u16s(&mut self) -> Result<Vec<u16>, &'static str> {
        let n = self.len(2)?;
        Ok(self.take(n * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
    }
    fn u32s(&mut self) -> Result<Vec<u32>, &'static str> {
        let n = self.len(4)?;
        Ok(self.take(n * 4)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
    }
    fn u64s(&mut self) -> Result<Vec<u64>, &'static str> {
        let n = self.len(8)?;
        Ok(self.take(n * 8)?.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect())
    }
    fn usizes(&mut self) -> Result<Vec<usize>, &'static str> {
        Ok(self.u32s()?.into_iter().map(|c| c as usize).collect())
    }
}

impl SearchIndex {
    /// The index as bytes, to keep (OPFS) and read back with `from_bytes`
    /// instead of building it again. `fingerprint` identifies the table it
    /// was built over (`Table::fingerprint`).
    pub fn to_bytes(&self, fingerprint: u64) -> Vec<u8> {
        let mut o = Out(Vec::with_capacity(self.stats().list_bytes + self.stats().vocab_bytes + 1024));
        o.0.extend_from_slice(SAVED_MAGIC);
        o.u32(SAVED_VERSION);
        o.u64(fingerprint);
        o.u32s(&self.cols.iter().map(|&c| c as u32).collect::<Vec<_>>());
        o.u32s(&self.plain.iter().map(|&c| c as u32).collect::<Vec<_>>());
        o.u32(self.fields);
        o.u8s(&self.words.bytes);
        o.u32s(&self.words.ends);
        o.u32s(&self.counts);
        o.len(self.groups.len());
        for g in &self.groups {
            o.u32s(&g.ids);
            o.u32s(&g.entries.iter().flat_map(|e| [e.start, e.len]).collect::<Vec<_>>());
            o.u16s(&g.rows);
            o.u64s(&g.bitmaps);
        }
        o.len(self.dicts.len());
        for d in &self.dicts {
            o.u64(d.col as u64);
            o.u64(d.values as u64);
            o.u32s(&d.offsets);
            o.u32s(&d.codes);
        }
        o.0
    }

    /// An index saved by `to_bytes`, if it was saved by this build
    /// (`SAVED_VERSION`) for this table and is whole: `fingerprint` and
    /// `group_rows` (rows per row group) describe the table, `kind` its
    /// columns. Every list, row, word and code is checked to lie in range,
    /// so a damaged file is refused, never trusted.
    pub fn from_bytes(bytes: &[u8], fingerprint: u64, group_rows: &[usize], kind: impl Fn(usize) -> ColKind) -> Result<SearchIndex, &'static str> {
        let groups = group_rows.len();
        let mut r = In(bytes);
        if r.take(4)? != SAVED_MAGIC {
            return Err("not a saved index");
        }
        if r.u32()? != SAVED_VERSION {
            return Err("saved by another version");
        }
        if r.u64()? != fingerprint {
            return Err("saved for another table");
        }
        let cols = r.usizes()?;
        let plain = r.usizes()?;
        let fields = r.u32()?;
        let words = Words { bytes: r.u8s()?, ends: r.u32s()? };
        let counts = r.u32s()?;
        let nwords = words.ends.len();
        let ok = cols.iter().all(|&c| kind(c) != ColKind::Other)
            && plain.iter().all(|&c| cols.contains(&c) && kind(c) == ColKind::Plain)
            && fields == (plain.len() as u32).max(1)
            && counts.len() == nwords
            && words.ends.windows(2).all(|w| w[0] <= w[1])
            && words.ends.last().is_none_or(|&e| e as usize == words.bytes.len());
        if !ok {
            return Err("saved index damaged");
        }
        if r.len(1)? != groups {
            return Err("saved for another table");
        }
        let lists = nwords as u64 * fields as u64;
        let mut gs = Vec::with_capacity(groups);
        for &n in group_rows {
            let ids = r.u32s()?;
            let flat = r.u32s()?;
            let rows = r.u16s()?;
            let bitmaps = r.u64s()?;
            let entries: Vec<Entry> = flat.chunks_exact(2).map(|p| Entry { start: p[0], len: p[1] }).collect();
            let maps = bitmaps.len() / BITMAP_WORDS;
            let ok = flat.len() == ids.len() * 2
                && bitmaps.len() % BITMAP_WORDS == 0
                && ids.windows(2).all(|w| w[0] < w[1])
                && ids.last().is_none_or(|&l| (l as u64) < lists)
                && entries.iter().all(|e| {
                    if e.len == BITMAP {
                        (e.start as usize) < maps
                    } else {
                        e.start as usize + e.len as usize <= rows.len()
                    }
                })
                && rows.iter().all(|&r| (r as usize) < n)
                && bitmaps.chunks(BITMAP_WORDS).all(|b| {
                    // no row at or past the group's end
                    b.iter().enumerate().all(|(w, &bits)| match n.saturating_sub(w * 64) {
                        0 => bits == 0,
                        k if k < 64 => bits >> k == 0,
                        _ => true,
                    })
                });
            if !ok {
                return Err("saved index damaged");
            }
            gs.push(GroupIndex { ids, entries, rows, bitmaps });
        }
        let ndicts = r.len(1)?;
        let mut dicts = Vec::with_capacity(ndicts);
        for _ in 0..ndicts {
            let col = r.u64()? as usize;
            let values = r.u64()? as usize;
            let offsets = r.u32s()?;
            let codes = r.u32s()?;
            let ok = cols.contains(&col)
                && kind(col) == ColKind::Dict(values)
                && offsets.len() == nwords + 1
                && offsets.windows(2).all(|w| w[0] <= w[1])
                && offsets.last().is_some_and(|&e| e as usize == codes.len())
                && codes.iter().all(|&c| (c as usize) < values);
            if !ok {
                return Err("saved index damaged");
            }
            dicts.push(DictIndex { col, values, offsets, codes });
        }
        if !r.0.is_empty() {
            return Err("saved index damaged");
        }
        Ok(SearchIndex { cols, plain, fields, words, counts, groups: gs, dicts, phases: [0.0; 4], mem: [0; 4] })
    }

    /// An index of no columns over `groups` row groups (what a table without
    /// an index brings to `merge`).
    pub fn empty(groups: usize) -> SearchIndex {
        SearchIndex {
            cols: Vec::new(),
            plain: Vec::new(),
            fields: 1,
            words: Words::default(),
            counts: Vec::new(),
            groups: (0..groups).map(|_| GroupIndex::default()).collect(),
            dicts: Vec::new(),
            phases: [0.0; 4],
            mem: [0; 4],
        }
    }

    /// One index over both images' columns, from a table's index and that
    /// of the image attached to it (`Table::attach`), whose columns come
    /// `shift` places later: what building over the attached table would
    /// give. The vocabularies merge in word order, so each side's ids map up
    /// in order: its lists stay sorted, the second image's fields follow the
    /// first's, and each group's two runs merge like sorted lists.
    pub fn merge(a: &SearchIndex, b: &SearchIndex, shift: usize) -> SearchIndex {
        use std::cmp::Ordering;
        let mut words = Words::default();
        let mut counts = Vec::with_capacity(a.words.len().max(b.words.len()));
        let (mut ma, mut mb) = (Vec::with_capacity(a.words.len()), Vec::with_capacity(b.words.len()));
        let (mut i, mut j) = (0, 0);
        while i < a.words.len() || j < b.words.len() {
            let ord = if i == a.words.len() {
                Ordering::Greater
            } else if j == b.words.len() {
                Ordering::Less
            } else {
                a.words.get(i).cmp(b.words.get(j))
            };
            let id = words.len() as u32;
            if ord != Ordering::Greater {
                words.push(a.words.get(i));
                ma.push(id);
            } else {
                words.push(b.words.get(j));
            }
            if ord != Ordering::Less {
                mb.push(id);
            }
            counts.push(if ord != Ordering::Greater { a.counts[i] } else { 0 } + if ord != Ordering::Less { b.counts[j] } else { 0 });
            i += (ord != Ordering::Greater) as usize;
            j += (ord != Ordering::Less) as usize;
        }
        let fa = a.plain.len() as u32;
        let fields = (fa + b.plain.len() as u32).max(1);
        let groups = a
            .groups
            .iter()
            .zip(&b.groups)
            .map(|(ga, gb)| {
                let (rows_at, maps_at) = (ga.rows.len() as u32, (ga.bitmaps.len() / BITMAP_WORDS) as u32);
                let from_a = ga.ids.iter().zip(&ga.entries).map(|(&l, &e)| (ma[(l / a.fields) as usize] * fields + l % a.fields, e));
                let from_b = gb.ids.iter().zip(&gb.entries).map(|(&l, &e)| {
                    let e = Entry { start: e.start + if e.len == BITMAP { maps_at } else { rows_at }, len: e.len };
                    (mb[(l / b.fields) as usize] * fields + fa + l % b.fields, e)
                });
                let mut g = GroupIndex {
                    ids: Vec::with_capacity(ga.ids.len() + gb.ids.len()),
                    entries: Vec::with_capacity(ga.ids.len() + gb.ids.len()),
                    rows: [ga.rows.as_slice(), gb.rows.as_slice()].concat(),
                    bitmaps: [ga.bitmaps.as_slice(), gb.bitmaps.as_slice()].concat(),
                };
                let (mut x, mut y) = (from_a.peekable(), from_b.peekable());
                while let Some(&(id, e)) = match (x.peek(), y.peek()) {
                    (Some(p), Some(q)) if p.0 < q.0 => x.peek(),
                    (Some(_), Some(_)) => y.peek(),
                    (p, q) => p.or(q),
                } {
                    if x.peek().is_some_and(|p| p.0 == id) {
                        x.next();
                    } else {
                        y.next();
                    }
                    g.ids.push(id);
                    g.entries.push(e);
                }
                g
            })
            .collect();
        // dictionary columns: a word's codes keep their order, as the ids do
        let dicts = a
            .dicts
            .iter()
            .map(|d| (d, &ma, 0))
            .chain(b.dicts.iter().map(|d| (d, &mb, shift)))
            .map(|(d, map, sh)| {
                let mut offsets = vec![0u32; words.len() + 1];
                for (w, &m) in map.iter().enumerate() {
                    offsets[m as usize + 1] = d.offsets[w + 1] - d.offsets[w];
                }
                for k in 0..words.len() {
                    offsets[k + 1] += offsets[k];
                }
                DictIndex { col: d.col + sh, values: d.values, offsets, codes: d.codes.clone() }
            })
            .collect();
        let shifted = |v: &[usize]| v.iter().map(|c| c + shift).collect::<Vec<_>>();
        SearchIndex {
            cols: [a.cols.clone(), shifted(&b.cols)].concat(),
            plain: [a.plain.clone(), shifted(&b.plain)].concat(),
            fields,
            words,
            counts,
            groups,
            dicts,
            phases: [0.0; 4],
            mem: [0; 4],
        }
    }

    /// Index `cols` (text columns) of every row group, in one go.
    pub fn build<S: ReadAt>(table: &mut Table<S>, cols: &[usize]) -> Result<SearchIndex, FormatError> {
        let mut b = IndexBuilder::new(table, cols)?;
        while b.groups_done() < table.group_count() {
            b.add_group(table)?;
        }
        Ok(b.finish())
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

    fn rows_in(&self, ids: Ids) -> u64 {
        self.counts[ids.start as usize..ids.end as usize].iter().map(|&c| c as u64).sum()
    }

    /// The forms query word `w` (folded) matches: itself (a word, or with
    /// `prefix` every word starting with it), and unless `literal` (a quoted
    /// word): for a finished word its other spellings, corrections when it
    /// looks like a typo, and the inflections of all of those; for a word
    /// being typed, corrections only (its prefix already covers its endings).
    /// A word is corrected when it is missing or a word within `max_edits`
    /// (for the word being typed, a prefix one edit away) has `TYPO_RATIO`
    /// times its rows; a finished word only while it has under
    /// `TYPO_MAX_ROWS`. Corrections add to the word as typed, never replace it.
    pub fn word_forms(&self, w: &[u8], prefix: bool, literal: bool) -> Forms {
        let mut forms = self.typo_forms(w, prefix, literal);
        if literal || prefix {
            return forms;
        }
        let mut seen: Vec<u32> = forms.ranges.iter().flat_map(|r| r.clone()).collect();
        // a word that exists with enough rows, not seen yet: its range
        let mut add = |v: &[u8], to: &mut Vec<(Vec<u8>, u64)>, min_rows: u64| -> Option<Ids> {
            let id = self.word_id(v)?;
            let rows = self.counts[id as usize] as u64;
            if rows < min_rows || seen.contains(&id) {
                return None;
            }
            seen.push(id);
            to.push((v.to_vec(), rows));
            Some(id..id + 1)
        };
        let mut variants = Vec::new();
        for v in spelling_variants(w) {
            if let Some(r) = add(&v, &mut variants, 1) {
                forms.ranges.push(r.clone());
                forms.exact.push(r);
            }
        }
        // inflect the word as typed (when it exists), its variants and
        // corrections; a correction's forms are typos too
        let mut bases: Vec<(Vec<u8>, bool)> = Vec::new();
        if forms.own_rows > 0 {
            bases.push((w.to_vec(), false));
        }
        bases.extend(variants.iter().map(|v| (v.0.clone(), false)));
        bases.extend(forms.corrections.iter().map(|c| (c.0.clone(), true)));
        let reference = forms.own_rows.max(forms.corrections.iter().map(|c| c.1).max().unwrap_or(0));
        let min_rows = MIN_FORM_ROWS.max(reference / FORM_SHARE);
        let mut inflected = Vec::new();
        for (b, typo) in bases {
            for f in inflections(&b, |x| self.word_id(x).is_some()) {
                if let Some(r) = add(&f, &mut inflected, min_rows) {
                    forms.ranges.push(r.clone());
                    if typo {
                        forms.typo.push(r);
                    } else {
                        forms.exact.push(r);
                    }
                }
            }
        }
        forms.variants = variants;
        forms.inflections = inflected;
        forms
    }

    /// Words within `max_edits` of `w` (with `prefix`, prefixes one edit
    /// away), with their id ranges and rows: one edit first, by generating
    /// every string one edit away (letters and digits) and looking each up;
    /// two edits, and non-ASCII words, by measuring against words of a
    /// similar length, only when one edit found nothing.
    pub fn neighbors(&self, w: &[u8], prefix: bool) -> Vec<(Vec<u8>, Ids, u64)> {
        let k = max_edits(w.len());
        let mut cands: Vec<(Vec<u8>, Ids, u64)> = Vec::new();
        if k == 0 {
            return cands;
        }
        if w.is_ascii() {
            // one edit first, even where two are allowed: generate the
            // neighbours and look each one up. A word with a neighbour one
            // edit away does not look further (`comunity` is `community`, not
            // also `county`, two edits away)
            for v in edits1(w) {
                let ids = if prefix { self.prefix_ids(&v) } else { self.word_id(&v).map_or(0..0, |id| id..id + 1) };
                if !ids.is_empty() {
                    let rows = self.rows_in(ids.clone());
                    cands.push((v, ids, rows));
                }
            }
        }
        if !cands.is_empty() {
            // found at one edit
        } else if prefix && !w.is_ascii() {
            // a non-ASCII prefix (an ASCII one was covered above): distinct
            // prefixes one shorter, as long, one longer, within 1 edit
            for len in w.len() - 1..=w.len() + 1 {
                let mut i = 0;
                while i < self.words.len() {
                    let word = self.words.get(i);
                    if word.len() < len {
                        i += 1;
                        continue;
                    }
                    let pre = &word[..len];
                    let end = self.words.partition_from(i, |x| x.starts_with(pre));
                    if osa_within(w, pre, 1).is_some() {
                        cands.push((pre.to_vec(), i as u32..end as u32, self.rows_in(i as u32..end as u32)));
                    }
                    i = end;
                }
            }
        } else if k >= 2 || !w.is_ascii() {
            // two edits (words of 8 or more) when one found nothing, and
            // non-ASCII words: measure against words of a similar length
            for i in 0..self.words.len() {
                let word = self.words.get(i);
                if word != w && word.len().abs_diff(w.len()) <= k && osa_within(w, word, k).is_some() {
                    cands.push((word.to_vec(), i as u32..i as u32 + 1, self.counts[i] as u64));
                }
            }
        }
        cands
    }

    /// Whether `f` holds for some row (within group `g`) of word `id`'s row
    /// list, stopping at the first.
    pub fn any_row(&self, g: usize, id: u32, mut f: impl FnMut(usize) -> bool) -> bool {
        (id * self.fields..(id + 1) * self.fields).any(|list| self.list_any(g, list, &mut f))
    }

    /// Mark the rows (within group `g`) where a word of `ids` is in plain
    /// field `field` (its place in `plain`).
    pub fn mark_field(&self, g: usize, ids: Ids, field: u32, out: &mut [u8]) {
        self.each_field_row(g, ids, |_, f, row| {
            if f == field && row < out.len() {
                out[row] = 1;
            }
        });
    }

    /// Calls `f(word, field, row)` for each row of group `g` holding a word
    /// of `ids` in a plain field (once per word and field the row holds).
    pub fn each_field_row(&self, g: usize, ids: Ids, mut f: impl FnMut(u32, u32, usize)) {
        let gi = &self.groups[g];
        let lo = gi.ids.partition_point(|&l| l < ids.start * self.fields);
        let hi = lo + gi.ids[lo..].partition_point(|&l| l < ids.end * self.fields);
        for k in lo..hi {
            let (word, field) = (gi.ids[k] / self.fields, gi.ids[k] % self.fields);
            gi.each_row(gi.entries[k], |row| f(word, field, row));
        }
    }

    /// Per dictionary column of the index, one byte per code: the OR of
    /// `tag(word)` over the words of `ids` its value holds (0 for none).
    pub fn value_tags(&self, ids: &[Ids], tag: impl Fn(u32) -> u8) -> Vec<(usize, Vec<u8>)> {
        self.dicts
            .iter()
            .map(|d| {
                let mut t = vec![0u8; d.values];
                for r in ids {
                    for w in r.clone() {
                        let tw = tag(w);
                        for &code in &d.codes[d.offsets[w as usize] as usize..d.offsets[w as usize + 1] as usize] {
                            t[code as usize] |= tw;
                        }
                    }
                }
                (d.col, t)
            })
            .collect()
    }

    /// The number of plain fields (`plain.len()`, at least 1).
    pub fn fields(&self) -> u32 {
        self.fields
    }

    fn list_any(&self, g: usize, list: u32, f: &mut impl FnMut(usize) -> bool) -> bool {
        let gi = &self.groups[g];
        let Ok(k) = gi.ids.binary_search(&list) else { return false };
        let e = gi.entries[k];
        if e.len == BITMAP {
            let s = e.start as usize * BITMAP_WORDS;
            for (w, &bits) in gi.bitmaps[s..s + BITMAP_WORDS].iter().enumerate() {
                let mut b = bits;
                while b != 0 {
                    if f(w * 64 + b.trailing_zeros() as usize) {
                        return true;
                    }
                    b &= b - 1;
                }
            }
            false
        } else {
            gi.rows[e.start as usize..(e.start + e.len) as usize].iter().any(|&r| f(r as usize))
        }
    }

    /// The dictionary columns' codes holding word `id`: (column, codes).
    pub fn dict_codes(&self, id: u32) -> impl Iterator<Item = (usize, &[u32])> {
        self.dicts.iter().map(move |d| (d.col, &d.codes[d.offsets[id as usize] as usize..d.offsets[id as usize + 1] as usize]))
    }

    /// A word's other spellings that are words (see `word_forms`).
    pub fn spelling_variants(&self, w: &[u8]) -> Vec<Vec<u8>> {
        spelling_variants(w).into_iter().filter(|v| self.word_id(v).is_some()).collect()
    }

    /// The word as typed, and its corrections (see `word_forms`).
    fn typo_forms(&self, w: &[u8], prefix: bool, literal: bool) -> Forms {
        let own = if prefix { self.prefix_ids(w) } else { self.word_id(w).map_or(0..0, |id| id..id + 1) };
        let own_rows = self.rows_in(own.clone());
        let whole = if prefix { self.word_id(w).map_or(0..0, |id| id..id + 1) } else { own.clone() };
        let mut forms = Forms {
            ranges: if own.is_empty() { vec![] } else { vec![own] },
            own_rows,
            exact: if whole.is_empty() { vec![] } else { vec![whole] },
            ..Default::default()
        };
        if literal || max_edits(w.len()) == 0 {
            return forms;
        }
        // a spelling variant that exists is not a typo (`organization`)
        if !prefix && own_rows == 0 && spelling_variants(w).iter().any(|v| self.word_id(v).is_some()) {
            return forms;
        }
        // a finished word is corrected only while it is rare: settled before
        // looking for neighbours, which for a non-ASCII word means measuring
        // against the whole vocabulary
        if !prefix && own_rows >= TYPO_MAX_ROWS {
            return forms;
        }
        let mut cands = self.neighbors(w, prefix);
        // a word's own singular or plural is not a correction of it
        cands.retain(|c| prefix || !plural_pair(&c.0, w));
        let best = cands.iter().map(|c| c.2).max().unwrap_or(0);
        // a word, or the word being typed, is corrected when it is missing or a
        // neighbour has TYPO_RATIO times its rows (`comun…`, 322 rows, beside
        // `commun…`)
        if best == 0 || (own_rows > 0 && own_rows * TYPO_RATIO > best) {
            return forms;
        }
        cands.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        cands.retain(|c| c.2 * KEEP_BELOW_BEST >= best);
        cands.truncate(CORRECTIONS);
        for (text, ids, rows) in cands {
            forms.ranges.push(ids.clone());
            forms.typo.push(ids);
            forms.corrections.push((text, rows));
        }
        forms
    }

    /// The dictionary codes any of `ids` occurs in, per dictionary column.
    /// Once per query word; every group's lookup reuses it.
    pub fn value_sets(&self, ids: &[Ids]) -> ValueSets {
        ValueSets(
            self.dicts
                .iter()
                .map(|d| {
                    let mut t: Option<Vec<u8>> = None;
                    for r in ids {
                        // a range's codes are one contiguous run of `codes`
                        let run = d.offsets[r.start as usize] as usize..d.offsets[r.end as usize] as usize;
                        for &code in &d.codes[run] {
                            t.get_or_insert_with(|| vec![0u8; d.values])[code as usize] = 1;
                        }
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
        ids: &[Ids],
        values: &ValueSets,
        out: &mut [u8],
    ) -> Result<(), FormatError> {
        for r in ids {
            self.groups[g].mark_range(r.start * self.fields..r.end * self.fields, out);
        }
        let rows = out.len();
        // each dictionary column ORs in its own hits, its NULL rows left out
        // (a NULL row carries some code, and a hit there is not a hit)
        for (c, t) in &values.0 {
            let Some(t) = t else { continue };
            let done = table.with_code_segments(g, *c, |codes, width, valid| {
                if width == 1 {
                    or_value_hits(out, t, codes[..rows].iter().map(|&c| c as usize), valid);
                } else {
                    let wide = codes[..rows * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as usize);
                    or_value_hits(out, t, wide, valid);
                }
            })?;
            if done.is_none() {
                let codes = table.codes(g, *c, rows)?;
                let valid = table.validity(g, *c)?;
                or_value_hits(out, t, codes.iter().map(|&c| c as usize), valid.as_deref());
            }
        }
        Ok(())
    }
}

/// Rows a row group may have to be indexed: rows are 16-bit in the lists
/// and bitmaps. facetful's writers make groups of 65,536; the format
/// allows more, and such a table is refused rather than misread.
pub const MAX_GROUP_ROWS: u32 = 1 << 16;

const TOO_BIG: FormatError = FormatError::Corrupt("search index: row groups of more than 65,536 rows cannot be indexed");

/// What a word index can cover: text columns of the table (at least one;
/// anything else would be read as text offsets), in row groups of at most
/// `MAX_GROUP_ROWS` (those known so far, for a table still arriving).
pub fn check_table(cat: &crate::format::Catalog, cols: &[usize]) -> Result<(), FormatError> {
    let text = |c: usize| cat.schema.columns.get(c).is_some_and(|d| d.ty == crate::format::ColumnType::Utf8);
    if cols.is_empty() || !cols.iter().all(|&c| text(c)) {
        return Err(FormatError::Corrupt("search index: every column must be a text column of the table"));
    }
    if cat.groups.iter().any(|g| g.row_count > MAX_GROUP_ROWS) {
        return Err(TOO_BIG);
    }
    Ok(())
}

/// OR into `out` the rows whose code `t` admits, leaving out NULL rows
/// (bit clear in `valid`).
fn or_value_hits(out: &mut [u8], t: &[u8], codes: impl Iterator<Item = usize>, valid: Option<&[u8]>) {
    match valid {
        None => out.iter_mut().zip(codes).for_each(|(o, c)| *o |= t[c]),
        Some(v) => out.iter_mut().zip(codes).enumerate().for_each(|(i, (o, c))| *o |= t[c] & (v[i / 8] >> (i % 8) & 1)),
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
    fn edit_distance() {
        use super::{max_edits, osa_within};
        assert_eq!(osa_within(b"helth", b"health", 1), Some(1));
        assert_eq!(osa_within(b"yuoth", b"youth", 1), Some(1), "a swap is one edit");
        assert_eq!(osa_within(b"chnage", b"change", 1), Some(1));
        assert_eq!(osa_within(b"enviromental", b"environmental", 2), Some(1));
        assert_eq!(osa_within(b"sprot", b"spot", 1), Some(1));
        assert_eq!(osa_within(b"cat", b"dog", 2), None);
        assert_eq!(osa_within(b"health", b"health", 0), Some(0));
        assert_eq!(osa_within(b"ab", b"abcd", 1), None, "length gap over k");
        assert_eq!((max_edits(3), max_edits(4), max_edits(7), max_edits(8)), (0, 1, 1, 2));
    }

    /// The build's fast path (`row_keys`) reads the same words as
    /// `for_each_word`, apostrophes and all.
    #[test]
    fn fast_path_reads_words_alike() {
        let bytes = |k: super::Key| match k {
            super::Key::Short(u) => {
                let b = u.to_le_bytes();
                b[..b[15] as usize].to_vec()
            }
            super::Key::Long(w) => w.to_vec(),
        };
        let alphabet = b"aZs1'' -";
        let mut x = 7u64;
        let (mut buf, mut masks) = (Vec::new(), Vec::new());
        for _ in 0..20000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = (x >> 60) as usize + 1;
            let text: Vec<u8> = (0..len).map(|k| alphabet[(x >> (4 + 3 * k % 56)) as usize % alphabet.len()]).collect();
            let mut fast = Vec::new();
            super::row_keys(&text, &mut buf, &mut masks, |k| fast.push(bytes(k)));
            let mut slow = Vec::new();
            for_each_word(&text, &mut Vec::new(), |w| slow.push(w.to_vec()));
            assert_eq!(fast, slow, "{:?}", String::from_utf8_lossy(&text));
        }
        // words over 15 bytes across an apostrophe
        let text = b"internationalisation's o'internationalisation ab'cdefghijklmnopqrstuvwxyz";
        let mut fast = Vec::new();
        super::row_keys(text, &mut buf, &mut masks, |k| fast.push(bytes(k)));
        assert_eq!(fast, [b"internationalisation".to_vec(), b"ointernationalisation".to_vec(), b"abcdefghijklmnopqrstuvwxyz".to_vec()]);
    }

    #[test]
    fn folding_and_splitting() {
        assert_eq!(words("Mental-health, LONDON 2024!"), ["mental", "health", "london", "2024"]);
        assert_eq!(words("  "), Vec::<String>::new());
        assert_eq!(words("a"), ["a"]);
        // accents fold, precomposed or written separately; a word stays whole
        assert_eq!(words("São Paulo ÉCOLE"), ["sao", "paulo", "ecole"]);
        assert_eq!(words("re\u{301}sume\u{301} résumé RÉSUMÉ"), ["resume", "resume", "resume"]);
        assert_eq!(words("Ǖ ǖ Ångström \u{212B}"), ["u", "u", "angstrom", "a"]);
        assert_eq!(words("İstanbul \u{212A}elvin"), ["istanbul", "kelvin"], "dotted I, the Kelvin sign");
        assert_eq!(words("Ελλάδα ΆΘΗΝΑ ёлка"), ["ελλαδα", "αθηνα", "елка"]);
        // other marks are part of their word, not breaks (Devanagari vowel signs)
        assert_eq!(words("हिन्दी भाषा"), ["हिन्दी", "भाषा"]);
        // not accent folding: these stay
        assert_eq!(words("straße æther ﬁne"), ["straße", "æther", "ﬁne"]);
        // apostrophes inside a word: a possessive 's goes, otherwise the word runs on
        assert_eq!(words("children's O'Brien don't rock'n'roll"), ["children", "obrien", "dont", "rocknroll"]);
        assert_eq!(words("children\u{2019}s O\u{2019}Brien"), ["children", "obrien"], "curly apostrophes");
        assert_eq!(words("parents' 'quoted' it's 90's '"), ["parents", "quoted", "it", "90"]);
        assert_eq!(words("a's b'sx 's"), ["a", "bsx", "s"]);
        assert_eq!(words("a's'b"), ["a", "b"], "a possessive ends the word");
        assert_eq!(words("a'1 1'a 1'2 10'000"), ["a", "1", "1", "a", "12", "10000"], "letter and digit: two words (UAX #29)");
    }
}
