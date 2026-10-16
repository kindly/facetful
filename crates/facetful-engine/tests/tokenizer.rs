//! The search tokenizer (`for_each_word`) against outside test data, in
//! tests/data (regenerate with scripts/gen-unicode-tables.py):
//! - Unicode's NormalizationTest (17.0.0, character by character): a word
//!   gives the same words precomposed or decomposed;
//! - Unicode's WordBreakTest: apostrophes between letters or digits;
//! - the Big List of Naughty Strings (MIT, blns-LICENSE): no panics, and
//!   scanning and the index agree.
#![cfg(feature = "search")]

use facetful_engine::format::write::{ColumnChunk, SegmentData, Writer};
use facetful_engine::format::{ColumnDef, ColumnType, Schema};
use facetful_engine::search_index::for_each_word;
use facetful_engine::sql::run_query;
use facetful_engine::Table;

fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for_each_word(s.as_bytes(), &mut buf, |w| out.push(String::from_utf8_lossy(w).into_owned()));
    out
}

fn data(file: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/data/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn hex(cps: &str) -> String {
    cps.split_whitespace().map(|h| char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap()).collect()
}

/// A text precomposed, decomposed, or as written gives the same words:
/// accents in U+0300-036F fold either way (`é`, `e` + U+0301: `e`).
/// Characters whose decomposition holds other marks, and Hangul, keep
/// their form (the fixture marks them; they must still tokenize).
#[test]
fn composed_and_decomposed_text_give_the_same_words() {
    let (mut same, mut kept) = (0, 0);
    for line in data("normalization.txt").lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split(';').collect();
        let (src, nfc, nfd) = (hex(f[0]), hex(f[1]), hex(f[2]));
        // inside a word too: a decomposed accent must not split it
        let w = |s: &str| words(&format!("x{s}y z"));
        if f[3] == "y" {
            assert_eq!(w(&src), w(&nfd), "{line}: as written and decomposed");
            assert_eq!(w(&nfc), w(&nfd), "{line}: precomposed and decomposed");
            same += 1;
        } else {
            let _ = (w(&src), w(&nfc), w(&nfd));
            kept += 1;
        }
    }
    assert!(same > 5000 && kept > 11000, "{same} folded, {kept} kept");
}

/// Apostrophes between two letters or two digits keep a word whole, as
/// UAX #29 does (the apostrophe itself is dropped); anywhere else they
/// separate.
#[test]
fn apostrophes_split_as_uax29_does() {
    let mut n = 0;
    for line in data("wordbreak-apostrophes.txt").lines().filter(|l| !l.starts_with('#')) {
        let (cps, segs) = line.split_once(" | ").unwrap();
        let want: Vec<String> = segs
            .split(' ')
            .map(|s| s.replace(['\'', '\u{2019}'], "").to_lowercase())
            .filter(|s| s.chars().any(|c| c.is_alphanumeric()))
            .collect();
        assert_eq!(words(&hex(cps)), want, "{line}");
        n += 1;
    }
    assert!(n >= 20);
}

/// One plain text column `t`.
fn table_of(texts: &[&str]) -> Table<Vec<u8>> {
    let schema = Schema { columns: vec![ColumnDef { name: "t".into(), ty: ColumnType::Utf8, flags: 0 }] };
    let mut w = Writer::new(schema, vec![], 64, &[None]);
    for chunk in texts.chunks(64) {
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

fn count(t: &mut Table<Vec<u8>>, q: &str) -> i64 {
    let sql = format!("select count(*) from t where search('{}', t)", q.replace('\'', "''"));
    let mut r = run_query(t, &sql).unwrap_or_else(|d| panic!("{}", d.render(&sql)));
    r.ensure_rows();
    match r.rows[0][0] {
        facetful_engine::sql::exec::Val::Int(n) => n,
        ref v => panic!("{v:?}"),
    }
}

/// Scanning and the index find the same rows for quoted words and phrases
/// (which neither corrects): `queries` against a table of `texts`.
fn scan_and_index_agree(texts: &[&str], queries: &[String]) {
    let mut scan = table_of(texts);
    scan.masks().set_budget(0);
    let mut indexed = table_of(texts);
    indexed.build_search_index(&[0]).unwrap();
    for q in queries {
        assert_eq!(count(&mut scan, q), count(&mut indexed, q), "query {q:?}");
    }
}

/// Our own cases: accents and apostrophes, both ways round.
#[test]
fn accents_and_apostrophes_match_alike_scanning_or_indexed() {
    let texts = ["Café Rouge", "cafe society", "re\u{301}sume\u{301} writing", "O'Brien Trust", "o\u{2019}brien", "obrien", "children's fund", "the children", "don't panic", "Ελλάδα"];
    let qs: Vec<String> = ["\"cafe\"", "\"café\"", "\"resume\"", "\"résumé\"", "\"o'brien\"", "\"obrien\"", "\"brien\"", "\"children\"", "\"dont\"", "\"don't\"", "\"ελλαδα\"", "\"children fund\""]
        .iter()
        .map(|s| s.to_string())
        .collect();
    scan_and_index_agree(&texts, &qs);
    let mut t = table_of(&texts);
    for (q, n) in [("\"cafe\"", 2), ("\"résumé\"", 1), ("\"o'brien\"", 3), ("\"brien\"", 0), ("\"children\"", 2), ("\"dont\"", 1)] {
        assert_eq!(count(&mut t, q), n, "{q}");
    }
}

/// The naughty strings: every one tokenizes, and searching a table of them
/// for each one's words (quoted, and as a phrase) finds the same rows
/// scanning or indexed.
#[test]
fn naughty_strings() {
    let text = data("blns.txt");
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let mut qs = Vec::new();
    for l in &lines {
        let ws = words(l);
        qs.extend(ws.iter().take(3).map(|w| format!("\"{w}\"")));
        if ws.len() > 1 {
            qs.push(format!("\"{}\"", ws.iter().take(4).cloned().collect::<Vec<_>>().join(" ")));
        }
    }
    qs.sort();
    qs.dedup();
    assert!(qs.len() > 300, "{} queries", qs.len());
    scan_and_index_agree(&lines, &qs);
}
