//! The binder: resolves a parsed Query against a table schema, checks names,
//! arity and types, classifies aggregate vs scalar context, and validates
//! GROUP BY shape. This is where the diagnostics users actually feel live —
//! every error carries the offending span and, where possible, a suggestion.

use super::ast::{BinOp, Expr, Query, SortDir, UnOp};
use super::span::{suggest, Diagnostic, Span};
use crate::format::{ColumnType, Schema};

/// Value types as the engine computes them (storage widths erased).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Int,
    Float,
    Text,
    Bool,
    /// days since 1970-01-01; represented as Int everywhere in execution
    Date,
    /// ms since the epoch, UTC; represented as Int in execution
    Timestamp,
    /// the type of a bare NULL literal — coerces to anything
    Null,
}

impl Ty {
    fn of_column(c: ColumnType) -> Ty {
        match c {
            ColumnType::Bool => Ty::Bool,
            ColumnType::Int8
            | ColumnType::Int16
            | ColumnType::Int32
            | ColumnType::Int64 => Ty::Int,
            ColumnType::Date => Ty::Date,
            ColumnType::Timestamp => Ty::Timestamp,
            ColumnType::Float64 => Ty::Float,
            ColumnType::Utf8 => Ty::Text,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::Text => "text",
            Ty::Bool => "bool",
            Ty::Date => "date",
            Ty::Timestamp => "timestamp",
            Ty::Null => "null",
        }
    }
    fn numeric(self) -> bool {
        // Date/Timestamp are ints with meaning: they compare, group, and
        // aggregate as their underlying days/ms
        matches!(self, Ty::Int | Ty::Float | Ty::Date | Ty::Timestamp | Ty::Null)
    }
    fn temporal(self) -> bool {
        matches!(self, Ty::Date | Ty::Timestamp | Ty::Null)
    }
    fn coerces_to(self, other: Ty) -> bool {
        self == other
            || self == Ty::Null
            || (self == Ty::Int && other == Ty::Float)
            || (matches!(self, Ty::Date | Ty::Timestamp) && matches!(other, Ty::Int | Ty::Float))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Bound {
    Number(f64, /*is_float*/ bool),
    Str(String),
    Null,
    Column { index: usize, ty: Ty },
    Call { func: &'static FuncDef, args: Vec<Bound>, ty: Ty },
    Unary { op: UnOp, expr: Box<Bound>, ty: Ty },
    Binary { op: BinOp, lhs: Box<Bound>, rhs: Box<Bound>, ty: Ty },
}

impl Bound {
    pub fn ty(&self) -> Ty {
        match self {
            Bound::Number(_, is_float) => {
                if *is_float { Ty::Float } else { Ty::Int }
            }
            Bound::Str(_) => Ty::Text,
            Bound::Null => Ty::Null,
            Bound::Column { ty, .. }
            | Bound::Call { ty, .. }
            | Bound::Unary { ty, .. }
            | Bound::Binary { ty, .. } => *ty,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    pub expr: Bound,
    pub name: String,
    /// true when the expression contains an aggregate call
    pub aggregated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundQuery {
    pub select: Vec<BoundSelect>,
    pub filter: Option<Bound>,
    pub group_by: Vec<Bound>,
    pub order_by: Vec<(Bound, SortDir)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    /// whether the query aggregates at all (explicit GROUP BY or bare aggregates)
    pub is_aggregate: bool,
}

// ---------------- function registry ----------------

#[derive(Debug, PartialEq)]
pub struct FuncDef {
    pub name: &'static str,
    pub kind: FuncKind,
    /// (min_args, max_args) — max None = variadic
    pub arity: (usize, Option<usize>),
    pub sig: Sig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncKind {
    Scalar,
    Aggregate,
}

/// Just enough signature machinery for the v1 registry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sig {
    /// args any-type, fixed return
    Any(Ty),
    /// all args numeric, returns Float
    NumericToFloat,
    /// all args numeric, returns the widest arg type
    NumericSame,
    /// all args numeric, returns Int
    NumericToInt,
    /// first arg text (rest per arity), returns Text
    TextToText,
    /// first arg text, returns Int
    TextToInt,
    /// comparison-style: args must share a comparable type, returns Bool
    ComparableToBool,
    /// temporal functions: per-name arg rules live in check_sig
    Temporal(Ty),
    /// all args same type as first, returns that type
    SameAsFirst,
    /// a user-defined function (crate::udf): declared parameter types (the
    /// last repeats when variadic; `Ty::Null` = any) and return type;
    /// `strict` = NULL in, NULL out
    Udf { id: u32, params: &'static [Ty], ret: Ty, strict: bool },
}

pub static FUNCS: &[FuncDef] = &[
    // aggregates
    FuncDef { name: "count", kind: FuncKind::Aggregate, arity: (1, Some(1)), sig: Sig::Any(Ty::Int) },
    FuncDef { name: "count_distinct", kind: FuncKind::Aggregate, arity: (1, Some(1)), sig: Sig::Any(Ty::Int) },
    FuncDef { name: "sum", kind: FuncKind::Aggregate, arity: (1, Some(1)), sig: Sig::NumericSame },
    FuncDef { name: "avg", kind: FuncKind::Aggregate, arity: (1, Some(1)), sig: Sig::NumericToFloat },
    FuncDef { name: "min", kind: FuncKind::Aggregate, arity: (1, Some(1)), sig: Sig::SameAsFirst },
    FuncDef { name: "max", kind: FuncKind::Aggregate, arity: (1, Some(1)), sig: Sig::SameAsFirst },
    FuncDef { name: "median", kind: FuncKind::Aggregate, arity: (1, Some(1)), sig: Sig::NumericToFloat },
    FuncDef { name: "stddev", kind: FuncKind::Aggregate, arity: (1, Some(1)), sig: Sig::NumericToFloat },
    FuncDef { name: "group_concat", kind: FuncKind::Aggregate, arity: (1, Some(2)), sig: Sig::Any(Ty::Text) },
    // scalars
    FuncDef { name: "abs", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::NumericSame },
    FuncDef { name: "round", kind: FuncKind::Scalar, arity: (1, Some(2)), sig: Sig::NumericSame },
    FuncDef { name: "floor", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::NumericSame },
    FuncDef { name: "ceil", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::NumericSame },
    FuncDef { name: "lower", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::TextToText },
    FuncDef { name: "upper", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::TextToText },
    FuncDef { name: "length", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::TextToInt },
    FuncDef { name: "substr", kind: FuncKind::Scalar, arity: (2, Some(3)), sig: Sig::TextToText },
    FuncDef { name: "concat", kind: FuncKind::Scalar, arity: (2, None), sig: Sig::TextToText },
    FuncDef { name: "coalesce", kind: FuncKind::Scalar, arity: (2, None), sig: Sig::SameAsFirst },
    FuncDef { name: "trim", kind: FuncKind::Scalar, arity: (1, Some(2)), sig: Sig::TextToText },
    FuncDef { name: "ltrim", kind: FuncKind::Scalar, arity: (1, Some(2)), sig: Sig::TextToText },
    FuncDef { name: "rtrim", kind: FuncKind::Scalar, arity: (1, Some(2)), sig: Sig::TextToText },
    FuncDef { name: "replace", kind: FuncKind::Scalar, arity: (3, Some(3)), sig: Sig::TextToText },
    FuncDef { name: "instr", kind: FuncKind::Scalar, arity: (2, Some(2)), sig: Sig::TextToInt },
    FuncDef { name: "nullif", kind: FuncKind::Scalar, arity: (2, Some(2)), sig: Sig::SameAsFirst },
    FuncDef { name: "ifnull", kind: FuncKind::Scalar, arity: (2, Some(2)), sig: Sig::SameAsFirst },
    FuncDef { name: "sign", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::NumericToInt },
    FuncDef { name: "sqrt", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::NumericToFloat },
    FuncDef { name: "exp", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::NumericToFloat },
    FuncDef { name: "ln", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::NumericToFloat },
    FuncDef { name: "pow", kind: FuncKind::Scalar, arity: (2, Some(2)), sig: Sig::NumericToFloat },
    FuncDef { name: "power", kind: FuncKind::Scalar, arity: (2, Some(2)), sig: Sig::NumericToFloat },
    // temporal (Date = days since epoch, Timestamp = ms since epoch, UTC)
    FuncDef { name: "year", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Temporal(Ty::Int) },
    FuncDef { name: "month", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Temporal(Ty::Int) },
    FuncDef { name: "day", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Temporal(Ty::Int) },
    FuncDef { name: "hour", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Temporal(Ty::Int) },
    FuncDef { name: "minute", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Temporal(Ty::Int) },
    FuncDef { name: "second", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Temporal(Ty::Int) },
    FuncDef { name: "date", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Temporal(Ty::Date) },
    FuncDef { name: "timestamp", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Temporal(Ty::Timestamp) },
    FuncDef { name: "strftime", kind: FuncKind::Scalar, arity: (2, Some(2)), sig: Sig::Temporal(Ty::Text) },
    // desugar targets
    FuncDef { name: "between", kind: FuncKind::Scalar, arity: (3, Some(3)), sig: Sig::ComparableToBool },
    FuncDef { name: "in", kind: FuncKind::Scalar, arity: (2, None), sig: Sig::ComparableToBool },
    FuncDef { name: "like", kind: FuncKind::Scalar, arity: (2, Some(2)), sig: Sig::ComparableToBool },
    FuncDef { name: "isnull", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Any(Ty::Bool) },
    FuncDef { name: "if", kind: FuncKind::Scalar, arity: (3, Some(3)), sig: Sig::SameAsFirst },
    FuncDef { name: "case", kind: FuncKind::Scalar, arity: (2, None), sig: Sig::SameAsFirst },
    FuncDef { name: "int", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Any(Ty::Int) },
    FuncDef { name: "float", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Any(Ty::Float) },
    FuncDef { name: "text", kind: FuncKind::Scalar, arity: (1, Some(1)), sig: Sig::Any(Ty::Text) },
];

fn lookup_func(name: &str) -> Option<&'static FuncDef> {
    FUNCS.iter().find(|f| f.name == name).or_else(|| crate::udf::lookup(name))
}

// ---------------- binder ----------------

pub struct Binder<'a> {
    schema: &'a Schema,
}

impl<'a> Binder<'a> {
    pub fn new(schema: &'a Schema) -> Self {
        Self { schema }
    }

    pub fn bind_query(&self, q: &Query) -> Result<BoundQuery, Diagnostic> {
        let filter = match &q.filter {
            Some(e) => {
                // bind permissively so the WHERE-specific message below wins
                let b = self.bind(e, true)?;
                if contains_aggregate(&b) {
                    return Err(Diagnostic::new(
                        "aggregate functions are not allowed in WHERE",
                        e.span(),
                    )
                    .with_hint("filter on aggregates by wrapping the query later (HAVING is not supported yet)"));
                }
                if !matches!(b.ty(), Ty::Bool | Ty::Null) {
                    return Err(Diagnostic::new(
                        format!("WHERE needs a boolean condition, this is {}", b.ty().name()),
                        e.span(),
                    ));
                }
                Some(b)
            }
            None => None,
        };

        let mut select = Vec::new();
        let mut select_spans = Vec::new(); // parallel to `select` (star expands 1 -> N)
        for (i, item) in q.select.iter().enumerate() {
            // `*` expands to every table column, in schema order
            if let Expr::Star(sp) = &item.expr {
                for c in &self.schema.columns {
                    let e = Expr::Column(c.name.clone(), *sp);
                    let b = self.bind(&e, true)?;
                    select.push(BoundSelect { expr: b, name: c.name.clone(), aggregated: false });
                    select_spans.push(*sp);
                }
                continue;
            }
            let b = self.bind(&item.expr, true)?;
            let aggregated = contains_aggregate(&b);
            let name = item.alias.clone().unwrap_or_else(|| default_name(&item.expr, i));
            select.push(BoundSelect { expr: b, name, aggregated });
            select_spans.push(item.expr.span());
        }

        // GROUP BY may reference select aliases or positions (1-based), like
        // ORDER BY. A name that is also a table column binds as the column
        // (input-column-first, as in Postgres); anything else binds as an
        // expression. Bound after the select list so aliases are known.
        let mut group_by = Vec::new();
        for e in &q.group_by {
            let b = match e {
                Expr::Column(name, span)
                    if !self.schema.columns.iter().any(|c| &c.name == name)
                        && select.iter().any(|s| &s.name == name) =>
                {
                    let item = select.iter().find(|s| &s.name == name).unwrap();
                    if item.aggregated {
                        return Err(Diagnostic::new(
                            format!("cannot GROUP BY '{name}': it is an aggregate"),
                            *span,
                        ));
                    }
                    item.expr.clone()
                }
                Expr::Number(n, false, span) if n.fract() == 0.0 => {
                    let idx = *n as usize;
                    if idx == 0 || idx > select.len() {
                        return Err(Diagnostic::new(
                            format!("GROUP BY position {idx} is out of range (1..={})", select.len()),
                            *span,
                        ));
                    }
                    if select[idx - 1].aggregated {
                        return Err(Diagnostic::new(
                            format!("cannot GROUP BY position {idx}: it is an aggregate"),
                            *span,
                        ));
                    }
                    select[idx - 1].expr.clone()
                }
                e => self.bind(e, false)?,
            };
            group_by.push(b);
        }

        let is_aggregate = !q.group_by.is_empty() || select.iter().any(|s| s.aggregated);
        if is_aggregate {
            for (bound, span) in select.iter().zip(&select_spans) {
                // bound comparison — Bound carries no spans, so `country` in the
                // select list equals `country` in GROUP BY
                if !bound.aggregated && !group_by.iter().any(|g| g == &bound.expr) {
                    return Err(Diagnostic::new(
                        format!(
                            "'{}' must appear in GROUP BY or be inside an aggregate function",
                            &bound.name
                        ),
                        *span,
                    ));
                }
            }
        }

        let mut order_by = Vec::new();
        for o in &q.order_by {
            // ORDER BY may reference select aliases or positions (1-based), like SQL
            let b = match &o.expr {
                Expr::Column(name, _) if select.iter().any(|s| &s.name == name) => {
                    select.iter().find(|s| &s.name == name).unwrap().expr.clone()
                }
                Expr::Number(n, false, span) if n.fract() == 0.0 => {
                    let idx = *n as usize;
                    if idx == 0 || idx > select.len() {
                        return Err(Diagnostic::new(
                            format!("ORDER BY position {idx} is out of range (1..={})", select.len()),
                            *span,
                        ));
                    }
                    select[idx - 1].expr.clone()
                }
                e => self.bind(e, true)?,
            };
            order_by.push((b, o.dir));
        }

        Ok(BoundQuery {
            select,
            filter,
            group_by,
            order_by,
            limit: q.limit,
            offset: q.offset,
            is_aggregate,
        })
    }

    fn bind(&self, e: &Expr, allow_aggregate: bool) -> Result<Bound, Diagnostic> {
        match e {
            Expr::Number(n, f, _) => Ok(Bound::Number(*n, *f)),
            Expr::Str(s, _) => Ok(Bound::Str(s.clone())),
            Expr::Null(_) => Ok(Bound::Null),
            Expr::Star(span) => Err(Diagnostic::new(
                "'*' can only be used in the select list or as count(*)",
                *span,
            )),
            Expr::Column(name, span) => self.bind_column(name, *span),
            Expr::Row(_, span) => Err(Diagnostic::new(
                "a row value (a, b) is only valid on the left of IN",
                *span,
            )),
            Expr::InSubquery { span, .. } => Err(Diagnostic::new(
                "IN (select …) is supported in WHERE, comparing plain columns",
                *span,
            )
            .with_hint("write the subquery as a CTE or JOIN to use it elsewhere")),
            Expr::Unary { op, expr, span } => {
                let b = self.bind(expr, allow_aggregate)?;
                let ty = match op {
                    UnOp::Neg => {
                        if !b.ty().numeric() {
                            return Err(Diagnostic::new(
                                format!("'-' needs a number, this is {}", b.ty().name()),
                                *span,
                            ));
                        }
                        b.ty()
                    }
                    UnOp::Not => {
                        if !matches!(b.ty(), Ty::Bool | Ty::Null) {
                            return Err(Diagnostic::new(
                                format!("'not' needs a boolean, this is {}", b.ty().name()),
                                *span,
                            ));
                        }
                        Ty::Bool
                    }
                };
                Ok(Bound::Unary { op: *op, expr: Box::new(b), ty })
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let l = self.bind(lhs, allow_aggregate)?;
                let r = self.bind(rhs, allow_aggregate)?;
                let ty = self.binary_type(*op, &l, &r, *span)?;
                Ok(Bound::Binary { op: *op, lhs: Box::new(l), rhs: Box::new(r), ty })
            }
            Expr::Call { name, args, span } => self.bind_call(name, args, *span, allow_aggregate),
        }
    }

    fn bind_column(&self, name: &str, span: Span) -> Result<Bound, Diagnostic> {
        match self.schema.columns.iter().position(|c| c.name == name) {
            Some(index) => {
                Ok(Bound::Column { index, ty: Ty::of_column(self.schema.columns[index].ty) })
            }
            None => {
                let mut d = Diagnostic::new(format!("unknown column '{name}'"), span);
                if let Some(s) = suggest(name, self.schema.columns.iter().map(|c| c.name.as_str()))
                {
                    d = d.with_hint(format!("did you mean '{s}'?"));
                }
                Err(d)
            }
        }
    }

    fn bind_call(
        &self,
        name: &str,
        args: &[Expr],
        span: Span,
        allow_aggregate: bool,
    ) -> Result<Bound, Diagnostic> {
        let Some(func) = lookup_func(name) else {
            let mut d = Diagnostic::new(format!("unknown function '{name}'"), span);
            if let Some(s) = suggest(name, FUNCS.iter().map(|f| f.name).chain(crate::udf::names())) {
                d = d.with_hint(format!("did you mean '{s}()'?"));
            }
            return Err(d);
        };

        if func.kind == FuncKind::Aggregate && !allow_aggregate {
            return Err(Diagnostic::new(
                format!("aggregate function '{name}()' is not allowed here"),
                span,
            ));
        }

        // count(*) special case: the only place Star binds
        let mut bound_args = Vec::with_capacity(args.len());
        for a in args {
            if let (Expr::Star(_), "count") = (a, name) {
                bound_args.push(Bound::Number(1.0, false)); // count(*) == count(1)
                continue;
            }
            // scalar calls pass the context through (round(sum(x)) is legal in a
            // select list); aggregate calls bind permissively so the tailored
            // nested-aggregate error below fires instead of a generic one
            let arg_allow = if func.kind == FuncKind::Aggregate { true } else { allow_aggregate };
            let b = self.bind(a, arg_allow)?;
            if func.kind == FuncKind::Aggregate && contains_aggregate(&b) {
                return Err(Diagnostic::new(
                    format!("aggregate functions cannot be nested inside '{name}()'"),
                    a.span(),
                ));
            }
            bound_args.push(b);
        }

        let (min, max) = func.arity;
        if bound_args.len() < min || max.map_or(false, |m| bound_args.len() > m) {
            let want = match max {
                Some(m) if m == min => format!("{min}"),
                Some(m) => format!("{min} to {m}"),
                None => format!("at least {min}"),
            };
            return Err(Diagnostic::new(
                format!("{name}() takes {want} argument(s), got {}", bound_args.len()),
                span,
            ));
        }

        let ty = self.check_sig(func, &bound_args, args, span)?;
        // the executor evaluates the separator once, at plan time
        if func.name == "group_concat"
            && bound_args.len() == 2
            && !matches!(bound_args[1], Bound::Str(_))
        {
            return Err(Diagnostic::new(
                "group_concat() separator must be a text literal",
                args[1].span(),
            ));
        }
        Ok(Bound::Call { func, args: bound_args, ty })
    }

    fn check_sig(
        &self,
        func: &FuncDef,
        bound: &[Bound],
        exprs: &[Expr],
        span: Span,
    ) -> Result<Ty, Diagnostic> {
        let arg_span = |i: usize| exprs.get(i).map(|e| e.span()).unwrap_or(span);
        match func.sig {
            Sig::Any(ret) => Ok(ret),
            Sig::Udf { params, ret, .. } => {
                for (i, b) in bound.iter().enumerate() {
                    let want = params[i.min(params.len() - 1)];
                    // Ty::Null in a signature means "any type"
                    if want != Ty::Null && !b.ty().coerces_to(want) {
                        return Err(Diagnostic::new(
                            format!("{}() argument {} needs {}, this is {}", func.name, i + 1, want.name(), b.ty().name()),
                            arg_span(i),
                        ));
                    }
                }
                Ok(ret)
            }
            Sig::NumericToFloat | Sig::NumericSame | Sig::NumericToInt => {
                let mut widest = Ty::Int;
                for (i, b) in bound.iter().enumerate() {
                    if !b.ty().numeric() {
                        return Err(Diagnostic::new(
                            format!("{}() needs a number, argument {} is {}", func.name, i + 1, b.ty().name()),
                            arg_span(i),
                        ));
                    }
                    if b.ty() == Ty::Float {
                        widest = Ty::Float;
                    }
                }
                Ok(match func.sig {
                    Sig::NumericToFloat => Ty::Float,
                    Sig::NumericToInt => Ty::Int,
                    _ => widest,
                })
            }
            Sig::TextToText | Sig::TextToInt => {
                if !bound[0].ty().coerces_to(Ty::Text) {
                    return Err(Diagnostic::new(
                        format!("{}() needs text, this is {}", func.name, bound[0].ty().name()),
                        arg_span(0),
                    ));
                }
                Ok(if func.sig == Sig::TextToText { Ty::Text } else { Ty::Int })
            }
            Sig::ComparableToBool => {
                let base = bound[0].ty();
                for (i, b) in bound.iter().enumerate().skip(1) {
                    if !(b.ty().coerces_to(base) || base.coerces_to(b.ty())) {
                        return Err(Diagnostic::new(
                            format!(
                                "{}() compares {} values, argument {} is {}",
                                func.name, base.name(), i + 1, b.ty().name()
                            ),
                            arg_span(i),
                        ));
                    }
                }
                Ok(Ty::Bool)
            }
            Sig::Temporal(ret) => {
                match func.name {
                    // extraction: the argument must carry temporal meaning
                    "year" | "month" | "day" => {
                        if !bound[0].ty().temporal() {
                            return Err(Diagnostic::new(
                                format!(
                                    "{}() needs a date or timestamp, this is {}",
                                    func.name, bound[0].ty().name()
                                ),
                                arg_span(0),
                            )
                            .with_hint("wrap plain values: date('2020-01-15'), date(days), timestamp(ms)"));
                        }
                    }
                    "hour" | "minute" | "second" => {
                        if !matches!(bound[0].ty(), Ty::Timestamp | Ty::Null) {
                            return Err(Diagnostic::new(
                                format!(
                                    "{}() needs a timestamp, this is {}",
                                    func.name, bound[0].ty().name()
                                ),
                                arg_span(0),
                            ));
                        }
                    }
                    // constructors accept text (ISO), their own type, or raw ints
                    "date" | "timestamp" => {
                        let t = bound[0].ty();
                        if !(t.temporal() || t == Ty::Text || t == Ty::Int) {
                            return Err(Diagnostic::new(
                                format!("{}() cannot convert {}", func.name, t.name()),
                                arg_span(0),
                            ));
                        }
                    }
                    "strftime" => {
                        if !bound[0].ty().coerces_to(Ty::Text) {
                            return Err(Diagnostic::new(
                                format!(
                                    "strftime() needs a format string first, this is {}",
                                    bound[0].ty().name()
                                ),
                                arg_span(0),
                            ));
                        }
                        if !bound[1].ty().temporal() {
                            return Err(Diagnostic::new(
                                format!(
                                    "strftime() needs a date or timestamp, this is {}",
                                    bound[1].ty().name()
                                ),
                                arg_span(1),
                            ));
                        }
                    }
                    _ => {}
                }
                Ok(ret)
            }
            Sig::SameAsFirst => {
                // if/case: value type comes from the first value position
                let ret = match func.name {
                    "if" => bound[1].ty(),
                    "case" => bound[1].ty(),
                    _ => bound[0].ty(),
                };
                Ok(ret)
            }
        }
    }

    fn binary_type(&self, op: BinOp, l: &Bound, r: &Bound, span: Span) -> Result<Ty, Diagnostic> {
        use BinOp::*;
        match op {
            Add | Sub | Mul | Div | Mod => {
                if !l.ty().numeric() || !r.ty().numeric() {
                    return Err(Diagnostic::new(
                        format!(
                            "arithmetic needs numbers, got {} and {}",
                            l.ty().name(), r.ty().name()
                        ),
                        span,
                    )
                    .with_hint("to join text use '||' or concat()"));
                }
                // SQLite semantics: int/int stays Int (truncating division)
                Ok(if l.ty() == Ty::Float || r.ty() == Ty::Float { Ty::Float } else { Ty::Int })
            }
            Eq | Ne | Lt | Le | Gt | Ge => {
                if !(l.ty().coerces_to(r.ty()) || r.ty().coerces_to(l.ty())) {
                    return Err(Diagnostic::new(
                        format!("cannot compare {} with {}", l.ty().name(), r.ty().name()),
                        span,
                    ));
                }
                Ok(Ty::Bool)
            }
            And | Or => {
                for side in [l, r] {
                    if !matches!(side.ty(), Ty::Bool | Ty::Null) {
                        return Err(Diagnostic::new(
                            format!(
                                "'{}' needs boolean operands, got {}",
                                if op == And { "and" } else { "or" },
                                side.ty().name()
                            ),
                            span,
                        ));
                    }
                }
                Ok(Ty::Bool)
            }
        }
    }
}

pub fn contains_aggregate(b: &Bound) -> bool {
    match b {
        Bound::Call { func, args, .. } => {
            func.kind == FuncKind::Aggregate || args.iter().any(contains_aggregate)
        }
        Bound::Unary { expr, .. } => contains_aggregate(expr),
        Bound::Binary { lhs, rhs, .. } => contains_aggregate(lhs) || contains_aggregate(rhs),
        _ => false,
    }
}

fn default_name(e: &Expr, i: usize) -> String {
    match e {
        Expr::Column(name, _) => name.clone(),
        Expr::Call { name, .. } => name.clone(),
        _ => format!("column{}", i + 1),
    }
}
