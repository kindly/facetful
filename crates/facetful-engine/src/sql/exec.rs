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
        use core::hash::Hash;
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
    pub rows: Vec<Vec<Val>>,
    pub scanned_groups: usize,
    pub total_groups: usize,
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
            (Data::F64(v), Ty::Int) => Val::Int(v[i] as i64),
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
    Text(Rc<Vec<VStr>>),
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
            GroupCol::Text(v) => Data::Text(v.clone()),
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
                    if *ty == Ty::Int {
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
        (true, false) => LikeShape::Suffix(core.to_string()),
        (false, true) => LikeShape::Prefix(core.to_string()),
        (false, false) => LikeShape::Exact(core.to_string()),
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
            let a = eval_vec(&args[0], ctx);
            let pat = eval_vec(&args[1], ctx);
            if let (Data::Codes { codes, dict }, Data::Const(Val::Text(p))) = (&a.data, &pat.data) {
                // pattern evaluated once per dictionary entry
                let table: Vec<u8> = dict.iter().map(|s| like_match(p, s) as u8).collect();
                let out: Vec<u8> = codes.iter().map(|&c| table[c as usize]).collect();
                return VV { data: Data::Bool(Rc::new(out)), valid: a.valid.clone() };
            }
            // plain-text column vs constant pattern: literal fast paths
            // (%x% / x% / %x / exact — the shapes people actually write) run at
            // substring-search speed; the general matcher only sees real wildcards.
            if let (Data::Text(texts), Data::Const(Val::Text(p))) = (&a.data, &pat.data) {
                let shape = classify_like(p);
                let mut out = vec![0u8; rows];
                match &shape {
                    LikeShape::Contains(n) => {
                        for i in 0..rows {
                            out[i] = texts[i].to_ascii_lowercase().contains(n.as_str()) as u8;
                        }
                    }
                    LikeShape::Prefix(n) => {
                        for i in 0..rows {
                            let t = texts[i].as_bytes();
                            out[i] = (t.len() >= n.len()
                                && t[..n.len()].eq_ignore_ascii_case(n.as_bytes()))
                                as u8;
                        }
                    }
                    LikeShape::Suffix(n) => {
                        for i in 0..rows {
                            let t = texts[i].as_bytes();
                            out[i] = (t.len() >= n.len()
                                && t[t.len() - n.len()..].eq_ignore_ascii_case(n.as_bytes()))
                                as u8;
                        }
                    }
                    LikeShape::Exact(n) => {
                        for i in 0..rows {
                            out[i] =
                                texts[i].as_bytes().eq_ignore_ascii_case(n.as_bytes()) as u8;
                        }
                    }
                    LikeShape::General => {
                        // pre-lower the pattern once (the old code re-built it per row)
                        let pchars: Vec<char> = p.chars().collect();
                        for i in 0..rows {
                            let sc: Vec<char> = texts[i].chars().collect();
                            out[i] = like_rec(&pchars, &sc) as u8;
                        }
                    }
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
        "coalesce" => {
            let items: Vec<VV> = args.iter().map(|a| eval_vec(a, ctx)).collect();
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
        Ty::Int => {
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

/// scalar function on materialized values — cold path + grouped-context eval
fn scalar_fn(name: &str, mut args: Vec<Val>) -> Val {
    match name {
        "isnull" => Val::Bool(args[0].is_null()),
        "coalesce" => args.into_iter().find(|v| !v.is_null()).unwrap_or(Val::Null),
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
    DistinctNum(Vec<std::collections::HashSet<u64>>),
    DistinctStr(Vec<std::collections::HashSet<VStr>>),
    SumI { v: Vec<i64>, any: Vec<bool> },
    SumF { v: Vec<f64>, any: Vec<bool> },
    Avg { sum: Vec<f64>, n: Vec<i64> },
    MinMaxNum { v: Vec<f64>, seen: Vec<bool>, is_min: bool, int: bool },
    MinMaxStr { v: Vec<Option<VStr>>, is_min: bool },
}

impl AggAcc {
    /// `arg_is_dict`: the argument is a dictionary column (distinct on codes).
    fn new(call: &Bound, arg_is_dict: bool) -> AggAcc {
        let Bound::Call { func, args, .. } = call else { unreachable!() };
        let aty = args[0].ty();
        match func.name {
            "count" => AggAcc::Count(Vec::new()),
            "count_distinct" => {
                if arg_is_dict || matches!(aty, Ty::Int | Ty::Float | Ty::Bool) {
                    AggAcc::DistinctNum(Vec::new())
                } else {
                    AggAcc::DistinctStr(Vec::new())
                }
            }
            "sum" => {
                if aty == Ty::Int {
                    AggAcc::SumI { v: Vec::new(), any: Vec::new() }
                } else {
                    AggAcc::SumF { v: Vec::new(), any: Vec::new() }
                }
            }
            "avg" => AggAcc::Avg { sum: Vec::new(), n: Vec::new() },
            "min" | "max" => {
                let is_min = func.name == "min";
                if aty == Ty::Text {
                    AggAcc::MinMaxStr { v: Vec::new(), is_min }
                } else {
                    AggAcc::MinMaxNum { v: Vec::new(), seen: Vec::new(), is_min, int: aty == Ty::Int }
                }
            }
            _ => unreachable!(),
        }
    }
    fn grow(&mut self, n: usize) {
        match self {
            AggAcc::Count(v) => v.resize(n, 0),
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
        }
    }

    /// One pass over the group: specialized loops per (accumulator, arg shape).
    fn update_batch(&mut self, gids: &[u32], arg: &VV) {
        let rows = gids.len();
        macro_rules! for_kept {
            (|$i:ident, $g:ident| $body:expr) => {
                for $i in 0..rows {
                    let $g = gids[$i];
                    if $g == u32::MAX {
                        continue;
                    }
                    let $g = $g as usize;
                    $body
                }
            };
        }
        match (&mut *self, &arg.data) {
            (AggAcc::Count(c), Data::Const(v)) => {
                if !v.is_null() {
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
            (AggAcc::DistinctNum(sets), Data::Codes { codes, .. }) => for_kept!(|i, g| {
                if arg.is_valid(i) {
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
            // generic fallbacks (consts, computed vectors, text min/max)
            (acc, _) => {
                for i in 0..rows {
                    let g = gids[i];
                    if g == u32::MAX || !arg.is_valid(i) {
                        continue;
                    }
                    let g = g as usize;
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
                        AggAcc::DistinctStr(_) => unreachable!(),
                    }
                }
            }
        }
    }

    fn finish(&self, gid: usize) -> Val {
        match self {
            AggAcc::Count(v) => Val::Int(v[gid]),
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
            scalar_fn(func.name, vals)
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

// ---------------- grouping plans ----------------

/// Direct-index grouping: all group-bys are dict columns with a small
/// cardinality product — gid from arithmetic on codes, no hashing.
struct DirectGroups {
    cols: Vec<usize>,
    cards: Vec<usize>,
    dense: Vec<i32>,
}

fn direct_plan<S: ReadAt>(table: &mut Table<S>, group_by: &[Bound]) -> Option<DirectGroups> {
    let mut cols = Vec::new();
    for g in group_by {
        match g {
            Bound::Column { index, .. } if table.catalog().schema.columns[*index].is_dict() => {
                cols.push(*index)
            }
            _ => return None,
        }
    }
    if cols.is_empty() {
        return None;
    }
    let mut cards = Vec::new();
    let mut product: usize = 1;
    for &c in &cols {
        let card = table.dictionary(c).ok()?.len() + 1; // extra lane for nulls
        product = product.checked_mul(card)?;
        if product > (1 << 22) {
            return None;
        }
        cards.push(card);
    }
    Some(DirectGroups { cols, cards, dense: vec![-1; product] })
}

// ---------------- execute ----------------

pub fn execute<S: ReadAt>(
    table: &mut Table<S>,
    q: &super::binder::BoundQuery,
) -> Result<QueryResult, FormatError> {
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
            let d = table.dictionary(c)?;
            dicts.insert(c, Rc::new(d.into_iter().map(Rc::new).collect()));
        }
    }

    let constraints = q.filter.as_ref().map(collect_ranges).unwrap_or_default();

    let agg_calls: Vec<Bound> = {
        let mut v = Vec::new();
        q.select.iter().for_each(|s| collect_aggs(&s.expr, &mut v));
        q.order_by.iter().for_each(|(e, _)| collect_aggs(e, &mut v));
        v
    };

    let mut direct = if q.is_aggregate { direct_plan(table, &q.group_by) } else { None };
    let mut hash_groups: HashMap<Vec<Val>, usize> = HashMap::new();
    let mut group_keys: Vec<Vec<Val>> = Vec::new();
    let arg_is_dict = |call: &Bound| -> bool {
        let Bound::Call { args, .. } = call else { return false };
        matches!(&args[0], Bound::Column { index, .. }
            if table.catalog().schema.columns[*index].is_dict())
    };
    let mut accs: Vec<AggAcc> =
        agg_calls.iter().map(|c| AggAcc::new(c, arg_is_dict(c))).collect();
    let mut n_groups = 0usize;

    let sel_tys: Vec<Ty> = q.select.iter().map(|s| s.expr.ty()).collect();
    let ord_tys: Vec<Ty> = q.order_by.iter().map(|(e, _)| e.ty()).collect();
    let mut out_rows: Vec<(Vec<Val>, Vec<Val>)> = Vec::new();

    // Bounded top-k: ORDER BY + LIMIT with a small window — keep only ~2*cap
    // candidates, cheap first-key reject for the vast majority of rows, and
    // project ONLY the winners at the end.
    let topk_cap = if !q.is_aggregate && !q.order_by.is_empty() {
        q.limit
            .map(|l| (l + q.offset.unwrap_or(0)) as usize)
            .filter(|c| *c <= 100_000)
    } else {
        None
    };
    struct Cand {
        keys: Vec<Val>,
        gslot: u32,
        row: u32,
    }
    let mut cands: Vec<Cand> = Vec::new();
    let mut bound_key: Option<Vec<Val>> = None; // full key of current cutoff
    let mut kept_sel: Vec<Vec<VV>> = Vec::new(); // per scanned group, for late projection
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

    let mut scanned_groups = 0usize;
    for g in 0..table.group_count() {
        if group_prunable(table, g, &constraints) {
            continue;
        }
        scanned_groups += 1;
        let rows = table.group_rows(g);
        let mut cols = HashMap::new();
        for &c in &needed {
            let (cty, is_dict) = {
                let def = &table.catalog().schema.columns[c];
                (def.ty, def.is_dict())
            };
            let validity = table.validity(g, c)?.map(Rc::new);
            let col = if is_dict {
                GroupCol::Dict { codes: Rc::new(table.codes(g, c)?), dict: dicts[&c].clone() }
            } else {
                match cty {
                    ColumnType::Float64 => GroupCol::F64(Rc::new(table.f64s(g, c)?)),
                    ColumnType::Utf8 => GroupCol::Text(Rc::new(
                        table.texts(g, c)?.into_iter().map(Rc::new).collect(),
                    )),
                    _ => GroupCol::I64(Rc::new(table.i64s(g, c)?)),
                }
            };
            cols.insert(c, (col, validity));
        }
        let ctx = GroupCtx { cols, rows };

        let keep: Option<Vec<u8>> = q.filter.as_ref().map(|f| {
            let m = eval_vec(f, &ctx);
            (0..rows).map(|i| (m.bool3_at(i) == Some(true)) as u8).collect()
        });
        let kept = |i: usize| keep.as_ref().map_or(true, |k| k[i] != 0);

        if q.is_aggregate {
            let gids: Vec<u32> = if q.group_by.is_empty() {
                // ungrouped: one group, no hashing at all
                if n_groups == 0 {
                    group_keys.push(Vec::new());
                    n_groups = 1;
                    for a in &mut accs {
                        a.grow(1);
                    }
                }
                match &keep {
                    None => vec![0u32; rows],
                    Some(k) => {
                        k.iter().map(|&b| if b != 0 { 0 } else { u32::MAX }).collect()
                    }
                }
            } else if let Some(d) = &mut direct {
                let code_cols: Vec<(VV, usize)> = d
                    .cols
                    .iter()
                    .zip(&d.cards)
                    .map(|(&c, &card)| (ctx.column(c), card))
                    .collect();
                // hoist raw code slices out of the row loop
                struct FastDim<'a> {
                    codes: &'a [u16],
                    valid: Option<&'a [u8]>,
                    card: usize,
                }
                let fast: Vec<FastDim> = code_cols
                    .iter()
                    .map(|(vv, card)| match &vv.data {
                        Data::Codes { codes, .. } => FastDim {
                            codes,
                            valid: vv.valid.as_deref().map(|v| v.as_slice()),
                            card: *card,
                        },
                        _ => unreachable!("direct plan only over dict columns"),
                    })
                    .collect();
                let mut gids = vec![u32::MAX; rows];
                for i in 0..rows {
                    if !kept(i) {
                        continue;
                    }
                    let mut composite = 0usize;
                    for fd in &fast {
                        let ok = fd.valid.map_or(true, |v| v[i / 8] >> (i % 8) & 1 != 0);
                        let code = if ok { fd.codes[i] as usize } else { fd.card - 1 };
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
                    }
                    gids[i] = *dense as u32;
                }
                gids
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
                gids
            };
            for (acc, call) in accs.iter_mut().zip(&agg_calls) {
                let Bound::Call { args, .. } = call else { unreachable!() };
                let arg = eval_vec(&args[0], &ctx);
                acc.update_batch(&gids, &arg);
            }
        } else if let Some(cap) = topk_cap {
            let sel_vvs: Vec<VV> = q.select.iter().map(|s| eval_vec(&s.expr, &ctx)).collect();
            let ord_vvs: Vec<VV> = q.order_by.iter().map(|(e, _)| eval_vec(e, &ctx)).collect();
            let gslot = kept_sel.len() as u32;
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
                cands.push(Cand { keys, gslot, row: i as u32 });
                if cands.len() >= cap * 2 + 16 {
                    cands.sort_by(|a, b| cmp_keys(&a.keys, &b.keys, &q.order_by));
                    cands.truncate(cap);
                    bound_key = cands.last().map(|c| c.keys.clone());
                }
            }
            kept_sel.push(sel_vvs);
        } else {
            let sel_vvs: Vec<VV> = q.select.iter().map(|s| eval_vec(&s.expr, &ctx)).collect();
            let ord_vvs: Vec<VV> = q.order_by.iter().map(|(e, _)| eval_vec(e, &ctx)).collect();
            for i in 0..rows {
                if !kept(i) {
                    continue;
                }
                let projected: Vec<Val> =
                    sel_vvs.iter().zip(&sel_tys).map(|(v, t)| v.val_at(i, *t)).collect();
                let order: Vec<Val> =
                    ord_vvs.iter().zip(&ord_tys).map(|(v, t)| v.val_at(i, *t)).collect();
                out_rows.push((projected, order));
            }
        }
    }

    // finish bounded top-k: sort survivors, window, project only the winners
    if topk_cap.is_some() {
        cands.sort_by(|a, b| cmp_keys(&a.keys, &b.keys, &q.order_by));
        let offset = q.offset.unwrap_or(0) as usize;
        let limit = q.limit.unwrap_or(0) as usize;
        let rows: Vec<Vec<Val>> = cands
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|c| {
                let sel = &kept_sel[c.gslot as usize];
                sel.iter()
                    .zip(&sel_tys)
                    .map(|(v, t)| v.val_at(c.row as usize, *t))
                    .collect()
            })
            .collect();
        return Ok(QueryResult {
            columns,
            rows,
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

    Ok(QueryResult { columns, rows, scanned_groups, total_groups: table.group_count() })
}
