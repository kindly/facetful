//! Packs an image for the wire (the form `packed` describes and unpacks).
//! Every choice here is the packer's alone: the unpacker reads a tag and
//! does what it says, so the rules for when an encoding pays can change
//! without touching readers. `pack` unpacks what it made and compares, so a
//! packed image always gives back the image it came from.
//!
//! Only compiled into what writes images; the engine's wasm never calls it.

use std::collections::HashMap;

use crate::packed::*;
use crate::read;
use crate::{align_up, Catalog, ColumnType, DictLoc, FormatError, PACKED_VERSION, VERSION};

/// Which encodings the packer may use. Each is measured in
/// docs/file-size.sv; `ALL` is what an image should be published with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Opts {
    /// text values end in a 0 byte instead of carrying offsets
    pub text_sep: bool,
    /// identifier-like text: the trailing number as a difference from the
    /// row before
    pub numbered: bool,
    /// dictionary values end in a 0 byte instead of carrying offsets
    pub dict_sep: bool,
    /// floats that are decimals: integers and a power of ten
    pub decimals: bool,
    /// common words of text columns and dictionaries as byte codes: how many
    /// of the 128 lead bytes are a code on their own (the rest start
    /// two-byte codes); None: no word codes
    pub words: Option<u8>,
    /// integers that run in sequence: differences
    pub delta: bool,
}

impl Opts {
    pub const NONE: Opts = Opts { text_sep: false, numbered: false, dict_sep: false, decimals: false, words: None, delta: false };
    pub const ALL: Opts = Opts { text_sep: true, numbered: true, dict_sep: true, decimals: true, words: Some(64), delta: true };
}

impl Default for Opts {
    fn default() -> Self {
        Opts::ALL
    }
}

const ODD: FormatError = FormatError::Corrupt("image cannot be packed: a segment is not where its lengths put it");

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

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The integer `i` with `i / 10^e == v` to the bit, when there is one: the
/// division is the one the unpacker does.
fn decimal(v: f64, e: usize) -> Option<i64> {
    let r = (v * P10[e]).round();
    // written so that NaN fails it too
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(r.abs() < 9007199254740992.0) {
        return None;
    }
    ((r as i64 as f64 / P10[e]).to_bits() == v.to_bits()).then_some(r as i64)
}

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
struct WordList<'a> {
    one: usize,
    words: Vec<&'a [u8]>,
    code: HashMap<&'a [u8], u32>,
}

impl<'a> WordList<'a> {
    fn build(values: impl Iterator<Item = &'a [u8]>, one: usize) -> WordList<'a> {
        let mut count: HashMap<&[u8], u32> = HashMap::new();
        for v in values {
            tokens(v, |t, word| {
                if word && t.len() >= 3 {
                    *count.entry(t).or_default() += 1;
                }
            });
        }
        let mut best: Vec<(&[u8], u32)> = count.into_iter().filter(|&(_, n)| n >= 4).collect();
        // by the bytes a code saves; ties by the word, so the same image
        // always packs to the same bytes
        best.sort_by_key(|&(t, n)| (std::cmp::Reverse(n as u64 * (t.len() as u64 - 1)), t));
        best.truncate(one + (WORD_LEADS - one) * 256);
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

    /// One value: words as codes, bytes that would read as something else
    /// after an escape, then the closing 0.
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
                    if b >= 0x80 || b <= ESCAPE {
                        out.push(ESCAPE);
                    }
                    out.push(b);
                }
            }
        });
        out.push(0);
    }
}

/// How many of a value's last bytes are digits `T_NUM` takes as its number.
fn trailing_digits(v: &[u8]) -> usize {
    v.iter().rev().take(NUM_DIGITS).take_while(|b| b.is_ascii_digit()).count()
}

/// Whether a text column reads as identifiers: nearly every value ends in
/// digits, four or more on average.
fn is_numbered<'a>(values: impl Iterator<Item = &'a [u8]>) -> bool {
    let (mut n, mut with, mut digits) = (0usize, 0usize, 0usize);
    for v in values {
        let d = trailing_digits(v);
        n += 1;
        with += (d > 0) as usize;
        digits += d;
    }
    n > 0 && with * 10 >= n * 9 && digits >= with * 4
}

/// One row group of a plain text column.
struct TextSeg<'a> {
    rows: usize,
    /// the rows + 1 offsets
    offs: &'a [u8],
    /// the text, without the segment's padding
    text: &'a [u8],
    /// offsets start at 0, rise, and end where the padded text does: values
    /// can be read out and written back. Otherwise the group is stored.
    clean: bool,
}

impl<'a> TextSeg<'a> {
    fn new(offs_seg: &'a [u8], bytes_seg: &'a [u8], rows: usize) -> Result<TextSeg<'a>, FormatError> {
        let offs = offs_seg.get(..(rows + 1) * 4).ok_or(ODD)?;
        let end = u32_at(offs, rows * 4) as usize;
        let rising = offs.chunks_exact(4).zip(offs[4..].chunks_exact(4)).all(|(a, b)| u32_at(a, 0) <= u32_at(b, 0));
        let clean = u32_at(offs, 0) == 0 && rising && align_up(end) == bytes_seg.len();
        Ok(TextSeg { rows, offs, text: if clean { &bytes_seg[..end] } else { bytes_seg }, clean })
    }

    fn values(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        let (offs, text) = (self.offs, self.text);
        (0..if self.clean { self.rows } else { 0 }).map(move |i| &text[u32_at(offs, i * 4) as usize..u32_at(offs, i * 4 + 4) as usize])
    }
}

/// A dictionary as the image holds it.
struct Dict<'a> {
    offs: &'a [u8],
    bytes: &'a [u8],
    /// the values, when the padded offsets and bytes are just what their
    /// count and text give back; otherwise the dictionary is stored
    values: Option<Vec<&'a [u8]>>,
}

impl<'a> Dict<'a> {
    fn new(image: &'a [u8], d: &DictLoc) -> Result<Dict<'a>, FormatError> {
        let at = |off: u64, len: u32| usize::try_from(off).ok().and_then(|o| image.get(o..o.checked_add(len as usize)?)).ok_or(ODD);
        let (offs, bytes) = (at(d.offsets_off, d.offsets_len)?, at(d.bytes_off, d.bytes_len)?);
        let all: Vec<usize> = offs.chunks_exact(4).map(|c| u32_at(c, 0) as usize).collect();
        // the real offsets end where they stop rising (zero padding after)
        let mut n = all.len().min(1);
        while n < all.len() && all[n] >= all[n - 1] && !(all[n] == 0 && all[n - 1] > 0) {
            n += 1;
        }
        let fits = n > 0 && all[0] == 0 && align_up(n * 4) == offs.len() && align_up(all[n - 1]) == bytes.len();
        let values = fits.then(|| all[..n].windows(2).map(|w| &bytes[w[0]..w[1]]).collect());
        Ok(Dict { offs, bytes, values })
    }
}

/// `image` packed with every encoding (`Opts::ALL`).
pub fn pack(image: &[u8]) -> Result<Vec<u8>, FormatError> {
    pack_with(image, Opts::ALL)
}

/// `image` in its packed form. Fails for bytes that are not a whole
/// version 1 image, and for an image the packed form would not give back
/// exactly (none that this crate's writer makes).
pub fn pack_with(image: &[u8], opts: Opts) -> Result<Vec<u8>, FormatError> {
    let cat = read::open(&image)?;
    if cat.version != VERSION {
        return Err(FormatError::UnsupportedVersion(cat.version));
    }
    let (_, first) = read::open_prefix(image)?.ok_or(FormatError::Truncated)?;
    let wire = pack_parts(image, &cat, first as usize, opts, None)?;
    if unpack(&wire)? != image {
        return Err(FormatError::Corrupt("image cannot be packed: unpacking would not give it back exactly"));
    }
    Ok(wire)
}

/// Each column's share of the packed form, in schema order: its word list,
/// its dictionary and its row-group payloads as they are on the wire. For
/// size reports: compressed apart, they show what each column costs to
/// download (their sum runs a little over the whole image's size, since
/// columns that repeat each other compress better together).
pub fn pack_columns(image: &[u8], opts: Opts) -> Result<Vec<Vec<u8>>, FormatError> {
    let cat = read::open(&image)?;
    let (_, first) = read::open_prefix(image)?.ok_or(FormatError::Truncated)?;
    let mut shares = vec![Vec::new(); cat.schema.columns.len()];
    pack_parts(image, &cat, first as usize, opts, Some(&mut shares))?;
    Ok(shares)
}

fn pack_parts(image: &[u8], cat: &Catalog, first: usize, opts: Opts, mut shares: Option<&mut Vec<Vec<u8>>>) -> Result<Vec<u8>, FormatError> {
    // what `out` gained since `from` is column `ci`'s
    let mut share = |ci: usize, out: &[u8], from: usize| {
        if let Some(s) = shares.as_deref_mut() {
            s[ci].extend_from_slice(&out[from..]);
        }
    };
    let ncols = cat.schema.columns.len();
    if ncols == 0 {
        return Err(ODD);
    }
    let hlen = u32_at(image, 4) as usize;
    let one = opts.words.map(|one| (one as usize).min(WORD_LEADS));

    // where every segment lies, checked once
    let seg = |g: &crate::GroupMeta, ci: usize, s: usize| {
        let off = usize::try_from(read::segment_offset(cat, g, ci, s)).map_err(|_| ODD)?;
        off.checked_add(g.cols[ci].seg_lens[s] as usize).and_then(|end| image.get(off..end)).ok_or(ODD)
    };
    let mut texts: Vec<Vec<TextSeg>> = (0..ncols).map(|_| Vec::new()).collect();
    let mut dicts: Vec<Option<Dict>> = (0..ncols).map(|_| None).collect();
    for (ci, def) in cat.schema.columns.iter().enumerate() {
        if let Some(d) = &cat.dicts[ci] {
            dicts[ci] = Some(Dict::new(image, d)?);
        } else if def.ty == ColumnType::Utf8 {
            for g in &cat.groups {
                texts[ci].push(TextSeg::new(seg(g, ci, 0)?, seg(g, ci, 1)?, g.row_count as usize)?);
            }
        }
    }

    let mut out = Vec::with_capacity(image.len() / 2);
    out.extend_from_slice(&image[..hlen]);
    out[8..10].copy_from_slice(&PACKED_VERSION.to_le_bytes());
    out.extend_from_slice(&(image.len() as u64).to_le_bytes());
    let prelude = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(image.get(hlen..hlen + read::dict_table_len(&cat.schema)).ok_or(ODD)?);

    // per text column: identifiers, or a word list (for a dictionary
    // column, over the dictionary's values)
    let mut numbered = vec![false; ncols];
    let mut lists: Vec<Option<WordList>> = (0..ncols).map(|_| None).collect();
    out.push(one.map_or(NO_WORDS, |one| one as u8));
    for (ci, def) in cat.schema.columns.iter().enumerate() {
        if def.ty != ColumnType::Utf8 {
            continue;
        }
        let values = || -> Box<dyn Iterator<Item = &[u8]> + '_> {
            match &dicts[ci] {
                Some(d) => Box::new(d.values.iter().flatten().copied()),
                None => Box::new(texts[ci].iter().flat_map(|t| t.values())),
            }
        };
        if dicts[ci].is_none() {
            numbered[ci] = opts.numbered && opts.text_sep && is_numbered(values());
        }
        let Some(one) = one else { continue };
        // word codes take the bytes of 0x80 and over, so text in other
        // scripts would pay an escape for each of its bytes: columns that
        // are mostly ASCII only, and large enough to repay a word list
        let (mut bytes, mut high) = (0usize, 0usize);
        for v in values() {
            bytes += v.len();
            high += v.iter().filter(|&&b| b >= 0x80).count();
        }
        if !numbered[ci] && bytes >= 1 << 16 && high * 50 <= bytes {
            let list = WordList::build(values(), one);
            let from = out.len();
            list.write(&mut out);
            share(ci, &out, from);
            lists[ci] = Some(list);
        } else {
            varint(&mut out, 0);
        }
    }
    for (ci, d) in dicts.iter().enumerate() {
        let Some(d) = d else { continue };
        let from = out.len();
        match (&d.values, &lists[ci]) {
            (Some(values), Some(list)) => {
                out.push(D_WORDS);
                varint(&mut out, values.len() as u64);
                values.iter().for_each(|v| list.encode(v, &mut out));
            }
            (Some(values), None) if opts.dict_sep && !values.iter().any(|v| v.contains(&0)) => {
                out.push(D_SEP);
                varint(&mut out, values.len() as u64);
                for v in values {
                    out.extend_from_slice(v);
                    out.push(0);
                }
            }
            _ => {
                out.push(D_STORED);
                out.extend_from_slice(d.offs);
                out.extend_from_slice(d.bytes);
            }
        }
        share(ci, &out, from);
    }
    let len = (out.len() - prelude - 4) as u32;
    out[prelude..prelude + 4].copy_from_slice(&len.to_le_bytes());

    let mut end = first;
    for (gi, g) in cat.groups.iter().enumerate() {
        if g.offset != end as u64 {
            return Err(ODD);
        }
        let n = g.row_count as usize;
        let at = out.len();
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&g.row_count.to_le_bytes());
        for c in &g.cols {
            varint(&mut out, c.null_count as u64);
        }
        for (ci, (def, cm)) in cat.schema.columns.iter().zip(&g.cols).enumerate() {
            let fixed = |len: usize| seg(g, ci, 0)?.get(..len).ok_or(ODD);
            let from = out.len();
            match (def.ty, def.is_dict()) {
                (ColumnType::Utf8, true) => out.extend_from_slice(fixed(n * def.code_width())?),
                (ColumnType::Utf8, false) => pack_text(&texts[ci][gi], opts, numbered[ci], lists[ci].as_ref(), &mut out),
                (ColumnType::Bool, _) => out.extend_from_slice(fixed(n.div_ceil(8))?),
                (ColumnType::Float64, _) => pack_floats(fixed(n * 8)?, opts, &mut out),
                (ty, _) => {
                    let w = ty.fixed_width().expect("fixed width");
                    pack_ints(fixed(n * w)?, w, opts, &mut out)
                }
            }
            if cm.null_count > 0 {
                out.extend_from_slice(seg(g, ci, 2)?.get(..n.div_ceil(8)).ok_or(ODD)?);
            }
            share(ci, &out, from);
        }
        let len = u32::try_from(out.len() - at - 4).map_err(|_| ODD)?;
        out[at..at + 4].copy_from_slice(&len.to_le_bytes());
        let last = seg(g, ncols - 1, crate::MAX_SEGS - 1)?;
        end = last.as_ptr() as usize - image.as_ptr() as usize + last.len();
    }
    out.extend_from_slice(&[0; 4]);
    // the footer: everything after the last group
    let footer = image.get(end..).ok_or(ODD)?;
    out.extend_from_slice(&u32::try_from(footer.len()).map_err(|_| ODD)?.to_le_bytes());
    out.extend_from_slice(footer);
    Ok(out)
}

fn pack_text(seg: &TextSeg, opts: Opts, numbered: bool, list: Option<&WordList>, out: &mut Vec<u8>) {
    let stored = |out: &mut Vec<u8>| {
        out.push(T_STORED);
        varint(out, seg.text.len() as u64);
        out.extend_from_slice(seg.offs);
        out.extend_from_slice(seg.text);
    };
    if !seg.clean {
        return stored(out);
    }
    if let Some(list) = list {
        out.push(T_WORDS);
        varint(out, seg.text.len() as u64);
        seg.values().for_each(|v| list.encode(v, out));
        return;
    }
    // a 0 byte inside a value would read as its end
    if !opts.text_sep || seg.text.contains(&0) {
        return stored(out);
    }
    if numbered {
        out.push(T_NUM);
        varint(out, seg.text.len() as u64);
        let (mut widths, mut nums) = (Vec::with_capacity(seg.rows), Vec::new());
        let mut prev = 0i64;
        for v in seg.values() {
            let d = trailing_digits(v);
            let (head, tail) = v.split_at(v.len() - d);
            out.extend_from_slice(head);
            out.push(0);
            widths.push(d as u8);
            if d > 0 {
                let x = tail.iter().fold(0i64, |a, &b| a * 10 + (b - b'0') as i64);
                varint(&mut nums, zigzag(x - prev));
                prev = x;
            }
        }
        out.extend(widths);
        out.extend(nums);
        return;
    }
    out.push(T_SEP);
    varint(out, seg.text.len() as u64);
    for v in seg.values() {
        out.extend_from_slice(v);
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

/// Floats as stored, or as decimals: the power of ten that leaves fewest
/// exceptions, each value times it as a varint, and the exceptions (a 0 in
/// their place) listed after. Stored when over a quarter are exceptions.
fn pack_floats(bytes: &[u8], opts: Opts, out: &mut Vec<u8>) {
    let vals: Vec<f64> = bytes.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect();
    let mut best = (u64::MAX, 0usize, 0u64);
    if opts.decimals {
        for e in 0..P10.len() {
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
    if !opts.decimals || exc as usize * 4 > vals.len() {
        out.push(F_STORED);
        out.extend_from_slice(bytes);
        return;
    }
    out.push(F_DECIMAL);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::{ColumnChunk, DictData, SegmentData, Writer};
    use crate::{flags, ColumnDef, Schema};

    fn offsets(strings: &[String]) -> (Vec<u32>, Vec<u8>) {
        let mut offs = vec![0u32];
        let mut bytes = Vec::new();
        for s in strings {
            bytes.extend_from_slice(s.as_bytes());
            offs.push(bytes.len() as u32);
        }
        (offs, bytes)
    }

    /// An image with every column kind, NULLs, a short last group, and
    /// text for each text encoding. `odd`: text with 0 bytes and other
    /// scripts, floats that are not decimals.
    fn image(rows: u32, group: u32, odd: bool) -> Vec<u8> {
        image_tail(rows, group, odd, None)
    }

    /// `image` whose first dictionary is long enough for word codes and
    /// ends in one more value: `tail.1` bytes and then `tail.0`, so that a
    /// word can end at any distance from the dictionary's padding.
    fn image_tail(rows: u32, group: u32, odd: bool, tail: Option<(&str, usize)>) -> Vec<u8> {
        let col = |name: &str, ty, flags| ColumnDef { name: name.into(), ty, flags };
        let schema = Schema {
            columns: vec![
                col("id", ColumnType::Int32, 0),
                col("big", ColumnType::Int64, 0),
                col("small", ColumnType::Int8, 0),
                col("amount", ColumnType::Float64, 0),
                col("ok", ColumnType::Bool, 0),
                col("status", ColumnType::Utf8, flags::DICTIONARY | flags::CODES_U8),
                col("owner", ColumnType::Utf8, flags::DICTIONARY),
                col("title", ColumnType::Utf8, 0),
                col("ref", ColumnType::Utf8, 0),
                col("note", ColumnType::Utf8, 0),
            ],
        };
        let status: Vec<String> = ["open", "closed", ""].iter().map(|s| s.to_string()).collect();
        // enough dictionary text for a word list
        let owner: Vec<String> = (0..4000).map(|i| format!("The Community Foundation for Region {i} and District")).collect();
        // in the first dictionary, so that more of the prelude follows it
        let mut status = status;
        if let Some((end, t)) = tail {
            status.extend(owner.iter().cloned());
            status.push(format!("{}{end}", "!".repeat(t)));
        }
        let (so, sb) = offsets(&status);
        let (oo, ob) = offsets(&owner);
        let mut dicts: Vec<Option<DictData>> = (0..10).map(|_| None).collect();
        dicts[5] = Some(DictData { offsets: so, bytes: sb });
        dicts[6] = Some(DictData { offsets: oo, bytes: ob });
        let mut w = Writer::new(schema, vec![], group, &dicts);
        let mut at = 0u32;
        while at < rows {
            let n = group.min(rows - at);
            let r = at..at + n;
            let ids: Vec<u8> = r.clone().flat_map(|i| (i as i32 * 3 - 7).to_le_bytes()).collect();
            let big: Vec<u8> = r.clone().flat_map(|i| (i as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64).to_le_bytes()).collect();
            let small: Vec<u8> = r.clone().map(|i| (i % 251) as u8).collect();
            let amount: Vec<u8> = r
                .clone()
                .flat_map(|i| {
                    let v = match (odd, i % 7) {
                        (true, 0) => f64::NAN,
                        (true, 1) => -0.0,
                        (true, 2) => (i as f64).sqrt(),
                        (_, 3) => -(i as f64) / 4.0,
                        _ => i as f64 * 12.5 + 0.01,
                    };
                    v.to_le_bytes()
                })
                .collect();
            let bools: Vec<u8> = (0..n.div_ceil(8)).map(|b| (at + b) as u8).collect();
            let scodes: Vec<u8> = r.clone().map(|i| (i % 3) as u8).collect();
            let ocodes: Vec<u16> = r.clone().map(|i| (i * 7 % 4000) as u16).collect();
            let title: Vec<String> = r
                .clone()
                .map(|i| match (odd, i % 5) {
                    (true, 0) => format!("zero\0inside {i}"),
                    (true, 1) => format!("Ünïcödé \u{1} and Ελληνικά {i}"),
                    _ => format!("Support for the community programme in area {i}, with grant funding"),
                })
                .collect();
            let refs: Vec<String> = r.clone().map(|i| if i % 40 == 39 { "none".into() } else { format!("GB-CHC-{:07}", 1000000 + i * 3) }).collect();
            let note: Vec<String> = r.clone().map(|i| if i % 3 == 0 { String::new() } else { format!("n{i}") }).collect();
            let (to, tb) = offsets(&title);
            let (ro, rb) = offsets(&refs);
            let (no, nb) = offsets(&note);
            let validity: Vec<u8> = (0..n.div_ceil(8)).map(|b| if b % 2 == 0 { 0xB7 } else { 0xFF }).collect();
            let mut valid = validity.clone();
            if n % 8 != 0 {
                *valid.last_mut().unwrap() &= (1u8 << (n % 8)) - 1;
            }
            let nulls = n - valid.iter().map(|b| b.count_ones()).sum::<u32>();
            let plain = |data| ColumnChunk { data, validity: None, null_count: 0 };
            let holed = |data| ColumnChunk { data, validity: (nulls > 0).then_some(&valid[..]), null_count: nulls };
            w.write_group(
                n,
                &[
                    plain(SegmentData::Fixed(&ids)),
                    plain(SegmentData::Fixed(&big)),
                    plain(SegmentData::Fixed(&small)),
                    holed(SegmentData::Fixed(&amount)),
                    plain(SegmentData::Bool(&bools)),
                    plain(SegmentData::Codes8(&scodes)),
                    plain(SegmentData::Codes16(&ocodes)),
                    plain(SegmentData::Utf8 { offsets: &to, bytes: &tb }),
                    plain(SegmentData::Utf8 { offsets: &ro, bytes: &rb }),
                    holed(SegmentData::Utf8 { offsets: &no, bytes: &nb }),
                ],
            );
            at += n;
        }
        w.finish()
    }

    #[test]
    fn packs_and_unpacks_every_kind() {
        for (rows, group, odd) in [(3000u32, 1000u32, false), (3000, 1000, true), (2501, 1000, false), (1, 1000, true), (5000, 5000, true)] {
            let img = image(rows, group, odd);
            for opts in [Opts::ALL, Opts::NONE, Opts { words: None, ..Opts::ALL }, Opts { words: Some(0), ..Opts::ALL }, Opts { words: Some(128), ..Opts::ALL }, Opts { numbered: false, ..Opts::ALL }] {
                let wire = pack_with(&img, opts).unwrap();
                assert!(is_packed(&wire) && !is_packed(&img));
                assert!(unpack(&wire).unwrap() == img, "{rows} rows, {opts:?}");
            }
            let wire = pack(&img).unwrap();
            assert!(wire.len() < img.len(), "packed {} of {}", wire.len(), img.len());
        }
    }

    #[test]
    fn each_encoding_is_used() {
        // with everything on the regular image is smaller than with any one
        // encoding off: each of them took a column. Differences keep the
        // width, so those only change the bytes.
        let img = image(4000, 1000, false);
        let all = pack(&img).unwrap();
        for (name, opts) in [
            ("text_sep", Opts { text_sep: false, numbered: false, words: None, ..Opts::ALL }),
            ("numbered", Opts { numbered: false, ..Opts::ALL }),
            ("dict_sep", Opts { dict_sep: false, words: None, ..Opts::ALL }),
            ("decimals", Opts { decimals: false, ..Opts::ALL }),
            ("words", Opts { words: None, ..Opts::ALL }),
        ] {
            let without = pack_with(&img, opts).unwrap().len();
            assert!(without > all.len() + 1000, "{name}: {without} without, {} with", all.len());
        }
        let stored = pack_with(&img, Opts { delta: false, ..Opts::ALL }).unwrap();
        assert!(stored.len() == all.len() && stored != all, "delta");
    }

    #[test]
    fn unpacks_in_any_chunking() {
        let img = image(2501, 1000, true);
        let wire = pack(&img).unwrap();
        for step in [1usize, 7, 4096, wire.len() - 1] {
            let mut un = Unpacker::new();
            let mut out = Vec::new();
            for c in wire.chunks(step) {
                assert!(!un.done());
                un.push(c, &mut out).unwrap();
                // what is out is always a prefix of the image
                assert!(out == img[..out.len()]);
            }
            assert!(un.done() && out == img, "step {step}");
        }
    }

    #[test]
    fn old_readers_refuse_a_packed_image() {
        let wire = pack(&image(100, 50, false)).unwrap();
        let src: &[u8] = &wire;
        assert!(matches!(read::open(&src), Err(FormatError::UnsupportedVersion(2))));
        assert!(matches!(read::open_prefix(&wire), Err(FormatError::UnsupportedVersion(2))));
        // and the unpacker refuses an image that is not packed, and versions after its own
        assert!(matches!(unpack(&image(100, 50, false)), Err(FormatError::UnsupportedVersion(1))));
        let mut later = wire.clone();
        later[8] = 3;
        assert!(matches!(unpack(&later), Err(FormatError::UnsupportedVersion(3))));
        assert!(matches!(unpack(b"nonsense, not an image"), Err(FormatError::BadMagic)));
    }

    #[test]
    fn damage_is_an_error_not_a_panic() {
        let img = image(600, 250, true);
        let wire = pack(&img).unwrap();
        // cut short anywhere: an error, never a whole image
        for cut in (0..wire.len()).step_by(97) {
            assert!(unpack(&wire[..cut]).is_err(), "cut at {cut}");
        }
        // bytes after the end
        let mut long = wire.clone();
        long.push(0);
        assert!(unpack(&long).is_err());
        // any byte changed: an error, or an image (the image's own checks
        // are the reader's), but no panic and no more output than declared
        let mut state = 0x2545F4914F6CDD1Du64;
        for _ in 0..4000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let at = (state % wire.len() as u64) as usize;
            let mut bad = wire.clone();
            bad[at] ^= 1 << (state >> 60 & 7);
            if let Ok(out) = unpack(&bad) {
                assert!(out.len() == img.len());
            }
        }
        // a declared image length far over what the wire holds must not be
        // believed past the header: the reserve is the only cost
        let hlen = u32_at(&wire, 4) as usize;
        let mut small = wire.clone();
        small[hlen..hlen + 8].copy_from_slice(&((img.len() - 8) as u64).to_le_bytes());
        assert!(unpack(&small).is_err());
    }

    #[test]
    fn dictionary_padding_stays_zero() {
        for end in ["The", " Community", " Foundation", " for", " Region", " and", " District", "Community Foundation for"] {
            for tail in 0..9 {
                let img = image_tail(300, 128, false, Some((end, tail)));
                let wire = pack_with(&img, Opts { words: Some(64), ..Opts::ALL }).unwrap();
                assert!(unpack(&wire).unwrap() == img, "{end:?} after {tail}");
            }
        }
    }

    #[test]
    fn an_empty_image_packs() {
        let schema = Schema { columns: vec![ColumnDef { name: "t".into(), ty: ColumnType::Utf8, flags: 0 }] };
        let img = Writer::new(schema, vec![], 100, &[None]).finish();
        assert!(unpack(&pack(&img).unwrap()).unwrap() == img);
    }
}
