//! materialize: a query's result becomes a table that answers the same
//! questions, with types, nulls and sort metadata intact.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::materialize::materialize;
use facetful_engine::sql::run_query;
use facetful_engine::Table;

/// 10 rows over 2 groups: region dict, capacity float (one null), year int16.
fn table() -> Table<Vec<u8>> {
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "region".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
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
    let dicts = vec![Some(DictData { offsets: doff, bytes: dbytes }), None, None];
    let mut w = Writer::new(schema, vec![], 5, &dicts);
    let groups: [(&[u8], &[f64], &[i16], Option<(&[u8], u32)>); 2] = [
        (&[0, 1, 0, 2, 1], &[1.0, 2.0, 3.0, 4.0, 0.0], &[2000, 2001, 2002, 2003, 2004], Some((&[0b0000_1111], 1))),
        (&[0, 1, 0, 2, 0], &[6.0, 7.0, 8.0, 9.0, 10.0], &[2000, 2001, 2002, 2003, 2004], None),
    ];
    for (codes, caps, years, validity) in groups {
        let cap_bytes: Vec<u8> = caps.iter().flat_map(|x| x.to_le_bytes()).collect();
        let year_bytes: Vec<u8> = years.iter().flat_map(|x| x.to_le_bytes()).collect();
        w.write_group(5, &[
            ColumnChunk { data: SegmentData::Codes8(codes), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Fixed(&cap_bytes), validity: validity.map(|(v, _)| v), null_count: validity.map(|(_, n)| n).unwrap_or(0) },
            ColumnChunk { data: SegmentData::Fixed(&year_bytes), validity: None, null_count: 0 },
        ]);
    }
    Table::open(w.finish()).unwrap()
}

fn rows(t: &mut Table<Vec<u8>>, sql: &str) -> Vec<Vec<String>> {
    let mut r = run_query(t, sql).unwrap();
    r.ensure_rows();
    r.rows.iter().map(|row| row.iter().map(|v| format!("{v:?}")).collect()).collect()
}

#[test]
fn grouped_result_becomes_a_typed_sorted_table() {
    let mut t = table();
    let sql = "select region, count(*) as n, sum(capacity) as cap, min(year) as first from t \
               group by region order by cap desc";
    let image = materialize(&mut t, sql, 65_536).unwrap();
    let mut d = Table::open(image).unwrap();
    let cols = &d.catalog().schema.columns;
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["region", "n", "cap", "first"]);
    // three rows, three distinct regions: below the dictionary payoff, so plain text
    assert_eq!((cols[0].ty, cols[0].is_dict()), (ColumnType::Utf8, false));
    assert_eq!(cols[1].ty, ColumnType::Int8, "counts narrow to their range");
    assert_eq!(cols[2].ty, ColumnType::Float64);
    assert_eq!(cols[3].ty, ColumnType::Int16);
    // the ORDER BY key is a selected column: recorded as the table's sort
    let sorted = &d.catalog().sorted_by;
    assert_eq!(sorted.len(), 1);
    assert_eq!((sorted[0].column, sorted[0].descending), (2, true));
    // the derived table answers what the source computed
    assert_eq!(
        rows(&mut d, "select region, n, cap, first from t order by cap desc"),
        rows(&mut t, sql)
    );
    assert_eq!(rows(&mut d, "select count(*) from t"), vec![vec!["Int(3)"]]);
}

#[test]
fn projection_keeps_nulls_rows_and_order() {
    let mut t = table();
    let sql = "select region, capacity, year from t where year >= 2002 order by year, region";
    let image = materialize(&mut t, sql, 3).unwrap(); // small groups: several row groups
    let mut d = Table::open(image).unwrap();
    assert_eq!(rows(&mut d, "select region, capacity, year from t"), rows(&mut t, sql));
    assert_eq!(rows(&mut d, "select count(*) from t"), vec![vec!["Int(6)"]]);
    // the null capacity (us, 2004) survived as a null
    assert_eq!(rows(&mut d, "select count(*) from t where capacity is null"), vec![vec!["Int(1)"]]);
    // a two-key sort prefix of selected columns is recorded whole
    assert_eq!(d.catalog().sorted_by.len(), 2);
}

#[test]
fn empty_result_and_duplicate_names() {
    let mut t = table();
    let image = materialize(&mut t, "select region, year from t where year > 9000", 65_536).unwrap();
    let mut d = Table::open(image).unwrap();
    assert_eq!(rows(&mut d, "select count(*) from t"), vec![vec!["Int(0)"]]);
    let err = materialize(&mut t, "select region, region from t", 65_536).unwrap_err();
    assert!(err.render("").contains("duplicate column name 'region'"), "{}", err.render(""));
}
