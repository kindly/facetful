//! End-to-end SQL tests: parse -> bind -> execute against an in-code table.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::exec::Val;
use facetful_engine::sql::run_query;
use facetful_engine::Table;

/// 10 rows over 2 groups: region dict, capacity float (one null), year int16.
/// rows: (region, capacity, year)
///  eu 1.0 2000 | us 2.0 2001 | eu 3.0 2002 | asia 4.0 2003 | us NULL 2004
///  eu 6.0 2000 | us 7.0 2001 | eu 8.0 2002 | asia 9.0 2003 | eu 10.0 2004
fn table() -> Table<Vec<u8>> {
    let schema = Schema {
        columns: vec![
            ColumnDef {
                name: "region".into(),
                ty: ColumnType::Utf8,
                flags: flags::DICTIONARY | flags::CODES_U8,
            },
            ColumnDef { name: "capacity".into(), ty: ColumnType::Float64, flags: 0 },
            ColumnDef { name: "year".into(), ty: ColumnType::Int16, flags: 0 },
        ],
    };
    let (doff, dbytes) = {
        let mut offs = vec![0u32];
        let mut bytes = Vec::new();
        for s in ["eu", "us", "asia"] {
            bytes.extend_from_slice(s.as_bytes());
            offs.push(bytes.len() as u32);
        }
        (offs, bytes)
    };
    let dicts =
        vec![Some(DictData { offsets: doff, bytes: dbytes }), None, None];
    let mut w = Writer::new(schema, vec![], 5, &dicts);

    let groups: [(&[u8], &[f64], &[i16], Option<(&[u8], u32)>); 2] = [
        (&[0, 1, 0, 2, 1], &[1.0, 2.0, 3.0, 4.0, 0.0], &[2000, 2001, 2002, 2003, 2004], Some((&[0b0000_1111], 1))),
        (&[0, 1, 0, 2, 0], &[6.0, 7.0, 8.0, 9.0, 10.0], &[2000, 2001, 2002, 2003, 2004], None),
    ];
    for (codes, caps, years, validity) in groups {
        let cap_bytes: Vec<u8> = caps.iter().flat_map(|x| x.to_le_bytes()).collect();
        let year_bytes: Vec<u8> = years.iter().flat_map(|x| x.to_le_bytes()).collect();
        w.write_group(
            5,
            &[
                ColumnChunk { data: SegmentData::Codes8(codes), validity: None, null_count: 0 },
                ColumnChunk {
                    data: SegmentData::Fixed(&cap_bytes),
                    validity: validity.map(|(v, _)| v),
                    null_count: validity.map(|(_, n)| n).unwrap_or(0),
                },
                ColumnChunk { data: SegmentData::Fixed(&year_bytes), validity: None, null_count: 0 },
            ],
        );
    }
    Table::open(w.finish()).unwrap()
}

fn q(sql: &str) -> Vec<Vec<String>> {
    let mut t = table();
    let mut r = run_query(&mut t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Val::Null => "NULL".into(),
                    Val::Bool(b) => b.to_string(),
                    Val::Int(i) => i.to_string(),
                    Val::Float(f) => format!("{f:.1}"),
                    Val::Text(s) => s.to_string(),
                })
                .collect()
        })
        .collect()
}

#[test]
fn group_by_with_null_skipping_sum() {
    let rows = q("select region, sum(capacity) as total, count(*) as n \
                  from t group by region order by total desc");
    assert_eq!(
        rows,
        vec![
            vec!["eu", "28.0", "5"],       // 1+3+6+8+10
            vec!["asia", "13.0", "2"],     // 4+9
            vec!["us", "9.0", "3"],        // 2+7, NULL skipped; count(*) still 3
        ]
    );
}

#[test]
fn where_idioms_and_projection() {
    let rows = q("select region, capacity from t \
                  where region in ('eu', 'us') and year between 2001 and 2002 \
                  order by capacity");
    assert_eq!(
        rows,
        vec![
            vec!["us", "2.0"],
            vec!["eu", "3.0"],
            vec!["us", "7.0"],
            vec!["eu", "8.0"],
        ]
    );
}

#[test]
fn null_comparisons_exclude_rows() {
    // capacity > 0 is NULL for the null row -> excluded; isnull finds it
    let rows = q("select count(*) from t where capacity > 0");
    assert_eq!(rows, vec![vec!["9"]]);
    let rows = q("select year from t where capacity is null");
    assert_eq!(rows, vec![vec!["2004"]]);
}

#[test]
fn aggregates_without_group_by() {
    let rows = q("select count(*), count(capacity), avg(capacity), min(year), max(year) from t");
    assert_eq!(rows, vec![vec!["10", "9", "5.6", "2000", "2004"]]); // avg over 9 non-null: 50/9? -> 50/9=5.55..
}

#[test]
fn count_distinct_and_case() {
    let rows = q("select count(distinct region), \
                  sum(case when region = 'eu' then 1 else 0 end) as eus from t");
    assert_eq!(rows, vec![vec!["3", "5"]]);
}

#[test]
fn scalar_functions_and_expressions() {
    let rows = q("select upper(region) || '-' || text(year) as tag from t \
                  where like(region, 'e%') and year = 2000 order by 1");
    assert_eq!(rows, vec![vec!["EU-2000"], vec!["EU-2000"]]);
}

#[test]
fn limit_offset_and_empty_aggregate() {
    let rows = q("select year from t order by year desc, capacity desc limit 3 offset 1");
    assert_eq!(rows, vec![vec!["2004"], vec!["2003"], vec!["2003"]]);
    let rows = q("select count(*), sum(capacity) from t where year > 3000");
    assert_eq!(rows, vec![vec!["0", "NULL"]]); // empty aggregate: count 0, sum NULL
}

#[test]
fn expression_over_aggregates() {
    let rows = q("select sum(capacity) / count(capacity) as manual_avg from t where region = 'eu'");
    assert_eq!(rows, vec![vec!["5.6"]]); // 28/5
}

#[test]
fn integer_division_truncates_like_sqlite() {
    let rows = q("select 7 / 2, 7.0 / 2, 7 / 0 from t limit 1");
    assert_eq!(rows, vec![vec!["3", "3.5", "NULL"]]);
}

#[test]
fn minmax_pruning_skips_groups_and_keeps_results() {
    // year lives in [2000, 2004] in BOTH groups (no pruning there), but a
    // year > 9000 filter prunes everything
    let mut t = table();
    let mut r = facetful_engine::sql::run_query(&mut t, "select count(*) from t where year > 9000").unwrap();
    r.ensure_rows();
    assert_eq!(r.scanned_groups, 0);
    assert_eq!(r.total_groups, 2);
    match &r.rows[0][0] {
        Val::Int(0) => {}
        v => panic!("expected 0, got {v:?}"),
    }
    // equality inside the range scans, and answers match the unpruned truth
    let mut r = facetful_engine::sql::run_query(&mut t, "select count(*) from t where year = 2001").unwrap();
    r.ensure_rows();
    assert_eq!(r.scanned_groups, 2);
    match &r.rows[0][0] {
        Val::Int(2) => {}
        v => panic!("expected 2, got {v:?}"),
    }
}

#[test]
fn like_fast_paths_match_general_matcher() {
    // plain-text (non-dict) column exercising every LIKE shape
    let schema = facetful_engine::format::Schema {
        columns: vec![
            ColumnDef { name: "note".into(), ty: ColumnType::Utf8, flags: 0 },
            ColumnDef { name: "id".into(), ty: ColumnType::Int8, flags: 0 },
        ],
    };
    let notes = ["Solar plant operating", "coal RETIRED early", "wind permit review",
        "operations resumed", "solar expansion", "gas peaker", "SOLAR", ""];
    let (mut offs, mut bytes) = (vec![0u32], Vec::new());
    for n in &notes {
        bytes.extend_from_slice(n.as_bytes());
        offs.push(bytes.len() as u32);
    }
    let ids: Vec<u8> = (0..notes.len() as i8).map(|i| i as u8).collect();
    let mut w = Writer::new(schema, vec![], 8, &[None, None]);
    w.write_group(
        notes.len() as u32,
        &[
            ColumnChunk {
                data: SegmentData::Utf8 { offsets: &offs, bytes: &bytes },
                validity: None,
                null_count: 0,
            },
            ColumnChunk { data: SegmentData::Fixed(&ids), validity: None, null_count: 0 },
        ],
    );
    let file = w.finish();
    let mut t = Table::open(file).unwrap();

    let count = |t: &mut Table<Vec<u8>>, pat: &str| -> i64 {
        let mut r = facetful_engine::sql::run_query(
            t,
            &format!("select count(*) from t where note like '{pat}'"),
        )
        .unwrap();
        r.ensure_rows();
        match r.rows[0][0] {
            Val::Int(n) => n,
            _ => panic!(),
        }
    };
    assert_eq!(count(&mut t, "%solar%"), 3); // contains, case-insensitive
    assert_eq!(count(&mut t, "solar%"), 3);  // prefix: Solar plant…, solar expansion, SOLAR
    assert_eq!(count(&mut t, "%review"), 1); // suffix
    assert_eq!(count(&mut t, "solar"), 1);   // exact (SOLAR)
    assert_eq!(count(&mut t, "%oper_ting%"), 1); // general path: underscore wildcard
    assert_eq!(count(&mut t, "%"), 8);       // degenerate: matches everything incl empty
}

#[test]
fn text_scalar_batch() {
    let rows = q("select trim('  x  ') as a, ltrim('xxay', 'x') as b, rtrim('ayxx', 'x') as c, \
                  replace('banana', 'a', 'o') as d, replace('abc', '', 'z') as e, \
                  instr('hello', 'll') as f, instr('hello', 'z') as g \
                  from t limit 1");
    assert_eq!(rows, vec![vec!["x", "ay", "ay", "bonono", "abc", "3", "0"]]);
}

#[test]
fn numeric_scalar_batch() {
    let rows = q("select sign(0 - 5) as a, sign(0) as b, sign(3.2) as c, \
                  sqrt(9) as d, pow(2, 10) as e, ln(1) as f, exp(0) as g, \
                  sqrt(0 - 1) as h, ln(0) as i \
                  from t limit 1");
    assert_eq!(
        rows,
        vec![vec!["-1", "0", "1", "3.0", "1024.0", "0.0", "1.0", "NULL", "NULL"]]
    );
}

#[test]
fn nullif_and_ifnull() {
    let rows = q("select nullif(1, 1) as a, nullif(2, 3) as b, \
                  ifnull(capacity, 0.0) as c, nullif(region, 'us') as d \
                  from t where year = 2004 and capacity is null");
    assert_eq!(rows, vec![vec!["NULL", "2", "0.0", "NULL"]]);
}

#[test]
fn median_and_stddev() {
    // capacity: [1,2,3,4,6,7,8,9,10] valid; NULL skipped
    let rows = q("select median(capacity) as m, stddev(capacity) as s from t");
    assert_eq!(rows, vec![vec!["6.0", "3.2"]]);
    let rows = q("select region, median(capacity) as m from t group by region order by region");
    assert_eq!(
        rows,
        vec![vec!["asia", "6.5"], vec!["eu", "6.0"], vec!["us", "4.5"]] // even counts average
    );
    // n < 2 -> NULL; two equal values -> 0
    let rows = q("select stddev(capacity) as a from t where capacity = 1");
    assert_eq!(rows, vec![vec!["NULL"]]);
    let rows = q("select stddev(year) as a from t where year = 2000");
    assert_eq!(rows, vec![vec!["0.0"]]);
}

#[test]
fn group_concat_orders_and_skips_nulls() {
    let rows = q("select group_concat(region) as g from t");
    assert_eq!(rows, vec![vec!["eu,us,eu,asia,us,eu,us,eu,asia,eu"]]);
    let rows =
        q("select region, group_concat(year, '-') as g from t group by region order by region");
    assert_eq!(
        rows,
        vec![
            vec!["asia", "2003-2003"],
            vec!["eu", "2000-2002-2000-2002-2004"],
            vec!["us", "2001-2004-2001"],
        ]
    );
    // NULL capacity contributes nothing (row 5 is us/NULL)
    let rows = q("select group_concat(capacity) as g from t where region = 'us'");
    assert_eq!(rows, vec![vec!["2,7"]]);
}

#[test]
fn group_concat_separator_must_be_literal() {
    let mut t = table();
    let Err(err) =
        facetful_engine::sql::run_query(&mut t, "select group_concat(region, year) from t")
    else {
        panic!("expected a bind error");
    };
    assert!(err.render("").contains("separator must be a text literal"), "{}", err.render(""));
}

// ---------------- temporal columns ----------------

/// 6 rows: d (Date, one null), ts (Timestamp), n (int)
fn temporal_table() -> Table<Vec<u8>> {
    use facetful_engine::format::compile::{compile, InCol};
    use facetful_engine::format::time::{days_from_civil, MS_PER_DAY};
    let d = |y, m, dd| days_from_civil(y, m, dd) as i32;
    let days = vec![d(2020, 1, 15), d(2020, 3, 1), d(2021, 12, 31), 0, d(2021, 1, 1), d(2020, 1, 15)];
    let valid = Some(vec![true, true, true, false, true, true]);
    let ts: Vec<i64> = days
        .iter()
        .enumerate()
        .map(|(i, &dd)| dd as i64 * MS_PER_DAY + (i as i64) * 3_661_000) // +1h1m1s steps
        .collect();
    let (bytes, _) = compile(
        &["d".into(), "ts".into(), "n".into()],
        vec![
            InCol::Date { v: days, valid },
            InCol::Timestamp { v: ts, valid: None },
            InCol::Int { v: vec![1, 2, 3, 4, 5, 6], valid: None },
        ],
        4, // two groups
    )
    .unwrap();
    Table::open(bytes).unwrap()
}

fn tq(sql: &str) -> Vec<Vec<String>> {
    let mut t = temporal_table();
    let mut r = run_query(&mut t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Val::Null => "NULL".into(),
                    Val::Bool(b) => b.to_string(),
                    Val::Int(i) => i.to_string(),
                    Val::Float(f) => format!("{f:.1}"),
                    Val::Text(s) => s.to_string(),
                })
                .collect()
        })
        .collect()
}

#[test]
fn temporal_extraction_and_grouping() {
    let rows = tq("select year(d) as y, count(*) as c from t where d is not null \
                   group by year(d) order by y");
    assert_eq!(rows, vec![vec!["2020", "3"], vec!["2021", "2"]]);
    let rows = tq("select month(d), day(d) from t where n = 2");
    assert_eq!(rows, vec![vec!["3", "1"]]);
    // timestamp parts: row n=3 has ts offset 2h2m2s past midnight
    let rows = tq("select hour(ts), minute(ts), second(ts) from t where n = 3");
    assert_eq!(rows, vec![vec!["2", "2", "2"]]);
    // year() works on timestamps too
    let rows = tq("select count(*) from t where year(ts) = 2020");
    assert_eq!(rows, vec![vec!["3"]]);
}

#[test]
fn temporal_literals_compare_and_prune() {
    let rows = tq("select n from t where d >= date('2021-01-01') order by n");
    assert_eq!(rows, vec![vec!["3"], vec!["5"]]);
    let rows = tq("select n from t where d = date('2020-01-15') order by n");
    assert_eq!(rows, vec![vec!["1"], vec!["6"]]);
    // between sugar over dates
    let rows = tq("select count(*) from t where d between date('2020-01-01') and date('2020-12-31')");
    assert_eq!(rows, vec![vec!["3"]]);
    // null date excluded everywhere
    let rows = tq("select count(d), count(*) from t");
    assert_eq!(rows, vec![vec!["5", "6"]]);
}

#[test]
fn temporal_formatting_and_aggregates() {
    let rows = tq("select strftime('%Y-%m-%d', d) from t where n = 1");
    assert_eq!(rows, vec![vec!["2020-01-15"]]);
    let rows = tq("select strftime('%Y-%m-%d %H:%M:%S', ts) from t where n = 3");
    assert_eq!(rows, vec![vec!["2021-12-31 02:02:02"]]);
    // min/max keep temporal typing (Val stays Int; col ty checked below)
    let mut t = temporal_table();
    let r = run_query(&mut t, "select min(d) as lo, max(ts) as hi from t").unwrap();
    use facetful_engine::sql::binder::Ty;
    assert_eq!(r.col_types, vec![Ty::Date, Ty::Timestamp]);
    // year over an aggregate (grouped-context temporal dispatch)
    let rows = tq("select year(min(d)) from t");
    assert_eq!(rows, vec![vec!["2020"]]);
    // constructors from raw ints and text
    let rows = tq("select year(date(18276)), year(timestamp('2021-06-01 12:00')) from t limit 1");
    assert_eq!(rows, vec![vec!["2020", "2021"]]);
}

#[test]
fn temporal_type_errors() {
    let mut t = temporal_table();
    for (sql, needle) in [
        ("select year(n) from t", "needs a date or timestamp"),
        ("select hour(d) from t", "needs a timestamp"),
        ("select strftime(d, d) from t", "format string"),
    ] {
        let Err(e) = run_query(&mut t, sql) else { panic!("expected error: {sql}") };
        assert!(e.render(sql).contains(needle), "{sql}: {}", e.render(sql));
    }
}

#[test]
fn select_star_expands_to_all_columns() {
    let mut t = table();
    let mut r = run_query(&mut t, "select * from t where year = 2004 order by region").unwrap();
    r.ensure_rows();
    assert_eq!(r.columns, vec!["region", "capacity", "year"]);
    assert_eq!(r.rows.len(), 2);
    // star combined with expressions, SQLite-style
    let mut r = run_query(&mut t, "select *, capacity * 2 as dbl from t where capacity = 10").unwrap();
    r.ensure_rows();
    assert_eq!(r.columns, vec!["region", "capacity", "year", "dbl"]);
    match &r.rows[0][3] {
        Val::Float(f) => assert_eq!(*f, 20.0),
        v => panic!("expected 20.0, got {v:?}"),
    }
    // star still respects GROUP BY validation
    let Err(err) = run_query(&mut t, "select * from t group by region") else {
        panic!("expected GROUP BY error");
    };
    assert!(err.render("").contains("GROUP BY"), "{}", err.render(""));
    // and stays illegal outside the select list
    let Err(err) = run_query(&mut t, "select count(*) from t where *") else {
        panic!("expected star-position error");
    };
    assert!(err.render("").contains("select list"), "{}", err.render(""));
}

#[test]
fn group_by_alias_and_exponent_literal() {
    // GROUP BY through a select alias, filter written with an exponent literal
    let rows = q("select upper(region) as r, count(*) as n from t where capacity < 1e2 group by r order by r");
    let plain = q("select upper(region) as r, count(*) as n from t where capacity < 100 group by upper(region) order by r");
    assert_eq!(rows, plain);
    assert_eq!(rows[0][0], "ASIA");
}

// ---------------- mask cache ----------------

/// 10 rows over 2 groups with a plain (non-dict) text column and a null.
///  notes: "coal plant" | "Gas turbine" | "COAL and gas" | NULL | "wind farm"
///         "Coastal wind" | "coal" | "solar" | "gas coal" | "hydro"
///  year: 2000..2004 in each group
fn notes_table() -> Table<Vec<u8>> {
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "notes".into(), ty: ColumnType::Utf8, flags: 0 },
            ColumnDef { name: "year".into(), ty: ColumnType::Int16, flags: 0 },
        ],
    };
    let mut w = Writer::new(schema, vec![], 5, &[None, None]);
    let groups: [(&[&str], Option<(&[u8], u32)>); 2] = [
        (&["coal plant", "Gas turbine", "COAL and gas", "", "wind farm"], Some((&[0b0001_0111], 1))),
        (&["Coastal wind", "coal", "solar", "gas coal", "hydro"], None),
    ];
    for (strs, validity) in groups {
        let mut offs = vec![0u32];
        let mut bytes = Vec::new();
        for s in strs {
            bytes.extend_from_slice(s.as_bytes());
            offs.push(bytes.len() as u32);
        }
        let years: Vec<u8> = (2000i16..2005).flat_map(|y| y.to_le_bytes()).collect();
        w.write_group(
            5,
            &[
                ColumnChunk {
                    data: SegmentData::Utf8 { offsets: &offs, bytes: &bytes },
                    validity: validity.map(|(v, _)| v),
                    null_count: validity.map(|(_, n)| n).unwrap_or(0),
                },
                ColumnChunk { data: SegmentData::Fixed(&years), validity: None, null_count: 0 },
            ],
        );
    }
    Table::open(w.finish()).unwrap()
}

fn qt(t: &mut Table<Vec<u8>>, sql: &str) -> Vec<Vec<String>> {
    let mut r = run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Val::Null => "NULL".into(),
                    Val::Bool(b) => b.to_string(),
                    Val::Int(i) => i.to_string(),
                    Val::Float(f) => format!("{f:.1}"),
                    Val::Text(s) => s.to_string(),
                })
                .collect()
        })
        .collect()
}

#[test]
fn mask_cache_hits_on_repeat_and_composes_per_conjunct() {
    let mut t = notes_table();
    let sql = "select count(*) from t where notes like '%coal%' and year > 2000";
    assert_eq!(qt(&mut t, sql), vec![vec!["3"]]); // COAL and gas, coal, gas coal
    let (hits0, misses0) = (t.masks().hits, t.masks().misses);
    assert_eq!((hits0, misses0), (0, 4), "2 conjuncts x 2 groups, all misses");
    assert_eq!(t.masks().stats().0, 2);

    // identical filter: every conjunct/group is a hit, no filter columns touched
    assert_eq!(qt(&mut t, sql), vec![vec!["3"]]);
    assert_eq!((t.masks().hits, t.masks().misses), (4, 4));

    // reordered conjuncts + one new: the shared ones hit, only `year < 2004` misses
    let sql2 = "select count(*) from t where year > 2000 and year < 2004 and notes like '%coal%'";
    assert_eq!(qt(&mut t, sql2), vec![vec!["3"]]); // coal (2001), COAL and gas (2002), gas coal (2003)
    assert_eq!((t.masks().hits, t.masks().misses), (8, 6));
    assert_eq!(t.masks().stats().0, 3);
}

#[test]
fn mask_cache_like_narrowing_matches_full_scan() {
    let mut t = notes_table();
    // prefix chain as typed: co -> coa -> coal; each narrows through the last.
    // Compared against a cold table per needle (a plain blob scan).
    for needle in ["co", "coa", "coal", "COAL"] {
        let mut fresh = notes_table();
        let sql = format!("select year from t where notes like '%{needle}%' order by year, notes");
        assert_eq!(qt(&mut t, &sql), qt(&mut fresh, &sql), "{needle}");
    }
    // 'co' scanned; 'coa' and 'coal' narrowed (2 groups each); 'COAL' binds to
    // the same lowercase needle and hits the cache outright
    assert_eq!(t.masks().narrowed, 4);
    assert_eq!(qt(&mut t, "select count(*) from t where notes like '%coal%'"), vec![vec!["4"]]);
    // null note never matches on either path
    assert_eq!(qt(&mut t, "select count(*) from t where notes like '%%'"), vec![vec!["9"]]);
    // NOT LIKE is its own conjunct: nulls excluded, not the complement of a mask
    assert_eq!(qt(&mut t, "select count(*) from t where notes not like '%coal%'"), vec![vec!["5"]]);
}

#[test]
fn mask_cache_survives_eviction() {
    let mut t = notes_table();
    t.masks().set_budget(1); // one 1-byte bitmap at a time
    for _ in 0..2 {
        assert_eq!(qt(&mut t, "select count(*) from t where notes like '%gas%' and year > 2000"), vec![vec!["3"]]);
        assert_eq!(qt(&mut t, "select count(*) from t where year > 2000"), vec![vec!["8"]]);
    }
    assert!(t.masks().stats().1 <= 1, "budget respected: {:?}", t.masks().stats());
}
