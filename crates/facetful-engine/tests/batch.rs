//! A batch (design.sv d59) answers exactly what the statements answer one
//! by one: fused members and ordinary members alike, in order.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::exec::{OutCol, Val};
use facetful_engine::sql::{run_batch, run_query};
use facetful_engine::Table;

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

fn rows_of(mut r: facetful_engine::sql::exec::QueryResult) -> (Vec<String>, Vec<Vec<String>>) {
    let names = r.columns.clone();
    r.ensure_rows();
    let rows = r
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Val::Null => "NULL".to_string(),
                    Val::Text(s) => s.to_string(),
                    Val::Int(i) => i.to_string(),
                    Val::Float(f) => format!("{f:.6}"),
                    Val::Bool(b) => b.to_string(),
                })
                .collect()
        })
        .collect();
    (names, rows)
}

/// Batch results equal single results; `ordered` statements compare in
/// order, the rest as sets (unordered SQL output has no defined order).
fn check(t: &mut Table<Vec<u8>>, sqls: &[&str], ordered: &[bool]) {
    let batch = run_batch(t, sqls);
    for ((sql, br), &ord) in sqls.iter().zip(batch).zip(ordered) {
        let one = run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
        let br = br.unwrap_or_else(|d| panic!("batch: {}", d.render(sql)));
        let (n1, mut r1) = rows_of(one);
        let (n2, mut r2) = rows_of(br);
        assert_eq!(n1, n2, "{sql}: column names");
        if !ord {
            r1.sort();
            r2.sort();
        }
        assert_eq!(r1, r2, "{sql}");
    }
}

#[test]
fn family_matches_single_statements() {
    let mut t = table();
    let sqls = [
        // the facet family: filters-except-own over region/year, count + sum
        "select region, count(*) as n, sum(capacity) as mw from t where year >= 2001 group by region order by n desc, region",
        "select region, count(*) as n from t where year >= 2001 and region <> 'asia' group by region order by region",
        "select region, count(capacity) as k, sum(year) as y from t group by region",
        // totals, with and without a filter
        "select count(*) as n, sum(capacity) as mw from t where region = 'eu' and year < 2004",
        "select count(*) as n, count(capacity) as k, sum(capacity) as mw from t",
        // a window over the groups
        "select region, count(*) as n from t group by region order by n desc, region limit 2 offset 1",
        // NULL keys form their own group; sum over no contributing row is NULL
        "select region, sum(capacity) as mw from t where capacity > 100 or region is null group by region order by region",
        // not fusable: an expression key, a text key without a dictionary path, a projection
        "select upper(region) as r, count(*) as n from t group by upper(region) order by r",
        "select region, min(capacity) as lo from t group by region order by region",
        "select region, year from t where capacity > 5 order by year",
    ];
    let ordered = [true, true, false, true, true, true, true, true, true, true];
    check(&mut t, &sqls, &ordered);
}

#[test]
fn fused_members_keep_dictionary_keys_and_report_errors_in_place() {
    let mut t = table();
    let sqls = ["select region, count(*) as n from t group by region order by region", "select nope from t", "select count(*) as n from t"];
    let out = run_batch(&mut t, &sqls);
    let first = out[0].as_ref().unwrap();
    assert!(matches!(first.cols.as_ref().unwrap()[0], OutCol::Dict { .. }), "the key stays codes + dictionary");
    let err = out[1].as_ref().err().expect("the bad statement fails alone");
    assert!(err.render(sqls[1]).contains("unknown column 'nope'"));
    assert_eq!(out[2].as_ref().unwrap().n_rows(), 1);
}

/// The 200K spike image, when checked out: a real facet interaction's
/// statements through both paths.
#[test]
fn spike_facet_interaction_agrees() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spikes/facet-spike/data-200000.facetful");
    let Ok(bytes) = std::fs::read(path) else { eprintln!("skipping: {path} not present"); return };
    let mut t = Table::open(bytes).unwrap();
    let dims = ["country", "status", "fuel", "region", "owner", "year"];
    let filters = [("status", "'status_0', 'status_1'"), ("fuel", "'fuel_2'"), ("region", "'region_1', 'region_3'")];
    let where_except = |dim: &str| {
        let parts: Vec<String> = filters.iter().filter(|(d, _)| *d != dim).map(|(d, v)| format!("{d} in ({v})")).collect();
        if parts.is_empty() { String::new() } else { format!(" where {}", parts.join(" and ")) }
    };
    let mut sqls: Vec<String> = dims
        .iter()
        .map(|d| format!("select {d}, count(*) as n, sum(capacity) as mw, count(capacity) as k from t{} group by {d} order by n desc, {d} limit 50", where_except(d)))
        .collect();
    sqls.push(format!("select count(*) as n, sum(capacity) as mw from t{}", where_except("")));
    sqls.push("select year, count(*) as n from t where capacity > 250 group by year order by year".into());
    let refs: Vec<&str> = sqls.iter().map(|s| s.as_str()).collect();
    let ordered = vec![true; refs.len()];
    check(&mut t, &refs, &ordered);
}
