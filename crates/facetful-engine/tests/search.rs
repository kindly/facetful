//! search('q', col, …) against a brute-force reading of what it means:
//! every word of q starts a word (folded, split as the index splits) in one
//! of the columns. Checked on both paths, scanning and the word index, cold,
//! cached, while typing (each word narrowed from the shorter one cached
//! before it), outside a WHERE conjunct, and with a facet.
#![cfg(feature = "search")]

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::exec::Val;
use facetful_engine::search_index::{for_each_word, SearchIndex};
use facetful_engine::sql::run_query;
use facetful_engine::Table;

const ROWS: usize = 500;
const TITLES: [&str; 9] = [
    "Mental health support",
    "Youth sport in London",
    "MENTAL HEALTH and wellbeing",
    "Climate change adaptation",
    "São Paulo youth project",
    "Community sport fund",
    "münchen health centre",
    "",
    "London climate youth",
];
const ORGS: [&str; 5] = ["Sport England", "Mind", "Climate Trust", "Youth London", "Health Board"];
const DESCS: [&str; 7] = [
    "Funding for mental-health services in London",
    "A youth club",
    "Sporting chance for young people",
    "Adaptation to climate change in SÃO PAULO",
    "health and sport",
    "general support",
    "MÜNCHEN partnership",
];

/// title plain text (every 13th row NULL), org dict, descr plain text; groups of 64
fn table() -> Table<Vec<u8>> {
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "title".into(), ty: ColumnType::Utf8, flags: 0 },
            ColumnDef { name: "org".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
            ColumnDef { name: "descr".into(), ty: ColumnType::Utf8, flags: 0 },
        ],
    };
    let mut doffs = vec![0u32];
    let mut dbytes = Vec::new();
    for s in ORGS {
        dbytes.extend_from_slice(s.as_bytes());
        doffs.push(dbytes.len() as u32);
    }
    let mut w = Writer::new(schema, vec![], 64, &[None, Some(DictData { offsets: doffs, bytes: dbytes }), None]);
    for start in (0..ROWS).step_by(64) {
        let end = (start + 64).min(ROWS);
        let text = |pick: &dyn Fn(usize) -> &'static str| {
            let mut offs = vec![0u32];
            let mut bytes = Vec::new();
            for i in start..end {
                bytes.extend_from_slice(pick(i).as_bytes());
                offs.push(bytes.len() as u32);
            }
            (offs, bytes)
        };
        let (toffs, tbytes) = text(&|i| if i % 13 == 0 { "" } else { TITLES[i % TITLES.len()] });
        let mut tvalid = vec![0u8; (end - start).div_ceil(8)];
        let mut tnull = 0;
        for i in start..end {
            if i % 13 != 0 {
                tvalid[(i - start) / 8] |= 1 << ((i - start) % 8);
            } else {
                tnull += 1;
            }
        }
        let (doffs, dbytes) = text(&|i| DESCS[(i * 3) % DESCS.len()]);
        let codes: Vec<u8> = (start..end).map(|i| ((i * 7) % ORGS.len()) as u8).collect();
        w.write_group((end - start) as u32, &[
            ColumnChunk { data: SegmentData::Utf8 { offsets: &toffs, bytes: &tbytes }, validity: Some(&tvalid), null_count: tnull },
            ColumnChunk { data: SegmentData::Codes8(&codes), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Utf8 { offsets: &doffs, bytes: &dbytes }, validity: None, null_count: 0 },
        ]);
    }
    Table::open(w.finish()).unwrap()
}

fn count(t: &mut Table<Vec<u8>>, sql: &str) -> i64 {
    let mut r = run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    match r.rows[0][0] {
        Val::Int(n) => n,
        ref v => panic!("{sql}: {v:?}"),
    }
}

const COLS: &str = "title, org, descr";

/// Row i's texts in the searched columns (title is NULL every 13th row).
fn texts(i: usize) -> Vec<&'static str> {
    let mut v = vec![ORGS[(i * 7) % ORGS.len()], DESCS[(i * 3) % DESCS.len()]];
    if i % 13 != 0 {
        v.push(TITLES[i % TITLES.len()]);
    }
    v
}

fn words_of(s: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for_each_word(s.as_bytes(), &mut buf, |w| out.push(w.to_vec()));
    out
}

/// How many rows search(q, title, org, descr) should match, and which:
/// every finished word as a whole word, the last word as a word prefix
/// unless the query ends in a separator.
fn expected(q: &str, only: impl Fn(usize) -> bool) -> (i64, Vec<bool>) {
    let qw = words_of(q);
    let open = q.chars().next_back().is_some_and(|c| c.is_alphanumeric());
    let hits: Vec<bool> = (0..ROWS)
        .map(|i| {
            let row: Vec<Vec<u8>> = texts(i).iter().flat_map(|t| words_of(t)).collect();
            only(i)
                && qw.iter().enumerate().all(|(k, w)| {
                    let prefix = open && k + 1 == qw.len();
                    row.iter().any(|r| if prefix { r.starts_with(w) } else { r == w })
                })
        })
        .collect();
    (hits.iter().filter(|&&h| h).count() as i64, hits)
}

const QUERIES: [&str; 22] = [
    "sport ",
    "sporting",
    "sport club",
    "fund ",
    "fund",
    "men ",
    "mental health ",
    "health heal",
    "mental health",
    "Youth London",
    "sport",
    "climate CHANGE adaptation",
    "são paulo",
    "SÃO",
    "münchen",
    "climate change ",
    "youth-sport",
    "ment",
    "tal",
    "zzz",
    "o",
    "",
];

/// Both paths: the scan, then the word index over the same columns.
fn both_paths(mut f: impl FnMut(&mut Table<Vec<u8>>, &str)) {
    let mut t = table();
    f(&mut t, "scan");
    let mut t = table();
    t.build_search_index(&[0, 1, 2]).unwrap();
    f(&mut t, "index");
}

#[test]
fn matches_word_starts_cold_and_cached() {
    both_paths(|t, path| {
        for cached in [false, true] {
            t.masks().set_budget(if cached { 1 << 20 } else { 0 });
            for q in QUERIES {
                let want = expected(q, |_| true).0;
                let sql = format!("select count(*) from t where search('{q}', {COLS})");
                assert_eq!(count(t, &sql), want, "{path}: search('{q}') cached={cached}");
                assert_eq!(count(t, &sql), want, "{path}: search('{q}') again");
            }
        }
    });
    // word starts, not substrings: `ment` is in `mental`, `tal` starts nothing
    assert!(expected("ment", |_| true).0 > 0);
    assert_eq!(expected("tal", |_| true).0, 0);
    // a finished word is whole: `sport ` is not `sporting`, `fund ` not `funding`
    assert!(expected("sport ", |_| true).0 < expected("sport", |_| true).0);
    assert!(expected("fund ", |_| true).0 < expected("fund", |_| true).0);
    assert_eq!(expected("men ", |_| true).0, 0);
}

#[test]
fn typing_narrows_word_by_word() {
    both_paths(|t, path| {
        t.masks().set_budget(1 << 20);
        for q in ["mental health", "youth sport london", "climate change", "são paulo", "münchen"] {
            let chars: Vec<char> = q.chars().collect();
            for i in 1..=chars.len() {
                let typed: String = chars[..i].iter().collect();
                let got = count(t, &format!("select count(*) from t where search('{typed}', {COLS})"));
                assert_eq!(got, expected(&typed, |_| true).0, "{path}: typing '{typed}'");
            }
        }
        if path == "scan" {
            assert!(t.masks().narrowed > 0, "later keystrokes narrow from the cached shorter word");
        }
    });
}

#[test]
fn outside_a_where_conjunct() {
    let mut t = table();
    for q in QUERIES {
        let want = expected(q, |_| true).0;
        let as_value = count(&mut t, &format!("select sum(case when search('{q}', {COLS}) then 1 else 0 end) from t"));
        let under_or = count(&mut t, &format!("select count(*) from t where search('{q}', {COLS}) or 1 = 0"));
        assert_eq!((as_value, under_or), (want, want), "search('{q}')");
    }
}

#[test]
fn facets_combine() {
    both_paths(|t, path| {
        t.masks().set_budget(1 << 20);
        let mind = |i: usize| ORGS[(i * 7) % ORGS.len()] == "Mind";
        let got = count(t, &format!("select count(*) from t where search('health', {COLS}) and org = 'Mind'"));
        assert_eq!(got, expected("health", mind).0, "{path}");
    });
}

#[test]
fn index_only_for_its_own_columns() {
    // an index over title alone must not answer a search over title and descr
    let mut t = table();
    t.build_search_index(&[0]).unwrap();
    let got = count(&mut t, &format!("select count(*) from t where search('funding', {COLS})"));
    assert_eq!(got, expected("funding", |_| true).0);
    assert!(got > 0);
}

#[test]
fn bind_errors() {
    let mut t = table();
    for (sql, msg) in [
        ("select count(*) from t where search(title, title)", "query must be a text literal"),
        ("select count(*) from t where search('x', 'y')", "searches text columns"),
        ("select count(*) from t where search('x')", "at least 2"),
    ] {
        let Err(d) = run_query(&mut t, sql) else { panic!("{sql} should not bind") };
        assert!(d.render(sql).contains(msg), "{sql}: {}", d.render(sql));
    }
}

#[test]
fn index_rows_and_pairs() {
    let mut t = table();
    let idx = SearchIndex::build(&mut t, &[0, 1, 2]).unwrap();
    for prefix in ["m", "men", "mental", "health", "he", "sport", "s", "são", "münchen", "lon", "zzz", "2", "climate"] {
        let ids = idx.prefix_ids(prefix.as_bytes());
        let mut got = Vec::new();
        for g in 0..t.group_count() {
            let rows = t.catalog().groups[g].row_count as usize;
            let mut m = vec![0u8; rows];
            idx.mark_group(&mut t, g, ids.clone(), &idx.value_sets(ids.clone()), &mut m).unwrap();
            got.extend(m.iter().map(|&b| b == 1));
        }
        assert_eq!(got, expected(prefix, |_| true).1, "prefix '{prefix}'");
    }
    assert!(idx.prefix_ids(b"ntal").is_empty());
    assert!(idx.word_id(b"london").is_some() && idx.word_id(b"lond").is_none());
    // the row lists hold the plain columns (title, descr); org is a
    // dictionary column, indexed by value
    let pairs: usize = (0..ROWS)
        .map(|i| {
            let mut w: Vec<Vec<u8>> = texts(i)[1..].iter().flat_map(|t| words_of(t)).collect();
            w.sort();
            w.dedup();
            w.len()
        })
        .sum();
    assert_eq!(idx.stats().pairs, pairs);
    let value_pairs: usize = ORGS.iter().map(|o| {
        let mut w = words_of(o);
        w.sort();
        w.dedup();
        w.len()
    }).sum();
    assert_eq!(idx.stats().value_pairs, value_pairs);
}

