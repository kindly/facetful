//! A query's result as a new immutable table — the derived-table primitive
//! behind `db.materialize`, and the function the CTE and JOIN caches call.
//!
//! The result's typed columns feed the same compiler Parquet ingest uses:
//! text is dictionary-encoded when it pays, ints narrow to their range,
//! every segment gets min/max stats, and a materialized `ORDER BY` is
//! recorded as `sorted_by`. Rows come out in the query's output order, so a
//! joined or filtered table keeps its source's clustering.

use crate::format::compile::{compile_sorted, InCol};
use crate::format::read::ReadAt;
use crate::format::SortKey;
use crate::sql::binder::{BoundQuery, Ty};
use crate::sql::exec::{OutCol, QueryResult, Val};
use crate::sql::{execute_sql_with, span::Span, Catalog, Diagnostic, NoCatalog};
use crate::Table;

/// Run `sql` against `table` (CTEs and subqueries included) and compile the
/// result into a `.facetful` image. Every SELECT item needs a distinct name
/// (alias it if not).
pub fn materialize<S: ReadAt>(
    table: &mut Table<S>,
    sql: &str,
    group_target: u32,
) -> Result<Vec<u8>, Diagnostic> {
    materialize_with(table, sql, group_target, &mut NoCatalog)
}

/// `materialize` with other tables in scope for JOIN / FROM.
pub fn materialize_with<S: ReadAt>(
    table: &mut Table<S>,
    sql: &str,
    group_target: u32,
    cat: &mut dyn Catalog<S>,
) -> Result<Vec<u8>, Diagnostic> {
    let (bound, result) = execute_sql_with(table, sql, cat)?;
    compile_result(&bound, result, group_target)
}

/// A finished query's result as an image: the columns typed from the bound
/// query, nulls kept, the leading ORDER BY keys that are SELECT items
/// recorded as the table's sort.
pub fn compile_result(bound: &BoundQuery, mut r: QueryResult, group_target: u32) -> Result<Vec<u8>, Diagnostic> {
    let err = |m: String| Diagnostic::new(m, Span::new(0, 0));
    let names: Vec<String> = bound.select.iter().map(|s| s.name.clone()).collect();
    for (i, n) in names.iter().enumerate() {
        if names[..i].contains(n) {
            return Err(err(format!("materialize: duplicate column name '{n}' — alias it")));
        }
    }
    let tys: Vec<Ty> = bound.select.iter().map(|s| s.expr.ty()).collect();
    let sorted_by = sorted_by_of(bound);
    let n = r.n_rows();
    let cols: Vec<InCol> = match r.cols.take() {
        Some(out) => out.iter().zip(&tys).map(|(c, ty)| from_outcol(c, *ty, n)).collect(),
        None => (0..names.len()).map(|i| from_rows(&r, i, tys[i])).collect(),
    };
    compile_sorted(&names, cols, group_target, sorted_by)
        .map(|(image, _)| image)
        .map_err(|e| err(format!("materialize: {e}")))
}

/// The leading ORDER BY keys that are SELECT items, as the new table's sort
/// metadata; the first key that isn't a selected column ends the prefix.
fn sorted_by_of(q: &BoundQuery) -> Vec<SortKey> {
    let mut keys = Vec::new();
    for (expr, dir) in &q.order_by {
        let Some(i) = q.select.iter().position(|s| &s.expr == expr) else { break };
        keys.push(SortKey {
            column: i as u16,
            descending: *dir == crate::sql::ast::SortDir::Desc,
        });
    }
    keys
}

fn valid_of(bits: &[u8], n: usize) -> Option<Vec<bool>> {
    let v: Vec<bool> = (0..n).map(|i| bits[i / 8] >> (i % 8) & 1 != 0).collect();
    if v.iter().all(|&b| b) { None } else { Some(v) }
}

/// One result column, columnar channel, into the compiler's input shape.
fn from_outcol(c: &OutCol, ty: Ty, n: usize) -> InCol {
    match (c, ty) {
        (OutCol::I64 { v, valid }, Ty::Date) => {
            InCol::Date { v: v.iter().map(|&x| x as i32).collect(), valid: valid_of(valid, n) }
        }
        (OutCol::I64 { v, valid }, Ty::Timestamp) => {
            InCol::Timestamp { v: v.clone(), valid: valid_of(valid, n) }
        }
        (OutCol::I64 { v, valid }, Ty::Float) => {
            InCol::Float { v: v.iter().map(|&x| x as f64).collect(), valid: valid_of(valid, n) }
        }
        (OutCol::I64 { v, valid }, _) => InCol::Int { v: v.clone(), valid: valid_of(valid, n) },
        (OutCol::F64 { v, valid }, Ty::Int | Ty::Date | Ty::Timestamp) => {
            InCol::Int { v: v.iter().map(|&x| x as i64).collect(), valid: valid_of(valid, n) }
        }
        (OutCol::F64 { v, valid }, _) => InCol::Float { v: v.clone(), valid: valid_of(valid, n) },
        // the format stores booleans; the compiler's input has no bool lane,
        // so they land as 0/1 ints
        (OutCol::Bool { v, valid }, _) => {
            InCol::Int { v: v.iter().map(|&b| b as i64).collect(), valid: valid_of(valid, n) }
        }
        (OutCol::Text { offsets, bytes, valid }, _) => {
            let v = (0..n)
                .map(|i| {
                    String::from_utf8_lossy(&bytes[offsets[i] as usize..offsets[i + 1] as usize])
                        .into_owned()
                })
                .collect();
            InCol::Text { v, valid: valid_of(valid, n) }
        }
    }
}

/// One result column from the row channel (the top-k path still returns
/// rows) into the compiler's input shape.
fn from_rows(r: &QueryResult, col: usize, ty: Ty) -> InCol {
    let n = r.rows.len();
    let at = |i: usize| &r.rows[i][col];
    let valid: Option<Vec<bool>> = {
        let v: Vec<bool> = (0..n).map(|i| !matches!(at(i), Val::Null)).collect();
        if v.iter().all(|&b| b) { None } else { Some(v) }
    };
    let ints = |f: &dyn Fn(&Val) -> i64| -> Vec<i64> { (0..n).map(|i| f(at(i))).collect() };
    let as_i64 = |v: &Val| match v {
        Val::Int(x) => *x,
        Val::Float(x) => *x as i64,
        Val::Bool(b) => *b as i64,
        _ => 0,
    };
    match ty {
        Ty::Date => InCol::Date { v: ints(&as_i64).into_iter().map(|x| x as i32).collect(), valid },
        Ty::Timestamp => InCol::Timestamp { v: ints(&as_i64), valid },
        Ty::Float => InCol::Float { v: (0..n).map(|i| at(i).as_f64().unwrap_or(0.0)).collect(), valid },
        Ty::Text => InCol::Text {
            v: (0..n).map(|i| match at(i) { Val::Text(s) => s.to_string(), _ => String::new() }).collect(),
            valid,
        },
        _ => InCol::Int { v: ints(&as_i64), valid },
    }
}
