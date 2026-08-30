//! Wasm surface. Hand-rolled exports (no wasm-bindgen) — the JS glue talks a
//! small pointer-based protocol, keeping the binary inside the size budget.

use std::mem;

#[no_mangle]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(n);
    let p = v.as_mut_ptr();
    mem::forget(v);
    p
}

/// Format version this engine reads (also proves the linkage end-to-end).
#[no_mangle]
pub extern "C" fn format_version() -> u32 {
    facetful_engine::format::VERSION as u32
}
