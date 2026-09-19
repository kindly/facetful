//! One-pass streaming writer: header, then the file-level dictionary block
//! (dictionaries are known before writing begins — the CLI computes them during
//! inference), then self-framing row groups as they arrive, footer last.

use crate::*;

/// One dictionary's payload: (n+1) offsets + UTF-8 blob, shared by every group.
pub struct DictData {
    pub offsets: Vec<u32>,
    pub bytes: Vec<u8>,
}

/// Column data for ONE row group, in schema order.
pub enum SegmentData<'a> {
    /// Fixed-width values as raw LE bytes (len = rows * width). Covers
    /// Int8/16/32/64, Float64, Date, Timestamp.
    Fixed(&'a [u8]),
    /// Bit-packed bools, ceil(rows/8) bytes.
    Bool(&'a [u8]),
    /// Plain strings: (n+1) u32 offsets + UTF-8 blob.
    Utf8 { offsets: &'a [u32], bytes: &'a [u8] },
    /// Dictionary codes, u8 (column flag CODES_U8 must be set).
    Codes8(&'a [u8]),
    /// Dictionary codes, u16.
    Codes16(&'a [u16]),
}

pub struct ColumnChunk<'a> {
    pub data: SegmentData<'a>,
    /// Validity bitmap (bit i set = row i present), ceil(rows/8) bytes;
    /// None = no nulls in this chunk. Stored in segment slot 2. Null rows'
    /// data values are placeholders (zeros) and must be masked by readers.
    pub validity: Option<&'a [u8]>,
    pub null_count: u32,
}

pub struct Writer {
    schema: Schema,
    sorted_by: Vec<SortKey>,
    row_group_target: u32,
    /// bytes not yet handed out by `take_output`
    buf: Vec<u8>,
    /// bytes already handed out — `buf` starts at this file position
    flushed: u64,
    groups: Vec<GroupMeta>,
    total_rows: u64,
}

impl Writer {
    /// `dicts` must align with the schema: `Some(DictData)` exactly for columns
    /// flagged DICTIONARY.
    pub fn new(
        schema: Schema,
        sorted_by: Vec<SortKey>,
        row_group_target: u32,
        dicts: &[Option<DictData>],
    ) -> Self {
        assert_eq!(dicts.len(), schema.columns.len());
        for (c, d) in schema.columns.iter().zip(dicts) {
            assert_eq!(c.is_dict(), d.is_some(), "dict presence must match DICTIONARY flag");
        }
        let mut w = Self {
            schema,
            sorted_by,
            row_group_target,
            buf: Vec::new(),
            flushed: 0,
            groups: Vec::new(),
            total_rows: 0,
        };
        w.write_header();
        w.write_dict_block(dicts);
        w
    }

    fn put_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn pad(&mut self) {
        while self.buf.len() % ALIGN != 0 {
            self.buf.push(0);
        }
    }

    fn write_header(&mut self) {
        self.buf.extend_from_slice(&MAGIC);
        let len_pos = self.buf.len();
        self.put_u32(0); // header_len placeholder
        self.put_u16(VERSION);
        self.put_u16(0); // file flags, reserved
        let target = self.row_group_target;
        self.put_u32(target);
        let cols = self.schema.columns.clone();
        self.put_u16(cols.len() as u16);
        for c in &cols {
            self.put_u16(c.name.len() as u16);
            self.buf.extend_from_slice(c.name.as_bytes());
            self.buf.push(c.ty as u8);
            self.put_u16(c.flags);
        }
        let sorted = self.sorted_by.clone();
        self.buf.push(sorted.len() as u8);
        for s in &sorted {
            self.put_u16(s.column);
            self.buf.push(s.descending as u8);
        }
        self.pad();
        let hlen = self.buf.len() as u32;
        self.buf[len_pos..len_pos + 4].copy_from_slice(&hlen.to_le_bytes());
    }

    /// Dictionary block: a lens table (offsets_len, bytes_len per dict column,
    /// in schema order), then the padded payloads. Self-framing for streaming
    /// readers; random-access readers recompute the same offsets from the lens.
    fn write_dict_block(&mut self, dicts: &[Option<DictData>]) {
        let present: Vec<&DictData> = dicts.iter().flatten().collect();
        if present.is_empty() {
            return;
        }
        for d in &present {
            self.put_u32(align_up(d.offsets.len() * 4) as u32);
            self.put_u32(align_up(d.bytes.len()) as u32);
        }
        self.pad();
        for d in &present {
            for o in &d.offsets {
                self.put_u32(*o);
            }
            self.pad();
            self.buf.extend_from_slice(&d.bytes);
            self.pad();
        }
    }

    /// Append one row group. `cols` must match the schema order and all describe
    /// `row_count` rows.
    pub fn write_group(&mut self, row_count: u32, cols: &[ColumnChunk<'_>]) {
        assert_eq!(cols.len(), self.schema.columns.len(), "column count mismatch");
        assert!(row_count > 0, "empty row group");

        let group_offset = self.flushed + self.buf.len() as u64;

        let mut metas: Vec<ColMeta> = Vec::with_capacity(cols.len());
        let mut payloads: Vec<[Option<&[u8]>; MAX_SEGS]> = Vec::with_capacity(cols.len());
        let mut owned: Vec<Vec<u8>> = Vec::new();

        for (ci, chunk) in cols.iter().enumerate() {
            let def = &self.schema.columns[ci];
            if let Some(v) = chunk.validity {
                assert_eq!(v.len(), (row_count as usize + 7) / 8, "col {ci} validity length");
                assert!(chunk.null_count > 0, "validity present but null_count = 0");
            } else {
                assert_eq!(chunk.null_count, 0, "null_count > 0 requires a validity bitmap");
            }
            let mut segs: [Option<&[u8]>; MAX_SEGS] = [None, None, None];
            segs[2] = chunk.validity; // slot 2 is validity for every column kind
            let stats;
            match (&chunk.data, def.ty, def.is_dict()) {
                (SegmentData::Fixed(bytes), ty, false) => {
                    let w = ty.fixed_width().expect("fixed type");
                    assert_eq!(bytes.len(), row_count as usize * w, "col {ci} length");
                    segs[0] = Some(bytes);
                    stats = compute_numeric_stats(bytes, ty, chunk.validity);
                }
                (SegmentData::Bool(bits), ColumnType::Bool, false) => {
                    assert_eq!(bits.len(), (row_count as usize + 7) / 8);
                    segs[0] = Some(bits);
                    stats = Stats::None;
                }
                (SegmentData::Utf8 { offsets, bytes }, ColumnType::Utf8, false) => {
                    assert_eq!(offsets.len(), row_count as usize + 1);
                    owned.push(u32s_as_bytes(offsets));
                    segs[1] = Some(bytes);
                    stats = Stats::None;
                }
                (SegmentData::Codes8(codes), ColumnType::Utf8, true) => {
                    assert_eq!(def.code_width(), 1, "col {ci}: CODES_U8 flag mismatch");
                    assert_eq!(codes.len(), row_count as usize);
                    segs[0] = Some(codes);
                    stats = Stats::None;
                }
                (SegmentData::Codes16(codes), ColumnType::Utf8, true) => {
                    assert_eq!(def.code_width(), 2, "col {ci}: expected u16 codes");
                    assert_eq!(codes.len(), row_count as usize);
                    owned.push(u16s_as_bytes(codes));
                    stats = Stats::None;
                }
                _ => panic!("column {ci}: data does not match schema type/flags"),
            }
            payloads.push(segs);
            metas.push(ColMeta { null_count: chunk.null_count, seg_lens: [0; MAX_SEGS], stats });
        }

        // Resolve owned re-encoded segments (utf8 offsets -> slot 0, u16 codes -> slot 0).
        let mut oi = 0;
        for (ci, chunk) in cols.iter().enumerate() {
            match &chunk.data {
                SegmentData::Utf8 { .. } | SegmentData::Codes16(_) => {
                    payloads[ci][0] = Some(&owned[oi]);
                    oi += 1;
                }
                _ => {}
            }
        }

        // Group header.
        self.put_u32(row_count);
        for (ci, meta) in metas.iter_mut().enumerate() {
            self.put_u32(meta.null_count);
            for s in 0..MAX_SEGS {
                let len = payloads[ci][s].map_or(0, |p| align_up(p.len()) as u32);
                meta.seg_lens[s] = len;
                self.put_u32(len);
            }
        }
        self.pad();

        for segs in &payloads {
            for seg in segs.iter().flatten() {
                self.buf.extend_from_slice(seg);
                self.pad();
            }
        }

        self.total_rows += row_count as u64;
        self.groups.push(GroupMeta { offset: group_offset, row_count, cols: metas });
    }

    /// Hand out the bytes written so far (pull-model streaming: a caller
    /// appends them to a file or drains them across the wasm boundary after
    /// each group). Callers that never take keep getting the whole file
    /// from `finish`.
    pub fn take_output(&mut self) -> Vec<u8> {
        self.flushed += self.buf.len() as u64;
        core::mem::take(&mut self.buf)
    }

    pub fn total_rows(&self) -> u64 {
        self.total_rows
    }

    /// Write the footer and return the finished file bytes (the remainder,
    /// after any `take_output`).
    pub fn finish(mut self) -> Vec<u8> {
        self.pad();
        let footer_start = self.buf.len();
        let total = self.total_rows;
        self.put_u64(total);
        let groups = core::mem::take(&mut self.groups);
        self.put_u32(groups.len() as u32);
        for g in &groups {
            self.put_u64(g.offset);
            self.put_u32(g.row_count);
            for c in &g.cols {
                self.put_u32(c.null_count);
                for s in 0..MAX_SEGS {
                    self.put_u32(c.seg_lens[s]);
                }
                match c.stats {
                    Stats::None => {
                        self.buf.push(0);
                        self.put_u64(0);
                        self.put_u64(0);
                    }
                    Stats::Int { min, max } => {
                        self.buf.push(1);
                        self.put_u64(min as u64);
                        self.put_u64(max as u64);
                    }
                    Stats::Float { min, max } => {
                        self.buf.push(2);
                        self.put_u64(min.to_bits());
                        self.put_u64(max.to_bits());
                    }
                }
            }
        }
        let footer_len = (self.buf.len() - footer_start) as u32;
        self.put_u32(footer_len);
        self.buf.extend_from_slice(&MAGIC);
        self.buf
    }
}

fn u32s_as_bytes(v: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn u16s_as_bytes(v: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 2);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn compute_numeric_stats(bytes: &[u8], ty: ColumnType, validity: Option<&[u8]>) -> Stats {
    let valid = |i: usize| validity.map_or(true, |v| v[i / 8] & (1 << (i % 8)) != 0);
    match ty {
        ColumnType::Int8 => int_stats(bytes, 1, valid, |b| b[0] as i8 as i64),
        ColumnType::Int16 => int_stats(bytes, 2, valid, |b| i16::from_le_bytes([b[0], b[1]]) as i64),
        ColumnType::Int32 | ColumnType::Date => {
            int_stats(bytes, 4, valid, |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64)
        }
        ColumnType::Int64 | ColumnType::Timestamp => int_stats(bytes, 8, valid, |b| {
            i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        }),
        ColumnType::Float64 => {
            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            for (i, c) in bytes.chunks_exact(8).enumerate() {
                if !valid(i) {
                    continue;
                }
                let v = f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
                if v < min {
                    min = v;
                }
                if v > max {
                    max = v;
                }
            }
            Stats::Float { min, max }
        }
        _ => Stats::None,
    }
}

fn int_stats(
    bytes: &[u8],
    w: usize,
    valid: impl Fn(usize) -> bool,
    f: impl Fn(&[u8]) -> i64,
) -> Stats {
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for (i, c) in bytes.chunks_exact(w).enumerate() {
        if !valid(i) {
            continue;
        }
        let v = f(c);
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    Stats::Int { min, max }
}
