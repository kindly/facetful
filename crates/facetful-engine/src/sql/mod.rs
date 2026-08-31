//! The SQL layer: lexer -> parser -> (binder -> planner, arriving next).
//! Design decisions and error-message philosophy: docs/sql-parser.sv.

pub mod ast;
pub mod binder;
pub mod lexer;
pub mod parser;
pub mod span;

pub use parser::{parse_expr, parse_query};
pub use span::Diagnostic;
