//! AST. Deliberately small: every SQL idiom the parser accepts as sugar
//! (BETWEEN, IN, IS NULL, LIKE, CASE, CAST, COUNT(DISTINCT), ||) desugars into
//! `Expr::Call`, so the binder/planner/executor see one uniform shape and never
//! know which spelling arrived.

use super::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Number(f64, bool, Span),
    Str(String, Span),
    Column(String, Span),
    /// function call — both spelled calls and desugared idioms
    Call { name: String, args: Vec<Expr>, span: Span },
    Unary { op: UnOp, expr: Box<Expr>, span: Span },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    /// `*` in `count(*)` / `select *`
    Star(Span),
    Null(Span),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Number(_, _, s)
            | Expr::Str(_, s)
            | Expr::Column(_, s)
            | Expr::Star(s)
            | Expr::Null(s) => *s,
            Expr::Call { span, .. } | Expr::Unary { span, .. } | Expr::Binary { span, .. } => *span,
        }
    }

    pub fn call(name: &str, args: Vec<Expr>, span: Span) -> Expr {
        Expr::Call { name: name.into(), args, span }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub dir: SortDir,
}

/// `WITH name AS (query)`: a named derived table, materialized once and
/// cached — the optimization fence is the only mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    pub name_span: Span,
    pub query: Box<Query>,
    /// the parenthesized body's text, for the derived-table cache key
    pub body_span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub with: Vec<Cte>,
    pub select: Vec<SelectItem>,
    /// the FROM name: a CTE in scope, else the table itself (any spelling);
    /// for `FROM (subquery) alias`, the alias (or empty)
    pub from: String,
    pub from_span: Span,
    /// `FROM (subquery)`: an anonymous CTE
    pub from_subquery: Option<Box<Query>>,
    /// the whole query's text
    pub span: Span,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}
