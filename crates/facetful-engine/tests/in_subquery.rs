//! IN (select …), row values, NOT IN with NULLs, literal tuple lists —
//! exact SQL three-valued semantics, over the join fixtures.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::{run_query_with, TableSet};
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

/// facts: cat (row 6 NULL), id, v — 8 rows
fn facts() -> Table<Vec<u8>> {
    let schema = Schema { columns: vec![
        ColumnDef { name: "cat".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
        ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
        ColumnDef { name: "v".into(), ty: ColumnType::Float64, flags: 0 },
    ] };
    let dicts = vec![Some(dict(&["eu", "us", "asia"])), None, None];
    let mut w = Writer::new(schema, vec![], 8, &dicts);
    let cats: [u8; 8] = [0, 1, 0, 2, 1, 2, 0, 1];
    let ids: [i32; 8] = [10, 20, 10, 30, 99, 30, 20, 20];
    let vs: [f64; 8] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    let cv: [u8; 1] = [0b1011_1111];
    let id_bytes: Vec<u8> = ids.iter().flat_map(|x| x.to_le_bytes()).collect();
    let v_bytes: Vec<u8> = vs.iter().flat_map(|x| x.to_le_bytes()).collect();
    w.write_group(8, &[
        ColumnChunk { data: SegmentData::Codes8(&cats), validity: Some(&cv), null_count: 1 },
        ColumnChunk { data: SegmentData::Fixed(&id_bytes), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Fixed(&v_bytes), validity: None, null_count: 0 },
    ]);
    Table::open(w.finish()).unwrap()
}

/// dims: cat, id, label; `nulls`: one extra row with a NULL cat and id 40
fn dims(with_null: bool) -> Table<Vec<u8>> {
    let schema = Schema { columns: vec![
        ColumnDef { name: "cat".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
        ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
        ColumnDef { name: "label".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
    ] };
    let dicts = vec![Some(dict(&["asia", "eu", "us"])), None, Some(dict(&["A", "B", "C", "N"]))];
    let n = if with_null { 4 } else { 3 };
    let mut w = Writer::new(schema, vec![], 8, &dicts);
    let cats: Vec<u8> = vec![1, 2, 0, 0][..n].to_vec();
    let ids: Vec<i32> = vec![10, 20, 30, 40][..n].to_vec();
    let labels: Vec<u8> = vec![0, 1, 2, 3][..n].to_vec();
    let cv: [u8; 1] = [0b0000_0111];
    let id_bytes: Vec<u8> = ids.iter().flat_map(|x| x.to_le_bytes()).collect();
    w.write_group(n as u32, &[
        ColumnChunk { data: SegmentData::Codes8(&cats), validity: if with_null { Some(&cv) } else { None }, null_count: if with_null { 1 } else { 0 } },
        ColumnChunk { data: SegmentData::Fixed(&id_bytes), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Codes8(&labels), validity: None, null_count: 0 },
    ]);
    Table::open(w.finish()).unwrap()
}

fn q(t: &mut Table<Vec<u8>>, cat: &mut TableSet<Vec<u8>>, sql: &str) -> Vec<Vec<String>> {
    let mut r = run_query_with(t, sql, cat).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    r.rows.iter().map(|row| row.iter().map(|v| format!("{v:?}")).collect()).collect()
}

#[test]
fn scalar_and_row_value_in_subquery() {
    let mut f = facts();
    let mut cat = TableSet { tables: vec![("dims".to_string(), dims(false))] };
    // cats present in dims with id >= 20: us, asia → rows 1,3,4,5,7 (row 6 NULL cat never matches)
    assert_eq!(q(&mut f, &mut cat, "select count(*) from t where cat in (select cat from dims where id >= 20)"), vec![vec!["Int(5)"]]);
    // row value: (eu,10)x2, (us,20)x3 [rows 1,7 — row 6 is NULL], (asia,30)x2
    assert_eq!(q(&mut f, &mut cat, "select count(*) from t where (cat, id) in (select cat, id from dims)"), vec![vec!["Int(6)"]]);
    // the same over a CTE, combined with another predicate
    assert_eq!(
        q(&mut f, &mut cat, "with big as (select id from dims where id > 15) select id, count(*) as n from t where id in (select id from big) and v > 2 group by id order by id"),
        vec![vec!["Int(20)", "Int(2)"], vec!["Int(30)", "Int(2)"]]
    );
    // the derived key set and the join are cached: a rerun adds no tables
    let before = f.derived_stats().0;
    q(&mut f, &mut cat, "select count(*) from t where (cat, id) in (select cat, id from dims)");
    assert_eq!(f.derived_stats().0, before);
}

#[test]
fn not_in_follows_three_valued_logic() {
    let mut f = facts();
    let mut cat = TableSet { tables: vec![("dims".to_string(), dims(false)), ("dimsn".to_string(), dims(true))] };
    // NOT IN: ids not in {10,20,30} → 99; a NULL outer key is excluded (row 6 has id 20 anyway)
    assert_eq!(q(&mut f, &mut cat, "select id from t where id not in (select id from dims)"), vec![vec!["Int(99)"]]);
    // cats not in {eu,us,asia}: none — and the NULL-cat row is NOT returned
    assert_eq!(q(&mut f, &mut cat, "select count(*) from t where cat not in (select cat from dims)"), vec![vec!["Int(0)"]]);
    // the inner set holds a NULL: NOT IN is never true
    assert_eq!(q(&mut f, &mut cat, "select count(*) from t where cat not in (select cat from dimsn)"), vec![vec!["Int(0)"]]);
    // … while IN still matches the non-NULL members
    assert_eq!(q(&mut f, &mut cat, "select count(*) from t where cat in (select cat from dimsn)"), vec![vec!["Int(7)"]]);
}

#[test]
fn literal_row_lists_desugar() {
    let mut f = facts();
    let mut cat = TableSet { tables: vec![] };
    assert_eq!(q(&mut f, &mut cat, "select count(*) from t where (cat, id) in (('eu', 10), ('asia', 30))"), vec![vec!["Int(4)"]]);
    // rows 1, 4, 7 (us) and row 6: its cat is NULL but its id 20 differs from
    // both tuples, so the row comparison is FALSE, not unknown — NOT IN keeps it
    assert_eq!(q(&mut f, &mut cat, "select count(*) from t where (cat, id) not in (('eu', 10), ('asia', 30))"), vec![vec!["Int(4)"]]);
    let err = run_query_with(&mut f, "select count(*) from t where (cat, id) in (('eu', 10, 1))", &mut cat).err().unwrap();
    assert!(err.render("").contains("row has 3 values"));
    let err = run_query_with(&mut f, "select (cat, id) from t", &mut cat).err().unwrap();
    assert!(err.render("").contains("only valid on the left of IN"));
}
