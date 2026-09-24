//! Projection sources, columnar gathering, and the group-table rewrite of output expressions.

use super::*;

/// Gather one select expression column-wise over ordered (gslot, row) refs.
/// `vvs[gslot]` = evaluated VV per group; `raw[gslot]` = (offsets, bytes,
/// validity) when the expr is a direct plain-text column (skips VStr lanes).
pub(super) enum SelSrc {
    Vv(Vec<VV>),
    RawText(Vec<(Rc<Vec<u32>>, Rc<Vec<u8>>, Option<Rc<Vec<u8>>>)>),
    /// a dictionary column selected as-is: per group its codes + validity,
    /// one dictionary shared by every group (the image's is table-wide)
    Dict { groups: Vec<(Rc<Vec<u16>>, Option<Rc<Vec<u8>>>)>, dict: Rc<Vec<VStr>> },
    /// an expression whose evaluation waits for the window: the strategy
    /// keeps each group's lanes and `project` runs it over the surviving
    /// rows only (a paged query pays for its page, not for every kept row)
    Deferred,
    /// a plain-text column selected as-is: gathered at finish straight off
    /// the image's resident segments (no lane is ever copied for it)
    Segments(usize),
}

pub(super) fn gather_outcol(src: &SelSrc, refs: &[(u32, u32)], ty: Ty) -> OutCol {
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
        SelSrc::Dict { groups, dict } => {
            // codes copy at memcpy-like speed; no string is touched
            let mut codes = Vec::with_capacity(n);
            for (i, &(g, r)) in refs.iter().enumerate() {
                let (c, v) = &groups[g as usize];
                let r = r as usize;
                if v.as_deref().map_or(true, |vb| vb[r / 8] >> (r % 8) & 1 != 0) {
                    valid[i / 8] |= 1 << (i % 8);
                }
                codes.push(c[r]);
            }
            OutCol::Dict { codes, dict: dict.clone(), valid }
        }
        SelSrc::Deferred | SelSrc::Segments(_) => unreachable!("evaluated by `project`"),
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

/// Build per-select sources for one group: direct text and dictionary
/// columns give lane access; everything else evaluates to a VV now, or —
/// with `defer` — waits for the window (the caller keeps the group's lanes).
pub(super) fn sel_srcs_for_group(
    q: &crate::sql::binder::BoundQuery,
    ctx: &GroupCtx,
    srcs: &mut [Option<SelSrc>],
    defer: bool,
    direct_text: &[Option<usize>],
) {
    for (si, sel) in q.select.iter().enumerate() {
        if let Some(col) = direct_text[si] {
            srcs[si] = Some(SelSrc::Segments(col));
            continue;
        }
        let lane = match &sel.expr {
            Bound::Column { index, ty: Ty::Text } => ctx.cols.get(index),
            _ => None,
        };
        let slot = &mut srcs[si];
        match (lane, &mut *slot) {
            (Some((GroupCol::Text { offsets, bytes, .. }, validity)), Some(SelSrc::RawText(v))) => {
                v.push((offsets.clone(), bytes.clone(), validity.clone()))
            }
            (Some((GroupCol::Text { offsets, bytes, .. }, validity)), None) => {
                *slot = Some(SelSrc::RawText(vec![(offsets.clone(), bytes.clone(), validity.clone())]))
            }
            (Some((GroupCol::Dict { codes, .. }, validity)), Some(SelSrc::Dict { groups, .. })) => {
                groups.push((codes.clone(), validity.clone()))
            }
            (Some((GroupCol::Dict { codes, dict }, validity)), None) => {
                *slot = Some(SelSrc::Dict {
                    groups: vec![(codes.clone(), validity.clone())],
                    dict: dict.clone(),
                })
            }
            (Some((GroupCol::Text { .. } | GroupCol::Dict { .. }, _)), Some(_)) => {
                unreachable!("select expr shape is stable across groups")
            }
            (_, Some(SelSrc::Deferred | SelSrc::Segments(_))) => {}
            (_, Some(SelSrc::Vv(v))) => v.push(eval_vec(&sel.expr, ctx)),
            (_, None) if defer => *slot = Some(SelSrc::Deferred),
            (_, None) => *slot = Some(SelSrc::Vv(vec![eval_vec(&sel.expr, ctx)])),
            (_, Some(_)) => unreachable!("select expr shape is stable across groups"),
        }
    }
}

/// Run `f` over one group's plain-text column as `(offset(k), blob, validity)`:
/// borrowed from the resident image segments when they are, else from a lane
/// loaded for the purpose (tiny positional-cache budgets).
fn with_texts<S: ReadAt, R>(
    table: &mut Table<S>,
    g: usize,
    col: usize,
    rows: usize,
    f: impl FnOnce(&dyn Fn(usize) -> u32, &[u8], Option<&[u8]>) -> R,
) -> Result<R, FormatError> {
    let mut f = Some(f);
    let got = table.with_text_segments(g, col, |offs, blob, valid| {
        let at = |k: usize| u32::from_le_bytes(offs[k * 4..k * 4 + 4].try_into().unwrap());
        (f.take().unwrap())(&at, blob, valid)
    })?;
    match got {
        Some(r) => Ok(r),
        None => {
            let (offs, blob) = table.texts_raw(g, col, rows)?;
            let valid = table.validity(g, col)?;
            let at = |k: usize| offs[k];
            Ok((f.take().unwrap())(&at, &blob, valid.as_deref()))
        }
    }
}

/// A plain-text column for `refs`, gathered straight off the image: one pass
/// for lengths and validity, one to copy the bytes into place. Nothing but
/// the output is allocated.
fn gather_segments<S: ReadAt>(
    table: &mut Table<S>,
    col: usize,
    refs: &[(u32, u32)],
    groups: &[usize],
) -> Result<OutCol, FormatError> {
    let n = refs.len();
    let mut by_group: Vec<Vec<u32>> = vec![Vec::new(); groups.len()];
    for (i, &(gs, _)) in refs.iter().enumerate() {
        by_group[gs as usize].push(i as u32);
    }
    let mut valid = vec![0u8; n.div_ceil(8)];
    let mut offsets = vec![0u32; n + 1];
    for (gs, idxs) in by_group.iter().enumerate() {
        if idxs.is_empty() {
            continue;
        }
        let rows = idxs.iter().map(|&i| refs[i as usize].1 as usize + 1).max().unwrap();
        with_texts(table, groups[gs], col, rows, |at, _blob, v| {
            for &i in idxs {
                let (i, r) = (i as usize, refs[i as usize].1 as usize);
                if v.is_some_and(|v| v[r / 8] >> (r % 8) & 1 == 0) {
                    continue; // NULL: zero length, bit clear
                }
                valid[i / 8] |= 1 << (i % 8);
                offsets[i + 1] = at(r + 1) - at(r);
            }
        })?;
    }
    for i in 0..n {
        offsets[i + 1] += offsets[i];
    }
    let mut bytes = vec![0u8; offsets[n] as usize];
    for (gs, idxs) in by_group.iter().enumerate() {
        if idxs.is_empty() {
            continue;
        }
        let rows = idxs.iter().map(|&i| refs[i as usize].1 as usize + 1).max().unwrap();
        with_texts(table, groups[gs], col, rows, |at, blob, _v| {
            for &i in idxs {
                let (i, r) = (i as usize, refs[i as usize].1 as usize);
                let (o, e) = (offsets[i] as usize, offsets[i + 1] as usize);
                if e > o {
                    let a = at(r) as usize;
                    bytes[o..e].copy_from_slice(&blob[a..a + (e - o)]);
                }
            }
        })?;
    }
    Ok(OutCol::Text { offsets, bytes, valid })
}

/// The output columns for `refs` (group slot, row): lane sources gather by
/// reference; direct text columns gather off the image (`groups[gslot]` is
/// the row group behind a slot); deferred expressions evaluate over the
/// surviving rows only, in contexts gathered from the kept groups' lanes
/// (`ctxs[gslot]`).
pub(super) fn project<S: ReadAt>(
    table: &mut Table<S>,
    q: &crate::sql::binder::BoundQuery,
    sel_tys: &[Ty],
    srcs: &[Option<SelSrc>],
    refs: &[(u32, u32)],
    groups: &[usize],
    ctxs: Option<&[Cols]>,
) -> Result<Vec<OutCol>, FormatError> {
    let deferred: Vec<usize> = srcs
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, Some(SelSrc::Deferred)))
        .map(|(i, _)| i)
        .collect();
    let mut late: HashMap<usize, OutCol> = HashMap::new();
    if !deferred.is_empty() {
        let ctxs = ctxs.expect("deferred select sources need the groups' lanes");
        // the surviving rows of each group, in ref order; a ref becomes
        // (sub-context index, position) so gather_outcol reads the small lanes
        let mut rows_of: Vec<Vec<u32>> = vec![Vec::new(); ctxs.len()];
        for &(g, r) in refs {
            rows_of[g as usize].push(r);
        }
        let mut sub_of: Vec<u32> = vec![u32::MAX; ctxs.len()];
        let mut subs: Vec<GroupCtx> = Vec::new();
        for (g, rows) in rows_of.iter().enumerate() {
            if !rows.is_empty() {
                sub_of[g] = subs.len() as u32;
                subs.push(gather_ctx(&ctxs[g], rows));
            }
        }
        let mut next: Vec<u32> = vec![0; ctxs.len()];
        let local: Vec<(u32, u32)> = refs
            .iter()
            .map(|&(g, _)| {
                let pos = next[g as usize];
                next[g as usize] += 1;
                (sub_of[g as usize], pos)
            })
            .collect();
        for &si in &deferred {
            let vvs: Vec<VV> = subs.iter().map(|sub| eval_vec(&q.select[si].expr, sub)).collect();
            late.insert(si, gather_outcol(&SelSrc::Vv(vvs), &local, sel_tys[si]));
        }
    }
    let mut out = Vec::with_capacity(srcs.len());
    for (si, (src, ty)) in srcs.iter().zip(sel_tys).enumerate() {
        out.push(match (late.remove(&si), src) {
            (Some(c), _) => c,
            (None, Some(SelSrc::Segments(col))) => gather_segments(table, *col, refs, groups)?,
            (None, Some(src)) => gather_outcol(src, refs, *ty),
            (None, None) => gather_outcol(&SelSrc::Vv(Vec::new()), &[], *ty), // zero groups scanned
        });
    }
    Ok(out)
}


/// Rewrite an output expression against the group table: an aggregate call
/// becomes the column holding its finished values, a GROUP BY expression the
/// column holding its keys, and the rest evaluates as any scan expression
/// would. A bare column that is neither cannot occur in a valid aggregate
/// query (the binder rejects it); it reads as NULL, as it always did.
pub(super) fn substitute_grouped(
    b: &Bound,
    group_by: &[Bound],
    aggs: &[Bound],
    agg_base: usize,
    key_base: usize,
) -> Bound {
    if let Some(i) = aggs.iter().position(|a| a == b) {
        return Bound::Column { index: agg_base + i, ty: b.ty() };
    }
    if let Some(k) = group_by.iter().position(|g| g == b) {
        return Bound::Column { index: key_base + k, ty: b.ty() };
    }
    let sub = |e: &Bound| substitute_grouped(e, group_by, aggs, agg_base, key_base);
    match b {
        Bound::Column { .. } => Bound::Null,
        Bound::Unary { op, expr, ty } => Bound::Unary { op: *op, expr: Box::new(sub(expr)), ty: *ty },
        Bound::Binary { op, lhs, rhs, ty } => {
            Bound::Binary { op: *op, lhs: Box::new(sub(lhs)), rhs: Box::new(sub(rhs)), ty: *ty }
        }
        Bound::Call { func, args, ty } => {
            Bound::Call { func, args: args.iter().map(sub).collect(), ty: *ty }
        }
        other => other.clone(),
    }
}

/// Order-preserving (validity, bits) key for one numeric ORDER BY lane value,
/// so sorting is an integer compare. Validity first so NULLs sort first
/// ascending (and last after a DESC flip), matching `cmp_sql`.
pub(super) fn encode_order(vv: &VV, i: usize, ty: Ty, desc: bool) -> (u8, u64) {
    let (v, k) = if !vv.is_valid(i) {
        (0u8, 0u64)
    } else if ty == Ty::Float {
        let b = vv.f64_at(i).to_bits();
        (1, if b >> 63 == 1 { !b } else { b | (1u64 << 63) })
    } else {
        (1, (vv.i64_at(i) as u64) ^ (1u64 << 63))
    };
    if desc { (1 - v, !k) } else { (v, k) }
}

/// Compare two ORDER BY key tuples under the query's directions.
pub(super) fn cmp_keys(a: &[Val], b: &[Val], order_by: &[(Bound, SortDir)]) -> core::cmp::Ordering {
    for (i, (_, dir)) in order_by.iter().enumerate() {
        let ord = a[i].cmp_sql(&b[i]);
        let ord = if *dir == SortDir::Desc { ord.reverse() } else { ord };
        if ord != core::cmp::Ordering::Equal {
            return ord;
        }
    }
    core::cmp::Ordering::Equal
}

/// Sort a permutation of indices by their packed numeric keys — `nk`
/// `(validity, bits)` pairs per index. Unstable, and deliberately without
/// an index tiebreak: making every key distinct defeats the sort's
/// equal-element fast path, which real data (capacities, years) hits hard —
/// measured +9% on a two-key sort of 183K rows. A caller that wants
/// deterministic ties appends one as an extra key. One instantiation serves
/// every caller: each distinct closure handed to `sort_by` is a separate
/// copy of the algorithm.
pub(super) fn sort_perm_packed(perm: &mut [u32], flat: &[(u8, u64)], nk: usize) {
    perm.sort_unstable_by(move |&a, &b| {
        let (ia, ib) = (a as usize * nk, b as usize * nk);
        flat[ia..ia + nk].cmp(&flat[ib..ib + nk])
    });
}

/// Single-key numeric sorts over 16-byte `(validity, bits, payload)`
/// tuples — every caller in the crate sorts through one of these two, so
/// there are two instantiations of the algorithm, not one per call site.
/// Two on purpose: the key is the first two fields here, because real data
/// ties constantly and a tiebreak defeats the sort's equal-element fast path
/// (measured +10% on the 183K-row full sort) …
pub(crate) fn sort_keyed2(v: &mut [(u8, u64, u32)]) {
    v.sort_unstable_by_key(|t| (t.0, t.1));
}

/// … and here the payload (a gid) is the tiebreak, so ties keep a
/// deterministic order — worth it where the rows are groups a UI will show.
pub(super) fn sort_keyed3(v: &mut [(u8, u64, u32)]) {
    v.sort_unstable_by_key(|t| (t.0, t.1, t.2));
}

/// Sort a permutation by materialized key tuples through `cmp_sql`. Stable.
/// Concrete `&[Vec<Val>]` rather than a closure or `dyn`: one instantiation,
/// static dispatch (a `dyn` call per compare measured +10% on a text sort).
pub(super) fn sort_perm_keys(perm: &mut [u32], keys: &[Vec<Val>], order_by: &[(Bound, SortDir)]) {
    perm.sort_by(|&a, &b| cmp_keys(&keys[a as usize], &keys[b as usize], order_by));
}
