//! The one-shot hash join: dictionary keys with different code orders, dense
//! int keys, plain-text keys, composite keys, LEFT vs INNER, NULL keys,
//! and the errors — checked against hand-derived expectations.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::join::{join, JoinKind, JoinSpec};
use facetful_engine::sql::run_query;
use facetful_engine::Table;

fn dict(words: &[&str]) -> DictData {
    let mut offsets = vec![0u32];
    let mut bytes = Vec::new();
    for w in words {
        bytes.extend_from_slice(w.as_bytes());
        offsets.push(bytes.len() as u32);
    }
    DictData { offsets, bytes }
}

/// facts: 8 rows in two groups — cat (dict eu/us/asia; row 6 NULL), id int,
/// name plain text, v float
fn facts() -> Table<Vec<u8>> {
    let schema = Schema { columns: vec![
        ColumnDef { name: "cat".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
        ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
        ColumnDef { name: "name".into(), ty: ColumnType::Utf8, flags: 0 },
        ColumnDef { name: "v".into(), ty: ColumnType::Float64, flags: 0 },
    ] };
    let dicts = vec![Some(dict(&["eu", "us", "asia"])), None, None, None];
    let mut w = Writer::new(schema, vec![], 4, &dicts);
    let cats: [u8; 8] = [0, 1, 0, 2, 1, 2, 0, 1];
    let ids: [i32; 8] = [10, 20, 10, 30, 99, 30, 20, 20];
    let names = ["Tolk", "Amos", "Bear", "Dolet", "Nowhere", "Dolet", "Tolk", "Amos"];
    let vs: [f64; 8] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    for g in 0..2usize {
        let r = g * 4..g * 4 + 4;
        let d = dict(&names[r.clone()]);
        let id_bytes: Vec<u8> = ids[r.clone()].iter().flat_map(|x| x.to_le_bytes()).collect();
        let v_bytes: Vec<u8> = vs[r.clone()].iter().flat_map(|x| x.to_le_bytes()).collect();
        // group 1 = rows 4..8; row 6 (local 2) is NULL: bits 1101
        let cv: [u8; 1] = [0b0000_1011];
        w.write_group(4, &[
            ColumnChunk {
                data: SegmentData::Codes8(&cats[r.clone()]),
                validity: if g == 1 { Some(&cv) } else { None },
                null_count: if g == 1 { 1 } else { 0 },
            },
            ColumnChunk { data: SegmentData::Fixed(&id_bytes), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Utf8 { offsets: &d.offsets, bytes: &d.bytes }, validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Fixed(&v_bytes), validity: None, null_count: 0 },
        ]);
    }
    Table::open(w.finish()).unwrap()
}

/// dims: cat (dict in a DIFFERENT order: asia, eu, us), id, name plain, label dict, w float
fn dims(dup_key: bool) -> Table<Vec<u8>> {
    let schema = Schema { columns: vec![
        ColumnDef { name: "cat".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
        ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
        ColumnDef { name: "name".into(), ty: ColumnType::Utf8, flags: 0 },
        ColumnDef { name: "label".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
        ColumnDef { name: "w".into(), ty: ColumnType::Float64, flags: 0 },
    ] };
    let dicts = vec![Some(dict(&["asia", "eu", "us"])), None, None, Some(dict(&["A", "B", "C"])), None];
    let mut w = Writer::new(schema, vec![], 8, &dicts);
    let cats: [u8; 3] = [1, 2, 0]; // eu, us, asia in the asia/eu/us dictionary
    let ids: [i32; 3] = [10, 20, if dup_key { 20 } else { 30 }];
    let d = dict(&["Tolk", "Amos", "Dolet"]);
    let labels: [u8; 3] = [0, 1, 2];
    let ws: [f64; 3] = [0.5, 0.25, 0.125];
    let id_bytes: Vec<u8> = ids.iter().flat_map(|x| x.to_le_bytes()).collect();
    let w_bytes: Vec<u8> = ws.iter().flat_map(|x| x.to_le_bytes()).collect();
    w.write_group(3, &[
        ColumnChunk { data: SegmentData::Codes8(&cats), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Fixed(&id_bytes), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Utf8 { offsets: &d.offsets, bytes: &d.bytes }, validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Codes8(&labels), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Fixed(&w_bytes), validity: None, null_count: 0 },
    ]);
    Table::open(w.finish()).unwrap()
}

fn rows(t: &mut Table<Vec<u8>>, sql: &str) -> Vec<Vec<String>> {
    let mut r = run_query(t, sql).unwrap();
    r.ensure_rows();
    r.rows.iter().map(|row| row.iter().map(|v| format!("{v:?}")).collect()).collect()
}

fn spec(keys: &[(&str, &str)], columns: &[&str], kind: JoinKind) -> JoinSpec {
    JoinSpec {
        keys: keys.iter().map(|(l, r)| (l.to_string(), r.to_string())).collect(),
        left_columns: None,
        // an empty list here means "all non-key right columns", as the CLI's default
        columns: if columns.is_empty() { None } else { Some(columns.iter().map(|c| c.to_string()).collect()) },
        renames: Vec::new(),
        kind,
        matched: true,
    }
}

#[test]
fn dictionary_key_translates_codes_and_left_keeps_every_row() {
    let (mut f, mut d) = (facts(), dims(false));
    let img = join(&mut f, &mut d, &spec(&[("cat", "cat")], &["label", "w"], JoinKind::Left), 65_536).unwrap();
    let mut j = Table::open(img).unwrap();
    let names: Vec<&str> = j.catalog().schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["cat", "id", "name", "v", "label", "w", "matched"]);
    assert!(j.catalog().schema.columns[4].is_dict(), "a gathered dictionary column stays a dictionary");
    // eu→A, us→B, asia→C by value, not by code; the NULL-cat row is unmatched
    assert_eq!(
        rows(&mut j, "select cat, label, w, matched from t order by v"),
        vec![
            vec!["Text(\"eu\")", "Text(\"A\")", "Float(0.5)", "Int(1)"],
            vec!["Text(\"us\")", "Text(\"B\")", "Float(0.25)", "Int(1)"],
            vec!["Text(\"eu\")", "Text(\"A\")", "Float(0.5)", "Int(1)"],
            vec!["Text(\"asia\")", "Text(\"C\")", "Float(0.125)", "Int(1)"],
            vec!["Text(\"us\")", "Text(\"B\")", "Float(0.25)", "Int(1)"],
            vec!["Text(\"asia\")", "Text(\"C\")", "Float(0.125)", "Int(1)"],
            vec!["Null", "Null", "Null", "Int(0)"],
            vec!["Text(\"us\")", "Text(\"B\")", "Float(0.25)", "Int(1)"],
        ]
    );
    assert_eq!(rows(&mut j, "select count(*) from t"), vec![vec!["Int(8)"]]);
}

#[test]
fn int_key_dense_lane_inner_drops_unmatched() {
    let (mut f, mut d) = (facts(), dims(false));
    let img = join(&mut f, &mut d, &spec(&[("id", "id")], &["label"], JoinKind::Inner), 65_536).unwrap();
    let mut j = Table::open(img).unwrap();
    // id 99 has no dimension row: gone under INNER; no matched column
    assert_eq!(rows(&mut j, "select count(*) from t"), vec![vec!["Int(7)"]]);
    assert!(!j.catalog().schema.columns.iter().any(|c| c.name == "matched"));
    assert_eq!(
        rows(&mut j, "select id, label, count(*) as n from t group by id, label order by id"),
        vec![
            vec!["Int(10)", "Text(\"A\")", "Int(2)"],
            vec!["Int(20)", "Text(\"B\")", "Int(3)"],
            vec!["Int(30)", "Text(\"C\")", "Int(2)"],
        ]
    );
}

#[test]
fn text_and_composite_keys() {
    let (mut f, mut d) = (facts(), dims(false));
    // plain text on both sides
    let img = join(&mut f, &mut d, &spec(&[("name", "name")], &["w"], JoinKind::Left), 65_536).unwrap();
    let mut j = Table::open(img).unwrap();
    assert_eq!(
        rows(&mut j, "select name, w from t where matched = 1 group by name, w order by name"),
        vec![
            vec!["Text(\"Amos\")", "Float(0.25)"],
            vec!["Text(\"Dolet\")", "Float(0.125)"],
            vec!["Text(\"Tolk\")", "Float(0.5)"],
        ]
    );
    assert_eq!(rows(&mut j, "select count(*) from t where matched = 0"), vec![vec!["Int(2)"]]); // Bear, Nowhere
    // composite (cat, id): (eu,10)x2, (us,20)x3, (asia,30)x2 match; (us,99) and the NULL row don't
    let img = join(&mut f, &mut d, &spec(&[("cat", "cat"), ("id", "id")], &["label"], JoinKind::Left), 65_536).unwrap();
    let mut j = Table::open(img).unwrap();
    assert_eq!(rows(&mut j, "select sum(matched), count(*) from t"), vec![vec!["Int(6)", "Int(8)"]]);
    // the facts as a right side repeat names: rejected as non-unique
    let e = join(&mut d, &mut f, &spec(&[("name", "name")], &["v"], JoinKind::Left), 65_536).unwrap_err();
    assert!(e.contains("not unique"), "{e}");
}

#[test]
fn errors_are_loud() {
    let (mut f, mut d) = (facts(), dims(true));
    let e = join(&mut f, &mut d, &spec(&[("id", "id")], &["label"], JoinKind::Left), 65_536).unwrap_err();
    assert!(e.contains("not unique"), "{e}");
    let mut d = dims(false);
    let e = join(&mut f, &mut d, &spec(&[("cat", "cat")], &["id"], JoinKind::Left), 65_536).unwrap_err();
    assert!(e.contains("both sides"), "{e}");
    let e = join(&mut f, &mut d, &spec(&[("v", "w")], &["label"], JoinKind::Left), 65_536).unwrap_err();
    assert!(e.contains("cannot join on"), "{e}");
    let e = join(&mut f, &mut d, &spec(&[("nope", "cat")], &["label"], JoinKind::Left), 65_536).unwrap_err();
    assert!(e.contains("no column 'nope'"), "{e}");
    // default right columns = every non-key column, which here clash with the left
    let e = join(&mut f, &mut d, &spec(&[("cat", "cat")], &[], JoinKind::Left), 65_536).unwrap_err();
    assert!(e.contains("both sides"), "{e}");
}
