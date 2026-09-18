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

/// The WHERE mask for row group `g`, per conjunct: from the mask cache when
/// present, else evaluated and cached. `None` when there is no WHERE. Columns
/// a conjunct had to load stay in `cols` for the strategy to reuse.
pub(super) fn where_mask<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    conjuncts: &[Conjunct],
    dicts: &Dicts,
    cols: &mut Cols,
) -> Result<Option<Vec<u8>>, FormatError> {
    if conjuncts.is_empty() {
        return Ok(None);
    }
    let mut keep = vec![1u8; rows];
    let mut pending: Vec<&Conjunct> = Vec::new();
    for c in conjuncts {
        match table.masks().get(&c.key, g) {
            Some(bits) => {
                for (i, k) in keep.iter_mut().enumerate() {
                    *k &= bits[i / 8] >> (i % 8) & 1;
                }
            }
            None => pending.push(c),
        }
    }
    if pending.is_empty() {
        return Ok(Some(keep));
    }
    let n_groups = table.group_count();
    let store = |table: &mut Table<S>, c: &Conjunct, m: &[u8], keep: &mut [u8]| {
        table.masks().put(&c.key, g, n_groups, Rc::new(pack_bits(m)), c.like.clone());
        for (k, &b) in keep.iter_mut().zip(m) {
            *k &= b;
        }
    };
    // contains-LIKE extending a cached needle: verify only the rows the
    // superset admits, straight off the image — no blob scan, no column copy
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
        // integer comparison straight off the raw narrow segment — widening
        // the lane into Vec<i64> costs more than the compare
        let int_fast = int_cmp_lit(&c.expr).and_then(|(col, lo, hi, inv)| {
            table
                .with_fixed_segments(g, col, |vals, w, valid| {
                    let mut m = vec![0u8; rows];
                    // Bounds are clamped to the stored width so the compare
                    // runs at that width — verified in the wasm disassembly:
                    // i64 bounds forced an extend-to-i64x2 chain (2 lanes/op);
                    // clamped i16 bounds compare as i16x8 (8 lanes/op).
                    macro_rules! sweep {
                        ($t:ty, $w:expr, |$i:ident, $ch:ident| $x:expr) => {{
                            if lo > <$t>::MAX as i64 || hi < <$t>::MIN as i64 {
                                m.fill(inv as u8); // empty range
                            } else {
                                let lo = lo.max(<$t>::MIN as i64) as $t;
                                let hi = hi.min(<$t>::MAX as i64) as $t;
                                for ($i, $ch) in vals[..rows * $w].chunks_exact($w).enumerate() {
                                    let x: $t = $x;
                                    m[$i] = ((x >= lo && x <= hi) != inv) as u8;
                                }
                            }
                        }};
                    }
                    match w {
                        1 => sweep!(i8, 1, |i, ch| ch[0] as i8),
                        2 => sweep!(i16, 2, |i, ch| i16::from_le_bytes([ch[0], ch[1]])),
                        4 => sweep!(i32, 4, |i, ch| i32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]])),
                        _ => sweep!(i64, 8, |i, ch| i64::from_le_bytes([
                            ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7]
                        ])),
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
        let mut full_cols: Vec<usize> = full.iter().flat_map(|c| c.cols.iter().copied()).collect();
        full_cols.sort_unstable();
        full_cols.dedup();
        load_columns(table, g, dicts, cols, &full_cols, usize::MAX)?;
        for c in full {
            let fctx = GroupCtx { cols: core::mem::take(cols), rows };
            let v = eval_vec(&c.expr, &fctx);
            *cols = fctx.cols;
            let m: Vec<u8> = (0..rows).map(|i| (v.bool3_at(i) == Some(true)) as u8).collect();
            store(table, c, &m, &mut keep);
        }
    }
    Ok(Some(keep))
}
