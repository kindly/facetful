//! An image held in memory, with typed views of its segments.

use facetful_format::read;
use facetful_format::{Catalog, ColumnDef, ColumnType};

pub struct Img {
    pub bytes: Vec<u8>,
    pub cat: Catalog,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Text,
    Dict,
    Int(usize),
    Float,
    Bool,
}

impl Img {
    pub fn open(path: &str) -> Img {
        let bytes = std::fs::read(path).expect("read image");
        Img::from_bytes(bytes)
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Img {
        let cat = read::open(&bytes).expect("open image");
        Img { bytes, cat }
    }

    pub fn ncols(&self) -> usize {
        self.cat.schema.columns.len()
    }
    pub fn ngroups(&self) -> usize {
        self.cat.groups.len()
    }
    pub fn def(&self, col: usize) -> &ColumnDef {
        &self.cat.schema.columns[col]
    }
    pub fn rows(&self, g: usize) -> usize {
        self.cat.groups[g].row_count as usize
    }

    pub fn kind(&self, col: usize) -> Kind {
        let d = self.def(col);
        match d.ty {
            ColumnType::Utf8 if d.is_dict() => Kind::Dict,
            ColumnType::Utf8 => Kind::Text,
            ColumnType::Float64 => Kind::Float,
            ColumnType::Bool => Kind::Bool,
            t => Kind::Int(t.fixed_width().unwrap()),
        }
    }

    /// A segment's bytes as stored, padding included.
    pub fn seg(&self, g: usize, col: usize, s: usize) -> &[u8] {
        let gm = &self.cat.groups[g];
        let len = gm.cols[col].seg_lens[s] as usize;
        let off = read::segment_offset(&self.cat, gm, col, s) as usize;
        &self.bytes[off..off + len]
    }

    /// Fixed-width values or codes, without padding.
    pub fn fixed(&self, g: usize, col: usize) -> &[u8] {
        let w = match self.kind(col) {
            Kind::Dict => self.def(col).code_width(),
            Kind::Int(w) => w,
            Kind::Float => 8,
            k => panic!("not fixed: {k:?}"),
        };
        &self.seg(g, col, 0)[..self.rows(g) * w]
    }

    /// Dictionary codes of a group, widened.
    pub fn codes(&self, g: usize, col: usize) -> Vec<u32> {
        let b = self.fixed(g, col);
        if self.def(col).code_width() == 1 {
            b.iter().map(|&x| x as u32).collect()
        } else {
            b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32).collect()
        }
    }

    /// Integer values of a group, widened.
    pub fn ints(&self, g: usize, col: usize) -> Vec<i64> {
        let b = self.fixed(g, col);
        match self.kind(col) {
            Kind::Int(1) => b.iter().map(|&x| x as i8 as i64).collect(),
            Kind::Int(2) => b.chunks_exact(2).map(|c| i16::from_le_bytes(c.try_into().unwrap()) as i64).collect(),
            Kind::Int(4) => b.chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap()) as i64).collect(),
            Kind::Int(8) => b.chunks_exact(8).map(|c| i64::from_le_bytes(c.try_into().unwrap())).collect(),
            k => panic!("not int: {k:?}"),
        }
    }

    pub fn floats(&self, g: usize, col: usize) -> Vec<f64> {
        self.fixed(g, col).chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect()
    }

    /// Offsets (n + 1) and text bytes of a plain text column's group.
    pub fn text(&self, g: usize, col: usize) -> (Vec<u32>, &[u8]) {
        let n = self.rows(g);
        let offs: Vec<u32> = self.seg(g, col, 0)[..(n + 1) * 4].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let bytes = &self.seg(g, col, 1)[..offs[n] as usize];
        (offs, bytes)
    }

    /// The validity bitmap of a group, or None without NULLs.
    pub fn validity(&self, g: usize, col: usize) -> Option<&[u8]> {
        let gm = &self.cat.groups[g];
        (gm.cols[col].null_count > 0).then(|| &self.seg(g, col, 2)[..self.rows(g).div_ceil(8)])
    }

    /// A dictionary's offsets (count + 1) and bytes.
    pub fn dict(&self, col: usize) -> (Vec<u32>, &[u8]) {
        let loc = self.cat.dicts[col].expect("dictionary column");
        let o = &self.bytes[loc.offsets_off as usize..(loc.offsets_off + loc.offsets_len as u64) as usize];
        let mut offs: Vec<u32> = o.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        // padded with zeros: the real offsets end where they stop rising
        let mut n = 1;
        while n < offs.len() && offs[n] >= offs[n - 1] && !(offs[n] == 0 && n > 1 && offs[n - 1] > 0) {
            n += 1;
        }
        // a trailing run of equal offsets could be padding only when it is zero
        offs.truncate(n);
        let bytes = &self.bytes[loc.bytes_off as usize..loc.bytes_off as usize + *offs.last().unwrap() as usize];
        (offs, bytes)
    }

    /// The dictionary block's stored bytes for one column (offsets, bytes; padded).
    pub fn dict_stored(&self, col: usize) -> &[u8] {
        let loc = self.cat.dicts[col].expect("dictionary column");
        &self.bytes[loc.offsets_off as usize..(loc.bytes_off + loc.bytes_len as u64) as usize]
    }
}
