//! Vectorized expression evaluation: arithmetic, comparisons, LIKE, function calls.

use super::*;

// ---------------- vector evaluation ----------------

pub(super) fn eval_vec(b: &Bound, ctx: &GroupCtx) -> VV {
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

pub(super) fn eval_binary_vec(op: BinOp, a: VV, b: VV, ty: Ty, rows: usize) -> VV {
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

pub(super) fn to_bitmap(rows: usize, valid: &Option<Rc<Vec<u8>>>) -> Vec<u8> {
    match valid {
        Some(v) => v.as_ref().clone(),
        None => vec![0xffu8; (rows + 7) / 8],
    }
}

pub(super) fn cmp_ord(op: BinOp, o: core::cmp::Ordering) -> bool {
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

pub(super) fn cmp_vec(op: BinOp, a: VV, b: VV, rows: usize) -> VV {
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
pub(super) fn like_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let sc: Vec<char> = s.chars().collect();
    like_rec(&p, &sc)
}

pub(super) fn like_rec(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('%') => (0..=s.len()).any(|k| like_rec(&p[1..], &s[k..])),
        Some('_') => !s.is_empty() && like_rec(&p[1..], &s[1..]),
        Some(c) => !s.is_empty() && s[0].eq_ignore_ascii_case(c) && like_rec(&p[1..], &s[1..]),
    }
}

/// The literal shapes of a LIKE pattern (needle stored lowercase for Contains).
pub(super) enum LikeShape {
    Contains(String),
    Prefix(String),
    Suffix(String),
    Exact(String),
    General,
}

pub(super) fn classify_like(p: &str) -> LikeShape {
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
pub(super) fn substr_str(s: &str, start: usize, len: Option<usize>) -> String {
    let (b0, b1) = substr_bounds(s, start, len);
    s[b0..b1].to_string()
}

/// Byte bounds of the char-based substring (empty range when out of bounds).
pub(super) fn substr_bounds(s: &str, start: usize, len: Option<usize>) -> (usize, usize) {
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
pub(super) fn like_shape_match(shape: &LikeShape, s: &str, full_pattern: &str) -> bool {
    let b = s.as_bytes();
    match shape {
        LikeShape::Contains(n) => crate::text::contains_ci(b, n.as_bytes()),
        LikeShape::Prefix(n) => crate::text::prefix_ci(b, n.as_bytes()),
        LikeShape::Suffix(n) => crate::text::suffix_ci(b, n.as_bytes()),
        LikeShape::Exact(n) => crate::text::eq_ci(b, n.as_bytes()),
        LikeShape::General => like_match(full_pattern, s),
    }
}

pub(super) fn eval_call_vec(name: &str, args: &[Bound], ty: Ty, ctx: &GroupCtx) -> VV {
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
            lanes_to_vv(rows, ty, &|i| {
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
            lanes_to_vv(rows, ty, &|i| {
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
            lanes_to_vv(rows, ty, &|i| {
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
                    lanes_to_vv(rows, ty, &|i| {
                        scalar_fn(name, items.iter().map(|v| lane_val(v, i)).collect())
                    })
                }
            }
        }
        _ if TEMPORAL_FNS.contains(&name) => {
            let aty = args[temporal_arg_index(name)].ty();
            let items: Vec<VV> = args.iter().map(|a| eval_vec(a, ctx)).collect();
            lanes_to_vv(rows, ty, &|i| {
                let vals: Vec<Val> = items.iter().map(|v| lane_val(v, i)).collect();
                temporal_fn(name, aty, &vals)
            })
        }
        // remaining scalars through lanes (cold path)
        _ => {
            let items: Vec<VV> = args.iter().map(|a| eval_vec(a, ctx)).collect();
            lanes_to_vv(rows, ty, &|i| {
                let vals: Vec<Val> = items.iter().map(|v| lane_val(v, i)).collect();
                scalar_fn(name, vals)
            })
        }
    }
}

pub(super) fn lane_val(v: &VV, i: usize) -> Val {
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
/// `dyn`: every `impl Fn` caller was its own copy of the four typed loops.
pub(super) fn lanes_to_vv(rows: usize, ty: Ty, f: &dyn Fn(usize) -> Val) -> VV {
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
