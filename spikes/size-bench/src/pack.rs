//! A packed wire form of an image, and its decoder: the prototype of what
//! docs/file-size.sv proposes. Packing is a lossless rewrite of the image's
//! bytes; unpacking gives back the same image, byte for byte, one row group
//! at a time, so everything after the loader (queries, OPFS, split, attach,
//! fingerprints) sees the image it always saw.
//!
//! ```text
//! "FCTZ" version:u8 pad:3
//! header_len:u32, the image's header as it is
//! dictionary lengths table as it is
//! one:u8 (one-byte word codes; 0xFF: no word lists), then per text column
//!   its word list: count:varint, the words (each ending in 0)
//! per dictionary: tag:u8 (D_*), then its values in that form
//! per row group: wire_len:u32 rows:u32, per column null_count:varint,
//!   then per column its payload (a tag, T_* F_* I_*, for text, floats and
//!   integers; codes and bools as stored, unpadded) and its validity bitmap
//!   when it has NULLs
//! 0:u32
//! footer_len:u32, the image's footer as it is (through the closing magic)
//! ```
//! The proposal starts the packed form with the image's header at version 2
//! instead of a magic of its own; the prototype did not need that.
//! No dependencies, no std collections on the decode side: it has to compile
//! into the wasm engine.

use facetful_format::read;
use facetful_format::{align_up, Catalog, ColumnType, MAX_SEGS};

pub const MAGIC: [u8; 4] = *b"FCTZ";

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    /// text values end in a 0 byte instead of carrying offsets
    pub text_sep: bool,
    /// identifier-like text: the trailing number as a difference
    pub numsuffix: bool,
    /// dictionaries: values end in a 0 byte instead of carrying offsets
    pub dict_sep: bool,
    /// floats that are decimals: scaled integers
    pub alp: bool,
    /// common words of text columns and dictionaries as byte codes: how many
    /// of the 128 lead bytes are one-byte codes (the rest start two-byte
    /// codes); None: off
    pub words: Option<u8>,
    /// integers that run in sequence: differences
    pub delta: bool,
}

impl Opts {
    pub const NONE: Opts = Opts { text_sep: false, numsuffix: false, dict_sep: false, alp: false, words: None, delta: false };
    pub const ALL: Opts = Opts { text_sep: true, numsuffix: true, dict_sep: true, alp: true, words: Some(64), delta: true };
}

// text payload tags
const T_STORED: u8 = 0;
const T_SEP: u8 = 1;
const T_NUM: u8 = 2;
const T_WORDS: u8 = 3;
// float payload tags
const F_STORED: u8 = 0;
const F_ALP: u8 = 1;
// integer payload tags
const I_STORED: u8 = 0;
const I_DELTA: u8 = 1;
// dictionary tags
const D_STORED: u8 = 0;
const D_SEP: u8 = 1;
const D_WORDS: u8 = 2;

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

const P10: [f64; 19] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18];

/// The integer `i` with `i / 10^e == v` exactly, when there is one.
fn decimal(v: f64, e: usize) -> Option<i64> {
    let r = (v * P10[e]).round();
    if !(r.abs() < 9007199254740992.0) {
        return None;
    }
    ((r as i64 as f64 / P10[e]).to_bits() == v.to_bits()).then_some(r as i64)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

// ---------------------------------------------------------------- pack ----

/// A value's tokens: a word (a run of ASCII letters, with the space before
/// it when there is one) or a single other byte.
fn tokens<'a>(v: &'a [u8], mut f: impl FnMut(&'a [u8], bool)) {
    let mut i = 0;
    while i < v.len() {
        let w0 = i + (v[i] == b' ') as usize;
        let mut j = w0;
        while j < v.len() && v[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j > w0 {
            f(&v[i..j], true);
            i = j;
        } else {
            f(&v[i..i + 1], false);
            i += 1;
        }
    }
}

/// The words of one column that are worth a code, best first.
pub struct WordList<'a> {
    one: usize,
    words: Vec<&'a [u8]>,
    code: std::collections::HashMap<&'a [u8], u32>,
}

impl<'a> WordList<'a> {
    fn build(values: impl Iterator<Item = &'a [u8]>, one: usize) -> WordList<'a> {
        let mut count: std::collections::HashMap<&[u8], u32> = std::collections::HashMap::new();
        for v in values {
            tokens(v, |t, word| {
                if word && t.len() >= 3 {
                    *count.entry(t).or_default() += 1;
                }
            });
        }
        let mut best: Vec<(&[u8], u32)> = count.into_iter().filter(|&(_, n)| n >= 4).collect();
        best.sort_by_key(|&(t, n)| (std::cmp::Reverse(n as u64 * (t.len() as u64 - 1)), t));
        best.truncate(one + (128 - one) * 256);
        let words: Vec<&[u8]> = best.iter().map(|b| b.0).collect();
        let code = words.iter().enumerate().map(|(i, &w)| (w, i as u32)).collect();
        WordList { one, words, code }
    }

    fn write(&self, out: &mut Vec<u8>) {
        varint(out, self.words.len() as u64);
        for w in &self.words {
            out.extend_from_slice(w);
            out.push(0);
        }
    }

    /// One value: words as codes, bytes of 0x80 and over (and 0, 1) after a
    /// 0x01 escape, then the closing 0.
    fn encode(&self, v: &[u8], out: &mut Vec<u8>) {
        tokens(v, |t, word| match self.code.get(t) {
            Some(&k) if word && (k as usize) < self.one => out.push(0x80 + k as u8),
            Some(&k) if word => {
                let k = k as usize - self.one;
                out.push((0x80 + self.one + k / 256) as u8);
                out.push((k % 256) as u8);
            }
            _ => {
                for &b in t {
                    if b >= 0x80 || b <= 1 {
                        out.push(1);
                    }
                    out.push(b);
                }
            }
        });
        out.push(0);
    }
}

/// Whether a text column reads as identifiers: nearly every value ends in
/// digits, four or more on average.
fn is_numbered(values: impl Iterator<Item = impl AsRef<[u8]>>) -> bool {
    let (mut n, mut with, mut digits) = (0usize, 0usize, 0usize);
    for v in values {
        let d = v.as_ref().iter().rev().take(18).take_while(|b| b.is_ascii_digit()).count();
        n += 1;
        with += (d > 0) as usize;
        digits += d;
    }
    with * 10 >= n * 9 && digits >= with * 4
}

pub fn pack(image: &[u8], cat: &Catalog, opts: Opts) -> Vec<u8> {
    let mut out = Vec::with_capacity(image.len() / 2);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[1, 0, 0, 0]);
    let ncols = cat.schema.columns.len();
    let hlen = u32_at(image, 4) as usize;
    out.extend_from_slice(&(hlen as u32).to_le_bytes());
    out.extend_from_slice(&image[..hlen]);
    let ndict = cat.dicts.iter().flatten().count();
    let table = if ndict == 0 { 0 } else { align_up(ndict * 8) };
    out.extend_from_slice(&image[hlen..hlen + table]);
    // per text column: identifiers, or a word list (dictionary columns: over
    // the dictionary's values)
    let text_values = |ci: usize| {
        cat.groups.iter().flat_map(move |g| {
            let n = g.row_count as usize;
            let o = read::segment_offset(cat, g, ci, 0) as usize;
            let b = read::segment_offset(cat, g, ci, 1) as usize;
            (0..n).map(move |i| &image[b + u32_at(image, o + i * 4) as usize..b + u32_at(image, o + i * 4 + 4) as usize])
        })
    };
    let dict_values = |d: &facetful_format::DictLoc| {
        let offs = &image[d.offsets_off as usize..(d.offsets_off + d.offsets_len as u64) as usize];
        let all: Vec<u32> = offs.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let mut n = 1;
        while n < all.len() && all[n] >= all[n - 1] && !(all[n] == 0 && all[n - 1] > 0) {
            n += 1;
        }
        let at = d.bytes_off as usize;
        (0..n - 1).map(move |i| &image[at + all[i] as usize..at + all[i + 1] as usize]).collect::<Vec<_>>()
    };
    let mut numbered = vec![false; ncols];
    let mut lists: Vec<Option<WordList>> = (0..ncols).map(|_| None).collect();
    out.push(opts.words.unwrap_or(0xFF));
    for (ci, def) in cat.schema.columns.iter().enumerate() {
        if def.ty != ColumnType::Utf8 {
            continue;
        }
        if !def.is_dict() {
            numbered[ci] = opts.numsuffix && opts.text_sep && is_numbered(text_values(ci));
        }
        if let (Some(one), false) = (opts.words, numbered[ci]) {
            // word codes take the bytes of 0x80 and over, so text in other
            // scripts would pay an escape per byte: mostly-ASCII columns only,
            // and ones large enough to repay a word list
            let (mut bytes, mut high) = (0usize, 0usize);
            let mut see = |v: &[u8]| {
                bytes += v.len();
                high += v.iter().filter(|&&b| b >= 0x80).count();
            };
            match &cat.dicts[ci] {
                Some(d) => dict_values(d).into_iter().for_each(&mut see),
                None => text_values(ci).for_each(&mut see),
            }
            if bytes >= 1 << 16 && high * 50 <= bytes {
                let list = match &cat.dicts[ci] {
                    Some(d) => WordList::build(dict_values(d).into_iter(), one as usize),
                    None => WordList::build(text_values(ci), one as usize),
                };
                list.write(&mut out);
                lists[ci] = Some(list);
                continue;
            }
        }
        if opts.words.is_some() {
            varint(&mut out, 0);
        }
    }
    for (ci, d) in cat.dicts.iter().enumerate() {
        let Some(d) = d else { continue };
        let offs = &image[d.offsets_off as usize..(d.offsets_off + d.offsets_len as u64) as usize];
        let bytes = &image[d.bytes_off as usize..(d.bytes_off + d.bytes_len as u64) as usize];
        // the real offsets end where they stop rising (zero padding after)
        let all: Vec<u32> = offs.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let mut n = 1;
        while n < all.len() && all[n] >= all[n - 1] && !(all[n] == 0 && all[n - 1] > 0) {
            n += 1;
        }
        let used = all[n - 1] as usize;
        // the padded lengths must be what the count and bytes give back
        let fits = align_up(n * 4) == offs.len() && align_up(used) == bytes.len() && all[0] == 0;
        if let (Some(list), true) = (&lists[ci], fits) {
            out.push(D_WORDS);
            varint(&mut out, (n - 1) as u64);
            for w in all[..n].windows(2) {
                list.encode(&bytes[w[0] as usize..w[1] as usize], &mut out);
            }
        } else if opts.dict_sep && fits && !bytes[..used].contains(&0) {
            out.push(D_SEP);
            varint(&mut out, (n - 1) as u64);
            for w in all[..n].windows(2) {
                out.extend_from_slice(&bytes[w[0] as usize..w[1] as usize]);
                out.push(0);
            }
        } else {
            out.push(D_STORED);
            out.extend_from_slice(offs);
            out.extend_from_slice(bytes);
        }
    }
    for g in &cat.groups {
        let n = g.row_count as usize;
        let at = out.len();
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&g.row_count.to_le_bytes());
        for c in &g.cols {
            varint(&mut out, c.null_count as u64);
        }
        for (ci, (def, cm)) in cat.schema.columns.iter().zip(&g.cols).enumerate() {
            let seg = |s: usize| {
                let off = read::segment_offset(cat, g, ci, s) as usize;
                &image[off..off + cm.seg_lens[s] as usize]
            };
            match (def.ty, def.is_dict()) {
                (ColumnType::Utf8, true) => out.extend_from_slice(&seg(0)[..n * def.code_width()]),
                (ColumnType::Utf8, false) => pack_text(seg(0), seg(1), n, opts, numbered[ci], lists[ci].as_ref(), &mut out),
                (ColumnType::Bool, _) => out.extend_from_slice(&seg(0)[..n.div_ceil(8)]),
                (ColumnType::Float64, _) => pack_floats(&seg(0)[..n * 8], opts, &mut out),
                (ty, _) => pack_ints(&seg(0)[..n * ty.fixed_width().unwrap()], ty.fixed_width().unwrap(), opts, &mut out),
            }
            if cm.null_count > 0 {
                out.extend_from_slice(&seg(2)[..n.div_ceil(8)]);
            }
        }
        let len = (out.len() - at - 4) as u32;
        out[at..at + 4].copy_from_slice(&len.to_le_bytes());
    }
    out.extend_from_slice(&[0; 4]);
    // footer: everything after the last group
    let last = cat.groups.last().unwrap();
    let end = read::segment_offset(cat, last, ncols - 1, MAX_SEGS - 1) as usize + last.cols[ncols - 1].seg_lens[MAX_SEGS - 1] as usize;
    out.extend_from_slice(&((image.len() - end) as u32).to_le_bytes());
    out.extend_from_slice(&image[end..]);
    out
}

fn pack_text(offs: &[u8], bytes: &[u8], n: usize, opts: Opts, numbered: bool, list: Option<&WordList>, out: &mut Vec<u8>) {
    let off = |i: usize| u32_at(offs, i * 4) as usize;
    let used = off(n);
    let text = &bytes[..used];
    if let (Some(list), true) = (list, off(0) == 0) {
        out.push(T_WORDS);
        varint(out, used as u64);
        for i in 0..n {
            list.encode(&text[off(i)..off(i + 1)], out);
        }
        return;
    }
    if !opts.text_sep || off(0) != 0 || text.contains(&0) {
        out.push(T_STORED);
        varint(out, used as u64);
        out.extend_from_slice(&offs[..(n + 1) * 4]);
        out.extend_from_slice(text);
        return;
    }
    if numbered {
        {
            out.push(T_NUM);
            let (mut heads, mut widths, mut nums) = (Vec::new(), Vec::with_capacity(n), Vec::new());
            let mut prev = 0i64;
            for i in 0..n {
                let v = &text[off(i)..off(i + 1)];
                let d = v.iter().rev().take(18).take_while(|b| b.is_ascii_digit()).count();
                let (head, tail) = v.split_at(v.len() - d);
                heads.extend_from_slice(head);
                heads.push(0);
                widths.push(d as u8);
                if d > 0 {
                    let x = tail.iter().fold(0i64, |a, &b| a * 10 + (b - b'0') as i64);
                    varint(&mut nums, zigzag(x - prev));
                    prev = x;
                }
            }
            varint(out, used as u64);
            out.extend(heads);
            out.extend(widths);
            out.extend(nums);
            return;
        }
    }
    out.push(T_SEP);
    varint(out, used as u64);
    for i in 0..n {
        out.extend_from_slice(&text[off(i)..off(i + 1)]);
        out.push(0);
    }
}

/// Bits per byte of `bytes`, taking each byte on its own (order 0), times
/// their number: a rough size after compression.
fn entropy(bytes: &[u8]) -> f64 {
    let mut count = [0u32; 256];
    bytes.iter().for_each(|&b| count[b as usize] += 1);
    let n = bytes.len() as f64;
    count.iter().filter(|&&c| c > 0).map(|&c| -(c as f64) * (c as f64 / n).log2()).sum()
}

/// Integers as stored, or each minus the one before (wrapping, same width)
/// when that looks at least twice as small: sequences, sorted timestamps.
/// Brotli finds repeats, not arithmetic.
fn pack_ints(bytes: &[u8], w: usize, opts: Opts, out: &mut Vec<u8>) {
    if opts.delta && bytes.len() >= w {
        let mut d = Vec::with_capacity(bytes.len());
        let mut prev = 0u64;
        for v in bytes.chunks_exact(w) {
            let mut b = [0u8; 8];
            b[..w].copy_from_slice(v);
            let x = u64::from_le_bytes(b);
            d.extend_from_slice(&x.wrapping_sub(prev).to_le_bytes()[..w]);
            prev = x;
        }
        if entropy(&d) * 2.0 < entropy(bytes) {
            out.push(I_DELTA);
            out.extend(d);
            return;
        }
    }
    out.push(I_STORED);
    out.extend_from_slice(bytes);
}

fn pack_floats(bytes: &[u8], opts: Opts, out: &mut Vec<u8>) {
    let vals: Vec<f64> = bytes.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect();
    let mut best = (u64::MAX, 0usize, 0u64);
    if opts.alp {
        for e in 0..=18 {
            let (mut exc, mut maxabs) = (0u64, 0u64);
            for &v in &vals {
                match decimal(v, e) {
                    Some(i) => maxabs = maxabs.max(i.unsigned_abs()),
                    None => exc += 1,
                }
            }
            let cost = exc * 10 + vals.len() as u64 * (64 - maxabs.leading_zeros() as u64 + 1) / 8;
            if cost < best.0 {
                best = (cost, e, exc);
            }
        }
    }
    let (_, e, exc) = best;
    if !opts.alp || exc as usize * 4 > vals.len() {
        out.push(F_STORED);
        out.extend_from_slice(bytes);
        return;
    }
    out.push(F_ALP);
    out.push(e as u8);
    let mut tail = Vec::new();
    let mut last = 0usize;
    for (i, &v) in vals.iter().enumerate() {
        match decimal(v, e) {
            Some(x) => varint(out, zigzag(x)),
            None => {
                out.push(0);
                varint(&mut tail, (i - last) as u64);
                last = i;
                tail.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    varint(out, exc);
    out.extend(tail);
}

// -------------------------------------------------------------- unpack ----

pub struct Reader<'a> {
    pub b: &'a [u8],
    pub p: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    #[inline]
    fn varint(&mut self) -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0u32);
        loop {
            let b = *self.b.get(self.p)?;
            self.p += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
}

fn pad(out: &mut Vec<u8>) {
    out.resize(align_up(out.len()), 0);
}

/// What the unpacker knows of the schema: per column, how its payload reads.
#[derive(Clone, Copy)]
pub enum Col {
    Text,
    Int(usize),
    Fixed(usize),
    Float,
    Bool,
}

pub struct Unpacker {
    pub cols: Vec<Col>,
    /// one-byte word codes (0xFF: no word lists)
    one: usize,
    /// per column: its word list (ends, bytes)
    words: Vec<(Vec<u32>, Vec<u8>)>,
}

/// The whole packed image, unpacked. Returns None on malformed input.
pub fn unpack(wire: &[u8]) -> Option<Vec<u8>> {
    let mut r = Reader { b: wire, p: 0 };
    if r.take(4)? != MAGIC {
        return None;
    }
    r.take(4)?;
    let hlen = r.u32()? as usize;
    let header = r.take(hlen)?;
    let (schema, ..) = read::parse_header(header).ok()?;
    let image_len = None::<usize>;
    let mut out: Vec<u8> = Vec::with_capacity(image_len.unwrap_or(wire.len() * 2));
    out.extend_from_slice(header);
    let ndict = schema.columns.iter().filter(|c| c.is_dict()).count();
    let table = if ndict == 0 { 0 } else { align_up(ndict * 8) };
    out.extend_from_slice(r.take(table)?);
    let one = r.u8()? as usize;
    let mut words: Vec<(Vec<u32>, Vec<u8>)> = Vec::new();
    for c in &schema.columns {
        let mut list = (vec![0u32], Vec::new());
        if one != 0xFF && c.ty == ColumnType::Utf8 {
            for _ in 0..r.varint()? {
                let len = find0(&r.b[r.p..])?;
                list.1.extend_from_slice(r.take(len + 1)?.split_last()?.1);
                list.0.push(list.1.len() as u32);
            }
        }
        words.push(list);
    }
    let dict_cols: Vec<usize> = (0..schema.columns.len()).filter(|&c| schema.columns[c].is_dict()).collect();
    for d in 0..ndict {
        let ol = u32_at(&out, hlen + d * 8) as usize;
        let bl = u32_at(&out, hlen + d * 8 + 4) as usize;
        match r.u8()? {
            D_STORED => out.extend_from_slice(r.take(ol + bl)?),
            D_SEP => {
                let n = r.varint()? as usize;
                let offs_at = out.len();
                out.resize(offs_at + ol, 0);
                let bytes_at = out.len();
                unsep(&mut r, n, offs_at, &mut out)?;
                if out.len() - bytes_at > bl {
                    return None;
                }
                out.resize(bytes_at + bl, 0);
            }
            D_WORDS => {
                let n = r.varint()? as usize;
                let offs_at = out.len();
                out.resize(offs_at + ol, 0);
                let bytes_at = out.len();
                unwords(&mut r, n, offs_at, &mut out, &words[dict_cols[d]], one)?;
                if out.len() - bytes_at > bl {
                    return None;
                }
                out.resize(bytes_at + bl, 0);
            }
            _ => return None,
        }
    }
    let un = Unpacker {
        one,
        words,
        cols: schema
            .columns
            .iter()
            .map(|c| match (c.ty, c.is_dict()) {
                (ColumnType::Utf8, true) => Col::Fixed(c.code_width()),
                (ColumnType::Utf8, false) => Col::Text,
                (ColumnType::Bool, _) => Col::Bool,
                (ColumnType::Float64, _) => Col::Float,
                (ty, _) => Col::Int(ty.fixed_width().unwrap()),
            })
            .collect(),
    };
    loop {
        let len = r.u32()? as usize;
        if len == 0 {
            break;
        }
        un.group(r.take(len)?, &mut out)?;
    }
    let flen = r.u32()? as usize;
    out.extend_from_slice(r.take(flen)?);
    Some(out)
}

/// `n` values, each ending in a 0 byte, appended to `out` without the
/// zeros; their n + 1 offsets written at `offs_at`.
#[inline]
fn unsep(r: &mut Reader, n: usize, offs_at: usize, out: &mut Vec<u8>) -> Option<()> {
    let base = out.len();
    let src = &r.b[r.p..];
    let mut s = 0usize;
    out[offs_at..offs_at + 4].copy_from_slice(&0u32.to_le_bytes());
    for i in 0..n {
        let len = find0(&src[s..])?;
        out.extend_from_slice(&src[s..s + len]);
        s += len + 1;
        let o = offs_at + (i + 1) * 4;
        let end = (out.len() - base) as u32;
        out[o..o + 4].copy_from_slice(&end.to_le_bytes());
    }
    r.p += s;
    Some(())
}

/// `unsep` for values written with word codes (`WordList::encode`).
#[inline]
fn unwords(r: &mut Reader, n: usize, offs_at: usize, out: &mut Vec<u8>, words: &(Vec<u32>, Vec<u8>), one: usize) -> Option<()> {
    let base = out.len();
    let src = &r.b[r.p..];
    let mut s = 0usize;
    out[offs_at..offs_at + 4].copy_from_slice(&0u32.to_le_bytes());
    let (ends, wbytes) = words;
    for i in 0..n {
        loop {
            // plain bytes (2 to 0x7F), eight at a time
            while s + 8 <= src.len() {
                let w = u64::from_le_bytes(src[s..s + 8].try_into().unwrap());
                let special = (w | (w.wrapping_sub(0x0202020202020202) & !w)) & 0x8080808080808080;
                if special != 0 {
                    let k = (special.trailing_zeros() / 8) as usize;
                    out.extend_from_slice(&src[s..s + k]);
                    s += k;
                    break;
                }
                out.extend_from_slice(&src[s..s + 8]);
                s += 8;
            }
            let b = *src.get(s)?;
            s += 1;
            if b >= 0x80 {
                let mut k = (b - 0x80) as usize;
                if k >= one {
                    k = one + (k - one) * 256 + *src.get(s)? as usize;
                    s += 1;
                }
                let (a, e) = (*ends.get(k)? as usize, *ends.get(k + 1)? as usize);
                out.extend_from_slice(&wbytes[a..e]);
            } else if b == 0 {
                break;
            } else if b == 1 {
                out.push(*src.get(s)?);
                s += 1;
            } else {
                out.push(b);
            }
        }
        let o = offs_at + (i + 1) * 4;
        let end = (out.len() - base) as u32;
        out[o..o + 4].copy_from_slice(&end.to_le_bytes());
    }
    r.p += s;
    Some(())
}

/// Index of the first 0 byte: eight bytes at a time.
#[inline]
fn find0(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 8 <= b.len() {
        let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let z = w.wrapping_sub(0x0101010101010101) & !w & 0x8080808080808080;
        if z != 0 {
            return Some(i + (z.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    b[i..].iter().position(|&x| x == 0).map(|p| i + p)
}

impl Unpacker {
    /// One row group's wire bytes, appended to `out` as the image holds it:
    /// group header, then each column's padded segments.
    pub fn group(&self, wire: &[u8], out: &mut Vec<u8>) -> Option<()> {
        let mut r = Reader { b: wire, p: 0 };
        let rows = r.u32()?;
        let n = rows as usize;
        let ncols = self.cols.len();
        let head = out.len();
        out.resize(head + align_up(4 + ncols * (4 + 4 * MAX_SEGS)), 0);
        out[head..head + 4].copy_from_slice(&rows.to_le_bytes());
        let mut nulls = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            nulls.push(r.varint()? as u32);
        }
        for (ci, col) in self.cols.iter().enumerate() {
            let mut lens = [0u32; MAX_SEGS];
            let start = out.len();
            match *col {
                Col::Fixed(w) => out.extend_from_slice(r.take(n * w)?),
                Col::Int(w) => match r.u8()? {
                    I_STORED => out.extend_from_slice(r.take(n * w)?),
                    I_DELTA => {
                        let mut prev = 0u64;
                        for d in r.take(n * w)?.chunks_exact(w) {
                            let mut b = [0u8; 8];
                            b[..w].copy_from_slice(d);
                            prev = prev.wrapping_add(u64::from_le_bytes(b));
                            out.extend_from_slice(&prev.to_le_bytes()[..w]);
                        }
                    }
                    _ => return None,
                },
                Col::Bool => out.extend_from_slice(r.take(n.div_ceil(8))?),
                Col::Float => match r.u8()? {
                    F_STORED => out.extend_from_slice(r.take(n * 8)?),
                    F_ALP => {
                        let e = *P10.get(r.u8()? as usize)?;
                        out.reserve(n * 8);
                        for _ in 0..n {
                            let x = unzigzag(r.varint()?) as f64 / e;
                            out.extend_from_slice(&x.to_le_bytes());
                        }
                        let mut at = 0usize;
                        for _ in 0..r.varint()? {
                            at += r.varint()? as usize;
                            let v = r.take(8)?;
                            out.get_mut(start + at * 8..start + at * 8 + 8)?.copy_from_slice(v);
                        }
                    }
                    _ => return None,
                },
                Col::Text => {
                    let tag = r.u8()?;
                    let used = r.varint()? as usize;
                    out.resize(start + (n + 1) * 4, 0);
                    pad(out);
                    lens[0] = (out.len() - start) as u32;
                    let bytes_at = out.len();
                    out.reserve(used + 8);
                    match tag {
                        T_STORED => {
                            out[start..start + (n + 1) * 4].copy_from_slice(r.take((n + 1) * 4)?);
                            out.extend_from_slice(r.take(used)?);
                        }
                        T_SEP => unsep(&mut r, n, start, out)?,
                        T_WORDS => unwords(&mut r, n, start, out, &self.words[ci], self.one)?,
                        T_NUM => {
                            // heads end in 0; widths and numbers follow them
                            let mut heads = Reader { b: r.b, p: r.p };
                            for _ in 0..n {
                                heads.p += find0(&heads.b[heads.p..])? + 1;
                            }
                            let widths = heads.take(n)?;
                            let mut nums = Reader { b: r.b, p: heads.p };
                            let mut hp = r.p;
                            let mut prev = 0i64;
                            for i in 0..n {
                                let hl = find0(&r.b[hp..])?;
                                out.extend_from_slice(&r.b[hp..hp + hl]);
                                hp += hl + 1;
                                let w = widths[i] as usize;
                                if w > 0 {
                                    prev += unzigzag(nums.varint()?);
                                    let mut digits = [b'0'; 20];
                                    let mut x = prev as u64;
                                    let mut k = 20;
                                    while x > 0 {
                                        k -= 1;
                                        digits[k] = b'0' + (x % 10) as u8;
                                        x /= 10;
                                    }
                                    out.extend_from_slice(digits.get(20usize.checked_sub(w)?..)?);
                                    let _ = k;
                                }
                                let o = start + (i + 1) * 4;
                                let end = (out.len() - bytes_at) as u32;
                                out[o..o + 4].copy_from_slice(&end.to_le_bytes());
                            }
                            r.p = nums.p;
                        }
                        _ => return None,
                    }
                    if out.len() - bytes_at != used {
                        return None;
                    }
                    pad(out);
                    lens[1] = (out.len() - bytes_at) as u32;
                }
            }
            if !matches!(col, Col::Text) {
                pad(out);
                lens[0] = (out.len() - start) as u32;
            }
            if nulls[ci] > 0 {
                let at = out.len();
                out.extend_from_slice(r.take(n.div_ceil(8))?);
                pad(out);
                lens[2] = (out.len() - at) as u32;
            }
            let h = head + 4 + ci * (4 + 4 * MAX_SEGS);
            out[h..h + 4].copy_from_slice(&nulls[ci].to_le_bytes());
            for (s, l) in lens.iter().enumerate() {
                out[h + 4 + s * 4..h + 8 + s * 4].copy_from_slice(&l.to_le_bytes());
            }
        }
        (r.p == wire.len()).then_some(())
    }
}
