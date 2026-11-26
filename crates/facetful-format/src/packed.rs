//! The packed form of an image, and its unpacker. Packing (`pack`) is a
//! lossless rewrite for the wire: unpacking gives back the image it was made
//! from, byte for byte, one row group at a time. Everything after the loader
//! (queries, OPFS, split, attach, fingerprints) sees the image it always saw.
//! See docs/file-size.sv for what each encoding saves after brotli and gzip.
//!
//! ```text
//! the image's header, its version set to 2 (PACKED_VERSION)
//! image_len: u64      length of the unpacked image
//! prelude_len: u32
//! prelude:
//!   the dictionary lens table, as the image has it
//!   one: u8           how many word codes are one byte; 0xFF: no word lists
//!   per text column (dictionary columns too), unless one is 0xFF:
//!     its word list: count: varint, the words, each ending in a 0 byte
//!   per dictionary: tag: u8 (D_*), its values in that form
//! row groups, each:
//!   wire_len: u32 (never 0), then that many bytes:
//!   rows: u32, per column null_count: varint, then per column its payload
//!   (text, floats and integers after a tag, T_* F_* I_*; codes and bools as
//!   stored, without padding) and its validity bitmap when it has NULLs
//! 0: u32
//! footer_len: u32, the image's footer as it is, through the closing magic
//! ```
//! An engine from before the packed form reads version 2 in the header and
//! refuses the file (`UnsupportedVersion`). A tag this unpacker does not know
//! is refused the same way a damaged file is, with its own message, so later
//! encodings need no new version.
//!
//! The unpacker trusts nothing: every length is checked against its input,
//! and it never writes past the image length the wire declares. What it
//! produces then goes through the same checks as any image.

use crate::read;
use crate::{align_up, ColumnType, FormatError, MAGIC, MAX_SEGS, PACKED_VERSION, VERSION};

// text payloads
pub(crate) const T_STORED: u8 = 0;
/// values one after another, each ending in a 0 byte
pub(crate) const T_SEP: u8 = 1;
/// identifiers: the text before the trailing digits (ending in 0), how many
/// digits, and the number as a difference from the row before
pub(crate) const T_NUM: u8 = 2;
/// `T_SEP` with the column's common words as byte codes
pub(crate) const T_WORDS: u8 = 3;
// float payloads
pub(crate) const F_STORED: u8 = 0;
/// decimals: a power of ten, then each value times it as an integer
pub(crate) const F_DECIMAL: u8 = 1;
// integer payloads
pub(crate) const I_STORED: u8 = 0;
/// each value minus the one before
pub(crate) const I_DELTA: u8 = 1;
// dictionaries
pub(crate) const D_STORED: u8 = 0;
pub(crate) const D_SEP: u8 = 1;
pub(crate) const D_WORDS: u8 = 2;

/// No word lists in this file.
pub(crate) const NO_WORDS: u8 = 0xFF;
/// The 128 bytes from 0x80 start a word code: the first `one` of them are a
/// code on their own, each of the rest takes a second byte.
pub(crate) const WORD_LEADS: usize = 128;
/// In word-coded text: the next byte stands for itself (bytes of 0x80 and
/// over, and 0 and 1, which would read as a code, the end or this escape).
pub(crate) const ESCAPE: u8 = 1;

pub(crate) const P10: [f64; 19] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18];
/// Most trailing digits `T_NUM` takes as the number.
pub(crate) const NUM_DIGITS: usize = 18;

const BAD: FormatError = FormatError::Corrupt("packed image is damaged");
const UNKNOWN: FormatError = FormatError::Corrupt("packed image uses an encoding this build does not read");

/// Whether `bytes` begin a packed image (true from its first 10 bytes on).
pub fn is_packed(bytes: &[u8]) -> bool {
    bytes.len() >= 10 && bytes[..4] == MAGIC && u16::from_le_bytes([bytes[8], bytes[9]]) == PACKED_VERSION
}

/// A whole packed image, unpacked.
pub fn unpack(wire: &[u8]) -> Result<Vec<u8>, FormatError> {
    let mut un = Unpacker::new();
    let mut out = Vec::new();
    un.push(wire, &mut out)?;
    if !un.done() {
        return Err(FormatError::Truncated);
    }
    Ok(out)
}

pub(crate) fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a [u8] {
        &self.b[self.p..]
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let s = self.p.checked_add(n).and_then(|e| self.b.get(self.p..e)).ok_or(BAD)?;
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    #[inline]
    fn varint(&mut self) -> Result<u64, FormatError> {
        let (mut v, mut shift) = (0u64, 0u32);
        loop {
            let b = *self.b.get(self.p).ok_or(BAD)?;
            self.p += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(BAD);
            }
        }
    }
    fn len(&mut self) -> Result<usize, FormatError> {
        usize::try_from(self.varint()?).map_err(|_| BAD)
    }
}

fn mul(a: usize, b: usize) -> Result<usize, FormatError> {
    a.checked_mul(b).ok_or(BAD)
}

fn padded(n: usize) -> Result<usize, FormatError> {
    n.checked_add(crate::ALIGN - 1).map(|n| n & !(crate::ALIGN - 1)).ok_or(BAD)
}

/// `n` zero bytes on the end of `out`, which may not pass `limit`; where
/// they start.
fn grow(out: &mut Vec<u8>, n: usize, limit: usize) -> Result<usize, FormatError> {
    let at = out.len();
    if n > limit.saturating_sub(at) {
        return Err(BAD);
    }
    out.resize(at + n, 0);
    Ok(at)
}

fn put(out: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), FormatError> {
    if bytes.len() > limit.saturating_sub(out.len()) {
        return Err(BAD);
    }
    out.extend_from_slice(bytes);
    Ok(())
}

/// Zero bytes up to the next multiple of 8.
fn pad(out: &mut Vec<u8>, limit: usize) -> Result<(), FormatError> {
    let n = align_up(out.len()) - out.len();
    grow(out, n, limit).map(|_| ())
}

fn set_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Index of the first 0 byte, eight bytes at a time.
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

/// `n` values from `src`, each ending in a 0 byte, written to `text`
/// without the zeros, and their n + 1 offsets to `offs`. Returns the bytes
/// read and written.
fn unsep(src: &[u8], n: usize, offs: &mut [u8], text: &mut [u8]) -> Result<(usize, usize), FormatError> {
    let (mut s, mut w) = (0usize, 0usize);
    set_u32(offs, 0, 0);
    for i in 0..n {
        let len = find0(&src[s..]).ok_or(BAD)?;
        text.get_mut(w..w + len).ok_or(BAD)?.copy_from_slice(&src[s..s + len]);
        s += len + 1;
        w += len;
        set_u32(offs, (i + 1) * 4, w as u32);
    }
    Ok((s, w))
}

/// Words up to this long are copied as this many bytes (see `unwords`).
const WORD_COPY: usize = 16;

/// One column's word list: word `k` is `bytes[ends[k]..ends[k + 1]]`.
/// `bytes` runs WORD_COPY zeros past the last word.
struct Words {
    ends: Vec<u32>,
    bytes: Vec<u8>,
}

/// `unsep` for values written with word codes.
fn unwords(src: &[u8], n: usize, offs: &mut [u8], text: &mut [u8], words: &Words, one: usize) -> Result<(usize, usize), FormatError> {
    let (mut s, mut w) = (0usize, 0usize);
    set_u32(offs, 0, 0);
    for i in 0..n {
        loop {
            // Away from the ends of both buffers, copies are of a fixed 8 or
            // WORD_COPY bytes, which is a load and a store where a copy of
            // the exact length is a call. What they write past the value's
            // end, later bytes overwrite. Nearly all the time goes here.
            while s + 8 <= src.len() && w + WORD_COPY <= text.len() {
                let b = src[s];
                if b >= 0x80 {
                    let mut k = (b - 0x80) as usize;
                    s += 1;
                    if k >= one {
                        k = one + (k - one) * 256 + src[s] as usize;
                        s += 1;
                    }
                    let (a, e) = (*words.ends.get(k).ok_or(BAD)? as usize, *words.ends.get(k + 1).ok_or(BAD)? as usize);
                    match words.bytes.get(a..a + WORD_COPY) {
                        Some(word) if e - a <= WORD_COPY => text[w..w + WORD_COPY].copy_from_slice(word),
                        _ => text.get_mut(w..w + (e - a)).ok_or(BAD)?.copy_from_slice(&words.bytes[a..e]),
                    }
                    w += e - a;
                } else if b < 2 {
                    break;
                } else {
                    // plain bytes (2 to 0x7F), eight at a time
                    let x = u64::from_le_bytes(src[s..s + 8].try_into().unwrap());
                    let special = (x | (x.wrapping_sub(0x0202020202020202) & !x)) & 0x8080808080808080;
                    let k = if special != 0 { (special.trailing_zeros() / 8) as usize } else { 8 };
                    text[w..w + 8].copy_from_slice(&src[s..s + 8]);
                    s += k;
                    w += k;
                }
            }
            let b = *src.get(s).ok_or(BAD)?;
            s += 1;
            if b >= 0x80 {
                let mut k = (b - 0x80) as usize;
                if k >= one {
                    k = one + (k - one) * 256 + *src.get(s).ok_or(BAD)? as usize;
                    s += 1;
                }
                let (a, e) = (*words.ends.get(k).ok_or(BAD)? as usize, *words.ends.get(k + 1).ok_or(BAD)? as usize);
                text.get_mut(w..w + (e - a)).ok_or(BAD)?.copy_from_slice(&words.bytes[a..e]);
                w += e - a;
            } else if b == 0 {
                break;
            } else {
                let b = if b == ESCAPE {
                    s += 1;
                    *src.get(s - 1).ok_or(BAD)?
                } else {
                    b
                };
                *text.get_mut(w).ok_or(BAD)? = b;
                w += 1;
            }
        }
        set_u32(offs, (i + 1) * 4, w as u32);
    }
    Ok((s, w))
}

/// How a column's payload reads.
#[derive(Clone, Copy)]
enum Col {
    Text,
    Int(usize),
    /// dictionary codes of this width
    Codes(usize),
    Float,
    Bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Start,
    Groups,
    Footer,
    Done,
}

/// Unpacks a packed image as its bytes arrive.
pub struct Unpacker {
    stage: Stage,
    /// wire bytes received and not yet used: the start of a row group (or
    /// of the header and prelude, or of the footer) still short of its end
    buf: Vec<u8>,
    /// the unpacked image's length: output never passes it
    limit: usize,
    row_group_target: u32,
    cols: Vec<Col>,
    one: usize,
    /// per column
    words: Vec<Words>,
}

impl Default for Unpacker {
    fn default() -> Self {
        Self::new()
    }
}

impl Unpacker {
    pub fn new() -> Unpacker {
        Unpacker { stage: Stage::Start, buf: Vec::new(), limit: 0, row_group_target: 0, cols: Vec::new(), one: 0, words: Vec::new() }
    }

    /// The footer has been unpacked: the image is whole.
    pub fn done(&self) -> bool {
        self.stage == Stage::Done
    }

    /// The next bytes of the packed image. What they complete is appended
    /// to `out`, which holds the image unpacked so far and nothing else:
    /// first the header and dictionaries together, then whole row groups,
    /// then the footer.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> Result<(), FormatError> {
        if self.buf.is_empty() {
            let used = self.feed(chunk, out)?;
            self.buf.extend_from_slice(&chunk[used..]);
        } else {
            self.buf.extend_from_slice(chunk);
            let buf = core::mem::take(&mut self.buf);
            let used = self.feed(&buf, out)?;
            self.buf = buf;
            self.buf.drain(..used);
        }
        Ok(())
    }

    /// Unpack every whole unit at the start of `b`; the bytes used.
    fn feed(&mut self, b: &[u8], out: &mut Vec<u8>) -> Result<usize, FormatError> {
        let mut p = 0usize;
        loop {
            let rest = &b[p..];
            let u32_at = |at: usize| u32::from_le_bytes(rest[at..at + 4].try_into().unwrap()) as usize;
            match self.stage {
                Stage::Start => {
                    if rest.len() < 10 {
                        if rest.len() >= 4 && rest[..4] != MAGIC {
                            return Err(FormatError::BadMagic);
                        }
                        break;
                    }
                    if rest[..4] != MAGIC {
                        return Err(FormatError::BadMagic);
                    }
                    let version = u16::from_le_bytes([rest[8], rest[9]]);
                    if version != PACKED_VERSION {
                        return Err(FormatError::UnsupportedVersion(version));
                    }
                    let hlen = u32_at(4);
                    let Some(fixed) = hlen.checked_add(12).filter(|_| hlen >= 10) else { return Err(BAD) };
                    if rest.len() < fixed {
                        break;
                    }
                    let image_len = u64::from_le_bytes(rest[hlen..hlen + 8].try_into().unwrap());
                    let Some(total) = fixed.checked_add(u32_at(hlen + 8)) else { return Err(BAD) };
                    if rest.len() < total {
                        break;
                    }
                    self.limit = usize::try_from(image_len).map_err(|_| BAD)?;
                    self.start(&rest[..hlen], &rest[fixed..total], out)?;
                    p += total;
                    self.stage = Stage::Groups;
                }
                Stage::Groups => {
                    if rest.len() < 4 {
                        break;
                    }
                    let len = u32_at(0);
                    if len == 0 {
                        p += 4;
                        self.stage = Stage::Footer;
                        continue;
                    }
                    let Some(total) = len.checked_add(4) else { return Err(BAD) };
                    if rest.len() < total {
                        break;
                    }
                    self.group(&rest[4..total], out)?;
                    p += total;
                }
                Stage::Footer => {
                    if rest.len() < 4 {
                        break;
                    }
                    let Some(total) = u32_at(0).checked_add(4) else { return Err(BAD) };
                    if rest.len() < total {
                        break;
                    }
                    put(out, &rest[4..total], self.limit)?;
                    if out.len() != self.limit {
                        return Err(BAD);
                    }
                    p += total;
                    self.stage = Stage::Done;
                }
                Stage::Done => {
                    if !rest.is_empty() {
                        return Err(BAD);
                    }
                    break;
                }
            }
        }
        Ok(p)
    }

    /// The header and the prelude: the image up to its first row group.
    fn start(&mut self, header: &[u8], prelude: &[u8], out: &mut Vec<u8>) -> Result<(), FormatError> {
        if !out.is_empty() {
            return Err(FormatError::Io("unpacking into a buffer that is not empty"));
        }
        let limit = self.limit;
        // the declared length is not to be trusted with an abort
        out.try_reserve_exact(limit).map_err(|_| FormatError::Io("not enough memory for the image"))?;
        put(out, header, limit)?;
        out[8..10].copy_from_slice(&VERSION.to_le_bytes());
        let (schema, _, _, target, hlen, later) = read::parse_header_len(out)?;
        if hlen as usize != header.len() {
            return Err(BAD);
        }
        // the image's header gives its length too (images written before it
        // did have none): the two must agree, or the loader reserves for one
        // and the unpacker fills to the other
        if later.image_len.is_some_and(|n| n != limit as u64) {
            return Err(BAD);
        }
        self.row_group_target = target;
        let mut r = Reader { b: prelude, p: 0 };
        let dict_cols: Vec<usize> = (0..schema.columns.len()).filter(|&c| schema.columns[c].is_dict()).collect();
        let table = out.len();
        put(out, r.take(read::dict_table_len(&schema))?, limit)?;
        self.one = match r.u8()? {
            NO_WORDS => NO_WORDS as usize,
            one if one as usize <= WORD_LEADS => one as usize,
            _ => return Err(UNKNOWN),
        };
        for c in &schema.columns {
            let mut list = Words { ends: vec![0], bytes: Vec::new() };
            if self.one != NO_WORDS as usize && c.ty == ColumnType::Utf8 {
                for _ in 0..r.varint()? {
                    let len = find0(r.rest()).ok_or(BAD)?;
                    list.bytes.extend_from_slice(&r.take(len + 1)?[..len]);
                    list.ends.push(u32::try_from(list.bytes.len()).map_err(|_| BAD)?);
                }
            }
            list.bytes.extend_from_slice(&[0; WORD_COPY]);
            self.words.push(list);
        }
        for (d, &ci) in dict_cols.iter().enumerate() {
            let len_at = |i: usize| u32::from_le_bytes(out[table + d * 8 + i * 4..][..4].try_into().unwrap()) as usize;
            let (ol, bl) = (len_at(0), len_at(1));
            let tag = r.u8()?;
            if tag == D_STORED {
                put(out, r.take(ol)?, limit)?;
                put(out, r.take(bl)?, limit)?;
                continue;
            }
            let n = r.len()?;
            if mul(n.checked_add(1).ok_or(BAD)?, 4)? > ol {
                return Err(BAD);
            }
            let offs_at = grow(out, ol, limit)?;
            let bytes_at = grow(out, bl, limit)?;
            let (head, text) = out.split_at_mut(bytes_at);
            let offs = &mut head[offs_at..];
            let (used, written) = match tag {
                D_SEP => unsep(r.rest(), n, offs, text)?,
                D_WORDS if self.one != NO_WORDS as usize => unwords(r.rest(), n, offs, text, &self.words[ci], self.one)?,
                _ => return Err(UNKNOWN),
            };
            // the segment's padding, which `unwords` may have copied past
            // the last value into
            text.get_mut(written..).ok_or(BAD)?.fill(0);
            r.p += used;
        }
        if r.p != prelude.len() {
            return Err(BAD);
        }
        self.cols = schema
            .columns
            .iter()
            .map(|c| match (c.ty, c.is_dict()) {
                (ColumnType::Utf8, true) => Col::Codes(c.code_width()),
                (ColumnType::Utf8, false) => Col::Text,
                (ColumnType::Bool, _) => Col::Bool,
                (ColumnType::Float64, _) => Col::Float,
                (ty, _) => Col::Int(ty.fixed_width().expect("fixed width")),
            })
            .collect();
        Ok(())
    }

    /// One row group, appended to `out` as the image holds it: the group
    /// header, then each column's padded segments.
    fn group(&self, wire: &[u8], out: &mut Vec<u8>) -> Result<(), FormatError> {
        let limit = self.limit;
        let mut r = Reader { b: wire, p: 0 };
        let rows = r.u32()?;
        if rows == 0 || rows > self.row_group_target {
            return Err(BAD);
        }
        let n = rows as usize;
        let ncols = self.cols.len();
        let per_col = 4 + 4 * MAX_SEGS;
        let head = grow(out, align_up(4 + ncols * per_col), limit)?;
        set_u32(out, head, rows);
        let mut nulls = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let nc = u32::try_from(r.varint()?).ok().filter(|&nc| nc <= rows).ok_or(BAD)?;
            nulls.push(nc);
        }
        let bits = n.div_ceil(8);
        for (ci, col) in self.cols.iter().enumerate() {
            let mut lens = [0u32; MAX_SEGS];
            let start = out.len();
            match *col {
                Col::Codes(w) => put(out, r.take(mul(n, w)?)?, limit)?,
                Col::Bool => put(out, r.take(bits)?, limit)?,
                Col::Int(w) => match r.u8()? {
                    I_STORED => put(out, r.take(mul(n, w)?)?, limit)?,
                    I_DELTA => {
                        let src = r.take(mul(n, w)?)?;
                        let at = grow(out, src.len(), limit)?;
                        let mut prev = 0u64;
                        for (d, o) in src.chunks_exact(w).zip(out[at..].chunks_exact_mut(w)) {
                            let mut b = [0u8; 8];
                            b[..w].copy_from_slice(d);
                            prev = prev.wrapping_add(u64::from_le_bytes(b));
                            o.copy_from_slice(&prev.to_le_bytes()[..w]);
                        }
                    }
                    _ => return Err(UNKNOWN),
                },
                Col::Float => match r.u8()? {
                    F_STORED => put(out, r.take(mul(n, 8)?)?, limit)?,
                    F_DECIMAL => {
                        let e = *P10.get(r.u8()? as usize).ok_or(BAD)?;
                        let at = grow(out, mul(n, 8)?, limit)?;
                        for o in out[at..].chunks_exact_mut(8) {
                            let x = unzigzag(r.varint()?) as f64 / e;
                            o.copy_from_slice(&x.to_le_bytes());
                        }
                        // the values that are not such decimals, as they are
                        let mut i = 0usize;
                        for _ in 0..r.varint()? {
                            i = i.checked_add(r.len()?).filter(|&i| i < n).ok_or(BAD)?;
                            out[at + i * 8..at + i * 8 + 8].copy_from_slice(r.take(8)?);
                        }
                    }
                    _ => return Err(UNKNOWN),
                },
                Col::Text => {
                    let tag = r.u8()?;
                    let used = r.len()?;
                    let offs_len = mul(n.checked_add(1).ok_or(BAD)?, 4)?;
                    grow(out, padded(offs_len)?, limit)?;
                    lens[0] = (out.len() - start) as u32;
                    let bytes_at = grow(out, used, limit)?;
                    let (head, text) = out.split_at_mut(bytes_at);
                    let offs = &mut head[start..start + offs_len];
                    match tag {
                        T_STORED => {
                            offs.copy_from_slice(r.take(offs_len)?);
                            text.copy_from_slice(r.take(used)?);
                        }
                        T_SEP => {
                            let src = r.take(used.checked_add(n).ok_or(BAD)?)?;
                            if unsep(src, n, offs, text)? != (src.len(), used) {
                                return Err(BAD);
                            }
                        }
                        T_WORDS if self.one != NO_WORDS as usize => {
                            let (read, written) = unwords(r.rest(), n, offs, text, &self.words[ci], self.one)?;
                            if written != used {
                                return Err(BAD);
                            }
                            r.p += read;
                        }
                        T_NUM => {
                            // n heads ending in 0, then n digit counts, then
                            // the numbers of the values that have digits
                            let heads = r.rest();
                            let mut end = 0usize;
                            for _ in 0..n {
                                end += find0(&heads[end..]).ok_or(BAD)? + 1;
                            }
                            r.p += end;
                            let widths = r.take(n)?;
                            let (mut hp, mut w, mut prev) = (0usize, 0usize, 0i64);
                            for (i, &width) in widths.iter().enumerate() {
                                let hl = find0(&heads[hp..]).ok_or(BAD)?;
                                text.get_mut(w..w + hl).ok_or(BAD)?.copy_from_slice(&heads[hp..hp + hl]);
                                hp += hl + 1;
                                w += hl;
                                let width = width as usize;
                                if width > 0 {
                                    prev = prev.wrapping_add(unzigzag(r.varint()?));
                                    let mut digits = [b'0'; 20];
                                    let (mut x, mut k) = (prev as u64, 20);
                                    while x > 0 {
                                        k -= 1;
                                        digits[k] = b'0' + (x % 10) as u8;
                                        x /= 10;
                                    }
                                    let d = digits.get(20usize.wrapping_sub(width)..).ok_or(BAD)?;
                                    text.get_mut(w..w + width).ok_or(BAD)?.copy_from_slice(d);
                                    w += width;
                                }
                                set_u32(offs, (i + 1) * 4, w as u32);
                            }
                            if w != used {
                                return Err(BAD);
                            }
                        }
                        _ => return Err(UNKNOWN),
                    }
                    pad(out, limit)?;
                    lens[1] = (out.len() - bytes_at) as u32;
                }
            }
            if !matches!(col, Col::Text) {
                pad(out, limit)?;
                lens[0] = (out.len() - start) as u32;
            }
            if nulls[ci] > 0 {
                let at = out.len();
                put(out, r.take(bits)?, limit)?;
                pad(out, limit)?;
                lens[2] = (out.len() - at) as u32;
            }
            let h = head + 4 + ci * per_col;
            set_u32(out, h, nulls[ci]);
            for (s, &l) in lens.iter().enumerate() {
                set_u32(out, h + 4 + s * 4, l);
            }
        }
        if r.p != wire.len() {
            return Err(BAD);
        }
        Ok(())
    }
}
