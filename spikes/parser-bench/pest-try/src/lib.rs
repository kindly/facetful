// Same expression subset via pest (PEG grammar file + runtime crate).
// Includes pest's pretty error Display (caret + span) in the measurement.
use pest::Parser;
use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "expr.pest"]
struct ExprParser;

static mut LAST_ERR: Option<String> = None;

#[no_mangle]
pub extern "C" fn parse(ptr: *const u8, len: usize) -> i32 {
    let s = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len)) };
    match ExprParser::parse(Rule::top, s) {
        Ok(pairs) => {
            core::mem::forget(pairs);
            1
        }
        Err(e) => {
            unsafe { LAST_ERR = Some(e.to_string()) }; // pest's caret-rendered message
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(n);
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}
