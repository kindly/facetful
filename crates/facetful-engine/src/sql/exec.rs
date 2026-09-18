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


mod value;
mod vector;
mod eval;
mod scalar;
mod distinct;
mod agg;
mod output;
mod filter;
mod grouping;
use vector::*;
use eval::*;
use scalar::*;
use distinct::*;
use agg::*;
use output::*;
use filter::*;
use grouping::*;
pub use value::{OutCol, QueryResult, Val};

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Round doubles differ only in their exponent and top mantissa bits; a
    /// hash that draws its index from below those bits puts them all in one
    /// probe chain (measured: 4x slower than the SipHash set it replaced).
    #[test]
    fn distinct_table_spreads_high_bit_keys() {
        let mut t = DistinctU64::new();
        t.grow(1);
        // 0.5, 1.0, 1.5 … 2048.0: the old index put these 4096 keys on 32 slots
        let keys: Vec<u64> = (1..=4096).map(|k| (k as f64 * 0.5).to_bits()).collect();
        for &k in &keys {
            t.insert(0, k);
        }
        assert_eq!(t.counts[0], 4096);
        let mut hit = vec![false; t.slots.len()];
        for &k in &keys {
            hit[t.index(0, k)] = true;
        }
        let spread = hit.iter().filter(|&&h| h).count();
        // uniform hashing of 4096 keys into 16384 slots lands on ~3600 of them
        assert!(spread > 3000, "{spread} home slots for 4096 keys in {}", t.slots.len());
    }

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

    let mut direct = if q.is_aggregate { packed_plan(table, &q.group_by) } else { None };
    let mut textg =
        if q.is_aggregate && direct.is_none() { text_plan(table, &q.group_by) } else { None };
    let mut hash_groups: HashMap<Vec<Val>, usize> = HashMap::new();
    // one of these holds the keys: packed codes under a PackedGroups plan,
    // materialized tuples otherwise (expression keys, plain-text keys)
    let mut group_codes: Vec<u64> = Vec::new();
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
                let fast = fast_dims(&code_cols, &d.dims);
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
                // one loop body, specialized per lookup: testing the
                // Option inside the row loop measurably slowed the wasm build
                macro_rules! group_rows {
                    (|$composite:ident| $lookup:expr) => {
                        for i in 0..rows {
                            if !kept(i) {
                                continue;
                            }
                            let $composite = composite_of(&fast, i);
                            let gid: u32 = $lookup;
                            if gid as usize == n_groups {
                                group_codes.push($composite);
                                n_groups += 1;
                                if count_only {
                                    local_counts.push(0);
                                }
                            }
                            if count_only {
                                local_counts[gid as usize] += 1;
                            } else {
                                gids[i] = gid;
                            }
                        }
                    };
                }
                match &mut d.dense {
                    Some(dense) => group_rows!(|composite| {
                        let slot = &mut dense[composite as usize];
                        if *slot < 0 {
                            *slot = n_groups as i32;
                        }
                        *slot as u32
                    }),
                    None => group_rows!(|composite| d.map.get_or_insert(composite, n_groups as u32)),
                }
                // one growth per batch, not one per group
                for a in &mut accs {
                    a.grow(n_groups);
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
            } else if let Some(tg) = &mut textg {
                let code_cols: Vec<(VV, usize)> =
                    tg.pcols.iter().zip(&tg.pcards).map(|(&c, &card)| (ctx.column(c), card)).collect();
                let fast = fast_dims(&code_cols, &tg.pdims);
                // text dims: borrowed views of the blob, no strings
                struct TextLane<'a> {
                    offsets: &'a [u32],
                    bytes: &'a [u8],
                    valid: Option<&'a [u8]>,
                }
                let tlanes: Vec<TextLane> = tg
                    .dims
                    .iter()
                    .filter_map(|d| match d {
                        TextDim::Text(c) => Some(*c),
                        TextDim::Packed => None,
                    })
                    .map(|c| match &ctx.cols[&c] {
                        (GroupCol::Text { offsets, bytes, .. }, valid) => TextLane {
                            offsets,
                            bytes,
                            valid: valid.as_deref().map(|v| v.as_slice()),
                        },
                        _ => unreachable!("text plan over a plain Utf8 column"),
                    })
                    .collect();
                let mut texts: Vec<Option<&[u8]>> = vec![None; tlanes.len()];
                let mut gids = vec![u32::MAX; rows];
                for i in 0..rows {
                    if !kept(i) {
                        continue;
                    }
                    let composite = composite_of(&fast, i);
                    let mut hash = composite.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                    for (k, tl) in tlanes.iter().enumerate() {
                        let ok = tl.valid.map_or(true, |v| v[i / 8] >> (i % 8) & 1 != 0);
                        let b = ok.then(|| &tl.bytes[tl.offsets[i] as usize..tl.offsets[i + 1] as usize]);
                        // a NULL key hashes as a constant no byte string maps to
                        hash = mix64(hash ^ b.map_or(0x4E55_4C4C, hash_bytes));
                        texts[k] = b;
                    }
                    let gid = tg.get_or_insert(hash, composite, &texts, n_groups as u32);
                    if gid as usize == n_groups {
                        n_groups += 1;
                    }
                    gids[i] = gid;
                }
                for a in &mut accs {
                    a.grow(n_groups);
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
                    }
                    gids[i] = gid as u32;
                }
                for a in &mut accs {
                    a.grow(n_groups);
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

        let order_refs: Vec<(u32, u32)> = if numeric_keys && nk == 1 {
            let (_, dir) = &q.order_by[0];
            let desc = *dir == SortDir::Desc;
            let ty = ord_tys[0];
            let mut keyed: Vec<(u8, u64, u32, u32)> = refs
                .iter()
                .map(|&(g, r)| {
                    let (v, k) = encode_order(&sort_groups[g as usize].0[0], r as usize, ty, desc);
                    (v, k, g, r)
                })
                .collect();
            keyed.sort_unstable_by_key(|t| (t.0, t.1));
            keyed.into_iter().map(|t| (t.2, t.3)).collect()
        } else if numeric_keys {
            let mut flat: Vec<(u8, u64)> = Vec::with_capacity(refs.len() * nk);
            for &(g, r) in &refs {
                for (ki, (_, dir)) in q.order_by.iter().enumerate() {
                    flat.push(encode_order(
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

    let offset = q.offset.unwrap_or(0) as usize;
    let limit = q.limit.map(|l| l as usize).unwrap_or(usize::MAX);

    if q.is_aggregate {
        if q.group_by.is_empty() && n_groups == 0 {
            n_groups = 1;
            for a in &mut accs {
                a.grow(1);
            }
        }
        // The output phase is a vectorized pass over the GROUP TABLE — one
        // row per group, aggregates and keys as columns under synthetic
        // indices, select/order expressions rewritten onto them and run by
        // the same kernels as the scan — not a per-group tree walk. Keys of
        // a packed plan land as code lanes: no string is touched until the
        // surviving rows are gathered.
        let base = table.catalog().schema.columns.len();
        let mut gcols: HashMap<usize, (GroupCol, Option<Rc<Vec<u8>>>)> = HashMap::new();
        for (i, (acc, call)) in accs.iter().zip(&agg_calls).enumerate() {
            let vv = lanes_to_vv(n_groups, call.ty(), |g| acc.finish(g));
            gcols.insert(base + i, (GroupCol::Ready(vv), None));
        }
        let key_base = base + agg_calls.len();
        if let Some(plan) = &direct {
            let lanes = packed_key_lanes(&group_codes, &plan.cols, &plan.dims, &plan.cards, &dicts);
            for (k, lane) in lanes.into_iter().enumerate() {
                gcols.insert(key_base + k, lane);
            }
        } else if let Some(tg) = &textg {
            let mut packed =
                packed_key_lanes(&tg.codes, &tg.pcols, &tg.pdims, &tg.pcards, &dicts).into_iter();
            let nt = tg.n_text();
            let mut tk = 0;
            for (k, dim) in tg.dims.iter().enumerate() {
                let lane = match dim {
                    TextDim::Packed => packed.next().unwrap(),
                    TextDim::Text(_) => {
                        // the arena, compacted per text dim: a raw text column
                        let mut offsets = Vec::with_capacity(n_groups + 1);
                        offsets.push(0u32);
                        let mut bytes = Vec::new();
                        let mut valid = vec![0u8; n_groups.div_ceil(8)];
                        for g in 0..n_groups {
                            let (start, len) = tg.spans[g * nt + tk];
                            if len != u32::MAX {
                                bytes.extend_from_slice(&tg.arena[start as usize..(start + len) as usize]);
                                valid[g / 8] |= 1 << (g % 8);
                            }
                            offsets.push(bytes.len() as u32);
                        }
                        tk += 1;
                        (
                            GroupCol::Text {
                                strs: std::cell::OnceCell::new(),
                                offsets: Rc::new(offsets),
                                bytes: Rc::new(bytes),
                            },
                            Some(Rc::new(valid)),
                        )
                    }
                };
                gcols.insert(key_base + k, lane);
            }
        } else {
            for (k, g_expr) in q.group_by.iter().enumerate() {
                let vv = lanes_to_vv(n_groups, g_expr.ty(), |g| group_keys[g][k].clone());
                gcols.insert(key_base + k, (GroupCol::Ready(vv), None));
            }
        }
        let gctx = GroupCtx { cols: gcols, rows: n_groups };
        let rewrite = |b: &Bound| substitute_grouped(b, &q.group_by, &agg_calls, base, key_base);

        // ORDER BY over every group; numeric keys pack to u64 and sort as
        // integers, anything else compares through cmp_sql. Stable, so ties
        // keep group discovery order.
        let order: Vec<u32> = if q.order_by.is_empty() {
            (0..n_groups as u32).collect()
        } else {
            let ord_vvs: Vec<VV> =
                q.order_by.iter().map(|(e, _)| eval_vec(&rewrite(e), &gctx)).collect();
            let nk = ord_vvs.len();
            let numeric = ord_tys
                .iter()
                .all(|t| matches!(t, Ty::Int | Ty::Float | Ty::Date | Ty::Timestamp));
            let mut perm: Vec<u32> = (0..n_groups as u32).collect();
            if numeric && nk == 1 {
                let (_, dir) = &q.order_by[0];
                let desc = *dir == SortDir::Desc;
                let mut keyed: Vec<(u8, u64, u32)> = (0..n_groups)
                    .map(|g| {
                        let (v, k) = encode_order(&ord_vvs[0], g, ord_tys[0], desc);
                        (v, k, g as u32)
                    })
                    .collect();
                // gid last: ties keep discovery order, and unstable is fine
                keyed.sort_unstable();
                perm = keyed.into_iter().map(|t| t.2).collect();
            } else if numeric {
                let mut flat: Vec<(u8, u64)> = Vec::with_capacity(n_groups * nk);
                for g in 0..n_groups {
                    for (ki, (_, dir)) in q.order_by.iter().enumerate() {
                        flat.push(encode_order(&ord_vvs[ki], g, ord_tys[ki], *dir == SortDir::Desc));
                    }
                }
                perm.sort_unstable_by(|&a, &b| {
                    let (ia, ib) = (a as usize * nk, b as usize * nk);
                    flat[ia..ia + nk].cmp(&flat[ib..ib + nk]).then(a.cmp(&b))
                });
            } else {
                let keys: Vec<Vec<Val>> = (0..n_groups)
                    .map(|g| ord_vvs.iter().zip(&ord_tys).map(|(v, t)| v.val_at(g, *t)).collect())
                    .collect();
                perm.sort_by(|&a, &b| cmp_keys(&keys[a as usize], &keys[b as usize], &q.order_by));
            }
            perm
        };
        // project only the survivors, straight into the columnar channel
        let refs: Vec<(u32, u32)> =
            order.into_iter().skip(offset).take(limit).map(|g| (0, g)).collect();
        let cols: Vec<OutCol> = q
            .select
            .iter()
            .zip(&sel_tys)
            .map(|(sel, ty)| {
                let expr = rewrite(&sel.expr);
                // a text key selected as-is gathers off its raw lane: no
                // Rc<String> is ever built for it
                if let Bound::Column { index, ty: Ty::Text } = &expr {
                    if let Some((GroupCol::Text { offsets, bytes, .. }, valid)) = gctx.cols.get(index) {
                        let src = SelSrc::RawText(vec![(offsets.clone(), bytes.clone(), valid.clone())]);
                        return gather_outcol(&src, &refs, *ty);
                    }
                }
                gather_outcol(&SelSrc::Vv(vec![eval_vec(&expr, &gctx)]), &refs, *ty)
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

    // every non-aggregate path returned columnar above; this only drains
    // the (empty) row seed
    let rows: Vec<Vec<Val>> =
        out_rows.into_iter().skip(offset).take(limit).map(|(p, _)| p).collect();
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
