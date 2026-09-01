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
    let r = run_query(&mut t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
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
