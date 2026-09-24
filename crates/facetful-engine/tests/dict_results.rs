//! A dictionary column selected as-is stays codes + dictionary through the
//! columnar channel on every projection path, expands correctly on demand,
//! compacts to the entries in use, and feeds the materializer without strings.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::materialize::materialize;
use facetful_engine::sql::exec::{compact_dict, OutCol, Val};
use facetful_engine::sql::run_query;
use facetful_engine::Table;
use std::rc::Rc;

/// 10 rows over 2 groups: region dict (one NULL), capacity float (one null), year int16.
///  eu 1.0 2000 | us 2.0 2001 | eu 3.0 2002 | asia 4.0 2003 | us NULL 2004
///  eu 6.0 2000 | us 7.0 2001 | NULL 8.0 2002 | asia 9.0 2003 | eu 10.0 2004
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
    let groups: [(&[u8], Option<(&[u8], u32)>, &[f64], Option<(&[u8], u32)>, &[i16]); 2] = [
        (&[0, 1, 0, 2, 1], None, &[1.0, 2.0, 3.0, 4.0, 0.0], Some((&[0b0000_1111], 1)), &[2000, 2001, 2002, 2003, 2004]),
        (&[0, 1, 0, 2, 0], Some((&[0b0001_1011], 1)), &[6.0, 7.0, 8.0, 9.0, 10.0], None, &[2000, 2001, 2002, 2003, 2004]),
    ];
    for (codes, rvalid, caps, cvalid, years) in groups {
        let cap_bytes: Vec<u8> = caps.iter().flat_map(|x| x.to_le_bytes()).collect();
        let year_bytes: Vec<u8> = years.iter().flat_map(|x| x.to_le_bytes()).collect();
        w.write_group(5, &[
            ColumnChunk { data: SegmentData::Codes8(codes), validity: rvalid.map(|(v, _)| v), null_count: rvalid.map(|(_, n)| n).unwrap_or(0) },
            ColumnChunk { data: SegmentData::Fixed(&cap_bytes), validity: cvalid.map(|(v, _)| v), null_count: cvalid.map(|(_, n)| n).unwrap_or(0) },
            ColumnChunk { data: SegmentData::Fixed(&year_bytes), validity: None, null_count: 0 },
        ]);
    }
    Table::open(w.finish()).unwrap()
}

fn text_rows(t: &mut Table<Vec<u8>>, sql: &str) -> Vec<String> {
    let mut r = run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    r.rows
        .iter()
        .map(|row| match &row[0] {
            Val::Null => "NULL".to_string(),
            Val::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        })
        .collect()
}

fn dict_col(t: &mut Table<Vec<u8>>, sql: &str) -> (Vec<u16>, Vec<u8>, Vec<String>) {
    let r = run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    let cols = r.cols.as_ref().expect("columnar channel");
    match &cols[0] {
        OutCol::Dict { codes, dict, valid } => {
            (codes.clone(), valid.clone(), dict.iter().map(|s| s.to_string()).collect())
        }
        _ => panic!("{sql}: first column is not OutCol::Dict"),
    }
}

#[test]
fn plain_sorted_and_grouped_paths_keep_codes() {
    let mut t = table();
    // plain projection: codes copied straight from the groups, validity kept
    let (codes, valid, dict) = dict_col(&mut t, "select region, year from t where capacity is not null");
    assert_eq!(dict, ["eu", "us", "asia"]);
    assert_eq!(codes, [0, 1, 0, 2, 0, 1, 0, 2, 0]);
    assert_eq!(valid, [0b1011_1111, 0b0000_0001], "row 6 (group 2 row 2) is the NULL region");
    assert_eq!(
        text_rows(&mut t, "select region from t where capacity is not null"),
        ["eu", "us", "eu", "asia", "eu", "us", "NULL", "asia", "eu"]
    );
    // full sort (ORDER BY, no LIMIT): the same codes in key order
    let (codes, _, _) = dict_col(&mut t, "select region from t order by year, capacity");
    assert_eq!(codes[..4], [0, 0, 1, 1], "years 2000,2000,2001,2001");
    assert_eq!(text_rows(&mut t, "select region from t order by year, capacity"),
        ["eu", "eu", "us", "us", "eu", "NULL", "asia", "asia", "us", "eu"]);
    // aggregate: a selected GROUP BY key comes off the group table's codes
    let (codes, valid, _) = dict_col(&mut t, "select region, count(*) as n from t group by region order by n desc");
    assert_eq!(codes[..3], [0, 1, 2], "eu 4, us 3, asia 2 — then the NULL group");
    assert_eq!(valid[0] & 0b1000, 0, "the NULL group's key is NULL");
    assert_eq!(text_rows(&mut t, "select region from t group by region order by count(*) desc"),
        ["eu", "us", "asia", "NULL"]);
    // a computed text expression is still per-row text
    let r = run_query(&mut t, "select region || '!' as x from t").unwrap();
    assert!(matches!(r.cols.as_ref().unwrap()[0], OutCol::Text { .. }));
}

#[test]
fn compact_drops_unused_entries_and_ignores_null_codes() {
    let dict: Vec<Rc<String>> = ["a", "b", "c", "d"].iter().map(|s| Rc::new(s.to_string())).collect();
    // rows: d, a, d, (NULL with an out-of-range code), a
    let (codes, used) = compact_dict(&[3, 0, 3, 9, 0], &dict, &[0b0001_0111], 5);
    assert_eq!(used.iter().map(|s| s.as_str()).collect::<Vec<_>>(), ["a", "d"], "dictionary order, only entries in use");
    assert_eq!(codes, [1, 0, 1, 0, 0], "NULL rows read as code 0");
}

#[test]
fn materialize_feeds_codes_to_the_compiler() {
    let mut t = table();
    // 7 rows, 2 distinct: the dictionary pays; it holds only the entries used
    let image = materialize(&mut t, "select region, year from t where region <> 'asia'", 65_536).unwrap();
    let mut d = Table::open(image).unwrap();
    assert!(d.catalog().schema.columns[0].is_dict());
    assert_eq!(d.dictionary(0).unwrap(), ["eu", "us"]);
    assert_eq!(text_rows(&mut d, "select region from t"), text_rows(&mut t, "select region from t where region <> 'asia'"));
    // 2 rows over a 3-entry dictionary: the compiler's payoff test says plain text
    let image = materialize(&mut t, "select region from t where region = 'asia'", 65_536).unwrap();
    let mut d = Table::open(image).unwrap();
    assert!(!d.catalog().schema.columns[0].is_dict());
    assert_eq!(text_rows(&mut d, "select region from t"), ["asia", "asia"]);
    // NULLs survive the code path
    let image = materialize(&mut t, "select region from t where year = 2002 or year = 2000", 65_536).unwrap();
    let mut d = Table::open(image).unwrap();
    assert_eq!(text_rows(&mut d, "select region from t"), ["eu", "eu", "eu", "NULL"]);
}
