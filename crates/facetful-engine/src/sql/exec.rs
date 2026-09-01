//! Execution: run a BoundQuery against a Table. v1 shape — correct first:
//! per-row-group scan over cached column vectors, a row-wise expression
//! interpreter with SQL three-valued logic and null-skipping aggregates,
//! hash aggregation keyed by group-by values, Val-ordered sort, limit/offset.
//! (Row-group stats pruning and vectorized expression kernels are planned
//! optimizations; the flagship facet path keeps its specialized kernels.)

use super::ast::{BinOp, SortDir, UnOp};
use super::binder::{Bound, FuncKind};
use crate::format::read::ReadAt;
use crate::format::{ColumnType, FormatError};
use crate::Table;
use std::collections::HashMap;
use std::rc::Rc;

/// A computed value. Text is Rc'd so dictionary values clone cheaply per row.
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
    /// SQL ordering: NULL smallest; numbers cross-compare; text lexicographic.
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
                _ => Equal, // incomparable kinds — binder prevents this
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
        // grouping equality: NULLs group together (SQL GROUP BY semantics)
        match (self, other) {
            (Val::Null, Val::Null) => true,
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
            Val::Int(i) => (2u8, *i as f64).to_bits_hash(h),
            Val::Float(f) => (2u8, *f).to_bits_hash(h),
            Val::Text(s) => (3u8, s).hash(h),
        }
    }
}

trait BitsHash {
    fn to_bits_hash<H: core::hash::Hasher>(&self, h: &mut H);
}
impl BitsHash for (u8, f64) {
    fn to_bits_hash<H: core::hash::Hasher>(&self, h: &mut H) {
        use core::hash::Hash;
        self.0.hash(h);
        // Int and Float hash identically when numerically equal (1 == 1.0)
        self.1.to_bits().hash(h);
    }
}

pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Val>>,
}

/// One row group's referenced columns, materialized.
enum GroupCol {
    I64(Vec<i64>),
    F64(Vec<f64>),
    Dict { codes: Vec<u16>, dict: Rc<Vec<Rc<String>>> },
    Text(Vec<Rc<String>>),
}

struct GroupCtx {
    cols: HashMap<usize, (GroupCol, Option<Vec<u8>>)>,
}

impl GroupCtx {
    fn value(&self, col: usize, row: usize) -> Val {
        let (c, validity) = &self.cols[&col];
        if let Some(v) = validity {
            if v[row / 8] & (1 << (row % 8)) == 0 {
                return Val::Null;
            }
        }
        match c {
            GroupCol::I64(v) => Val::Int(v[row]),
            GroupCol::F64(v) => Val::Float(v[row]),
            GroupCol::Dict { codes, dict } => Val::Text(dict[codes[row] as usize].clone()),
            GroupCol::Text(v) => Val::Text(v[row].clone()),
        }
    }
}

pub fn execute<S: ReadAt>(
    table: &mut Table<S>,
    q: &super::binder::BoundQuery,
) -> Result<QueryResult, FormatError> {
    let columns: Vec<String> = q.select.iter().map(|s| s.name.clone()).collect();

    // Which columns do we need to load per group?
    let mut needed = Vec::new();
    let mut visit_all = |b: &Bound| collect_columns(b, &mut needed);
    q.select.iter().for_each(|s| visit_all(&s.expr));
    if let Some(f) = &q.filter {
        visit_all(f);
    }
    q.group_by.iter().for_each(|g| visit_all(g));
    q.order_by.iter().for_each(|(e, _)| visit_all(e));
    needed.sort_unstable();
    needed.dedup();

    // Shared dictionaries, Rc'd once.
    let mut dicts: HashMap<usize, Rc<Vec<Rc<String>>>> = HashMap::new();
    for &c in &needed {
        if table.catalog().schema.columns[c].is_dict() {
            let d = table.dictionary(c)?;
            dicts.insert(c, Rc::new(d.into_iter().map(Rc::new).collect()));
        }
    }

    // Aggregate pipeline state
    let agg_calls: Vec<Bound> = {
        let mut v = Vec::new();
        q.select.iter().for_each(|s| collect_aggs(&s.expr, &mut v));
        q.order_by.iter().for_each(|(e, _)| collect_aggs(e, &mut v));
        v
    };
    let mut groups: HashMap<Vec<Val>, Vec<AggState>> = HashMap::new();
    let mut group_order: Vec<Vec<Val>> = Vec::new(); // stable first-seen order

    // Non-aggregate pipeline: projected rows (+ hidden order keys)
    let mut out_rows: Vec<(Vec<Val>, Vec<Val>)> = Vec::new();

    for g in 0..table.group_count() {
        let rows = table.group_rows(g);
        let mut cols = HashMap::new();
        for &c in &needed {
            let (ty, is_dict) = {
                let def = &table.catalog().schema.columns[c];
                (def.ty, def.is_dict())
            };
            let validity = table.validity(g, c)?;
            let col = if is_dict {
                GroupCol::Dict { codes: table.codes(g, c)?, dict: dicts[&c].clone() }
            } else {
                match ty {
                    ColumnType::Float64 => GroupCol::F64(table.f64s(g, c)?),
                    ColumnType::Utf8 => {
                        GroupCol::Text(table.texts(g, c)?.into_iter().map(Rc::new).collect())
                    }
                    _ => GroupCol::I64(table.i64s(g, c)?),
                }
            };
            cols.insert(c, (col, validity));
        }
        let ctx = GroupCtx { cols };

        for row in 0..rows {
            if let Some(f) = &q.filter {
                // three-valued logic: only TRUE passes
                if !matches!(eval(f, &ctx, row, None), Val::Bool(true)) {
                    continue;
                }
            }
            if q.is_aggregate {
                let key: Vec<Val> =
                    q.group_by.iter().map(|e| eval(e, &ctx, row, None)).collect();
                let states = groups.entry(key.clone()).or_insert_with(|| {
                    group_order.push(key);
                    agg_calls.iter().map(AggState::new).collect()
                });
                for (st, call) in states.iter_mut().zip(&agg_calls) {
                    st.update(call, &ctx, row);
                }
            } else {
                let projected: Vec<Val> =
                    q.select.iter().map(|s| eval(&s.expr, &ctx, row, None)).collect();
                let order: Vec<Val> =
                    q.order_by.iter().map(|(e, _)| eval(e, &ctx, row, None)).collect();
                out_rows.push((projected, order));
            }
        }
    }

    let mut rows: Vec<(Vec<Val>, Vec<Val>)> = if q.is_aggregate {
        // no rows + no GROUP BY still yields one output row (SQL: sum over empty = null, count = 0)
        if q.group_by.is_empty() && group_order.is_empty() {
            group_order.push(Vec::new());
            groups.insert(Vec::new(), agg_calls.iter().map(AggState::new).collect());
        }
        group_order
            .into_iter()
            .map(|key| {
                let states = &groups[&key];
                let overrides = Overrides { group_by: &q.group_by, key: &key, aggs: &agg_calls, states };
                let projected: Vec<Val> =
                    q.select.iter().map(|s| eval_grouped(&s.expr, &overrides)).collect();
                let order: Vec<Val> =
                    q.order_by.iter().map(|(e, _)| eval_grouped(e, &overrides)).collect();
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
    let rows: Vec<Vec<Val>> =
        rows.into_iter().skip(offset).take(limit).map(|(p, _)| p).collect();

    Ok(QueryResult { columns, rows })
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

// ---------------- aggregation ----------------

enum AggState {
    Count(i64),
    CountDistinct(std::collections::HashSet<Val>),
    Sum { int: i64, float: f64, any: bool, is_float: bool },
    Avg { sum: f64, n: i64 },
    MinMax { best: Val, is_min: bool },
}

impl AggState {
    fn new(call: &Bound) -> AggState {
        let Bound::Call { func, .. } = call else { unreachable!() };
        match func.name {
            "count" => AggState::Count(0),
            "count_distinct" => AggState::CountDistinct(Default::default()),
            "sum" => AggState::Sum { int: 0, float: 0.0, any: false, is_float: false },
            "avg" => AggState::Avg { sum: 0.0, n: 0 },
            "min" => AggState::MinMax { best: Val::Null, is_min: true },
            "max" => AggState::MinMax { best: Val::Null, is_min: false },
            _ => unreachable!("unknown aggregate"),
        }
    }

    fn update(&mut self, call: &Bound, ctx: &GroupCtx, row: usize) {
        let Bound::Call { args, .. } = call else { unreachable!() };
        let v = eval(&args[0], ctx, row, None);
        match self {
            AggState::Count(n) => {
                if !v.is_null() {
                    *n += 1;
                }
            }
            AggState::CountDistinct(set) => {
                if !v.is_null() {
                    set.insert(v);
                }
            }
            AggState::Sum { int, float, any, is_float } => match v {
                Val::Int(i) => {
                    *int += i;
                    *float += i as f64;
                    *any = true;
                }
                Val::Float(f) => {
                    *float += f;
                    *any = true;
                    *is_float = true;
                }
                _ => {}
            },
            AggState::Avg { sum, n } => {
                if let Some(f) = v.as_f64() {
                    *sum += f;
                    *n += 1;
                }
            }
            AggState::MinMax { best, is_min } => {
                if v.is_null() {
                    return;
                }
                let better = if best.is_null() {
                    true
                } else {
                    let ord = v.cmp_sql(best);
                    if *is_min { ord.is_lt() } else { ord.is_gt() }
                };
                if better {
                    *best = v;
                }
            }
        }
    }

    fn finish(&self) -> Val {
        match self {
            AggState::Count(n) => Val::Int(*n),
            AggState::CountDistinct(s) => Val::Int(s.len() as i64),
            AggState::Sum { any: false, .. } => Val::Null,
            AggState::Sum { int, float, is_float, .. } => {
                if *is_float { Val::Float(*float) } else { Val::Int(*int) }
            }
            AggState::Avg { n: 0, .. } => Val::Null,
            AggState::Avg { sum, n } => Val::Float(sum / *n as f64),
            AggState::MinMax { best, .. } => best.clone(),
        }
    }
}

struct Overrides<'a> {
    group_by: &'a [Bound],
    key: &'a [Val],
    aggs: &'a [Bound],
    states: &'a [AggState],
}

/// Evaluate a select/order expression in group context: aggregate calls and
/// group-by expressions resolve to their computed values.
fn eval_grouped(b: &Bound, o: &Overrides<'_>) -> Val {
    if let Some(i) = o.aggs.iter().position(|a| a == b) {
        return o.states[i].finish();
    }
    if let Some(i) = o.group_by.iter().position(|g| g == b) {
        return o.key[i].clone();
    }
    match b {
        Bound::Number(n) => num_val(*n),
        Bound::Str(s) => Val::text(s.clone()),
        Bound::Null => Val::Null,
        Bound::Unary { op, expr, .. } => eval_unary(*op, eval_grouped(expr, o)),
        Bound::Binary { op, lhs, rhs, .. } => {
            eval_binary(*op, eval_grouped(lhs, o), eval_grouped(rhs, o))
        }
        Bound::Call { func, args, .. } => {
            let vals: Vec<Val> = args.iter().map(|a| eval_grouped(a, o)).collect();
            eval_scalar_fn(func.name, vals)
        }
        Bound::Column { .. } => Val::Null, // binder guarantees this can't happen
    }
}

// ---------------- row-wise evaluation ----------------

fn num_val(n: f64) -> Val {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        Val::Int(n as i64)
    } else {
        Val::Float(n)
    }
}

fn eval(b: &Bound, ctx: &GroupCtx, row: usize, _unused: Option<()>) -> Val {
    match b {
        Bound::Number(n) => num_val(*n),
        Bound::Str(s) => Val::text(s.clone()),
        Bound::Null => Val::Null,
        Bound::Column { index, .. } => ctx.value(*index, row),
        Bound::Unary { op, expr, .. } => eval_unary(*op, eval(expr, ctx, row, None)),
        Bound::Binary { op, lhs, rhs, .. } => {
            eval_binary(*op, eval(lhs, ctx, row, None), eval(rhs, ctx, row, None))
        }
        Bound::Call { func, args, .. } => {
            let vals: Vec<Val> = args.iter().map(|a| eval(a, ctx, row, None)).collect();
            eval_scalar_fn(func.name, vals)
        }
    }
}

fn eval_unary(op: UnOp, v: Val) -> Val {
    match (op, v) {
        (_, Val::Null) => Val::Null,
        (UnOp::Neg, Val::Int(i)) => Val::Int(-i),
        (UnOp::Neg, Val::Float(f)) => Val::Float(-f),
        (UnOp::Not, Val::Bool(b)) => Val::Bool(!b),
        _ => Val::Null,
    }
}

fn eval_binary(op: BinOp, l: Val, r: Val) -> Val {
    use BinOp::*;
    match op {
        And => match (as_bool3(&l), as_bool3(&r)) {
            (Some(false), _) | (_, Some(false)) => Val::Bool(false),
            (Some(true), Some(true)) => Val::Bool(true),
            _ => Val::Null,
        },
        Or => match (as_bool3(&l), as_bool3(&r)) {
            (Some(true), _) | (_, Some(true)) => Val::Bool(true),
            (Some(false), Some(false)) => Val::Bool(false),
            _ => Val::Null,
        },
        Add | Sub | Mul | Mod => {
            if l.is_null() || r.is_null() {
                return Val::Null;
            }
            match (&l, &r) {
                (Val::Int(a), Val::Int(b)) => match op {
                    Add => Val::Int(a + b),
                    Sub => Val::Int(a - b),
                    Mul => Val::Int(a * b),
                    _ => {
                        if *b == 0 { Val::Null } else { Val::Int(a % b) }
                    }
                },
                _ => {
                    let (a, b) = (l.as_f64(), r.as_f64());
                    match (a, b) {
                        (Some(a), Some(b)) => match op {
                            Add => Val::Float(a + b),
                            Sub => Val::Float(a - b),
                            Mul => Val::Float(a * b),
                            _ => {
                                if b == 0.0 { Val::Null } else { Val::Float(a % b) }
                            }
                        },
                        _ => Val::Null,
                    }
                }
            }
        }
        Div => match (l.as_f64(), r.as_f64()) {
            (Some(a), Some(b)) if b != 0.0 => Val::Float(a / b),
            _ => Val::Null, // division by zero -> NULL (SQLite semantics)
        },
        Eq | Ne | Lt | Le | Gt | Ge => {
            if l.is_null() || r.is_null() {
                return Val::Null;
            }
            let ord = l.cmp_sql(&r);
            let b = match op {
                Eq => ord.is_eq(),
                Ne => !ord.is_eq(),
                Lt => ord.is_lt(),
                Le => ord.is_le(),
                Gt => ord.is_gt(),
                Ge => ord.is_ge(),
                _ => unreachable!(),
            };
            Val::Bool(b)
        }
    }
}

fn as_bool3(v: &Val) -> Option<bool> {
    match v {
        Val::Bool(b) => Some(*b),
        Val::Null => None,
        _ => None,
    }
}

fn eval_scalar_fn(name: &str, mut args: Vec<Val>) -> Val {
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
                    Some(false) => {}
                }
            }
            if saw_null { Val::Null } else { Val::Bool(false) }
        }
        "between" => {
            let (x, lo, hi) = (args[0].clone(), args[1].clone(), args[2].clone());
            eval_binary(
                BinOp::And,
                eval_binary(BinOp::Ge, x.clone(), lo),
                eval_binary(BinOp::Le, x, hi),
            )
        }
        "like" => match (&args[0], &args[1]) {
            (Val::Text(s), Val::Text(p)) => Val::Bool(like_match(p, s)),
            (Val::Null, _) | (_, Val::Null) => Val::Null,
            _ => Val::Null,
        },
        "if" | "case" => {
            // case(c1, v1, c2, v2, ..., else?)
            let mut i = 0;
            while i + 1 < args.len() {
                if matches!(as_bool3(&args[i]), Some(true)) {
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
            Val::Text(s) => Val::text(if name == "lower" {
                s.to_lowercase()
            } else {
                s.to_uppercase()
            }),
            _ => Val::Null,
        },
        "length" => match &args[0] {
            Val::Text(s) => Val::Int(s.chars().count() as i64),
            _ => Val::Null,
        },
        "substr" => match &args[0] {
            Val::Text(s) => {
                let start = match args.get(1).and_then(Val::as_f64) {
                    Some(f) => (f as i64 - 1).max(0) as usize, // SQL substr is 1-based
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
                        let digits =
                            args.get(1).and_then(Val::as_f64).unwrap_or(0.0) as i32;
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

/// SQL LIKE: % = any run, _ = any one char; ASCII-case-insensitive (SQLite default).
fn like_match(pattern: &str, s: &str) -> bool {
    fn rec(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('%') => (0..=s.len()).any(|k| rec(&p[1..], &s[k..])),
            Some('_') => !s.is_empty() && rec(&p[1..], &s[1..]),
            Some(c) => {
                !s.is_empty() && s[0].eq_ignore_ascii_case(c) && rec(&p[1..], &s[1..])
            }
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let sc: Vec<char> = s.chars().collect();
    rec(&p, &sc)
}
