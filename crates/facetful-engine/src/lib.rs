//! facetful engine. M1 spike scope: a structured query API (no SQL yet) over a
//! [`Table`] — filter masks, correct filters-except-own facet counts, totals,
//! top-k sort, and row gathering for a virtual-scrolled table view.
//!
//! Executor shape per the design doc: work happens per row group (the unit of
//! larger-than-memory scanning); segments load through a synchronous source into
//! a per-(group, column) cache; dictionary columns are executed on their codes.

pub use facetful_format as format;

use format::read::{self, ReadAt};
use format::{Catalog, ColumnType, FormatError};

/// One opened `.facetful` table over a synchronous byte source.
pub struct Table<S: ReadAt> {
    src: S,
    cat: Catalog,
    /// cache[group][column][segment] — loaded on first touch, never invalidated
    /// (SELECT-only: there is nothing to invalidate).
    cache: Vec<Vec<[Option<Vec<u8>>; format::MAX_SEGS]>>,
    /// Decoded dictionaries, cached per column.
    dict_cache: Vec<Option<Vec<String>>>,
}

/// A facet-refresh request: `dims` are dictionary-encoded Utf8 columns,
/// `selected[i]` is the set of selected dict codes for dim i (empty = no
/// filter — real facet UIs are multi-select), `measure` is a Float64 column
/// summed for totals.
pub struct FacetQuery {
    pub dims: Vec<usize>,
    pub selected: Vec<Vec<u16>>,
    pub measure: usize,
}

pub struct FacetResult {
    /// counts[d][code] — per dimension, count per dictionary code, computed
    /// under every filter except dimension d's own.
    pub counts: Vec<Vec<u32>>,
    /// Rows passing ALL filters, as a global mask (1 byte per row).
    pub mask: Vec<u8>,
    pub pass_count: u64,
    pub sum: f64,
}

impl<S: ReadAt> Table<S> {
    pub fn open(src: S) -> Result<Self, FormatError> {
        let cat = read::open(&src)?;
        let cache = cat
            .groups
            .iter()
            .map(|_| cat.schema.columns.iter().map(|_| [None, None, None]).collect())
            .collect();
        let dict_cache = cat.schema.columns.iter().map(|_| None).collect();
        Ok(Self { src, cat, cache, dict_cache })
    }

    pub fn catalog(&self) -> &Catalog {
        &self.cat
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.cat.schema.columns.iter().position(|c| c.name == name)
    }

    fn segment(&mut self, group: usize, col: usize, seg: usize) -> Result<&[u8], FormatError> {
        if self.cache[group][col][seg].is_none() {
            let bytes = read::read_segment(&self.src, &self.cat, group, col, seg)?;
            self.cache[group][col][seg] = Some(bytes);
        }
        Ok(self.cache[group][col][seg].as_deref().unwrap())
    }

    fn codes(&mut self, group: usize, col: usize) -> Result<Vec<u16>, FormatError> {
        debug_assert!(self.cat.schema.columns[col].is_dict());
        let rows = self.cat.groups[group].row_count as usize;
        let width = self.cat.schema.columns[col].code_width();
        let seg = self.segment(group, col, 0)?;
        Ok(match width {
            1 => seg[..rows].iter().map(|&b| b as u16).collect(),
            _ => seg[..rows * 2]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect(),
        })
    }

    /// Validity bitmap for (group, col): None when the chunk has no nulls.
    fn validity(&mut self, group: usize, col: usize) -> Result<Option<Vec<u8>>, FormatError> {
        if self.cat.groups[group].cols[col].null_count == 0 {
            return Ok(None);
        }
        Ok(Some(self.segment(group, col, 2)?.to_vec()))
    }

    fn f64s(&mut self, group: usize, col: usize) -> Result<Vec<f64>, FormatError> {
        debug_assert_eq!(self.cat.schema.columns[col].ty, ColumnType::Float64);
        let rows = self.cat.groups[group].row_count as usize;
        let seg = self.segment(group, col, 0)?;
        Ok(seg[..rows * 8]
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect())
    }

    /// Dictionary values of `col`, from the file-level dictionary block (cached).
    pub fn dictionary(&mut self, col: usize) -> Result<Vec<String>, FormatError> {
        if self.dict_cache[col].is_none() {
            let d = read::read_dictionary(&self.src, &self.cat, col)?;
            self.dict_cache[col] = Some(d);
        }
        Ok(self.dict_cache[col].clone().unwrap())
    }

    /// One facet-interface refresh with correct filters-except-own semantics,
    /// executed per row group on dictionary codes.
    pub fn facet_refresh(&mut self, q: &FacetQuery) -> Result<FacetResult, FormatError> {
        let d = q.dims.len();
        assert_eq!(q.selected.len(), d);
        let total_rows = self.cat.total_rows as usize;

        let cards: Vec<usize> = q
            .dims
            .iter()
            .map(|&c| self.dictionary(c).map(|d| d.len()))
            .collect::<Result<_, _>>()?;
        let mut counts: Vec<Vec<u32>> = cards.iter().map(|&c| vec![0u32; c]).collect();
        // Per-dim membership tables: None = unfiltered, else 1 byte per code.
        let members: Vec<Option<Vec<u8>>> = q
            .selected
            .iter()
            .zip(&cards)
            .map(|(sel, &card)| {
                if sel.is_empty() {
                    None
                } else {
                    let mut m = vec![0u8; card];
                    for &c in sel {
                        if (c as usize) < card {
                            m[c as usize] = 1;
                        }
                    }
                    Some(m)
                }
            })
            .collect();
        let mut mask = vec![0u8; total_rows];
        let mut pass_count = 0u64;
        let mut sum = 0f64;

        let ngroups = self.cat.groups.len();
        let mut base = 0usize;
        for g in 0..ngroups {
            let rows = self.cat.groups[g].row_count as usize;
            let dim_codes: Vec<Vec<u16>> = q
                .dims
                .iter()
                .map(|&c| self.codes(g, c))
                .collect::<Result<_, _>>()?;
            let measure = self.f64s(g, q.measure)?;
            let mvalid = self.validity(g, q.measure)?;
            let is_valid =
                |row: usize| mvalid.as_ref().map_or(true, |v| v[row / 8] & (1 << (row % 8)) != 0);

            for row in 0..rows {
                let mut fails = 0u32;
                let mut fail_dim = usize::MAX;
                for k in 0..d {
                    if let Some(m) = &members[k] {
                        if m[dim_codes[k][row] as usize] == 0 {
                            fails += 1;
                            if fails == 2 {
                                break;
                            }
                            fail_dim = k;
                        }
                    }
                }
                if fails == 0 {
                    mask[base + row] = 1;
                    pass_count += 1;
                    if is_valid(row) {
                        sum += measure[row]; // SQL sum(): nulls don't contribute
                    }
                    for k in 0..d {
                        counts[k][dim_codes[k][row] as usize] += 1;
                    }
                } else if fails == 1 {
                    counts[fail_dim][dim_codes[fail_dim][row] as usize] += 1;
                }
            }
            base += rows;
        }

        Ok(FacetResult { counts, mask, pass_count, sum })
    }

    /// Top-k global row indices by Float64 column `by` (descending) among
    /// mask-set rows — the table view sorted by a column under current filters.
    pub fn sort_topk(&mut self, by: usize, mask: &[u8], k: usize) -> Result<Vec<u32>, FormatError> {
        let mut pairs: Vec<(f64, u32)> = Vec::new();
        let mut base = 0usize;
        for g in 0..self.cat.groups.len() {
            let rows = self.cat.groups[g].row_count as usize;
            let vals = self.f64s(g, by)?;
            let valid = self.validity(g, by)?;
            for row in 0..rows {
                if mask[base + row] != 0
                    && valid.as_ref().map_or(true, |v| v[row / 8] & (1 << (row % 8)) != 0)
                {
                    // nulls sort last in DESC (SQLite semantics) — beyond top-k they vanish
                    pairs.push((vals[row], (base + row) as u32));
                }
            }
            base += rows;
        }
        pairs.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        pairs.truncate(k);
        Ok(pairs.into_iter().map(|(_, i)| i).collect())
    }

    /// Materialize output values of one column for the given global row indices
    /// (table-viewer page fetch). Dict columns decode to their strings here —
    /// output is the only place strings materialize.
    pub fn gather(&mut self, col: usize, indices: &[u32]) -> Result<GatherResult, FormatError> {
        // group start offsets
        let mut starts = Vec::with_capacity(self.cat.groups.len() + 1);
        let mut acc = 0u32;
        for g in &self.cat.groups {
            starts.push(acc);
            acc += g.row_count;
        }
        starts.push(acc);
        let locate = |idx: u32| -> (usize, usize) {
            let g = starts.partition_point(|&s| s <= idx) - 1;
            (g, (idx - starts[g]) as usize)
        };

        let def = &self.cat.schema.columns[col];
        match (def.ty, def.is_dict()) {
            (ColumnType::Float64, _) => {
                let mut out = Vec::with_capacity(indices.len());
                for &i in indices {
                    let (g, r) = locate(i);
                    out.push(self.f64s(g, col)?[r]);
                }
                Ok(GatherResult::Float(out))
            }
            (ColumnType::Utf8, true) => {
                let dict = self.dictionary(col)?;
                let mut out = Vec::with_capacity(indices.len());
                for &i in indices {
                    let (g, r) = locate(i);
                    let code = self.codes(g, col)?[r] as usize;
                    out.push(dict.get(code).cloned().unwrap_or_default());
                }
                Ok(GatherResult::Text(out))
            }
            (ty, false) if ty.fixed_width().is_some() && ty != ColumnType::Float64 => {
                let w = ty.fixed_width().unwrap();
                let mut out = Vec::with_capacity(indices.len());
                for &i in indices {
                    let (g, r) = locate(i);
                    let seg = self.segment(g, col, 0)?;
                    let b = &seg[r * w..r * w + w];
                    out.push(match w {
                        1 => b[0] as i8 as i64,
                        2 => i16::from_le_bytes([b[0], b[1]]) as i64,
                        4 => i32::from_le_bytes(b.try_into().unwrap()) as i64,
                        _ => i64::from_le_bytes(b.try_into().unwrap()),
                    });
                }
                Ok(GatherResult::Int(out))
            }
            _ => Err(FormatError::Corrupt("gather: unsupported column type in spike")),
        }
    }
}

pub enum GatherResult {
    Int(Vec<i64>),
    Float(Vec<f64>),
    Text(Vec<String>),
}
