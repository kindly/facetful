//! Vectorized execution (M6 stage 1). Expressions evaluate ONCE per row group
//! into column vectors; per-row work happens only inside flat kernel loops.
//! Hot paths are specialized: numeric comparisons, dictionary-code fast paths
//! for =/IN/LIKE against string literals (predicates evaluated once per
//! dictionary entry, scans compare integers), three-valued mask logic,
//! aggregation update loops, and direct-indexed grouping when every group-by
//! dim is a dict column. Cold ops (string functions, CASE, casts) run through
//! per-lane accessors — still no per-row tree walks or Val allocations.

use super::ast::{BinOp, SortDir, UnOp};
use super::binder::{Bound, BoundQuery, FuncKind, Sig, Ty};
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
mod aggregate;
mod plain;
mod sort;
mod topk;
use vector::*;
use eval::*;
use scalar::*;
use distinct::*;
use agg::*;
use output::*;
use filter::*;
use grouping::*;
pub use value::{OutCol, QueryResult, Val};
pub(crate) use output::sort_keyed2;
pub(crate) use grouping::{hash_bytes, int_range, GroupMap, TextGroups, DENSE_LANES};
pub(crate) use distinct::mix64;

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

/// Per-row-group column lanes, keyed by column index.
type Cols = HashMap<usize, (GroupCol, Option<Rc<Vec<u8>>>)>;
/// Dictionaries of every dict column the query touches.
type Dicts = HashMap<usize, Rc<Vec<VStr>>>;

/// Load `which` columns of row group `g` into `cols` (those not already
/// there), `cap` rows deep.
fn load_columns<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    dicts: &Dicts,
    cols: &mut Cols,
    which: &[usize],
    cap: usize,
) -> Result<(), FormatError> {
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
}

/// What every strategy shares: the bound query, which columns it needs and
/// how they type, the dictionaries, and the WHERE as cacheable conjuncts.
struct Shared<'q> {
    q: &'q BoundQuery,
    columns: Vec<String>,
    dicts: Dicts,
    constraints: Vec<Range>,
    conjuncts: Vec<Conjunct>,
    /// columns the projection, grouping and ordering read (the filter's own
    /// columns load only when a conjunct misses the mask cache)
    proj_needed: Vec<usize>,
    /// just the ORDER BY columns — all a top-k scan needs
    ord_needed: Vec<usize>,
    sel_tys: Vec<Ty>,
    ord_tys: Vec<Ty>,
    scanned_groups: usize,
}

impl<'q> Shared<'q> {
    fn new<S: ReadAt>(table: &mut Table<S>, q: &'q BoundQuery) -> Result<Self, FormatError> {
        let cols_of = |exprs: &[&Bound]| -> Vec<usize> {
            let mut v = Vec::new();
            exprs.iter().for_each(|e| collect_columns(e, &mut v));
            v.sort_unstable();
            v.dedup();
            v
        };
        let sel: Vec<&Bound> = q.select.iter().map(|s| &s.expr).collect();
        let ord: Vec<&Bound> = q.order_by.iter().map(|(e, _)| e).collect();
        let grp: Vec<&Bound> = q.group_by.iter().collect();
        let proj_needed = cols_of(&[sel.as_slice(), grp.as_slice(), ord.as_slice()].concat());
        let ord_needed = cols_of(&ord);
        let mut needed = proj_needed.clone();
        if let Some(f) = &q.filter {
            collect_columns(f, &mut needed);
        }
        let mut dicts: Dicts = HashMap::new();
        for &c in &needed {
            if table.catalog().schema.columns[c].is_dict() && !dicts.contains_key(&c) {
                dicts.insert(c, table.dictionary_rc(c)?);
            }
        }
        Ok(Shared {
            q,
            columns: q.select.iter().map(|s| s.name.clone()).collect(),
            dicts,
            constraints: q.filter.as_ref().map(collect_ranges).unwrap_or_default(),
            conjuncts: conjuncts_of(table, q.filter.as_ref()),
            proj_needed,
            ord_needed,
            sel_tys: q.select.iter().map(|s| s.expr.ty()).collect(),
            ord_tys: q.order_by.iter().map(|(e, _)| e.ty()).collect(),
            scanned_groups: 0,
        })
    }

    fn load<S: ReadAt>(
        &self,
        table: &mut Table<S>,
        g: usize,
        cols: &mut Cols,
        which: &[usize],
        cap: usize,
    ) -> Result<(), FormatError> {
        load_columns(table, g, &self.dicts, cols, which, cap)
    }

    /// The result envelope every strategy fills the same way.
    fn result<S: ReadAt>(
        &self,
        table: &Table<S>,
        rows: Vec<Vec<Val>>,
        cols: Option<Vec<OutCol>>,
        out_rows: usize,
    ) -> QueryResult {
        QueryResult {
            columns: self.columns.clone(),
            col_types: self.sel_tys.clone(),
            rows,
            out_rows,
            cols,
            scanned_groups: self.scanned_groups,
            total_groups: table.group_count(),
        }
    }

    fn window(&self) -> (usize, usize) {
        let offset = self.q.offset.unwrap_or(0) as usize;
        let limit = self.q.limit.map(|l| l as usize).unwrap_or(usize::MAX);
        (offset, limit)
    }
}

/// Whether the scan continues after this row group.
enum Flow {
    Continue,
    Stop,
}

/// How a query runs. Chosen once, before the scan; each variant owns only
/// its own state and lives in its own module.
enum Strategy {
    /// GROUP BY / aggregates: accumulate per group, then a vectorized pass
    /// over the group table
    Aggregate(aggregate::Aggregate),
    /// ORDER BY + small LIMIT: bounded candidates, project only the winners
    TopK(topk::TopK),
    /// ORDER BY without a usable bound: sort packed keys + refs, project after
    FullSort(sort::FullSort),
    /// projection, optionally LIMIT-capped: stop the scan when enough rows exist
    Plain(plain::Plain),
}

impl Strategy {
    fn plan<S: ReadAt>(table: &mut Table<S>, sh: &Shared) -> Result<Strategy, FormatError> {
        let q = sh.q;
        if q.is_aggregate {
            return Ok(Strategy::Aggregate(aggregate::Aggregate::plan(table, sh)?));
        }
        let cap = q.limit.map(|l| l as usize + q.offset.unwrap_or(0) as usize);
        if q.order_by.is_empty() {
            return Ok(Strategy::Plain(plain::Plain::new(q.select.len(), cap)));
        }
        Ok(match cap.filter(|c| *c <= 100_000) {
            Some(cap) => Strategy::TopK(topk::TopK::new(cap)),
            None => Strategy::FullSort(sort::FullSort::new(q.select.len())),
        })
    }

    /// Which lanes this group needs, how deep to load them, and how many
    /// rows to evaluate.
    fn load_plan<'s>(&self, sh: &'s Shared, keep: Option<&[u8]>, rows: usize) -> (&'s [usize], usize, usize) {
        match self {
            Strategy::TopK(_) => (&sh.ord_needed, usize::MAX, rows),
            Strategy::Plain(p) => {
                let depth = p.depth(keep, rows);
                (&sh.proj_needed, depth, depth)
            }
            Strategy::Aggregate(_) | Strategy::FullSort(_) => (&sh.proj_needed, rows, rows),
        }
    }

    fn scan_group(&mut self, sh: &Shared, g: usize, ctx: &GroupCtx, keep: Option<&[u8]>) -> Flow {
        match self {
            Strategy::Aggregate(a) => a.scan_group(sh, ctx, keep),
            Strategy::TopK(t) => t.scan_group(sh, g, ctx, keep),
            Strategy::FullSort(f) => f.scan_group(sh, ctx, keep),
            Strategy::Plain(p) => return p.scan_group(sh, ctx, keep),
        }
        Flow::Continue
    }

    fn finish<S: ReadAt>(self, table: &mut Table<S>, sh: &Shared) -> Result<QueryResult, FormatError> {
        match self {
            Strategy::Aggregate(a) => a.finish(table, sh),
            Strategy::TopK(t) => t.finish(table, sh),
            Strategy::FullSort(f) => Ok(f.finish(table, sh)),
            Strategy::Plain(p) => Ok(p.finish(table, sh)),
        }
    }
}

pub fn execute<S: ReadAt>(
    table: &mut Table<S>,
    q: &BoundQuery,
) -> Result<QueryResult, FormatError> {
    let folded = fold_query(q, table.catalog());
    let mut sh = Shared::new(table, folded.as_ref())?;
    let mut strategy = Strategy::plan(table, &sh)?;

    for g in 0..table.group_count() {
        if group_prunable(table, g, &sh.constraints) {
            continue;
        }
        sh.scanned_groups += 1;
        let rows = table.group_rows(g);
        let mut cols: Cols = HashMap::new();
        let keep = where_mask(table, g, rows, &sh.conjuncts, &sh.dicts, &mut cols)?;
        // fully filtered out: nothing else to load or evaluate for this group
        if keep.as_ref().is_some_and(|k| k.iter().all(|&b| b == 0)) {
            continue;
        }
        let (which, cap, eval_rows) = strategy.load_plan(&sh, keep.as_deref(), rows);
        sh.load(table, g, &mut cols, which, cap)?;
        let ctx = GroupCtx { cols, rows: eval_rows };
        if let Flow::Stop = strategy.scan_group(&sh, g, &ctx, keep.as_deref()) {
            break;
        }
    }
    strategy.finish(table, &sh)
}
