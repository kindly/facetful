//! Recursive-descent query parser + Pratt expression parser.
//!
//! Every accepted SQL idiom desugars into `Expr::Call` (see ast.rs), so
//! `x BETWEEN 1 AND 5` and `between(x, 1, 5)` produce identical trees.
//! Error messages are contextual: at every decision point the parser knows
//! what it is in the middle of, and says so.

use super::ast::*;
use super::lexer::{lex, SpannedTok, Tok};
use super::span::{Diagnostic, Span};

pub fn parse_query(src: &str) -> Result<Query, Diagnostic> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    let q = p.query()?;
    p.expect_eof()?;
    Ok(q)
}

/// Parse a bare expression (used by tests and later by materialize options).
pub fn parse_expr(src: &str) -> Result<Expr, Diagnostic> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    let e = p.expr(0)?;
    p.expect_eof()?;
    Ok(e)
}

struct Parser {
    toks: Vec<SpannedTok>,
    pos: usize,
}

// Binding powers (higher binds tighter). OR < AND < NOT < cmp < || < +- < */% < unary.
const BP_OR: u8 = 10;
const BP_AND: u8 = 20;
const BP_NOT: u8 = 30;
const BP_CMP: u8 = 40;
const BP_CONCAT: u8 = 50;
const BP_ADD: u8 = 60;
const BP_MUL: u8 = 70;
const BP_UNARY: u8 = 80;

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_span(&self) -> Span {
        self.toks[self.pos].span
    }
    fn next(&mut self) -> SpannedTok {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.next();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: Tok, ctx: &str) -> Result<Span, Diagnostic> {
        if self.peek() == &t {
            Ok(self.next().span)
        } else {
            Err(Diagnostic::new(
                format!("expected {} {}, found {}", describe(&t), ctx, describe(self.peek())),
                self.peek_span(),
            ))
        }
    }
    fn expect_eof(&mut self) -> Result<(), Diagnostic> {
        if self.peek() == &Tok::Eof {
            Ok(())
        } else {
            Err(Diagnostic::new(
                format!("unexpected {} after the end of the query", describe(self.peek())),
                self.peek_span(),
            ))
        }
    }

    // ---------------- query skeleton ----------------

    fn query(&mut self) -> Result<Query, Diagnostic> {
        let start = self.peek_span().start;
        // WITH name AS (query) [, …] — each body is a full query, recursively
        let mut with = Vec::new();
        if self.eat(&Tok::With) {
            loop {
                let (name, name_span) = self.ident("as the name after 'with'")?;
                self.expect(Tok::As, "after the name in 'with name as (…)'")?;
                let open = self.expect(Tok::LParen, "to open the with-query")?;
                let query = Box::new(self.query()?);
                let close = self.expect(Tok::RParen, "to close the with-query")?;
                let body_span = Span::new(open.start, close.end);
                with.push(Cte { name, name_span, query, body_span });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::Select, "to start the query")?;
        let mut select = vec![self.select_item()?];
        while self.eat(&Tok::Comma) {
            select.push(self.select_item()?);
        }
        self.expect(Tok::From, "after the select list")
            .map_err(|d| d.with_hint("multiple select expressions are separated by ','"))?;
        // FROM name, or FROM (subquery) [AS] alias
        let (from, from_span, from_subquery) = if let Tok::LParen = self.peek() {
            let open = self.next().span;
            let sub = Box::new(self.query()?);
            let close = self.expect(Tok::RParen, "to close the subquery")?;
            let alias = if self.eat(&Tok::As) {
                Some(self.ident("after 'as'")?.0)
            } else if let Tok::Ident(_) | Tok::QuotedIdent(_) = self.peek() {
                Some(self.ident("as the subquery alias")?.0)
            } else {
                None
            };
            (alias.unwrap_or_default(), Span::new(open.start, close.end), Some(sub))
        } else {
            let (f, s) = self.table_name()?;
            (f, s, None)
        };
        let from_alias = self.table_alias()?;
        let mut joins = Vec::new();
        loop {
            let start = self.peek_span().start;
            let kind = match self.peek() {
                Tok::Join => JoinKind::Inner,
                Tok::Inner => {
                    self.next();
                    JoinKind::Inner
                }
                Tok::Left => {
                    self.next();
                    self.eat(&Tok::Outer);
                    JoinKind::Left
                }
                _ => break,
            };
            self.expect(Tok::Join, "after the join kind")?;
            let source = if let Tok::LParen = self.peek() {
                let open = self.next().span;
                let sub = Box::new(self.query()?);
                let close = self.expect(Tok::RParen, "to close the joined subquery")?;
                JoinSource::Subquery(sub, Span::new(open.start, close.end))
            } else {
                JoinSource::Table(self.ident("as the joined table name")?.0)
            };
            let alias = self.table_alias()?;
            let on = if self.eat(&Tok::Using) {
                self.expect(Tok::LParen, "after 'using'")?;
                let mut cols = Vec::new();
                loop {
                    let (name, sp) = self.ident("as a column name in using (…)")?;
                    cols.push((Expr::Column(name.clone(), sp), Expr::Column(name, sp)));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen, "to close using (…)")?;
                cols
            } else {
                self.expect(Tok::On, "after the joined table (on a.k = b.k, or using (k))")?;
                let cond = self.expr(0)?;
                let mut pairs = Vec::new();
                flatten_equalities(cond, &mut pairs)?;
                pairs
            };
            let end = self.toks[self.pos.saturating_sub(1)].span.end;
            joins.push(Join { kind, source, alias, on, span: Span::new(start, end) });
        }

        let filter = if self.eat(&Tok::Where) { Some(self.expr(0)?) } else { None };

        let mut group_by = Vec::new();
        if self.eat(&Tok::Group) {
            self.expect(Tok::By, "after 'group'")?;
            group_by.push(self.expr(0)?);
            while self.eat(&Tok::Comma) {
                group_by.push(self.expr(0)?);
            }
        }

        let mut order_by = Vec::new();
        if self.eat(&Tok::Order) {
            self.expect(Tok::By, "after 'order'")?;
            loop {
                let expr = self.expr(0)?;
                let dir = if self.eat(&Tok::Desc) {
                    SortDir::Desc
                } else {
                    self.eat(&Tok::Asc);
                    SortDir::Asc
                };
                order_by.push(OrderItem { expr, dir });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }

        let mut limit = None;
        let mut offset = None;
        if self.eat(&Tok::Limit) {
            limit = Some(self.integer("after 'limit'")?);
            if self.eat(&Tok::Offset) {
                offset = Some(self.integer("after 'offset'")?);
            }
        }

        let end = self.toks[self.pos.saturating_sub(1)].span.end;
        Ok(Query {
            with,
            select,
            from,
            from_span,
            from_alias,
            from_subquery,
            joins,
            span: Span::new(start, end),
            filter,
            group_by,
            order_by,
            limit,
            offset,
        })
    }

    fn select_item(&mut self) -> Result<SelectItem, Diagnostic> {
        let expr = self.expr(0)?;
        let alias = if self.eat(&Tok::As) {
            Some(self.ident("after 'as'")?.0)
        } else if let Tok::Ident(_) | Tok::QuotedIdent(_) = self.peek() {
            // bare alias: `select sum(x) total from t`
            Some(self.ident("as an alias")?.0)
        } else {
            None
        };
        Ok(SelectItem { expr, alias })
    }

    fn table_name(&mut self) -> Result<(String, Span), Diagnostic> {
        self.ident("as the table name after 'from'")
    }

    /// `[AS] alias` after a table or subquery; a bare identifier counts.
    fn table_alias(&mut self) -> Result<Option<String>, Diagnostic> {
        if self.eat(&Tok::As) {
            return Ok(Some(self.ident("after 'as'")?.0));
        }
        if let Tok::Ident(_) | Tok::QuotedIdent(_) = self.peek() {
            return Ok(Some(self.ident("as an alias")?.0));
        }
        Ok(None)
    }

    fn ident(&mut self, ctx: &str) -> Result<(String, Span), Diagnostic> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                let s = self.next().span;
                Ok((name, s))
            }
            Tok::QuotedIdent(name) => {
                let s = self.next().span;
                Ok((name, s))
            }
            other => Err(Diagnostic::new(
                format!("expected a name {ctx}, found {}", describe(&other)),
                self.peek_span(),
            )),
        }
    }

    fn integer(&mut self, ctx: &str) -> Result<u64, Diagnostic> {
        match *self.peek() {
            Tok::Number(n, false) if n >= 0.0 && n.fract() == 0.0 => {
                self.next();
                Ok(n as u64)
            }
            _ => Err(Diagnostic::new(
                format!("expected a whole number {ctx}, found {}", describe(self.peek())),
                self.peek_span(),
            )),
        }
    }

    // ---------------- expressions (Pratt) ----------------

    fn expr(&mut self, min_bp: u8) -> Result<Expr, Diagnostic> {
        let mut lhs = self.prefix()?;
        loop {
            let (bp, negated) = match self.peek() {
                Tok::Or if BP_OR >= min_bp => (BP_OR, false),
                Tok::And if BP_AND >= min_bp => (BP_AND, false),
                Tok::Eq | Tok::Ne | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge if BP_CMP >= min_bp => {
                    (BP_CMP, false)
                }
                Tok::Concat if BP_CONCAT >= min_bp => (BP_CONCAT, false),
                Tok::Plus | Tok::Minus if BP_ADD >= min_bp => (BP_ADD, false),
                Tok::Star | Tok::Slash | Tok::Percent if BP_MUL >= min_bp => (BP_MUL, false),
                // idiom sugar operates at comparison precedence
                Tok::In | Tok::Is | Tok::Between | Tok::Like if BP_CMP >= min_bp => (BP_CMP, false),
                Tok::Not if BP_CMP >= min_bp => {
                    // `x NOT IN …`, `x NOT LIKE …`, `x NOT BETWEEN …`
                    match &self.toks.get(self.pos + 1).map(|t| t.tok.clone()) {
                        Some(Tok::In) | Some(Tok::Like) | Some(Tok::Between) => (BP_CMP, true),
                        _ => break,
                    }
                }
                _ => break,
            };
            if negated {
                self.next(); // consume NOT; the sugar handler sees IN/LIKE/BETWEEN next
            }
            lhs = self.infix(lhs, bp, negated)?;
        }
        Ok(lhs)
    }

    fn infix(&mut self, lhs: Expr, bp: u8, negated: bool) -> Result<Expr, Diagnostic> {
        let op_tok = self.next();
        let wrap_not = |e: Expr| {
            let span = e.span();
            if negated { Expr::Unary { op: UnOp::Not, expr: Box::new(e), span } } else { e }
        };
        match op_tok.tok {
            Tok::Or => self.binary(BinOp::Or, lhs, bp),
            Tok::And => self.binary(BinOp::And, lhs, bp),
            Tok::Eq => self.binary(BinOp::Eq, lhs, bp),
            Tok::Ne => self.binary(BinOp::Ne, lhs, bp),
            Tok::Lt => self.binary(BinOp::Lt, lhs, bp),
            Tok::Le => self.binary(BinOp::Le, lhs, bp),
            Tok::Gt => self.binary(BinOp::Gt, lhs, bp),
            Tok::Ge => self.binary(BinOp::Ge, lhs, bp),
            Tok::Concat => {
                let rhs = self.expr(BP_CONCAT + 1)?;
                let span = lhs.span().to(rhs.span());
                Ok(Expr::call("concat", vec![lhs, rhs], span))
            }
            Tok::Plus => self.binary(BinOp::Add, lhs, bp),
            Tok::Minus => self.binary(BinOp::Sub, lhs, bp),
            Tok::Star => self.binary(BinOp::Mul, lhs, bp),
            Tok::Slash => self.binary(BinOp::Div, lhs, bp),
            Tok::Percent => self.binary(BinOp::Mod, lhs, bp),

            // ---- sugar: all desugars to Call, spans cover the whole idiom ----
            Tok::In => {
                let open = self
                    .expect(Tok::LParen, "after 'in'")
                    .map_err(|d| d.with_hint("in expects a parenthesized list: x in ('a', 'b'), or a subquery"))?;
                // x IN (select …) / (a, b) IN (select …): a semi-join
                if let Tok::Select | Tok::With = self.peek() {
                    let query = Box::new(self.query()?);
                    let close = self.expect(Tok::RParen, "to close the subquery")?;
                    let cols = match lhs {
                        Expr::Row(items, _) => items,
                        e => vec![e],
                    };
                    let span = cols[0].span().to(close);
                    let body = Span::new(open.start, close.end);
                    return Ok(wrap_not(Expr::InSubquery { cols, query, body, span }));
                }
                let mut args = vec![lhs];
                loop {
                    args.push(self.expr(0)?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                let close = self.expect(Tok::RParen, "to close the 'in' list")?;
                let span = args[0].span().to(close);
                // (a, b) IN ((1, 'x'), (2, 'y')) → (a = 1 and b = 'x') or (…): the
                // three-valued logic of AND/OR is exactly the row-value rule
                if let Expr::Row(cols, _) = &args[0] {
                    let mut alts: Vec<Expr> = Vec::new();
                    for item in &args[1..] {
                        let Expr::Row(vals, vsp) = item else {
                            return Err(Diagnostic::new(
                                "every element of a row-value IN list must be a row of the same width",
                                item.span(),
                            ));
                        };
                        if vals.len() != cols.len() {
                            return Err(Diagnostic::new(
                                format!("row has {} values, the left side has {}", vals.len(), cols.len()),
                                *vsp,
                            ));
                        }
                        let mut conj: Option<Expr> = None;
                        for (c, v) in cols.iter().zip(vals) {
                            let eq = Expr::Binary { op: BinOp::Eq, lhs: Box::new(c.clone()), rhs: Box::new(v.clone()), span };
                            conj = Some(match conj {
                                None => eq,
                                Some(a) => Expr::Binary { op: BinOp::And, lhs: Box::new(a), rhs: Box::new(eq), span },
                            });
                        }
                        alts.push(conj.expect("a row has at least one value"));
                    }
                    let mut out = alts.remove(0);
                    for a in alts {
                        out = Expr::Binary { op: BinOp::Or, lhs: Box::new(out), rhs: Box::new(a), span };
                    }
                    return Ok(wrap_not(out));
                }
                Ok(wrap_not(Expr::call("in", args, span)))
            }
            Tok::Like => {
                let pat = self.expr(BP_CMP + 1)?;
                let span = lhs.span().to(pat.span());
                Ok(wrap_not(Expr::call("like", vec![lhs, pat], span)))
            }
            Tok::Between => {
                // the middle operand parses above AND so `between a and b` terminates
                let lo = self.expr(BP_AND + 1)?;
                self.expect(Tok::And, "between the bounds of 'between'")
                    .map_err(|d| d.with_hint("between is written: x between low and high"))?;
                let hi = self.expr(BP_AND + 1)?;
                let span = lhs.span().to(hi.span());
                Ok(wrap_not(Expr::call("between", vec![lhs, lo, hi], span)))
            }
            Tok::Is => {
                let neg = self.eat(&Tok::Not);
                let null_span = self.expect(Tok::Null, "after 'is'").map_err(|d| {
                    d.with_hint("only 'is null' / 'is not null' are supported")
                })?;
                let span = lhs.span().to(null_span);
                let e = Expr::call("isnull", vec![lhs], span);
                Ok(if neg { Expr::Unary { op: UnOp::Not, expr: Box::new(e), span } } else { e })
            }
            other => unreachable!("infix dispatched on unexpected token {other:?}"),
        }
    }

    fn binary(&mut self, op: BinOp, lhs: Expr, bp: u8) -> Result<Expr, Diagnostic> {
        let rhs = self.expr(bp + 1)?;
        let span = lhs.span().to(rhs.span());
        Ok(Expr::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs), span })
    }

    fn prefix(&mut self) -> Result<Expr, Diagnostic> {
        let t = self.next();
        match t.tok {
            Tok::Number(n, f) => Ok(Expr::Number(n, f, t.span)),
            Tok::Str(s) => Ok(Expr::Str(s, t.span)),
            Tok::Null => Ok(Expr::Null(t.span)),
            Tok::Star => Ok(Expr::Star(t.span)),
            Tok::Minus => {
                let e = self.expr(BP_UNARY)?;
                let span = t.span.to(e.span());
                Ok(Expr::Unary { op: UnOp::Neg, expr: Box::new(e), span })
            }
            Tok::Not => {
                let e = self.expr(BP_NOT)?;
                let span = t.span.to(e.span());
                Ok(Expr::Unary { op: UnOp::Not, expr: Box::new(e), span })
            }
            Tok::LParen => {
                let e = self.expr(0)?;
                if self.eat(&Tok::Comma) {
                    // a row value: (a, b, …)
                    let mut items = vec![e];
                    loop {
                        items.push(self.expr(0)?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    let close = self.expect(Tok::RParen, "to close the row value")?;
                    return Ok(Expr::Row(items, Span::new(t.span.start, close.end)));
                }
                self.expect(Tok::RParen, "to close this parenthesis")?;
                Ok(e)
            }
            // keyword-named function spellings stay valid: between(x,1,5), in(x,'a'), like(x,p), case(c,v,e)
            Tok::Between if self.peek() == &Tok::LParen => self.call("between".into(), t.span),
            Tok::In if self.peek() == &Tok::LParen => self.call("in".into(), t.span),
            Tok::Like if self.peek() == &Tok::LParen => self.call("like".into(), t.span),
            Tok::Case if self.peek() == &Tok::LParen => self.call("case".into(), t.span),
            Tok::Case => self.case_expr(t.span),
            Tok::Cast => self.cast_expr(t.span),
            Tok::Ident(name) => {
                if self.peek() == &Tok::LParen {
                    self.call(name, t.span)
                } else if self.eat(&Tok::Dot) {
                    // `alias.column`: kept as one dotted name; resolved
                    // against the FROM/JOIN aliases before binding
                    let (col, sp) = self.ident("after '.'")?;
                    Ok(Expr::Column(format!("{name}.{col}"), Span::new(t.span.start, sp.end)))
                } else {
                    Ok(Expr::Column(name, t.span))
                }
            }
            Tok::QuotedIdent(name) => Ok(Expr::Column(name, t.span)),
            other => Err(Diagnostic::new(
                format!("expected an expression, found {}", describe(&other)),
                t.span,
            )),
        }
    }

    fn call(&mut self, name: String, name_span: Span) -> Result<Expr, Diagnostic> {
        self.expect(Tok::LParen, "to open the argument list")?;
        // COUNT(DISTINCT x) sugar -> count_distinct(x)
        let name = if self.eat(&Tok::Distinct) {
            if name != "count" {
                return Err(Diagnostic::new(
                    format!("distinct inside a call is only supported for count(), not {name}()"),
                    name_span,
                ));
            }
            "count_distinct".to_string()
        } else {
            name
        };
        let mut args = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                args.push(self.expr(0)?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let close = self.expect(Tok::RParen, &format!("to close the arguments of {name}("))?;
        Ok(Expr::call(&name, args, name_span.to(close)))
    }

    /// CASE WHEN c THEN v [WHEN c THEN v]* [ELSE e] END
    /// -> case(c1, v1, c2, v2, …, else?) — the binder gives case() its semantics.
    fn case_expr(&mut self, start: Span) -> Result<Expr, Diagnostic> {
        let mut args = Vec::new();
        self.expect(Tok::When, "after 'case'")
            .map_err(|d| d.with_hint("only the searched form is supported: case when <cond> then <value> … end"))?;
        loop {
            args.push(self.expr(0)?);
            self.expect(Tok::Then, "after the 'when' condition")?;
            args.push(self.expr(0)?);
            if !self.eat(&Tok::When) {
                break;
            }
        }
        if self.eat(&Tok::Else) {
            args.push(self.expr(0)?);
        }
        let end = self.expect(Tok::End, "to finish the 'case' expression")?;
        Ok(Expr::call("case", args, start.to(end)))
    }

    /// CAST(x AS type) -> int(x) / float(x) / text(x)
    fn cast_expr(&mut self, start: Span) -> Result<Expr, Diagnostic> {
        self.expect(Tok::LParen, "after 'cast'")?;
        let inner = self.expr(0)?;
        self.expect(Tok::As, "before the target type in cast(… as …)")?;
        let (ty, ty_span) = self.ident("as the cast target type")?;
        let fname = match ty.as_str() {
            "int" | "integer" | "bigint" | "smallint" => "int",
            "float" | "double" | "real" | "numeric" | "decimal" => "float",
            "text" | "varchar" | "string" | "char" => "text",
            other => {
                return Err(Diagnostic::new(
                    format!("unknown cast target type '{other}'"),
                    ty_span,
                )
                .with_hint("supported: int, float, text (and their common aliases)"))
            }
        };
        let end = self.expect(Tok::RParen, "to close the cast")?;
        Ok(Expr::call(fname, vec![inner], start.to(end)))
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Number(n, _) => format!("the number {n}"),
        Tok::Str(s) => format!("the string '{s}'"),
        Tok::Ident(s) => format!("'{s}'"),
        Tok::QuotedIdent(s) => format!("\"{s}\""),
        Tok::Eof => "the end of the query".into(),
        Tok::LParen => "'('".into(),
        Tok::RParen => "')'".into(),
        Tok::Comma => "','".into(),
        Tok::Star => "'*'".into(),
        Tok::Slash => "'/'".into(),
        Tok::Percent => "'%'".into(),
        Tok::Plus => "'+'".into(),
        Tok::Minus => "'-'".into(),
        Tok::Eq => "'='".into(),
        Tok::Ne => "'!='".into(),
        Tok::Lt => "'<'".into(),
        Tok::Le => "'<='".into(),
        Tok::Gt => "'>'".into(),
        Tok::Ge => "'>='".into(),
        Tok::Concat => "'||'".into(),
        kw => format!("the keyword '{kw:?}'").to_lowercase(),
    }
}


/// `a = b [and c = d …]` into key pairs; anything else is not a join condition.
fn flatten_equalities(e: Expr, out: &mut Vec<(Expr, Expr)>) -> Result<(), Diagnostic> {
    match e {
        Expr::Binary { op: BinOp::And, lhs, rhs, .. } => {
            flatten_equalities(*lhs, out)?;
            flatten_equalities(*rhs, out)
        }
        Expr::Binary { op: BinOp::Eq, lhs, rhs, .. }
            if matches!(*lhs, Expr::Column(..)) && matches!(*rhs, Expr::Column(..)) =>
        {
            out.push((*lhs, *rhs));
            Ok(())
        }
        other => Err(Diagnostic::new(
            "a join condition is one or more column equalities: on a.key = b.key [and …]",
            other.span(),
        )
        .with_hint("filters on the joined table belong in WHERE")),
    }
}
