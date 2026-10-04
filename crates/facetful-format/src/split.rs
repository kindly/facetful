//! Splitting an image into two with the same row groups, so a page can open
//! the first and attach the second when it arrives (the engine's
//! `Table::attach`). Segments are copied as they are; only the headers,
//! dictionaries and footers are written anew.

use crate::read::{self, ReadAt};
use crate::write::{ColumnChunk, DictData, SegmentData, Writer};
use crate::*;

/// Split `src` into two images: the columns in `second` (indices into its
/// schema) go to the second image, the rest stay in the first, each in schema
/// order. Both have the same row groups and carry `split_id` (non-zero),
/// which attaching checks. The sort order stays with the first image when all
/// its columns do.
pub fn split(src: &impl ReadAt, second: &[usize], split_id: u64) -> Result<(Vec<u8>, Vec<u8>), FormatError> {
    let cat = read::open(src)?;
    let ncols = cat.schema.columns.len();
    if split_id == 0 {
        return Err(FormatError::Corrupt("split id must not be 0"));
    }
    if second.is_empty() || second.len() >= ncols || second.iter().any(|&c| c >= ncols) {
        return Err(FormatError::Corrupt("split: the second image needs some columns, not all"));
    }
    let parts: [Vec<usize>; 2] = [
        (0..ncols).filter(|c| !second.contains(c)).collect(),
        (0..ncols).filter(|c| second.contains(c)).collect(),
    ];
    let mut writers = Vec::with_capacity(2);
    for (p, cols) in parts.iter().enumerate() {
        let schema = Schema { columns: cols.iter().map(|&c| cat.schema.columns[c].clone()).collect() };
        let mut dicts = Vec::with_capacity(cols.len());
        for &c in cols {
            dicts.push(if cat.schema.columns[c].is_dict() {
                let values = read::read_dictionary(src, &cat, c)?;
                let (offsets, bytes) = crate::compile::utf8_offsets(&values);
                Some(DictData { offsets, bytes })
            } else {
                None
            });
        }
        let local = |c: u16| cols.iter().position(|&x| x == c as usize).map(|i| i as u16);
        let sorted_by = if p == 0 && cat.sorted_by.iter().all(|k| local(k.column).is_some()) {
            cat.sorted_by.iter().map(|k| SortKey { column: local(k.column).unwrap(), descending: k.descending }).collect()
        } else {
            Vec::new()
        };
        let mut w = Writer::new(schema, sorted_by, cat.row_group_target, &dicts);
        w.set_split_id(split_id);
        writers.push(w);
    }
    for g in 0..cat.groups.len() {
        let rows = cat.groups[g].row_count as usize;
        for (cols, w) in parts.iter().zip(writers.iter_mut()) {
            let segs: Vec<Owned> = cols.iter().map(|&c| Owned::read(src, &cat, g, c)).collect::<Result<_, _>>()?;
            let chunks: Vec<ColumnChunk> = cols.iter().zip(&segs).map(|(&c, s)| s.chunk(&cat.schema.columns[c], rows)).collect();
            w.write_group(rows as u32, &chunks);
        }
    }
    let [a, b]: [Vec<u8>; 2] = writers.into_iter().map(Writer::finish).collect::<Vec<_>>().try_into().unwrap();
    Ok((a, b))
}

/// One column's segments in one row group, as stored (padded).
struct Owned {
    segs: [Vec<u8>; MAX_SEGS],
    null_count: u32,
    /// plain text offsets, decoded
    offsets: Vec<u32>,
    /// u16 dictionary codes, decoded
    codes: Vec<u16>,
}

impl Owned {
    fn read(src: &impl ReadAt, cat: &Catalog, g: usize, c: usize) -> Result<Owned, FormatError> {
        let meta = &cat.groups[g].cols[c];
        let rows = cat.groups[g].row_count as usize;
        let mut segs: [Vec<u8>; MAX_SEGS] = Default::default();
        for (s, seg) in segs.iter_mut().enumerate() {
            if meta.seg_lens[s] > 0 {
                *seg = read::read_segment(src, cat, g, c, s)?;
            }
        }
        let def = &cat.schema.columns[c];
        let le32 = |b: &[u8]| u32::from_le_bytes(b.try_into().unwrap());
        let offsets = match (def.ty, def.is_dict()) {
            (ColumnType::Utf8, false) => segs[0][..(rows + 1) * 4].chunks_exact(4).map(le32).collect(),
            _ => Vec::new(),
        };
        let codes = if def.is_dict() && def.code_width() == 2 {
            segs[0][..rows * 2].chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect()
        } else {
            Vec::new()
        };
        Ok(Owned { segs, null_count: meta.null_count, offsets, codes })
    }

    fn chunk(&self, def: &ColumnDef, rows: usize) -> ColumnChunk<'_> {
        let bits = rows.div_ceil(8);
        let data = match (def.ty, def.is_dict()) {
            (ColumnType::Utf8, true) if def.code_width() == 1 => SegmentData::Codes8(&self.segs[0][..rows]),
            (ColumnType::Utf8, true) => SegmentData::Codes16(&self.codes),
            (ColumnType::Utf8, false) => {
                SegmentData::Utf8 { offsets: &self.offsets, bytes: &self.segs[1][..self.offsets[rows] as usize] }
            }
            (ColumnType::Bool, _) => SegmentData::Bool(&self.segs[0][..bits]),
            (ty, _) => SegmentData::Fixed(&self.segs[0][..rows * ty.fixed_width().expect("fixed type")]),
        };
        let validity = (self.null_count > 0).then(|| &self.segs[2][..bits]);
        ColumnChunk { data, validity, null_count: self.null_count }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::{open, read_dictionary, read_segment};

    /// Every column kind, NULLs, both code widths, a sort order; 3 groups.
    fn image() -> Vec<u8> {
        let schema = Schema {
            columns: vec![
                ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
                ColumnDef { name: "ok".into(), ty: ColumnType::Bool, flags: 0 },
                ColumnDef { name: "s".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
                ColumnDef { name: "t".into(), ty: ColumnType::Utf8, flags: 0 },
                ColumnDef { name: "o".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY },
                ColumnDef { name: "x".into(), ty: ColumnType::Float64, flags: 0 },
            ],
        };
        let small = compile::utf8_offsets(&["a".to_string(), "b".to_string()]);
        let big: Vec<String> = (0..300).map(|i| format!("v{i}")).collect();
        let big = compile::utf8_offsets(&big);
        let dicts = vec![
            None,
            None,
            Some(DictData { offsets: small.0, bytes: small.1 }),
            None,
            Some(DictData { offsets: big.0, bytes: big.1 }),
            None,
        ];
        let mut w = Writer::new(schema, vec![SortKey { column: 0, descending: false }], 8, &dicts);
        for g in 0..3usize {
            let n = if g == 2 { 5 } else { 8 };
            let ids: Vec<u8> = (0..n).flat_map(|i| ((g * 8 + i) as i32).to_le_bytes()).collect();
            let bits = vec![0b1010_1010u8];
            let s: Vec<u8> = (0..n).map(|i| (i % 2) as u8).collect();
            let texts: Vec<String> = (0..n).map(|i| "word ".repeat(i)).collect();
            let (toff, tbytes) = compile::utf8_offsets(&texts);
            let o: Vec<u16> = (0..n).map(|i| (g * 100 + i) as u16).collect();
            let xs: Vec<u8> = (0..n).flat_map(|i| (i as f64 * 0.5).to_le_bytes()).collect();
            let valid = vec![0b1111_0110u8 & ((1u16 << n) - 1) as u8];
            let nulls = n as u32 - valid[0].count_ones();
            w.write_group(
                n as u32,
                &[
                    ColumnChunk { data: SegmentData::Fixed(&ids), validity: None, null_count: 0 },
                    ColumnChunk { data: SegmentData::Bool(&bits), validity: None, null_count: 0 },
                    ColumnChunk { data: SegmentData::Codes8(&s), validity: None, null_count: 0 },
                    ColumnChunk { data: SegmentData::Utf8 { offsets: &toff, bytes: &tbytes }, validity: Some(&valid), null_count: nulls },
                    ColumnChunk { data: SegmentData::Codes16(&o), validity: Some(&valid), null_count: nulls },
                    ColumnChunk { data: SegmentData::Fixed(&xs), validity: None, null_count: 0 },
                ],
            );
        }
        w.finish()
    }

    #[test]
    fn parts_hold_the_same_segments() {
        let file = image();
        let src: &[u8] = &file;
        let cat = open(&src).unwrap();
        assert_eq!(cat.split_id, None);
        let (a, b) = split(&src, &[3, 4], 0xfeed).unwrap();
        let (sa, sb): (&[u8], &[u8]) = (&a, &b);
        for (part, cols) in [(sa, vec![0, 1, 2, 5]), (sb, vec![3, 4])] {
            let pc = open(&part).unwrap();
            assert_eq!(pc.split_id, Some(0xfeed));
            assert_eq!(pc.image_len, Some(part.len() as u64));
            assert_eq!(pc.total_rows, cat.total_rows);
            assert_eq!(pc.groups.iter().map(|g| g.row_count).collect::<Vec<_>>(), cat.groups.iter().map(|g| g.row_count).collect::<Vec<_>>());
            for (l, &c) in cols.iter().enumerate() {
                assert_eq!(pc.schema.columns[l].name, cat.schema.columns[c].name);
                assert_eq!(pc.schema.columns[l].flags, cat.schema.columns[c].flags);
                assert_eq!(pc.clustered[l], cat.clustered[c], "column {c}");
                if cat.schema.columns[c].is_dict() {
                    assert_eq!(read_dictionary(&part, &pc, l).unwrap(), read_dictionary(&src, &cat, c).unwrap());
                }
                for g in 0..cat.groups.len() {
                    let (x, y) = (&pc.groups[g].cols[l], &cat.groups[g].cols[c]);
                    assert_eq!((x.null_count, x.seg_lens), (y.null_count, y.seg_lens), "column {c} group {g}");
                    assert_eq!(format!("{:?}", x.stats), format!("{:?}", y.stats));
                    for s in 0..MAX_SEGS {
                        if y.seg_lens[s] > 0 {
                            assert_eq!(read_segment(&part, &pc, g, l, s).unwrap(), read_segment(&src, &cat, g, c, s).unwrap(), "column {c} group {g} segment {s}");
                        }
                    }
                }
            }
        }
        // the sort order stays with its column; a part without it has none
        assert_eq!(open(&sa).unwrap().sorted_by.len(), 1);
        let (c, _) = split(&src, &[0], 1).unwrap();
        let sc: &[u8] = &c;
        assert!(open(&sc).unwrap().sorted_by.is_empty());
        // nothing, everything, out of range, id 0
        assert!(split(&src, &[], 1).is_err());
        assert!(split(&src, &[0, 1, 2, 3, 4, 5], 1).is_err());
        assert!(split(&src, &[9], 1).is_err());
        assert!(split(&src, &[3], 0).is_err());
    }
}
