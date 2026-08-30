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
