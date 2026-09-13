//! Vectorized execution (M6 stage 1). Expressions evaluate ONCE per row group
//! into column vectors; per-row work happens only inside flat kernel loops.
//! Hot paths are specialized: numeric comparisons, dictionary-code fast paths
//! for =/IN/LIKE against string literals (predicates evaluated once per
//! dictionary entry, scans compare integers), three-valued mask logic,
//! aggregation update loops, and direct-indexed grouping when every group-by
//! dim is a dict column. Cold ops (string functions, CASE, casts) run through
//! per-lane accessors — still no per-row tree walks or Val allocations.

use super::ast::{BinOp, SortDir, UnOp};
use super::binder::{Bound, FuncKind, Ty};
use crate::format::read::ReadAt;
use crate::format::{ColumnType, FormatError};
use crate::mask_cache::LikeKey;
use crate::Table;
use std::collections::HashMap;
use std::rc::Rc;

// ---------------- values (output + group keys only) ----------------

#[derive(Debug, Clone)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(Rc<String>),
}

impl Val {
    pub fn text(s: impl Into<String>) -> Val {
        Val::Text(Rc::new(s.into()))
    }
    fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }
    fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Int(i) => Some(*i as f64),
            Val::Float(f) => Some(*f),
            _ => None,
        }
    }
    fn cmp_sql(&self, other: &Val) -> core::cmp::Ordering {
        use core::cmp::Ordering::*;
        use Val::*;
        match (self, other) {
            (Null, Null) => Equal,
            (Null, _) => Less,
            (_, Null) => Greater,
            (Bool(a), Bool(b)) => a.cmp(b),
            (Text(a), Text(b)) => a.cmp(b),
            (a, b) => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => Equal,
            },
        }
    }
    fn eq_sql(&self, other: &Val) -> Option<bool> {
        if self.is_null() || other.is_null() {
            return None;
        }
        Some(self.cmp_sql(other) == core::cmp::Ordering::Equal)
    }
}

impl PartialEq for Val {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Val::Null, Val::Null) => true, // grouping semantics
            (Val::Null, _) | (_, Val::Null) => false,
            _ => self.cmp_sql(other) == core::cmp::Ordering::Equal,
        }
    }
}
impl Eq for Val {}
impl core::hash::Hash for Val {
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        match self {
            Val::Null => 0u8.hash(h),
            Val::Bool(b) => (1u8, b).hash(h),
            Val::Int(i) => {
                2u8.hash(h);
                (*i as f64).to_bits().hash(h);
            }
            Val::Float(f) => {
                2u8.hash(h);
                f.to_bits().hash(h);
            }
            Val::Text(s) => (3u8, s).hash(h),
        }
    }
}

pub struct QueryResult {
    pub columns: Vec<String>,
    /// bound type per output column (Date/Timestamp make marshalling
    /// format the underlying days/ms as ISO strings where appropriate)
    pub col_types: Vec<Ty>,
    pub rows: Vec<Vec<Val>>,
    /// Columnar channel: the non-aggregate projection paths fill this instead
    /// of `rows` (typed vectors gather at memcpy-like speed; per-row Vals cost
    /// ~5x). Consumers that want rows call `ensure_rows()`.
    pub cols: Option<Vec<OutCol>>,
    pub out_rows: usize,
    pub scanned_groups: usize,
    pub total_groups: usize,
}

/// One projected output column plus its validity bitmap (bit set = non-null).
pub enum OutCol {
    I64 { v: Vec<i64>, valid: Vec<u8> },
    F64 { v: Vec<f64>, valid: Vec<u8> },
    Bool { v: Vec<u8>, valid: Vec<u8> },
    Text { offsets: Vec<u32>, bytes: Vec<u8>, valid: Vec<u8> },
}

impl QueryResult {
    pub fn n_rows(&self) -> usize {
        if self.cols.is_some() { self.out_rows } else { self.rows.len() }
    }

    /// Materialize `rows` from the columnar channel (no-op when already rows).
    pub fn ensure_rows(&mut self) {
        let Some(cols) = self.cols.take() else { return };
        let n = self.out_rows;
        let mut rows: Vec<Vec<Val>> = (0..n).map(|_| Vec::with_capacity(cols.len())).collect();
        for (ci, c) in cols.iter().enumerate() {
            let ty = self.col_types[ci];
            for (i, row) in rows.iter_mut().enumerate() {
                row.push(outcol_val(c, i, ty));
            }
        }
        self.rows = rows;
    }
}

fn outcol_val(c: &OutCol, i: usize, ty: Ty) -> Val {
    let ok = |valid: &Vec<u8>| valid[i / 8] >> (i % 8) & 1 != 0;
    match c {
        OutCol::I64 { v, valid } => {
            if !ok(valid) {
                Val::Null
            } else if ty == Ty::Float {
                Val::Float(v[i] as f64)
            } else {
                Val::Int(v[i])
            }
        }
        OutCol::F64 { v, valid } => {
            if !ok(valid) {
                Val::Null
            } else if ty == Ty::Int {
                Val::Int(v[i] as i64)
            } else {
                Val::Float(v[i])
            }
        }
        OutCol::Bool { v, valid } => {
            if !ok(valid) { Val::Null } else { Val::Bool(v[i] != 0) }
        }
        OutCol::Text { offsets, bytes, valid } => {
            if !ok(valid) {
                Val::Null
            } else {
                Val::text(
                    String::from_utf8_lossy(&bytes[offsets[i] as usize..offsets[i + 1] as usize])
                        .into_owned(),
                )
            }
        }
    }
}

// ---------------- vectors ----------------

type VStr = Rc<String>;

#[derive(Clone)]
enum Data {
    I64(Rc<Vec<i64>>),
    F64(Rc<Vec<f64>>),
    /// dictionary column: codes + shared dict
    Codes { codes: Rc<Vec<u16>>, dict: Rc<Vec<VStr>> },
    Text(Rc<Vec<VStr>>),
    Bool(Rc<Vec<u8>>),
    /// broadcast literal
    Const(Val),
}

#[derive(Clone)]
struct VV {
    data: Data,
    /// validity bitmap (bit set = present); None = all valid
    valid: Option<Rc<Vec<u8>>>,
}

#[inline]
fn bit(v: &Option<Rc<Vec<u8>>>, i: usize) -> bool {
    v.as_ref().map_or(true, |b| b[i / 8] & (1 << (i % 8)) != 0)
}

impl VV {
    fn all_valid(data: Data) -> VV {
        VV { data, valid: None }
    }
    fn const_val(v: Val) -> VV {
        VV { data: Data::Const(v), valid: None }
    }
    #[inline]
    fn is_valid(&self, i: usize) -> bool {
        if let Data::Const(v) = &self.data {
            return !v.is_null();
        }
        bit(&self.valid, i)
    }
    #[inline]
    fn f64_at(&self, i: usize) -> f64 {
        match &self.data {
            Data::F64(v) => v[i],
            Data::I64(v) => v[i] as f64,
            Data::Const(c) => c.as_f64().unwrap_or(0.0),
            _ => 0.0,
        }
    }
    #[inline]
    fn i64_at(&self, i: usize) -> i64 {
        match &self.data {
            Data::I64(v) => v[i],
            Data::F64(v) => v[i] as i64,
            Data::Const(Val::Int(x)) => *x,
            Data::Const(Val::Float(x)) => *x as i64,
            _ => 0,
        }
    }
    /// three-valued bool at lane i
    #[inline]
    fn bool3_at(&self, i: usize) -> Option<bool> {
        if !self.is_valid(i) {
            return None;
        }
        match &self.data {
            Data::Bool(v) => Some(v[i] != 0),
            Data::Const(Val::Bool(b)) => Some(*b),
            _ => None,
        }
    }
    fn text_at(&self, i: usize) -> Option<VStr> {
        if !self.is_valid(i) {
            return None;
        }
        match &self.data {
            Data::Codes { codes, dict } => dict.get(codes[i] as usize).cloned(),
            Data::Text(v) => Some(v[i].clone()),
            Data::Const(Val::Text(s)) => Some(s.clone()),
            _ => None,
        }
    }
    /// output materialization
    fn val_at(&self, i: usize, ty: Ty) -> Val {
        if !self.is_valid(i) {
            return Val::Null;
        }
        match (&self.data, ty) {
            (Data::Const(v), _) => v.clone(),
            (Data::Bool(v), _) => Val::Bool(v[i] != 0),
            (Data::Codes { codes, dict }, _) => match dict.get(codes[i] as usize) {
                Some(s) => Val::Text(s.clone()),
                None => Val::Null,
            },
            (Data::Text(v), _) => Val::Text(v[i].clone()),
            (Data::I64(v), Ty::Float) => Val::Float(v[i] as f64),
            (Data::I64(v), _) => Val::Int(v[i]),
            (Data::F64(v), Ty::Int | Ty::Date | Ty::Timestamp) => Val::Int(v[i] as i64),
            (Data::F64(v), _) => Val::Float(v[i]),
        }
    }
}

/// intersect validities (arithmetic null propagation)
fn valid_and(rows: usize, a: &VV, b: &VV) -> Option<Rc<Vec<u8>>> {
    let an = matches!(&a.data, Data::Const(v) if v.is_null());
    let bn = matches!(&b.data, Data::Const(v) if v.is_null());
    if an || bn {
        return Some(Rc::new(vec![0u8; (rows + 7) / 8]));
    }
    match (&a.valid, &b.valid) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (Some(x), Some(y)) => Some(Rc::new(x.iter().zip(y.iter()).map(|(p, q)| p & q).collect())),
    }
}

// ---------------- group context ----------------

enum GroupCol {
    I64(Rc<Vec<i64>>),
    F64(Rc<Vec<f64>>),
    Dict { codes: Rc<Vec<u16>>, dict: Rc<Vec<VStr>> },
    /// raw blob feeds the LIKE scan kernel; strs materialize lazily, only
    /// when something actually evaluates the strings (a like-only filter
    /// never pays the per-string allocation)
    Text {
        strs: std::cell::OnceCell<Rc<Vec<VStr>>>,
        offsets: Rc<Vec<u32>>,
        bytes: Rc<Vec<u8>>,
    },
}

struct GroupCtx {
    cols: HashMap<usize, (GroupCol, Option<Rc<Vec<u8>>>)>,
    rows: usize,
}

impl GroupCtx {
    fn column(&self, idx: usize) -> VV {
        let (c, valid) = &self.cols[&idx];
        let data = match c {
            GroupCol::I64(v) => Data::I64(v.clone()),
            GroupCol::F64(v) => Data::F64(v.clone()),
            GroupCol::Dict { codes, dict } => {
                Data::Codes { codes: codes.clone(), dict: dict.clone() }
            }
            GroupCol::Text { strs, offsets, bytes } => Data::Text(
                strs.get_or_init(|| {
                    Rc::new(
                        offsets
                            .windows(2)
                            .map(|w| {
                                Rc::new(
                                    String::from_utf8_lossy(
                                        &bytes[w[0] as usize..w[1] as usize],
                                    )
                                    .into_owned(),
                                )
                            })
                            .collect(),
                    )
                })
                .clone(),
            ),
        };
        VV { data, valid: valid.clone() }
    }
}

// ---------------- vector evaluation ----------------

fn eval_vec(b: &Bound, ctx: &GroupCtx) -> VV {
    let rows = ctx.rows;
    match b {
        Bound::Number(n, is_float) => VV::const_val(if *is_float {
            Val::Float(*n)
        } else if n.fract() == 0.0 && n.abs() < 9e15 {
            Val::Int(*n as i64)
        } else {
            Val::Float(*n)
        }),
        Bound::Str(s) => VV::const_val(Val::text(s.clone())),
        Bound::Null => VV::const_val(Val::Null),
        Bound::Column { index, .. } => ctx.column(*index),
        Bound::Unary { op, expr, ty } => {
            let a = eval_vec(expr, ctx);
            match op {
                UnOp::Neg => {
                    if let Data::Const(v) = &a.data {
                        return VV::const_val(match v {
                            Val::Int(i) => Val::Int(-i),
                            Val::Float(f) => Val::Float(-f),
                            _ => Val::Null,
                        });
                    }
                    if matches!(ty, Ty::Int | Ty::Date | Ty::Timestamp) {
                        let out: Vec<i64> = (0..rows).map(|i| -a.i64_at(i)).collect();
                        VV { data: Data::I64(Rc::new(out)), valid: a.valid.clone() }
                    } else {
                        let out: Vec<f64> = (0..rows).map(|i| -a.f64_at(i)).collect();
                        VV { data: Data::F64(Rc::new(out)), valid: a.valid.clone() }
                    }
                }
                UnOp::Not => {
                    let mut out = vec![0u8; rows];
                    let mut valid = vec![0u8; (rows + 7) / 8];
                    for i in 0..rows {
                        if let Some(x) = a.bool3_at(i) {
                            out[i] = !x as u8;
                            valid[i / 8] |= 1 << (i % 8);
                        }
                    }
                    VV { data: Data::Bool(Rc::new(out)), valid: Some(Rc::new(valid)) }
                }
            }
        }
        Bound::Binary { op, lhs, rhs, ty } => {
            let a = eval_vec(lhs, ctx);
            let b2 = eval_vec(rhs, ctx);
            eval_binary_vec(*op, a, b2, *ty, rows)
        }
        Bound::Call { func, args, ty } => eval_call_vec(func.name, args, *ty, ctx),
    }
}

fn eval_binary_vec(op: BinOp, a: VV, b: VV, ty: Ty, rows: usize) -> VV {
    use BinOp::*;
    match op {
        And | Or => {
            let mut out = vec![0u8; rows];
            let mut valid = vec![0u8; (rows + 7) / 8];
            for i in 0..rows {
                let (x, y) = (a.bool3_at(i), b.bool3_at(i));
                let r = if op == And {
                    match (x, y) {
                        (Some(false), _) | (_, Some(false)) => Some(false),
                        (Some(true), Some(true)) => Some(true),
                        _ => None,
                    }
                } else {
                    match (x, y) {
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    }
                };
                if let Some(r) = r {
                    out[i] = r as u8;
                    valid[i / 8] |= 1 << (i % 8);
                }
            }
            VV { data: Data::Bool(Rc::new(out)), valid: Some(Rc::new(valid)) }
        }
        Eq | Ne | Lt | Le | Gt | Ge => cmp_vec(op, a, b, rows),
        Add | Sub | Mul | Mod | Div => {
            let valid = valid_and(rows, &a, &b);
            if ty == Ty::Int && op != Div && op != Mod {
                let out: Vec<i64> = (0..rows)
                    .map(|i| {
                        let (x, y) = (a.i64_at(i), b.i64_at(i));
                        match op {
                            Add => x.wrapping_add(y),
                            Sub => x.wrapping_sub(y),
                            Mul => x.wrapping_mul(y),
                            _ => unreachable!(),
                        }
                    })
                    .collect();
                VV { data: Data::I64(Rc::new(out)), valid }
            } else if ty == Ty::Int {
                // SQLite: int/int truncates; /0 and %0 -> NULL
                let mut v = to_bitmap(rows, &valid);
                let out: Vec<i64> = (0..rows)
                    .map(|i| {
                        let y = b.i64_at(i);
                        if y == 0 {
                            v[i / 8] &= !(1 << (i % 8));
                            0
                        } else if op == Div {
                            a.i64_at(i) / y
                        } else {
                            a.i64_at(i) % y
                        }
                    })
                    .collect();
                VV { data: Data::I64(Rc::new(out)), valid: Some(Rc::new(v)) }
            } else {
                let mut v = to_bitmap(rows, &valid);
                let out: Vec<f64> = (0..rows)
                    .map(|i| {
                        let (x, y) = (a.f64_at(i), b.f64_at(i));
                        match op {
                            Add => x + y,
                            Sub => x - y,
                            Mul => x * y,
                            Mod | Div => {
                                if y == 0.0 {
                                    v[i / 8] &= !(1 << (i % 8));
                                    0.0
                                } else if op == Div {
                                    x / y
                                } else {
                                    x % y
                                }
                            }
                            _ => unreachable!(),
                        }
                    })
                    .collect();
                VV { data: Data::F64(Rc::new(out)), valid: Some(Rc::new(v)) }
            }
        }
    }
}

fn to_bitmap(rows: usize, valid: &Option<Rc<Vec<u8>>>) -> Vec<u8> {
    match valid {
        Some(v) => v.as_ref().clone(),
        None => vec![0xffu8; (rows + 7) / 8],
    }
}

fn cmp_ord(op: BinOp, o: core::cmp::Ordering) -> bool {
    use BinOp::*;
    match op {
        Eq => o.is_eq(),
        Ne => !o.is_eq(),
        Lt => o.is_lt(),
        Le => o.is_le(),
        Gt => o.is_gt(),
        Ge => o.is_ge(),
        _ => unreachable!(),
    }
}

fn cmp_vec(op: BinOp, a: VV, b: VV, rows: usize) -> VV {
    // fast path: dict codes vs string literal
    if let (Data::Codes { codes, dict }, Data::Const(Val::Text(lit))) = (&a.data, &b.data) {
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            let code = dict.iter().position(|s| **s == **lit);
            let mut out = vec![0u8; rows];
            match code {
                Some(c) => {
                    let c = c as u16;
                    for i in 0..rows {
                        let hit = codes[i] == c;
                        out[i] = (if op == BinOp::Eq { hit } else { !hit }) as u8;
                    }
                }
                None => {
                    if op == BinOp::Ne {
                        out.fill(1);
                    }
                }
            }
            return VV { data: Data::Bool(Rc::new(out)), valid: a.valid.clone() };
        }
        // ordered comparison vs literal: per-code truth table
        let table: Vec<u8> =
            dict.iter().map(|s| cmp_ord(op, s.as_str().cmp(lit.as_str())) as u8).collect();
        let out: Vec<u8> = codes.iter().map(|&c| table[c as usize]).collect();
        return VV { data: Data::Bool(Rc::new(out)), valid: a.valid.clone() };
    }
    // numeric lane vs literal: specialized loops — the generic per-row
    // accessor path defeats auto-vectorization (measured ~10x slower)
    fn fill<T: Copy>(xs: &[T], f: impl Fn(T) -> bool) -> Vec<u8> {
        xs.iter().map(|&x| f(x) as u8).collect()
    }
    fn cmp_i64(op: BinOp, xs: &[i64], lit: i64) -> Vec<u8> {
        match op {
            BinOp::Eq => fill(xs, |x| x == lit),
            BinOp::Ne => fill(xs, |x| x != lit),
            BinOp::Lt => fill(xs, |x| x < lit),
            BinOp::Le => fill(xs, |x| x <= lit),
            BinOp::Gt => fill(xs, |x| x > lit),
            BinOp::Ge => fill(xs, |x| x >= lit),
            _ => unreachable!("cmp op"),
        }
    }
    fn cmp_f64(op: BinOp, xs: &[f64], lit: f64) -> Vec<u8> {
        match op {
            BinOp::Eq => fill(xs, |x| x.total_cmp(&lit).is_eq()),
            BinOp::Ne => fill(xs, |x| x.total_cmp(&lit).is_ne()),
            BinOp::Lt => fill(xs, |x| x.total_cmp(&lit).is_lt()),
            BinOp::Le => fill(xs, |x| x.total_cmp(&lit).is_le()),
            BinOp::Gt => fill(xs, |x| x.total_cmp(&lit).is_gt()),
            BinOp::Ge => fill(xs, |x| x.total_cmp(&lit).is_ge()),
            _ => unreachable!("cmp op"),
        }
    }
    // literal on the left: mirror (lit < x  ==  x > lit)
    let (a, b, op) = if matches!(a.data, Data::Const(_)) {
        let m = match op {
            BinOp::Lt => BinOp::Gt,
            BinOp::Le => BinOp::Ge,
            BinOp::Gt => BinOp::Lt,
            BinOp::Ge => BinOp::Le,
            other => other,
        };
        (b, a, m)
    } else {
        (a, b, op)
    };
    if let Data::Const(cv) = &b.data {
        let out = match (&a.data, cv) {
            (Data::I64(xs), Val::Int(lit)) => Some(cmp_i64(op, xs, *lit)),
            (Data::I64(xs), Val::Float(lit)) if lit.fract() == 0.0 && lit.abs() < 9e15 => {
                Some(cmp_i64(op, xs, *lit as i64))
            }
            (Data::I64(xs), Val::Float(lit)) => {
                let lit = *lit;
                Some(match op {
                    BinOp::Eq => fill(xs, |x| (x as f64).total_cmp(&lit).is_eq()),
                    BinOp::Ne => fill(xs, |x| (x as f64).total_cmp(&lit).is_ne()),
                    BinOp::Lt => fill(xs, |x| (x as f64).total_cmp(&lit).is_lt()),
                    BinOp::Le => fill(xs, |x| (x as f64).total_cmp(&lit).is_le()),
                    BinOp::Gt => fill(xs, |x| (x as f64).total_cmp(&lit).is_gt()),
                    BinOp::Ge => fill(xs, |x| (x as f64).total_cmp(&lit).is_ge()),
                    _ => unreachable!("cmp op"),
                })
            }
            (Data::F64(xs), Val::Int(lit)) => Some(cmp_f64(op, xs, *lit as f64)),
            (Data::F64(xs), Val::Float(lit)) => Some(cmp_f64(op, xs, *lit)),
            _ => None,
        };
        if let Some(out) = out {
            let valid = valid_and(rows, &a, &b);
            return VV { data: Data::Bool(Rc::new(out)), valid };
        }
    }
    // numeric path
    let numeric = |d: &Data| {
        matches!(d, Data::I64(_) | Data::F64(_))
            || matches!(d, Data::Const(v) if v.as_f64().is_some())
    };
    if numeric(&a.data) && numeric(&b.data) {
        let valid = valid_and(rows, &a, &b);
        let out: Vec<u8> =
            (0..rows).map(|i| cmp_ord(op, a.f64_at(i).total_cmp(&b.f64_at(i))) as u8).collect();
        return VV { data: Data::Bool(Rc::new(out)), valid };
    }
    // generic (text/text, codes/codes, …)
    let valid = valid_and(rows, &a, &b);
    let mut out = vec![0u8; rows];
    for i in 0..rows {
        if !bit(&valid, i) {
            continue;
        }
        let r = match (a.text_at(i), b.text_at(i)) {
            (Some(x), Some(y)) => cmp_ord(op, x.cmp(&y)),
            _ => cmp_ord(op, a.f64_at(i).total_cmp(&b.f64_at(i))),
        };
        out[i] = r as u8;
    }
    VV { data: Data::Bool(Rc::new(out)), valid }
}

/// SQL LIKE (%/_ wildcards, ascii-case-insensitive)
fn like_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let sc: Vec<char> = s.chars().collect();
    like_rec(&p, &sc)
}

fn like_rec(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('%') => (0..=s.len()).any(|k| like_rec(&p[1..], &s[k..])),
        Some('_') => !s.is_empty() && like_rec(&p[1..], &s[1..]),
        Some(c) => !s.is_empty() && s[0].eq_ignore_ascii_case(c) && like_rec(&p[1..], &s[1..]),
    }
}

/// The literal shapes of a LIKE pattern (needle stored lowercase for Contains).
enum LikeShape {
    Contains(String),
    Prefix(String),
    Suffix(String),
    Exact(String),
    General,
}

fn classify_like(p: &str) -> LikeShape {
    let inner_has_wildcard = |s: &str| s.contains('%') || s.contains('_');
    let starts = p.starts_with('%');
    let ends = p.ends_with('%') && p.len() >= 2;
    let core: &str = match (starts, ends) {
        (true, true) if p.len() >= 2 => &p[1..p.len() - 1],
        (true, false) => &p[1..],
        (false, true) => &p[..p.len() - 1],
        _ => p,
    };
    if inner_has_wildcard(core) || core.is_empty() {
        return LikeShape::General;
    }
    match (starts, ends) {
        (true, true) => LikeShape::Contains(core.to_ascii_lowercase()),
        (true, false) => LikeShape::Suffix(core.to_ascii_lowercase()),
        (false, true) => LikeShape::Prefix(core.to_ascii_lowercase()),
        (false, false) => LikeShape::Exact(core.to_ascii_lowercase()),
    }
}

/// Char-based substring matching scalar_fn's SQLite semantics (start is
/// already 0-based and clamped; None len = to end).
fn substr_str(s: &str, start: usize, len: Option<usize>) -> String {
    let (b0, b1) = substr_bounds(s, start, len);
    s[b0..b1].to_string()
}

/// Byte bounds of the char-based substring (empty range when out of bounds).
fn substr_bounds(s: &str, start: usize, len: Option<usize>) -> (usize, usize) {
    let Some((b0, _)) = s.char_indices().nth(start) else {
        return (0, 0);
    };
    let end = match len {
        None => s.len(),
        Some(l) => s[b0..].char_indices().nth(l).map(|(b, _)| b0 + b).unwrap_or(s.len()),
    };
    (b0, end)
}

/// One string against a classified pattern (needles are lowercase).
fn like_shape_match(shape: &LikeShape, s: &str, full_pattern: &str) -> bool {
    let b = s.as_bytes();
    match shape {
        LikeShape::Contains(n) => crate::text::contains_ci(b, n.as_bytes()),
        LikeShape::Prefix(n) => crate::text::prefix_ci(b, n.as_bytes()),
        LikeShape::Suffix(n) => crate::text::suffix_ci(b, n.as_bytes()),
        LikeShape::Exact(n) => crate::text::eq_ci(b, n.as_bytes()),
        LikeShape::General => like_match(full_pattern, s),
    }
}

fn eval_call_vec(name: &str, args: &[Bound], ty: Ty, ctx: &GroupCtx) -> VV {
    let rows = ctx.rows;
    match name {
        "isnull" => {
            let a = eval_vec(&args[0], ctx);
            let out: Vec<u8> = (0..rows).map(|i| !a.is_valid(i) as u8).collect();
            VV::all_valid(Data::Bool(Rc::new(out)))
        }
        "between" => {
            let x = eval_vec(&args[0], ctx);
            let lo = eval_vec(&args[1], ctx);
            let hi = eval_vec(&args[2], ctx);
            let ge = cmp_vec(BinOp::Ge, x.clone(), lo, rows);
            let le = cmp_vec(BinOp::Le, x, hi, rows);
            eval_binary_vec(BinOp::And, ge, le, Ty::Bool, rows)
        }
        "in" => {
            let needle = eval_vec(&args[0], ctx);
            // dict-aware: string list vs codes -> per-code membership table
            if let Data::Codes { codes, dict } = &needle.data {
                let lits: Option<Vec<&str>> = args[1..]
                    .iter()
                    .map(|a| match a {
                        Bound::Str(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect();
                if let Some(lits) = lits {
                    let table: Vec<u8> = dict
                        .iter()
                        .map(|s| lits.iter().any(|l| *l == s.as_str()) as u8)
                        .collect();
                    let out: Vec<u8> = codes.iter().map(|&c| table[c as usize]).collect();
                    return VV { data: Data::Bool(Rc::new(out)), valid: needle.valid.clone() };
                }
            }
            // generic with SQL IN null semantics
            let items: Vec<VV> = args[1..].iter().map(|a| eval_vec(a, ctx)).collect();
            let mut out = vec![0u8; rows];
            let mut valid = vec![0u8; (rows + 7) / 8];
            for i in 0..rows {
                if !needle.is_valid(i) {
                    continue;
                }
                let nv = lane_val(&needle, i);
                let mut saw_null = false;
                let mut hit = false;
                for it in &items {
                    if !it.is_valid(i) {
                        saw_null = true;
                        continue;
                    }
                    if nv.eq_sql(&lane_val(it, i)) == Some(true) {
                        hit = true;
                        break;
                    }
                }
                if hit {
                    out[i] = 1;
                    valid[i / 8] |= 1 << (i % 8);
                } else if !saw_null {
                    valid[i / 8] |= 1 << (i % 8);
                }
            }
            VV { data: Data::Bool(Rc::new(out)), valid: Some(Rc::new(valid)) }
        }
        "like" => {
            // direct plain-text column vs constant contains-pattern: scan the
            // contiguous byte blob ONCE (SIMD on wasm), walking offsets along
            // the hits — no per-string calls, no String materialization
            if let (Bound::Column { index, .. }, Bound::Str(p)) = (&args[0], &args[1]) {
                if let Some((GroupCol::Text { offsets, bytes, .. }, validity)) =
                    ctx.cols.get(index)
                {
                    if let LikeShape::Contains(n) = classify_like(p) {
                        let n = n.as_bytes();
                        let blob = &bytes[..offsets[rows] as usize];
                        let mut out = vec![0u8; rows];
                        let mut row = 0usize;
                        let mut pos = 0usize;
                        while let Some(hit) = crate::text::find_ci(blob, pos, n) {
                            while (offsets[row + 1] as usize) <= hit {
                                row += 1;
                            }
                            let end = offsets[row + 1] as usize;
                            if hit + n.len() <= end {
                                out[row] = 1;
                                pos = end; // matched: skip the rest of this string
                            } else {
                                pos = hit + 1; // straddles a boundary: not a match
                            }
                        }
                        return VV { data: Data::Bool(Rc::new(out)), valid: validity.clone() };
                    }
                }
            }
            let a = eval_vec(&args[0], ctx);
            let pat = eval_vec(&args[1], ctx);
            if let (Data::Codes { codes, dict }, Data::Const(Val::Text(p))) = (&a.data, &pat.data) {
                // pattern evaluated once per dictionary entry — through the
                // same no-allocation fast paths as string lanes
                let shape = classify_like(p);
                let table: Vec<u8> =
                    dict.iter().map(|s| like_shape_match(&shape, s, p) as u8).collect();
                let out: Vec<u8> = codes.iter().map(|&c| table[c as usize]).collect();
                return VV { data: Data::Bool(Rc::new(out)), valid: a.valid.clone() };
            }
            // string lanes vs constant pattern: literal fast paths (%x% / x% /
            // %x / exact); the general matcher only sees real wildcards
            if let (Data::Text(texts), Data::Const(Val::Text(p))) = (&a.data, &pat.data) {
                let shape = classify_like(p);
                let mut out = vec![0u8; rows];
                for i in 0..rows {
                    out[i] = like_shape_match(&shape, &texts[i], p) as u8;
                }
                return VV { data: Data::Bool(Rc::new(out)), valid: a.valid.clone() };
            }
            let valid = valid_and(rows, &a, &pat);
            let mut out = vec![0u8; rows];
            for i in 0..rows {
                if bit(&valid, i) {
                    if let (Some(s), Some(p)) = (a.text_at(i), pat.text_at(i)) {
                        out[i] = like_match(&p, &s) as u8;
                    }
                }
            }
            VV { data: Data::Bool(Rc::new(out)), valid }
        }
        "coalesce" | "ifnull" => {
            let first = eval_vec(&args[0], ctx);
            // A nullable column can still be all-present in this row group.
            // Keep its native vector (including dictionary codes) in that case.
            if first.valid.is_none() && !matches!(first.data, Data::Const(Val::Null)) {
                return first;
            }
            let items: Vec<VV> = std::iter::once(first)
                .chain(args[1..].iter().map(|a| eval_vec(a, ctx)))
                .collect();
            lanes_to_vv(rows, ty, |i| {
                items.iter().find(|v| v.is_valid(i)).map(|v| lane_val(v, i)).unwrap_or(Val::Null)
            })
        }
        "if" | "case" => {
            let items: Vec<VV> = args.iter().map(|a| eval_vec(a, ctx)).collect();
            // numeric two-branch fast path: case when <cond> then <num> else <num> end
            if matches!(ty, Ty::Int | Ty::Float) && items.len() == 3 {
                let numeric = |v: &VV| {
                    matches!(v.data, Data::F64(_) | Data::I64(_))
                        || matches!(&v.data, Data::Const(c) if c.as_f64().is_some())
                };
                if numeric(&items[1]) && numeric(&items[2]) {
                    let (cond, then_v, else_v) = (&items[0], &items[1], &items[2]);
                    let mut out = vec![0f64; rows];
                    let mut valid = vec![0u8; (rows + 7) / 8];
                    for i in 0..rows {
                        let src = if cond.bool3_at(i) == Some(true) { then_v } else { else_v };
                        if src.is_valid(i) {
                            out[i] = src.f64_at(i);
                            valid[i / 8] |= 1 << (i % 8);
                        }
                    }
                    return VV { data: Data::F64(Rc::new(out)), valid: Some(Rc::new(valid)) };
                }
            }
            lanes_to_vv(rows, ty, |i| {
                let mut k = 0;
                while k + 1 < items.len() {
                    if items[k].bool3_at(i) == Some(true) {
                        return lane_val(&items[k + 1], i);
                    }
                    k += 2;
                }
                if items.len() % 2 == 1 {
                    lane_val(items.last().unwrap(), i)
                } else {
                    Val::Null
                }
            })
        }
        "substr" => {
            let a = eval_vec(&args[0], ctx);
            let start_lit = match &args[1] {
                Bound::Number(f, _) => Some(*f),
                _ => None,
            };
            // None = no len arg; Some(None) would be non-literal (fall back)
            let len_lit: Option<Option<f64>> = match args.get(2) {
                None => Some(None),
                Some(Bound::Number(f, _)) => Some(Some(*f)),
                _ => None,
            };
            if let (Some(sf), Some(lf)) = (start_lit, len_lit) {
                let start = (sf as i64 - 1).max(0) as usize;
                let len = lf.map(|f| f as usize);
                // per-dictionary-entry, then per-row Rc clones
                if let Data::Codes { codes, dict } = &a.data {
                    let table: Vec<VStr> =
                        dict.iter().map(|s| Rc::new(substr_str(s, start, len))).collect();
                    let out: Vec<VStr> = codes.iter().map(|&c| table[c as usize].clone()).collect();
                    return VV { data: Data::Text(Rc::new(out)), valid: a.valid.clone() };
                }
                // straight over the string lane: one output allocation per row
                if let Data::Text(texts) = &a.data {
                    let out: Vec<VStr> =
                        texts.iter().map(|s| Rc::new(substr_str(s, start, len))).collect();
                    return VV { data: Data::Text(Rc::new(out)), valid: a.valid.clone() };
                }
            }
            let items: Vec<VV> =
                std::iter::once(a).chain(args[1..].iter().map(|e| eval_vec(e, ctx))).collect();
            lanes_to_vv(rows, ty, |i| {
                scalar_fn(name, items.iter().map(|v| lane_val(v, i)).collect())
            })
        }
        "int" | "float" => {
            let want_int = name == "int";
            // fused cast(substr(col, lit[, lit])): slice + parse straight out of
            // the column's byte blob — no substring materialization at all
            if let Bound::Call { func, args: sargs, .. } = &args[0] {
                if func.name == "substr" {
                    let start_lit = match sargs.get(1) {
                        Some(Bound::Number(f, _)) => Some((*f as i64 - 1).max(0) as usize),
                        _ => None,
                    };
                    let len_lit: Option<Option<usize>> = match sargs.get(2) {
                        None => Some(None),
                        Some(Bound::Number(f, _)) => Some(Some(*f as usize)),
                        _ => None,
                    };
                    if let (Bound::Column { index, .. }, Some(start), Some(len)) =
                        (&sargs[0], start_lit, len_lit)
                    {
                        if let Some((GroupCol::Text { offsets, bytes, .. }, validity)) =
                            ctx.cols.get(index)
                        {
                            let colvalid = validity.clone();
                            let cv = |i: usize| {
                                colvalid.as_deref().map_or(true, |v| v[i / 8] >> (i % 8) & 1 != 0)
                            };
                            let mut valid = vec![0u8; (rows + 7) / 8];
                            let mut outf = vec![0f64; rows];
                            let mut outi = vec![0i64; rows];
                            for i in 0..rows {
                                if !cv(i) {
                                    continue;
                                }
                                let cell =
                                    &bytes[offsets[i] as usize..offsets[i + 1] as usize];
                                let Ok(cs) = core::str::from_utf8(cell) else { continue };
                                let (b0, b1) = substr_bounds(cs, start, len);
                                let piece = cs[b0..b1].trim();
                                if want_int {
                                    if let Ok(x) = piece.parse::<i64>() {
                                        outi[i] = x;
                                        valid[i / 8] |= 1 << (i % 8);
                                    }
                                } else if let Ok(x) = piece.parse::<f64>() {
                                    outf[i] = x;
                                    valid[i / 8] |= 1 << (i % 8);
                                }
                            }
                            return if want_int {
                                VV { data: Data::I64(Rc::new(outi)), valid: Some(Rc::new(valid)) }
                            } else {
                                VV { data: Data::F64(Rc::new(outf)), valid: Some(Rc::new(valid)) }
                            };
                        }
                    }
                }
            }
            let a = eval_vec(&args[0], ctx);
            match &a.data {
                // parse straight from the string lane — no Val boxing, no Rc churn
                Data::Text(texts) => {
                    let mut valid = vec![0u8; (rows + 7) / 8];
                    if want_int {
                        let mut out = vec![0i64; rows];
                        for i in 0..rows {
                            if !a.is_valid(i) {
                                continue;
                            }
                            if let Ok(x) = texts[i].trim().parse::<i64>() {
                                out[i] = x;
                                valid[i / 8] |= 1 << (i % 8);
                            }
                        }
                        VV { data: Data::I64(Rc::new(out)), valid: Some(Rc::new(valid)) }
                    } else {
                        let mut out = vec![0f64; rows];
                        for i in 0..rows {
                            if !a.is_valid(i) {
                                continue;
                            }
                            if let Ok(x) = texts[i].trim().parse::<f64>() {
                                out[i] = x;
                                valid[i / 8] |= 1 << (i % 8);
                            }
                        }
                        VV { data: Data::F64(Rc::new(out)), valid: Some(Rc::new(valid)) }
                    }
                }
                // dictionary column: parse each distinct value once
                Data::Codes { codes, dict } => {
                    let table: Vec<Option<f64>> =
                        dict.iter().map(|s| s.trim().parse::<f64>().ok()).collect();
                    let mut valid = vec![0u8; (rows + 7) / 8];
                    let mut outf = vec![0f64; rows];
                    for i in 0..rows {
                        if !a.is_valid(i) {
                            continue;
                        }
                        if let Some(x) = table[codes[i] as usize] {
                            outf[i] = x;
                            valid[i / 8] |= 1 << (i % 8);
                        }
                    }
                    if want_int {
                        let out: Vec<i64> = outf.iter().map(|&x| x as i64).collect();
                        VV { data: Data::I64(Rc::new(out)), valid: Some(Rc::new(valid)) }
                    } else {
                        VV { data: Data::F64(Rc::new(outf)), valid: Some(Rc::new(valid)) }
                    }
                }
                Data::F64(v) if want_int => {
                    let out: Vec<i64> = v.iter().map(|&x| x as i64).collect();
                    VV { data: Data::I64(Rc::new(out)), valid: a.valid.clone() }
                }
                Data::I64(v) if !want_int => {
                    let out: Vec<f64> = v.iter().map(|&x| x as f64).collect();
                    VV { data: Data::F64(Rc::new(out)), valid: a.valid.clone() }
                }
                Data::I64(_) if want_int => a,
                Data::F64(_) if !want_int => a,
                _ => {
                    let items = vec![a];
                    lanes_to_vv(rows, ty, |i| {
                        scalar_fn(name, items.iter().map(|v| lane_val(v, i)).collect())
                    })
                }
            }
        }
        _ if TEMPORAL_FNS.contains(&name) => {
            let aty = args[temporal_arg_index(name)].ty();
            let items: Vec<VV> = args.iter().map(|a| eval_vec(a, ctx)).collect();
            lanes_to_vv(rows, ty, |i| {
                let vals: Vec<Val> = items.iter().map(|v| lane_val(v, i)).collect();
                temporal_fn(name, aty, &vals)
            })
        }
        // remaining scalars through lanes (cold path)
        _ => {
            let items: Vec<VV> = args.iter().map(|a| eval_vec(a, ctx)).collect();
            lanes_to_vv(rows, ty, |i| {
                let vals: Vec<Val> = items.iter().map(|v| lane_val(v, i)).collect();
                scalar_fn(name, vals)
            })
        }
    }
}

fn lane_val(v: &VV, i: usize) -> Val {
    if !v.is_valid(i) {
        return Val::Null;
    }
    match &v.data {
        Data::I64(x) => Val::Int(x[i]),
        Data::F64(x) => Val::Float(x[i]),
        Data::Bool(x) => Val::Bool(x[i] != 0),
        Data::Codes { codes, dict } => {
            dict.get(codes[i] as usize).map(|s| Val::Text(s.clone())).unwrap_or(Val::Null)
        }
        Data::Text(x) => Val::Text(x[i].clone()),
        Data::Const(c) => c.clone(),
    }
}

/// Build a typed vector from a per-lane Val producer (cold-op path).
fn lanes_to_vv(rows: usize, ty: Ty, f: impl Fn(usize) -> Val) -> VV {
    let mut valid = vec![0u8; (rows + 7) / 8];
    match ty {
        Ty::Int | Ty::Date | Ty::Timestamp => {
            let mut out = vec![0i64; rows];
            for i in 0..rows {
                match f(i) {
                    Val::Int(x) => {
                        out[i] = x;
                        valid[i / 8] |= 1 << (i % 8);
                    }
                    Val::Float(x) => {
                        out[i] = x as i64;
                        valid[i / 8] |= 1 << (i % 8);
                    }
                    _ => {}
                }
            }
            VV { data: Data::I64(Rc::new(out)), valid: Some(Rc::new(valid)) }
        }
        Ty::Float => {
            let mut out = vec![0f64; rows];
            for i in 0..rows {
                if let Some(x) = f(i).as_f64() {
                    out[i] = x;
                    valid[i / 8] |= 1 << (i % 8);
                }
            }
            VV { data: Data::F64(Rc::new(out)), valid: Some(Rc::new(valid)) }
        }
        Ty::Bool => {
            let mut out = vec![0u8; rows];
            for i in 0..rows {
                if let Val::Bool(b) = f(i) {
                    out[i] = b as u8;
                    valid[i / 8] |= 1 << (i % 8);
                }
            }
            VV { data: Data::Bool(Rc::new(out)), valid: Some(Rc::new(valid)) }
        }
        _ => {
            let empty = Rc::new(String::new());
            let mut out: Vec<VStr> = vec![empty; rows];
            for i in 0..rows {
                if let Val::Text(s) = f(i) {
                    out[i] = s;
                    valid[i / 8] |= 1 << (i % 8);
                }
            }
            VV { data: Data::Text(Rc::new(out)), valid: Some(Rc::new(valid)) }
        }
    }
}

const TEMPORAL_FNS: &[&str] =
    &["year", "month", "day", "hour", "minute", "second", "date", "timestamp", "strftime"];

/// The argument whose bound type disambiguates days-vs-ms for a temporal call.
fn temporal_arg_index(name: &str) -> usize {
    if name == "strftime" { 1 } else { 0 }
}

use facetful_format::time;

/// Temporal functions need the *bound type* of their argument (Date = days,
/// Timestamp = ms share Val::Int), so they bypass scalar_fn's untyped Vals.
fn temporal_fn(name: &str, aty: Ty, args: &[Val]) -> Val {
    let x = &args[temporal_arg_index(name)];
    if x.is_null() {
        return Val::Null;
    }
    // the temporal argument as (days, ms) where applicable
    let ms_of = |v: &Val| match (aty, v) {
        (Ty::Date, Val::Int(d)) => Some(d * time::MS_PER_DAY),
        (_, Val::Int(ms)) => Some(*ms),
        _ => None,
    };
    let days_of = |v: &Val| match (aty, v) {
        (Ty::Timestamp, Val::Int(ms)) => Some(ms.div_euclid(time::MS_PER_DAY)),
        (_, Val::Int(d)) => Some(*d),
        _ => None,
    };
    match name {
        "year" | "month" | "day" => {
            let Some(days) = days_of(x) else { return Val::Null };
            let (y, m, d) = time::civil_from_days(days);
            Val::Int(match name {
                "year" => y,
                "month" => m as i64,
                _ => d as i64,
            })
        }
        "hour" | "minute" | "second" => {
            let Some(ms) = ms_of(x) else { return Val::Null };
            let t = ms.rem_euclid(time::MS_PER_DAY) / 1000;
            Val::Int(match name {
                "hour" => t / 3600,
                "minute" => t / 60 % 60,
                _ => t % 60,
            })
        }
        "date" => match x {
            Val::Text(s) => time::parse_date(s).map(Val::Int).unwrap_or(Val::Null),
            _ => days_of(x).map(Val::Int).unwrap_or(Val::Null),
        },
        "timestamp" => match x {
            Val::Text(s) => time::parse_timestamp(s).map(Val::Int).unwrap_or(Val::Null),
            _ => ms_of(x).map(Val::Int).unwrap_or(Val::Null),
        },
        "strftime" => {
            let Val::Text(fmt) = &args[0] else { return Val::Null };
            let Some(ms) = ms_of(x) else { return Val::Null };
            let days = ms.div_euclid(time::MS_PER_DAY);
            let (y, m, d) = time::civil_from_days(days);
            let t = ms.rem_euclid(time::MS_PER_DAY) / 1000;
            let mut out = String::new();
            let mut it = fmt.chars();
            while let Some(c) = it.next() {
                if c != '%' {
                    out.push(c);
                    continue;
                }
                match it.next() {
                    Some('Y') => out.push_str(&format!("{y:04}")),
                    Some('m') => out.push_str(&format!("{m:02}")),
                    Some('d') => out.push_str(&format!("{d:02}")),
                    Some('H') => out.push_str(&format!("{:02}", t / 3600)),
                    Some('M') => out.push_str(&format!("{:02}", t / 60 % 60)),
                    Some('S') => out.push_str(&format!("{:02}", t % 60)),
                    Some('s') => out.push_str(&(ms.div_euclid(1000)).to_string()),
                    Some('%') => out.push('%'),
                    Some(other) => {
                        out.push('%');
                        out.push(other);
                    }
                    None => out.push('%'),
                }
            }
            Val::text(out)
        }
        _ => unreachable!("not a temporal function: {name}"),
    }
}

/// scalar function on materialized values — cold path + grouped-context eval
fn scalar_fn(name: &str, mut args: Vec<Val>) -> Val {
    match name {
        "isnull" => Val::Bool(args[0].is_null()),
        "coalesce" | "ifnull" => args.into_iter().find(|v| !v.is_null()).unwrap_or(Val::Null),
        "nullif" => {
            if args[0] == args[1] { Val::Null } else { args.swap_remove(0) }
        }
        "trim" | "ltrim" | "rtrim" => match (&args[0], args.get(1)) {
            (Val::Text(s), sel) => {
                // SQLite semantics: default trims spaces only; 2-arg form trims
                // any character present in the second argument
                let set: Vec<char> = match sel {
                    None => vec![' '],
                    Some(Val::Text(c)) => c.chars().collect(),
                    _ => return Val::Null,
                };
                let f = |c: char| set.contains(&c);
                Val::text(match name {
                    "trim" => s.trim_matches(f),
                    "ltrim" => s.trim_start_matches(f),
                    _ => s.trim_end_matches(f),
                })
            }
            _ => Val::Null,
        },
        "replace" => match (&args[0], &args[1], &args[2]) {
            (Val::Text(s), Val::Text(from), Val::Text(to)) => {
                // empty needle: SQLite returns the input unchanged
                if from.is_empty() { Val::Text(s.clone()) } else { Val::text(s.replace(&**from, to)) }
            }
            _ => Val::Null,
        },
        "instr" => match (&args[0], &args[1]) {
            // 1-based character position of the first occurrence, 0 = absent
            (Val::Text(s), Val::Text(sub)) => match s.find(&**sub) {
                Some(byte) => Val::Int(s[..byte].chars().count() as i64 + 1),
                None => Val::Int(0),
            },
            _ => Val::Null,
        },
        "sign" => match args[0].as_f64() {
            Some(f) if f > 0.0 => Val::Int(1),
            Some(f) if f < 0.0 => Val::Int(-1),
            Some(_) => Val::Int(0),
            None => Val::Null,
        },
        "sqrt" | "exp" | "ln" | "pow" | "power" => {
            let (Some(a), b) = (args[0].as_f64(), args.get(1).and_then(|v| v.as_f64())) else {
                return Val::Null;
            };
            let r = match name {
                "sqrt" => a.sqrt(),
                "exp" => a.exp(),
                "ln" => {
                    if a <= 0.0 { return Val::Null } else { a.ln() }
                }
                _ => match b {
                    Some(b) => a.powf(b),
                    None => return Val::Null,
                },
            };
            // out-of-domain (sqrt(-1), pow(-1, .5)) is NULL, matching SQLite
            if r.is_nan() { Val::Null } else { Val::Float(r) }
        }
        "in" => {
            let needle = args.remove(0);
            if needle.is_null() {
                return Val::Null;
            }
            let mut saw_null = false;
            for v in &args {
                match needle.eq_sql(v) {
                    Some(true) => return Val::Bool(true),
                    None => saw_null = true,
                    _ => {}
                }
            }
            if saw_null { Val::Null } else { Val::Bool(false) }
        }
        "between" => {
            let (x, lo, hi) = (args[0].clone(), args[1].clone(), args[2].clone());
            if x.is_null() || lo.is_null() || hi.is_null() {
                return Val::Null;
            }
            Val::Bool(
                x.cmp_sql(&lo) != core::cmp::Ordering::Less
                    && x.cmp_sql(&hi) != core::cmp::Ordering::Greater,
            )
        }
        "like" => match (&args[0], &args[1]) {
            (Val::Text(s), Val::Text(p)) => Val::Bool(like_match(p, s)),
            _ => Val::Null,
        },
        "if" | "case" => {
            let mut i = 0;
            while i + 1 < args.len() {
                if matches!(args[i], Val::Bool(true)) {
                    return args[i + 1].clone();
                }
                i += 2;
            }
            if args.len() % 2 == 1 { args.last().unwrap().clone() } else { Val::Null }
        }
        "concat" => {
            let mut out = String::new();
            for v in &args {
                match v {
                    Val::Null => return Val::Null,
                    Val::Text(s) => out.push_str(s),
                    Val::Int(i) => out.push_str(&i.to_string()),
                    Val::Float(f) => out.push_str(&f.to_string()),
                    Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                }
            }
            Val::text(out)
        }
        "lower" | "upper" => match &args[0] {
            Val::Text(s) => {
                Val::text(if name == "lower" { s.to_lowercase() } else { s.to_uppercase() })
            }
            _ => Val::Null,
        },
        "length" => match &args[0] {
            Val::Text(s) => Val::Int(s.chars().count() as i64),
            _ => Val::Null,
        },
        "substr" => match &args[0] {
            Val::Text(s) => {
                let start = match args.get(1).and_then(Val::as_f64) {
                    Some(f) => (f as i64 - 1).max(0) as usize,
                    None => return Val::Null,
                };
                let len = args.get(2).and_then(Val::as_f64).map(|f| f as usize);
                let chars: Vec<char> = s.chars().collect();
                let end = len.map(|l| (start + l).min(chars.len())).unwrap_or(chars.len());
                if start >= chars.len() {
                    Val::text("")
                } else {
                    Val::text(chars[start..end].iter().collect::<String>())
                }
            }
            _ => Val::Null,
        },
        "abs" => match &args[0] {
            Val::Int(i) => Val::Int(i.abs()),
            Val::Float(f) => Val::Float(f.abs()),
            _ => Val::Null,
        },
        "floor" | "ceil" | "round" => match args[0].as_f64() {
            Some(f) => {
                let r = match name {
                    "floor" => f.floor(),
                    "ceil" => f.ceil(),
                    _ => {
                        let digits = args.get(1).and_then(Val::as_f64).unwrap_or(0.0) as i32;
                        let m = 10f64.powi(digits);
                        (f * m).round() / m
                    }
                };
                if matches!(args[0], Val::Int(_)) && name != "round" {
                    Val::Int(r as i64)
                } else {
                    Val::Float(r)
                }
            }
            None => Val::Null,
        },
        "int" => match &args[0] {
            Val::Int(i) => Val::Int(*i),
            Val::Float(f) => Val::Int(*f as i64),
            Val::Text(s) => s.trim().parse::<i64>().map(Val::Int).unwrap_or(Val::Null),
            Val::Bool(b) => Val::Int(*b as i64),
            Val::Null => Val::Null,
        },
        "float" => match &args[0] {
            Val::Int(i) => Val::Float(*i as f64),
            Val::Float(f) => Val::Float(*f),
            Val::Text(s) => s.trim().parse::<f64>().map(Val::Float).unwrap_or(Val::Null),
            Val::Bool(b) => Val::Float(*b as i64 as f64),
            Val::Null => Val::Null,
        },
        "text" => match &args[0] {
            Val::Null => Val::Null,
            Val::Text(s) => Val::Text(s.clone()),
            Val::Int(i) => Val::text(i.to_string()),
            Val::Float(f) => Val::text(f.to_string()),
            Val::Bool(b) => Val::text(if *b { "true" } else { "false" }),
        },
        other => unreachable!("unbound scalar function {other}"),
    }
}

// ---------------- aggregation ----------------

enum AggAcc {
    Count(Vec<i64>),
    /// One bitmap per SQL group; dictionary codes are file-global.
    DistinctCodes { bits: Vec<Vec<u64>>, words: usize },
    DistinctNum(Vec<std::collections::HashSet<u64>>),
    DistinctStr(Vec<std::collections::HashSet<VStr>>),
    SumI { v: Vec<i64>, any: Vec<bool> },
    SumF { v: Vec<f64>, any: Vec<bool> },
    Avg { sum: Vec<f64>, n: Vec<i64> },
    MinMaxNum { v: Vec<f64>, seen: Vec<bool>, is_min: bool, int: bool },
    MinMaxStr { v: Vec<Option<VStr>>, is_min: bool },
    /// keeps every value; finish() selects — memory is O(group rows)
    Median(Vec<Vec<f64>>),
    /// sample stddev via (n, Σx, Σx²)
    Stddev { n: Vec<i64>, sum: Vec<f64>, sumsq: Vec<f64> },
    GroupConcat { v: Vec<Option<String>>, sep: String },
}

/// Row source for one aggregation pass: explicit per-row group ids, or (for
/// ungrouped queries) the raw keep mask with the row count.
#[derive(Clone, Copy)]
enum RowsSrc<'a> {
    Gids(&'a [u32]),
    Mask { keep: Option<&'a [u8]>, n: usize },
}

impl AggAcc {
    /// `dict_len`: cardinality when the argument is a direct dictionary column.
    fn new(call: &Bound, dict_len: Option<usize>) -> AggAcc {
        let Bound::Call { func, args, .. } = call else { unreachable!() };
        let aty = args[0].ty();
        match func.name {
            "count" => AggAcc::Count(Vec::new()),
            "count_distinct" => {
                if let Some(len) = dict_len {
                    AggAcc::DistinctCodes { bits: Vec::new(), words: (len + 63) / 64 }
                } else if matches!(aty, Ty::Int | Ty::Float | Ty::Bool | Ty::Date | Ty::Timestamp) {
                    AggAcc::DistinctNum(Vec::new())
                } else {
                    AggAcc::DistinctStr(Vec::new())
                }
            }
            "sum" => {
                if matches!(aty, Ty::Int | Ty::Date | Ty::Timestamp) {
                    AggAcc::SumI { v: Vec::new(), any: Vec::new() }
                } else {
                    AggAcc::SumF { v: Vec::new(), any: Vec::new() }
                }
            }
            "avg" => AggAcc::Avg { sum: Vec::new(), n: Vec::new() },
            "median" => AggAcc::Median(Vec::new()),
            "stddev" => AggAcc::Stddev { n: Vec::new(), sum: Vec::new(), sumsq: Vec::new() },
            "group_concat" => {
                // binder guarantees the separator is a text literal
                let sep = match args.get(1) {
                    Some(Bound::Str(s)) => s.clone(),
                    _ => ",".to_string(),
                };
                AggAcc::GroupConcat { v: Vec::new(), sep }
            }
            "min" | "max" => {
                let is_min = func.name == "min";
                if aty == Ty::Text {
                    AggAcc::MinMaxStr { v: Vec::new(), is_min }
                } else {
                    AggAcc::MinMaxNum {
                        v: Vec::new(),
                        seen: Vec::new(),
                        is_min,
                        int: matches!(aty, Ty::Int | Ty::Date | Ty::Timestamp),
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    fn grow(&mut self, n: usize) {
        // Dense bitmaps win for few groups, but a large dictionary crossed
        // with many tiny groups would waste memory. Cap them at 8 MiB per
        // aggregate, then retain only observed codes in the sparse fallback.
        if let AggAcc::DistinctCodes { bits, words } = self {
            if n > (1 << 20) / (*words).max(1) {
                let sets = bits.iter().map(|bitmap| {
                    let mut set = std::collections::HashSet::new();
                    for (word_index, &word) in bitmap.iter().enumerate() {
                        let mut remaining = word;
                        while remaining != 0 {
                            set.insert((word_index * 64 + remaining.trailing_zeros() as usize) as u64);
                            remaining &= remaining - 1;
                        }
                    }
                    set
                }).collect();
                *self = AggAcc::DistinctNum(sets);
            }
        }
        match self {
            AggAcc::Count(v) => v.resize(n, 0),
            AggAcc::DistinctCodes { bits, words } => bits.resize_with(n, || vec![0; *words]),
            AggAcc::DistinctNum(v) => v.resize_with(n, Default::default),
            AggAcc::DistinctStr(v) => v.resize_with(n, Default::default),
            AggAcc::SumI { v, any } => {
                v.resize(n, 0);
                any.resize(n, false);
            }
            AggAcc::SumF { v, any } => {
                v.resize(n, 0.0);
                any.resize(n, false);
            }
            AggAcc::Avg { sum, n: c } => {
                sum.resize(n, 0.0);
                c.resize(n, 0);
            }
            AggAcc::MinMaxNum { v, seen, .. } => {
                v.resize(n, 0.0);
                seen.resize(n, false);
            }
            AggAcc::MinMaxStr { v, .. } => v.resize(n, None),
            AggAcc::Median(v) => v.resize_with(n, Default::default),
            AggAcc::Stddev { n: c, sum, sumsq } => {
                c.resize(n, 0);
                sum.resize(n, 0.0);
                sumsq.resize(n, 0.0);
            }
            AggAcc::GroupConcat { v, .. } => v.resize(n, None),
        }
    }

    /// One pass over the group: specialized loops per (accumulator, arg shape).
    /// `src` is either per-row group ids or (for ungrouped queries) the raw
    /// keep mask — the latter never materializes a gids vector.
    /// (RowsSrc is defined just above `impl AggAcc`.)
    fn update_batch(&mut self, src: RowsSrc<'_>, arg: &VV) {
        macro_rules! for_kept {
            (|$i:ident, $g:ident| $body:expr) => {
                match src {
                    RowsSrc::Gids(gids) => {
                        for $i in 0..gids.len() {
                            let $g = gids[$i];
                            if $g == u32::MAX {
                                continue;
                            }
                            let $g = $g as usize;
                            $body
                        }
                    }
                    RowsSrc::Mask { keep: None, n } => {
                        for $i in 0..n {
                            let $g = 0usize;
                            $body
                        }
                    }
                    RowsSrc::Mask { keep: Some(k), n } => {
                        for $i in 0..n {
                            if k[$i] == 0 {
                                continue;
                            }
                            let $g = 0usize;
                            $body
                        }
                    }
                }
            };
        }
        match (&mut *self, &arg.data) {
            (AggAcc::Count(c), Data::Const(v)) => {
                if !v.is_null() {
                    // ungrouped count(*): the mask popcount is the answer
                    if let RowsSrc::Mask { keep, n } = src {
                        c[0] += match keep {
                            None => n as i64,
                            Some(k) => k.iter().filter(|&&b| b != 0).count() as i64,
                        };
                        return;
                    }
                    for_kept!(|i, g| {
                        let _ = i;
                        c[g] += 1
                    });
                }
            }
            (AggAcc::Count(c), _) => match &arg.valid {
                None => for_kept!(|i, g| {
                    let _ = i;
                    c[g] += 1
                }),
                Some(vb) => for_kept!(|i, g| {
                    c[g] += (vb[i / 8] >> (i % 8) & 1) as i64;
                }),
            },
            (AggAcc::SumF { v, any }, Data::F64(x)) => match &arg.valid {
                None => for_kept!(|i, g| {
                    v[g] += x[i];
                    any[g] = true;
                }),
                Some(vb) => for_kept!(|i, g| {
                    if vb[i / 8] >> (i % 8) & 1 != 0 {
                        v[g] += x[i];
                        any[g] = true;
                    }
                }),
            },
            (AggAcc::SumF { v, any }, Data::I64(x)) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    v[g] += x[i] as f64;
                    any[g] = true;
                }
            }),
            (AggAcc::SumI { v, any }, Data::I64(x)) => match &arg.valid {
                None => for_kept!(|i, g| {
                    v[g] += x[i];
                    any[g] = true;
                }),
                Some(vb) => for_kept!(|i, g| {
                    if vb[i / 8] >> (i % 8) & 1 != 0 {
                        v[g] += x[i];
                        any[g] = true;
                    }
                }),
            },
            (AggAcc::Avg { sum, n }, Data::F64(x)) => match &arg.valid {
                None => for_kept!(|i, g| {
                    sum[g] += x[i];
                    n[g] += 1;
                }),
                Some(vb) => for_kept!(|i, g| {
                    if vb[i / 8] >> (i % 8) & 1 != 0 {
                        sum[g] += x[i];
                        n[g] += 1;
                    }
                }),
            },
            (AggAcc::Avg { sum, n }, Data::I64(x)) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    sum[g] += x[i] as f64;
                    n[g] += 1;
                }
            }),
            (AggAcc::MinMaxNum { v, seen, is_min, .. }, Data::F64(_) | Data::I64(_)) => {
                let is_min = *is_min;
                for_kept!(|i, g| {
                    if arg.is_valid(i) {
                        let x = arg.f64_at(i);
                        if !seen[g] || (is_min && x < v[g]) || (!is_min && x > v[g]) {
                            v[g] = x;
                            seen[g] = true;
                        }
                    }
                });
            }
            (AggAcc::DistinctCodes { bits, .. }, Data::Codes { codes, dict }) => match &arg.valid {
                None => for_kept!(|i, g| {
                    let code = codes[i] as usize;
                    // Invalid codes are NULL, including codes in bitmap padding.
                    if code < dict.len() {
                        bits[g][code / 64] |= 1u64 << (code % 64);
                    }
                }),
                Some(vb) => for_kept!(|i, g| {
                    if vb[i / 8] >> (i % 8) & 1 != 0 {
                        let code = codes[i] as usize;
                        if code < dict.len() {
                            bits[g][code / 64] |= 1u64 << (code % 64);
                        }
                    }
                }),
            },
            (AggAcc::DistinctNum(sets), Data::Codes { codes, dict }) => for_kept!(|i, g| {
                if arg.is_valid(i) && (codes[i] as usize) < dict.len() {
                    sets[g].insert(codes[i] as u64);
                }
            }),
            (AggAcc::DistinctNum(sets), Data::I64(x)) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    sets[g].insert(x[i] as u64);
                }
            }),
            (AggAcc::DistinctNum(sets), Data::F64(x)) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    sets[g].insert(x[i].to_bits());
                }
            }),
            (AggAcc::DistinctStr(sets), _) => for_kept!(|i, g| {
                if let Some(t) = arg.text_at(i) {
                    sets[g].insert(t);
                }
            }),
            (AggAcc::Median(vs), _) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    vs[g].push(arg.f64_at(i));
                }
            }),
            (AggAcc::Stddev { n, sum, sumsq }, _) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    let x = arg.f64_at(i);
                    n[g] += 1;
                    sum[g] += x;
                    sumsq[g] += x * x;
                }
            }),
            (AggAcc::GroupConcat { v, sep }, _) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    let piece = match lane_val(arg, i) {
                        Val::Text(s) => s.to_string(),
                        Val::Int(x) => x.to_string(),
                        Val::Float(x) => x.to_string(),
                        Val::Bool(b) => (if b { "1" } else { "0" }).to_string(),
                        Val::Null => continue,
                    };
                    match &mut v[g] {
                        Some(acc) => {
                            acc.push_str(sep);
                            acc.push_str(&piece);
                        }
                        None => v[g] = Some(piece),
                    }
                }
            }),
            // generic fallbacks (consts, computed vectors, text min/max)
            (acc, _) => {
                for_kept!(|i, g| {
                    if !arg.is_valid(i) {
                        continue;
                    }
                    match acc {
                        AggAcc::SumI { v, any } => {
                            v[g] += arg.i64_at(i);
                            any[g] = true;
                        }
                        AggAcc::SumF { v, any } => {
                            v[g] += arg.f64_at(i);
                            any[g] = true;
                        }
                        AggAcc::Avg { sum, n } => {
                            sum[g] += arg.f64_at(i);
                            n[g] += 1;
                        }
                        AggAcc::MinMaxNum { v, seen, is_min, .. } => {
                            let x = arg.f64_at(i);
                            if !seen[g] || (*is_min && x < v[g]) || (!*is_min && x > v[g]) {
                                v[g] = x;
                                seen[g] = true;
                            }
                        }
                        AggAcc::MinMaxStr { v, is_min } => {
                            if let Some(t) = arg.text_at(i) {
                                let better = match &v[g] {
                                    None => true,
                                    Some(cur) => {
                                        if *is_min { t < *cur } else { t > *cur }
                                    }
                                };
                                if better {
                                    v[g] = Some(t);
                                }
                            }
                        }
                        AggAcc::DistinctNum(sets) => {
                            let bits = match lane_val(arg, i) {
                                Val::Int(x) => x as u64,
                                Val::Float(x) => x.to_bits(),
                                Val::Bool(b) => b as u64,
                                _ => continue,
                            };
                            sets[g].insert(bits);
                        }
                        AggAcc::Count(c) => c[g] += 1,
                        // these have shape-generic arms above the fallback
                        AggAcc::DistinctCodes { .. }
                        | AggAcc::DistinctStr(_)
                        | AggAcc::Median(_)
                        | AggAcc::Stddev { .. }
                        | AggAcc::GroupConcat { .. } => unreachable!(),
                    }
                });
            }
        }
    }

    fn finish(&self, gid: usize) -> Val {
        match self {
            AggAcc::Count(v) => Val::Int(v[gid]),
            AggAcc::DistinctCodes { bits, .. } => {
                Val::Int(bits[gid].iter().map(|word| word.count_ones() as i64).sum())
            }
            AggAcc::DistinctNum(v) => Val::Int(v[gid].len() as i64),
            AggAcc::DistinctStr(v) => Val::Int(v[gid].len() as i64),
            AggAcc::SumI { v, any } => {
                if any[gid] { Val::Int(v[gid]) } else { Val::Null }
            }
            AggAcc::SumF { v, any } => {
                if any[gid] { Val::Float(v[gid]) } else { Val::Null }
            }
            AggAcc::Avg { sum, n } => {
                if n[gid] == 0 { Val::Null } else { Val::Float(sum[gid] / n[gid] as f64) }
            }
            AggAcc::MinMaxNum { v, seen, int, .. } => {
                if !seen[gid] {
                    Val::Null
                } else if *int {
                    Val::Int(v[gid] as i64)
                } else {
                    Val::Float(v[gid])
                }
            }
            AggAcc::MinMaxStr { v, .. } => {
                v[gid].as_ref().map(|s| Val::Text(s.clone())).unwrap_or(Val::Null)
            }
            AggAcc::Median(vs) => {
                let src = &vs[gid];
                if src.is_empty() {
                    return Val::Null;
                }
                let mut v = src.clone();
                v.sort_unstable_by(f64::total_cmp);
                let m = v.len() / 2;
                Val::Float(if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
            }
            AggAcc::Stddev { n, sum, sumsq } => {
                let c = n[gid];
                if c < 2 {
                    return Val::Null;
                }
                let mean = sum[gid] / c as f64;
                let var = (sumsq[gid] - sum[gid] * mean) / (c - 1) as f64;
                Val::Float(var.max(0.0).sqrt())
            }
            AggAcc::GroupConcat { v, .. } => {
                v[gid].as_ref().map(|s| Val::text(s.clone())).unwrap_or(Val::Null)
            }
        }
    }
}

/// Gather one select expression column-wise over ordered (gslot, row) refs.
/// `vvs[gslot]` = evaluated VV per group; `raw[gslot]` = (offsets, bytes,
/// validity) when the expr is a direct plain-text column (skips VStr lanes).
enum SelSrc {
    Vv(Vec<VV>),
    RawText(Vec<(Rc<Vec<u32>>, Rc<Vec<u8>>, Option<Rc<Vec<u8>>>)>),
}

fn gather_outcol(src: &SelSrc, refs: &[(u32, u32)], ty: Ty) -> OutCol {
    let n = refs.len();
    let mut valid = vec![0u8; n.div_ceil(8)];
    match src {
        SelSrc::RawText(groups) => {
            let mut offsets = Vec::with_capacity(n + 1);
            offsets.push(0u32);
            let mut bytes = Vec::with_capacity(n * 16);
            for (i, &(g, r)) in refs.iter().enumerate() {
                let (offs, blob, v) = &groups[g as usize];
                let r = r as usize;
                if v.as_deref().map_or(true, |vb| vb[r / 8] >> (r % 8) & 1 != 0) {
                    valid[i / 8] |= 1 << (i % 8);
                }
                bytes.extend_from_slice(&blob[offs[r] as usize..offs[r + 1] as usize]);
                offsets.push(bytes.len() as u32);
            }
            OutCol::Text { offsets, bytes, valid }
        }
        SelSrc::Vv(vvs) => match ty {
            Ty::Float => {
                let mut v = vec![0f64; n];
                for (i, &(g, r)) in refs.iter().enumerate() {
                    let vv = &vvs[g as usize];
                    if vv.is_valid(r as usize) {
                        v[i] = vv.f64_at(r as usize);
                        valid[i / 8] |= 1 << (i % 8);
                    }
                }
                OutCol::F64 { v, valid }
            }
            Ty::Int | Ty::Date | Ty::Timestamp => {
                let mut v = vec![0i64; n];
                for (i, &(g, r)) in refs.iter().enumerate() {
                    let vv = &vvs[g as usize];
                    if vv.is_valid(r as usize) {
                        v[i] = vv.i64_at(r as usize);
                        valid[i / 8] |= 1 << (i % 8);
                    }
                }
                OutCol::I64 { v, valid }
            }
            Ty::Bool => {
                let mut v = vec![0u8; n];
                for (i, &(g, r)) in refs.iter().enumerate() {
                    let vv = &vvs[g as usize];
                    if let Some(b) = vv.bool3_at(r as usize) {
                        v[i] = b as u8;
                        valid[i / 8] |= 1 << (i % 8);
                    }
                }
                OutCol::Bool { v, valid }
            }
            _ => {
                // text-valued expressions (computed or dict): through the lane
                let mut offsets = Vec::with_capacity(n + 1);
                offsets.push(0u32);
                let mut bytes = Vec::new();
                for (i, &(g, r)) in refs.iter().enumerate() {
                    let vv = &vvs[g as usize];
                    if let Some(t) = vv.text_at(r as usize) {
                        bytes.extend_from_slice(t.as_bytes());
                        valid[i / 8] |= 1 << (i % 8);
                    }
                    offsets.push(bytes.len() as u32);
                }
                OutCol::Text { offsets, bytes, valid }
            }
        },
    }
}

/// Build per-select sources for one group: direct plain-text columns give raw
/// blob access, everything else evaluates to a VV.
fn sel_srcs_for_group(
    q: &super::binder::BoundQuery,
    ctx: &GroupCtx,
    srcs: &mut [Option<SelSrc>],
) {
    for (si, sel) in q.select.iter().enumerate() {
        let raw = match &sel.expr {
            Bound::Column { index, ty: Ty::Text } => match ctx.cols.get(index) {
                Some((GroupCol::Text { offsets, bytes, .. }, validity)) => {
                    Some((offsets.clone(), bytes.clone(), validity.clone()))
                }
                _ => None,
            },
            _ => None,
        };
        match (&mut srcs[si], raw) {
            (Some(SelSrc::RawText(v)), Some(r)) => v.push(r),
            (slot @ None, Some(r)) => *slot = Some(SelSrc::RawText(vec![r])),
            (Some(SelSrc::Vv(v)), None) => v.push(eval_vec(&sel.expr, ctx)),
            (slot @ None, None) => *slot = Some(SelSrc::Vv(vec![eval_vec(&sel.expr, ctx)])),
            _ => unreachable!("select expr shape is stable across groups"),
        }
    }
}

fn collect_aggs(b: &Bound, out: &mut Vec<Bound>) {
    match b {
        Bound::Call { func, .. } if func.kind == FuncKind::Aggregate => {
            if !out.contains(b) {
                out.push(b.clone());
            }
        }
        Bound::Call { args, .. } => args.iter().for_each(|a| collect_aggs(a, out)),
        Bound::Unary { expr, .. } => collect_aggs(expr, out),
        Bound::Binary { lhs, rhs, .. } => {
            collect_aggs(lhs, out);
            collect_aggs(rhs, out);
        }
        _ => {}
    }
}

fn collect_columns(b: &Bound, out: &mut Vec<usize>) {
    match b {
        Bound::Column { index, .. } => out.push(*index),
        Bound::Call { args, .. } => args.iter().for_each(|a| collect_columns(a, out)),
        Bound::Unary { expr, .. } => collect_columns(expr, out),
        Bound::Binary { lhs, rhs, .. } => {
            collect_columns(lhs, out);
            collect_columns(rhs, out);
        }
        _ => {}
    }
}

// grouped-context evaluation (per output group — groups are few)
struct Overrides<'a> {
    group_by: &'a [Bound],
    key: &'a [Val],
    aggs: &'a [Bound],
    accs: &'a [AggAcc],
    gid: usize,
}

fn eval_grouped(b: &Bound, o: &Overrides<'_>) -> Val {
    if let Some(i) = o.aggs.iter().position(|a| a == b) {
        return o.accs[i].finish(o.gid);
    }
    if let Some(i) = o.group_by.iter().position(|g| g == b) {
        return o.key[i].clone();
    }
    match b {
        Bound::Number(n, is_float) => {
            if *is_float || n.fract() != 0.0 {
                Val::Float(*n)
            } else {
                Val::Int(*n as i64)
            }
        }
        Bound::Str(s) => Val::text(s.clone()),
        Bound::Null => Val::Null,
        Bound::Unary { op, expr, .. } => {
            let v = eval_grouped(expr, o);
            match (op, v) {
                (_, Val::Null) => Val::Null,
                (UnOp::Neg, Val::Int(i)) => Val::Int(-i),
                (UnOp::Neg, Val::Float(f)) => Val::Float(-f),
                (UnOp::Not, Val::Bool(x)) => Val::Bool(!x),
                _ => Val::Null,
            }
        }
        Bound::Binary { op, lhs, rhs, ty } => {
            let l = eval_grouped(lhs, o);
            let r = eval_grouped(rhs, o);
            scalar_binary(*op, l, r, *ty)
        }
        Bound::Call { func, args, .. } => {
            let vals: Vec<Val> = args.iter().map(|a| eval_grouped(a, o)).collect();
            if TEMPORAL_FNS.contains(&func.name) {
                temporal_fn(func.name, args[temporal_arg_index(func.name)].ty(), &vals)
            } else {
                scalar_fn(func.name, vals)
            }
        }
        Bound::Column { .. } => Val::Null,
    }
}

fn scalar_binary(op: BinOp, l: Val, r: Val, ty: Ty) -> Val {
    use BinOp::*;
    match op {
        And => match (as_b3(&l), as_b3(&r)) {
            (Some(false), _) | (_, Some(false)) => Val::Bool(false),
            (Some(true), Some(true)) => Val::Bool(true),
            _ => Val::Null,
        },
        Or => match (as_b3(&l), as_b3(&r)) {
            (Some(true), _) | (_, Some(true)) => Val::Bool(true),
            (Some(false), Some(false)) => Val::Bool(false),
            _ => Val::Null,
        },
        Eq | Ne | Lt | Le | Gt | Ge => {
            if l.is_null() || r.is_null() {
                return Val::Null;
            }
            Val::Bool(cmp_ord(op, l.cmp_sql(&r)))
        }
        _ => {
            if l.is_null() || r.is_null() {
                return Val::Null;
            }
            match (ty, &l, &r) {
                (Ty::Int, Val::Int(a), Val::Int(b)) => match op {
                    Add => Val::Int(a + b),
                    Sub => Val::Int(a - b),
                    Mul => Val::Int(a * b),
                    Div => {
                        if *b == 0 { Val::Null } else { Val::Int(a / b) }
                    }
                    _ => {
                        if *b == 0 { Val::Null } else { Val::Int(a % b) }
                    }
                },
                _ => match (l.as_f64(), r.as_f64()) {
                    (Some(a), Some(b)) => match op {
                        Add => Val::Float(a + b),
                        Sub => Val::Float(a - b),
                        Mul => Val::Float(a * b),
                        Div => {
                            if b == 0.0 { Val::Null } else { Val::Float(a / b) }
                        }
                        _ => {
                            if b == 0.0 { Val::Null } else { Val::Float(a % b) }
                        }
                    },
                    _ => Val::Null,
                },
            }
        }
    }
}

fn as_b3(v: &Val) -> Option<bool> {
    match v {
        Val::Bool(b) => Some(*b),
        _ => None,
    }
}

// ---------------- pruning ----------------

type Range = (usize, Option<f64>, Option<f64>);

fn collect_ranges(f: &Bound) -> Vec<Range> {
    let mut out = Vec::new();
    walk_and(f, &mut out);
    out
}

fn walk_and(b: &Bound, out: &mut Vec<Range>) {
    match b {
        Bound::Binary { op: BinOp::And, lhs, rhs, .. } => {
            walk_and(lhs, out);
            walk_and(rhs, out);
        }
        Bound::Binary { op, lhs, rhs, .. } => {
            let (col, lit, flipped) = match (&**lhs, &**rhs) {
                (Bound::Column { index, .. }, Bound::Number(n, _)) => (*index, *n, false),
                (Bound::Number(n, _), Bound::Column { index, .. }) => (*index, *n, true),
                _ => return,
            };
            let op = if flipped {
                match op {
                    BinOp::Lt => BinOp::Gt,
                    BinOp::Le => BinOp::Ge,
                    BinOp::Gt => BinOp::Lt,
                    BinOp::Ge => BinOp::Le,
                    o => *o,
                }
            } else {
                *op
            };
            match op {
                BinOp::Eq => out.push((col, Some(lit), Some(lit))),
                BinOp::Lt | BinOp::Le => out.push((col, None, Some(lit))),
                BinOp::Gt | BinOp::Ge => out.push((col, Some(lit), None)),
                _ => {}
            }
        }
        Bound::Call { func, args, .. } if func.name == "between" => {
            if let (Bound::Column { index, .. }, Bound::Number(lo, _), Bound::Number(hi, _)) =
                (&args[0], &args[1], &args[2])
            {
                out.push((*index, Some(*lo), Some(*hi)));
            }
        }
        _ => {}
    }
}

fn group_prunable<S: ReadAt>(table: &Table<S>, g: usize, constraints: &[Range]) -> bool {
    use crate::format::Stats;
    for (col, lo, hi) in constraints {
        let stats = &table.catalog().groups[g].cols[*col].stats;
        let (smin, smax) = match stats {
            Stats::Int { min, max } => (*min as f64, *max as f64),
            Stats::Float { min, max } => (*min, *max),
            Stats::None => continue,
        };
        if lo.map_or(false, |l| smax < l) || hi.map_or(false, |h| smin > h) {
            return true;
        }
    }
    false
}

// ---------------- filter conjuncts (mask cache units) ----------------

/// One top-level AND operand of the WHERE clause, with what the mask cache
/// needs to key, load and (for contains-LIKE) narrow it.
struct Conjunct {
    expr: Bound,
    /// Canonical key: the bound tree's Debug form — names are resolved to
    /// column indices, literals are typed, function identity is by def. Two
    /// spellings of the same predicate bind to the same tree.
    key: String,
    cols: Vec<usize>,
    like: Option<LikeKey>,
}

/// `col <cmp> integral-literal` (either side) over a stored integer column,
/// as an inclusive range test plus an invert flag (Ne). The filter fast path
/// compares straight off the raw narrow segment with this.
fn int_cmp_lit(b: &Bound) -> Option<(usize, i64, i64, bool)> {
    let Bound::Binary { op, lhs, rhs, .. } = b else { return None };
    let (col, lit, op) = match (&**lhs, &**rhs) {
        (Bound::Column { index, ty }, Bound::Number(n, _))
            if matches!(ty, Ty::Int | Ty::Date | Ty::Timestamp) && n.fract() == 0.0 =>
        {
            (*index, *n, *op)
        }
        (Bound::Number(n, _), Bound::Column { index, ty })
            if matches!(ty, Ty::Int | Ty::Date | Ty::Timestamp) && n.fract() == 0.0 =>
        {
            // mirror: lit < x  ==  x > lit
            let m = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Le => BinOp::Ge,
                BinOp::Gt => BinOp::Lt,
                BinOp::Ge => BinOp::Le,
                other => *other,
            };
            (*index, *n, m)
        }
        _ => return None,
    };
    if lit.abs() > 9e15 {
        return None; // not exactly representable — leave to the general path
    }
    let lit = lit as i64;
    Some(match op {
        BinOp::Eq => (col, lit, lit, false),
        BinOp::Ne => (col, lit, lit, true),
        BinOp::Lt => (col, i64::MIN, lit.checked_sub(1)?, false),
        BinOp::Le => (col, i64::MIN, lit, false),
        BinOp::Gt => (col, lit.checked_add(1)?, i64::MAX, false),
        BinOp::Ge => (col, lit, i64::MAX, false),
        _ => return None,
    })
}

fn split_conjuncts(b: &Bound, out: &mut Vec<Bound>) {
    match b {
        Bound::Binary { op: BinOp::And, lhs, rhs, .. } => {
            split_conjuncts(lhs, out);
            split_conjuncts(rhs, out);
        }
        _ => out.push(b.clone()),
    }
}

fn conjuncts_of<S: ReadAt>(table: &Table<S>, filter: Option<&Bound>) -> Vec<Conjunct> {
    let mut parts = Vec::new();
    if let Some(f) = filter {
        split_conjuncts(f, &mut parts);
    }
    parts
        .into_iter()
        .map(|expr| {
            let mut cols = Vec::new();
            collect_columns(&expr, &mut cols);
            cols.sort_unstable();
            cols.dedup();
            // contains-LIKE over a plain text column: narrowable through a
            // cached superset (dict columns are already ~free per row)
            let like = match &expr {
                Bound::Call { func, args, .. } if func.name == "like" => match (&args[0], &args[1]) {
                    (Bound::Column { index, .. }, Bound::Str(p))
                        if !table.catalog().schema.columns[*index].is_dict() =>
                    {
                        match classify_like(p) {
                            LikeShape::Contains(n) => Some(LikeKey { col: *index, needle: n }),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                _ => None,
            };
            // LIKE is ASCII-case-insensitive, so `%Coal%` and `%coal%` are one
            // mask: key those on the folded needle rather than the literal
            let key = match &like {
                Some(lk) => format!("like:{}:{}", lk.col, lk.needle),
                None => format!("{expr:?}"),
            };
            Conjunct { key, expr, cols, like }
        })
        .collect()
}

fn pack_bits(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; bytes.len().div_ceil(8)];
    let mut chunks = bytes.chunks_exact(8);
    for (o, ch) in out.iter_mut().zip(&mut chunks) {
        // branchless byte-at-a-time pack — the bit-indexed loop was a
        // read-modify-write with a data-dependent branch per row
        *o = (ch[0] != 0) as u8
            | ((ch[1] != 0) as u8) << 1
            | ((ch[2] != 0) as u8) << 2
            | ((ch[3] != 0) as u8) << 3
            | ((ch[4] != 0) as u8) << 4
            | ((ch[5] != 0) as u8) << 5
            | ((ch[6] != 0) as u8) << 6
            | ((ch[7] != 0) as u8) << 7;
    }
    for (i, &b) in chunks.remainder().iter().enumerate() {
        if b != 0 {
            let n = bytes.len() / 8 * 8 + i;
            out[n / 8] |= 1 << (n % 8);
        }
    }
    out
}

// ---------------- grouping plans ----------------

/// Direct-index grouping: all group-bys are dict columns with a small
/// cardinality product — gid from arithmetic on codes, no hashing.
/// How one group-by dimension maps to a dense code.
enum DirectDim {
    /// dictionary column: the code lane is the dense code
    Dict,
    /// integer column with a small global value range: `value - min`
    Int { min: i64 },
}

struct DirectGroups {
    cols: Vec<usize>,
    dims: Vec<DirectDim>,
    cards: Vec<usize>,
    dense: Vec<i32>,
}

/// Global (all-groups) integer min/max from the footer stats, if every group
/// has them.
fn int_range<S: ReadAt>(table: &Table<S>, col: usize) -> Option<(i64, i64)> {
    use crate::format::Stats;
    let mut r: Option<(i64, i64)> = None;
    for g in &table.catalog().groups {
        match g.cols[col].stats {
            Stats::Int { min, max } => {
                let (lo, hi) = r.unwrap_or((min, max));
                r = Some((lo.min(min), hi.max(max)));
            }
            _ => return None,
        }
    }
    r
}

fn direct_plan<S: ReadAt>(table: &mut Table<S>, group_by: &[Bound]) -> Option<DirectGroups> {
    let mut cols = Vec::new();
    let mut dims = Vec::new();
    let mut cards = Vec::new();
    let mut product: usize = 1;
    for g in group_by {
        let Bound::Column { index, .. } = g else { return None };
        let (cty, is_dict) = {
            let def = &table.catalog().schema.columns[*index];
            (def.ty, def.is_dict())
        };
        let int_lane = matches!(
            cty,
            ColumnType::Int8
                | ColumnType::Int16
                | ColumnType::Int32
                | ColumnType::Int64
                | ColumnType::Date
                | ColumnType::Timestamp
        ) && !is_dict;
        let card = if is_dict {
            dims.push(DirectDim::Dict);
            table.dictionary(*index).ok()?.len() + 1 // extra lane for nulls
        } else if let Some((min, max)) = int_lane.then(|| int_range(table, *index)).flatten() {
            // narrow-range integers (years, small ids, dates) group densely too
            let range = usize::try_from(max.checked_sub(min)?).ok()?.checked_add(1)?;
            dims.push(DirectDim::Int { min });
            range + 1 // extra lane for nulls
        } else {
            return None;
        };
        product = product.checked_mul(card)?;
        if product > (1 << 22) {
            return None;
        }
        cols.push(*index);
        cards.push(card);
    }
    if cols.is_empty() {
        return None;
    }
    Some(DirectGroups { cols, dims, cards, dense: vec![-1; product] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_bitmap_memory_cap_preserves_codes_on_sparse_fallback() {
        let mut acc = AggAcc::DistinctCodes { bits: Vec::new(), words: 1024 };
        acc.grow(2);
        // Only cardinality matters here; this accumulator never decodes text.
        let dict = Rc::new(vec![Rc::new(String::new()); 65535]);
        let codes = |values| VV::all_valid(Data::Codes {
            codes: Rc::new(values),
            dict: dict.clone(),
        });
        acc.update_batch(RowsSrc::Gids(&[0, 0, 1, 1]), &codes(vec![63, 63, 65534, 65535]));
        // Crossing the cap must preserve existing groups and allow new ones.
        acc.grow(1025);
        assert!(matches!(acc, AggAcc::DistinctNum(_)));
        acc.update_batch(RowsSrc::Gids(&[0, 0, 1, 1024, 1024]), &codes(vec![63, 64, 65534, 0, 65535]));
        assert_eq!(acc.finish(0), Val::Int(2));
        assert_eq!(acc.finish(1), Val::Int(1));
        assert_eq!(acc.finish(2), Val::Int(0));
        assert_eq!(acc.finish(1024), Val::Int(1));
    }
}

// ---------------- execute ----------------

/// Fold before column collection and planning, so predicates, grouping and
/// aggregate arguments can use the same fast paths as a bare column. Schema
/// flags alone are insufficient: every row group's null count must be zero.
fn fold_nonnull<'a>(expr: &'a Bound, cat: &crate::format::Catalog) -> std::borrow::Cow<'a, Bound> {
    use std::borrow::Cow;
    let mut out = Cow::Borrowed(expr);
    match expr {
        Bound::Call { args, .. } => {
            for (i, arg) in args.iter().enumerate() {
                if let Cow::Owned(folded) = fold_nonnull(arg, cat) {
                    let Bound::Call { args, .. } = out.to_mut() else { unreachable!() };
                    args[i] = folded;
                }
            }
        }
        Bound::Unary { expr, .. } => {
            if let Cow::Owned(folded) = fold_nonnull(expr, cat) {
                let Bound::Unary { expr, .. } = out.to_mut() else { unreachable!() };
                **expr = folded;
            }
        }
        Bound::Binary { lhs, rhs, .. } => {
            let left = fold_nonnull(lhs, cat);
            let right = fold_nonnull(rhs, cat);
            if matches!(left, Cow::Owned(_)) || matches!(right, Cow::Owned(_)) {
                let Bound::Binary { lhs, rhs, .. } = out.to_mut() else { unreachable!() };
                **lhs = left.into_owned();
                **rhs = right.into_owned();
            }
        }
        _ => {}
    }
    if let Bound::Call { func, args, ty } = out.as_ref() {
        if matches!(func.name, "coalesce" | "ifnull") {
            if let Bound::Column { index, ty: col_ty } = &args[0] {
                if ty == col_ty && cat.groups.iter().all(|g| g.cols[*index].null_count == 0) {
                    return Cow::Owned(args[0].clone());
                }
            }
        }
    }
    out
}

fn fold_query<'a>(
    q: &'a super::binder::BoundQuery,
    cat: &crate::format::Catalog,
) -> std::borrow::Cow<'a, super::binder::BoundQuery> {
    use std::borrow::Cow;
    let mut out = Cow::Borrowed(q);
    for (i, s) in q.select.iter().enumerate() {
        if let Cow::Owned(expr) = fold_nonnull(&s.expr, cat) {
            out.to_mut().select[i].expr = expr;
        }
    }
    if let Some(f) = &q.filter {
        if let Cow::Owned(expr) = fold_nonnull(f, cat) {
            out.to_mut().filter = Some(expr);
        }
    }
    for (i, g) in q.group_by.iter().enumerate() {
        if let Cow::Owned(expr) = fold_nonnull(g, cat) {
            out.to_mut().group_by[i] = expr;
        }
    }
    for (i, (e, _)) in q.order_by.iter().enumerate() {
        if let Cow::Owned(expr) = fold_nonnull(e, cat) {
            out.to_mut().order_by[i].0 = expr;
        }
    }
    out
}

pub fn execute<S: ReadAt>(
    table: &mut Table<S>,
    q: &super::binder::BoundQuery,
) -> Result<QueryResult, FormatError> {
    let folded = fold_query(q, table.catalog());
    let q = folded.as_ref();
    let columns: Vec<String> = q.select.iter().map(|s| s.name.clone()).collect();

    let mut needed = Vec::new();
    q.select.iter().for_each(|s| collect_columns(&s.expr, &mut needed));
    if let Some(f) = &q.filter {
        collect_columns(f, &mut needed);
    }
    q.group_by.iter().for_each(|g| collect_columns(g, &mut needed));
    q.order_by.iter().for_each(|(e, _)| collect_columns(e, &mut needed));
    needed.sort_unstable();
    needed.dedup();

    let mut dicts: HashMap<usize, Rc<Vec<VStr>>> = HashMap::new();
    for &c in &needed {
        if table.catalog().schema.columns[c].is_dict() {
            dicts.insert(c, table.dictionary_rc(c)?);
        }
    }

    let constraints = q.filter.as_ref().map(collect_ranges).unwrap_or_default();
    let conjuncts = conjuncts_of(table, q.filter.as_ref());
    // columns the projection/grouping/ordering need — the filter's own columns
    // are loaded only when a conjunct misses the mask cache
    let proj_needed: Vec<usize> = {
        let mut v = Vec::new();
        q.select.iter().for_each(|s| collect_columns(&s.expr, &mut v));
        q.group_by.iter().for_each(|g| collect_columns(g, &mut v));
        q.order_by.iter().for_each(|(e, _)| collect_columns(e, &mut v));
        v.sort_unstable();
        v.dedup();
        v
    };

    let agg_calls: Vec<Bound> = {
        let mut v = Vec::new();
        q.select.iter().for_each(|s| collect_aggs(&s.expr, &mut v));
        q.order_by.iter().for_each(|(e, _)| collect_aggs(e, &mut v));
        v
    };

    let mut direct = if q.is_aggregate { direct_plan(table, &q.group_by) } else { None };
    let mut hash_groups: HashMap<Vec<Val>, usize> = HashMap::new();
    let mut group_keys: Vec<Vec<Val>> = Vec::new();
    let arg_dict_len = |call: &Bound| -> Option<usize> {
        let Bound::Call { args, .. } = call else { return None };
        let Bound::Column { index, .. } = &args[0] else { return None };
        dicts.get(index).map(|dict| dict.len())
    };
    let mut accs: Vec<AggAcc> =
        agg_calls.iter().map(|c| AggAcc::new(c, arg_dict_len(c))).collect();
    let mut n_groups = 0usize;

    let sel_tys: Vec<Ty> = q.select.iter().map(|s| s.expr.ty()).collect();
    let ord_tys: Vec<Ty> = q.order_by.iter().map(|(e, _)| e.ty()).collect();
    // every non-aggregate path now early-returns columnar; this remains only
    // as the aggregate path's row buffer seed
    let out_rows: Vec<(Vec<Val>, Vec<Val>)> = Vec::new();

    // Bounded top-k: ORDER BY + LIMIT with a small window — keep only ~2*cap
    // candidates, cheap first-key reject for the vast majority of rows, and
    // project ONLY the winners at the end.
    // Limit-only bound: no ORDER BY, no aggregation — the scan can stop the
    // moment offset+limit rows are collected, and each group only needs its
    // projection lanes materialized up to the last row it can contribute.
    let scan_cap = if !q.is_aggregate && q.order_by.is_empty() {
        q.limit.map(|l| l as usize + q.offset.unwrap_or(0) as usize)
    } else {
        None
    };
    // top-k scans need only the ORDER BY columns; select lanes load later,
    // for winning groups only (true late materialization)
    let ord_needed: Vec<usize> = {
        let mut v = Vec::new();
        q.order_by.iter().for_each(|(e, _)| collect_columns(e, &mut v));
        v.sort_unstable();
        v.dedup();
        v
    };

    let topk_cap = if !q.is_aggregate && !q.order_by.is_empty() {
        q.limit
            .map(|l| (l + q.offset.unwrap_or(0)) as usize)
            .filter(|c| *c <= 100_000)
    } else {
        None
    };
    struct Cand {
        keys: Vec<Val>,
        g: u32,
        row: u32,
    }
    let mut cands: Vec<Cand> = Vec::new();
    let mut bound_key: Option<Vec<Val>> = None; // full key of current cutoff
    let cmp_keys = |a: &Vec<Val>, b: &Vec<Val>, order_by: &[(Bound, SortDir)]| {
        for (i, (_, dir)) in order_by.iter().enumerate() {
            let ord = a[i].cmp_sql(&b[i]);
            let ord = if *dir == SortDir::Desc { ord.reverse() } else { ord };
            if ord != core::cmp::Ordering::Equal {
                return ord;
            }
        }
        core::cmp::Ordering::Equal
    };

    // full-table ORDER BY (no usable top-k bound): defer everything — sort
    // packed keys + row refs, project only afterwards
    let full_sort = !q.is_aggregate && !q.order_by.is_empty() && topk_cap.is_none();
    let mut sort_groups: Vec<(Vec<VV>, Vec<u32>)> = Vec::new();
    let mut sort_srcs: Vec<Option<SelSrc>> = (0..q.select.len()).map(|_| None).collect();
    // plain projection (no ORDER BY): columnar refs + sources
    let plain = !q.is_aggregate && q.order_by.is_empty();
    let mut plain_srcs: Vec<Option<SelSrc>> = (0..q.select.len()).map(|_| None).collect();
    let mut plain_refs: Vec<(u32, u32)> = Vec::new();
    let mut plain_gslot = 0u32;

    let mut scanned_groups = 0usize;
    for g in 0..table.group_count() {
        if group_prunable(table, g, &constraints) {
            continue;
        }
        scanned_groups += 1;
        let rows = table.group_rows(g);
        let load = |table: &mut Table<S>,
                    cols: &mut HashMap<usize, (GroupCol, Option<Rc<Vec<u8>>>)>,
                    which: &[usize],
                    cap: usize|
         -> Result<(), FormatError> {
            for &c in which {
                if cols.contains_key(&c) {
                    continue;
                }
                let (cty, is_dict) = {
                    let def = &table.catalog().schema.columns[c];
                    (def.ty, def.is_dict())
                };
                let validity = table.validity(g, c)?.map(Rc::new);
                let col = if is_dict {
                    GroupCol::Dict { codes: Rc::new(table.codes(g, c, cap)?), dict: dicts[&c].clone() }
                } else {
                    match cty {
                        ColumnType::Float64 => GroupCol::F64(Rc::new(table.f64s(g, c, cap)?)),
                        ColumnType::Utf8 => {
                            let (offs, bytes) = table.texts_raw(g, c, cap)?;
                            GroupCol::Text {
                                strs: std::cell::OnceCell::new(),
                                offsets: Rc::new(offs),
                                bytes: Rc::new(bytes),
                            }
                        }
                        _ => GroupCol::I64(Rc::new(table.i64s(g, c, cap)?)),
                    }
                };
                cols.insert(c, (col, validity));
            }
            Ok(())
        };

        // phase 1: the WHERE mask, per conjunct — from the mask cache when
        // present, else evaluated over its columns (full group) and cached
        let mut cols = HashMap::new();
        let keep: Option<Vec<u8>> = if conjuncts.is_empty() {
            None
        } else {
            let mut keep = vec![1u8; rows];
            let mut pending: Vec<&Conjunct> = Vec::new();
            for c in &conjuncts {
                match table.masks().get(&c.key, g) {
                    Some(bits) => {
                        for (i, k) in keep.iter_mut().enumerate() {
                            *k &= bits[i / 8] >> (i % 8) & 1;
                        }
                    }
                    None => pending.push(c),
                }
            }
            if !pending.is_empty() {
                let n_groups = table.group_count();
                let store = |table: &mut Table<S>, c: &Conjunct, m: &[u8], keep: &mut [u8]| {
                    table.masks().put(&c.key, g, n_groups, Rc::new(pack_bits(m)), c.like.clone());
                    for (k, &b) in keep.iter_mut().zip(m) {
                        *k &= b;
                    }
                };
                // contains-LIKE extending a cached needle: verify only the rows
                // the superset admits, straight off the image — no blob scan,
                // no column copy
                let mut full: Vec<&Conjunct> = Vec::new();
                for c in pending {
                    let narrowed = c.like.as_ref().and_then(|lk| {
                        let sup = table.masks().like_superset(lk.col, &lk.needle, g)?;
                        let needle = lk.needle.as_bytes();
                        table
                            .with_text_segments(g, lk.col, |offs, blob, valid| {
                                let off = |i: usize| {
                                    u32::from_le_bytes(offs[i * 4..i * 4 + 4].try_into().unwrap()) as usize
                                };
                                let mut m = vec![0u8; rows];
                                for (i, mi) in m.iter_mut().enumerate() {
                                    if sup[i / 8] >> (i % 8) & 1 == 0
                                        || valid.is_some_and(|v| v[i / 8] >> (i % 8) & 1 == 0)
                                    {
                                        continue;
                                    }
                                    let s = &blob[off(i)..off(i + 1)];
                                    *mi = crate::text::contains_ci(s, needle) as u8;
                                }
                                m
                            })
                            .ok()
                            .flatten()
                    });
                    if let Some(m) = narrowed {
                        store(table, c, &m, &mut keep);
                        continue;
                    }
                    // integer comparison straight off the raw narrow segment —
                    // widening the lane into Vec<i64> costs more than the compare
                    let int_fast = int_cmp_lit(&c.expr).and_then(|(col, lo, hi, inv)| {
                        table
                            .with_fixed_segments(g, col, |vals, w, valid| {
                                let mut m = vec![0u8; rows];
                                // Bounds are clamped to the stored width so the
                                // compare runs at that width — verified in the
                                // wasm disassembly: i64 bounds forced an
                                // extend-to-i64x2 chain (2 lanes/op); clamped
                                // i16 bounds compare as i16x8 (8 lanes/op).
                                macro_rules! sweep {
                                    ($t:ty, $w:expr, |$i:ident, $ch:ident| $x:expr) => {{
                                        if lo > <$t>::MAX as i64 || hi < <$t>::MIN as i64 {
                                            m.fill(inv as u8); // empty range
                                        } else {
                                            let lo = lo.max(<$t>::MIN as i64) as $t;
                                            let hi = hi.min(<$t>::MAX as i64) as $t;
                                            for ($i, $ch) in
                                                vals[..rows * $w].chunks_exact($w).enumerate()
                                            {
                                                let x: $t = $x;
                                                m[$i] =
                                                    ((x >= lo && x <= hi) != inv) as u8;
                                            }
                                        }
                                    }};
                                }
                                match w {
                                    1 => sweep!(i8, 1, |i, ch| ch[0] as i8),
                                    2 => {
                                        sweep!(i16, 2, |i, ch| i16::from_le_bytes([
                                            ch[0], ch[1]
                                        ]))
                                    }
                                    4 => {
                                        sweep!(i32, 4, |i, ch| i32::from_le_bytes([
                                            ch[0], ch[1], ch[2], ch[3]
                                        ]))
                                    }
                                    _ => {
                                        sweep!(i64, 8, |i, ch| i64::from_le_bytes([
                                            ch[0], ch[1], ch[2], ch[3], ch[4], ch[5],
                                            ch[6], ch[7]
                                        ]))
                                    }
                                }
                                if let Some(vb) = valid {
                                    for (i, mi) in m.iter_mut().enumerate() {
                                        *mi &= vb[i / 8] >> (i % 8) & 1;
                                    }
                                }
                                m
                            })
                            .ok()
                            .flatten()
                    });
                    match int_fast {
                        Some(m) => store(table, c, &m, &mut keep),
                        None => full.push(c),
                    }
                }
                if !full.is_empty() {
                    let mut full_cols: Vec<usize> =
                        full.iter().flat_map(|c| c.cols.iter().copied()).collect();
                    full_cols.sort_unstable();
                    full_cols.dedup();
                    load(table, &mut cols, &full_cols, usize::MAX)?;
                    for c in full {
                        let fctx = GroupCtx { cols: core::mem::take(&mut cols), rows };
                        let v = eval_vec(&c.expr, &fctx);
                        cols = fctx.cols;
                        let m: Vec<u8> =
                            (0..rows).map(|i| (v.bool3_at(i) == Some(true)) as u8).collect();
                        store(table, c, &m, &mut keep);
                    }
                }
            }
            Some(keep)
        };
        // fully filtered out: nothing else to load or evaluate for this group
        if keep.as_ref().is_some_and(|k| k.iter().all(|&b| b == 0)) {
            continue;
        }

        // phase 2: how deep must projection lanes go? (limit-only: just far
        // enough to yield the rows still missing)
        let take_rows = match scan_cap {
            None => rows,
            Some(c) => {
                let rem = c.saturating_sub(out_rows.len());
                match &keep {
                    None => rem.min(rows),
                    Some(k) => {
                        let mut cnt = 0usize;
                        let mut cut = rows;
                        for (i, &b) in k.iter().enumerate() {
                            if b != 0 {
                                cnt += 1;
                                if cnt == rem {
                                    cut = i + 1;
                                    break;
                                }
                            }
                        }
                        cut
                    }
                }
            }
        };
        if topk_cap.is_some() {
            load(table, &mut cols, &ord_needed, usize::MAX)?;
        } else {
            load(table, &mut cols, &proj_needed, take_rows)?;
        }
        let eval_rows = if scan_cap.is_some() { take_rows } else { rows };
        let ctx = GroupCtx { cols, rows: eval_rows };
        let kept = |i: usize| keep.as_ref().map_or(true, |k| k[i] != 0);

        if q.is_aggregate {
            let gids: Option<Vec<u32>> = if q.group_by.is_empty() {
                // ungrouped: one group, no gids vector — aggregate off the mask
                if n_groups == 0 {
                    group_keys.push(Vec::new());
                    n_groups = 1;
                    for a in &mut accs {
                        a.grow(1);
                    }
                }
                None
            } else if let Some(d) = &mut direct {
                let code_cols: Vec<(VV, usize)> = d
                    .cols
                    .iter()
                    .zip(&d.cards)
                    .map(|(&c, &card)| (ctx.column(c), card))
                    .collect();
                // hoist raw lanes out of the row loop
                enum FastLane<'a> {
                    Codes(&'a [u16]),
                    Ints { vals: &'a [i64], min: i64 },
                }
                struct FastDim<'a> {
                    lane: FastLane<'a>,
                    valid: Option<&'a [u8]>,
                    card: usize,
                }
                let fast: Vec<FastDim> = code_cols
                    .iter()
                    .zip(&d.dims)
                    .map(|((vv, card), dim)| {
                        let lane = match (&vv.data, dim) {
                            (Data::Codes { codes, .. }, DirectDim::Dict) => {
                                FastLane::Codes(codes)
                            }
                            (Data::I64(vals), DirectDim::Int { min }) => {
                                FastLane::Ints { vals, min: *min }
                            }
                            _ => unreachable!("direct plan only over dict/int columns"),
                        };
                        FastDim {
                            lane,
                            valid: vv.valid.as_deref().map(|v| v.as_slice()),
                            card: *card,
                        }
                    })
                    .collect();
                // count(*)-only queries fuse the count into this loop —
                // no gids vector, no second aggregation pass
                let count_only = accs.iter().all(|a| matches!(a, AggAcc::Count(_)))
                    && agg_calls.iter().all(|c| {
                        let Bound::Call { args, .. } = c else { return false };
                        matches!(args[0], Bound::Number(..) | Bound::Str(_))
                    });
                let mut gids =
                    if count_only { Vec::new() } else { vec![u32::MAX; rows] };
                // fused counting goes through a plain local buffer (merged
                // below) — touching the accumulator enum per row is slower
                // than the gids pass it replaces
                let mut local_counts: Vec<i64> =
                    if count_only { vec![0; n_groups] } else { Vec::new() };
                for i in 0..rows {
                    if !kept(i) {
                        continue;
                    }
                    let mut composite = 0usize;
                    for fd in &fast {
                        let ok = fd.valid.map_or(true, |v| v[i / 8] >> (i % 8) & 1 != 0);
                        let code = if !ok {
                            fd.card - 1
                        } else {
                            match &fd.lane {
                                FastLane::Codes(codes) => codes[i] as usize,
                                FastLane::Ints { vals, min } => (vals[i] - min) as usize,
                            }
                        };
                        composite = composite * fd.card + code;
                    }
                    let dense = &mut d.dense[composite];
                    if *dense < 0 {
                        *dense = n_groups as i32;
                        let key: Vec<Val> =
                            code_cols.iter().map(|(vv, _)| lane_val(vv, i)).collect();
                        group_keys.push(key);
                        n_groups += 1;
                        for a in &mut accs {
                            a.grow(n_groups);
                        }
                        if count_only {
                            local_counts.push(0);
                        }
                    }
                    if count_only {
                        local_counts[*dense as usize] += 1;
                    } else {
                        gids[i] = *dense as u32;
                    }
                }
                if count_only {
                    for a in &mut accs {
                        let AggAcc::Count(c) = a else { unreachable!("count_only") };
                        for (g, &n) in local_counts.iter().enumerate() {
                            c[g] += n;
                        }
                    }
                    continue; // this group's aggregates are done
                }
                Some(gids)
            } else {
                let key_vvs: Vec<VV> = q.group_by.iter().map(|e| eval_vec(e, &ctx)).collect();
                let mut gids = vec![u32::MAX; rows];
                for i in 0..rows {
                    if !kept(i) {
                        continue;
                    }
                    let key: Vec<Val> = key_vvs.iter().map(|v| lane_val(v, i)).collect();
                    let next = n_groups;
                    let gid = *hash_groups.entry(key).or_insert_with_key(|k| {
                        group_keys.push(k.clone());
                        next
                    });
                    if gid == next && gid == n_groups {
                        n_groups += 1;
                        for a in &mut accs {
                            a.grow(n_groups);
                        }
                    }
                    gids[i] = gid as u32;
                }
                Some(gids)
            };
            let src = match &gids {
                Some(g) => RowsSrc::Gids(g),
                None => RowsSrc::Mask { keep: keep.as_deref(), n: rows },
            };
            for (acc, call) in accs.iter_mut().zip(&agg_calls) {
                let Bound::Call { args, .. } = call else { unreachable!() };
                let arg = eval_vec(&args[0], &ctx);
                acc.update_batch(src, &arg);
            }
        } else if let Some(cap) = topk_cap {
            let ord_vvs: Vec<VV> = q.order_by.iter().map(|(e, _)| eval_vec(e, &ctx)).collect();
            for i in 0..rows {
                if !kept(i) {
                    continue;
                }
                // cheap reject on the first order key against the cutoff
                if let Some(bk) = &bound_key {
                    let k0 = ord_vvs[0].val_at(i, ord_tys[0]);
                    let ord = k0.cmp_sql(&bk[0]);
                    let ord =
                        if q.order_by[0].1 == SortDir::Desc { ord.reverse() } else { ord };
                    if ord == core::cmp::Ordering::Greater {
                        continue;
                    }
                }
                let keys: Vec<Val> =
                    ord_vvs.iter().zip(&ord_tys).map(|(v, t)| v.val_at(i, *t)).collect();
                cands.push(Cand { keys, g: g as u32, row: i as u32 });
                if cands.len() >= cap * 2 + 16 {
                    cands.sort_by(|a, b| cmp_keys(&a.keys, &b.keys, &q.order_by));
                    cands.truncate(cap);
                    bound_key = cands.last().map(|c| c.keys.clone());
                }
            }
        } else if full_sort {
            sel_srcs_for_group(q, &ctx, &mut sort_srcs);
            let ord_vvs: Vec<VV> = q.order_by.iter().map(|(e, _)| eval_vec(e, &ctx)).collect();
            let kept_rows: Vec<u32> =
                (0..ctx.rows).filter(|&i| kept(i)).map(|i| i as u32).collect();
            sort_groups.push((ord_vvs, kept_rows));
        } else {
            debug_assert!(plain);
            sel_srcs_for_group(q, &ctx, &mut plain_srcs);
            plain_refs
                .extend((0..ctx.rows).filter(|&i| kept(i)).map(|i| (plain_gslot, i as u32)));
            plain_gslot += 1;
            if let Some(c) = scan_cap {
                if plain_refs.len() >= c {
                    plain_refs.truncate(c);
                    break;
                }
            }
        }
    }

    if plain {
        let offset = q.offset.unwrap_or(0) as usize;
        let limit = q.limit.map(|l| l as usize).unwrap_or(usize::MAX);
        let refs: Vec<(u32, u32)> =
            plain_refs.into_iter().skip(offset).take(limit).collect();
        let cols: Vec<OutCol> = plain_srcs
            .iter()
            .zip(&sel_tys)
            .map(|(src, ty)| match src {
                Some(src) => gather_outcol(src, &refs, *ty),
                None => gather_outcol(&SelSrc::Vv(Vec::new()), &[], *ty), // zero groups scanned
            })
            .collect();
        return Ok(QueryResult {
            columns,
            col_types: q.select.iter().map(|s| s.expr.ty()).collect(),
            rows: Vec::new(),
            out_rows: refs.len(),
            cols: Some(cols),
            scanned_groups,
            total_groups: table.group_count(),
        });
    }

    if full_sort {
        // flatten refs: (group slot, row)
        let total: usize = sort_groups.iter().map(|(_, k)| k.len()).sum();
        let mut refs: Vec<(u32, u32)> = Vec::with_capacity(total);
        for (gslot, (_, kept_rows)) in sort_groups.iter().enumerate() {
            for &r in kept_rows {
                refs.push((gslot as u32, r));
            }
        }
        let numeric_keys = q
            .order_by
            .iter()
            .all(|(e, _)| matches!(e.ty(), Ty::Int | Ty::Float | Ty::Date | Ty::Timestamp));
        let nk = q.order_by.len();

        // order-preserving u64 encoding; validity first so NULLs sort first
        // ascending (and last after a DESC flip), matching cmp_sql
        let encode = |vv: &VV, i: usize, ty: Ty, desc: bool| -> (u8, u64) {
            let (v, k) = if !vv.is_valid(i) {
                (0u8, 0u64)
            } else if ty == Ty::Float {
                let b = vv.f64_at(i).to_bits();
                (1, if b >> 63 == 1 { !b } else { b | (1u64 << 63) })
            } else {
                (1, (vv.i64_at(i) as u64) ^ (1u64 << 63))
            };
            if desc { (1 - v, !k) } else { (v, k) }
        };

        let order_refs: Vec<(u32, u32)> = if numeric_keys && nk == 1 {
            let (_, dir) = &q.order_by[0];
            let desc = *dir == SortDir::Desc;
            let ty = ord_tys[0];
            let mut keyed: Vec<(u8, u64, u32, u32)> = refs
                .iter()
                .map(|&(g, r)| {
                    let (v, k) = encode(&sort_groups[g as usize].0[0], r as usize, ty, desc);
                    (v, k, g, r)
                })
                .collect();
            keyed.sort_unstable_by_key(|t| (t.0, t.1));
            keyed.into_iter().map(|t| (t.2, t.3)).collect()
        } else if numeric_keys {
            let mut flat: Vec<(u8, u64)> = Vec::with_capacity(refs.len() * nk);
            for &(g, r) in &refs {
                for (ki, (_, dir)) in q.order_by.iter().enumerate() {
                    flat.push(encode(
                        &sort_groups[g as usize].0[ki],
                        r as usize,
                        ord_tys[ki],
                        *dir == SortDir::Desc,
                    ));
                }
            }
            let mut perm: Vec<u32> = (0..refs.len() as u32).collect();
            perm.sort_unstable_by(|&a, &b| {
                flat[a as usize * nk..a as usize * nk + nk]
                    .cmp(&flat[b as usize * nk..b as usize * nk + nk])
            });
            perm.into_iter().map(|p| refs[p as usize]).collect()
        } else {
            // text keys: materialize the (small) key tuples, sort refs by them
            let keys: Vec<Vec<Val>> = refs
                .iter()
                .map(|&(g, r)| {
                    sort_groups[g as usize]
                        .0
                        .iter()
                        .zip(&ord_tys)
                        .map(|(v, t)| v.val_at(r as usize, *t))
                        .collect()
                })
                .collect();
            let mut perm: Vec<u32> = (0..refs.len() as u32).collect();
            perm.sort_by(|&a, &b| {
                cmp_keys(&keys[a as usize], &keys[b as usize], &q.order_by)
            });
            perm.into_iter().map(|p| refs[p as usize]).collect()
        };

        let offset = q.offset.unwrap_or(0) as usize;
        let limit = q.limit.map(|l| l as usize).unwrap_or(usize::MAX);
        let final_refs: Vec<(u32, u32)> =
            order_refs.into_iter().skip(offset).take(limit).collect();
        let cols: Vec<OutCol> = sort_srcs
            .iter()
            .zip(&sel_tys)
            .map(|(src, ty)| match src {
                Some(src) => gather_outcol(src, &final_refs, *ty),
                None => gather_outcol(&SelSrc::Vv(Vec::new()), &[], *ty),
            })
            .collect();
        return Ok(QueryResult {
            columns,
            col_types: q.select.iter().map(|s| s.expr.ty()).collect(),
            rows: Vec::new(),
            out_rows: final_refs.len(),
            cols: Some(cols),
            scanned_groups,
            total_groups: table.group_count(),
        });
    }

    // finish bounded top-k: sort survivors, window, then load ONLY the
    // winning groups' select lanes and project the winner rows
    if topk_cap.is_some() {
        cands.sort_by(|a, b| cmp_keys(&a.keys, &b.keys, &q.order_by));
        let offset = q.offset.unwrap_or(0) as usize;
        let limit = q.limit.unwrap_or(0) as usize;
        let winners: Vec<Cand> = cands.into_iter().skip(offset).take(limit).collect();

        // Plain-text columns selected directly are gathered per winner row
        // straight off the borrowed segments — loading the lane would copy
        // the whole blob up to the deepest winner for a handful of rows.
        let is_direct_text = |b: &Bound| -> Option<usize> {
            let Bound::Column { index, .. } = b else { return None };
            let def = &table.catalog().schema.columns[*index];
            (def.ty == ColumnType::Utf8 && !def.is_dict()).then_some(*index)
        };
        let mut direct_text: Vec<Option<usize>> =
            q.select.iter().map(|s| is_direct_text(&s.expr)).collect();
        // gathered[sel_idx][winner_idx]
        let mut gathered: HashMap<usize, Vec<Val>> = HashMap::new();
        for (si, ci) in direct_text.clone().into_iter().enumerate() {
            let Some(ci) = ci else { continue };
            let mut vals = vec![Val::Null; winners.len()];
            let mut ok = true;
            for g in winners.iter().map(|w| w.g).collect::<std::collections::BTreeSet<_>>() {
                let got = table.with_text_segments(g as usize, ci, |offs, blob, valid| {
                    for (wi, w) in winners.iter().enumerate() {
                        if w.g != g {
                            continue;
                        }
                        let i = w.row as usize;
                        if valid.is_some_and(|v| v[i / 8] >> (i % 8) & 1 == 0) {
                            continue; // stays Null
                        }
                        let at = |n: usize| {
                            u32::from_le_bytes(offs[n * 4..n * 4 + 4].try_into().unwrap())
                                as usize
                        };
                        let s = core::str::from_utf8(&blob[at(i)..at(i + 1)])
                            .expect("image text is utf8");
                        vals[wi] = Val::Text(Rc::new(s.to_string()));
                    }
                })?;
                if got.is_none() {
                    ok = false; // segments not resident: fall back to the lane path
                    break;
                }
            }
            if ok {
                gathered.insert(si, vals);
            } else {
                direct_text[si] = None;
            }
        }
        // lanes still needed: any select expr that isn't a gathered direct text
        let mut lane_cols = Vec::new();
        for (si, s) in q.select.iter().enumerate() {
            if direct_text[si].is_none() || !gathered.contains_key(&si) {
                collect_columns(&s.expr, &mut lane_cols);
            }
        }
        lane_cols.sort_unstable();
        lane_cols.dedup();

        let mut sel_cache: HashMap<u32, Vec<Option<VV>>> = HashMap::new();
        let mut rows: Vec<Vec<Val>> = Vec::with_capacity(winners.len());
        for (wi, c) in winners.iter().enumerate() {
            if !sel_cache.contains_key(&c.g) {
                let g = c.g as usize;
                let cap = winners
                    .iter()
                    .filter(|w| w.g == c.g)
                    .map(|w| w.row as usize + 1)
                    .max()
                    .unwrap();
                let mut cols = HashMap::new();
                for &ci in &lane_cols {
                    let (cty, is_dict) = {
                        let def = &table.catalog().schema.columns[ci];
                        (def.ty, def.is_dict())
                    };
                    let validity = table.validity(g, ci)?.map(Rc::new);
                    let col = if is_dict {
                        GroupCol::Dict {
                            codes: Rc::new(table.codes(g, ci, cap)?),
                            dict: dicts[&ci].clone(),
                        }
                    } else {
                        match cty {
                            ColumnType::Float64 => GroupCol::F64(Rc::new(table.f64s(g, ci, cap)?)),
                            ColumnType::Utf8 => {
                                let (offs, bytes) = table.texts_raw(g, ci, cap)?;
                                GroupCol::Text {
                                    strs: std::cell::OnceCell::new(),
                                    offsets: Rc::new(offs),
                                    bytes: Rc::new(bytes),
                                }
                            }
                            _ => GroupCol::I64(Rc::new(table.i64s(g, ci, cap)?)),
                        }
                    };
                    cols.insert(ci, (col, validity));
                }
                let ctx = GroupCtx { cols, rows: cap };
                let sel_vvs: Vec<Option<VV>> = q
                    .select
                    .iter()
                    .enumerate()
                    .map(|(si, s)| {
                        (!gathered.contains_key(&si)).then(|| eval_vec(&s.expr, &ctx))
                    })
                    .collect();
                sel_cache.insert(c.g, sel_vvs);
            }
            let sel = &sel_cache[&c.g];
            rows.push(
                sel.iter()
                    .zip(&sel_tys)
                    .enumerate()
                    .map(|(si, (v, t))| match v {
                        Some(v) => v.val_at(c.row as usize, *t),
                        None => gathered[&si][wi].clone(),
                    })
                    .collect(),
            );
        }
        let out_rows = rows.len();
        return Ok(QueryResult {
            columns,
            col_types: q.select.iter().map(|s| s.expr.ty()).collect(),
            rows,
            out_rows,
            cols: None,
            scanned_groups,
            total_groups: table.group_count(),
        });
    }

    let mut rows: Vec<(Vec<Val>, Vec<Val>)> = if q.is_aggregate {
        if q.group_by.is_empty() && n_groups == 0 {
            group_keys.push(Vec::new());
            n_groups = 1;
            for a in &mut accs {
                a.grow(1);
            }
        }
        (0..n_groups)
            .map(|gid| {
                let o = Overrides {
                    group_by: &q.group_by,
                    key: &group_keys[gid],
                    aggs: &agg_calls,
                    accs: &accs,
                    gid,
                };
                let projected: Vec<Val> =
                    q.select.iter().map(|s| eval_grouped(&s.expr, &o)).collect();
                let order: Vec<Val> =
                    q.order_by.iter().map(|(e, _)| eval_grouped(e, &o)).collect();
                (projected, order)
            })
            .collect()
    } else {
        out_rows
    };

    if !q.order_by.is_empty() {
        rows.sort_by(|a, b| {
            for (i, (_, dir)) in q.order_by.iter().enumerate() {
                let ord = a.1[i].cmp_sql(&b.1[i]);
                let ord = if *dir == SortDir::Desc { ord.reverse() } else { ord };
                if ord != core::cmp::Ordering::Equal {
                    return ord;
                }
            }
            core::cmp::Ordering::Equal
        });
    }

    let offset = q.offset.unwrap_or(0) as usize;
    let limit = q.limit.map(|l| l as usize).unwrap_or(usize::MAX);
    let rows: Vec<Vec<Val>> = rows.into_iter().skip(offset).take(limit).map(|(p, _)| p).collect();

    let out_rows = rows.len();
    Ok(QueryResult {
        columns,
        col_types: q.select.iter().map(|s| s.expr.ty()).collect(),
        rows,
        out_rows,
        cols: None,
        scanned_groups,
        total_groups: table.group_count(),
    })
}
