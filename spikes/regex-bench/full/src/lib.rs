// Representative use: compile a pattern from input, run is_match + one capture.
use regex::Regex;

#[no_mangle]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(n);
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn rx(pat_ptr: *const u8, pat_len: usize, s_ptr: *const u8, s_len: usize) -> i32 {
    let pat = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(pat_ptr, pat_len)) };
    let s = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(s_ptr, s_len)) };
    match Regex::new(pat) {
        Ok(re) => {
            let m = re.is_match(s) as i32;
            let cap = re.captures(s).and_then(|c| c.get(1)).map(|g| g.len()).unwrap_or(0);
            m + cap as i32
        }
        Err(_) => -1,
    }
}
