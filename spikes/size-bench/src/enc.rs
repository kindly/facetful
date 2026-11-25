//! Candidate wire encodings per column kind. Every encoding is lossless and
//! is laid out row group by row group, as a streamed image has to be.

use std::collections::HashMap;

use crate::img::{Img, Kind};
use crate::Job;

pub fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Byte planes: every value's byte 0, then every value's byte 1, …
pub fn planes(bytes: &[u8], w: usize) -> Vec<u8> {
    let n = bytes.len() / w;
    let mut out = Vec::with_capacity(bytes.len());
    for p in 0..w {
        out.extend((0..n).map(|i| bytes[i * w + p]));
    }
    out
}

/// Values packed at `bits` each, low bits first.
pub fn bitpack(vals: impl Iterator<Item = u64>, bits: u32, out: &mut Vec<u8>) {
    let (mut acc, mut have) = (0u128, 0u32);
    for v in vals {
        acc |= (v as u128) << have;
        have += bits;
        while have >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            have -= 8;
        }
    }
    if have > 0 {
        out.push(acc as u8);
    }
}

fn bits_for(max: u64) -> u32 {
    64 - max.leading_zeros()
}
fn bytes_for(max: u64) -> usize {
    (bits_for(max) as usize).div_ceil(8).max(1)
}

type Make = Box<dyn Fn(&Img) -> Vec<u8> + Send + Sync>;

pub fn jobs(img: &Img) -> Vec<Job> {
    let mut jobs = Vec::new();
    let mut add = |col: &str, kind: &str, variant: &'static str, make: Make| {
        jobs.push(Job { column: col.to_string(), kind: kind.to_string(), variant, make });
    };
    for c in 0..img.ncols() {
        let name = img.def(c).name.clone();
        let name = name.as_str();
        match img.kind(c) {
            Kind::Text => {
                add(name, "text", "stored", Box::new(move |i| stored(i, c)));
                add(name, "text", "lens-varint", Box::new(move |i| text_lens(i, c, 0)));
                add(name, "text", "lens-u16", Box::new(move |i| text_lens(i, c, 2)));
                add(name, "text", "nul-sep", Box::new(move |i| text_sep(i, c)));
                add(name, "text", "dedup-dist", Box::new(move |i| text_dedup(i, c, false)));
                add(name, "text", "dedup-id", Box::new(move |i| text_dedup(i, c, true)));
                add(name, "text", "front", Box::new(move |i| text_front(i, c)));
                add(name, "text", "len-inline", Box::new(move |i| text_inline(i, c)));
                add(name, "text", "dedup-dist-sep", Box::new(move |i| text_dedup_sep(i, c)));
                add(name, "text", "numsuffix", Box::new(move |i| text_numsuffix(i, c)));
                add(name, "text", "words-64", Box::new(move |i| text_words(i, c, 64)));
                add(name, "text", "words-120", Box::new(move |i| text_words(i, c, 120)));
                add(name, "text", "words-32", Box::new(move |i| text_words(i, c, 32)));
                add(name, "text", "words-0", Box::new(move |i| text_words(i, c, 0)));
                add(name, "text", "words-64-dedup", Box::new(move |i| text_words_dedup(i, c, 64)));
            }
            Kind::Dict => {
                add(name, "codes", "stored", Box::new(move |i| stored(i, c)));
                add(name, "codes", "bitpack", Box::new(move |i| codes_bitpack(i, c)));
                add(name, "codes", "rle", Box::new(move |i| codes_rle(i, c)));
                add(name, "codes", "freq-varint", Box::new(move |i| codes_freq(i, c, false)));
                add(name, "codes", "freq-planes", Box::new(move |i| codes_freq(i, c, true)));
                if img.def(c).code_width() == 2 {
                    add(name, "codes", "planes", Box::new(move |i| per_group(i, c, |b| planes(b, 2))));
                }
                add(name, "dict", "stored", Box::new(move |i| i.dict_stored(c).to_vec()));
                add(name, "dict", "lens-varint", Box::new(move |i| dict_lens(i, c)));
                add(name, "dict", "nul-sep", Box::new(move |i| dict_sep(i, c)));
                add(name, "dict", "sorted-sep", Box::new(move |i| dict_sorted(i, c, false)));
                add(name, "dict", "sorted-front", Box::new(move |i| dict_sorted(i, c, true)));
            }
            Kind::Int(w) => {
                add(name, "int", "stored", Box::new(move |i| stored(i, c)));
                add(name, "int", "delta", Box::new(move |i| per_group(i, c, move |b| delta(b, w))));
                add(name, "int", "delta-varint", Box::new(move |i| int_delta_varint(i, c)));
                if w > 1 {
                    add(name, "int", "planes", Box::new(move |i| per_group(i, c, move |b| planes(b, w))));
                    add(name, "int", "delta-planes", Box::new(move |i| per_group(i, c, move |b| planes(&delta(b, w), w))));
                }
                add(name, "int", "for-narrow", Box::new(move |i| int_for(i, c, false)));
                add(name, "int", "for-bitpack", Box::new(move |i| int_for(i, c, true)));
                add(name, "int", "valdict", Box::new(move |i| valdict(i, c, false)));
            }
            Kind::Float => {
                add(name, "float", "stored", Box::new(move |i| stored(i, c)));
                add(name, "float", "planes", Box::new(move |i| per_group(i, c, |b| planes(b, 8))));
                add(name, "float", "xor-prev", Box::new(move |i| per_group(i, c, xor_prev)));
                add(name, "float", "alp-varint", Box::new(move |i| alp(i, c, 0)));
                add(name, "float", "alp-narrow", Box::new(move |i| alp(i, c, 1)));
                add(name, "float", "alp-planes", Box::new(move |i| alp(i, c, 2)));
                add(name, "float", "valdict", Box::new(move |i| valdict(i, c, false)));
                add(name, "float", "valdict-alp", Box::new(move |i| valdict(i, c, true)));
            }
            Kind::Bool => add(name, "bool", "stored", Box::new(move |i| stored(i, c))),
        }
    }
    add("(validity, all columns)", "validity", "stored", Box::new(validity_all));
    add("(headers, footer)", "misc", "stored", Box::new(misc));
    jobs
}

/// The column's value segments exactly as the image holds them (padding
/// included), group after group. Validity is counted on its own.
fn stored(img: &Img, c: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        out.extend_from_slice(img.seg(g, c, 0));
        out.extend_from_slice(img.seg(g, c, 1));
    }
    out
}

fn per_group(img: &Img, c: usize, f: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        out.extend(f(img.fixed(g, c)));
    }
    out
}

fn validity_all(img: &Img) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        for c in 0..img.ncols() {
            out.extend_from_slice(img.seg(g, c, 2));
        }
    }
    out
}

/// Everything that is not a segment or a dictionary: header, dictionary
/// lengths, group headers, footer.
fn misc(img: &Img) -> Vec<u8> {
    let mut out = Vec::new();
    let first_dict = img.cat.dicts.iter().flatten().map(|d| d.offsets_off).min();
    let head_end = first_dict.unwrap_or(img.cat.groups[0].offset) as usize;
    out.extend_from_slice(&img.bytes[..head_end]);
    for g in 0..img.ngroups() {
        let gm = &img.cat.groups[g];
        let seg0 = facetful_format::read::segment_offset(&img.cat, gm, 0, 0) as usize;
        out.extend_from_slice(&img.bytes[gm.offset as usize..seg0]);
    }
    let last = img.cat.groups.last().unwrap();
    let nc = img.ncols();
    let end = facetful_format::read::segment_offset(&img.cat, last, nc - 1, 2) as usize + last.cols[nc - 1].seg_lens[2] as usize;
    out.extend_from_slice(&img.bytes[end..]);
    out
}

// ---- text ----

/// Lengths then bytes per group. `width` 0: varints; 2: u16 (u32 when a
/// group has a longer value).
fn text_lens(img: &Img, c: usize, width: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        let (offs, bytes) = img.text(g, c);
        let lens = offs.windows(2).map(|w| w[1] - w[0]);
        if width == 0 {
            lens.for_each(|l| varint(&mut out, l as u64));
        } else if lens.clone().max().unwrap_or(0) < 65536 {
            lens.for_each(|l| out.extend_from_slice(&(l as u16).to_le_bytes()));
        } else {
            lens.for_each(|l| out.extend_from_slice(&l.to_le_bytes()));
        }
        out.extend_from_slice(bytes);
    }
    out
}

/// Values end with a 0 byte, no lengths (when no value holds a 0 byte).
fn text_sep(img: &Img, c: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        let (offs, bytes) = img.text(g, c);
        assert!(!bytes.contains(&0), "text holds a 0 byte");
        for w in offs.windows(2) {
            out.extend_from_slice(&bytes[w[0] as usize..w[1] as usize]);
            out.push(0);
        }
    }
    out
}

/// A value seen before is a reference, not a copy. Per group: one varint a
/// row (0: new value; otherwise the distance in rows to its last use, or
/// with `by_id` the number of the earlier distinct value, plus one), then
/// the new values' lengths, then their bytes.
fn text_dedup(img: &Img, c: usize, by_id: bool) -> Vec<u8> {
    let mut seen: HashMap<&[u8], u64> = HashMap::new();
    let mut out = Vec::new();
    let mut row = 0u64;
    for g in 0..img.ngroups() {
        let (offs, bytes) = img.text(g, c);
        let (mut toks, mut lens, mut lits) = (Vec::new(), Vec::new(), Vec::new());
        for w in offs.windows(2) {
            let v = &bytes[w[0] as usize..w[1] as usize];
            row += 1;
            let next_id = seen.len() as u64 + 1;
            match seen.get_mut(v) {
                Some(at) if by_id => varint(&mut toks, *at),
                Some(at) => {
                    varint(&mut toks, row - *at);
                    *at = row;
                }
                None => {
                    seen.insert(v, if by_id { next_id } else { row });
                    toks.push(0);
                    varint(&mut lens, v.len() as u64);
                    lits.extend_from_slice(v);
                }
            }
        }
        out.extend(toks);
        out.extend(lens);
        out.extend(lits);
    }
    out
}

/// Each value after its length, a varint.
fn text_inline(img: &Img, c: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        let (offs, bytes) = img.text(g, c);
        for w in offs.windows(2) {
            varint(&mut out, (w[1] - w[0]) as u64);
            out.extend_from_slice(&bytes[w[0] as usize..w[1] as usize]);
        }
    }
    out
}

/// `text_dedup` by distance, the new values ending with a 0 byte.
fn text_dedup_sep(img: &Img, c: usize) -> Vec<u8> {
    let mut seen: HashMap<&[u8], u64> = HashMap::new();
    let mut out = Vec::new();
    let mut row = 0u64;
    for g in 0..img.ngroups() {
        let (offs, bytes) = img.text(g, c);
        let (mut toks, mut lits) = (Vec::new(), Vec::new());
        for w in offs.windows(2) {
            let v = &bytes[w[0] as usize..w[1] as usize];
            row += 1;
            match seen.insert(v, row) {
                Some(at) => varint(&mut toks, row - at),
                None => {
                    toks.push(0);
                    lits.extend_from_slice(v);
                    lits.push(0);
                }
            }
        }
        out.extend(toks);
        out.extend(lits);
    }
    out
}

/// Identifiers that end in a number: the text before it (ending with a 0
/// byte), the number's digit count, and the number as the difference from
/// the previous row's.
fn text_numsuffix(img: &Img, c: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        let (offs, bytes) = img.text(g, c);
        let (mut heads, mut widths, mut nums) = (Vec::new(), Vec::new(), Vec::new());
        let mut prev = 0i64;
        for w in offs.windows(2) {
            let v = &bytes[w[0] as usize..w[1] as usize];
            let digits = v.iter().rev().take(18).take_while(|b| b.is_ascii_digit()).count();
            let (head, tail) = v.split_at(v.len() - digits);
            let n = tail.iter().fold(0i64, |a, &b| a * 10 + (b - b'0') as i64);
            heads.extend_from_slice(head);
            heads.push(0);
            widths.push(digits as u8);
            if digits > 0 {
                varint(&mut nums, zigzag(n - prev));
                prev = n;
            }
        }
        out.extend(heads);
        out.extend(widths);
        out.extend(nums);
    }
    out
}

/// Common words as byte codes (after XWRT and friends): a word is a run of
/// ASCII letters, with the space before it when there is one. The `one`
/// words that save most get one byte (0x80…), the next (128 - one) * 256 two
/// bytes; other bytes of 0x80 and over follow a 0x01 escape. The word list
/// comes first, then the values, each ending in a 0 byte.
fn text_words(img: &Img, c: usize, one: usize) -> Vec<u8> {
    text_words_opt(img, c, one, false)
}

/// `text_words` with repeated values as distances (`text_dedup_sep`).
fn text_words_dedup(img: &Img, c: usize, one: usize) -> Vec<u8> {
    text_words_opt(img, c, one, true)
}

fn text_words_opt(img: &Img, c: usize, one: usize, dedup: bool) -> Vec<u8> {
    fn tokens<'a>(v: &'a [u8], mut f: impl FnMut(&'a [u8], bool)) {
        let mut i = 0;
        while i < v.len() {
            let start = i;
            let mut j = i + (v[i] == b' ') as usize;
            let w0 = j;
            while j < v.len() && v[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j > w0 {
                f(&v[start..j], true);
                i = j;
            } else {
                f(&v[i..i + 1], false);
                i += 1;
            }
        }
    }
    let mut count: HashMap<&[u8], u32> = HashMap::new();
    let groups: Vec<_> = (0..img.ngroups()).map(|g| img.text(g, c)).collect();
    for (offs, bytes) in &groups {
        for w in offs.windows(2) {
            tokens(&bytes[w[0] as usize..w[1] as usize], |t, word| {
                if word && t.len() >= 2 {
                    *count.entry(t).or_default() += 1;
                }
            });
        }
    }
    let two = (128 - one) * 256;
    let mut by_gain: Vec<(&[u8], u32)> = count.into_iter().filter(|&(t, n)| n >= 4 && t.len() >= 3).collect();
    by_gain.sort_by_key(|&(t, n)| (std::cmp::Reverse(n as u64 * (t.len() as u64 - 1)), t));
    by_gain.truncate(one + two);
    let code: HashMap<&[u8], usize> = by_gain.iter().enumerate().map(|(i, &(t, _))| (t, i)).collect();
    let mut out = Vec::new();
    for (t, _) in &by_gain {
        out.extend_from_slice(t);
        out.push(0);
    }
    let mut seen: HashMap<&[u8], u64> = HashMap::new();
    let mut row = 0u64;
    for (offs, bytes) in &groups {
        let mut toks = Vec::new();
        let lits_at = out.len();
        for w in offs.windows(2) {
            row += 1;
            if dedup {
                if let Some(at) = seen.insert(&bytes[w[0] as usize..w[1] as usize], row) {
                    varint(&mut toks, row - at);
                    continue;
                }
                toks.push(0);
            }
            tokens(&bytes[w[0] as usize..w[1] as usize], |t, word| match code.get(t) {
                Some(&k) if word && k < one => out.push(0x80 + k as u8),
                Some(&k) if word => {
                    out.push((0x80 + one + (k - one) / 256) as u8);
                    out.push(((k - one) % 256) as u8);
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
        out.splice(lits_at..lits_at, toks);
    }
    out
}

/// Front coding against the previous row: shared prefix length, rest length,
/// rest bytes.
fn text_front(img: &Img, c: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        let (offs, bytes) = img.text(g, c);
        let (mut pre, mut lens, mut rest) = (Vec::new(), Vec::new(), Vec::new());
        let mut prev: &[u8] = &[];
        for w in offs.windows(2) {
            let v = &bytes[w[0] as usize..w[1] as usize];
            let p = v.iter().zip(prev).take_while(|(a, b)| a == b).count();
            varint(&mut pre, p as u64);
            varint(&mut lens, (v.len() - p) as u64);
            rest.extend_from_slice(&v[p..]);
            prev = v;
        }
        out.extend(pre);
        out.extend(lens);
        out.extend(rest);
    }
    out
}

fn dict_lens(img: &Img, c: usize) -> Vec<u8> {
    let (offs, bytes) = img.dict(c);
    let mut out = Vec::new();
    offs.windows(2).for_each(|w| varint(&mut out, (w[1] - w[0]) as u64));
    out.extend_from_slice(bytes);
    out
}

fn dict_sep(img: &Img, c: usize) -> Vec<u8> {
    let (offs, bytes) = img.dict(c);
    let mut out = Vec::new();
    for w in offs.windows(2) {
        out.extend_from_slice(&bytes[w[0] as usize..w[1] as usize]);
        out.push(0);
    }
    out
}

/// The dictionary's values in byte order (the codes would have to be
/// renumbered to match: this only measures the dictionary), ending in a 0
/// byte, or front coded against the value before.
fn dict_sorted(img: &Img, c: usize, front: bool) -> Vec<u8> {
    let (offs, bytes) = img.dict(c);
    let mut vals: Vec<&[u8]> = offs.windows(2).map(|w| &bytes[w[0] as usize..w[1] as usize]).collect();
    vals.sort();
    let (mut pre, mut out) = (Vec::new(), Vec::new());
    let mut prev: &[u8] = &[];
    for v in vals {
        let p = if front { v.iter().zip(prev).take_while(|(a, b)| a == b).count().min(255) } else { 0 };
        if front {
            pre.push(p as u8);
        }
        out.extend_from_slice(&v[p..]);
        out.push(0);
        prev = v;
    }
    pre.extend(out);
    pre
}

// ---- dictionary codes ----

fn dict_count(img: &Img, c: usize) -> usize {
    img.dict(c).0.len() - 1
}

fn codes_bitpack(img: &Img, c: usize) -> Vec<u8> {
    let bits = bits_for(dict_count(img, c).saturating_sub(1) as u64).max(1);
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        bitpack(img.codes(g, c).into_iter().map(|x| x as u64), bits, &mut out);
    }
    out
}

/// Runs: each code once at its stored width, then the run lengths as varints.
fn codes_rle(img: &Img, c: usize) -> Vec<u8> {
    let w = img.def(c).code_width();
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        let codes = img.codes(g, c);
        let (mut vals, mut runs) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < codes.len() {
            let mut j = i + 1;
            while j < codes.len() && codes[j] == codes[i] {
                j += 1;
            }
            vals.extend_from_slice(&codes[i].to_le_bytes()[..w]);
            varint(&mut runs, (j - i) as u64);
            i = j;
        }
        out.extend(vals);
        out.extend(runs);
    }
    out
}

/// Codes renumbered by how common they are over the whole column (a table
/// maps them back), as varints or as byte planes.
fn codes_freq(img: &Img, c: usize, as_planes: bool) -> Vec<u8> {
    let n = dict_count(img, c).max(1);
    let mut count = vec![0u64; 65536.max(n)];
    for g in 0..img.ngroups() {
        img.codes(g, c).into_iter().for_each(|x| count[x as usize] += 1);
    }
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by_key(|&x| std::cmp::Reverse(count[x as usize]));
    let mut rank = vec![0u32; count.len()];
    order.iter().enumerate().for_each(|(r, &x)| rank[x as usize] = r as u32);
    let w = img.def(c).code_width();
    let mut out = Vec::new();
    for x in &order {
        out.extend_from_slice(&x.to_le_bytes()[..w]);
    }
    for g in 0..img.ngroups() {
        let codes = img.codes(g, c);
        if as_planes {
            let mut b = Vec::with_capacity(codes.len() * w);
            codes.iter().for_each(|&x| b.extend_from_slice(&rank[x as usize].to_le_bytes()[..w]));
            out.extend(planes(&b, w));
        } else {
            codes.iter().for_each(|&x| varint(&mut out, rank[x as usize] as u64));
        }
    }
    out
}

// ---- integers ----

/// Each value minus the one before, wrapping, at the stored width.
fn delta(bytes: &[u8], w: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut prev = 0u64;
    for v in bytes.chunks_exact(w) {
        let mut b = [0u8; 8];
        b[..w].copy_from_slice(v);
        let x = u64::from_le_bytes(b);
        out.extend_from_slice(&x.wrapping_sub(prev).to_le_bytes()[..w]);
        prev = x;
    }
    out
}

fn int_delta_varint(img: &Img, c: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        let mut prev = 0i64;
        for v in img.ints(g, c) {
            varint(&mut out, zigzag(v.wrapping_sub(prev)));
            prev = v;
        }
    }
    out
}

/// Frame of reference: each group's minimum, then the values above it at the
/// narrowest byte width, or bit-packed.
fn int_for(img: &Img, c: usize, bits: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        for_group(&img.ints(g, c), bits, &mut out);
    }
    out
}

fn for_group(vals: &[i64], bits: bool, out: &mut Vec<u8>) {
    let min = vals.iter().copied().min().unwrap_or(0);
    let max = vals.iter().copied().max().unwrap_or(0);
    let range = max.wrapping_sub(min) as u64;
    out.extend_from_slice(&min.to_le_bytes());
    if bits {
        let b = bits_for(range).max(1);
        out.push(b as u8);
        bitpack(vals.iter().map(|&v| v.wrapping_sub(min) as u64), b, out);
    } else {
        let w = bytes_for(range);
        out.push(w as u8);
        for &v in vals {
            out.extend_from_slice(&(v.wrapping_sub(min) as u64).to_le_bytes()[..w]);
        }
    }
}

/// The column's distinct values in a table, most common first (decimals as
/// scaled integers when `alp_table`), and each row its value's number, a
/// varint.
fn valdict(img: &Img, c: usize, alp_table: bool) -> Vec<u8> {
    let mut count: HashMap<u64, u64> = HashMap::new();
    let key = |g: usize| -> Vec<u64> { img.fixed(g, c).chunks_exact(img.fixed(g, c).len() / img.rows(g)).map(|b| { let mut x = [0u8; 8]; x[..b.len()].copy_from_slice(b); u64::from_le_bytes(x) }).collect() };
    for g in 0..img.ngroups() {
        key(g).into_iter().for_each(|k| *count.entry(k).or_default() += 1);
    }
    let mut order: Vec<(u64, u64)> = count.into_iter().collect();
    order.sort_by_key(|&(k, n)| (std::cmp::Reverse(n), k));
    let rank: HashMap<u64, u64> = order.iter().enumerate().map(|(r, &(k, _))| (k, r as u64)).collect();
    let mut out = Vec::new();
    varint(&mut out, order.len() as u64);
    if alp_table {
        let vals: Vec<f64> = order.iter().map(|&(k, _)| f64::from_bits(k)).collect();
        alp_group(&vals, 0, &mut out);
    } else {
        let w = img.fixed(0, c).len() / img.rows(0);
        order.iter().for_each(|&(k, _)| out.extend_from_slice(&k.to_le_bytes()[..w]));
    }
    for g in 0..img.ngroups() {
        key(g).into_iter().for_each(|k| varint(&mut out, rank[&k]));
    }
    out
}

// ---- floats ----

fn xor_prev(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut prev = 0u64;
    for v in bytes.chunks_exact(8) {
        let x = u64::from_le_bytes(v.try_into().unwrap());
        out.extend_from_slice(&(x ^ prev).to_le_bytes());
        prev = x;
    }
    out
}

/// The integer `i` with `i / 10^e == v` exactly, when there is one.
pub fn decimal(v: f64, e: u32) -> Option<i64> {
    const P: [f64; 19] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18];
    let r = (v * P[e as usize]).round();
    if !(r.abs() < 9007199254740992.0) {
        return None;
    }
    ((r as i64 as f64 / P[e as usize]).to_bits() == v.to_bits()).then_some(r as i64)
}

/// Decimals in disguise as scaled integers (after ALP): per group the power
/// of ten that costs least, the integers, and the values that power cannot
/// give (row, raw bits). `mode` 0: zigzag varints; 1: frame of reference at
/// the narrowest byte width; 2: i64 byte planes.
fn alp(img: &Img, c: usize, mode: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        alp_group(&img.floats(g, c), mode, &mut out);
    }
    out
}

fn alp_group(vals: &[f64], mode: u8, out: &mut Vec<u8>) {
    let mut best = (u64::MAX, 0u32);
    for e in 0..=18u32 {
        let (mut exc, mut maxabs) = (0u64, 0u64);
        for &v in vals {
            match decimal(v, e) {
                Some(i) => maxabs = maxabs.max(i.unsigned_abs()),
                None => exc += 1,
            }
        }
        let cost = exc * 10 + vals.len() as u64 * (bits_for(maxabs) as u64 + 1) / 8;
        if cost < best.0 {
            best = (cost, e);
        }
    }
    let e = best.1;
    let mut ints = Vec::with_capacity(vals.len());
    let mut exc = Vec::new();
    let mut nexc = 0u64;
    let mut last = 0usize;
    for (i, &v) in vals.iter().enumerate() {
        match decimal(v, e) {
            Some(x) => ints.push(x),
            None => {
                ints.push(0);
                varint(&mut exc, (i - last) as u64);
                last = i;
                exc.extend_from_slice(&v.to_le_bytes());
                nexc += 1;
            }
        }
    }
    out.push(e as u8);
    match mode {
        0 => ints.iter().for_each(|&x| varint(out, zigzag(x))),
        1 => for_group(&ints, false, out),
        _ => {
            let b: Vec<u8> = ints.iter().flat_map(|x| x.to_le_bytes()).collect();
            out.extend(planes(&b, 8));
        }
    }
    varint(out, nexc);
    out.extend(exc);
}

/// Several fixed-width columns as one: each column's group after the other
/// (`interleave` false, what a packed image does) or row by row, one value of
/// each (`interleave` true). Floats as scaled-integer varints when `alp_on`,
/// everything else as stored.
pub fn tuple(img: &Img, cols: &[usize], interleave: bool, alp_on: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for g in 0..img.ngroups() {
        let n = img.rows(g);
        // per column, per row, its bytes
        let mut per: Vec<Vec<Vec<u8>>> = Vec::new();
        for &c in cols {
            let rows: Vec<Vec<u8>> = if img.kind(c) == Kind::Float && alp_on {
                let vals = img.floats(g, c);
                let e = (0..=18u32).min_by_key(|&e| vals.iter().filter(|&&v| decimal(v, e).is_none()).count() * 1000 + e as usize).unwrap();
                vals.iter().map(|&v| { let mut b = Vec::new(); match decimal(v, e) { Some(x) => varint(&mut b, zigzag(x)), None => { b.push(0xFF); b.extend_from_slice(&v.to_le_bytes()) } } b }).collect()
            } else {
                let f = img.fixed(g, c);
                f.chunks_exact(f.len() / n).map(|b| b.to_vec()).collect()
            };
            per.push(rows);
        }
        if interleave {
            for i in 0..n { for p in &per { out.extend_from_slice(&p[i]); } }
        } else {
            for p in &per { for r in p { out.extend_from_slice(r); } }
        }
    }
    out
}
