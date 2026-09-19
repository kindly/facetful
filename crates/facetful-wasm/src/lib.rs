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

// The module's ONE import: a synchronous positional read served by the JS
// worker over an OPFS sync access handle. Offset travels as f64 (exact to
// 2^53) to keep BigInt out of the boundary.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    fn opfs_read(file_id: u32, offset: f64, len: u32, dest: *mut u8) -> i32;
    /// The second import (design.sv d49): evaluate user-defined function `id`
    /// over `len` rows. `args` points at `argc` lane descriptors and `out` at
    /// one more for the result (layout: `udf::DESC_WORDS` u32s each, see
    /// `WasmHost`). Returns 0, or -n with an n-byte error message the JS side
    /// allocated (its pointer in the out descriptor's ERR field).
    fn udf_call(id: u32, argc: u32, args: *const u32, out: *mut u32, len: u32) -> i32;
}

/// Native builds (tests, CLI linkage) never call these; stubs keep them linking.
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
unsafe fn opfs_read(_file_id: u32, _offset: f64, _len: u32, _dest: *mut u8) -> i32 {
    -1
}
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
unsafe fn udf_call(_id: u32, _argc: u32, _args: *const u32, _out: *mut u32, _len: u32) -> i32 {
    -1
}

// ---------------- user-defined functions ----------------
//
// Lane descriptor, 8 u32 words:
//   0 KIND   udf::Kind as u8        4 AUX    text: u32 offsets ptr (len+1)
//   1 LEN    values in the lane     5 VALID  validity bitmap ptr, 0 = all valid
//   2 FLAGS  bit0 = broadcast       6 BYTES  text: byte length of DATA
//   3 DATA   f64 lane / u8 bools /  7 ERR    out only: JS-allocated error text
//            text bytes ptr
// Numbers (ints, days, ms) travel as f64, like the rest of the boundary. For a
// text OUTPUT the engine allocates the offsets; JS allocates the bytes with
// `alloc`, writes their ptr in DATA and length in BYTES, and the host frees it.
const DESC_WORDS: usize = 8;

struct WasmHost;

impl facetful_engine::udf::Host for WasmHost {
    fn call(
        &mut self,
        id: u32,
        args: &[facetful_engine::udf::Arg],
        len: usize,
        out: &mut facetful_engine::udf::Output,
    ) -> Result<(), String> {
        use facetful_engine::udf::{Lane, Out};
        let mut desc = vec![0u32; (args.len() + 1) * DESC_WORDS];
        for (i, a) in args.iter().enumerate() {
            let d = &mut desc[i * DESC_WORDS..(i + 1) * DESC_WORDS];
            d[0] = a.kind as u32;
            d[2] = a.broadcast as u32;
            match &a.lane {
                Lane::Num(v) => {
                    d[1] = v.len() as u32;
                    d[3] = v.as_ptr() as u32;
                }
                Lane::Bool(v) => {
                    d[1] = v.len() as u32;
                    d[3] = v.as_ptr() as u32;
                }
                Lane::Text { offsets, bytes } => {
                    d[1] = (offsets.len() - 1) as u32;
                    d[3] = bytes.as_ptr() as u32;
                    d[4] = offsets.as_ptr() as u32;
                    d[6] = bytes.len() as u32;
                }
            }
            d[5] = a.valid.map_or(0, |v| v.as_ptr() as u32);
        }
        let o = args.len() * DESC_WORDS;
        desc[o] = out.kind as u32;
        desc[o + 1] = len as u32;
        desc[o + 5] = out.valid.as_ptr() as u32;
        match &mut out.out {
            Out::Num(v) => desc[o + 3] = v.as_mut_ptr() as u32,
            Out::Bool(v) => desc[o + 3] = v.as_mut_ptr() as u32,
            Out::Text { offsets, .. } => desc[o + 4] = offsets.as_mut_ptr() as u32,
        }
        let rc = unsafe { udf_call(id, args.len() as u32, desc.as_ptr(), desc[o..].as_mut_ptr(), len as u32) };
        if rc < 0 {
            let n = (-rc) as usize;
            let p = desc[o + 7] as *mut u8;
            let msg = if p.is_null() || n == 0 {
                "user function failed".to_string()
            } else {
                let m = unsafe { String::from_utf8_lossy(core::slice::from_raw_parts(p, n)).into_owned() };
                dealloc(p, n);
                m
            };
            return Err(msg);
        }
        if let Out::Text { bytes, .. } = &mut out.out {
            let (p, n) = (desc[o + 3] as *mut u8, desc[o + 6] as usize);
            if !p.is_null() && n > 0 {
                bytes.extend_from_slice(unsafe { core::slice::from_raw_parts(p, n) });
                dealloc(p, n);
            }
        }
        Ok(())
    }
}

static mut UDF_ERROR: Option<String> = None;

/// Declare a user-defined function: `params` is `argc` kind bytes, `ret` a
/// kind, `flags` bit0 = strict (NULL in → NULL out), bit1 = variadic. Returns
/// the function id (≥ 1), or 0 with the reason in `udf_error`.
#[no_mangle]
pub extern "C" fn udf_register(
    name_ptr: *const u8,
    name_len: usize,
    params_ptr: *const u8,
    argc: usize,
    ret: u32,
    flags: u32,
) -> u32 {
    use facetful_engine::udf::{self, Kind};
    let name = unsafe { core::slice::from_raw_parts(name_ptr, name_len) };
    let params = unsafe { core::slice::from_raw_parts(params_ptr, argc) };
    let result = (|| {
        let name = core::str::from_utf8(name).map_err(|_| "name is not UTF-8".to_string())?;
        let params: Vec<_> = params
            .iter()
            .map(|&k| Kind::from_u8(k).map(|k| k.ty()).ok_or_else(|| format!("unknown parameter kind {k}")))
            .collect::<Result<_, _>>()?;
        let ret = Kind::from_u8(ret as u8).ok_or_else(|| format!("unknown return kind {ret}"))?.ty();
        udf::set_host(Box::new(WasmHost));
        udf::register(name, &params, ret, flags & 1 != 0, flags & 2 != 0)
    })();
    match result {
        Ok(id) => id,
        Err(e) => {
            unsafe { *core::ptr::addr_of_mut!(UDF_ERROR) = Some(e) };
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn udf_unregister(name_ptr: *const u8, name_len: usize) -> u32 {
    let name = unsafe { core::slice::from_raw_parts(name_ptr, name_len) };
    core::str::from_utf8(name).is_ok_and(facetful_engine::udf::unregister) as u32
}

/// The last `udf_register` failure, copied into `out` (returns bytes written).
#[no_mangle]
pub extern "C" fn udf_error(out: *mut u8, cap: usize) -> u32 {
    let msg = unsafe { (*core::ptr::addr_of_mut!(UDF_ERROR)).take() }.unwrap_or_default();
    let n = msg.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(msg.as_ptr(), out, n) };
    n as u32
}

pub enum Src {
    Mem(Vec<u8>),
    Opfs { file_id: u32, len: u64 },
}

impl ReadAt for Src {
    fn from_memory(bytes: Vec<u8>) -> Option<Self> {
        Some(Src::Mem(bytes))
    }
    fn len(&self) -> u64 {
        match self {
            Src::Mem(v) => v.len() as u64,
            Src::Opfs { len, .. } => *len,
        }
    }
    fn read_ref(&self, offset: u64, len: usize) -> Option<&[u8]> {
        match self {
            Src::Mem(v) => v.read_ref(offset, len),
            Src::Opfs { .. } => None,
        }
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FormatError> {
        match self {
            Src::Mem(v) => v.as_slice().read_at(offset, buf),
            Src::Opfs { file_id, .. } => {
                let n = unsafe {
                    opfs_read(*file_id, offset as f64, buf.len() as u32, buf.as_mut_ptr())
                };
                if n as usize == buf.len() {
                    Ok(())
                } else {
                    Err(FormatError::Io("opfs read failed or short"))
                }
            }
        }
    }
}

type T = Table<Src>;

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

/// Free a buffer from `alloc` (the compile path marshals whole columns in,
/// too much to leak like the small argument scratch buffers).
#[no_mangle]
pub extern "C" fn dealloc(p: *mut u8, n: usize) {
    drop(unsafe { Vec::from_raw_parts(p, 0, n) });
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
    match Table::open(Src::Mem(bytes)) {
        Ok(t) => Box::into_raw(Box::new(t)) as usize,
        Err(_) => 0,
    }
}

/// Open a table backed by an OPFS file the JS side registered under `file_id`.
/// Metadata (header, dictionaries, footer) is read through `opfs_read`; column
/// segments load on demand into the byte-budgeted LRU cache.
#[no_mangle]
pub extern "C" fn table_open_opfs(file_id: u32, file_len: f64, cache_budget: f64) -> usize {
    match Table::open(Src::Opfs { file_id, len: file_len as u64 }) {
        Ok(mut t) => {
            if cache_budget > 0.0 {
                t.set_cache_budget(cache_budget as usize);
            }
            Box::into_raw(Box::new(t)) as usize
        }
        Err(_) => 0,
    }
}

/// Pre-touch a column's segments (background warming). Returns bytes read.
#[no_mangle]
pub extern "C" fn table_warm(t: usize, col: u32) -> f64 {
    let t = unsafe { &mut *(t as *mut T) };
    t.warm_column(col as usize).map(|b| b as f64).unwrap_or(-1.0)
}

/// Filter-mask cache byte budget for this table; 0 disables the cache.
#[no_mangle]
pub extern "C" fn table_set_mask_budget(t: usize, bytes: f64) {
    let t = unsafe { &mut *(t as *mut T) };
    t.masks().set_budget(bytes as usize);
}

/// (cached segments << 32) | cached KiB — cache observability for the JS side.
/// Byte budget for derived tables (materialized CTEs / subqueries) cached on
/// this table; 0 disables caching.
#[no_mangle]
pub extern "C" fn table_set_derived_budget(t: usize, bytes: f64) {
    let t = unsafe { &mut *(t as *mut T) };
    t.set_derived_budget(bytes as usize);
}

#[no_mangle]
pub extern "C" fn table_cache_stats(t: usize) -> u64 {
    let t = unsafe { &*(t as *const T) };
    let (n, bytes) = t.cache_stats();
    ((n as u64) << 32) | (bytes as u64 / 1024)
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
use facetful_engine::sql::{run_query_with, Catalog};

/// The worker's registry of loaded tables, mirrored here so a query can name
/// them in FROM / JOIN: (name, handle, identity). Identity increments per
/// registration, so a table reloaded under the same name is a different key.
static mut CATALOG: Vec<(String, usize, u64)> = Vec::new();
static mut NEXT_ID: u64 = 1;

/// Register (or re-register) `handle` under `name`.
#[no_mangle]
pub extern "C" fn catalog_register(name_ptr: *const u8, name_len: usize, handle: usize) {
    let name = unsafe { core::slice::from_raw_parts(name_ptr, name_len) };
    let Ok(name) = core::str::from_utf8(name) else { return };
    unsafe {
        let cat = &mut *core::ptr::addr_of_mut!(CATALOG);
        cat.retain(|(n, _, _)| n != name);
        let id = NEXT_ID;
        NEXT_ID += 1;
        cat.push((name.to_string(), handle, id));
    }
}

/// Forget every name bound to `handle` (before the table is freed).
#[no_mangle]
pub extern "C" fn catalog_unregister(handle: usize) {
    unsafe {
        let cat = &mut *core::ptr::addr_of_mut!(CATALOG);
        cat.retain(|(_, h, _)| *h != handle);
    }
}

/// The registry as the running query sees it: every table but its own.
struct WasmCatalog {
    base: usize,
}

impl Catalog<Src> for WasmCatalog {
    fn table(&mut self, name: &str) -> Option<(&mut T, u64)> {
        let cat = unsafe { &*core::ptr::addr_of!(CATALOG) };
        let (_, h, id) = cat.iter().find(|(n, h, _)| n == name && *h != self.base)?;
        Some((unsafe { &mut *(*h as *mut T) }, *id))
    }
    fn is_self(&self, name: &str) -> bool {
        let cat = unsafe { &*core::ptr::addr_of!(CATALOG) };
        cat.iter().any(|(n, h, _)| n == name && *h == self.base)
    }
}

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
    /// a materialized query: a compiled image, taken with `outcome_image`
    Image(Vec<u8>),
}

fn columnize(mut r: QueryResult) -> Outcome {
    // columnar channel: typed vectors move straight into ColBufs (near-memcpy)
    if let Some(out_cols) = r.cols.take() {
        use facetful_engine::sql::binder::Ty;
        use facetful_engine::sql::exec::OutCol;
        let rows = r.out_rows;
        let mut cols = Vec::with_capacity(out_cols.len());
        for ((oc, name), ty) in out_cols.into_iter().zip(&r.columns).zip(&r.col_types) {
            let kind_of = |fallback: u32| match ty {
                Ty::Date => 5,
                Ty::Timestamp => 6,
                _ => fallback,
            };
            let mut c = ColBuf {
                kind: 0,
                name: name.clone(),
                f64s: Vec::new(),
                bools: Vec::new(),
                offsets: Vec::new(),
                bytes: Vec::new(),
                validity: Vec::new(),
            };
            match oc {
                OutCol::F64 { v, valid } => {
                    c.kind = kind_of(2);
                    c.f64s = v;
                    c.validity = valid;
                }
                OutCol::I64 { v, valid } => {
                    c.kind = kind_of(1);
                    c.f64s = v.into_iter().map(|x| x as f64).collect();
                    c.validity = valid;
                }
                OutCol::Bool { v, valid } => {
                    c.kind = 3;
                    c.bools = v;
                    c.validity = valid;
                }
                OutCol::Text { offsets, bytes, valid } => {
                    c.kind = 4;
                    c.offsets = offsets;
                    c.bytes = bytes;
                    c.validity = valid;
                }
            }
            cols.push(c);
        }
        return Outcome::Ok { cols, rows, scanned: 0, total: 0 };
    }
    let rows = r.rows.len();
    let mut cols = Vec::with_capacity(r.columns.len());
    for (ci, name) in r.columns.iter().enumerate() {
        // kind from the bound type; date/timestamp cross as f64 days/ms with
        // their own kinds so the JS side can materialize ISO strings
        use facetful_engine::sql::binder::Ty;
        let mut kind = match r.col_types[ci] {
            Ty::Int => 1,
            Ty::Float => 2,
            Ty::Bool => 3,
            Ty::Text => 4,
            Ty::Date => 5,
            Ty::Timestamp => 6,
            // NULL literal column: type from the first non-null value
            Ty::Null => 0,
        };
        for row in &r.rows {
            if kind != 0 {
                break;
            }
            kind = match &row[ci] {
                Val::Null => continue,
                Val::Int(_) => 1,
                Val::Float(_) => 2,
                Val::Bool(_) => 3,
                Val::Text(_) => 4,
            };
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
                1 | 2 | 5 | 6 => c.f64s.push(match v {
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
    let t_handle = t;
    let t = unsafe { &mut *(t as *mut T) };
    let sql = unsafe { core::slice::from_raw_parts(sql_ptr, sql_len) };
    let outcome = match core::str::from_utf8(sql) {
        Err(_) => Outcome::Err("query is not valid UTF-8".into()),
        Ok(sql) => match run_query_with(t, sql, &mut WasmCatalog { base: t_handle }) {
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

/// Materialize `sql` over table `t` into a compiled image. The outcome is
/// `Image` on success (take it with `outcome_image`, then `image_open_table`
/// or `image_ptr`/`image_len`) or `Err` with the rendered diagnostic.
#[no_mangle]
pub extern "C" fn table_materialize(
    t: usize,
    sql_ptr: *const u8,
    sql_len: usize,
    group_target: u32,
) -> usize {
    let t_handle = t;
    let t = unsafe { &mut *(t as *mut T) };
    let sql = unsafe { core::slice::from_raw_parts(sql_ptr, sql_len) };
    let outcome = match core::str::from_utf8(sql) {
        Err(_) => Outcome::Err("query is not valid UTF-8".into()),
        Ok(sql) => match facetful_engine::materialize::materialize_with(
            t,
            sql,
            group_target,
            &mut WasmCatalog { base: t_handle },
        ) {
            Ok(image) => Outcome::Image(image),
            Err(d) => Outcome::Err(d.render(sql)),
        },
    };
    Box::into_raw(Box::new(outcome)) as usize
}

/// Move a materialized image out of its outcome as an image handle
/// (0 when the outcome is not an image).
#[no_mangle]
pub extern "C" fn outcome_image(h: usize) -> usize {
    let o = unsafe { &mut *(h as *mut Outcome) };
    match o {
        Outcome::Image(img) => Box::into_raw(Box::new(core::mem::take(img))) as usize,
        _ => 0,
    }
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

// ---------------- baseline compiler (Parquet transcode path) ----------------
// JS reads Parquet (hyparquet in the worker), marshals raw typed columns in,
// and this compiles them into a .facetful image — the exact same code path as
// the native CLI's convert, so browser-built and CLI-built images can't drift.

use facetful_engine::format::compile::{self, InCol};

pub struct CompileBuilder {
    names: Vec<String>,
    cols: Vec<InCol>,
    rows: usize,
}

#[no_mangle]
pub extern "C" fn compile_begin(rows: u32) -> usize {
    Box::into_raw(Box::new(CompileBuilder {
        names: Vec::new(),
        cols: Vec::new(),
        rows: rows as usize,
    })) as usize
}

fn read_str(ptr: *const u8, len: usize) -> String {
    let s = unsafe { core::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(s).into_owned()
}

/// validity: pointer to one byte per row (0 = null), or 0 for no nulls.
fn read_validity(ptr: *const u8, rows: usize) -> Option<Vec<bool>> {
    if ptr.is_null() {
        return None;
    }
    let s = unsafe { core::slice::from_raw_parts(ptr, rows) };
    if s.iter().all(|&b| b != 0) {
        None
    } else {
        Some(s.iter().map(|&b| b != 0).collect())
    }
}

/// Numeric column from f64 lanes. `kind`: 0 = float, 1 = int (narrowed to the
/// smallest type), 2 = date (days since epoch), 3 = timestamp (ms since epoch).
/// (Int64/timestamp values beyond 2^53 lose precision crossing this f64
/// boundary — acceptable for the baseline profile; the native CLI has no such
/// limit.)
#[no_mangle]
pub extern "C" fn compile_add_num(
    b: usize,
    name_ptr: *const u8,
    name_len: usize,
    data: *const f64,
    validity: *const u8,
    kind: u32,
) {
    let b = unsafe { &mut *(b as *mut CompileBuilder) };
    let lanes = unsafe { core::slice::from_raw_parts(data, b.rows) };
    b.names.push(read_str(name_ptr, name_len));
    let valid = read_validity(validity, b.rows);
    b.cols.push(match kind {
        1 => InCol::Int { v: lanes.iter().map(|&x| x as i64).collect(), valid },
        2 => InCol::Date { v: lanes.iter().map(|&x| x as i32).collect(), valid },
        3 => InCol::Timestamp { v: lanes.iter().map(|&x| x as i64).collect(), valid },
        _ => InCol::Float { v: lanes.to_vec(), valid },
    });
}

/// Text column as offsets (rows+1 u32s) + utf-8 blob; dict-vs-plain decided here.
#[no_mangle]
pub extern "C" fn compile_add_text(
    b: usize,
    name_ptr: *const u8,
    name_len: usize,
    offsets: *const u32,
    bytes: *const u8,
    bytes_len: usize,
    validity: *const u8,
) {
    let b = unsafe { &mut *(b as *mut CompileBuilder) };
    let offs = unsafe { core::slice::from_raw_parts(offsets, b.rows + 1) };
    let blob = unsafe { core::slice::from_raw_parts(bytes, bytes_len) };
    b.names.push(read_str(name_ptr, name_len));
    let v = (0..b.rows)
        .map(|i| {
            String::from_utf8_lossy(&blob[offs[i] as usize..offs[i + 1] as usize]).into_owned()
        })
        .collect();
    b.cols.push(InCol::Text { v, valid: read_validity(validity, b.rows) });
}

/// Consume the builder, compile, return an image handle (0 = failure).
#[no_mangle]
pub extern "C" fn compile_finish(b: usize, group_target: u32) -> usize {
    let b = unsafe { Box::from_raw(b as *mut CompileBuilder) };
    match compile::compile(&b.names, b.cols, group_target) {
        Ok((image, _)) => Box::into_raw(Box::new(image)) as usize,
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "C" fn image_ptr(h: usize) -> *const u8 {
    unsafe { &*(h as *const Vec<u8>) }.as_ptr()
}

#[no_mangle]
pub extern "C" fn image_len(h: usize) -> usize {
    unsafe { &*(h as *const Vec<u8>) }.len()
}

/// Open a table directly over a compiled image, consuming the handle
/// (no copy back out through JS just to load it again).
#[no_mangle]
pub extern "C" fn image_open_table(h: usize) -> usize {
    let image = *unsafe { Box::from_raw(h as *mut Vec<u8>) };
    match Table::open(Src::Mem(image)) {
        Ok(t) => Box::into_raw(Box::new(t)) as usize,
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "C" fn image_free(h: usize) {
    drop(unsafe { Box::from_raw(h as *mut Vec<u8>) });
}

// ---------------- streaming CSV conversion ----------------
//
// The sans-I/O converter (facetful_format::stream, design.sv d51) driven from
// JS: feed chunks for pass 1, `convert_pass2`, feed the same bytes again,
// drain finished groups with `convert_output_*` after each feed, `convert_finish`.
// Memory stays bounded whatever the file size; the JS side owns the reading
// (fs in Node, File.stream() in the browser) and the sink.

enum Phase {
    Sniff { sniffer: Option<facetful_engine::format::stream::Sniffer> },
    Encode { enc: facetful_engine::format::stream::Encoder, header_seen: bool },
    Done,
}

pub struct Converter {
    reader: facetful_engine::format::stream::CsvReader,
    phase: Phase,
    group_target: u32,
    pending: Vec<u8>,
    error: Option<String>,
    rows: u64,
    schema: Option<facetful_engine::format::Schema>,
}

#[no_mangle]
pub extern "C" fn convert_begin(group_target: u32) -> usize {
    Box::into_raw(Box::new(Converter {
        reader: facetful_engine::format::stream::CsvReader::new(),
        phase: Phase::Sniff { sniffer: None },
        group_target,
        pending: Vec::new(),
        error: None,
        rows: 0,
        schema: None,
    })) as usize
}

fn converter(h: usize) -> &'static mut Converter {
    unsafe { &mut *(h as *mut Converter) }
}

impl Converter {
    fn feed(&mut self, bytes: &[u8], end: bool) {
        if self.error.is_some() {
            return;
        }
        let Converter { reader, phase, error, .. } = self;
        let mut on_row = |row: &[String]| {
            if error.is_some() {
                return;
            }
            match phase {
                Phase::Sniff { sniffer } => match sniffer {
                    None => *sniffer = Some(facetful_engine::format::stream::Sniffer::new(row.to_vec())),
                    Some(s) => {
                        if let Err(e) = s.row(row) {
                            *error = Some(e);
                        }
                    }
                },
                Phase::Encode { enc, header_seen } => {
                    if !*header_seen {
                        *header_seen = true;
                        return;
                    }
                    if let Err(e) = enc.row(row) {
                        *error = Some(e);
                    }
                }
                Phase::Done => *error = Some("conversion already finished".into()),
            }
        };
        if end {
            reader.finish(&mut on_row);
        } else {
            reader.push(bytes, &mut on_row);
        }
        if let Phase::Encode { enc, .. } = &mut self.phase {
            self.pending.extend(enc.take_output());
            self.rows = enc.rows();
        }
    }
}

/// Feed a chunk (pass 1 or pass 2, by phase). Returns 0, or -1 with the
/// message in `convert_error`.
#[no_mangle]
pub extern "C" fn convert_feed(h: usize, ptr: *const u8, len: usize) -> i32 {
    let c = converter(h);
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    c.feed(bytes, false);
    if c.error.is_some() { -1 } else { 0 }
}

/// End of pass 1: decide the plan, start encoding. Feed the same bytes again.
#[no_mangle]
pub extern "C" fn convert_pass2(h: usize) -> i32 {
    let c = converter(h);
    c.feed(&[], true);
    if c.error.is_some() {
        return -1;
    }
    let sniffer = match core::mem::replace(&mut c.phase, Phase::Done) {
        Phase::Sniff { sniffer: Some(s) } => s,
        Phase::Sniff { sniffer: None } => {
            c.error = Some("empty input".into());
            return -1;
        }
        _ => {
            c.error = Some("convert_pass2 called twice".into());
            return -1;
        }
    };
    match facetful_engine::format::stream::Encoder::new(sniffer.finish(), c.group_target) {
        Ok(enc) => {
            c.schema = Some(enc.schema().clone());
            c.reader = facetful_engine::format::stream::CsvReader::new();
            c.phase = Phase::Encode { enc, header_seen: false };
            0
        }
        Err(e) => {
            c.error = Some(e);
            -1
        }
    }
}

/// End of pass 2: the last group and the footer land in the output.
#[no_mangle]
pub extern "C" fn convert_finish(h: usize) -> i32 {
    let c = converter(h);
    c.feed(&[], true);
    if c.error.is_some() {
        return -1;
    }
    match core::mem::replace(&mut c.phase, Phase::Done) {
        Phase::Encode { enc, .. } => {
            c.rows = enc.rows();
            match enc.finish() {
                Ok(tail) => {
                    c.pending.extend(tail);
                    0
                }
                Err(e) => {
                    c.error = Some(e);
                    -1
                }
            }
        }
        _ => {
            c.error = Some("convert_finish before convert_pass2".into());
            -1
        }
    }
}

/// Bytes ready to be written; `convert_output_copy` copies and clears them.
#[no_mangle]
pub extern "C" fn convert_output_len(h: usize) -> u32 {
    converter(h).pending.len() as u32
}

#[no_mangle]
pub extern "C" fn convert_output_copy(h: usize, dst: *mut u8) -> u32 {
    let c = converter(h);
    let n = c.pending.len();
    unsafe { core::ptr::copy_nonoverlapping(c.pending.as_ptr(), dst, n) };
    c.pending.clear();
    n as u32
}

#[no_mangle]
pub extern "C" fn convert_rows(h: usize) -> f64 {
    converter(h).rows as f64
}

/// "name\tkind\n" per column, after `convert_pass2`.
#[no_mangle]
pub extern "C" fn convert_schema(h: usize, out: *mut u8, cap: usize) -> u32 {
    let c = converter(h);
    let Some(schema) = &c.schema else { return 0 };
    let text: String = schema
        .columns
        .iter()
        .zip(facetful_engine::format::compile::describe(schema))
        .map(|(col, kind)| format!("{}\t{kind}\n", col.name))
        .collect();
    let n = text.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(text.as_ptr(), out, n) };
    n as u32
}

#[no_mangle]
pub extern "C" fn convert_error(h: usize, out: *mut u8, cap: usize) -> u32 {
    let msg = converter(h).error.clone().unwrap_or_default();
    let n = msg.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(msg.as_ptr(), out, n) };
    n as u32
}

#[no_mangle]
pub extern "C" fn convert_free(h: usize) {
    drop(unsafe { Box::from_raw(h as *mut Converter) });
}
