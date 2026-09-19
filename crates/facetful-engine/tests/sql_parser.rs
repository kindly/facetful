//! Parser tests: desugar equivalence (idiom == function spelling), precedence,
//! query skeleton, and exact error-message content (the messages are a product
//! feature — they get tested like one).

use facetful_engine::sql::ast::*;
use facetful_engine::sql::{parse_expr, parse_query};

/// Strip spans so structurally-equal trees compare equal.
fn shape(e: &Expr) -> String {
    match e {
        Expr::Number(n, _, _) => format!("{n}"),
        Expr::Str(s, _) => format!("'{s}'"),
        Expr::Column(c, _) => c.clone(),
        Expr::Null(_) => "null".into(),
        Expr::Star(_) => "*".into(),
        Expr::Unary { op, expr, .. } => format!("({op:?} {})", shape(expr)),
        Expr::Binary { op, lhs, rhs, .. } => format!("({op:?} {} {})", shape(lhs), shape(rhs)),
        Expr::Call { name, args, .. } => {
            let a: Vec<String> = args.iter().map(shape).collect();
            format!("{name}({})", a.join(","))
        }
        Expr::Row(items, _) => {
            let a: Vec<String> = items.iter().map(shape).collect();
            format!("row({})", a.join(","))
        }
        Expr::InSubquery { cols, query, .. } => {
            let a: Vec<String> = cols.iter().map(shape).collect();
            format!("in_subquery(({}) from {})", a.join(","), query.from)
        }
    }
}

#[test]
fn row_values_and_in_subqueries_parse() {
    // literal tuple lists desugar to AND/OR at parse time
    same("(a, b) in ((1, 'x'), (2, 'y'))", "(a = 1 and b = 'x') or (a = 2 and b = 'y')");
    same("(a, b) not in ((1, 'x'))", "not (a = 1 and b = 'x')");
    // subqueries stay a node for the resolver
    let e = parse_expr("(a, b) in (select x, y from d where z > 1)").unwrap();
    assert_eq!(shape(&e), "in_subquery((a,b) from d)");
    let e = parse_expr("a not in (select x from d)").unwrap();
    assert_eq!(shape(&e), "(Not in_subquery((a) from d))");
    let err = parse_expr("(a, b) in ((1, 2, 3))").unwrap_err();
    assert!(err.render("").contains("row has 3 values"));
}

fn same(a: &str, b: &str) {
    let ea = parse_expr(a).unwrap_or_else(|d| panic!("{a}: {}", d.render(a)));
    let eb = parse_expr(b).unwrap_or_else(|d| panic!("{b}: {}", d.render(b)));
    assert_eq!(shape(&ea), shape(&eb), "{a}  vs  {b}");
}

#[test]
fn idioms_desugar_to_function_calls() {
    same("x between 1 and 5", "between(x, 1, 5)");
    same("x not between 1 and 5", "not between(x, 1, 5)");
    same("region in ('EU', 'US')", "in(region, 'EU', 'US')");
    same("region not in ('EU')", "not in(region, 'EU')");
    same("name like '%coal%'", "like(name, '%coal%')");
    same("name not like 'x%'", "not like(name, 'x%')");
    same("capacity is null", "isnull(capacity)");
    same("capacity is not null", "not isnull(capacity)");
    same("a || b || c", "concat(concat(a, b), c)");
    same("cast(x as int)", "int(x)");
    same("cast(x as varchar)", "text(x)");
    same(
        "case when x = 1 then 'one' when x = 2 then 'two' else 'many' end",
        "case(x = 1, 'one', x = 2, 'two', 'many')",
    );
    same("count(distinct owner)", "count_distinct(owner)");
}

#[test]
fn precedence() {
    same("a + b * c", "a + (b * c)");
    same("a = 1 or b = 2 and c = 3", "(a = 1) or ((b = 2) and (c = 3))");
    same("not a = 1", "not (a = 1)");
    same("-a * b", "(-a) * b");
    // BETWEEN's AND doesn't swallow a following conjunction
    same("x between 1 and 5 and y = 2", "between(x, 1, 5) and (y = 2)");
    same("a = 1 and x in ('p') or b = 2", "((a = 1) and in(x, 'p')) or (b = 2)");
}

#[test]
fn query_skeleton() {
    let q = parse_query(
        "select country, sum(capacity) as total, count(*) n \
         from plants \
         where status in ('operating') and year between 2000 and 2020 \
         group by country \
         order by total desc, country \
         limit 50 offset 100",
    )
    .unwrap();
    assert_eq!(q.from, "plants");
    assert_eq!(q.select.len(), 3);
    assert_eq!(q.select[1].alias.as_deref(), Some("total"));
    assert_eq!(q.select[2].alias.as_deref(), Some("n")); // bare alias
    assert!(q.filter.is_some());
    assert_eq!(q.group_by.len(), 1);
    assert_eq!(q.order_by.len(), 2);
    assert_eq!(q.order_by[0].dir, SortDir::Desc);
    assert_eq!(q.order_by[1].dir, SortDir::Asc);
    assert_eq!(q.limit, Some(50));
    assert_eq!(q.offset, Some(100));
}

#[test]
fn quoted_identifiers_escape_keywords() {
    let q = parse_query("select \"between\", \"order\" from t").unwrap();
    assert_eq!(shape(&q.select[0].expr), "between");
    assert_eq!(shape(&q.select[1].expr), "order");
}

#[test]
fn comments_and_case_insensitive_keywords() {
    let q = parse_query("SELECT a FROM t -- trailing comment\nWHERE a > 1").unwrap();
    assert!(q.filter.is_some());
}

// ---------------- error messages are a feature ----------------

fn err(src: &str) -> String {
    match parse_query(src) {
        Err(d) => d.render(src),
        Ok(_) => panic!("expected an error for: {src}"),
    }
}

#[test]
fn error_unclosed_call() {
    let m = err("select sum(capacity from plants");
    assert!(m.contains("to close the arguments of sum("), "{m}");
    assert!(m.contains("^"), "{m}");
}

#[test]
fn error_unterminated_string() {
    let m = err("select * from t where name = 'oops");
    assert!(m.contains("unterminated string"), "{m}");
    assert!(m.contains("''"), "hint should mention quote escaping: {m}");
}

#[test]
fn error_between_missing_and() {
    let m = err("select * from t where x between 1, 5");
    assert!(m.contains("between the bounds of 'between'"), "{m}");
    assert!(m.contains("x between low and high"), "{m}");
}

#[test]
fn error_missing_from() {
    let m = err("select a b c from t"); // b eats alias slot, then c is unexpected
    assert!(m.contains("expected the keyword 'from'"), "{m}");
}

#[test]
fn error_is_without_null() {
    let m = err("select * from t where x is 5");
    assert!(m.contains("only 'is null' / 'is not null'"), "{m}");
}

#[test]
fn error_distinct_outside_count() {
    let m = err("select sum(distinct x) from t");
    assert!(m.contains("only supported for count()"), "{m}");
}

#[test]
fn error_position_points_at_offender() {
    let src = "select a from t where x @ 1";
    let m = err(src);
    assert!(m.contains("line 1, column 25"), "{m}");
}

#[test]
fn exponent_number_literals() {
    let f = |src: &str| match parse_expr(src).unwrap() {
        Expr::Number(n, is_float, _) => (n, is_float),
        e => panic!("not a number: {}", shape(&e)),
    };
    assert_eq!(f("1e6"), (1_000_000.0, true));
    assert_eq!(f("2.5E-3"), (0.0025, true));
    assert_eq!(f("1e+9"), (1e9, true));
    assert_eq!(f("1000000"), (1_000_000.0, false));
    // a bare `e` after digits is not an exponent — it stays a separate token
    let q = parse_query("select 1 e from t").unwrap();
    assert_eq!(q.select[0].alias.as_deref(), Some("e"));
}
