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
    /// `(a, b)`: a row value — valid only as the left side of IN, or as an
    /// element of an IN list (where it desugars at parse time)
    Row(Vec<Expr>, Span),
    /// `(a, b) IN (select x, y …)`: a semi-join, resolved before binding into
    /// a cached materialization + join (design.sv d41 step 4). With `exists`,
    /// `EXISTS (select … where inner.k = outer.k …)`: `cols` is empty and the
    /// keys come from the correlation in the subquery's WHERE.
    InSubquery { cols: Vec<Expr>, query: Box<Query>, body: Span, span: Span, exists: bool },
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
            | Expr::Null(s)
            | Expr::Row(_, s) => *s,
            Expr::Call { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::InSubquery { span, .. } => *span,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Left,
    Inner,
}

/// What a JOIN reads: a named table (a CTE in scope, or a table the
/// catalog knows) or a subquery.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinSource {
    Table(String),
    Subquery(Box<Query>, Span),
}

/// One `[LEFT|INNER] JOIN source [AS] alias ON a = b [AND …] | USING (cols)`.
/// Only column equalities: the join is a many-to-one materialization
/// (design.sv d41), so the condition is a key list, not a predicate.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub source: JoinSource,
    pub alias: Option<String>,
    /// (left side expr, right side expr) pairs, each a plain or qualified column
    pub on: Vec<(Expr, Expr)>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub with: Vec<Cte>,
    pub select: Vec<SelectItem>,
    /// the FROM name: a CTE in scope, a table the catalog knows, else the
    /// table itself (any spelling); for `FROM (subquery) alias`, the alias
    pub from: String,
    pub from_span: Span,
    pub from_alias: Option<String>,
    /// `FROM (subquery)`: an anonymous CTE
    pub from_subquery: Option<Box<Query>>,
    /// joins, applied left to right; each materializes onto the running table
    pub joins: Vec<Join>,
    /// the whole query's text
    pub span: Span,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}
