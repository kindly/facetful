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

/// Parse, bind and execute one SQL query against a table.
pub fn run_query<S: ReadAt>(
    table: &mut Table<S>,
    src: &str,
) -> Result<exec::QueryResult, Diagnostic> {
    execute_sql(table, src).map(|(_, r)| r)
}

/// `run_query`, also returning the final query as bound — its column names,
/// types and ORDER BY are what `materialize` records about the result.
pub fn execute_sql<S: ReadAt>(
    table: &mut Table<S>,
    src: &str,
) -> Result<(binder::BoundQuery, exec::QueryResult), Diagnostic> {
    let q = parse_query(src)?;
    exec_query(table, &q, src, &[])
}

/// Names in scope: each CTE and the cache key of its derived table.
type Scope = [(String, String)];

/// Resolve `WITH` and `FROM (subquery)` by materializing each into the
/// table's derived cache — the optimization fence, and the only mode — then
/// bind and run the query itself against whichever table its FROM names:
/// a CTE in scope, else this table. Every CTE body is a full query, so they
/// nest and chain; a body sees the CTEs declared before it.
fn exec_query<S: ReadAt>(
    table: &mut Table<S>,
    q: &ast::Query,
    src: &str,
    scope: &Scope,
) -> Result<(binder::BoundQuery, exec::QueryResult), Diagnostic> {
    let mut scope: Vec<(String, String)> = scope.to_vec();
    for cte in &q.with {
        let key = derive(table, &cte.query, cte.body_span, src, &scope)?;
        scope.push((cte.name.clone(), key));
    }
    let target: Option<String> = match &q.from_subquery {
        Some(sub) => Some(derive(table, sub, q.from_span, src, &scope)?),
        None => scope.iter().rev().find(|(n, _)| *n == q.from).map(|(_, k)| k.clone()),
    };
    let exec_err = |e: crate::format::FormatError| {
        Diagnostic::new(format!("execution error: {e}"), span::Span::new(0, 0))
    };
    match target {
        None => {
            let schema = table.catalog().schema.clone();
            let bound = binder::Binder::new(&schema).bind_query(q)?;
            let r = exec::execute(table, &bound).map_err(exec_err)?;
            Ok((bound, r))
        }
        Some(key) => {
            let derived = table
                .derived_table(&key)
                .expect("a derived table was just materialized or found");
            let schema = derived.catalog().schema.clone();
            let bound = binder::Binder::new(&schema).bind_query(q)?;
            let r = exec::execute(derived, &bound).map_err(exec_err)?;
            Ok((bound, r))
        }
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
) -> Result<String, Diagnostic> {
    let parent = match &q.from_subquery {
        Some(_) => String::new(), // its own key covers the nested body text
        None => scope.iter().rev().find(|(n, _)| *n == q.from).map(|(_, k)| k.clone()).unwrap_or_default(),
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
    let (bound, result) = exec_query(table, q, src, scope)?;
    let image = crate::materialize::compile_result(&bound, result, 65_536)?;
    table
        .derived_insert(&key, image)
        .map_err(|e| Diagnostic::new(format!("derived table: {e}"), body))?;
    Ok(key)
}
pub use span::Diagnostic;
