//! JOIN syntax: cached materialization onto the derived cache, qualified
//! names, clash renames, USING, INNER, chained joins, joins to CTEs, and
//! the errors — against the same fixtures as the one-shot join.

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
    let cv: [u8; 1] = [0b1011_1111]; // row 6 NULL cat
    let id_bytes: Vec<u8> = ids.iter().flat_map(|x| x.to_le_bytes()).collect();
    let v_bytes: Vec<u8> = vs.iter().flat_map(|x| x.to_le_bytes()).collect();
    w.write_group(8, &[
        ColumnChunk { data: SegmentData::Codes8(&cats), validity: Some(&cv), null_count: 1 },
        ColumnChunk { data: SegmentData::Fixed(&id_bytes), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Fixed(&v_bytes), validity: None, null_count: 0 },
    ]);
    Table::open(w.finish()).unwrap()
}

/// dims: cat (a different code order), id, label, v (clashes with facts.v)
fn dims() -> Table<Vec<u8>> {
    let schema = Schema { columns: vec![
        ColumnDef { name: "cat".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
        ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
        ColumnDef { name: "label".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
        ColumnDef { name: "v".into(), ty: ColumnType::Float64, flags: 0 },
    ] };
    let dicts = vec![Some(dict(&["asia", "eu", "us"])), None, Some(dict(&["A", "B", "C"])), None];
    let mut w = Writer::new(schema, vec![], 8, &dicts);
    let cats: [u8; 3] = [1, 2, 0];
    let ids: [i32; 3] = [10, 20, 30];
    let labels: [u8; 3] = [0, 1, 2];
    let vs: [f64; 3] = [0.5, 0.25, 0.125];
    let id_bytes: Vec<u8> = ids.iter().flat_map(|x| x.to_le_bytes()).collect();
    let v_bytes: Vec<u8> = vs.iter().flat_map(|x| x.to_le_bytes()).collect();
    w.write_group(3, &[
        ColumnChunk { data: SegmentData::Codes8(&cats), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Fixed(&id_bytes), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Codes8(&labels), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Fixed(&v_bytes), validity: None, null_count: 0 },
    ]);
    Table::open(w.finish()).unwrap()
}

fn q(t: &mut Table<Vec<u8>>, cat: &mut TableSet<Vec<u8>>, sql: &str) -> Vec<Vec<String>> {
    let mut r = run_query_with(t, sql, cat).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    r.rows.iter().map(|row| row.iter().map(|v| format!("{v:?}")).collect()).collect()
}

fn setup() -> (Table<Vec<u8>>, TableSet<Vec<u8>>) {
    (facts(), TableSet { tables: vec![("dims".to_string(), dims())] })
}

#[test]
fn left_join_with_aliases_qualified_names_and_a_clash_rename() {
    let (mut f, mut cat) = setup();
    // d.v clashes with facts.v: reachable as d.v, stored as dims_v… via the alias
    let rows = q(&mut f, &mut cat,
        "select f.cat, d.label, sum(f.v) as total, min(d.v) as w \
         from facts f left join dims d on f.cat = d.cat group by f.cat, d.label order by total desc");
    // eu = rows 0 and 2 (row 6 is the NULL-cat row): 1 + 3
    assert_eq!(rows, vec![
        vec!["Text(\"us\")", "Text(\"B\")", "Float(15.0)", "Float(0.25)"],
        vec!["Text(\"asia\")", "Text(\"C\")", "Float(10.0)", "Float(0.125)"],
        vec!["Null", "Null", "Float(7.0)", "Null"],
        vec!["Text(\"eu\")", "Text(\"A\")", "Float(4.0)", "Float(0.5)"],
    ]);
    // first query materialized, the identical one hits the cache
    assert_eq!(f.derived_stats().0, 1);
    q(&mut f, &mut cat, "select f.cat, d.label, sum(f.v) as total, min(d.v) as w \
         from facts f left join dims d on f.cat = d.cat group by f.cat, d.label order by total desc");
    assert_eq!(f.derived_stats().0, 1, "same join, same columns: one derived table");
    // a different column set is a different derived table
    q(&mut f, &mut cat, "select d.id as did, count(*) as n from facts f left join dims d on f.cat = d.cat group by d.id order by n desc");
    assert_eq!(f.derived_stats().0, 2);
}

#[test]
fn using_inner_unqualified_and_the_matched_flag() {
    let (mut f, mut cat) = setup();
    // USING + INNER: unmatched id 99 and the NULL-cat row (id 20 matches by id) → id 99 gone
    assert_eq!(
        q(&mut f, &mut cat, "select id, label, count(*) as n from t inner join dims using (id) group by id, label order by id"),
        vec![
            vec!["Int(10)", "Text(\"A\")", "Int(2)"],
            vec!["Int(20)", "Text(\"B\")", "Int(3)"],
            vec!["Int(30)", "Text(\"C\")", "Int(2)"],
        ]
    );
    // LEFT keeps every row; bare right names resolve when unambiguous; the
    // anti-join idiom works because a referenced right key is carried
    assert_eq!(
        q(&mut f, &mut cat, "select count(*) as n, count(label) as labelled from t left join dims on t.id = dims.id"),
        vec![vec!["Int(8)", "Int(7)"]]
    );
    assert_eq!(
        q(&mut f, &mut cat, "select id from t left join dims d on t.id = d.id where d.id is null"),
        vec![vec!["Int(99)"]]
    );
}

#[test]
fn chained_joins_and_joins_to_ctes() {
    let (mut f, mut cat) = setup();
    // join to a CTE of the same table, then to the catalog table
    let rows = q(&mut f, &mut cat,
        "with per_cat as (select cat, count(*) as n_cat from t group by cat) \
         select t.cat, p.n_cat, d.label from t \
         left join per_cat p on t.cat = p.cat \
         left join dims d on t.cat = d.cat \
         group by t.cat, p.n_cat, d.label order by t.cat");
    assert_eq!(rows, vec![
        vec!["Null", "Null", "Null"],
        vec!["Text(\"asia\")", "Int(2)", "Text(\"C\")"],
        vec!["Text(\"eu\")", "Int(2)", "Text(\"A\")"],
        vec!["Text(\"us\")", "Int(3)", "Text(\"B\")"],
    ]);
    // join to a subquery
    assert_eq!(
        q(&mut f, &mut cat, "select count(*) as n from t inner join (select id from dims where id >= 20) big on t.id = big.id"),
        vec![vec!["Int(5)"]]
    );
    // select * carries every right column (renamed where it clashes)
    let r = run_query_with(&mut f, "select * from t left join dims d using (cat) limit 1", &mut cat).unwrap();
    assert_eq!(r.columns, ["cat", "id", "v", "d_id", "label", "d_v"]);
}

#[test]
fn errors() {
    let (mut f, mut cat) = setup();
    let err = |sql: &str, f: &mut Table<Vec<u8>>, cat: &mut TableSet<Vec<u8>>| {
        run_query_with(f, sql, cat).err().map(|d| d.render(sql)).unwrap_or_else(|| panic!("expected an error: {sql}"))
    };
    assert!(err("select count(*) from t join nowhere n on t.id = n.id", &mut f, &mut cat).contains("unknown table 'nowhere'"));
    assert!(err("select count(*) from t join dims d on t.id > d.id", &mut f, &mut cat).contains("column equalities"));
    assert!(err("select x.id from t join dims d on t.id = d.id", &mut f, &mut cat).contains("unknown column 'x.id'"));
    assert!(err("select count(*) from t join dims d on t.id = t.id", &mut f, &mut cat).contains("must compare a column of the left side"));
}
