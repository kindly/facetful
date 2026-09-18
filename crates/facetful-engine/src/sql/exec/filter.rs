//! Min/max pruning, WHERE conjuncts as mask-cache units, bit packing.

use super::*;

// ---------------- pruning ----------------

pub(super) type Range = (usize, Option<f64>, Option<f64>);

pub(super) fn collect_ranges(f: &Bound) -> Vec<Range> {
    let mut out = Vec::new();
    walk_and(f, &mut out);
    out
}

pub(super) fn walk_and(b: &Bound, out: &mut Vec<Range>) {
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

pub(super) fn group_prunable<S: ReadAt>(table: &Table<S>, g: usize, constraints: &[Range]) -> bool {
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
pub(super) struct Conjunct {
    pub(super) expr: Bound,
    /// Canonical key: the bound tree's Debug form — names are resolved to
    /// column indices, literals are typed, function identity is by def. Two
    /// spellings of the same predicate bind to the same tree.
    pub(super) key: String,
    pub(super) cols: Vec<usize>,
    pub(super) like: Option<LikeKey>,
}

/// `col <cmp> integral-literal` (either side) over a stored integer column,
/// as an inclusive range test plus an invert flag (Ne). The filter fast path
/// compares straight off the raw narrow segment with this.
pub(super) fn int_cmp_lit(b: &Bound) -> Option<(usize, i64, i64, bool)> {
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

pub(super) fn split_conjuncts(b: &Bound, out: &mut Vec<Bound>) {
    match b {
        Bound::Binary { op: BinOp::And, lhs, rhs, .. } => {
            split_conjuncts(lhs, out);
            split_conjuncts(rhs, out);
        }
        _ => out.push(b.clone()),
    }
}

pub(super) fn conjuncts_of<S: ReadAt>(table: &Table<S>, filter: Option<&Bound>) -> Vec<Conjunct> {
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

pub(super) fn pack_bits(bytes: &[u8]) -> Vec<u8> {
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
