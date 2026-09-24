//! Baseline compiler: typed columns in, a finished `.facetful` image out.
//! The single implementation behind both the native CLI (`facetful convert`)
//! and the browser's lazy Parquet transcoder — the compiled-image discipline
//! says the browser compiler is a cache-filler running the same baseline
//! profile, so it must be the same code.
//!
//! Decisions owned here (moved out of the CLI): narrowest-int selection,
//! dictionary-vs-plain for text (dict when `distinct * 2 < rows`, u8 codes
//! when cardinality ≤ 256), validity bitmaps, group slicing.

use crate::write::{ColumnChunk, DictData, SegmentData, Writer};
use crate::{flags, ColumnDef, ColumnType, Schema, SortKey};
use std::collections::HashMap;

/// One input column. `valid` is per-row (true = present); None = no nulls.
pub enum InCol {
    /// an already dictionary-encoded column: codes into `dict` (≤ 65,535
    /// entries), kept as-is — a gathered dictionary column costs no string work
    Dict { codes: Vec<u16>, dict: Vec<String>, valid: Option<Vec<bool>> },
    Int { v: Vec<i64>, valid: Option<Vec<bool>> },
    Float { v: Vec<f64>, valid: Option<Vec<bool>> },
    Text { v: Vec<String>, valid: Option<Vec<bool>> },
    /// days since 1970-01-01
    Date { v: Vec<i32>, valid: Option<Vec<bool>> },
    /// ms since the epoch, UTC
    Timestamp { v: Vec<i64>, valid: Option<Vec<bool>> },
}

impl InCol {
    fn len(&self) -> usize {
        match self {
            InCol::Dict { codes, .. } => codes.len(),
            InCol::Int { v, .. } => v.len(),
            InCol::Float { v, .. } => v.len(),
            InCol::Text { v, .. } => v.len(),
            InCol::Date { v, .. } => v.len(),
            InCol::Timestamp { v, .. } => v.len(),
        }
    }
}

enum Planned {
    Int { v: Vec<i64>, ty: ColumnType, valid: Option<Vec<bool>> },
    Date { v: Vec<i32>, valid: Option<Vec<bool>> },
    Timestamp { v: Vec<i64>, valid: Option<Vec<bool>> },
    Float { v: Vec<f64>, valid: Option<Vec<bool>> },
    Dict { codes: Vec<u16>, dict: Vec<String>, valid: Option<Vec<bool>> },
    Text { v: Vec<String>, valid: Option<Vec<bool>> },
}

pub(crate) fn narrowest_int(min: i64, max: i64) -> ColumnType {
    if min >= i8::MIN as i64 && max <= i8::MAX as i64 {
        ColumnType::Int8
    } else if min >= i16::MIN as i64 && max <= i16::MAX as i64 {
        ColumnType::Int16
    } else if min >= i32::MIN as i64 && max <= i32::MAX as i64 {
        ColumnType::Int32
    } else {
        ColumnType::Int64
    }
}

fn plan(col: InCol) -> Planned {
    match col {
        // pre-encoded input takes the same payoff test as text: a handful of
        // rows over a big dictionary is smaller (and no slower) as plain text
        InCol::Dict { codes, dict, valid } => {
            if dict.len() * 2 < codes.len() {
                Planned::Dict { codes, dict, valid }
            } else {
                let present = |i: usize| valid.as_ref().map_or(true, |b| b[i]);
                let v = codes
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| if present(i) { dict.get(c as usize).cloned().unwrap_or_default() } else { String::new() })
                    .collect();
                Planned::Text { v, valid }
            }
        }
        InCol::Int { v, valid } => {
            let present = |i: usize| valid.as_ref().map_or(true, |b| b[i]);
            let mut min = i64::MAX;
            let mut max = i64::MIN;
            for (i, &x) in v.iter().enumerate() {
                if present(i) {
                    min = min.min(x);
                    max = max.max(x);
                }
            }
            if min > max {
                (min, max) = (0, 0); // all-null column
            }
            Planned::Int { ty: narrowest_int(min, max), v, valid }
        }
        InCol::Float { v, valid } => Planned::Float { v, valid },
        InCol::Date { v, valid } => Planned::Date { v, valid },
        InCol::Timestamp { v, valid } => Planned::Timestamp { v, valid },
        InCol::Text { v, valid } => {
            let mut index: HashMap<String, u16> = HashMap::new();
            let mut dict: Vec<String> = Vec::new();
            let mut codes: Vec<u16> = Vec::with_capacity(v.len());
            for s in &v {
                if let Some(&c) = index.get(s.as_str()) {
                    codes.push(c);
                } else {
                    if dict.len() >= u16::MAX as usize {
                        return Planned::Text { v, valid };
                    }
                    let c = dict.len() as u16;
                    codes.push(c);
                    dict.push(s.clone());
                    index.insert(s.clone(), c);
                }
            }
            if dict.len() * 2 < v.len() {
                Planned::Dict { codes, dict, valid }
            } else {
                Planned::Text { v, valid }
            }
        }
    }
}

pub(crate) fn utf8_offsets(strings: &[String]) -> (Vec<u32>, Vec<u8>) {
    let mut offsets = Vec::with_capacity(strings.len() + 1);
    let mut bytes = Vec::new();
    offsets.push(0u32);
    for s in strings {
        bytes.extend_from_slice(s.as_bytes());
        offsets.push(bytes.len() as u32);
    }
    (offsets, bytes)
}

/// Bitmap for rows [start, end); None if that slice has no nulls.
pub(crate) fn validity_bitmap(valids: &Option<Vec<bool>>, start: usize, end: usize) -> Option<(Vec<u8>, u32)> {
    let valids = valids.as_ref()?;
    let slice = &valids[start..end];
    let nulls = slice.iter().filter(|&&v| !v).count() as u32;
    if nulls == 0 {
        return None;
    }
    let mut bits = vec![0u8; slice.len().div_ceil(8)];
    for (i, &v) in slice.iter().enumerate() {
        if v {
            bits[i / 8] |= 1 << (i % 8);
        }
    }
    Some((bits, nulls))
}

enum OwnedChunk {
    Fixed(Vec<u8>, Option<(Vec<u8>, u32)>),
    Codes8(Vec<u8>, Option<(Vec<u8>, u32)>),
    Codes16(Vec<u16>, Option<(Vec<u8>, u32)>),
    Utf8 { off: Vec<u32>, bytes: Vec<u8>, validity: Option<(Vec<u8>, u32)> },
}

/// A human-readable one-liner per column ("utf8/dict[42] (u8 codes)") for
/// callers that want to report what the compiler decided.
pub fn describe(schema: &Schema) -> Vec<String> {
    schema
        .columns
        .iter()
        .map(|c| {
            if c.is_dict() {
                format!("utf8/dict (u{} codes)", c.code_width() * 8)
            } else {
                format!("{:?}", c.ty).to_lowercase()
            }
        })
        .collect()
}

/// Compile named columns into a complete `.facetful` image.
/// All columns must share one length; `group_target` rows per row group.
pub fn compile(
    names: &[String],
    cols: Vec<InCol>,
    group_target: u32,
) -> Result<(Vec<u8>, Schema), &'static str> {
    compile_sorted(names, cols, group_target, Vec::new())
}

/// `compile`, recording that the rows arrive sorted by `sorted_by` (a
/// materialized `ORDER BY`): readers then prune by disjoint ranges and skip
/// the sort for a matching `ORDER BY`.
pub fn compile_sorted(
    names: &[String],
    cols: Vec<InCol>,
    group_target: u32,
    sorted_by: Vec<SortKey>,
) -> Result<(Vec<u8>, Schema), &'static str> {
    if sorted_by.iter().any(|k| k.column as usize >= cols.len()) {
        return Err("sort key column out of range");
    }
    if names.len() != cols.len() || cols.is_empty() {
        return Err("column names and data must match and be non-empty");
    }
    let nrows = cols[0].len();
    if cols.iter().any(|c| c.len() != nrows) {
        return Err("all columns must have the same row count");
    }
    if group_target == 0 {
        return Err("row group target must be positive");
    }

    let planned: Vec<Planned> = cols.into_iter().map(plan).collect();
    let schema = Schema {
        columns: names
            .iter()
            .zip(&planned)
            .map(|(name, p)| {
                let (ty, fl) = match p {
                    Planned::Int { ty, .. } => (*ty, 0),
                    Planned::Date { .. } => (ColumnType::Date, 0),
                    Planned::Timestamp { .. } => (ColumnType::Timestamp, 0),
                    Planned::Float { .. } => (ColumnType::Float64, 0),
                    Planned::Dict { dict, .. } => (
                        ColumnType::Utf8,
                        flags::DICTIONARY
                            | if dict.len() <= 256 { flags::CODES_U8 } else { 0 },
                    ),
                    Planned::Text { .. } => (ColumnType::Utf8, 0),
                };
                ColumnDef { name: name.clone(), ty, flags: fl }
            })
            .collect(),
    };

    let dicts: Vec<Option<DictData>> = planned
        .iter()
        .map(|p| match p {
            Planned::Dict { dict, .. } => {
                let (offsets, bytes) = utf8_offsets(dict);
                Some(DictData { offsets, bytes })
            }
            _ => None,
        })
        .collect();

    let mut w = Writer::new(schema.clone(), sorted_by, group_target, &dicts);
    let group = group_target as usize;
    let mut start = 0;
    while start < nrows {
        let end = (start + group).min(nrows);
        let owned: Vec<OwnedChunk> = planned
            .iter()
            .zip(&schema.columns)
            .map(|(p, def)| match p {
                Planned::Int { v, ty, valid } => OwnedChunk::Fixed(
                    match ty {
                        ColumnType::Int8 => v[start..end].iter().map(|&x| x as i8 as u8).collect(),
                        ColumnType::Int16 => {
                            v[start..end].iter().flat_map(|&x| (x as i16).to_le_bytes()).collect()
                        }
                        ColumnType::Int32 => {
                            v[start..end].iter().flat_map(|&x| (x as i32).to_le_bytes()).collect()
                        }
                        _ => v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
                    },
                    validity_bitmap(valid, start, end),
                ),
                Planned::Date { v, valid } => OwnedChunk::Fixed(
                    v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
                    validity_bitmap(valid, start, end),
                ),
                Planned::Timestamp { v, valid } => OwnedChunk::Fixed(
                    v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
                    validity_bitmap(valid, start, end),
                ),
                Planned::Float { v, valid } => OwnedChunk::Fixed(
                    v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
                    validity_bitmap(valid, start, end),
                ),
                Planned::Dict { codes, valid, .. } => {
                    let vb = validity_bitmap(valid, start, end);
                    if def.code_width() == 1 {
                        OwnedChunk::Codes8(
                            codes[start..end].iter().map(|&c| c as u8).collect(),
                            vb,
                        )
                    } else {
                        OwnedChunk::Codes16(codes[start..end].to_vec(), vb)
                    }
                }
                Planned::Text { v, valid } => {
                    let (off, bytes) = utf8_offsets(&v[start..end]);
                    OwnedChunk::Utf8 { off, bytes, validity: validity_bitmap(valid, start, end) }
                }
            })
            .collect();
        let chunks: Vec<ColumnChunk> = owned
            .iter()
            .map(|o| {
                let (data, validity) = match o {
                    OwnedChunk::Fixed(b, v) => (SegmentData::Fixed(b), v.as_ref()),
                    OwnedChunk::Codes8(c, v) => (SegmentData::Codes8(c), v.as_ref()),
                    OwnedChunk::Codes16(c, v) => (SegmentData::Codes16(c), v.as_ref()),
                    OwnedChunk::Utf8 { off, bytes, validity } => {
                        (SegmentData::Utf8 { offsets: off, bytes }, validity.as_ref())
                    }
                };
                ColumnChunk {
                    data,
                    validity: validity.map(|(bits, _)| bits.as_slice()),
                    null_count: validity.map(|(_, n)| *n).unwrap_or(0),
                }
            })
            .collect();
        w.write_group((end - start) as u32, &chunks);
        start = end;
    }
    Ok((w.finish(), schema))
}
