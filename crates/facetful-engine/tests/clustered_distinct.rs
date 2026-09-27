//! `count(distinct)` over a clustered column (every value one run in file
//! order) counts runs instead of hashing. It must answer exactly what the
//! hash and bitmap paths answer: the same image with its layout trailer cut
//! off reads as unclustered and is the reference.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema, MAGIC};
use facetful_engine::sql::exec::Val;
use facetful_engine::sql::{run_batch, run_query};
use facetful_engine::Table;

const ROWS: usize = 700;
const GROUP: usize = 64;

fn utf8(values: &[&str]) -> DictData {
    let mut offsets = vec![0u32];
    let mut bytes = Vec::new();
    for s in values {
        bytes.extend_from_slice(s.as_bytes());
        offsets.push(bytes.len() as u32);
    }
    DictData { offsets, bytes }
}

/// Columns: `act` int32, clustered but not sorted, runs crossing row groups,
/// NULLs inside and between runs; `publisher` dict, clustered (the outer
/// key); `region` dict and `year` int16, not clustered; `usd` float.
fn image() -> Vec<u8> {
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "act".into(), ty: ColumnType::Int32, flags: 0 },
            ColumnDef { name: "publisher".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
            ColumnDef { name: "region".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
            ColumnDef { name: "year".into(), ty: ColumnType::Int16, flags: 0 },
            ColumnDef { name: "usd".into(), ty: ColumnType::Float64, flags: 0 },
        ],
    };
    let dicts = vec![None, Some(utf8(&["p0", "p1", "p2", "p3", "p4"])), Some(utf8(&["eu", "us", "asia", "af"])), None, None];

    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = |m: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % m
    };
    // runs of `act`, with ids in a scrambled order so the column is clustered
    // without being monotone; publisher changes every 30 runs
    let (mut act, mut act_ok, mut publisher, mut region, mut year, mut usd) = (vec![], vec![], vec![], vec![], vec![], vec![]);
    let mut run = 0i32;
    while act.len() < ROWS {
        let id = (run * 7919) % 1000 - 300;
        let p = (run / 30).min(4) as u8;
        for _ in 0..1 + rnd(9) {
            act.push(id);
            act_ok.push(rnd(10) != 0);
            publisher.push(p);
            region.push(rnd(4) as u8);
            year.push(2000 + rnd(6) as i16);
            usd.push(rnd(1000) as f64);
        }
        run += 1;
        // a stretch of NULL ids between runs
        if run % 13 == 0 {
            for _ in 0..3 {
                act.push(0);
                act_ok.push(false);
                publisher.push(p);
                region.push(rnd(4) as u8);
                year.push(2000 + rnd(6) as i16);
                usd.push(rnd(1000) as f64);
            }
        }
    }
    act.truncate(ROWS);

    let mut w = Writer::new(schema, vec![], GROUP as u32, &dicts);
    for start in (0..ROWS).step_by(GROUP) {
        let end = (start + GROUP).min(ROWS);
        let n = end - start;
        let mut valid = vec![0u8; n.div_ceil(8)];
        let mut nulls = 0;
        for i in 0..n {
            if act_ok[start + i] {
                valid[i / 8] |= 1 << (i % 8);
            } else {
                nulls += 1;
            }
        }
        let act_b: Vec<u8> = act[start..end].iter().flat_map(|x| x.to_le_bytes()).collect();
        let year_b: Vec<u8> = year[start..end].iter().flat_map(|x| x.to_le_bytes()).collect();
        let usd_b: Vec<u8> = usd[start..end].iter().flat_map(|x| x.to_le_bytes()).collect();
        w.write_group(n as u32, &[
            ColumnChunk {
                data: SegmentData::Fixed(&act_b),
                validity: (nulls > 0).then_some(valid.as_slice()),
                null_count: nulls,
            },
            ColumnChunk { data: SegmentData::Codes8(&publisher[start..end]), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Codes8(&region[start..end]), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Fixed(&year_b), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Fixed(&usd_b), validity: None, null_count: 0 },
        ]);
    }
    w.finish()
}

/// The same image as a file written before the layout trailer existed.
fn without_trailer(file: &[u8], ncols: usize) -> Vec<u8> {
    let n = file.len();
    let footer_len = u32::from_le_bytes(file[n - 8..n - 4].try_into().unwrap()) as usize;
    let cut = 2 + ncols;
    let mut out = file[..n - 8 - cut].to_vec();
    out.extend_from_slice(&((footer_len - cut) as u32).to_le_bytes());
    out.extend_from_slice(&MAGIC);
    out
}

fn rows(mut r: facetful_engine::sql::exec::QueryResult) -> Vec<Vec<String>> {
    r.ensure_rows();
    let mut out: Vec<Vec<String>> = r
        .rows
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
        .collect();
    out.sort();
    out
}

#[test]
fn layout_trailer_marks_the_clustered_columns() {
    let file = image();
    let t = Table::open(file.clone()).unwrap();
    assert_eq!(t.catalog().clustered, vec![true, true, false, false, false]);
    let old = Table::open(without_trailer(&file, 5)).unwrap();
    assert_eq!(old.catalog().clustered, vec![false; 5]);
}

#[test]
fn run_count_matches_the_hash_count() {
    let file = image();
    let mut runs = Table::open(file.clone()).unwrap();
    let mut hashed = Table::open(without_trailer(&file, 5)).unwrap();
    let sqls = [
        "select count(distinct act) as n from t",
        "select count(distinct act) as n, count(distinct publisher) as p from t where year >= 2003",
        "select region, sum(usd) as usd, count(distinct act) as n from t group by region order by usd desc limit 3",
        "select year, count(distinct act) as n, count(distinct publisher) as p from t where region in ('eu', 'af') group by year order by year",
        "select publisher, count(distinct act) as n from t where usd > 500 group by publisher order by publisher",
        "select region, year, count(distinct act) as n from t where act is not null group by region, year",
        "select count(distinct act) as n from t where act > 100 and region <> 'us'",
        "select region, count(distinct publisher) as p from t group by region",
        // no row kept: the count is 0, not NULL
        "select count(distinct act) as n from t where usd > 5000",
    ];
    for sql in sqls {
        let want = rows(run_query(&mut hashed, sql).unwrap_or_else(|d| panic!("{}", d.render(sql))));
        let got = rows(run_query(&mut runs, sql).unwrap_or_else(|d| panic!("{}", d.render(sql))));
        assert_eq!(got, want, "{sql}");
    }
    // the fused batch, over the same statements
    let want: Vec<_> = run_batch(&mut hashed, &sqls).into_iter().map(|r| rows(r.unwrap())).collect();
    let got: Vec<_> = run_batch(&mut runs, &sqls).into_iter().map(|r| rows(r.unwrap())).collect();
    for ((sql, g), w) in sqls.iter().zip(got).zip(want) {
        assert_eq!(g, w, "batch: {sql}");
    }
}

/// A column clustered within each row group but repeating a value across
/// groups is not clustered, and the count stays exact.
#[test]
fn a_value_back_after_its_run_ended_is_not_clustered() {
    let schema = Schema { columns: vec![ColumnDef { name: "k".into(), ty: ColumnType::Int64, flags: 0 }] };
    let mut w = Writer::new(schema, vec![], 4, &[None]);
    for vals in [[1i64, 1, 2, 2], [3, 3, 1, 1]] {
        let b: Vec<u8> = vals.iter().flat_map(|x| x.to_le_bytes()).collect();
        w.write_group(4, &[ColumnChunk { data: SegmentData::Fixed(&b), validity: None, null_count: 0 }]);
    }
    let mut t = Table::open(w.finish()).unwrap();
    assert_eq!(t.catalog().clustered, vec![false]);
    let sql = "select count(distinct k) as n from t";
    assert_eq!(rows(run_query(&mut t, sql).unwrap()), vec![vec!["3".to_string()]]);
}
