//! User-defined scalar functions (design.sv d49).
//!
//! Two halves. A **registry** of declared signatures the binder consults after
//! the built-ins, so a UDF binds like any scalar — arity and type checks with
//! the same diagnostics, return type known at bind time. And a **host** that
//! evaluates a call over whole lanes: the JS worker through one wasm import,
//! a Rust closure in tests and the CLI. The executor never learns which; a
//! UDF is a scalar `Call` whose body happens to live elsewhere.
//!
//! Lanes cross as the engine holds them — numbers as f64 (ints, days and ms
//! exact to 2^53, like every other number on the boundary), text as
//! contiguous UTF-8 bytes + u32 offsets, validity as a bitmap — one call per
//! evaluation unit (a row group, a group table, or a dictionary).

use crate::sql::binder::{FuncDef, FuncKind, Sig, Ty};
use std::cell::RefCell;

/// Lane kinds on the wire. The numbers are the ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Int = 0,
    Float = 1,
    Bool = 2,
    Text = 3,
    Date = 4,
    Timestamp = 5,
}

impl Kind {
    pub fn from_u8(k: u8) -> Option<Kind> {
        Some(match k {
            0 => Kind::Int,
            1 => Kind::Float,
            2 => Kind::Bool,
            3 => Kind::Text,
            4 => Kind::Date,
            5 => Kind::Timestamp,
            _ => return None,
        })
    }
    pub fn ty(self) -> Ty {
        match self {
            Kind::Int => Ty::Int,
            Kind::Float => Ty::Float,
            Kind::Bool => Ty::Bool,
            Kind::Text => Ty::Text,
            Kind::Date => Ty::Date,
            Kind::Timestamp => Ty::Timestamp,
        }
    }
    pub fn of(ty: Ty) -> Kind {
        match ty {
            Ty::Int | Ty::Null => Kind::Int,
            Ty::Float => Kind::Float,
            Ty::Bool => Kind::Bool,
            Ty::Text => Kind::Text,
            Ty::Date => Kind::Date,
            Ty::Timestamp => Kind::Timestamp,
        }
    }
}

/// One argument as the host sees it. `len` 1 with `broadcast` = a constant.
pub enum Lane<'a> {
    /// ints, dates and timestamps travel as f64
    Num(&'a [f64]),
    Bool(&'a [u8]),
    Text { offsets: &'a [u32], bytes: &'a [u8] },
}

pub struct Arg<'a> {
    pub kind: Kind,
    pub lane: Lane<'a>,
    /// validity bitmap (bit set = present); None = all valid
    pub valid: Option<&'a [u8]>,
    pub broadcast: bool,
}

/// What the host fills: `len` values of the declared kind plus validity.
pub enum Out {
    Num(Vec<f64>),
    Bool(Vec<u8>),
    Text { offsets: Vec<u32>, bytes: Vec<u8> },
}

pub struct Output {
    pub kind: Kind,
    pub out: Out,
    /// validity bitmap the host writes (starts all-set)
    pub valid: Vec<u8>,
}

impl Output {
    pub fn new(kind: Kind, len: usize) -> Output {
        let out = match kind {
            Kind::Bool => Out::Bool(vec![0; len]),
            Kind::Text => Out::Text { offsets: vec![0; len + 1], bytes: Vec::new() },
            _ => Out::Num(vec![0.0; len]),
        };
        let mut valid = vec![0xffu8; (len + 7) / 8];
        if len % 8 != 0 {
            if let Some(last) = valid.last_mut() {
                *last = (1u16 << (len % 8)) as u8 - 1;
            }
        }
        Output { kind, out, valid }
    }
}

/// The side that runs the function body.
pub trait Host {
    fn call(&mut self, id: u32, args: &[Arg], len: usize, out: &mut Output) -> Result<(), String>;
}

thread_local! {
    static DEFS: RefCell<Vec<&'static FuncDef>> = const { RefCell::new(Vec::new()) };
    static HOST: RefCell<Option<Box<dyn Host>>> = const { RefCell::new(None) };
    static NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// A failing function body noted mid-scan; the driver turns it into the
/// query's error once the scan returns.
pub(crate) fn note_error(e: String) {
    LAST_ERROR.with(|s| {
        let mut s = s.borrow_mut();
        if s.is_none() {
            *s = Some(e);
        }
    });
}

pub(crate) fn take_error() -> Option<String> {
    LAST_ERROR.with(|s| s.borrow_mut().take())
}

/// Install the host that evaluates registered functions.
pub fn set_host(h: Box<dyn Host>) {
    HOST.with(|s| *s.borrow_mut() = Some(h));
}

/// Declare a function. `params` are the parameter types (`Ty::Null` = any
/// type); the last `optional` of them may be omitted; `variadic` lets the
/// last repeat. Re-registering a name replaces it under a fresh id, so cached
/// masks keyed on the old definition are never served for the new body. Core
/// function names are refused.
pub fn register(name: &str, params: &[Ty], ret: Ty, strict: bool, variadic: bool, optional: usize) -> Result<u32, String> {
    let name = name.to_ascii_lowercase();
    if crate::sql::binder::FUNCS.iter().any(|f| f.name == name) {
        return Err(format!("'{name}' is a built-in function"));
    }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("'{name}' is not a valid function name"));
    }
    if variadic && params.is_empty() {
        return Err("a variadic function needs at least one parameter type".into());
    }
    if optional > params.len() {
        return Err("more optional parameters than parameters".into());
    }
    let id = NEXT_ID.with(|n| {
        let mut n = n.borrow_mut();
        let id = *n;
        *n += 1;
        id
    });
    let params: &'static [Ty] = Box::leak(params.to_vec().into_boxed_slice());
    let def: &'static FuncDef = Box::leak(Box::new(FuncDef {
        name: Box::leak(name.clone().into_boxed_str()),
        kind: FuncKind::Scalar,
        arity: (params.len() - optional, if variadic { None } else { Some(params.len()) }),
        sig: Sig::Udf { id, params, ret, strict },
    }));
    DEFS.with(|d| {
        let mut d = d.borrow_mut();
        d.retain(|f| f.name != name);
        d.push(def);
    });
    Ok(id)
}

pub fn unregister(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    DEFS.with(|d| {
        let mut d = d.borrow_mut();
        let before = d.len();
        d.retain(|f| f.name != name);
        d.len() != before
    })
}

pub(crate) fn lookup(name: &str) -> Option<&'static FuncDef> {
    DEFS.with(|d| d.borrow().iter().copied().find(|f| f.name == name))
}

pub(crate) fn names() -> Vec<&'static str> {
    DEFS.with(|d| d.borrow().iter().map(|f| f.name).collect())
}

pub(crate) fn call(id: u32, args: &[Arg], len: usize, out: &mut Output) -> Result<(), String> {
    HOST.with(|h| match h.borrow_mut().as_mut() {
        Some(h) => h.call(id, args, len, out),
        None => Err("no function host installed".into()),
    })
}
