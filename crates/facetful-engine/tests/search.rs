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
const DESCS: [&str; 8] = [
    "Mental and physical health",
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

/// A query item as the oracle reads it: words, whether it is a quoted
/// phrase, whether its last word is still being typed, whether it was quoted.
struct QItem {
    words: Vec<Vec<u8>>,
    phrase: bool,
    prefix: bool,
    literal: bool,
}

/// The query read the way search() documents it: quoted stretches are a
/// quoted word or a phrase, an unclosed quote runs to the end, and the last
/// word is a prefix unless the query ends in a separator or closing quote.
fn query_items(q: &str) -> Vec<QItem> {
    let mut items = Vec::new();
    for (k, part) in q.split('"').enumerate() {
        let ws = words_of(part);
        if k % 2 == 1 {
            if !ws.is_empty() {
                items.push(QItem { phrase: ws.len() > 1, words: ws, prefix: false, literal: true });
            }
        } else {
            items.extend(ws.into_iter().map(|w| QItem { words: vec![w], phrase: false, prefix: false, literal: false }));
        }
    }
    if q.chars().next_back().is_some_and(|c| c.is_alphanumeric()) {
        if let Some(last) = items.last_mut() {
            last.prefix = true;
        }
    }
    items
}

/// How many rows search(q, title, org, descr) should match, and which.
/// Without an index, words match as typed (whole, the last as a prefix).
/// With one, a word matches the words its forms in the index cover
/// (`word_forms`: respellings, inflections, corrections); the forms
/// themselves are checked by `forms_follow_the_rules`. A phrase matches its
/// words as typed, consecutively, within one field.
fn expected_on(idx: Option<&SearchIndex>, q: &str, only: impl Fn(usize) -> bool) -> (i64, Vec<bool>) {
    let items = query_items(q);
    let accepts: Vec<Box<dyn Fn(&[u8], usize) -> bool + '_>> = items
        .iter()
        .map(|it| -> Box<dyn Fn(&[u8], usize) -> bool + '_> {
            let n = it.words.len();
            if it.phrase {
                // position k of the phrase: the word as typed
                let words = it.words.clone();
                let prefix = it.prefix;
                return Box::new(move |t: &[u8], k: usize| if prefix && k + 1 == n { t.starts_with(&words[k]) } else { t == words[k].as_slice() });
            }
            let w = it.words[0].clone();
            let prefix = it.prefix;
            match idx {
                Some(ix) => {
                    let ranges = ix.word_forms(&w, prefix, it.literal).ranges;
                    Box::new(move |t: &[u8], _| ix.word_id(t).is_some_and(|id| ranges.iter().any(|r| r.contains(&id))))
                }
                None => Box::new(move |t: &[u8], _| if prefix { t.starts_with(&w) } else { t == w.as_slice() }),
            }
        })
        .collect();
    let hits: Vec<bool> = (0..ROWS)
        .map(|i| {
            let fields: Vec<Vec<Vec<u8>>> = texts(i).iter().map(|t| words_of(t)).collect();
            only(i)
                && items.iter().zip(&accepts).all(|(it, acc)| {
                    if it.phrase {
                        let n = it.words.len();
                        fields.iter().any(|f| f.windows(n).any(|win| win.iter().enumerate().all(|(k, t)| acc(t, k))))
                    } else {
                        fields.iter().flatten().any(|t| acc(t, 0))
                    }
                })
        })
        .collect();
    (hits.iter().filter(|&&h| h).count() as i64, hits)
}

/// `expected_on` without an index: words as typed.
fn expected(q: &str, only: impl Fn(usize) -> bool) -> (i64, Vec<bool>) {
    expected_on(None, q, only)
}

const QUERIES: [&str; 28] = [
    "\"mental health\"",
    "\"health mental\"",
    "\"mental hea",
    "\"youth sport\" london",
    "\"sport\"",
    "\"fund\" climate",
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
            let ix = t.search_index();
            for q in QUERIES {
                let want = expected_on(ix.as_deref(), q, |_| true).0;
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
                let ix = t.search_index();
                assert_eq!(got, expected_on(ix.as_deref(), &typed, |_| true).0, "{path}: typing '{typed}'");
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
        let ix = t.search_index();
        assert_eq!(got, expected_on(ix.as_deref(), "health", mind).0, "{path}");
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
            idx.mark_group(&mut t, g, std::slice::from_ref(&ids), &idx.value_sets(std::slice::from_ref(&ids)), &mut m).unwrap();
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


#[test]
fn incremental_build_with_queries_in_between() {
    let mut t = table();
    t.masks().set_budget(1 << 20);
    let groups = t.group_count();
    assert!(groups > 2, "the test table has several row groups");
    t.begin_search_index(&[0, 1, 2]).unwrap();
    let mut left = groups;
    let mut step = 0;
    while left > 0 {
        // between steps the index is not there yet: search() scans, correctly
        assert!(t.search_index().is_none());
        let q = QUERIES[step % QUERIES.len()];
        let got = count(&mut t, &format!("select count(*) from t where search('{q}', {COLS})"));
        assert_eq!(got, expected(q, |_| true).0, "during the build, step {step}: search('{q}')");
        left = t.search_index_step(1).unwrap();
        assert_eq!(left, groups - step - 1);
        step += 1;
    }
    // finished: the index answers, the same rows as a one-shot build
    let idx = t.search_index().expect("built");
    let mut t2 = table();
    let one = SearchIndex::build(&mut t2, &[0, 1, 2]).unwrap();
    assert_eq!(idx.stats().pairs, one.stats().pairs);
    assert_eq!(idx.stats().words, one.stats().words);
    for q in QUERIES {
        let got = count(&mut t, &format!("select count(*) from t where search('{q}', {COLS})"));
        assert_eq!(got, expected_on(Some(&idx), q, |_| true).0, "after the build: search('{q}')");
    }
    // a step with no build in progress does nothing
    assert_eq!(t.search_index_step(5).unwrap(), 0);
    assert!(t.search_index().is_some());
}

#[test]
fn typos_are_corrected_with_the_index() {
    let mut t = table();
    t.masks().set_budget(1 << 20);
    let q = |t: &mut Table<Vec<u8>>, s: &str| count(t, &format!("select count(*) from t where search('{s}', {COLS})"));
    // the scan path has no vocabulary, so no corrections
    assert_eq!(q(&mut t, "helth "), 0);
    t.build_search_index(&[0, 1, 2]).unwrap();
    // finished words: a missing word becomes its neighbours within the edit limit
    for (typo, right) in [
        ("helth ", "health "),
        ("sprot ", "sport "),
        ("mentl helth ", "mental health "),
        ("climte chnage ", "climate change "),
        // two edits, allowed from 8 letters, found by measuring against words
        ("partnrshp ", "partnership "),
        // non-ASCII, also measured
        ("münchn ", "münchen "),
    ] {
        // with the index, both go through respelling and inflection: compare them there
        let want = q(&mut t, right);
        assert!(want > 0, "{right}");
        assert_eq!(q(&mut t, typo), want, "'{typo}' should find what '{right}' finds");
    }
    // the word being typed: nothing starts with `londn`, so prefixes one edit away count
    assert_eq!(q(&mut t, "youth londn"), q(&mut t, "youth london"));
    // words that exist stay as typed; words under 4 letters are never corrected
    let ix = t.search_index();
    assert_eq!(q(&mut t, "sport "), expected_on(ix.as_deref(), "sport ", |_| true).0);
    assert_eq!(q(&mut t, "zzz "), 0);
    // the scan's cached answer is not reused once the index can correct
    let mut t = table();
    t.masks().set_budget(1 << 20);
    assert_eq!(q(&mut t, "helth "), 0);
    t.build_search_index(&[0, 1, 2]).unwrap();
    assert_eq!(q(&mut t, "helth "), q(&mut t, "health "));
    // and the index says what it did
    let idx = t.search_index().unwrap();
    let f = idx.word_forms(b"helth", false, false);
    assert_eq!(f.own_rows, 0);
    assert_eq!(f.corrections.iter().map(|c| c.0.as_slice()).collect::<Vec<_>>(), [b"health".as_slice()]);
    assert!(idx.word_forms(b"health", false, false).corrections.is_empty());
}

/// One plain text column `t`, each text repeated 5 times (inflected forms
/// need 5 rows to count).
fn word_table(texts: &[&str]) -> Table<Vec<u8>> {
    let schema = Schema { columns: vec![ColumnDef { name: "t".into(), ty: ColumnType::Utf8, flags: 0 }] };
    let mut w = Writer::new(schema, vec![], 64, &[None]);
    let rows: Vec<&str> = texts.iter().flat_map(|t| std::iter::repeat_n(*t, 5)).collect();
    for chunk in rows.chunks(64) {
        let mut offs = vec![0u32];
        let mut bytes = Vec::new();
        for r in chunk {
            bytes.extend_from_slice(r.as_bytes());
            offs.push(bytes.len() as u32);
        }
        w.write_group(chunk.len() as u32, &[ColumnChunk { data: SegmentData::Utf8 { offsets: &offs, bytes: &bytes }, validity: None, null_count: 0 }]);
    }
    Table::open(w.finish()).unwrap()
}

#[test]
fn forms_follow_the_rules() {
    let mut t = word_table(&[
        "fund funds funded funding fundamental funder",
        "charity charities plan plans planned planning",
        "organisation organisations organization centre center programme program",
        "heath heaths heather business news new use used using",
        "colour color travelled traveled defence defense",
    ]);
    let idx = SearchIndex::build(&mut t, &[0]).unwrap();
    let forms = |w: &str| {
        let f = idx.word_forms(w.as_bytes(), false, false);
        let mut all: Vec<String> = f.ranges.iter().flat_map(|r| r.clone()).map(|id| String::from_utf8(idx.word(id).to_vec()).unwrap()).collect();
        all.sort();
        all
    };
    // plurals, -ed, -ing; not -er (another word) and not words that only start alike
    assert_eq!(forms("fund"), ["fund", "funded", "funding", "funds"]);
    assert_eq!(forms("funding"), ["fund", "funded", "funding", "funds"]);
    assert_eq!(forms("charities"), ["charities", "charity"]);
    assert_eq!(forms("planning"), ["plan", "planned", "planning", "plans"]);
    assert_eq!(forms("heath"), ["heath", "heaths"]);
    assert_eq!(forms("used"), ["use", "used", "using"]);
    // a final s that is not a plural
    assert_eq!(forms("news"), ["news"]);
    assert_eq!(forms("business"), ["business"]);
    // UK/US spellings both ways, with their inflections
    assert_eq!(forms("organization"), ["organisation", "organisations", "organization"]);
    assert_eq!(forms("centre"), ["center", "centre"]);
    assert_eq!(forms("program"), ["program", "programme"]);
    assert_eq!(forms("color"), ["color", "colour"]);
    assert_eq!(forms("traveled"), ["traveled", "travelled"]);
    assert_eq!(forms("defence"), ["defence", "defense"]);
    // a spelling variant that exists is not a typo to correct
    assert!(idx.word_forms(b"organization", false, false).corrections.is_empty());
    // quoted: only itself
    let lit = idx.word_forms(b"fund", false, true);
    assert_eq!((lit.ranges.len(), lit.expanded()), (1, false));
    // the word being typed: its prefix covers its forms
    assert!(!idx.word_forms(b"fund", true, false).expanded());
    // and search() uses them: `fund ` finds funding, `"fund"` does not
    let n = |t: &mut Table<Vec<u8>>, q: &str| count(t, &format!("select count(*) from t where search('{q}', t)"));
    t.build_search_index(&[0]).unwrap();
    assert_eq!(n(&mut t, "funding "), n(&mut t, "fund "));
    assert_eq!(n(&mut t, "\"fund\""), 5);
}

#[test]
fn suggestions() {
    use facetful_engine::sql::exec::SuggestOptions;
    use facetful_engine::sql::search_suggest;
    let opts = SuggestOptions { limit: 8, did_you_mean: true };
    let n = |t: &mut Table<Vec<u8>>, q: &str, f: &str| count(t, &format!("select count(*) from t where search('{q}', {COLS}){f}"));
    // without an index: the count, nothing else
    let mut t = table();
    let s = search_suggest(&mut t, &[0, 1, 2], "mental hea", None, &opts).unwrap();
    assert!(!s.indexed && s.completions.is_empty() && s.did_you_mean.is_none());
    assert_eq!(s.rows as i64, n(&mut t, "mental hea", ""));

    t.build_search_index(&[0, 1, 2]).unwrap();
    // the word being typed alone: the index's counts
    let s = search_suggest(&mut t, &[0, 1, 2], "men", None, &opts).unwrap();
    assert_eq!(s.rows as i64, n(&mut t, "men", ""));
    let mental = s.completions.iter().find(|c| c.word == "mental").expect("mental completes men");
    assert_eq!(mental.query, "mental");
    assert_eq!(mental.rows as i64, n(&mut t, "mental ", ""));
    assert!(s.prefix_rows > 0);
    // after other words: counted on the rows they leave, so the completed query's count
    let s = search_suggest(&mut t, &[0, 1, 2], "mental hea", None, &opts).unwrap();
    let health = s.completions.iter().find(|c| c.word == "health").expect("health completes hea");
    assert_eq!(health.query, "mental health");
    assert_eq!(health.rows as i64, n(&mut t, "mental health ", ""));
    assert!(s.completions.windows(2).all(|w| w[0].rows >= w[1].rows), "most rows first");
    // under a filter
    let mind = " and org = 'Mind'";
    let s = search_suggest(&mut t, &[0, 1, 2], "hea", Some("org = 'Mind'"), &opts).unwrap();
    let health = s.completions.iter().find(|c| c.word == "health").unwrap();
    assert_eq!(health.rows as i64, n(&mut t, "health ", mind));
    assert_eq!(s.rows as i64, n(&mut t, "hea", mind));
    // a typo: corrected by itself, and did-you-mean spells it out
    let s = search_suggest(&mut t, &[0, 1, 2], "mentl helth ", None, &opts).unwrap();
    assert_eq!(s.corrected, [("mentl".to_string(), vec!["mental".to_string()]), ("helth".to_string(), vec!["health".to_string()])]);
    let (q, rows) = s.did_you_mean.expect("a better query");
    assert_eq!(q, "mental health ");
    assert_eq!(rows as i64, n(&mut t, "mental health ", ""));
    // the word being typed, when nothing starts with it: completions from
    // prefixes one edit away, and did-you-mean takes the top one, whole
    let s = search_suggest(&mut t, &[0, 1, 2], "youth londn", None, &opts).unwrap();
    assert!(s.completions.iter().any(|c| c.word == "london" && c.query == "youth london"));
    let (q, rows) = s.did_you_mean.expect("a whole word for the misspelt one being typed");
    assert_eq!(q, "youth london ");
    assert_eq!(rows as i64, n(&mut t, "youth london ", ""));
    // nothing to complete after a space; a quoted word is not respelled
    assert!(search_suggest(&mut t, &[0, 1, 2], "mental ", None, &opts).unwrap().completions.is_empty());
    assert!(search_suggest(&mut t, &[0, 1, 2], "\"helth\"", None, &opts).unwrap().did_you_mean.is_none());

    // a real word with a far more common neighbour: not corrected, but offered
    let mut texts = vec!["community health"; 25];
    texts.push("heath walk");
    texts.push("news today");
    texts.extend(vec!["new things"; 25]);
    let mut w = word_table(&texts);
    w.build_search_index(&[0]).unwrap();
    let s = search_suggest(&mut w, &[0], "heath ", None, &opts).unwrap();
    assert!(s.corrected.is_empty());
    assert_eq!(s.rows, 5);
    assert_eq!(s.did_you_mean, Some(("health ".to_string(), 125)));
    // a word's own plural is not offered
    assert!(search_suggest(&mut w, &[0], "news ", None, &opts).unwrap().did_you_mean.is_none());
}
