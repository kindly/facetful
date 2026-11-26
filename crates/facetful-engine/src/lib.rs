//! facetful engine. M1 spike scope: a structured query API (no SQL yet) over a
//! [`Table`] — filter masks, correct filters-except-own facet counts, totals,
//! top-k sort, and row gathering for a virtual-scrolled table view.
//!
//! Executor shape per the design doc: work happens per row group (the unit of
//! larger-than-memory scanning); segments load through a synchronous source into
//! a per-(group, column) cache; dictionary columns are executed on their codes.

pub use facetful_format as format;
pub mod mask_cache;
#[cfg(feature = "search")]
pub mod search_index;
#[cfg(feature = "search")]
mod unicode_tables;
#[cfg(feature = "write")]
pub mod join;
pub mod load;
#[cfg(feature = "write")]
pub mod materialize;
pub mod text;
pub mod udf;

pub mod sql;

use format::read::{self, ReadAt};
use format::{Catalog, ColumnType, FormatError};
use format::fxhash::HashMap;

/// Sort a short list (column indices, column names) in place. std's sort is
/// several KB of code per element type and caller crate; an insertion sort
/// is a few hundred bytes, and as fast at these lengths. Not for data.
pub(crate) fn sort_small<T: Ord>(v: &mut [T]) {
    for i in 1..v.len() {
        let mut j = i;
        while j > 0 && v[j - 1] > v[j] {
            v.swap(j - 1, j);
            j -= 1;
        }
    }
}

#[cfg(any(feature = "search", feature = "write"))]
thread_local! {
    /// Bumped whenever a table's word index comes or goes (`search_generation`).
    static SEARCH_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Changes whenever any table's word index is installed, replaced or
/// removed: part of the key of anything cached from query results, so a
/// result search() gave before is not served after.
#[cfg(feature = "write")]
pub(crate) fn search_generation() -> u64 {
    SEARCH_GENERATION.with(|g| g.get())
}

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
    /// The word index search() uses when its columns match (search_index.rs).
    #[cfg(feature = "search")]
    search_index: Option<std::rc::Rc<search_index::SearchIndex>>,
    /// A word index being built a row group at a time; search() scans
    /// until it is finished.
    #[cfg(feature = "search")]
    search_builder: Option<search_index::IndexBuilder>,
    /// columns read from `src`: the first image's; columns after them come
    /// from attached images (`attach`)
    base_cols: usize,
    attached: Vec<Attached<S>>,
}

/// An image whose columns were appended to a table's (`Table::attach`).
struct Attached<S: ReadAt> {
    src: S,
    /// its own catalog: segment offsets are within its file
    cat: Catalog,
    /// the table's index of its first column
    first: usize,
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
        // a packed image held in memory is unpacked; one that is not in
        // memory (OPFS) has to be stored unpacked
        if let Some(wire) = src.read_ref(0, src.len() as usize).filter(|b| format::packed::is_packed(b)) {
            let image = format::packed::unpack(wire)?;
            let src = S::from_memory(image).ok_or(FormatError::Io("a packed image must be unpacked before it is opened here"))?;
            let cat = read::open(&src)?;
            return Ok(Self::from_catalog(src, cat));
        }
        let cat = read::open(&src)?;
        Ok(Self::from_catalog(src, cat))
    }

    /// A table over `src` described by `cat` (from `read::open`, or a
    /// catalog still growing while the image arrives: see `load`).
    fn from_catalog(src: S, cat: Catalog) -> Self {
        let base_cols = cat.schema.columns.len();
        let dict_cache = cat.schema.columns.iter().map(|_| None).collect();
        let dict_rc_cache = cat.schema.columns.iter().map(|_| None).collect();
        Self {
            src,
            cat,
            cache: HashMap::default(),
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
            #[cfg(feature = "search")]
            search_index: None,
            #[cfg(feature = "search")]
            search_builder: None,
            base_cols,
            attached: Vec::new(),
        }
    }

    /// Append the columns of `other`, the other part of a split image
    /// (`format::split`), to this table: same split id, same row groups.
    /// Column indices of this table stay as they were, so cached masks stay
    /// valid; derived tables are dropped (a `select *` among them would miss
    /// the new columns). The two word indexes, if built, become one over
    /// both images' columns (a build under way is finished first).
    pub fn attach(&mut self, other: Table<S>) -> Result<(), FormatError> {
        let (a, b) = (&self.cat, &other.cat);
        if a.split_id.is_none() || a.split_id != b.split_id {
            return Err(FormatError::Corrupt("attach: the images are not parts of one split"));
        }
        if a.groups.len() != b.groups.len() || a.groups.iter().zip(&b.groups).any(|(x, y)| x.row_count != y.row_count) {
            return Err(FormatError::Corrupt("attach: the images' row groups differ"));
        }
        if b.schema.columns.iter().any(|c| a.schema.columns.iter().any(|d| d.name == c.name)) {
            return Err(FormatError::Corrupt("attach: a column name is in both images"));
        }
        if !other.attached.is_empty() {
            return Err(FormatError::Corrupt("attach: the image to attach has images attached"));
        }
        let first = self.cat.schema.columns.len();
        #[cfg(feature = "search")]
        let mut other = other;
        #[cfg(feature = "search")]
        let index = {
            self.search_index_step(usize::MAX)?;
            other.search_index_step(usize::MAX)?;
            match (self.search_index.take(), other.search_index.take()) {
                (a, None) => a,
                (a, Some(b)) => {
                    let a = a.unwrap_or_else(|| std::rc::Rc::new(search_index::SearchIndex::empty(self.group_count())));
                    Some(std::rc::Rc::new(search_index::SearchIndex::merge(&a, &b, first)))
                }
            }
        };
        let Table { src, cat, .. } = other;
        self.cat.schema.columns.extend(cat.schema.columns.iter().cloned());
        self.cat.dicts.extend(cat.dicts.iter().copied());
        self.cat.clustered.extend(cat.clustered.iter().copied());
        for (g, h) in self.cat.groups.iter_mut().zip(&cat.groups) {
            g.cols.extend(h.cols.iter().cloned());
        }
        self.dict_cache.extend(cat.schema.columns.iter().map(|_| None));
        self.dict_rc_cache.extend(cat.schema.columns.iter().map(|_| None));
        self.derived.clear();
        self.derived_bytes = 0;
        self.attached.push(Attached { src, cat, first });
        #[cfg(feature = "search")]
        {
            self.set_search_index(index);
        }
        Ok(())
    }

    /// Where segment `seg` of (`group`, `col`) is: its source and offset.
    fn locate(&self, group: usize, col: usize, seg: usize) -> (&S, u64) {
        match self.attached.iter().rev().find(|a| col >= a.first) {
            None => (&self.src, read::segment_offset_in(&self.cat.groups[group], self.base_cols, col, seg)),
            Some(a) => (&a.src, read::segment_offset(&a.cat, &a.cat.groups[group], col - a.first, seg)),
        }
    }

    /// Build the word index over `cols` (text columns) and keep it: search()
    /// over exactly these columns then looks words up instead of scanning.
    /// In one go; `begin_search_index` and `search_index_step` do the same a
    /// row group at a time.
    #[cfg(feature = "search")]
    pub fn build_search_index(&mut self, cols: &[usize]) -> Result<search_index::IndexStats, FormatError> {
        self.begin_search_index(cols)?;
        self.search_index_step(usize::MAX)?;
        Ok(self.search_index.as_ref().expect("finished").stats())
    }

    /// Start building the word index over `cols`, replacing any index or
    /// build in progress. The dictionary columns are read here.
    #[cfg(feature = "search")]
    pub fn begin_search_index(&mut self, cols: &[usize]) -> Result<(), FormatError> {
        // a request that cannot be built leaves the index there is alone
        search_index::check_table(&self.cat, cols)?;
        self.set_search_index(None); // its memory back before the new one is built
        self.search_builder = None;
        self.search_builder = Some(search_index::IndexBuilder::new(self, cols)?);
        Ok(())
    }

    /// Index up to `groups` more row groups of the build in progress. The
    /// index takes over when the last group is done. Returns the row groups
    /// still to index (0 when finished, or when no build is in progress).
    #[cfg(feature = "search")]
    pub fn search_index_step(&mut self, groups: usize) -> Result<usize, FormatError> {
        let Some(mut b) = self.search_builder.take() else { return Ok(0) };
        let total = self.group_count();
        let mut n = 0;
        while n < groups && b.groups_done() < total {
            if let Err(e) = b.add_group(self) {
                return Err(e); // the build is dropped; search() keeps scanning
            }
            n += 1;
        }
        let left = total - b.groups_done();
        if left == 0 {
            self.set_search_index(Some(std::rc::Rc::new(b.finish())));
        } else {
            self.search_builder = Some(b);
        }
        Ok(left)
    }

    /// The word index as bytes, to keep (in OPFS, say) and give back to
    /// `load_search_index` on a later visit instead of building it again.
    #[cfg(feature = "search")]
    pub fn save_search_index(&self) -> Option<Vec<u8>> {
        self.search_index.as_ref().map(|i| i.to_bytes(self.fingerprint()))
    }

    /// Use a word index over `cols` saved by `save_search_index`. Refused
    /// (and nothing changes) unless it was saved by this build for this very
    /// table over these columns, and is whole: then build it instead.
    #[cfg(feature = "search")]
    pub fn load_search_index(&mut self, bytes: &[u8], cols: &[usize]) -> Result<(), &'static str> {
        let rows: Vec<usize> = self.cat.groups.iter().map(|g| g.row_count as usize).collect();
        let mut dict_len = vec![None; self.cat.schema.columns.len()];
        for (c, len) in dict_len.iter_mut().enumerate() {
            let def = &self.cat.schema.columns[c];
            if def.ty == ColumnType::Utf8 && def.is_dict() {
                *len = Some(self.dictionary_rc(c).map_err(|_| "could not read a dictionary")?.len());
            }
        }
        let kind = |c: usize| match (self.cat.schema.columns.get(c), dict_len.get(c)) {
            (Some(def), Some(None)) if def.ty == ColumnType::Utf8 => search_index::ColKind::Plain,
            (Some(_), Some(Some(n))) => search_index::ColKind::Dict(*n),
            _ => search_index::ColKind::Other,
        };
        let idx = search_index::SearchIndex::from_bytes(bytes, self.fingerprint(), &rows, kind)?;
        let mut want = cols.to_vec();
        sort_small(&mut want);
        want.dedup();
        if idx.cols != want {
            return Err("saved over other columns");
        }
        self.search_builder = None;
        self.set_search_index(Some(std::rc::Rc::new(idx)));
        Ok(())
    }

    /// Identifies the table's contents for caches kept outside it (a saved
    /// word index): a hash of its schema and of every row group's layout and
    /// statistics, the image length and split id, attached parts included.
    /// Images that differ only where no length or statistic changes would
    /// share one, so a cache should also be keyed by the data's version.
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
            }
        };
        let cats = std::iter::once(&self.cat).chain(self.attached.iter().map(|a| &a.cat));
        for cat in cats {
            eat(&cat.row_group_target.to_le_bytes());
            eat(&cat.total_rows.to_le_bytes());
            eat(&cat.image_len.unwrap_or(0).to_le_bytes());
            eat(&cat.split_id.unwrap_or(0).to_le_bytes());
            for c in &cat.schema.columns {
                eat(c.name.as_bytes());
                eat(&[c.ty as u8]);
                eat(&c.flags.to_le_bytes());
            }
            for g in &cat.groups {
                eat(&g.offset.to_le_bytes());
                eat(&g.row_count.to_le_bytes());
                for c in &g.cols {
                    eat(&c.null_count.to_le_bytes());
                    c.seg_lens.iter().for_each(|l| eat(&l.to_le_bytes()));
                    match c.stats {
                        format::Stats::None => eat(&[0]),
                        format::Stats::Int { min, max } => eat(&[&[1u8][..], &min.to_le_bytes(), &max.to_le_bytes()].concat()),
                        format::Stats::Float { min, max } => eat(&[&[2u8][..], &min.to_bits().to_le_bytes(), &max.to_bits().to_le_bytes()].concat()),
                    }
                }
            }
        }
        h
    }

    /// Drop the word index; search() scans again.
    #[cfg(feature = "search")]
    pub fn drop_search_index(&mut self) {
        self.set_search_index(None);
        self.search_builder = None;
    }

    /// Install (or remove) the word index. search() answers differently with
    /// and without it (corrections, inflections), so anything cached from a
    /// search() result goes: this table's derived tables now, and every
    /// table's derived tables through the generation in their keys (one may
    /// hold a result read from this table).
    #[cfg(feature = "search")]
    fn set_search_index(&mut self, idx: Option<std::rc::Rc<search_index::SearchIndex>>) {
        if self.search_index.is_none() && idx.is_none() {
            return;
        }
        self.search_index = idx;
        self.derived.clear();
        self.derived_bytes = 0;
        // a search inside OR, NOT or a comparison is cached under the whole
        // condition, whose answer the index changes (typos, word forms): the
        // index arrives about once a load, so drop every cached mask
        self.masks.clear();
        SEARCH_GENERATION.with(|g| g.set(g.get() + 1));
    }

    #[cfg(feature = "search")]
    pub fn search_index(&self) -> Option<std::rc::Rc<search_index::SearchIndex>> {
        self.search_index.clone()
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

    /// The keys of every cached derived table (for superset matching).
    pub fn derived_keys(&self) -> Vec<String> {
        self.derived.iter().map(|d| d.key.clone()).collect()
    }

    /// Remove a derived table from the cache, handing it over — so it can be
    /// joined with this table (or another derived one) without two mutable
    /// borrows into the cache. Put it back with `derived_put`.
    pub fn derived_take(&mut self, key: &str) -> Option<(Table<S>, usize)> {
        let i = self.derived.iter().position(|d| d.key == key)?;
        let d = self.derived.swap_remove(i);
        self.derived_bytes -= d.bytes;
        Some((d.table, d.bytes))
    }

    pub fn derived_put(&mut self, key: &str, table: Table<S>, bytes: usize) {
        self.derived_tick += 1;
        self.derived_bytes += bytes;
        self.derived.push(Derived { key: key.to_string(), table, bytes, last_used: self.derived_tick });
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
        let len = self.cat.groups[group].cols[col].seg_lens[seg] as usize;
        let (src, off) = self.locate(group, col, seg);
        src.read_ref(off, len).or_else(|| {
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
        let len = self.cat.groups[group].cols[col].seg_lens[seg] as usize;
        {
            let (src, off) = self.locate(group, col, seg);
            if src.read_ref(off, len).is_some() {
                let (src, off) = self.locate(group, col, seg);
                return Ok(src.read_ref(off, len).unwrap());
            }
        }
        let key = (group as u32, col as u32, seg as u8);
        self.tick += 1;
        if !self.cache.contains_key(&key) {
            let (src, off) = self.locate(group, col, seg);
            let mut bytes = vec![0u8; len];
            src.read_at(off, &mut bytes)?;
            self.evict_to_budget(bytes.len());
            self.cache_bytes += bytes.len();
            self.cache.insert(key, SegEntry { data: bytes, last_used: self.tick });
        }
        let e = self.cache.get_mut(&key).unwrap();
        e.last_used = self.tick;
        Ok(&e.data)
    }

    /// Run `f` over several plain-text columns' raw segments for one group at
    /// once — per column `(offsets as LE u32 bytes, blob, validity)` —
    /// borrowed, no copy, so a caller can walk row by row across columns.
    /// `Ok(None)` when they could not all be held resident at once.
    #[cfg(feature = "search")]
    pub(crate) fn with_text_columns<R>(
        &mut self,
        group: usize,
        cols: &[usize],
        f: impl FnOnce(&[(&[u8], &[u8], Option<&[u8]>)]) -> R,
    ) -> Result<Option<R>, FormatError> {
        for &c in cols {
            self.segment(group, c, 0)?;
            self.segment(group, c, 1)?;
            if self.cat.groups[group].cols[c].null_count != 0 {
                self.segment(group, c, 2)?;
            }
        }
        let mut segs = Vec::with_capacity(cols.len());
        for &c in cols {
            let (Some(offs), Some(blob)) = (self.resident(group, c, 0), self.resident(group, c, 1)) else {
                return Ok(None);
            };
            let valid = if self.cat.groups[group].cols[c].null_count != 0 {
                match self.resident(group, c, 2) {
                    Some(v) => Some(v),
                    None => return Ok(None),
                }
            } else {
                None
            };
            segs.push((offs, blob, valid));
        }
        Ok(Some(f(&segs)))
    }

    /// Run `f` over a dictionary column's raw code segment for one group —
    /// `(codes as LE bytes, code width, validity)` — borrowed, no copy. For
    /// the search index's per-word lookups. `Ok(None)` when the segments
    /// could not both be held resident; callers fall back to `codes`.
    #[cfg(feature = "search")]
    pub(crate) fn with_code_segments<R>(
        &mut self,
        group: usize,
        col: usize,
        f: impl FnOnce(&[u8], usize, Option<&[u8]>) -> R,
    ) -> Result<Option<R>, FormatError> {
        debug_assert!(self.cat.schema.columns[col].is_dict());
        let width = self.cat.schema.columns[col].code_width();
        let has_nulls = self.cat.groups[group].cols[col].null_count != 0;
        self.segment(group, col, 0)?;
        if has_nulls {
            self.segment(group, col, 2)?;
        }
        let Some(codes) = self.resident(group, col, 0) else {
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
        Ok(Some(f(codes, width, valid)))
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
            // the merged catalog's dictionary locations are within each
            // column's own image
            let src = self.attached.iter().rev().find(|a| col >= a.first).map_or(&self.src, |a| &a.src);
            let d = read::read_dictionary(src, &self.cat, col)?;
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
