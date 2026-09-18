//! Projection sources, columnar gathering, and the group-table rewrite of output expressions.

use super::*;

/// Gather one select expression column-wise over ordered (gslot, row) refs.
/// `vvs[gslot]` = evaluated VV per group; `raw[gslot]` = (offsets, bytes,
/// validity) when the expr is a direct plain-text column (skips VStr lanes).
pub(super) enum SelSrc {
    Vv(Vec<VV>),
    RawText(Vec<(Rc<Vec<u32>>, Rc<Vec<u8>>, Option<Rc<Vec<u8>>>)>),
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
pub(super) fn sel_srcs_for_group(
    q: &crate::sql::binder::BoundQuery,
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
