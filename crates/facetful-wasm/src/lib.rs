//! Wasm surface — M1 spike protocol. Hand-rolled exports (no wasm-bindgen):
//! the JS glue speaks pointers + handles, keeping the binary inside budget.
//!
//! Protocol: JS copies the .facetful bytes into an `alloc`'d buffer, calls
//! `table_open` (which takes ownership), then per user interaction calls
//! `facet_refresh` once and reads the result through `result_*` accessors
//! (pointers into wasm memory — JS views them zero-copy, copies what it keeps,
//! then `result_free`s). One boundary call per operation, per the design doc.

use facetful_engine::format::read::ReadAt;
use facetful_engine::format::FormatError;
use facetful_engine::{FacetQuery, Table};
use std::mem;

pub struct OwnedBytes(Vec<u8>);

impl ReadAt for OwnedBytes {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FormatError> {
        let s: &[u8] = &self.0;
        s.read_at(offset, buf)
    }
}

type T = Table<OwnedBytes>;

pub struct FacetOut {
    counts: Vec<Vec<u32>>,
    mask: Vec<u8>,
    pass: u64,
    sum: f64,
}

#[no_mangle]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(n);
    let p = v.as_mut_ptr();
    mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn format_version() -> u32 {
    facetful_engine::format::VERSION as u32
}

/// Takes ownership of an `alloc`'d buffer holding a whole .facetful file.
/// Returns a table handle, or 0 on parse failure.
#[no_mangle]
pub extern "C" fn table_open(ptr: *mut u8, len: usize) -> usize {
    let bytes = unsafe { Vec::from_raw_parts(ptr, len, len) };
    match Table::open(OwnedBytes(bytes)) {
        Ok(t) => Box::into_raw(Box::new(t)) as usize,
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "C" fn table_total_rows(t: usize) -> u32 {
    let t = unsafe { &*(t as *const T) };
    t.catalog().total_rows as u32
}

/// Column index by name; -1 if absent.
#[no_mangle]
pub extern "C" fn table_col_by_name(t: usize, name_ptr: *const u8, name_len: usize) -> i32 {
    let t = unsafe { &*(t as *const T) };
    let name = unsafe { core::slice::from_raw_parts(name_ptr, name_len) };
    match core::str::from_utf8(name).ok().and_then(|n| t.column_index(n)) {
        Some(i) => i as i32,
        None => -1,
    }
}

/// Dictionary cardinality of a dict column (loads the dict on first call).
#[no_mangle]
pub extern "C" fn table_dict_len(t: usize, col: u32) -> u32 {
    let t = unsafe { &mut *(t as *mut T) };
    t.dictionary(col as usize).map(|d| d.len() as u32).unwrap_or(0)
}

/// One facet-interface refresh (multi-select: per dim a set of codes).
/// `sel_lens_ptr`: ndims u32 counts; `sel_values_ptr`: the concatenated u16
/// codes (sum of lens entries). A dim with len 0 is unfiltered.
/// Returns a result handle, or 0 on error.
#[no_mangle]
pub extern "C" fn facet_refresh(
    t: usize,
    dims_ptr: *const u32,
    ndims: usize,
    sel_lens_ptr: *const u32,
    sel_values_ptr: *const u16,
    measure: u32,
) -> usize {
    let t = unsafe { &mut *(t as *mut T) };
    let dims = unsafe { core::slice::from_raw_parts(dims_ptr, ndims) };
    let lens = unsafe { core::slice::from_raw_parts(sel_lens_ptr, ndims) };
    let total: usize = lens.iter().map(|&l| l as usize).sum();
    let values = unsafe { core::slice::from_raw_parts(sel_values_ptr, total) };
    let mut selected = Vec::with_capacity(ndims);
    let mut off = 0usize;
    for &l in lens {
        let l = l as usize;
        selected.push(values[off..off + l].to_vec());
        off += l;
    }
    let q = FacetQuery {
        dims: dims.iter().map(|&d| d as usize).collect(),
        selected,
        measure: measure as usize,
    };
    match t.facet_refresh(&q) {
        Ok(r) => Box::into_raw(Box::new(FacetOut {
            counts: r.counts,
            mask: r.mask,
            pass: r.pass_count,
            sum: r.sum,
        })) as usize,
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "C" fn result_counts_ptr(res: usize, dim: usize) -> *const u32 {
    let r = unsafe { &*(res as *const FacetOut) };
    r.counts[dim].as_ptr()
}

#[no_mangle]
pub extern "C" fn result_counts_len(res: usize, dim: usize) -> u32 {
    let r = unsafe { &*(res as *const FacetOut) };
    r.counts[dim].len() as u32
}

#[no_mangle]
pub extern "C" fn result_mask_ptr(res: usize) -> *const u8 {
    let r = unsafe { &*(res as *const FacetOut) };
    r.mask.as_ptr()
}

#[no_mangle]
pub extern "C" fn result_pass(res: usize) -> u32 {
    let r = unsafe { &*(res as *const FacetOut) };
    r.pass as u32
}

#[no_mangle]
pub extern "C" fn result_sum(res: usize) -> f64 {
    let r = unsafe { &*(res as *const FacetOut) };
    r.sum
}

#[no_mangle]
pub extern "C" fn result_free(res: usize) {
    drop(unsafe { Box::from_raw(res as *mut FacetOut) });
}

/// Top-k rows by Float64 column `by` (desc) among the result's mask rows.
/// Writes up to k u32 indices to out_ptr, returns the count written.
#[no_mangle]
pub extern "C" fn sort_topk(t: usize, res: usize, by: u32, k: u32, out_ptr: *mut u32) -> u32 {
    let t = unsafe { &mut *(t as *mut T) };
    let r = unsafe { &*(res as *const FacetOut) };
    match t.sort_topk(by as usize, &r.mask, k as usize) {
        Ok(idx) => {
            let out = unsafe { core::slice::from_raw_parts_mut(out_ptr, idx.len()) };
            out.copy_from_slice(&idx);
            idx.len() as u32
        }
        Err(_) => 0,
    }
}

/// Gather Float64 values for global row indices (table page fetch).
#[no_mangle]
pub extern "C" fn gather_f64(t: usize, col: u32, idx_ptr: *const u32, n: usize, out_ptr: *mut f64) -> u32 {
    let t = unsafe { &mut *(t as *mut T) };
    let idx = unsafe { core::slice::from_raw_parts(idx_ptr, n) };
    match t.gather(col as usize, idx) {
        Ok(facetful_engine::GatherResult::Float(v)) => {
            let out = unsafe { core::slice::from_raw_parts_mut(out_ptr, v.len()) };
            out.copy_from_slice(&v);
            v.len() as u32
        }
        _ => 0,
    }
}

/// Copy dictionary entry `code` of column `col` into out_ptr (cap bytes).
/// Returns the byte length written, or u32::MAX if out of range.
#[no_mangle]
pub extern "C" fn table_dict_value(t: usize, col: u32, code: u32, out_ptr: *mut u8, cap: usize) -> u32 {
    let t = unsafe { &mut *(t as *mut T) };
    match t.dictionary(col as usize) {
        Ok(dict) => match dict.get(code as usize) {
            Some(s) => {
                let b = s.as_bytes();
                let n = b.len().min(cap);
                unsafe { core::slice::from_raw_parts_mut(out_ptr, n) }.copy_from_slice(&b[..n]);
                n as u32
            }
            None => u32::MAX,
        },
        Err(_) => u32::MAX,
    }
}

// ---------------- M4: SQL across the boundary ----------------
//
// Results cross as column-major buffers (the round-2 decision): numbers as
// f64 (analytics-safe to 2^53), text as offsets + one UTF-8 blob, nulls as a
// validity bitmap. JS views the buffers in wasm memory, copies what it keeps
// (transferables), then frees the handle. Strings NEVER cross as JS arrays.

use facetful_engine::sql::exec::{QueryResult, Val};
use facetful_engine::sql::run_query;

pub struct ColBuf {
    kind: u32, // 1 = int (as f64), 2 = float, 3 = bool (u8), 4 = text
    name: String,
    f64s: Vec<f64>,
    bools: Vec<u8>,
    offsets: Vec<u32>,
    bytes: Vec<u8>,
    validity: Vec<u8>, // bit i set = present
}

pub enum Outcome {
    Ok { cols: Vec<ColBuf>, rows: usize, scanned: u32, total: u32 },
    Err(String),
}

fn columnize(r: QueryResult) -> Outcome {
    let rows = r.rows.len();
    let mut cols = Vec::with_capacity(r.columns.len());
    for (ci, name) in r.columns.iter().enumerate() {
        // pick kind from the first non-null value (all-null -> float)
        let mut kind = 0u32;
        for row in &r.rows {
            kind = match &row[ci] {
                Val::Null => continue,
                Val::Int(_) => 1,
                Val::Float(_) => 2,
                Val::Bool(_) => 3,
                Val::Text(_) => 4,
            };
            break;
        }
        if kind == 0 {
            kind = 2;
        }
        let mut c = ColBuf {
            kind,
            name: name.clone(),
            f64s: Vec::new(),
            bools: Vec::new(),
            offsets: Vec::new(),
            bytes: Vec::new(),
            validity: vec![0u8; (rows + 7) / 8],
        };
        if kind == 4 {
            c.offsets.push(0);
        }
        for (ri, row) in r.rows.iter().enumerate() {
            let v = &row[ci];
            if !matches!(v, Val::Null) {
                c.validity[ri / 8] |= 1 << (ri % 8);
            }
            match kind {
                1 | 2 => c.f64s.push(match v {
                    Val::Int(i) => *i as f64,
                    Val::Float(f) => *f,
                    _ => 0.0,
                }),
                3 => c.bools.push(matches!(v, Val::Bool(true)) as u8),
                _ => {
                    if let Val::Text(s) = v {
                        c.bytes.extend_from_slice(s.as_bytes());
                    }
                    c.offsets.push(c.bytes.len() as u32);
                }
            }
        }
        cols.push(c);
    }
    Outcome::Ok { cols, rows, scanned: 0, total: 0 }
}

/// Run SQL against a table handle. Always returns an Outcome handle;
/// check `outcome_is_err` before reading columns.
#[no_mangle]
pub extern "C" fn query_run(t: usize, sql_ptr: *const u8, sql_len: usize) -> usize {
    let t = unsafe { &mut *(t as *mut T) };
    let sql = unsafe { core::slice::from_raw_parts(sql_ptr, sql_len) };
    let outcome = match core::str::from_utf8(sql) {
        Err(_) => Outcome::Err("query is not valid UTF-8".into()),
        Ok(sql) => match run_query(t, sql) {
            Ok(r) => {
                let (sg, tg) = (r.scanned_groups as u32, r.total_groups as u32);
                match columnize(r) {
                    Outcome::Ok { cols, rows, .. } => {
                        Outcome::Ok { cols, rows, scanned: sg, total: tg }
                    }
                    e => e,
                }
            }
            Err(d) => Outcome::Err(d.render(sql)),
        },
    };
    Box::into_raw(Box::new(outcome)) as usize
}

fn outcome(h: usize) -> &'static Outcome {
    unsafe { &*(h as *const Outcome) }
}

#[no_mangle]
pub extern "C" fn outcome_is_err(h: usize) -> u32 {
    matches!(outcome(h), Outcome::Err(_)) as u32
}

/// Copy the rendered error into out (cap bytes); returns byte length.
#[no_mangle]
pub extern "C" fn outcome_error(h: usize, out: *mut u8, cap: usize) -> u32 {
    let Outcome::Err(e) = outcome(h) else { return 0 };
    let b = e.as_bytes();
    let n = b.len().min(cap);
    unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&b[..n]);
    n as u32
}

#[no_mangle]
pub extern "C" fn outcome_rows(h: usize) -> u32 {
    match outcome(h) {
        Outcome::Ok { rows, .. } => *rows as u32,
        _ => 0,
    }
}

#[no_mangle]
pub extern "C" fn outcome_cols(h: usize) -> u32 {
    match outcome(h) {
        Outcome::Ok { cols, .. } => cols.len() as u32,
        _ => 0,
    }
}

#[no_mangle]
pub extern "C" fn outcome_scan_stats(h: usize) -> u64 {
    match outcome(h) {
        Outcome::Ok { scanned, total, .. } => ((*total as u64) << 32) | *scanned as u64,
        _ => 0,
    }
}

fn col(h: usize, i: usize) -> Option<&'static ColBuf> {
    match outcome(h) {
        Outcome::Ok { cols, .. } => cols.get(i),
        _ => None,
    }
}

#[no_mangle]
pub extern "C" fn col_kind(h: usize, i: usize) -> u32 {
    col(h, i).map_or(0, |c| c.kind)
}

#[no_mangle]
pub extern "C" fn col_name(h: usize, i: usize, out: *mut u8, cap: usize) -> u32 {
    let Some(c) = col(h, i) else { return 0 };
    let b = c.name.as_bytes();
    let n = b.len().min(cap);
    unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&b[..n]);
    n as u32
}

#[no_mangle]
pub extern "C" fn col_f64_ptr(h: usize, i: usize) -> *const f64 {
    col(h, i).map_or(core::ptr::null(), |c| c.f64s.as_ptr())
}

#[no_mangle]
pub extern "C" fn col_bools_ptr(h: usize, i: usize) -> *const u8 {
    col(h, i).map_or(core::ptr::null(), |c| c.bools.as_ptr())
}

#[no_mangle]
pub extern "C" fn col_offsets_ptr(h: usize, i: usize) -> *const u32 {
    col(h, i).map_or(core::ptr::null(), |c| c.offsets.as_ptr())
}

#[no_mangle]
pub extern "C" fn col_bytes_ptr(h: usize, i: usize) -> *const u8 {
    col(h, i).map_or(core::ptr::null(), |c| c.bytes.as_ptr())
}

#[no_mangle]
pub extern "C" fn col_bytes_len(h: usize, i: usize) -> u32 {
    col(h, i).map_or(0, |c| c.bytes.len() as u32)
}

#[no_mangle]
pub extern "C" fn col_validity_ptr(h: usize, i: usize) -> *const u8 {
    col(h, i).map_or(core::ptr::null(), |c| c.validity.as_ptr())
}

#[no_mangle]
pub extern "C" fn outcome_free(h: usize) {
    drop(unsafe { Box::from_raw(h as *mut Outcome) });
}
