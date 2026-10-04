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
    Table::open(image()).unwrap()
}

/// `table()`'s image bytes.
fn image() -> Vec<u8> {
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
    w.finish()
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
    // per field: the rows where a word is in that column
    assert_eq!(idx.plain, [0, 2]);
    for (field, text_of) in [(0u32, &(|i: usize| if i % 13 != 0 { TITLES[i % TITLES.len()] } else { "" }) as &dyn Fn(usize) -> &'static str), (1, &|i: usize| DESCS[(i * 3) % DESCS.len()])] {
        for prefix in ["mental", "health", "sport", "s", "climate"] {
            let ids = idx.prefix_ids(prefix.as_bytes());
            let mut got = Vec::new();
            for g in 0..t.group_count() {
                let mut m = vec![0u8; t.catalog().groups[g].row_count as usize];
                idx.mark_field(g, ids.clone(), field, &mut m);
                got.extend(m.iter().map(|&b| b == 1));
            }
            let want: Vec<bool> = (0..ROWS).map(|i| words_of(text_of(i)).iter().any(|w| w.starts_with(prefix.as_bytes()))).collect();
            assert_eq!(got, want, "field {field}, prefix '{prefix}'");
        }
    }
    assert!(idx.word_id(b"london").is_some() && idx.word_id(b"lond").is_none());
    // the row lists hold the plain columns (title, descr), one list per
    // field, so a word in both fields of a row counts twice; org is a
    // dictionary column, indexed by value
    let distinct = |t: &str| {
        let mut w = words_of(t);
        w.sort();
        w.dedup();
        w.len()
    };
    let pairs: usize = (0..ROWS)
        .map(|i| distinct(DESCS[(i * 3) % DESCS.len()]) + if i % 13 != 0 { distinct(TITLES[i % TITLES.len()]) } else { 0 })
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
    // without an index: nothing
    let mut t = table();
    let s = search_suggest(&mut t, &[0, 1, 2], "mental hea", None, &opts).unwrap();
    assert!(!s.indexed && s.completions.is_empty() && s.did_you_mean.is_none());

    t.build_search_index(&[0, 1, 2]).unwrap();
    let idx = t.search_index().unwrap();
    // completions are the most common words with the prefix that occur in
    // at least one row the other words and the filter leave
    let expected_words = |t: &mut Table<Vec<u8>>, head: &str, prefix: &str, filter: &str| -> Vec<String> {
        let mut ids: Vec<u32> = idx.prefix_ids(prefix.as_bytes()).collect();
        ids.sort_by(|&a, &b| idx.rows_of(b).cmp(&idx.rows_of(a)).then(a.cmp(&b)));
        ids.into_iter()
            .map(|id| String::from_utf8(idx.word(id).to_vec()).unwrap())
            .filter(|w| n(t, &format!("{head}\"{w}\""), filter) > 0)
            .take(8)
            .collect()
    };
    let words = |s: &facetful_engine::sql::exec::Suggestions| s.completions.iter().map(|c| c.word.clone()).collect::<Vec<_>>();
    for (q, head, prefix, f, sql_f) in [
        ("men", "", "men", None, ""),
        ("mental hea", "mental ", "hea", None, ""),
        ("hea", "", "hea", Some("org = 'Mind'"), " and org = 'Mind'"),
        ("youth s", "youth ", "s", None, ""),
        ("c", "", "c", Some("org = 'Sport England'"), " and org = 'Sport England'"),
    ] {
        let s = search_suggest(&mut t, &[0, 1, 2], q, f, &opts).unwrap();
        assert_eq!(words(&s), expected_words(&mut t, head, prefix, sql_f), "completions of '{q}' {f:?}");
        assert!(s.completions.iter().all(|c| c.query == format!("{head}{}", c.word)));
        assert!(s.prefix_rows > 0);
    }
    // a typo: corrected by itself, and did-you-mean spells it out
    let s = search_suggest(&mut t, &[0, 1, 2], "mentl helth ", None, &opts).unwrap();
    assert_eq!(s.corrected, [("mentl".to_string(), vec!["mental".to_string()]), ("helth".to_string(), vec!["health".to_string()])]);
    assert_eq!(s.did_you_mean.as_deref(), Some("mental health "));
    // the word being typed, when nothing starts with it: completions from
    // prefixes one edit away, and did-you-mean takes the first, whole
    let s = search_suggest(&mut t, &[0, 1, 2], "youth londn", None, &opts).unwrap();
    assert!(s.completions.iter().any(|c| c.word == "london" && c.query == "youth london"));
    assert_eq!(s.did_you_mean.as_deref(), Some("youth london "));
    // nothing to complete after a space; a quoted word is not respelled;
    // a did-you-mean with no rows under the filter is not offered
    assert!(search_suggest(&mut t, &[0, 1, 2], "mental ", None, &opts).unwrap().completions.is_empty());
    assert!(search_suggest(&mut t, &[0, 1, 2], "\"helth\"", None, &opts).unwrap().did_you_mean.is_none());
    assert_eq!(n(&mut t, "climate ", " and org = 'Mind' and 1 = 0"), 0);
    assert!(search_suggest(&mut t, &[0, 1, 2], "climte ", Some("1 = 0"), &opts).unwrap().did_you_mean.is_none());

    // a real word with a far more common neighbour: not corrected, but offered
    let mut texts = vec!["community health"; 25];
    texts.push("heath walk");
    texts.push("news today");
    texts.extend(vec!["new things"; 25]);
    let mut w = word_table(&texts);
    w.build_search_index(&[0]).unwrap();
    // completions keep only words found alongside the others: `heath` is
    // never in a row with `community`
    let s = search_suggest(&mut w, &[0], "community h", None, &opts).unwrap();
    assert_eq!(s.completions.iter().map(|c| c.word.as_str()).collect::<Vec<_>>(), ["health"]);
    let s = search_suggest(&mut w, &[0], "h", None, &opts).unwrap();
    assert_eq!(s.completions.iter().map(|c| c.word.as_str()).collect::<Vec<_>>(), ["health", "heath"]);
    let s = search_suggest(&mut w, &[0], "heath ", None, &opts).unwrap();
    assert!(s.corrected.is_empty());
    assert_eq!(s.did_you_mean.as_deref(), Some("health "));
    // a word's own plural is not offered
    assert!(search_suggest(&mut w, &[0], "news ", None, &opts).unwrap().did_you_mean.is_none());
}

type Ranges = Vec<std::ops::Range<u32>>;

/// One query word as search_rank sees it, with the word ids it accepts
/// (None = compare text, no index).
struct RankWord {
    text: Vec<u8>,
    prefix: bool,
    forms: Option<(Ranges, Ranges, Ranges)>, // (all, exact, not a typo)
}

fn rank_words(idx: Option<&SearchIndex>, q: &str) -> Vec<RankWord> {
    let mut out = Vec::new();
    for it in query_items(q) {
        let n = it.words.len();
        for (k, w) in it.words.into_iter().enumerate() {
            let prefix = it.prefix && k + 1 == n;
            let forms = idx.map(|ix| {
                let f = ix.word_forms(&w, prefix, it.literal);
                let plain = f.ranges.iter().filter(|r| !f.typo.contains(r)).cloned().collect();
                (f.ranges.clone(), f.exact.clone(), plain)
            });
            out.push(RankWord { text: w, prefix, forms });
        }
    }
    out
}

/// search_rank's key for one row, read off its fields: per query word, the
/// lowest weight among the fields holding one of its accepted words (1000
/// when none), then words matched only inexactly, then only through a typo.
fn rank_key(idx: Option<&SearchIndex>, words: &[RankWord], fields: &[(&str, u64)]) -> i64 {
    let (mut weight, mut inexact, mut typos) = (0u64, 0u64, 0u64);
    for w in words {
        let (mut best, mut exact, mut plain) = (1000u64, false, false);
        for (text, wt) in fields {
            for t in words_of(text) {
                let (hit, ex, pl) = match (&w.forms, idx) {
                    (Some((all, ex, pl)), Some(ix)) => {
                        let id = ix.word_id(&t);
                        let inr = |rs: &[std::ops::Range<u32>]| id.is_some_and(|id| rs.iter().any(|r| r.contains(&id)));
                        (inr(all), inr(ex), inr(pl))
                    }
                    _ => {
                        let hit = if w.prefix { t.starts_with(&w.text) } else { t == w.text };
                        (hit, t == w.text, hit)
                    }
                };
                if hit {
                    best = best.min(*wt);
                }
                exact |= ex;
                plain |= pl;
            }
        }
        weight += best;
        if best < 1000 {
            inexact += !exact as u64;
            typos += !plain as u64;
        }
    }
    ((weight << 20) | (inexact << 10) | typos) as i64
}

#[test]
fn rank_by_field_weights_then_exact_then_typo() {
    let text = |v: &Val| match v {
        Val::Text(s) => s.to_string(),
        _ => String::new(),
    };
    for indexed in [false, true] {
        let mut t = table();
        if indexed {
            t.build_search_index(&[0, 1, 2]).unwrap();
        }
        let ix = t.search_index();
        for (cols, weights) in [("title, 1, org, 3, descr, 5", [1u64, 3, 5]), ("descr, title, org", [2, 3, 1]), ("org, 2, title, 2, descr, 1", [2, 2, 1])] {
            for q in ["mental health", "health ", "youth sport london", "climate chnage ", "sport", "\"mental health\" fund", "mentl helth "] {
                let words = rank_words(ix.as_deref(), q);
                let hits = expected_on(ix.as_deref(), q, |_| true).1;
                let all: Vec<i64> = (0..ROWS)
                    .filter(|&i| hits[i])
                    .map(|i| {
                        let title = if i % 13 != 0 { TITLES[i % TITLES.len()] } else { "" };
                        rank_key(ix.as_deref(), &words, &[(title, weights[0]), (ORGS[(i * 7) % ORGS.len()], weights[1]), (DESCS[(i * 3) % DESCS.len()], weights[2])])
                    })
                    .collect();
                let mut sorted = all.clone();
                sorted.sort();
                for limit in [Some(7usize), None] {
                    let sql = format!(
                        "select title, org, descr from t where search('{q}', title, org, descr) order by search_rank('{q}', {cols}){}",
                        limit.map(|l| format!(" limit {l}")).unwrap_or_default()
                    );
                    let mut r = run_query(&mut t, &sql).unwrap_or_else(|d| panic!("{}", d.render(&sql)));
                    r.ensure_rows();
                    let got: Vec<i64> = r
                        .rows
                        .iter()
                        .map(|row| rank_key(ix.as_deref(), &words, &[(&text(&row[0]), weights[0]), (&text(&row[1]), weights[1]), (&text(&row[2]), weights[2])]))
                        .collect();
                    let want: Vec<i64> = sorted.iter().copied().take(limit.unwrap_or(usize::MAX)).collect();
                    assert_eq!(got, want, "indexed={indexed} {cols} '{q}' limit {limit:?}");
                }
            }
        }
    }
    // a word in the title beats the same word only in the description
    let mut t = table();
    t.build_search_index(&[0, 1, 2]).unwrap();
    let mut r = run_query(&mut t, "select title, descr from t where search('wellbeing', title, org, descr) order by search_rank('wellbeing', title, org, descr) limit 1").unwrap();
    r.ensure_rows();
    assert!(matches!(&r.rows[0][0], Val::Text(s) if s.to_lowercase().contains("wellbeing")), "title match first: {:?}", r.rows[0]);
    // a rare word next to a far more common one also finds the common one as a
    // typo. Typos are never exact, so for the word being typed the order is:
    // the word as typed, longer words it starts, then typos
    let mut texts = vec!["health matters"; 400];
    texts.extend(["heathland walks", "heath and moor"]);
    let mut w = word_table(&texts);
    w.build_search_index(&[0]).unwrap();
    let f = w.search_index().unwrap().word_forms(b"heath", true, false);
    assert!(f.own_rows > 0 && !f.typo.is_empty(), "heath should keep its rows and add health as a typo");
    let mut r = run_query(&mut w, "select t from t where search('heath', t) order by search_rank('heath', t) limit 10").unwrap();
    r.ensure_rows();
    let got: Vec<String> = r.rows.iter().map(|row| match &row[0] { Val::Text(s) => s.to_string(), _ => String::new() }).collect();
    assert_eq!(got, [vec!["heath and moor"; 5], vec!["heathland walks"; 5]].concat());
}

#[test]
fn rank_without_a_search_filter_and_in_the_select_list() {
    // no WHERE: every row is ranked, matches first; selected, the key is
    // computed on the gathered rows from the text (the word as typed)
    let mut t = table();
    t.build_search_index(&[0, 1, 2]).unwrap();
    let rank = "search_rank('sport ', title, 1, org, 3, descr, 5)";
    let mut r = run_query(&mut t, &format!("select {rank}, title, org, descr from t order by {rank} limit 40")).unwrap();
    r.ensure_rows();
    let keys: Vec<i64> = r.rows.iter().map(|row| match row[0] { Val::Int(k) => k, ref v => panic!("{v:?}") }).collect();
    let words = rank_words(None, "sport ");
    let text = |v: &Val| match v {
        Val::Text(s) => s.to_string(),
        _ => String::new(),
    };
    for (row, &k) in r.rows.iter().zip(&keys) {
        assert_eq!(k, rank_key(None, &words, &[(&text(&row[1]), 1), (&text(&row[2]), 3), (&text(&row[3]), 5)]), "{row:?}");
    }
    assert!(keys.windows(2).all(|w| w[0] <= w[1]), "{keys:?}");
    assert_eq!(keys[0] >> 20, 1, "a title match leads");
}

#[test]
fn rank_bind_errors() {
    let mut t = table();
    for sql in [
        "select search_rank(title, title) from t",
        "select search_rank('x', 3) from t",
        "select search_rank('x', title, 1, 2) from t",
        "select search_rank('x', title, -1) from t",
    ] {
        assert!(run_query(&mut t, sql).is_err(), "{sql} should not bind");
    }
}

/// The table and its word index from an image read as it arrives, in chunks
/// of any size, indexed between chunks or only at the end: the same table,
/// index and answers as opening the whole image and building the index.
#[test]
fn index_built_while_the_image_arrives() {
    use facetful_engine::load::TableLoad;
    let img = image();
    let mut want = table();
    let want_stats = want.build_search_index(&[0, 1, 2]).unwrap();
    let groups = ROWS.div_ceil(64);
    let sql = |q: &str| format!("select count(*) from t where search('{q}', {COLS})");
    for chunk in [1usize, 7, 300, 4096, img.len()] {
        for (step, late) in [(false, false), (true, false), (true, true)] {
            let mut load = TableLoad::<Vec<u8>>::new(if chunk == 7 { img.len() } else { 0 });
            if !late {
                load.index_columns(&["descr", "title", "org"]).unwrap();
            }
            let (mut ready, mut indexed) = (0, 0);
            for (i, c) in img.chunks(chunk).enumerate() {
                load.push(c).unwrap();
                assert!(load.groups_ready() >= ready, "groups only join");
                ready = load.groups_ready();
                // columns named once the schema is known
                if late && load.header_ready() && i % 2 == 0 && indexed == 0 {
                    load.index_columns(&["descr", "title", "org"]).unwrap();
                }
                if step {
                    indexed = load.index_step(1).unwrap();
                }
            }
            assert_eq!(load.groups_ready(), groups, "chunk {chunk}");
            assert_eq!(load.bytes(), img.len());
            if step && chunk < img.len() {
                assert!(indexed >= groups - 1, "chunk {chunk}: the index kept up ({indexed} of {groups})");
            }
            let mut t = load.finish().unwrap();
            // the streamed build carries on where it was, not from the start
            assert_eq!(t.search_index_step(0).unwrap(), groups - if step { indexed } else { 0 });
            assert_eq!(t.search_index_step(usize::MAX).unwrap(), 0);
            assert_eq!(t.catalog().total_rows, ROWS as u64);
            let got = t.search_index().expect("index built").stats();
            assert_eq!(
                (got.words, got.pairs, got.value_pairs, got.bitmap_entries),
                (want_stats.words, want_stats.pairs, want_stats.value_pairs, want_stats.bitmap_entries),
                "chunk {chunk} step {step} late {late}"
            );
            for q in QUERIES {
                assert_eq!(count(&mut t, &sql(q)), count(&mut want, &sql(q)), "chunk {chunk}: '{q}'");
            }
            assert_eq!(
                count(&mut t, "select count(*) from t where org = 'Mind' and descr like '%health%'"),
                count(&mut want, "select count(*) from t where org = 'Mind' and descr like '%health%'")
            );
        }
    }
    // without columns to index: just the table
    let mut load = TableLoad::<Vec<u8>>::new(0);
    for c in img.chunks(100) {
        load.push(c).unwrap();
    }
    let mut t = load.finish().unwrap();
    assert!(t.search_index().is_none());
    assert_eq!(count(&mut t, "select count(*) from t"), ROWS as i64);
    // an image cut short does not open
    let mut load = TableLoad::<Vec<u8>>::new(0);
    load.push(&img[..img.len() - 3]).unwrap();
    assert!(load.finish().is_err());
    let mut load = TableLoad::<Vec<u8>>::new(0);
    load.push(&img[..40]).unwrap();
    assert!(!load.header_ready());
    assert!(load.finish().is_err());
}


/// Each part of a split image indexed on its own (as it streams), merged at
/// attach: the same index and answers as building over the whole table.
#[test]
fn split_parts_merge_their_indexes() {
    use facetful_engine::format::split::split;
    use facetful_engine::load::TableLoad;
    use facetful_engine::sql::exec::SuggestOptions;
    use facetful_engine::sql::search_suggest;
    let img = image();
    let src: &[u8] = &img;
    let (a, b) = split(&src, &[2], 7).unwrap(); // descr goes to the second part
    let mut whole = table();
    let want = whole.build_search_index(&[0, 1, 2]).unwrap();
    let sql = |q: &str| format!("select count(*) from t where search('{q}', {COLS})");
    let rank = "select title, org, descr from t where search('mental helth', title, org, descr) \
                order by search_rank('mental helth', title, 1, org, 3, descr, 5) limit 30";
    let rows = |t: &mut Table<Vec<u8>>, sql: &str| {
        let mut r = run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
        r.ensure_rows();
        r.rows.iter().map(|row| format!("{row:?}")).collect::<Vec<_>>()
    };
    let check = |t: &mut Table<Vec<u8>>, whole: &mut Table<Vec<u8>>, how: &str| {
        let got = t.search_index().expect("merged").stats();
        assert_eq!(
            (got.words, got.pairs, got.value_pairs, got.bitmap_entries),
            (want.words, want.pairs, want.value_pairs, want.bitmap_entries),
            "{how}"
        );
        for q in QUERIES {
            assert_eq!(count(t, &sql(q)), count(whole, &sql(q)), "{how}: '{q}'");
        }
        assert_eq!(rows(t, rank), rows(whole, rank), "{how}: ranked");
        let cols = [0, 1, 2];
        for q in ["men", "mental heal", "youth sp", "helth "] {
            let s = |t: &mut Table<Vec<u8>>| format!("{:?}", search_suggest(t, &cols, q, None, &SuggestOptions { limit: 5, did_you_mean: true }).unwrap());
            assert_eq!(s(t), s(whole), "{how}: suggest '{q}'");
        }
    };
    let stream = |bytes: &[u8], cols: &[&str]| {
        let mut l = TableLoad::<Vec<u8>>::new(0);
        l.index_columns(cols).unwrap();
        for c in bytes.chunks(500) {
            l.push(c).unwrap();
            l.index_step(1).unwrap();
        }
        l.finish().unwrap()
    };
    // descr alone to the second part, then the dictionary column org with it
    for (second_cols, first_names, second_names) in
        [(vec![2], vec!["title", "org"], vec!["descr"]), (vec![1, 2], vec!["title"], vec!["org", "descr"])]
    {
        let (a, b) = split(&src, &second_cols, 7).unwrap();
        let idx = |t: &Table<Vec<u8>>, names: &[&str]| names.iter().map(|n| t.column_index(n).unwrap()).collect::<Vec<_>>();
        // opened whole, indexed, attached
        let mut t = Table::open(a.clone()).unwrap();
        let cols = idx(&t, &first_names);
        t.build_search_index(&cols).unwrap();
        let mut second = Table::open(b.clone()).unwrap();
        let cols = idx(&second, &second_names);
        second.build_search_index(&cols).unwrap();
        t.attach(second).unwrap();
        check(&mut t, &mut whole, &format!("opened, {second_names:?} second"));
        // streamed, each indexed as it arrives
        let mut t = stream(&a, &first_names);
        t.attach(stream(&b, &second_names)).unwrap();
        check(&mut t, &mut whole, &format!("streamed, {second_names:?} second"));
    }
    // only one side indexed: the first keeps its index, the second's moves over
    let mut t = Table::open(a.clone()).unwrap();
    t.build_search_index(&[0, 1]).unwrap();
    t.attach(Table::open(b.clone()).unwrap()).unwrap();
    assert_eq!(t.search_index().unwrap().cols, vec![0, 1]);
    let mut t = Table::open(a.clone()).unwrap();
    let mut second = Table::open(b.clone()).unwrap();
    second.build_search_index(&[0]).unwrap();
    t.attach(second).unwrap();
    assert_eq!(t.search_index().unwrap().cols, vec![2]);
    let mut w2 = table();
    w2.build_search_index(&[2]).unwrap();
    for q in QUERIES {
        let one = |t: &mut Table<Vec<u8>>| count(t, &format!("select count(*) from t where search('{q}', descr)"));
        assert_eq!(one(&mut t), one(&mut w2), "descr only: '{q}'");
    }
}

/// A saved index (`save_search_index`) loads back into the same table, from
/// however it was opened, and is refused anywhere else, damaged or not.
#[test]
fn saved_index_loads_back_only_where_it_belongs() {
    use facetful_engine::format::split::split;
    use facetful_engine::load::TableLoad;
    let sql = |q: &str| format!("select count(*) from t where search('{q}', {COLS})");
    let mut built = table();
    let want = built.build_search_index(&[0, 1, 2]).unwrap();
    let saved = built.save_search_index().unwrap();
    let same = |t: &mut Table<Vec<u8>>, built: &mut Table<Vec<u8>>, how: &str| {
        let got = t.search_index().expect("loaded").stats();
        assert_eq!((got.words, got.pairs, got.value_pairs, got.bitmap_entries), (want.words, want.pairs, want.value_pairs, want.bitmap_entries), "{how}");
        for q in QUERIES {
            assert_eq!(count(t, &sql(q)), count(built, &sql(q)), "{how}: '{q}'");
        }
    };
    // into the same image, opened again or streamed in
    let mut t = table();
    t.load_search_index(&saved, &[0, 1, 2]).unwrap();
    same(&mut t, &mut built, "reopened");
    let img = image();
    let mut load = TableLoad::<Vec<u8>>::new(0);
    for c in img.chunks(300) {
        load.push(c).unwrap();
    }
    let mut t = load.finish().unwrap();
    t.load_search_index(&saved, &[0, 1, 2]).unwrap();
    same(&mut t, &mut built, "streamed");
    // and one saved from a streamed build loads into the image opened whole
    let mut load = TableLoad::<Vec<u8>>::new(0);
    load.index_columns(&["title", "org", "descr"]).unwrap();
    for c in img.chunks(300) {
        load.push(c).unwrap();
        load.index_step(1).unwrap();
    }
    let mut s = load.finish().unwrap();
    s.search_index_step(usize::MAX).unwrap();
    let mut t = table();
    t.load_search_index(&s.save_search_index().unwrap(), &[0, 1, 2]).unwrap();
    same(&mut t, &mut built, "saved from a stream");
    // other columns, another table, another version: refused, the table unchanged
    let mut t = table();
    assert!(t.load_search_index(&saved, &[0, 2]).is_err());
    assert!(t.search_index().is_none());
    t.load_search_index(&saved, &[2, 1, 0, 0]).unwrap();
    let mut other = word_table(&["mental health", "youth sport"]);
    assert!(other.load_search_index(&saved, &[0, 1, 2]).is_err());
    assert!(other.search_index().is_none());
    let mut v = saved.clone();
    v[4] ^= 1;
    assert!(table().load_search_index(&v, &[0, 1, 2]).is_err());
    // cut short anywhere: refused
    let mut t = table();
    for n in (0..saved.len()).step_by(saved.len() / 997 + 1) {
        assert!(t.load_search_index(&saved[..n], &[0, 1, 2]).is_err(), "cut at {n}");
    }
    assert!(t.search_index().is_none());
    // a flipped byte anywhere: refused, or loaded and answering without a panic
    let mut x = 12345u64;
    for _ in 0..400 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut d = saved.clone();
        let at = 16 + (x >> 33) as usize % (d.len() - 16);
        d[at] ^= 1 << (x % 8);
        let mut t = table();
        if t.load_search_index(&d, &[0, 1, 2]).is_ok() {
            for q in ["mental health", "sport", "helth "] {
                let _ = count(&mut t, &sql(q));
            }
        }
    }
    // a split table: saved after attach, it loads only into both parts attached
    let src: &[u8] = &img;
    let (a, b) = split(&src, &[2], 9).unwrap();
    let mut p = Table::open(a.clone()).unwrap();
    p.attach(Table::open(b.clone()).unwrap()).unwrap();
    p.build_search_index(&[0, 1, 2]).unwrap();
    let saved_parts = p.save_search_index().unwrap();
    let mut q = Table::open(a.clone()).unwrap();
    assert!(q.load_search_index(&saved_parts, &[0, 1, 2]).is_err(), "the first part alone");
    q.attach(Table::open(b).unwrap()).unwrap();
    q.load_search_index(&saved_parts, &[0, 1, 2]).unwrap();
    same(&mut q, &mut built, "split, attached");
    assert!(table().load_search_index(&saved_parts, &[0, 1, 2]).is_err(), "the unsplit image");
}
