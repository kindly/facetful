//! search('q', col, …) against the LIKE expression it stands for: every word
//! of q (case-folded, split on non-alphanumerics) as a contains-LIKE, ORed
//! over the columns and ANDed over the words. Checked cold, while typing
//! (each word's mask narrowed from the shorter one cached before it), and
//! outside a WHERE conjunct, where search() goes through LIKE row by row.
#![cfg(feature = "search")]

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::exec::Val;
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

/// title plain text (every 13th row NULL), org dict, desc plain text; groups of 64
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

/// The LIKE expression search(q, title, org, descr) stands for.
fn like_form(q: &str) -> String {
    let words: Vec<String> = q
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("(title like '%{w}%' or org like '%{w}%' or descr like '%{w}%')"))
        .collect();
    if words.is_empty() { "1 = 1".into() } else { words.join(" and ") }
}

const QUERIES: [&str; 12] = [
    "mental health",
    "Youth London",
    "sport",
    "climate CHANGE adaptation",
    "são paulo",
    "SÃO",
    "münchen",
    "health heal",
    "youth-sport",
    "zzz",
    "o",
    "",
];

#[test]
fn matches_the_like_form_cold_and_cached() {
    let mut t = table();
    for cached in [false, true] {
        t.masks().set_budget(if cached { 1 << 20 } else { 0 });
        for q in QUERIES {
            let want = count(&mut t, &format!("select count(*) from t where {}", like_form(q)));
            let got = count(&mut t, &format!("select count(*) from t where search('{q}', {COLS})"));
            assert_eq!(got, want, "search('{q}') cached={cached}");
            // again, now answered from the conjunct's cached mask
            assert_eq!(count(&mut t, &format!("select count(*) from t where search('{q}', {COLS})")), want);
        }
    }
    assert!(count(&mut t, "select count(*) from t where search('health', title, org, descr)") > 0);
}

#[test]
fn typing_narrows_word_by_word() {
    let mut t = table();
    t.masks().set_budget(1 << 20);
    for q in ["mental health", "youth sport london", "climate change", "são paulo"] {
        let chars: Vec<char> = q.chars().collect();
        for i in 1..=chars.len() {
            let typed: String = chars[..i].iter().collect();
            let want = count(&mut t, &format!("select count(*) from t where {}", like_form(&typed)));
            let got = count(&mut t, &format!("select count(*) from t where search('{typed}', {COLS})"));
            assert_eq!(got, want, "typing '{typed}'");
        }
    }
    assert!(t.masks().narrowed > 0, "later keystrokes narrow from the cached shorter word");
}

#[test]
fn outside_a_where_conjunct() {
    let mut t = table();
    for q in QUERIES {
        let want = count(&mut t, &format!("select count(*) from t where {}", like_form(q)));
        let as_value = count(&mut t, &format!("select sum(case when search('{q}', {COLS}) then 1 else 0 end) from t"));
        let under_or = count(&mut t, &format!("select count(*) from t where search('{q}', {COLS}) or 1 = 0"));
        assert_eq!((as_value, under_or), (want, want), "search('{q}')");
    }
}

#[test]
fn facets_combine() {
    let mut t = table();
    t.masks().set_budget(1 << 20);
    let want = count(&mut t, &format!("select count(*) from t where org = 'Mind' and {}", like_form("health")));
    assert_eq!(count(&mut t, &format!("select count(*) from t where search('health', {COLS}) and org = 'Mind'")), want);
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
