//! The SQL layer: lexer -> parser -> (binder -> planner, arriving next).
//! Design decisions and error-message philosophy: docs/sql-parser.sv.

pub mod ast;
pub mod binder;
pub mod exec;
pub mod lexer;
pub mod parser;
pub mod span;

pub use parser::{parse_expr, parse_query};

use crate::format::read::ReadAt;
use crate::Table;

/// Other tables a query may name: in the browser, the worker's registry of
/// loaded tables; in the CLI and tests, a `TableSet`. The query's own table
/// is not looked up here.
pub trait Catalog<S: ReadAt> {
    /// A table by name, with an identity stable for its lifetime (part of
    /// derived-table cache keys, so a table reloaded under the same name
    /// does not serve a stale join).
    fn table(&mut self, name: &str) -> Option<(&mut Table<S>, u64)>;
    /// Whether `name` is the query's own table — a join to it would alias.
    fn is_self(&self, _name: &str) -> bool {
        false
    }
}

/// No other tables.
pub struct NoCatalog;

impl<S: ReadAt> Catalog<S> for NoCatalog {
    fn table(&mut self, _name: &str) -> Option<(&mut Table<S>, u64)> {
        None
    }
}

/// Named tables held in memory: the CLI's `--table name=path` and the tests.
pub struct TableSet<S: ReadAt> {
    pub tables: Vec<(String, Table<S>)>,
}

impl<S: ReadAt> Catalog<S> for TableSet<S> {
    fn table(&mut self, name: &str) -> Option<(&mut Table<S>, u64)> {
        self.tables
            .iter_mut()
            .enumerate()
            .find(|(_, (n, _))| n == name)
            .map(|(i, (_, t))| (t, i as u64 + 1))
    }
}

/// Parse, bind and execute one SQL query against a table.
pub fn run_query<S: ReadAt>(
    table: &mut Table<S>,
    src: &str,
) -> Result<exec::QueryResult, Diagnostic> {
    execute_sql_with(table, src, &mut NoCatalog).map(|(_, r)| r)
}

/// `run_query` with other tables in scope for `JOIN` and `FROM`.
pub fn run_query_with<S: ReadAt>(
    table: &mut Table<S>,
    src: &str,
    cat: &mut dyn Catalog<S>,
) -> Result<exec::QueryResult, Diagnostic> {
    execute_sql_with(table, src, cat).map(|(_, r)| r)
}

/// `run_query`, also returning the final query as bound — its column names,
/// types and ORDER BY are what `materialize` records about the result.
pub fn execute_sql<S: ReadAt>(
    table: &mut Table<S>,
    src: &str,
) -> Result<(binder::BoundQuery, exec::QueryResult), Diagnostic> {
    execute_sql_with(table, src, &mut NoCatalog)
}

pub fn execute_sql_with<S: ReadAt>(
    table: &mut Table<S>,
    src: &str,
    cat: &mut dyn Catalog<S>,
) -> Result<(binder::BoundQuery, exec::QueryResult), Diagnostic> {
    let q = parse_query(src)?;
    exec_query(table, &q, src, &[], cat)
}

/// Names in scope: each CTE and the cache key of its derived table.
type Scope = [(String, String)];

/// Where a query (or a join side) reads from.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    /// the query's own table
    Base,
    /// a derived table in the base table's cache
    Derived(String),
    /// another loaded table, by name
    Catalog(String),
}

/// A target's identity for cache keys.
fn target_key<S: ReadAt>(t: &Target, cat: &mut dyn Catalog<S>) -> String {
    match t {
        Target::Base => String::new(),
        Target::Derived(k) => k.clone(),
        Target::Catalog(n) => {
            let id = cat.table(n).map(|(_, id)| id).unwrap_or(0);
            format!("cat:{n}:{id}")
        }
    }
}

/// Resolve `WITH`, `FROM (subquery)` and `JOIN`s by materializing each into
/// the table's derived cache — the optimization fence, and the only mode —
/// then bind and run the query itself against whichever table it ends up
/// reading: a CTE in scope, a catalog table, a joined table, else this one.
fn exec_query<S: ReadAt>(
    table: &mut Table<S>,
    q: &ast::Query,
    src: &str,
    scope: &Scope,
    cat: &mut dyn Catalog<S>,
) -> Result<(binder::BoundQuery, exec::QueryResult), Diagnostic> {
    let mut scope: Vec<(String, String)> = scope.to_vec();
    for cte in &q.with {
        let key = derive(table, &cte.query, cte.body_span, src, &scope, cat)?;
        scope.push((cte.name.clone(), key));
    }
    let mut target = match &q.from_subquery {
        Some(sub) => Target::Derived(derive(table, sub, q.from_span, src, &scope, cat)?),
        None => resolve_name(&q.from, &scope, cat),
    };
    // `IN (select …)` in WHERE becomes a synthetic LEFT JOIN + IS [NOT] NULL
    let expanded = expand_in_subqueries(table, q, src, &mut scope, cat)?;
    let q = expanded.as_ref().unwrap_or(q);
    let rewritten;
    let q = if q.joins.is_empty() {
        q
    } else {
        let (t, rq) = resolve_joins(table, q, src, &scope, cat, target)?;
        target = t;
        rewritten = rq;
        &rewritten
    };
    let exec_err = |e: crate::format::FormatError| {
        Diagnostic::new(format!("execution error: {e}"), span::Span::new(0, 0))
    };
    let run = |t: &mut Table<S>| -> Result<(binder::BoundQuery, exec::QueryResult), Diagnostic> {
        let schema = t.catalog().schema.clone();
        let bound = binder::Binder::new(&schema).bind_query(q)?;
        let r = exec::execute(t, &bound).map_err(exec_err)?;
        Ok((bound, r))
    };
    match target {
        Target::Base => run(table),
        Target::Derived(key) => {
            run(table.derived_table(&key).expect("a derived table was just materialized or found"))
        }
        Target::Catalog(name) => run(cat.table(&name).expect("resolved a moment ago").0),
    }
}

/// A FROM / JOIN name: a CTE in scope first, then another loaded table, else
/// the query's own table (whatever it is called).
fn resolve_name<S: ReadAt>(name: &str, scope: &Scope, cat: &mut dyn Catalog<S>) -> Target {
    if let Some((_, k)) = scope.iter().rev().find(|(n, _)| n == name) {
        return Target::Derived(k.clone());
    }
    if !cat.is_self(name) && cat.table(name).is_some() {
        return Target::Catalog(name.to_string());
    }
    Target::Base
}

fn schema_names<S: ReadAt>(
    table: &mut Table<S>,
    t: &Target,
    cat: &mut dyn Catalog<S>,
) -> Vec<String> {
    let names = |t: &Table<S>| t.catalog().schema.columns.iter().map(|c| c.name.clone()).collect();
    match t {
        Target::Base => names(table),
        Target::Derived(k) => table.derived_table(k).map(|t| names(t)).unwrap_or_default(),
        Target::Catalog(n) => cat.table(n).map(|(t, _)| names(t)).unwrap_or_default(),
    }
}

/// Apply the joins left to right, each a cached materialization onto the
/// running table, carrying only the right-side columns the query touches;
/// then rewrite the query onto the joined table: qualified names resolve
/// through the aliases, right columns that clashed take `alias_column`.
fn resolve_joins<S: ReadAt>(
    table: &mut Table<S>,
    q: &ast::Query,
    src: &str,
    scope: &Scope,
    cat: &mut dyn Catalog<S>,
    from: Target,
) -> Result<(Target, ast::Query), Diagnostic> {
    // alias → (original column, name in the running joined table)
    let mut aliases: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut cur_names = schema_names(table, &from, cat);
    let base_alias = q.from_alias.clone().unwrap_or_else(|| q.from.clone());
    aliases.push((base_alias, cur_names.iter().map(|n| (n.clone(), n.clone())).collect()));
    let mut left = from;

    // every column the query itself references (not the ON clauses); a
    // top-level `select *` carries everything — `count(*)` does not
    let mut referenced: Vec<String> = Vec::new();
    let star = q.select.iter().any(|s| matches!(s.expr, ast::Expr::Star(_)));
    for e in q.select.iter().map(|s| &s.expr).chain(q.filter.iter()).chain(q.group_by.iter()).chain(q.order_by.iter().map(|o| &o.expr)) {
        collect_names(e, &mut referenced);
    }

    for j in &q.joins {
        let right = match &j.source {
            ast::JoinSource::Subquery(sub, span) => Target::Derived(derive(table, sub, *span, src, scope, cat)?),
            ast::JoinSource::Table(name) => {
                if cat.is_self(name) && !scope.iter().any(|(n, _)| n == name) {
                    return Err(Diagnostic::new(
                        format!("cannot join '{name}' to itself yet"),
                        j.span,
                    ));
                }
                match resolve_name(name, scope, cat) {
                    Target::Base => {
                        return Err(Diagnostic::new(format!("unknown table '{name}'"), j.span)
                            .with_hint("joinable tables are CTEs of this query and tables loaded under a name"))
                    }
                    t => t,
                }
            }
        };
        let right_alias = j.alias.clone().unwrap_or_else(|| match &j.source {
            ast::JoinSource::Table(n) => n.clone(),
            ast::JoinSource::Subquery(..) => "subquery".to_string(),
        });
        let right_names = schema_names(table, &right, cat);

        // key pairs: each side a plain or qualified column; either order
        let mut keys: Vec<(String, String)> = Vec::new();
        for (a, b) in &j.on {
            let (an, asp) = column_name(a)?;
            let (bn, _) = column_name(b)?;
            let left_of = |n: &str| -> Option<String> { lookup(&aliases, &cur_names, n) };
            let right_of = |n: &str| -> Option<String> {
                let (alias, col) = split_qualified(n);
                match alias {
                    Some(al) if al != right_alias => None,
                    _ => right_names.iter().find(|c| *c == col).cloned(),
                }
            };
            let pair = match (left_of(&an), right_of(&bn)) {
                (Some(l), Some(r)) => (l, r),
                _ => match (left_of(&bn), right_of(&an)) {
                    (Some(l), Some(r)) => (l, r),
                    _ => {
                        return Err(Diagnostic::new(
                            format!("join condition '{an} = {bn}' must compare a column of the left side with a column of '{right_alias}'"),
                            asp,
                        ))
                    }
                },
            };
            keys.push(pair);
        }

        // right columns the query touches: `alias.col`, or a bare name the
        // left side doesn't have; `*` carries every non-key right column
        let mut carried: Vec<String> = Vec::new();
        for n in &referenced {
            let (alias, col) = split_qualified(n);
            let take = match alias {
                Some(al) => al == right_alias && right_names.iter().any(|c| c == col),
                None => !cur_names.contains(&col.to_string()) && right_names.iter().any(|c| c == col),
            };
            if take && !carried.iter().any(|c| c == col) {
                carried.push(col.to_string());
            }
        }
        if star {
            for c in &right_names {
                if !keys.iter().any(|(_, r)| r == c) && !carried.contains(c) {
                    carried.push(c.clone());
                }
            }
        }
        carried.sort();
        // left columns the query touches, plus this join's left keys; `*` = all
        let left_carried: Option<Vec<String>> = if star {
            None
        } else {
            let mut v: Vec<String> = keys.iter().map(|(l, _)| l.clone()).collect();
            for n in &referenced {
                if let Some(j) = lookup(&aliases, &cur_names, n) {
                    if !v.contains(&j) {
                        v.push(j);
                    }
                }
            }
            v.sort();
            Some(v)
        };
        let renames: Vec<(String, String)> = carried
            .iter()
            .filter(|c| cur_names.contains(*c))
            .map(|c| (c.clone(), format!("{right_alias}_{c}")))
            .collect();
        let kind = match j.kind {
            ast::JoinKind::Left => crate::join::JoinKind::Left,
            ast::JoinKind::Inner => crate::join::JoinKind::Inner,
        };
        let spec = crate::join::JoinSpec {
            keys: keys.clone(),
            left_columns: left_carried.clone(),
            columns: Some(carried.clone()),
            renames: renames.clone(),
            kind,
            matched: false,
        };
        let key = format!(
            "join\u{1}{}\u{1}{}\u{1}{:?}\u{1}{}\u{1}{}\u{1}{}",
            target_key(&left, cat),
            target_key(&right, cat),
            kind,
            keys.iter().map(|(l, r)| format!("{l}={r}")).collect::<Vec<_>>().join(","),
            left_carried.as_ref().map(|v| v.join(",")).unwrap_or_else(|| "*".into()),
            carried.join(",")
        );
        if table.derived_get(&key).is_none() {
            let image = join_targets(table, &left, &right, &spec, cat)
                .map_err(|e| Diagnostic::new(e, j.span))?;
            table.derived_insert(&key, image).map_err(|e| Diagnostic::new(format!("derived table: {e}"), j.span))?;
        }
        let joined: Vec<(String, String)> = carried
            .iter()
            .map(|c| (c.clone(), renames.iter().find(|(o, _)| o == c).map(|(_, r)| r.clone()).unwrap_or_else(|| c.clone())))
            .collect();
        // the running table now holds only the carried left columns
        if let Some(lc) = &left_carried {
            cur_names.retain(|n| lc.contains(n));
            for (_, cols) in aliases.iter_mut() {
                cols.retain(|(_, j)| lc.contains(j));
            }
        }
        cur_names.extend(joined.iter().map(|(_, n)| n.clone()));
        aliases.push((right_alias, joined));
        left = Target::Derived(key);
    }

    // the query, onto the joined table
    let mut rq = q.clone();
    rq.with.clear();
    rq.joins.clear();
    rq.from_subquery = None;
    rq.from_alias = None;
    let fix = |e: &mut ast::Expr| -> Result<(), Diagnostic> { rewrite_names(e, &aliases, &cur_names) };
    for s in &mut rq.select {
        fix(&mut s.expr)?;
    }
    if let Some(f) = &mut rq.filter {
        fix(f)?;
    }
    for g in &mut rq.group_by {
        fix(g)?;
    }
    for o in &mut rq.order_by {
        fix(&mut o.expr)?;
    }
    Ok((left, rq))
}

/// Run the join with both sides borrowed at once: a derived side is taken
/// out of the cache for the duration.
fn join_targets<S: ReadAt>(
    table: &mut Table<S>,
    left: &Target,
    right: &Target,
    spec: &crate::join::JoinSpec,
    cat: &mut dyn Catalog<S>,
) -> Result<Vec<u8>, String> {
    let gt = 65_536;
    match (left, right) {
        (Target::Base, Target::Catalog(n)) => {
            let (r, _) = cat.table(n).ok_or_else(|| format!("unknown table '{n}'"))?;
            crate::join::join(table, r, spec, gt)
        }
        (Target::Base, Target::Derived(rk)) => {
            let (mut r, bytes) = table.derived_take(rk).ok_or("derived table vanished")?;
            let out = crate::join::join(table, &mut r, spec, gt);
            table.derived_put(rk, r, bytes);
            out
        }
        (Target::Derived(lk), Target::Catalog(n)) => {
            let (mut l, bytes) = table.derived_take(lk).ok_or("derived table vanished")?;
            let out = match cat.table(n) {
                Some((r, _)) => crate::join::join(&mut l, r, spec, gt),
                None => Err(format!("unknown table '{n}'")),
            };
            table.derived_put(lk, l, bytes);
            out
        }
        (Target::Derived(lk), Target::Derived(rk)) => {
            let (mut l, lb) = table.derived_take(lk).ok_or("derived table vanished")?;
            let out = match table.derived_take(rk) {
                Some((mut r, rb)) => {
                    let out = crate::join::join(&mut l, &mut r, spec, gt);
                    table.derived_put(rk, r, rb);
                    out
                }
                None => Err("derived table vanished".to_string()),
            };
            table.derived_put(lk, l, lb);
            out
        }
        (Target::Derived(lk), Target::Base) => {
            let (mut l, bytes) = table.derived_take(lk).ok_or("derived table vanished")?;
            let out = crate::join::join(&mut l, table, spec, gt);
            table.derived_put(lk, l, bytes);
            out
        }
        (Target::Catalog(_), _) | (Target::Base, Target::Base) => Err(
            "joins start from this query's own table or a CTE; to join from another loaded table, run the query against it"
                .to_string(),
        ),
    }
}

/// `(a, b) IN (select x, y …)` (and `NOT IN`) in WHERE, as a semi-join with
/// exact three-valued semantics and no new executor code: the inner query
/// materializes, its distinct key set materializes over that, a synthetic
/// LEFT JOIN on (a = x, b = y) carries the right key, and the predicate is
/// `key IS NOT NULL` — for NOT IN, `key IS NULL AND a IS NOT NULL AND …`,
/// or constant FALSE when the inner set holds a NULL (SQL's rule).
fn expand_in_subqueries<S: ReadAt>(
    table: &mut Table<S>,
    q: &ast::Query,
    src: &str,
    scope: &mut Vec<(String, String)>,
    cat: &mut dyn Catalog<S>,
) -> Result<Option<ast::Query>, Diagnostic> {
    fn has_in(e: &ast::Expr) -> bool {
        match e {
            ast::Expr::InSubquery { .. } => true,
            ast::Expr::Call { args, .. } => args.iter().any(has_in),
            ast::Expr::Unary { expr, .. } => has_in(expr),
            ast::Expr::Binary { lhs, rhs, .. } => has_in(lhs) || has_in(rhs),
            _ => false,
        }
    }
    let Some(filter) = &q.filter else { return Ok(None) };
    if !has_in(filter) {
        return Ok(None);
    }
    let mut rq = q.clone();
    let mut n = 0usize;
    let mut filter = rq.filter.take().expect("checked above");
    expand_in_expr(table, &mut filter, src, scope, cat, &mut rq.joins, &mut n, false)?;
    rq.filter = Some(filter);
    Ok(Some(rq))
}

fn expand_in_expr<S: ReadAt>(
    table: &mut Table<S>,
    e: &mut ast::Expr,
    src: &str,
    scope: &mut Vec<(String, String)>,
    cat: &mut dyn Catalog<S>,
    joins: &mut Vec<ast::Join>,
    n: &mut usize,
    negated: bool,
) -> Result<(), Diagnostic> {
    use ast::{BinOp, Expr, UnOp};
    // NOT (x IN (select …)) is the negated form; other NOTs recurse
    if let Expr::Unary { op: UnOp::Not, expr, .. } = e {
        if matches!(**expr, Expr::InSubquery { .. }) {
            let mut inner = std::mem::replace(&mut **expr, Expr::Null(span::Span::new(0, 0)));
            expand_in_expr(table, &mut inner, src, scope, cat, joins, n, !negated)?;
            *e = inner;
            return Ok(());
        }
    }
    match e {
        Expr::InSubquery { cols, query, body, span } => {
            let span = *span;
            *n += 1;
            let alias = format!("__in{}", *n);
            // 1. the inner query, materialized
            let inner_key = derive(table, query, *body, src, scope, cat)?;
            let names = schema_names(table, &Target::Derived(inner_key.clone()), cat);
            if names.len() != cols.len() {
                return Err(Diagnostic::new(
                    format!("IN compares {} column(s) but the subquery selects {}", cols.len(), names.len()),
                    span,
                ));
            }
            for c in cols.iter() {
                if !matches!(c, Expr::Column(..)) {
                    return Err(Diagnostic::new("IN (select …) compares plain columns", c.span()));
                }
            }
            // 2. its distinct key set, materialized over it
            let quoted: Vec<String> = names.iter().map(|c| format!("\"{}\"", c.replace('"', "\"\""))).collect();
            let distinct_sql = format!(
                "select {} from __in_src group by {}",
                quoted.join(", "),
                quoted.join(", ")
            );
            scope.push(("__in_src".to_string(), inner_key.clone()));
            let distinct_q = parse_query(&distinct_sql)?;
            let distinct_key = derive(table, &distinct_q, span::Span::new(0, distinct_sql.len()), &distinct_sql, scope, cat)?;
            // 3. does the key set hold a NULL? (decides NOT IN)
            let null_sql = format!(
                "select count(*) from __in_src where {}",
                quoted.iter().map(|c| format!("{c} is null")).collect::<Vec<_>>().join(" or ")
            );
            let null_q = parse_query(&null_sql)?;
            let (_, mut r) = exec_query(table, &null_q, &null_sql, scope, cat)?;
            r.ensure_rows();
            let inner_has_null = matches!(r.rows.first().and_then(|row| row.first()), Some(exec::Val::Int(c)) if *c > 0);
            scope.pop();
            // 4. the synthetic join and the predicate
            let derived_name = format!("{alias}_d");
            scope.push((derived_name.clone(), distinct_key));
            let on: Vec<(Expr, Expr)> = cols
                .iter()
                .zip(&names)
                .map(|(c, rn)| (c.clone(), Expr::Column(format!("{alias}.{rn}"), span)))
                .collect();
            joins.push(ast::Join {
                kind: ast::JoinKind::Left,
                source: ast::JoinSource::Table(derived_name),
                alias: Some(alias.clone()),
                on,
                span,
            });
            let key_ref = Expr::Column(format!("{alias}.{}", names[0]), span);
            let is_null = |x: Expr| Expr::call("isnull", vec![x], span);
            let not = |x: Expr| Expr::Unary { op: UnOp::Not, expr: Box::new(x), span };
            let and = |a: Expr, b: Expr| Expr::Binary { op: BinOp::And, lhs: Box::new(a), rhs: Box::new(b), span };
            *e = if !negated {
                // matched → TRUE; unmatched or a NULL outer key → not TRUE
                not(is_null(key_ref))
            } else if inner_has_null {
                // NOT IN against a set with NULL is never TRUE
                Expr::Binary { op: BinOp::Eq, lhs: Box::new(Expr::Number(1.0, false, span)), rhs: Box::new(Expr::Number(0.0, false, span)), span }
            } else {
                // unmatched, and every outer key non-NULL
                let mut pred = is_null(key_ref);
                for c in cols.iter() {
                    pred = and(pred, not(is_null(c.clone())));
                }
                pred
            };
            Ok(())
        }
        Expr::Call { args, .. } => {
            for a in args.iter_mut() {
                expand_in_expr(table, a, src, scope, cat, joins, n, negated)?;
            }
            Ok(())
        }
        Expr::Unary { expr, .. } => expand_in_expr(table, expr, src, scope, cat, joins, n, negated),
        Expr::Binary { lhs, rhs, .. } => {
            expand_in_expr(table, lhs, src, scope, cat, joins, n, negated)?;
            expand_in_expr(table, rhs, src, scope, cat, joins, n, negated)
        }
        _ => Ok(()),
    }
}

fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.split_once('.') {
        Some((a, c)) => (Some(a), c),
        None => (None, name),
    }
}

/// A column reference's name and span, or an error for anything else.
fn column_name(e: &ast::Expr) -> Result<(String, span::Span), Diagnostic> {
    match e {
        ast::Expr::Column(n, sp) => Ok((n.clone(), *sp)),
        other => Err(Diagnostic::new("join keys must be plain column names", other.span())),
    }
}

/// Resolve a plain or qualified name against the running joined table.
fn lookup(aliases: &[(String, Vec<(String, String)>)], cur_names: &[String], name: &str) -> Option<String> {
    let (alias, col) = split_qualified(name);
    match alias {
        Some(al) => aliases
            .iter()
            .find(|(a, _)| a == al)
            .and_then(|(_, cols)| cols.iter().find(|(o, _)| o == col).map(|(_, j)| j.clone())),
        None => {
            if cur_names.iter().any(|c| c == col) {
                Some(col.to_string())
            } else {
                // a right column renamed for a clash is still reachable bare if unique
                let mut hits = aliases.iter().flat_map(|(_, cols)| cols.iter().filter(|(o, _)| o == col));
                let first = hits.next().map(|(_, j)| j.clone());
                if hits.next().is_some() { None } else { first }
            }
        }
    }
}

fn collect_names(e: &ast::Expr, out: &mut Vec<String>) {
    match e {
        ast::Expr::Column(n, _) => out.push(n.clone()),
        ast::Expr::Row(items, _) | ast::Expr::InSubquery { cols: items, .. } => {
            items.iter().for_each(|a| collect_names(a, out))
        }
        ast::Expr::Call { args, .. } => args.iter().for_each(|a| collect_names(a, out)),
        ast::Expr::Unary { expr, .. } => collect_names(expr, out),
        ast::Expr::Binary { lhs, rhs, .. } => {
            collect_names(lhs, out);
            collect_names(rhs, out);
        }
        _ => {}
    }
}

fn rewrite_names(
    e: &mut ast::Expr,
    aliases: &[(String, Vec<(String, String)>)],
    cur_names: &[String],
) -> Result<(), Diagnostic> {
    match e {
        ast::Expr::Column(n, sp) => {
            let (alias, _) = split_qualified(n);
            match lookup(aliases, cur_names, n) {
                Some(j) => *n = j,
                None if alias.is_some() => {
                    return Err(Diagnostic::new(format!("unknown column '{n}'"), *sp)
                        .with_hint("qualified names use the FROM/JOIN aliases"))
                }
                None => {} // let the binder report it against the joined table
            }
            Ok(())
        }
        ast::Expr::Call { args, .. } => args.iter_mut().try_for_each(|a| rewrite_names(a, aliases, cur_names)),
        ast::Expr::Row(items, _) | ast::Expr::InSubquery { cols: items, .. } => {
            items.iter_mut().try_for_each(|a| rewrite_names(a, aliases, cur_names))
        }
        ast::Expr::Unary { expr, .. } => rewrite_names(expr, aliases, cur_names),
        ast::Expr::Binary { lhs, rhs, .. } => {
            rewrite_names(lhs, aliases, cur_names)?;
            rewrite_names(rhs, aliases, cur_names)
        }
        _ => Ok(()),
    }
}

/// Materialize `q` (a CTE body or FROM subquery) into the derived cache
/// unless an identical one is already there; returns its cache key. The key
/// is the body's token stream plus the key of the table it reads from, so
/// the same body over a different CTE is a different table.
fn derive<S: ReadAt>(
    table: &mut Table<S>,
    q: &ast::Query,
    body: span::Span,
    src: &str,
    scope: &Scope,
    cat: &mut dyn Catalog<S>,
) -> Result<String, Diagnostic> {
    let parent = match &q.from_subquery {
        Some(_) => String::new(), // its own key covers the nested body text
        None => target_key(&resolve_name(&q.from, scope, cat), cat),
    };
    // canonical form = the token stream re-spelled from its spans: whitespace
    // vanishes, keywords and bare identifiers lowercase, string literals and
    // quoted identifiers keep their case. (Not `{:?}`: Debug for the token
    // enum is kilobytes of wasm.)
    let body_text = &src[body.start..body.end];
    let text: String = match lexer::lex(body_text) {
        Ok(toks) => toks
            .iter()
            .map(|t| {
                let s = &body_text[t.span.start..t.span.end];
                match t.tok {
                    lexer::Tok::Str(_) | lexer::Tok::QuotedIdent(_) => s.to_string(),
                    _ => s.to_lowercase(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        Err(_) => body_text.to_string(),
    };
    let key = format!("{parent}\u{1}{text}");
    if table.derived_get(&key).is_some() {
        return Ok(key);
    }
    let (bound, result) = exec_query(table, q, src, scope, cat)?;
    let image = crate::materialize::compile_result(&bound, result, 65_536)?;
    table
        .derived_insert(&key, image)
        .map_err(|e| Diagnostic::new(format!("derived table: {e}"), body))?;
    Ok(key)
}

pub use span::Diagnostic;
