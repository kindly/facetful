//! Reader: parse header + footer into a [`Catalog`] once at open, then serve
//! individual segments through a synchronous [`ReadAt`] source. Segment payloads
//! are returned as raw aligned bytes — callers view them as their native type.

use crate::*;

/// Synchronous positional reads — memory slice, OPFS sync access handle (in the
/// worker), or a JS-glued range cache all implement this shape.
pub trait ReadAt {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FormatError>;
    /// Borrow a range directly when the source is already in memory —
    /// callers then skip their copy-cache entirely (zero-copy read path).
    /// Positional sources (OPFS, HTTP) keep the `None` default.
    fn read_ref(&self, _offset: u64, _len: usize) -> Option<&[u8]> {
        None
    }
    /// Wrap an in-memory image as a source of this type, so a derived table
    /// (a materialized CTE or join) uses the same `S` as its source and the
    /// executor is instantiated once. Borrowed sources cannot own bytes and
    /// keep the `None` default.
    fn from_memory(_bytes: Vec<u8>) -> Option<Self>
    where
        Self: Sized,
    {
        None
    }
    /// The bytes of an in-memory source, to append to while an image is
    /// still arriving (see `open_prefix`). Positional sources keep `None`.
    fn memory_mut(&mut self) -> Option<&mut Vec<u8>> {
        None
    }
}

impl ReadAt for Vec<u8> {
    fn from_memory(bytes: Vec<u8>) -> Option<Self> {
        Some(bytes)
    }
    fn memory_mut(&mut self) -> Option<&mut Vec<u8>> {
        Some(self)
    }
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FormatError> {
        self.as_slice().read_at(offset, buf)
    }
    fn read_ref(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let start = offset as usize;
        self.get(start..start.checked_add(len)?)
    }
}

impl ReadAt for &[u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FormatError> {
        let start = offset as usize;
        let end = start.checked_add(buf.len()).ok_or(FormatError::Truncated)?;
        if end > <[u8]>::len(self) {
            return Err(FormatError::Truncated);
        }
        buf.copy_from_slice(&self[start..end]);
        Ok(())
    }
    fn read_ref(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let start = offset as usize;
        self.get(start..start.checked_add(len)?)
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        if self.p + n > self.b.len() {
            return Err(FormatError::Truncated);
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, FormatError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, FormatError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64, FormatError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes(s.try_into().unwrap()))
    }
}

/// Parse the header bytes (which begin at file offset 0, magic included).
/// `buf` must contain at least the whole header; pass the first few KB.
pub fn parse_header(buf: &[u8]) -> Result<(Schema, Vec<SortKey>, u16, u32, u32), FormatError> {
    parse_header_len(buf).map(|(schema, sorted_by, version, target, hlen, _)| (schema, sorted_by, version, target, hlen))
}

/// `parse_header`, and the image's length when the header carries it: a u64
/// after the sort keys, present when the header (padded to 8 bytes) has at
/// least 8 bytes left after them; 0 means not filled in.
fn parse_header_len(buf: &[u8]) -> Result<(Schema, Vec<SortKey>, u16, u32, u32, Option<u64>), FormatError> {
    let mut c = Cursor::new(buf);
    if c.take(4)? != MAGIC {
        return Err(FormatError::BadMagic);
    }
    let header_len = c.u32()?;
    let version = c.u16()?;
    if version > VERSION {
        return Err(FormatError::UnsupportedVersion(version));
    }
    let _file_flags = c.u16()?;
    let row_group_target = c.u32()?;
    let ncols = c.u16()? as usize;
    let mut columns = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let nlen = c.u16()? as usize;
        let name = core::str::from_utf8(c.take(nlen)?)
            .map_err(|_| FormatError::Corrupt("column name not utf-8"))?
            .to_string();
        let tag = c.u8()?;
        let ty = ColumnType::from_tag(tag).ok_or(FormatError::UnknownType(tag))?;
        let fl = c.u16()?;
        if fl & !flags::KNOWN != 0 {
            return Err(FormatError::UnknownFlags(fl));
        }
        columns.push(ColumnDef { name, ty, flags: fl });
    }
    let nsort = c.u8()? as usize;
    let mut sorted_by = Vec::with_capacity(nsort);
    for _ in 0..nsort {
        let column = c.u16()?;
        let descending = c.u8()? != 0;
        sorted_by.push(SortKey { column, descending });
    }
    let image_len = if header_len as usize >= c.p + 8 { Some(c.u64()?).filter(|&n| n > 0) } else { None };
    Ok((Schema { columns }, sorted_by, version, row_group_target, header_len, image_len))
}

/// Open a file: reads header + footer through `src`, returns the parsed catalog.
pub fn open(src: &impl ReadAt) -> Result<Catalog, FormatError> {
    let file_len = src.len();
    if file_len < 24 {
        return Err(FormatError::Truncated);
    }
    // Header (first read is generous; header is small).
    let hprobe = file_len.min(64 * 1024) as usize;
    let mut hbuf = vec![0u8; hprobe];
    src.read_at(0, &mut hbuf)?;
    let (schema, sorted_by, version, row_group_target, _hlen, image_len) = parse_header_len(&hbuf)?;

    // Tail: footer_len + magic.
    let mut tail = [0u8; 8];
    src.read_at(file_len - 8, &mut tail)?;
    if tail[4..] != MAGIC {
        return Err(FormatError::BadMagic);
    }
    let footer_len = u32::from_le_bytes(tail[..4].try_into().unwrap()) as u64;
    if footer_len + 8 > file_len {
        return Err(FormatError::Corrupt("footer length exceeds file"));
    }
    let mut fbuf = vec![0u8; footer_len as usize];
    src.read_at(file_len - 8 - footer_len, &mut fbuf)?;

    // Dictionary block (immediately after the header): lens table then payloads.
    let (schema, sorted_by, version, row_group_target, hlen) =
        (schema, sorted_by, version, row_group_target, _hlen);
    let mut tbuf = vec![0u8; dict_table_len(&schema)];
    src.read_at(hlen as u64, &mut tbuf)?;
    let (dicts, _) = dict_locs(&schema, hlen as u64, &tbuf);

    let mut c = Cursor::new(&fbuf);
    let total_rows = c.u64()?;
    let ngroups = c.u32()? as usize;
    let ncols = schema.columns.len();
    let mut groups = Vec::with_capacity(ngroups);
    for _ in 0..ngroups {
        let offset = c.u64()?;
        let row_count = c.u32()?;
        let mut cols = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let null_count = c.u32()?;
            let mut seg_lens = [0u32; MAX_SEGS];
            for s in seg_lens.iter_mut() {
                *s = c.u32()?;
            }
            let kind = c.u8()?;
            let a = c.u64()?;
            let b = c.u64()?;
            let stats = match kind {
                0 => Stats::None,
                1 => Stats::Int { min: a as i64, max: b as i64 },
                2 => Stats::Float { min: f64::from_bits(a), max: f64::from_bits(b) },
                _ => return Err(FormatError::Corrupt("unknown stats kind")),
            };
            cols.push(ColMeta { null_count, seg_lens, stats });
        }
        groups.push(GroupMeta { offset, row_count, cols });
    }
    // the layout trailer, absent in older files
    let mut clustered = vec![false; ncols];
    if c.p < c.b.len() {
        let n = c.u16()? as usize;
        if n != ncols {
            return Err(FormatError::Corrupt("layout trailer column count"));
        }
        for flag in clustered.iter_mut() {
            *flag = c.u8()? & layout::CLUSTERED != 0;
        }
    }

    Ok(Catalog {
        clustered,
        image_len,
        version,
        row_group_target,
        schema,
        sorted_by,
        dicts,
        total_rows,
        groups,
    })
}

/// Bytes of the dictionary lens table that follows the header (0 without
/// dictionary columns).
fn dict_table_len(schema: &Schema) -> usize {
    let ndict = schema.columns.iter().filter(|c| c.is_dict()).count();
    if ndict == 0 { 0 } else { crate::align_up(ndict * 8) }
}

/// Where each dictionary's payload lies, from the lens table at `hlen`, and
/// where the dictionary block ends (the first row group starts there).
fn dict_locs(schema: &Schema, hlen: u64, table: &[u8]) -> (Vec<Option<DictLoc>>, u64) {
    let mut dicts: Vec<Option<DictLoc>> = vec![None; schema.columns.len()];
    let mut off = hlen + table.len() as u64;
    let mut di = 0;
    for (ci, c) in schema.columns.iter().enumerate() {
        if c.is_dict() {
            let ol = u32::from_le_bytes(table[di * 8..di * 8 + 4].try_into().unwrap());
            let bl = u32::from_le_bytes(table[di * 8 + 4..di * 8 + 8].try_into().unwrap());
            dicts[ci] = Some(DictLoc { offsets_off: off, offsets_len: ol, bytes_off: off + ol as u64, bytes_len: bl });
            off += ol as u64 + bl as u64;
            di += 1;
        }
    }
    (dicts, off)
}

/// Reading an image front to back while it arrives: the catalog of its
/// beginning (header and dictionaries, no row groups yet, `total_rows` 0) and
/// the offset of the first row group, once `bytes` (the image's first bytes)
/// hold all of that; `None` until then.
pub fn open_prefix(bytes: &[u8]) -> Result<Option<(Catalog, u64)>, FormatError> {
    if bytes.len() < 8 {
        return Ok(None);
    }
    let hlen = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if bytes[..4] != MAGIC {
        return Err(FormatError::BadMagic);
    }
    if bytes.len() < hlen {
        return Ok(None);
    }
    let (schema, sorted_by, version, row_group_target, hlen, image_len) = parse_header_len(&bytes[..hlen])?;
    let table_end = hlen as usize + dict_table_len(&schema);
    if bytes.len() < table_end {
        return Ok(None);
    }
    let (dicts, end) = dict_locs(&schema, hlen as u64, &bytes[hlen as usize..table_end]);
    if (bytes.len() as u64) < end {
        return Ok(None);
    }
    let ncols = schema.columns.len();
    let cat = Catalog {
        clustered: vec![false; ncols],
        image_len,
        version,
        row_group_target,
        schema,
        sorted_by,
        dicts,
        total_rows: 0,
        groups: Vec::new(),
    };
    Ok(Some((cat, end)))
}

/// What follows a row group (or the dictionary block) in an arriving image.
#[derive(Debug)]
pub enum NextGroup {
    /// a whole row group: its metadata (no statistics; those are in the
    /// footer) and where the next one would start
    Ready(GroupMeta, u64),
    /// more bytes are needed to tell
    NeedBytes,
    /// not a row group: the groups are over and this is the footer
    End,
}

/// The row group starting at `at`, read from `bytes` (the image's first
/// bytes). The footer has no marker, so a group header is accepted only if
/// it fits the schema: a row count up to the row-group target, every
/// segment the length its column's type gives for that many rows (only text
/// bytes vary), validity exactly when there are NULLs. The footer's start
/// fails that; a reader should still check the groups against the footer.
pub fn next_group(cat: &Catalog, bytes: &[u8], at: u64) -> NextGroup {
    let ncols = cat.schema.columns.len();
    let header_len = align_up(4 + ncols * (4 + 4 * MAX_SEGS));
    let at = at as usize;
    if bytes.len() < at + header_len {
        return NextGroup::NeedBytes;
    }
    let mut c = Cursor::new(&bytes[at..at + header_len]);
    let rows = c.u32().unwrap();
    if rows == 0 || rows > cat.row_group_target {
        return NextGroup::End;
    }
    let n = rows as usize;
    let mut cols = Vec::with_capacity(ncols);
    let mut len = header_len;
    for def in &cat.schema.columns {
        let null_count = c.u32().unwrap();
        let mut seg_lens = [0u32; MAX_SEGS];
        for l in seg_lens.iter_mut() {
            *l = c.u32().unwrap();
        }
        let bits = align_up(n.div_ceil(8)) as u32;
        let expect = |l: usize| Some(align_up(l) as u32);
        let (first, second) = match (def.ty, def.is_dict()) {
            (ColumnType::Utf8, true) => (expect(n * def.code_width()), Some(0)),
            (ColumnType::Utf8, false) => (expect((n + 1) * 4), None), // text bytes: any padded length
            (ColumnType::Bool, _) => (Some(bits), Some(0)),
            (ty, _) => (ty.fixed_width().and_then(|w| expect(n * w)), Some(0)),
        };
        let validity = if null_count > 0 { bits } else { 0 };
        let fits = null_count <= rows
            && first == Some(seg_lens[0])
            && second.is_none_or(|l| l == seg_lens[1])
            && seg_lens[1] as usize % ALIGN == 0
            && seg_lens[2] == validity;
        if !fits {
            return NextGroup::End;
        }
        len += seg_lens.iter().map(|&l| l as usize).sum::<usize>();
        cols.push(ColMeta { null_count, seg_lens, stats: Stats::None });
    }
    if bytes.len() < at + len {
        return NextGroup::NeedBytes;
    }
    NextGroup::Ready(GroupMeta { offset: at as u64, row_count: rows, cols }, (at + len) as u64)
}

/// Absolute file offset of segment `seg` of column `col` within group `g`.
/// Segments follow the (padded) group header in column order.
pub fn segment_offset(cat: &Catalog, g: &GroupMeta, col: usize, seg: usize) -> u64 {
    let header_len = align_up(4 + cat.schema.columns.len() * (4 + 4 * MAX_SEGS));
    let mut off = g.offset + header_len as u64;
    for (ci, cm) in g.cols.iter().enumerate() {
        for (si, len) in cm.seg_lens.iter().enumerate() {
            if ci == col && si == seg {
                return off;
            }
            off += *len as u64; // lens are stored padded
        }
    }
    unreachable!("segment index out of range")
}

/// Read one segment's padded bytes into a fresh 8-aligned buffer.
pub fn read_segment(
    src: &impl ReadAt,
    cat: &Catalog,
    group: usize,
    col: usize,
    seg: usize,
) -> Result<Vec<u8>, FormatError> {
    let g = &cat.groups[group];
    let len = g.cols[col].seg_lens[seg] as usize;
    let off = segment_offset(cat, g, col, seg);
    let mut buf = vec![0u8; len];
    src.read_at(off, &mut buf)?;
    Ok(buf)
}

/// Decode one dictionary's values (dict columns only).
pub fn read_dictionary(
    src: &impl ReadAt,
    cat: &Catalog,
    col: usize,
) -> Result<Vec<String>, FormatError> {
    let loc = cat.dicts[col].ok_or(FormatError::Corrupt("not a dictionary column"))?;
    let mut obuf = vec![0u8; loc.offsets_len as usize];
    src.read_at(loc.offsets_off, &mut obuf)?;
    let mut bbuf = vec![0u8; loc.bytes_len as usize];
    src.read_at(loc.bytes_off, &mut bbuf)?;
    let offs: Vec<u32> = obuf
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    // (count+1) real entries; the segment is padded with zeros. The last real
    // offset equals the (unpadded) byte length; padding breaks monotonicity.
    let mut count = 0;
    for w in offs.windows(2) {
        if w[1] < w[0] {
            break;
        }
        count += 1;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let (a, b) = (offs[i] as usize, offs[i + 1] as usize);
        if b > bbuf.len() {
            break;
        }
        out.push(
            core::str::from_utf8(&bbuf[a..b])
                .map_err(|_| FormatError::Corrupt("dictionary value not utf-8"))?
                .to_string(),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::{ColumnChunk, DictData, SegmentData, Writer};

    fn utf8_offsets(strings: &[&str]) -> (Vec<u32>, Vec<u8>) {
        let mut offsets = vec![0u32];
        let mut bytes = Vec::new();
        for s in strings {
            bytes.extend_from_slice(s.as_bytes());
            offsets.push(bytes.len() as u32);
        }
        (offsets, bytes)
    }

    #[test]
    fn roundtrip_two_groups() {
        let schema = Schema {
            columns: vec![
                ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
                ColumnDef { name: "amount".into(), ty: ColumnType::Float64, flags: 0 },
                ColumnDef {
                    name: "status".into(),
                    ty: ColumnType::Utf8,
                    flags: flags::DICTIONARY | flags::CODES_U8,
                },
                ColumnDef { name: "owner".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY },
                ColumnDef { name: "note".into(), ty: ColumnType::Utf8, flags: 0 },
            ],
        };
        let (soff, sbytes) = utf8_offsets(&["ok", "closed", "pending"]);
        let (ooff, obytes) = utf8_offsets(&["alpha", "beta"]);
        let dicts = vec![
            None,
            None,
            Some(DictData { offsets: soff, bytes: sbytes }),
            Some(DictData { offsets: ooff, bytes: obytes }),
            None,
        ];
        let sorted = vec![SortKey { column: 0, descending: false }];
        let mut w = Writer::new(schema, sorted, 4, &dicts);

        for gi in 0..2i32 {
            let ids: Vec<u8> = (0..4i32).flat_map(|i| (gi * 4 + i).to_le_bytes()).collect();
            let amounts: Vec<u8> =
                (0..4).flat_map(|i| ((gi * 4 + i) as f64 * 1.5).to_le_bytes()).collect();
            let scodes: [u8; 4] = [0, 2, 1, 0];
            let ocodes: [u16; 4] = [1, 0, 0, 1];
            let (noff, nbytes) = utf8_offsets(&["a", "", "long note here", "x"]);
            w.write_group(
                4,
                &[
                    ColumnChunk { data: SegmentData::Fixed(&ids), validity: None, null_count: 0 },
                    ColumnChunk { data: SegmentData::Fixed(&amounts), validity: None, null_count: 0 },
                    ColumnChunk { data: SegmentData::Codes8(&scodes), validity: None, null_count: 0 },
                    ColumnChunk { data: SegmentData::Codes16(&ocodes), validity: None, null_count: 0 },
                    ColumnChunk {
                        data: SegmentData::Utf8 { offsets: &noff, bytes: &nbytes },
                        validity: None,
                        null_count: 0,
                    },
                ],
            );
        }
        let file = w.finish();

        let src: &[u8] = &file;
        let cat = open(&src).unwrap();
        assert_eq!(cat.total_rows, 8);
        assert_eq!(cat.groups.len(), 2);
        assert!(cat.schema.columns[2].is_dict());
        assert_eq!(cat.schema.columns[2].code_width(), 1);
        assert_eq!(cat.schema.columns[3].code_width(), 2);
        // only `id` keeps each value in one run; floats and plain text are not tracked
        assert_eq!(cat.clustered, vec![true, false, false, false, false]);

        // a file from before the layout trailer: same footer minus its last
        // 2 + ncols bytes, and every column reads as unclustered
        let flen = u32::from_le_bytes(file[file.len() - 8..file.len() - 4].try_into().unwrap()) as usize;
        let cut = file.len() - 8 - 7;
        let mut old = file[..cut].to_vec();
        old.extend_from_slice(&((flen - 7) as u32).to_le_bytes());
        old.extend_from_slice(&MAGIC);
        let old_src: &[u8] = &old;
        let old_cat = open(&old_src).unwrap();
        assert_eq!(old_cat.clustered, vec![false; 5]);
        assert_eq!(old_cat.total_rows, 8);

        // dictionaries decode from the dict block
        assert_eq!(read_dictionary(&src, &cat, 2).unwrap(), vec!["ok", "closed", "pending"]);
        assert_eq!(read_dictionary(&src, &cat, 3).unwrap(), vec!["alpha", "beta"]);
        assert!(read_dictionary(&src, &cat, 0).is_err());

        // narrow int stats + decode
        match cat.groups[1].cols[0].stats {
            Stats::Int { min, max } => {
                assert_eq!(min, 4);
                assert_eq!(max, 7);
            }
            _ => panic!("expected int stats"),
        }
        let seg = read_segment(&src, &cat, 1, 0, 0).unwrap();
        let ids: Vec<i32> =
            seg[..16].chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(ids, vec![4, 5, 6, 7]);

        // u8 codes of group 0
        let codes_seg = read_segment(&src, &cat, 0, 2, 0).unwrap();
        assert_eq!(&codes_seg[..4], &[0, 2, 1, 0]);
        // u16 codes of group 0
        let ocode_seg = read_segment(&src, &cat, 0, 3, 0).unwrap();
        let oc: Vec<u16> =
            ocode_seg[..8].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(oc, vec![1, 0, 0, 1]);

        // plain utf8 offsets survive
        let no = read_segment(&src, &cat, 0, 4, 0).unwrap();
        let offs: Vec<u32> =
            no[..20].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(offs, vec![0, 1, 1, 15, 16]);

        // the length is in the header; a writer whose header was taken out
        // before the end leaves it 0, which reads as unknown
        assert_eq!(cat.image_len, Some(file.len() as u64));
        let mut w = Writer::new(cat.schema.clone(), vec![], 4, &dicts);
        let mut streamed = w.take_output();
        let at = w.len_offset() as usize;
        streamed.extend(w.finish());
        let s: &[u8] = &streamed;
        assert_eq!(open(&s).unwrap().image_len, None);
        let len = (streamed.len() as u64).to_le_bytes();
        streamed[at..at + 8].copy_from_slice(&len);
        let s: &[u8] = &streamed;
        assert_eq!(open(&s).unwrap().image_len, Some(streamed.len() as u64));

        // header alone parses (streaming path)
        let (schema2, _, _, _, hlen) = parse_header(&file[..file.len().min(1024)]).unwrap();
        assert_eq!(schema2.columns[4].name, "note");
        assert!(hlen as usize % ALIGN == 0);
    }

    /// Every prefix of an image, read front to back, gives the footer's
    /// groups in order and then stops at the footer.
    fn stream_matches_footer(file: &[u8]) {
        let src: &[u8] = file;
        let full = open(&src).unwrap();
        assert_eq!(full.image_len, Some(file.len() as u64), "the header gives the image's length");
        let mut found: Vec<GroupMeta> = Vec::new();
        let (mut cat, mut at, mut ended) = (None::<Catalog>, 0u64, false);
        for n in 0..=file.len() {
            let bytes = &file[..n];
            if cat.is_none() {
                if let Some((c, first)) = open_prefix(bytes).unwrap() {
                    assert_eq!(c.image_len, full.image_len);
                    assert_eq!(c.schema.columns.len(), full.schema.columns.len());
                    assert_eq!(c.dicts, full.dicts);
                    cat = Some(c);
                    at = first;
                }
            }
            let Some(c) = &cat else { continue };
            while !ended {
                match next_group(c, bytes, at) {
                    NextGroup::Ready(g, next) => {
                        found.push(g);
                        at = next;
                    }
                    NextGroup::NeedBytes => break,
                    NextGroup::End => ended = true,
                }
            }
        }
        assert!(ended, "the footer ends the groups");
        assert_eq!(found.len(), full.groups.len());
        for (a, b) in found.iter().zip(&full.groups) {
            assert_eq!((a.offset, a.row_count), (b.offset, b.row_count));
            for (x, y) in a.cols.iter().zip(&b.cols) {
                assert_eq!((x.null_count, x.seg_lens), (y.null_count, y.seg_lens));
            }
        }
    }

    #[test]
    fn reads_front_to_back() {
        // nulls, every column kind, a group shorter than the target last
        let schema = Schema {
            columns: vec![
                ColumnDef { name: "id".into(), ty: ColumnType::Int64, flags: 0 },
                ColumnDef { name: "ok".into(), ty: ColumnType::Bool, flags: 0 },
                ColumnDef { name: "s".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
                ColumnDef { name: "t".into(), ty: ColumnType::Utf8, flags: 0 },
            ],
        };
        let (soff, sbytes) = utf8_offsets(&["a", "b"]);
        let dicts = vec![None, None, Some(DictData { offsets: soff, bytes: sbytes }), None];
        for (groups, last) in [(1usize, 3u32), (3, 5), (2, 1)] {
            let mut w = Writer::new(schema.clone(), vec![], 5, &dicts);
            for g in 0..groups {
                let n = if g + 1 == groups { last } else { 5 };
                let ids: Vec<u8> = (0..n as i64).flat_map(|i| i.to_le_bytes()).collect();
                let bits = vec![0b10101u8];
                let codes: Vec<u8> = (0..n).map(|i| (i % 2) as u8).collect();
                let texts: Vec<&str> = (0..n).map(|i| if i % 2 == 0 { "word" } else { "" }).collect();
                let (toff, tbytes) = utf8_offsets(&texts);
                let validity = vec![0b11110u8 & ((1u8 << n) - 1)];
                w.write_group(
                    n,
                    &[
                        ColumnChunk { data: SegmentData::Fixed(&ids), validity: None, null_count: 0 },
                        ColumnChunk { data: SegmentData::Bool(&bits), validity: None, null_count: 0 },
                        ColumnChunk { data: SegmentData::Codes8(&codes), validity: None, null_count: 0 },
                        ColumnChunk {
                            data: SegmentData::Utf8 { offsets: &toff, bytes: &tbytes },
                            validity: Some(&validity),
                            null_count: n - validity[0].count_ones(),
                        },
                    ],
                );
            }
            stream_matches_footer(&w.finish());
        }
    }

    #[test]
    fn rejects_garbage() {
        let junk = vec![0u8; 64];
        let src: &[u8] = &junk;
        assert!(matches!(open(&src), Err(FormatError::BadMagic)));
    }
}
