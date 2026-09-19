//! facetful engine. M1 spike scope: a structured query API (no SQL yet) over a
//! [`Table`] — filter masks, correct filters-except-own facet counts, totals,
//! top-k sort, and row gathering for a virtual-scrolled table view.
//!
//! Executor shape per the design doc: work happens per row group (the unit of
//! larger-than-memory scanning); segments load through a synchronous source into
//! a per-(group, column) cache; dictionary columns are executed on their codes.

pub use facetful_format as format;
pub mod mask_cache;
pub mod join;
pub mod materialize;
pub mod text;

pub mod sql;

use format::read::{self, ReadAt};
use format::{Catalog, ColumnType, FormatError};
use std::collections::HashMap;

/// One opened `.facetful` table over a synchronous byte source.
///
/// Segments load through a cache on first touch. With a `cache_budget` set
/// (the OPFS case), the cache is a byte-budgeted LRU: eviction is a plain
/// drop — SELECT-only means there is never anything to write back. Without a
/// budget (memory sources) nothing evicts. Dictionaries and the catalog are
/// always resident and never count against the budget.
pub struct Table<S: ReadAt> {
    src: S,
    cat: Catalog,
    cache: HashMap<(u32, u32, u8), SegEntry>,
    cache_bytes: usize,
    cache_budget: Option<usize>,
    tick: u64,
    /// Decoded dictionaries, cached per column (budget-exempt).
    dict_cache: Vec<Option<Vec<String>>>,
    /// The executor's shared form (Rc per entry), built once per column —
    /// cloning the plain form per query cost ~10 ms on wide real tables.
    dict_rc_cache: Vec<Option<std::rc::Rc<Vec<std::rc::Rc<String>>>>>,
    /// WHERE-conjunct bitmaps (see `mask_cache`); byte-bounded, LRU.
    masks: mask_cache::MaskCache,
    /// Derived tables — materialized CTEs, subqueries and (later) joins —
    /// keyed by their canonical SQL over this table; byte-bounded, LRU.
    /// The mask cache's shape, one level up: tables never change, so nothing
    /// goes stale and only the budget evicts. A Vec, not a HashMap: a handful
    /// of entries, and a HashMap<String, _> instantiation is kilobytes of wasm.
    derived: Vec<Derived<S>>,
    derived_bytes: usize,
    derived_budget: usize,
    derived_tick: u64,
    pub derived_hits: u64,
    pub derived_misses: u64,
}

/// Default derived-table budget: a few large rollups, dozens of facet-sized ones.
pub const DEFAULT_DERIVED_BUDGET: usize = 64 << 20;

struct Derived<S: ReadAt> {
    key: String,
    table: Table<S>,
    bytes: usize,
    last_used: u64,
}

struct SegEntry {
    data: Vec<u8>,
    last_used: u64,
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
        let dict_cache = cat.schema.columns.iter().map(|_| None).collect();
        let dict_rc_cache = cat.schema.columns.iter().map(|_| None).collect();
        Ok(Self {
            src,
            cat,
            cache: HashMap::new(),
            cache_bytes: 0,
            cache_budget: None,
            tick: 0,
            dict_cache,
            dict_rc_cache,
            masks: mask_cache::MaskCache::default(),
            derived: Vec::new(),
            derived_bytes: 0,
            derived_budget: DEFAULT_DERIVED_BUDGET,
            derived_tick: 0,
            derived_hits: 0,
            derived_misses: 0,
        })
    }

    /// The filter-mask cache (per-conjunct WHERE bitmaps).
    pub fn masks(&mut self) -> &mut mask_cache::MaskCache {
        &mut self.masks
    }

    /// A cached derived table, touched as most recently used.
    pub fn derived_get(&mut self, key: &str) -> Option<&mut Table<S>> {
        self.derived_tick += 1;
        let tick = self.derived_tick;
        match self.derived.iter_mut().find(|d| d.key == key) {
            Some(d) => {
                self.derived_hits += 1;
                d.last_used = tick;
                Some(&mut d.table)
            }
            None => {
                self.derived_misses += 1;
                None
            }
        }
    }

    /// A derived table known to be present, for running a query against it;
    /// touches it as most recently used without counting as a probe.
    pub fn derived_table(&mut self, key: &str) -> Option<&mut Table<S>> {
        self.derived_tick += 1;
        let tick = self.derived_tick;
        self.derived.iter_mut().find(|d| d.key == key).map(|d| {
            d.last_used = tick;
            &mut d.table
        })
    }

    /// Drop least-recently-used derived tables until `incoming` more bytes fit.
    fn evict_derived(&mut self, incoming: usize) {
        while self.derived_bytes + incoming > self.derived_budget && !self.derived.is_empty() {
            let (i, _) = self
                .derived
                .iter()
                .enumerate()
                .min_by_key(|(_, d)| d.last_used)
                .expect("non-empty");
            let d = self.derived.swap_remove(i);
            self.derived_bytes -= d.bytes;
        }
    }

    /// Open `image` as a derived table under `key`, evicting least-recently-
    /// used derived tables past the budget. With a zero budget the table is
    /// still returned for this query but not kept.
    pub fn derived_insert(&mut self, key: &str, image: Vec<u8>) -> Result<&mut Table<S>, FormatError> {
        let bytes = image.len();
        let src = S::from_memory(image)
            .ok_or(FormatError::Io("derived tables need a source that can own memory"))?;
        let table = Table::open(src)?;
        self.evict_derived(bytes);
        self.derived_tick += 1;
        self.derived_bytes += bytes;
        if let Some(i) = self.derived.iter().position(|d| d.key == key) {
            self.derived_bytes -= self.derived[i].bytes;
            self.derived.swap_remove(i);
        }
        self.derived.push(Derived { key: key.to_string(), table, bytes, last_used: self.derived_tick });
        Ok(&mut self.derived.last_mut().expect("just pushed").table)
    }

    pub fn set_derived_budget(&mut self, bytes: usize) {
        self.derived_budget = bytes;
        self.evict_derived(0);
    }

    /// (entries, bytes, hits, misses) of the derived-table cache.
    pub fn derived_stats(&self) -> (usize, usize, u64, u64) {
        (self.derived.len(), self.derived_bytes, self.derived_hits, self.derived_misses)
    }

    /// Bound the segment cache (bytes). Exceeding it evicts least-recently-used
    /// segments — a plain drop, nothing to write back.
    pub fn set_cache_budget(&mut self, bytes: usize) {
        self.cache_budget = Some(bytes);
        self.evict_to_budget(0);
    }

    fn evict_to_budget(&mut self, incoming: usize) {
        let Some(budget) = self.cache_budget else { return };
        while self.cache_bytes + incoming > budget && !self.cache.is_empty() {
            let (&key, _) =
                self.cache.iter().min_by_key(|(_, e)| e.last_used).expect("non-empty");
            if let Some(e) = self.cache.remove(&key) {
                self.cache_bytes -= e.data.len();
            }
        }
    }

    /// (cached segments, cached bytes) — cache observability for tests/JS.
    pub fn cache_stats(&self) -> (usize, usize) {
        (self.cache.len(), self.cache_bytes)
    }

    /// Touch every segment of `col` (all groups) so later queries hit cache.
    /// Returns bytes read. With a budget, warming beyond it just churns —
    /// callers warm the hot columns first.
    pub fn warm_column(&mut self, col: usize) -> Result<u64, FormatError> {
        let mut total = 0u64;
        let nsegs = format::seg_count(&self.cat.schema.columns[col]);
        for g in 0..self.cat.groups.len() {
            for seg in 0..nsegs {
                total += self.segment(g, col, seg)?.len() as u64;
            }
            if self.cat.groups[g].cols[col].null_count > 0 {
                total += self.segment(g, col, 2)?.len() as u64;
            }
        }
        Ok(total)
    }

    pub fn catalog(&self) -> &Catalog {
        &self.cat
    }

    /// Integer column of any storage width, widened to i64 (Date/Timestamp too).
    /// `n` caps materialization (min'd with the group's rows): limit-only
    /// queries decode only the rows they can output.
    pub(crate) fn i64s(&mut self, group: usize, col: usize, n: usize) -> Result<Vec<i64>, FormatError> {
        let ty = self.cat.schema.columns[col].ty;
        let w = ty.fixed_width().ok_or(FormatError::Corrupt("not a fixed-width column"))?;
        let rows = (self.cat.groups[group].row_count as usize).min(n);
        let seg = self.segment(group, col, 0)?;
        Ok(match w {
            1 => seg[..rows].iter().map(|&b| b as i8 as i64).collect(),
            2 => seg[..rows * 2].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as i64).collect(),
            4 => seg[..rows * 4].chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap()) as i64).collect(),
            _ => seg[..rows * 8].chunks_exact(8).map(|c| i64::from_le_bytes(c.try_into().unwrap())).collect(),
        })
    }

    /// Plain (non-dict) Utf8 column as raw (offsets, bytes) — the zero-copy
    /// shape the LIKE blob scan wants. Offsets are group-local, offsets[0]=0.
    pub(crate) fn texts_raw(
        &mut self,
        group: usize,
        col: usize,
        n: usize,
    ) -> Result<(Vec<u32>, Vec<u8>), FormatError> {
        let rows = (self.cat.groups[group].row_count as usize).min(n);
        let offs_seg = self.segment(group, col, 0)?;
        let offs: Vec<u32> = offs_seg[..(rows + 1) * 4]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let end = offs[rows] as usize;
        let bytes = self.segment(group, col, 1)?[..end].to_vec();
        Ok((offs, bytes))
    }

    /// Run `f` over a plain-text column's raw segments for one group —
    /// `(offsets as LE u32 bytes, blob, validity bitmap)` — borrowed, no
    /// copy. For the mask cache's LIKE narrowing, which touches only the
    /// candidate rows a cached superset admits; copying the blob out (as
    /// `texts_raw` does) would cost more than the verification itself.
    /// `Ok(None)` when the segments could not all be held resident at once
    /// (tiny positional-cache budgets) — callers fall back to the copy path.
    pub(crate) fn with_text_segments<R>(
        &mut self,
        group: usize,
        col: usize,
        f: impl FnOnce(&[u8], &[u8], Option<&[u8]>) -> R,
    ) -> Result<Option<R>, FormatError> {
        let has_nulls = self.cat.groups[group].cols[col].null_count != 0;
        // make resident (fills the LRU for positional sources; free in memory)
        self.segment(group, col, 0)?;
        self.segment(group, col, 1)?;
        if has_nulls {
            self.segment(group, col, 2)?;
        }
        let (Some(offs), Some(blob)) = (self.resident(group, col, 0), self.resident(group, col, 1))
        else {
            return Ok(None);
        };
        let valid = if has_nulls {
            match self.resident(group, col, 2) {
                Some(v) => Some(v),
                None => return Ok(None),
            }
        } else {
            None
        };
        Ok(Some(f(offs, blob, valid)))
    }

    /// Run `f` over a fixed-width numeric column's raw value segment (LE, the
    /// width in bytes is passed along) and validity bitmap for one group —
    /// borrowed, no copy. For filter fast paths that would otherwise widen a
    /// narrow int column into a `Vec<i64>` just to compare it to a literal.
    /// `Ok(None)` when the segments could not be held resident at once (tiny
    /// positional-cache budgets) — callers fall back to the lane path.
    pub(crate) fn with_fixed_segments<R>(
        &mut self,
        group: usize,
        col: usize,
        f: impl FnOnce(&[u8], usize, Option<&[u8]>) -> R,
    ) -> Result<Option<R>, FormatError> {
        if self.cat.schema.columns[col].is_dict() {
            return Ok(None); // seg 0 holds codes, not values
        }
        let Some(w) = self.cat.schema.columns[col].ty.fixed_width() else {
            return Ok(None);
        };
        let has_nulls = self.cat.groups[group].cols[col].null_count != 0;
        self.segment(group, col, 0)?;
        if has_nulls {
            self.segment(group, col, 2)?;
        }
        let Some(vals) = self.resident(group, col, 0) else {
            return Ok(None);
        };
        let valid = if has_nulls {
            match self.resident(group, col, 2) {
                Some(v) => Some(v),
                None => return Ok(None),
            }
        } else {
            None
        };
        Ok(Some(f(vals, w, valid)))
    }

    /// A segment already in memory (borrowed source, or LRU-resident), if so.
    fn resident(&self, group: usize, col: usize, seg: usize) -> Option<&[u8]> {
        let g = &self.cat.groups[group];
        let len = g.cols[col].seg_lens[seg] as usize;
        let off = read::segment_offset(&self.cat, g, col, seg);
        self.src.read_ref(off, len).or_else(|| {
            self.cache.get(&(group as u32, col as u32, seg as u8)).map(|e| e.data.as_slice())
        })
    }

    pub fn group_count(&self) -> usize {
        self.cat.groups.len()
    }
    pub fn group_rows(&self, g: usize) -> usize {
        self.cat.groups[g].row_count as usize
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.cat.schema.columns.iter().position(|c| c.name == name)
    }

    fn segment(&mut self, group: usize, col: usize, seg: usize) -> Result<&[u8], FormatError> {
        // In-memory sources are borrowed zero-copy: no cache entry, no 2x
        // resident cost. Only positional sources (OPFS) fill the LRU below.
        {
            let g = &self.cat.groups[group];
            let len = g.cols[col].seg_lens[seg] as usize;
            let off = read::segment_offset(&self.cat, g, col, seg);
            if self.src.read_ref(off, len).is_some() {
                return Ok(self.src.read_ref(off, len).unwrap());
            }
        }
        let key = (group as u32, col as u32, seg as u8);
        self.tick += 1;
        if !self.cache.contains_key(&key) {
            let bytes = read::read_segment(&self.src, &self.cat, group, col, seg)?;
            self.evict_to_budget(bytes.len());
            self.cache_bytes += bytes.len();
            self.cache.insert(key, SegEntry { data: bytes, last_used: self.tick });
        }
        let e = self.cache.get_mut(&key).unwrap();
        e.last_used = self.tick;
        Ok(&e.data)
    }

    pub(crate) fn codes(&mut self, group: usize, col: usize, n: usize) -> Result<Vec<u16>, FormatError> {
        debug_assert!(self.cat.schema.columns[col].is_dict());
        let rows = (self.cat.groups[group].row_count as usize).min(n);
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
    pub(crate) fn validity(&mut self, group: usize, col: usize) -> Result<Option<Vec<u8>>, FormatError> {
        if self.cat.groups[group].cols[col].null_count == 0 {
            return Ok(None);
        }
        Ok(Some(self.segment(group, col, 2)?.to_vec()))
    }

    pub(crate) fn f64s(&mut self, group: usize, col: usize, n: usize) -> Result<Vec<f64>, FormatError> {
        debug_assert_eq!(self.cat.schema.columns[col].ty, ColumnType::Float64);
        let rows = (self.cat.groups[group].row_count as usize).min(n);
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

    /// Executor form: shared outer Rc, one Rc<String> per entry. Built once.
    pub(crate) fn dictionary_rc(
        &mut self,
        col: usize,
    ) -> Result<std::rc::Rc<Vec<std::rc::Rc<String>>>, FormatError> {
        if self.dict_rc_cache[col].is_none() {
            let d = self.dictionary(col)?;
            self.dict_rc_cache[col] =
                Some(std::rc::Rc::new(d.into_iter().map(std::rc::Rc::new).collect()));
        }
        Ok(self.dict_rc_cache[col].as_ref().unwrap().clone())
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
                .map(|&c| self.codes(g, c, usize::MAX))
                .collect::<Result<_, _>>()?;
            let measure = self.f64s(g, q.measure, usize::MAX)?;
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
            let vals = self.f64s(g, by, usize::MAX)?;
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
        // order-preserving bits, flipped for DESC, in the (u8, u64, u32, u32)
        // shape the executor's sorts use — one sort instantiation, not a
        // total_cmp copy of its own
        let mut keyed: Vec<(u8, u64, u32)> = pairs
            .into_iter()
            .map(|(v, i)| {
                let b = v.to_bits();
                let k = if b >> 63 == 1 { !b } else { b | (1u64 << 63) };
                (0u8, !k, i)
            })
            .collect();
        crate::sql::exec::sort_keyed2(&mut keyed);
        keyed.truncate(k);
        Ok(keyed.into_iter().map(|t| t.2).collect())
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
                    out.push(self.f64s(g, col, usize::MAX)?[r]);
                }
                Ok(GatherResult::Float(out))
            }
            (ColumnType::Utf8, true) => {
                let dict = self.dictionary(col)?;
                let mut out = Vec::with_capacity(indices.len());
                for &i in indices {
                    let (g, r) = locate(i);
                    let code = self.codes(g, col, usize::MAX)?[r] as usize;
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
