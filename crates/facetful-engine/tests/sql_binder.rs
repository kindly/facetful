//! Binder tests: resolution, did-you-mean, arity/type errors, aggregate rules.

use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::binder::{Binder, Ty};
use facetful_engine::sql::parse_query;

fn schema() -> Schema {
    Schema {
        columns: vec![
            ColumnDef { name: "country".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY },
            ColumnDef { name: "status".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY },
            ColumnDef { name: "capacity".into(), ty: ColumnType::Float64, flags: 0 },
            ColumnDef { name: "year".into(), ty: ColumnType::Int16, flags: 0 },
        ],
    }
}

fn bind(src: &str) -> Result<facetful_engine::sql::binder::BoundQuery, String> {
    let q = parse_query(src).map_err(|d| d.render(src))?;
    let s = schema();
    Binder::new(&s).bind_query(&q).map_err(|d| d.render(src))
}

#[test]
fn resolves_and_types() {
    let b = bind("select country, sum(capacity) as total from t where year between 2000 and 2020 group by country").unwrap();
    assert!(b.is_aggregate);
    assert_eq!(b.select.len(), 2);
    assert_eq!(b.select[0].name, "country");
    assert_eq!(b.select[1].name, "total");
    assert!(b.select[1].aggregated);
    assert_eq!(b.select[1].expr.ty(), Ty::Float);
}

#[test]
fn count_star_and_order_alias_and_position() {
    let b = bind("select country, count(*) as n from t group by country order by n desc, 1").unwrap();
    assert_eq!(b.order_by.len(), 2);
    assert_eq!(b.order_by[0].0, b.select[1].expr);
    assert_eq!(b.order_by[1].0, b.select[0].expr);
}

#[test]
fn did_you_mean_column() {
    let m = bind("select contry from t").unwrap_err();
    assert!(m.contains("unknown column 'contry'"), "{m}");
    assert!(m.contains("did you mean 'country'?"), "{m}");
}

#[test]
fn did_you_mean_function() {
    let m = bind("select cont(*) from t").unwrap_err();
    assert!(m.contains("unknown function 'cont'"), "{m}");
    assert!(m.contains("did you mean 'count()'?"), "{m}");
}

#[test]
fn arity_error() {
    let m = bind("select between(capacity, 1) from t").unwrap_err();
    assert!(m.contains("between() takes 3 argument(s), got 2"), "{m}");
}

#[test]
fn type_errors() {
    let m = bind("select sum(country) from t").unwrap_err();
    assert!(m.contains("sum() needs a number"), "{m}");
    let m = bind("select capacity + country from t").unwrap_err();
    assert!(m.contains("arithmetic needs numbers"), "{m}");
    assert!(m.contains("concat()"), "hint expected: {m}");
    let m = bind("select * from t where capacity").unwrap_err();
    assert!(m.contains("WHERE needs a boolean condition"), "{m}");
}

#[test]
fn aggregate_rules() {
    let m = bind("select country from t where sum(capacity) > 5").unwrap_err();
    assert!(m.contains("not allowed in WHERE"), "{m}");
    let m = bind("select country, capacity from t group by country").unwrap_err();
    assert!(m.contains("'capacity' must appear in GROUP BY"), "{m}");
    let m = bind("select sum(sum(capacity)) from t").unwrap_err();
    assert!(m.contains("cannot be nested"), "{m}");
}

#[test]
fn group_by_alias_and_position() {
    let b = bind("select lower(country) as c, count(*) as n from t group by c").unwrap();
    assert_eq!(b.group_by.len(), 1);
    assert_eq!(b.group_by[0], b.select[0].expr);

    let b = bind("select lower(country) as c, count(*) as n from t group by 1").unwrap();
    assert_eq!(b.group_by[0], b.select[0].expr);

    // a name that is both a select alias and a table column binds as the column
    let b = bind("select status as country, count(*) from t group by country").unwrap_err();
    assert!(b.contains("'country' must appear in GROUP BY"), "{b}");

    let m = bind("select country, count(*) as n from t group by n").unwrap_err();
    assert!(m.contains("cannot GROUP BY 'n': it is an aggregate"), "{m}");
    let m = bind("select country, count(*) from t group by 3").unwrap_err();
    assert!(m.contains("GROUP BY position 3 is out of range (1..=2)"), "{m}");
}
