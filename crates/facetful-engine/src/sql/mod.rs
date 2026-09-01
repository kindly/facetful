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
    let q = parse_query(src)?;
    let schema = table.catalog().schema.clone();
    let bound = binder::Binder::new(&schema).bind_query(&q)?;
    exec::execute(table, &bound)
        .map_err(|e| Diagnostic::new(format!("execution error: {e}"), span::Span::new(0, 0)))
}
pub use span::Diagnostic;
