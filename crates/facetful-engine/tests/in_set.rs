//! One-key `IN (select …)` / `EXISTS` and long literal `IN` lists run as a
//! value set (keyset.rs): a per-row lookup cached as the outer table's own
//! conjunct mask. These check what the SQLite differential cannot see — the
//! batch fuses them, the mask is shared across statements and across
//! subqueries with the same members — plus the answers on a plain text key.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::exec::Val;
use facetful_engine::sql::{run_batch_with, run_query_with, TableSet};
use facetful_engine::Table;

const ROWS: usize = 300;

/// facts: id int32 (0..300), cat dict (4 values), name plain text, v int64
fn facts() -> Table<Vec<u8>> {
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
            ColumnDef { name: "cat".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
            ColumnDef { name: "name".into(), ty: ColumnType::Utf8, flags: 0 },
            ColumnDef { name: "v".into(), ty: ColumnType::Int64, flags: 0 },
        ],
    };
    let mut doffs = vec![0u32];
    let mut dbytes = Vec::new();
    for s in ["a", "b", "c", "d"] {
        dbytes.extend_from_slice(s.as_bytes());
        doffs.push(dbytes.len() as u32);
    }
    let mut w = Writer::new(schema, vec![], 64, &[None, Some(DictData { offsets: doffs, bytes: dbytes }), None, None]);
    for start in (0..ROWS).step_by(64) {
        let end = (start + 64).min(ROWS);
        let ids: Vec<u8> = (start..end).flat_map(|i| (i as i32).to_le_bytes()).collect();
        let cats: Vec<u8> = (start..end).map(|i| (i % 4) as u8).collect();
        let mut noffs = vec![0u32];
        let mut nbytes = Vec::new();
        for i in start..end {
            nbytes.extend_from_slice(format!("n{}", i % 50).as_bytes());
            noffs.push(nbytes.len() as u32);
        }
        let vs: Vec<u8> = (start..end).flat_map(|i| ((i * 7 % 11) as i64).to_le_bytes()).collect();
        w.write_group((end - start) as u32, &[
            ColumnChunk { data: SegmentData::Fixed(&ids), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Codes8(&cats), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Utf8 { offsets: &noffs, bytes: &nbytes }, validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Fixed(&vs), validity: None, null_count: 0 },
        ]);
    }
    Table::open(w.finish()).unwrap()
}

/// hits: id int32 and name text, the rows a search matched
fn hits(ids: &[i32]) -> Table<Vec<u8>> {
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
            ColumnDef { name: "name".into(), ty: ColumnType::Utf8, flags: 0 },
        ],
    };
    let mut w = Writer::new(schema, vec![], 1024, &[None, None]);
    let b: Vec<u8> = ids.iter().flat_map(|x| x.to_le_bytes()).collect();
    let mut noffs = vec![0u32];
    let mut nbytes = Vec::new();
    for i in ids {
        nbytes.extend_from_slice(format!("n{}", i % 50).as_bytes());
        noffs.push(nbytes.len() as u32);
    }
    w.write_group(ids.len() as u32, &[
        ColumnChunk { data: SegmentData::Fixed(&b), validity: None, null_count: 0 },
        ColumnChunk { data: SegmentData::Utf8 { offsets: &noffs, bytes: &nbytes }, validity: None, null_count: 0 },
    ]);
    Table::open(w.finish()).unwrap()
}

fn rows(mut r: facetful_engine::sql::exec::QueryResult) -> Vec<Vec<String>> {
    r.ensure_rows();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Val::Null => "NULL".into(),
                    Val::Text(s) => s.to_string(),
                    Val::Int(i) => i.to_string(),
                    Val::Float(f) => format!("{f:.3}"),
                    Val::Bool(b) => b.to_string(),
                })
                .collect()
        })
        .collect()
}

fn q(t: &mut Table<Vec<u8>>, cat: &mut TableSet<Vec<u8>>, sql: &str) -> Vec<Vec<String>> {
    rows(run_query_with(t, sql, cat).unwrap_or_else(|d| panic!("{}", d.render(sql))))
}

#[test]
fn a_search_conjunct_fuses_and_its_mask_is_shared() {
    let mut t = facts();
    let mut cat = TableSet { tables: vec![("hits".into(), hits(&[3, 4, 5, 40, 41, 200, 299]))] };
    let search = "id in (select id from hits where id > 3)";
    let sqls: Vec<String> = vec![
        format!("select cat, count(*) as n, sum(v) as s from t where {search} group by cat order by cat"),
        format!("select v, count(*) as n from t where {search} and cat <> 'b' group by v order by v"),
        format!("select count(*) as n, sum(v) as s from t where {search}"),
    ];
    let refs: Vec<&str> = sqls.iter().map(|s| s.as_str()).collect();
    let batch: Vec<_> = run_batch_with(&mut t, &refs, &mut cat).into_iter().map(|r| rows(r.unwrap())).collect();
    for (sql, b) in refs.iter().zip(&batch) {
        assert_eq!(&q(&mut t, &mut cat, sql), b, "{sql}");
    }
    // ids 4, 5, 40, 41, 200, 299: cats a b a b a d, v = id * 7 % 11 = 6 2 5 1 3 3
    assert_eq!(batch[0], vec![vec!["a", "3", "14"], vec!["b", "2", "3"], vec!["d", "1", "3"]]);
    // a second click: every statement reads the one cached mask; no table is built
    let (derived, misses) = (t.derived_stats().0, t.masks().misses);
    run_batch_with(&mut t, &refs, &mut cat).into_iter().for_each(|r| drop(r.unwrap()));
    assert_eq!(t.derived_stats().0, derived);
    assert_eq!(t.masks().misses, misses, "no conjunct recomputed");
}

#[test]
fn the_mask_is_keyed_by_the_members() {
    let mut t = facts();
    let mut cat = TableSet { tables: vec![("hits".into(), hits(&[7, 8, 9]))] };
    let a = q(&mut t, &mut cat, "select count(*) from t where id in (select id from hits)");
    let misses = t.masks().misses;
    // another subquery, the same members: the same mask
    let b = q(&mut t, &mut cat, "select count(*) from t where id in (select id from hits where id < 100)");
    assert_eq!((a, b.clone()), (vec![vec!["3".to_string()]], b));
    assert_eq!(t.masks().misses, misses);
    // other members: a mask of their own
    q(&mut t, &mut cat, "select count(*) from t where id in (select id from hits where id < 9)");
    assert!(t.masks().misses > misses);
}

#[test]
fn text_keys_and_exists_on_one_key() {
    let mut t = facts();
    let mut cat = TableSet { tables: vec![("hits".into(), hits(&[1, 2, 51]))] };
    // names n1, n2, n1: every row whose name is n1 or n2 (6 each in 300 rows)
    assert_eq!(q(&mut t, &mut cat, "select count(*) from t where name in (select name from hits)"), vec![vec!["12"]]);
    assert_eq!(q(&mut t, &mut cat, "select count(*) from t where name not in (select name from hits)"), vec![vec!["288"]]);
    assert_eq!(q(&mut t, &mut cat, "select count(*) from t where exists (select 1 from hits h where h.name = t.name and h.id > 1)"), vec![vec!["12"]]);
    assert_eq!(q(&mut t, &mut cat, "select count(*) from t where not exists (select 1 from hits h where h.id = t.id)"), vec![vec!["297"]]);
    // a dictionary key against a text set
    assert_eq!(q(&mut t, &mut cat, "select count(*) from t where cat in (select 'b' from hits)"), vec![vec!["75"]]);
}

#[test]
fn long_literal_lists_agree_with_short_ones() {
    let mut t = facts();
    let mut cat = TableSet { tables: vec![] };
    let ids: Vec<String> = (0..40).map(|i| (i * 7).to_string()).collect();
    let names: Vec<String> = (0..20).map(|i| format!("'n{}'", i * 2)).collect();
    let long = |col: &str, items: &[String]| format!("select count(*) from t where {col} in ({})", items.join(", "));
    let ors = |col: &str, items: &[String]| {
        format!("select count(*) from t where {}", items.iter().map(|i| format!("{col} = {i}")).collect::<Vec<_>>().join(" or "))
    };
    assert_eq!(q(&mut t, &mut cat, &long("id", &ids)), q(&mut t, &mut cat, &ors("id", &ids)));
    assert_eq!(q(&mut t, &mut cat, &long("name", &names)), q(&mut t, &mut cat, &ors("name", &names)));
    assert_eq!(q(&mut t, &mut cat, &long("v", &ids)), q(&mut t, &mut cat, &ors("v", &ids)));
    // a NULL in the list: NOT IN is never TRUE
    let mut with_null = ids.clone();
    with_null.push("null".into());
    assert_eq!(q(&mut t, &mut cat, &format!("select count(*) from t where id not in ({})", with_null.join(", "))), vec![vec!["0"]]);
}

#[test]
fn the_set_functions_are_not_sql() {
    let mut t = facts();
    let mut cat = TableSet { tables: vec![] };
    let err = run_query_with(&mut t, "select count(*) from t where in_set(id, 1)", &mut cat).err().unwrap();
    assert!(err.render("").contains("unknown function 'in_set'"), "{}", err.render(""));
}
