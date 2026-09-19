//! Streaming converter (design.sv d51): CSV bytes in, a `.facetful` image out,
//! in bounded memory and with no I/O of its own. Two passes over the input:
//!
//! 1. [`Sniffer`] — per column, a type state machine (int → float → date →
//!    timestamp → text, with int min/max for narrowing) and a bounded distinct
//!    set for the dictionary decision. `finish()` yields a [`Plan`].
//! 2. [`Encoder`] — rows encoded against the plan into the current row group;
//!    every `group_target` rows a group is written and its bytes become
//!    available from `take_output()`; `finish()` writes the footer.
//!
//! [`CsvReader`] turns byte chunks into rows across chunk boundaries. Drivers
//! (the native CLI, the wasm exports, Node) own the reading and the sink; the
//! decisions here are the same ones [`crate::compile`] makes over whole
//! columns, so a streamed image is byte-identical to a compiled one.

use crate::compile::{narrowest_int, utf8_offsets, validity_bitmap};
use crate::write::{ColumnChunk, DictData, SegmentData, Writer};
use crate::{flags, ColumnDef, ColumnType, Schema};
use std::collections::HashMap;

/// Dictionary cardinality cap: u16 codes, one value reserved (see compile).
const DICT_CAP: usize = u16::MAX as usize;

// ---------------- CSV ----------------

/// A streaming RFC-4180-ish CSV parser: `push` any byte chunking, get complete
/// rows; `"` quoting with `""` escapes, CRLF or LF, a leading UTF-8 BOM
/// skipped. Cells are handed out as `&str` (lossy on invalid UTF-8).
pub struct CsvReader {
    field: Vec<u8>,
    row: Vec<String>,
    in_quotes: bool,
    /// the byte before the current one was a quote inside a quoted field
    quote_pending: bool,
    at_start: bool,
    pub rows_seen: u64,
}

impl Default for CsvReader {
    fn default() -> Self {
        Self::new()
    }
}

impl CsvReader {
    pub fn new() -> Self {
        CsvReader { field: Vec::new(), row: Vec::new(), in_quotes: false, quote_pending: false, at_start: true, rows_seen: 0 }
    }

    /// Feed a chunk; `on_row` sees each completed row (cells in order).
    pub fn push(&mut self, mut bytes: &[u8], mut on_row: impl FnMut(&[String])) {
        if self.at_start {
            if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                bytes = &bytes[3..];
            }
            if !bytes.is_empty() {
                self.at_start = false;
            }
        }
        for &b in bytes {
            if self.in_quotes {
                if self.quote_pending {
                    self.quote_pending = false;
                    if b == b'"' {
                        self.field.push(b'"');
                        continue;
                    }
                    // the quote closed the field; fall through to unquoted handling
                    self.in_quotes = false;
                } else if b == b'"' {
                    self.quote_pending = true;
                    continue;
                } else {
                    self.field.push(b);
                    continue;
                }
            }
            match b {
                b'"' => self.in_quotes = true,
                b',' => self.end_field(),
                b'\r' => {}
                b'\n' => {
                    self.end_field();
                    self.rows_seen += 1;
                    on_row(&self.row);
                    self.row.clear();
                }
                _ => self.field.push(b),
            }
        }
    }

    fn end_field(&mut self) {
        let s = String::from_utf8_lossy(&self.field).into_owned();
        self.field.clear();
        self.row.push(s);
    }

    /// End of input: a final unterminated row is delivered.
    pub fn finish(&mut self, mut on_row: impl FnMut(&[String])) {
        if self.quote_pending {
            self.in_quotes = false;
            self.quote_pending = false;
        }
        if !self.field.is_empty() || !self.row.is_empty() {
            self.end_field();
            self.rows_seen += 1;
            on_row(&self.row);
            self.row.clear();
        }
    }
}

// ---------------- pass 1: sniff ----------------

#[derive(Clone)]
struct ColState {
    all_int: bool,
    all_float: bool,
    all_date: bool,
    all_ts: bool,
    non_empty: u64,
    rows: u64,
    min: i64,
    max: i64,
    /// first-appearance dictionary; None once the cap was exceeded
    distinct: Option<(HashMap<String, u16>, Vec<String>)>,
}

impl ColState {
    fn new() -> Self {
        ColState {
            all_int: true,
            all_float: true,
            all_date: true,
            all_ts: true,
            non_empty: 0,
            rows: 0,
            min: i64::MAX,
            max: i64::MIN,
            distinct: Some((HashMap::new(), Vec::new())),
        }
    }
    fn see(&mut self, v: &str) {
        self.rows += 1;
        // dictionary candidates: every value including "" (a text column keeps
        // "" as a value, matching compile's whole-column behaviour)
        if let Some((index, dict)) = &mut self.distinct {
            if !index.contains_key(v) {
                if dict.len() >= DICT_CAP {
                    self.distinct = None;
                } else {
                    index.insert(v.to_string(), dict.len() as u16);
                    dict.push(v.to_string());
                }
            }
        }
        if v.is_empty() {
            return;
        }
        self.non_empty += 1;
        if self.all_int {
            match v.parse::<i64>() {
                Ok(x) => {
                    self.min = self.min.min(x);
                    self.max = self.max.max(x);
                }
                Err(_) => self.all_int = false,
            }
        }
        if self.all_float && v.parse::<f64>().is_err() {
            self.all_float = false;
        }
        if self.all_date && crate::time::parse_date(v).is_none() {
            self.all_date = false;
        }
        if self.all_ts && crate::time::parse_timestamp(v).is_none() {
            self.all_ts = false;
        }
    }
}

/// The decided shape of one column.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Int(ColumnType),
    Float,
    Date,
    Timestamp,
    /// dictionary-encoded text: codes are indices into the plan's dictionary
    Dict,
    Text,
}

pub struct Plan {
    pub names: Vec<String>,
    pub kinds: Vec<Kind>,
    /// per column: the dictionary (index + values) for `Kind::Dict`
    dicts: Vec<Option<(HashMap<String, u16>, Vec<String>)>>,
    pub rows: u64,
}

impl Plan {
    pub fn schema(&self) -> Schema {
        Schema {
            columns: self
                .names
                .iter()
                .zip(&self.kinds)
                .zip(&self.dicts)
                .map(|((name, k), d)| {
                    let (ty, fl) = match k {
                        Kind::Int(t) => (*t, 0),
                        Kind::Float => (ColumnType::Float64, 0),
                        Kind::Date => (ColumnType::Date, 0),
                        Kind::Timestamp => (ColumnType::Timestamp, 0),
                        Kind::Dict => {
                            let n = d.as_ref().map_or(0, |(_, v)| v.len());
                            (ColumnType::Utf8, flags::DICTIONARY | if n <= 256 { flags::CODES_U8 } else { 0 })
                        }
                        Kind::Text => (ColumnType::Utf8, 0),
                    };
                    ColumnDef { name: name.clone(), ty, flags: fl }
                })
                .collect(),
        }
    }
}

pub struct Sniffer {
    names: Vec<String>,
    cols: Vec<ColState>,
    rows: u64,
}

impl Sniffer {
    pub fn new(names: Vec<String>) -> Self {
        let n = names.len();
        Sniffer { names, cols: vec![ColState::new(); n], rows: 0 }
    }

    /// One data row (header excluded). Ragged rows are an error.
    pub fn row(&mut self, cells: &[String]) -> Result<(), String> {
        if cells.len() != self.cols.len() {
            return Err(format!("row {} has {} cells, header has {}", self.rows + 2, cells.len(), self.cols.len()));
        }
        for (c, v) in self.cols.iter_mut().zip(cells) {
            c.see(v);
        }
        self.rows += 1;
        Ok(())
    }

    /// The same decisions as `compile::plan`, in this order: int, float, date,
    /// timestamp (all over non-empty cells, empty = NULL), else text, which is
    /// dictionary-encoded when distinct * 2 < rows and distinct ≤ 65,535.
    pub fn finish(self) -> Plan {
        let rows = self.rows;
        let mut kinds = Vec::with_capacity(self.cols.len());
        let mut dicts = Vec::with_capacity(self.cols.len());
        for c in self.cols {
            let (kind, dict) = if c.all_int && c.non_empty > 0 {
                (Kind::Int(narrowest_int(c.min, c.max)), None)
            } else if c.all_float && c.non_empty > 0 {
                (Kind::Float, None)
            } else if c.all_date && c.non_empty > 0 {
                (Kind::Date, None)
            } else if c.all_ts && c.non_empty > 0 {
                (Kind::Timestamp, None)
            } else {
                match c.distinct {
                    Some(d) if d.1.len() * 2 < rows as usize => (Kind::Dict, Some(d)),
                    _ => (Kind::Text, None),
                }
            };
            kinds.push(kind);
            dicts.push(dict);
        }
        Plan { names: self.names, kinds, dicts, rows }
    }
}

// ---------------- pass 2: encode ----------------

enum Buf {
    Int(Vec<i64>),
    Float(Vec<f64>),
    Date(Vec<i32>),
    Timestamp(Vec<i64>),
    Codes(Vec<u16>),
    Text(Vec<String>),
}

pub struct Encoder {
    plan: Plan,
    schema: Schema,
    writer: Writer,
    group_target: usize,
    bufs: Vec<Buf>,
    valids: Vec<Vec<bool>>,
    in_group: usize,
    rows: u64,
}

impl Encoder {
    pub fn new(plan: Plan, group_target: u32) -> Result<Encoder, String> {
        if group_target == 0 {
            return Err("row group target must be positive".into());
        }
        if plan.names.is_empty() {
            return Err("no columns".into());
        }
        let schema = plan.schema();
        let dict_data: Vec<Option<DictData>> = plan
            .dicts
            .iter()
            .map(|d| {
                d.as_ref().map(|(_, values)| {
                    let (offsets, bytes) = utf8_offsets(values);
                    DictData { offsets, bytes }
                })
            })
            .collect();
        let writer = Writer::new(schema.clone(), Vec::new(), group_target, &dict_data);
        let bufs = plan
            .kinds
            .iter()
            .map(|k| match k {
                Kind::Int(_) => Buf::Int(Vec::new()),
                Kind::Float => Buf::Float(Vec::new()),
                Kind::Date => Buf::Date(Vec::new()),
                Kind::Timestamp => Buf::Timestamp(Vec::new()),
                Kind::Dict => Buf::Codes(Vec::new()),
                Kind::Text => Buf::Text(Vec::new()),
            })
            .collect();
        let n = plan.names.len();
        Ok(Encoder {
            plan,
            schema,
            writer,
            group_target: group_target as usize,
            bufs,
            valids: vec![Vec::new(); n],
            in_group: 0,
            rows: 0,
        })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// One data row. Cells must parse as the plan decided in pass 1; a cell
    /// that no longer does means the input changed between passes.
    pub fn row(&mut self, cells: &[String]) -> Result<(), String> {
        if cells.len() != self.bufs.len() {
            return Err(format!("row {} has {} cells, header has {}", self.rows + 2, cells.len(), self.bufs.len()));
        }
        if self.rows >= self.plan.rows {
            return Err("more rows than the first pass saw: the input changed".into());
        }
        let bad = |what: &str, v: &str| format!("row {}: '{v}' is not {what} (the input changed between passes?)", self.rows + 2);
        for (ci, v) in cells.iter().enumerate() {
            let empty = v.is_empty();
            match &mut self.bufs[ci] {
                Buf::Int(b) => {
                    b.push(if empty { 0 } else { v.parse().map_err(|_| bad("an integer", v))? });
                    self.valids[ci].push(!empty);
                }
                Buf::Float(b) => {
                    b.push(if empty { 0.0 } else { v.parse().map_err(|_| bad("a number", v))? });
                    self.valids[ci].push(!empty);
                }
                Buf::Date(b) => {
                    b.push(if empty { 0 } else { crate::time::parse_date(v).ok_or_else(|| bad("a date", v))? as i32 });
                    self.valids[ci].push(!empty);
                }
                Buf::Timestamp(b) => {
                    b.push(if empty { 0 } else { crate::time::parse_timestamp(v).ok_or_else(|| bad("a timestamp", v))? });
                    self.valids[ci].push(!empty);
                }
                Buf::Codes(b) => {
                    let (index, _) = self.plan.dicts[ci].as_ref().expect("dict column has a dictionary");
                    b.push(*index.get(v.as_str()).ok_or_else(|| bad("in the dictionary", v))?);
                }
                Buf::Text(b) => b.push(v.clone()),
            }
        }
        self.in_group += 1;
        self.rows += 1;
        if self.in_group >= self.group_target {
            self.flush_group();
        }
        Ok(())
    }

    fn flush_group(&mut self) {
        let n = self.in_group;
        if n == 0 {
            return;
        }
        enum Owned {
            Fixed(Vec<u8>),
            Codes8(Vec<u8>),
            Codes16(Vec<u16>),
            Utf8(Vec<u32>, Vec<u8>),
        }
        let mut owned: Vec<(Owned, Option<(Vec<u8>, u32)>)> = Vec::with_capacity(self.bufs.len());
        for (ci, buf) in self.bufs.iter_mut().enumerate() {
            let def = &self.schema.columns[ci];
            let valid = if self.valids[ci].is_empty() { None } else { Some(core::mem::take(&mut self.valids[ci])) };
            let vb = validity_bitmap(&valid, 0, n);
            let data = match buf {
                Buf::Int(v) => Owned::Fixed(match def.ty {
                    ColumnType::Int8 => v.iter().map(|&x| x as i8 as u8).collect(),
                    ColumnType::Int16 => v.iter().flat_map(|&x| (x as i16).to_le_bytes()).collect(),
                    ColumnType::Int32 => v.iter().flat_map(|&x| (x as i32).to_le_bytes()).collect(),
                    _ => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
                }),
                Buf::Float(v) => Owned::Fixed(v.iter().flat_map(|x| x.to_le_bytes()).collect()),
                Buf::Date(v) => Owned::Fixed(v.iter().flat_map(|x| x.to_le_bytes()).collect()),
                Buf::Timestamp(v) => Owned::Fixed(v.iter().flat_map(|x| x.to_le_bytes()).collect()),
                Buf::Codes(v) => {
                    if def.code_width() == 1 {
                        Owned::Codes8(v.iter().map(|&c| c as u8).collect())
                    } else {
                        Owned::Codes16(v.clone())
                    }
                }
                Buf::Text(v) => {
                    let (o, b) = utf8_offsets(v);
                    Owned::Utf8(o, b)
                }
            };
            match buf {
                Buf::Int(v) => v.clear(),
                Buf::Float(v) => v.clear(),
                Buf::Date(v) => v.clear(),
                Buf::Timestamp(v) => v.clear(),
                Buf::Codes(v) => v.clear(),
                Buf::Text(v) => v.clear(),
            }
            owned.push((data, vb));
        }
        let chunks: Vec<ColumnChunk> = owned
            .iter()
            .map(|(o, vb)| ColumnChunk {
                data: match o {
                    Owned::Fixed(b) => SegmentData::Fixed(b),
                    Owned::Codes8(c) => SegmentData::Codes8(c),
                    Owned::Codes16(c) => SegmentData::Codes16(c),
                    Owned::Utf8(off, bytes) => SegmentData::Utf8 { offsets: off, bytes },
                },
                validity: vb.as_ref().map(|(bits, _)| bits.as_slice()),
                null_count: vb.as_ref().map_or(0, |(_, k)| *k),
            })
            .collect();
        self.writer.write_group(n as u32, &chunks);
        self.in_group = 0;
    }

    /// Bytes of finished groups (and, first time, the header and dictionaries).
    pub fn take_output(&mut self) -> Vec<u8> {
        self.writer.take_output()
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// The last group and the footer; returns the remaining bytes.
    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        if self.rows != self.plan.rows {
            return Err(format!("first pass saw {} rows, second {}: the input changed", self.plan.rows, self.rows));
        }
        if self.rows == 0 {
            return Err("no data rows".into());
        }
        self.flush_group();
        Ok(self.writer.finish())
    }
}

/// Convenience: a complete CSV in memory, streamed through both passes with
/// `chunk`-byte feeds — the drivers' loop in one place, and the test oracle.
pub fn convert_csv(csv: &[u8], group_target: u32, chunk: usize) -> Result<(Vec<u8>, Schema), String> {
    let chunk = chunk.max(1);
    // pass 1
    let mut reader = CsvReader::new();
    let mut header: Option<Vec<String>> = None;
    let mut sniffer: Option<Sniffer> = None;
    let mut err: Option<String> = None;
    {
        let mut on_row = |row: &[String]| {
            if err.is_some() {
                return;
            }
            match &mut sniffer {
                None => {
                    header = Some(row.to_vec());
                    sniffer = Some(Sniffer::new(row.to_vec()));
                }
                Some(s) => {
                    if let Err(e) = s.row(row) {
                        err = Some(e);
                    }
                }
            }
        };
        for c in csv.chunks(chunk) {
            reader.push(c, &mut on_row);
        }
        reader.finish(&mut on_row);
    }
    if let Some(e) = err {
        return Err(e);
    }
    let plan = sniffer.ok_or("empty input")?.finish();
    // pass 2
    let mut enc = Encoder::new(plan, group_target)?;
    let schema = enc.schema().clone();
    let mut out = Vec::new();
    let mut reader = CsvReader::new();
    let mut first = true;
    let mut err: Option<String> = None;
    {
        let mut on_row = |row: &[String]| {
            if first {
                first = false;
                return;
            }
            if err.is_none() {
                if let Err(e) = enc.row(row) {
                    err = Some(e);
                }
            }
        };
        for c in csv.chunks(chunk) {
            reader.push(c, &mut on_row);
        }
        reader.finish(&mut on_row);
    }
    if let Some(e) = err {
        return Err(e);
    }
    out.extend(enc.take_output());
    out.extend(enc.finish()?);
    Ok((out, schema))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{compile, InCol};

    fn parse_whole(csv: &str) -> (Vec<String>, Vec<Vec<String>>) {
        let mut rows = Vec::new();
        let mut r = CsvReader::new();
        r.push(csv.as_bytes(), |row| rows.push(row.to_vec()));
        r.finish(|row| rows.push(row.to_vec()));
        let header = rows.remove(0);
        (header, rows)
    }

    /// The whole-column compiler's answer for the same CSV (the CLI's old path).
    fn compiled(csv: &str, group: u32) -> Vec<u8> {
        let (header, rows) = parse_whole(csv);
        let ncols = header.len();
        let mut cols: Vec<Vec<String>> = vec![Vec::new(); ncols];
        for r in rows {
            for (i, v) in r.into_iter().enumerate() {
                cols[i].push(v);
            }
        }
        let in_cols: Vec<InCol> = cols
            .into_iter()
            .map(|values| {
                let non_empty: Vec<&String> = values.iter().filter(|v| !v.is_empty()).collect();
                let valids = || if non_empty.len() < values.len() { Some(values.iter().map(|v| !v.is_empty()).collect()) } else { None };
                if !non_empty.is_empty() && non_empty.iter().all(|v| v.parse::<i64>().is_ok()) {
                    InCol::Int { v: values.iter().map(|s| s.parse().unwrap_or(0)).collect(), valid: valids() }
                } else if !non_empty.is_empty() && non_empty.iter().all(|v| v.parse::<f64>().is_ok()) {
                    InCol::Float { v: values.iter().map(|s| s.parse().unwrap_or(0.0)).collect(), valid: valids() }
                } else if !non_empty.is_empty() && non_empty.iter().all(|v| crate::time::parse_date(v).is_some()) {
                    InCol::Date { v: values.iter().map(|s| crate::time::parse_date(s).unwrap_or(0) as i32).collect(), valid: valids() }
                } else {
                    InCol::Text { v: values, valid: None }
                }
            })
            .collect();
        compile(&header, in_cols, group).unwrap().0
    }

    const CSV: &str = "name,n,x,d,note\n\
        \"Sand, Point\",1,0.5,2020-01-02,a\n\
        Coal Creek,,1.5,2020-01-03,\"quoted \"\"x\"\"\"\n\
        Wind Farm,300,,,\n\
        Barry,4,3.5,2021-12-31,z\r\n\
        Coal Creek,70000,4.5,2020-01-02,a\n";

    #[test]
    fn csv_reader_handles_quotes_crlf_and_chunk_boundaries() {
        let (h, rows) = parse_whole(CSV);
        assert_eq!(h, ["name", "n", "x", "d", "note"]);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0], "Sand, Point");
        assert_eq!(rows[1][4], "quoted \"x\"");
        assert_eq!(rows[2][4], "");
        // every chunking yields the same rows
        for chunk in [1usize, 2, 3, 7, 64] {
            let mut rows2 = Vec::new();
            let mut r = CsvReader::new();
            for c in CSV.as_bytes().chunks(chunk) {
                r.push(c, |row| rows2.push(row.to_vec()));
            }
            r.finish(|row| rows2.push(row.to_vec()));
            assert_eq!(rows2.remove(0), h, "chunk {chunk}");
            assert_eq!(rows2, rows, "chunk {chunk}");
        }
        // BOM skipped; final row without newline delivered
        let (h, rows) = parse_whole("\u{feff}a,b\n1,2");
        assert_eq!(h, ["a", "b"]);
        assert_eq!(rows, [["1", "2"]]);
    }

    #[test]
    fn streamed_image_is_byte_identical_to_compiled() {
        for group in [2u32, 3, 65536] {
            let (streamed, schema) = convert_csv(CSV.as_bytes(), group, 5).unwrap();
            assert_eq!(streamed, compiled(CSV, group), "group {group}");
            let kinds: Vec<String> = crate::compile::describe(&schema);
            // name: 4 distinct of 5 rows → plain text; n: int32 (70000); x float; d date; note: 3 distinct of 5 → plain
            assert_eq!(kinds, ["utf8", "int32", "float64", "date", "utf8"]);
        }
        // a dictionary column: repeated values
        let csv = "k,v\n".to_string() + &(0..50).map(|i| format!("{},{}\n", ["eu", "us", "asia"][i % 3], i)).collect::<String>();
        let (streamed, schema) = convert_csv(csv.as_bytes(), 16, 11).unwrap();
        assert_eq!(streamed, compiled(&csv, 16));
        assert!(schema.columns[0].is_dict());
        // reads back
        let cat = crate::read::open(&streamed).unwrap();
        assert_eq!(cat.total_rows, 50);
        assert_eq!(crate::read::read_dictionary(&streamed, &cat, 0).unwrap(), vec!["eu", "us", "asia"]);
    }

    #[test]
    fn errors() {
        assert!(convert_csv(b"a,b\n1,2,3\n", 8, 4).unwrap_err().contains("row 2 has 3 cells"));
        assert!(convert_csv(b"a,b\n", 8, 4).unwrap_err().contains("no data rows"));
        // the input changing between passes is caught by the encoder
        let mut s = Sniffer::new(vec!["a".into()]);
        s.row(&["1".into()]).unwrap();
        let mut e = Encoder::new(s.finish(), 8).unwrap();
        assert!(e.row(&["x".into()]).unwrap_err().contains("not an integer"));
    }
}
