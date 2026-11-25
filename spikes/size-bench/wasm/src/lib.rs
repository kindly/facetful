#[path = "../../src/pack.rs"]
#[allow(dead_code)]
mod pack;

static mut OUT: Vec<u8> = Vec::new();

#[no_mangle]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// Unpack a whole packed image; returns the image's address (0 on error).
#[no_mangle]
pub extern "C" fn unpack(ptr: *const u8, len: usize) -> *const u8 {
    let wire = unsafe { std::slice::from_raw_parts(ptr, len) };
    #[cfg(feature = "unpack")]
    let out = pack::unpack(wire);
    #[cfg(not(feature = "unpack"))]
    let out = facetful_format::read::parse_header(wire).ok().map(|h| vec![h.0.columns.len() as u8]);
    match out {
        Some(v) => unsafe {
            OUT = v;
            (*std::ptr::addr_of!(OUT)).as_ptr()
        },
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub extern "C" fn out_len() -> usize {
    unsafe { (*std::ptr::addr_of!(OUT)).len() }
}
