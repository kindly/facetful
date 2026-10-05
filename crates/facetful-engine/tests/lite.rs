//! The lite build (no `write` feature): joins, CTEs and subqueries, which
//! all go through derived tables written on the fly, are refused with one
//! clear error; single-table queries run as usual.
#![cfg(not(feature = "write"))]

use facetful_engine::sql::run_query;
use facetful_engine::Table;

#[test]
fn derived_tables_are_refused() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../spikes/facet-spike/data-200000.facetful")).unwrap();
    let mut t = Table::open(bytes).unwrap();
    let mut r = run_query(&mut t, "select country, count(*) as n from t where capacity > 100 group by country order by n desc limit 3").unwrap();
    r.ensure_rows();
    assert_eq!(r.rows.len(), 3);
    for sql in [
        "with x as (select country from t) select count(*) from x",
        "select count(*) from (select country from t where capacity > 100)",
        "select count(*) from t where country in (select country from t where capacity > 290)",
        "select count(*) from t where not exists (select 1 from t u where u.id = t.id and u.capacity > 290)",
        "select count(*) from t join t u on t.id = u.id",
    ] {
        let Err(d) = run_query(&mut t, sql) else { panic!("{sql} ran in the lite build") };
        assert!(d.render(sql).contains("not in this build (lite)"), "{sql}: {}", d.render(sql));
    }
}
