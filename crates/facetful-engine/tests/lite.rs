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

/// Retained results are in the lite build: rows of the table and grouped
/// results page and sort; a join is refused as it is in a query.
#[test]
fn retained_results_page_and_sort() {
    use facetful_engine::sql::{prepare, NoCatalog};
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../spikes/facet-spike/data-200000.facetful")).unwrap();
    let mut t = Table::open(bytes).unwrap();
    let rows = |mut r: facetful_engine::sql::exec::QueryResult| {
        r.ensure_rows();
        r.rows.iter().map(|row| format!("{row:?}")).collect::<Vec<_>>()
    };
    let mut h = prepare(&mut t, "select id, country, capacity from t where capacity > 100").unwrap();
    assert!(h.is_rows() && h.has_row_ids());
    let n = h.rows();
    let got = rows(h.page(&mut t, n - 700, n + 10, None).unwrap());
    let want = rows(run_query(&mut t, &format!("select id, country, capacity from t where capacity > 100 limit 700 offset {}", n - 700)).unwrap());
    assert_eq!(got, want);
    h.sort(&mut t, "capacity desc, id, country", &mut NoCatalog).unwrap();
    let got = rows(h.page(&mut t, n / 2, n / 2 + 512, None).unwrap());
    let want = rows(run_query(&mut t, &format!("select id, country, capacity from t where capacity > 100 order by capacity desc, id, country limit 512 offset {}", n / 2)).unwrap());
    assert_eq!(got.len(), 512);
    // rows equal in all three keys are whole duplicates, so any tie order reads the same
    assert_eq!(got, want);
    let h = prepare(&mut t, "select country, count(*) as n from t group by country order by n desc, country").unwrap();
    assert!(!h.is_rows());
    let got = rows(h.page(&mut t, 5, 15, None).unwrap());
    let want = rows(run_query(&mut t, "select country, count(*) as n from t group by country order by n desc, country limit 10 offset 5").unwrap());
    assert_eq!(got, want);
    let sql = "select count(*) from t join t u on t.id = u.id";
    let Err(d) = prepare(&mut t, sql) else { panic!("{sql} was prepared in the lite build") };
    assert!(d.render(sql).contains("not in this build (lite)"));
}
