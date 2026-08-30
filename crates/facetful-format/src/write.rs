//! One-pass streaming writer: header first, then self-framing row groups as they
//! arrive, footer last. The caller chunks rows into groups (the CLI does this).

use crate::*;

/// Column data for ONE row group, in schema order. Values are already in their
/// final physical representation — the writer only frames, pads, and takes stats.
pub enum SegmentData<'a> {
    /// Fixed-width values as raw LE bytes (len = rows * width). Covers
    /// Int8/16/32/64, Float64, Date, Timestamp.
    Fixed(&'a [u8]),
    /// Bit-packed bools, ceil(rows/8) bytes.
    Bool(&'a [u8]),
    /// Plain strings: (n+1) u32 offsets + UTF-8 blob.
    Utf8 { offsets: &'a [u32], bytes: &'a [u8] },
    /// Dictionary-encoded strings: u16 codes per row + dict (offsets + blob).
    Dict {
        codes: &'a [u16],
        dict_offsets: &'a [u32],
        dict_bytes: &'a [u8],
    },
}

pub struct ColumnChunk<'a> {
    pub data: SegmentData<'a>,
    /// Validity bitmap (1 = present), ceil(rows/8) bytes; None = no nulls.
    /// NOTE: v1 stores validity inline ahead of data in the FIRST segment slot
    /// only when present — for the spike we keep nulls out of scope and require None.
    pub validity: Option<&'a [u8]>,
    pub null_count: u32,
}

pub struct Writer {
    schema: Schema,
    sorted_by: Vec<SortKey>,
    row_group_target: u32,
    buf: Vec<u8>,
    groups: Vec<GroupMeta>,
    total_rows: u64,
}

impl Writer {
    pub fn new(schema: Schema, sorted_by: Vec<SortKey>, row_group_target: u32) -> Self {
        let mut w = Self {
            schema,
            sorted_by,
            row_group_target,
            buf: Vec::new(),
            groups: Vec::new(),
            total_rows: 0,
        };
        w.write_header();
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
        self.put_u32(0); // header_len placeholder (bytes from magic through padding)
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

    /// Append one row group. `cols` must match the schema order and all describe
    /// `row_count` rows.
    pub fn write_group(&mut self, row_count: u32, cols: &[ColumnChunk<'_>]) {
        assert_eq!(cols.len(), self.schema.columns.len(), "column count mismatch");
        assert!(row_count > 0, "empty row group");

        let group_offset = self.buf.len() as u64;

        // Gather per-column segment payloads (unpadded) and stats first.
        let mut metas: Vec<ColMeta> = Vec::with_capacity(cols.len());
        let mut payloads: Vec<[Option<&[u8]>; MAX_SEGS]> = Vec::with_capacity(cols.len());
        let mut owned_offsets: Vec<Vec<u8>> = Vec::new(); // u32/u16 slices re-encoded as bytes

        for (ci, chunk) in cols.iter().enumerate() {
            let def = &self.schema.columns[ci];
            assert!(chunk.validity.is_none(), "nulls not implemented in spike writer");
            let mut segs: [Option<&[u8]>; MAX_SEGS] = [None, None, None];
            let stats;
            match (&chunk.data, def.ty, def.is_dict()) {
                (SegmentData::Fixed(bytes), ty, false) => {
                    let w = ty.fixed_width().expect("fixed type");
                    assert_eq!(bytes.len(), row_count as usize * w, "col {ci} length");
                    segs[0] = Some(bytes);
                    stats = compute_numeric_stats(bytes, ty);
                }
                (SegmentData::Bool(bits), ColumnType::Bool, false) => {
                    assert_eq!(bits.len(), (row_count as usize + 7) / 8);
                    segs[0] = Some(bits);
                    stats = Stats::None;
                }
                (SegmentData::Utf8 { offsets, bytes }, ColumnType::Utf8, false) => {
                    assert_eq!(offsets.len(), row_count as usize + 1);
                    let ob = u32s_as_bytes(offsets);
                    owned_offsets.push(ob);
                    segs[0] = None; // fixed up below from owned_offsets
                    segs[1] = Some(bytes);
                    stats = Stats::None;
                }
                (SegmentData::Dict { codes, dict_offsets, dict_bytes }, ColumnType::Utf8, true) => {
                    assert_eq!(codes.len(), row_count as usize);
                    owned_offsets.push(u16s_as_bytes(codes));
                    owned_offsets.push(u32s_as_bytes(dict_offsets));
                    segs[2] = Some(dict_bytes);
                    stats = Stats::None;
                }
                _ => panic!("column {ci}: data does not match schema type/flags"),
            }
            payloads.push(segs);
            metas.push(ColMeta {
                null_count: chunk.null_count,
                seg_lens: [0; MAX_SEGS],
                stats,
            });
        }

        // Resolve owned (re-encoded) segments into the payload table.
        // owned_offsets is filled in schema order: utf8 -> 1 entry (slot 0),
        // dict -> 2 entries (slots 0 and 1).
        let mut oi = 0;
        for (ci, chunk) in cols.iter().enumerate() {
            match &chunk.data {
                SegmentData::Utf8 { .. } => {
                    payloads[ci][0] = Some(&owned_offsets[oi]);
                    oi += 1;
                }
                SegmentData::Dict { .. } => {
                    payloads[ci][0] = Some(&owned_offsets[oi]);
                    payloads[ci][1] = Some(&owned_offsets[oi + 1]);
                    oi += 2;
                }
                _ => {}
            }
        }

        // Group header: row_count, then per column: null_count + MAX_SEGS padded lengths.
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

        // Segments, each padded to ALIGN.
        for segs in &payloads {
            for seg in segs.iter().flatten() {
                self.buf.extend_from_slice(seg);
                self.pad();
            }
        }

        self.total_rows += row_count as u64;
        self.groups.push(GroupMeta {
            offset: group_offset,
            row_count,
            cols: metas,
        });
    }

    /// Write the footer and return the finished file bytes.
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

fn compute_numeric_stats(bytes: &[u8], ty: ColumnType) -> Stats {
    match ty {
        ColumnType::Int8 => int_stats(bytes, 1, |b| b[0] as i8 as i64),
        ColumnType::Int16 => int_stats(bytes, 2, |b| i16::from_le_bytes([b[0], b[1]]) as i64),
        ColumnType::Int32 | ColumnType::Date => {
            int_stats(bytes, 4, |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64)
        }
        ColumnType::Int64 | ColumnType::Timestamp => int_stats(bytes, 8, |b| {
            i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        }),
        ColumnType::Float64 => {
            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            for c in bytes.chunks_exact(8) {
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

fn int_stats(bytes: &[u8], w: usize, f: impl Fn(&[u8]) -> i64) -> Stats {
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for c in bytes.chunks_exact(w) {
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
