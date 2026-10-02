//! Build the search() word index over a .facetful image and report what it
//! costs: build time, memory held, and lookup times for typed queries.
//! usage: cargo run --profile native -p facetful-engine --example index_build -- file.facetful "col" "col" …
use facetful_engine::search_index::SearchIndex;
use facetful_engine::Table;
use std::time::Instant;

/// The process's peak resident memory (Linux), in MB.
fn peak_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM:")).map(|l| l[6..].trim().trim_end_matches(" kB").trim().parse::<f64>().unwrap_or(0.0)))
        .unwrap_or(0.0)
        / 1024.0
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: index_build file.facetful col…");
    let names: Vec<String> = args.collect();
    let bytes = std::fs::read(&path).unwrap();
    let src: &[u8] = &bytes;
    let mut t = Table::open(src).unwrap();
    let cols: Vec<usize> = names.iter().map(|n| t.column_index(n).unwrap_or_else(|| panic!("no column {n}"))).collect();
    let before = peak_mb();

    let t0 = Instant::now();
    let idx = SearchIndex::build(&mut t, &cols).unwrap();
    let build = t0.elapsed();
    let s = idx.stats();
    println!(
        "build {:.2} s | {} words, {} (word, row) pairs + {} (word, value) pairs, {} bitmap entries | lists {:.1} MB + vocabulary {:.1} MB | peak RSS {:.0} MB before, {:.0} MB after",
        build.as_secs_f64(),
        s.words,
        s.pairs,
        s.value_pairs,
        s.bitmap_entries,
        s.list_bytes as f64 / 1e6,
        s.vocab_bytes as f64 / 1e6,
        before,
        peak_mb()
    );

    println!(
        "phases: dictionaries {:.2} s, rows to pairs {:.2} s, pairs to lists {:.2} s, finish {:.2} s",
        s.phases[0], s.phases[1], s.phases[2], s.phases[3]
    );

    // spelling: what each word corrects to, and how long finding it takes
    let show = |w: &str, prefix: bool| {
        let t0 = Instant::now();
        let f = idx.word_forms(w.as_bytes(), prefix, false);
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let mut fixes: Vec<String> = f.corrections.iter().map(|(c, n)| format!("fix {}{} {n}", String::from_utf8_lossy(c), if prefix { "…" } else { "" })).collect();
        fixes.extend(f.variants.iter().map(|(c, n)| format!("var {} {n}", String::from_utf8_lossy(c))));
        fixes.extend(f.inflections.iter().map(|(c, n)| format!("{} {n}", String::from_utf8_lossy(c))));
        println!("{:>16}{} | own {:>7} | {:<100} | {:.2} ms", w, if prefix { "…" } else { " " }, f.own_rows, if fixes.is_empty() { "(as typed)".into() } else { fixes.join(", ") }, ms);
    };
    for w in ["helth", "heath", "health", "fund", "funding", "charities", "organization", "organisation", "centre", "programme",
              "children", "news", "business", "plan", "training", "used", "sport", "community", "support", "projects"] {
        show(w, false);
    }
    for w in ["helt", "londn", "enviro", "climt", "comun", "suppo"] {
        show(w, true);
    }

    // typed queries: every word a prefix, ANDed (word-start matching)
    let groups = t.group_count();
    let sizes: Vec<usize> = (0..groups).map(|g| t.catalog().groups[g].row_count as usize).collect();
    for q in ["m", "me", "men", "mental", "mental h", "mental hea", "mental health", "youth sport london", "climate change adaptation", "support", "community project", "co", "a"] {
        let t0 = Instant::now();
        let words: Vec<&str> = q.split(' ').filter(|w| !w.is_empty()).collect();
        let id_sets: Vec<_> = words.iter().map(|w| idx.prefix_ids(w.as_bytes())).collect();
        let value_sets: Vec<_> = id_sets.iter().map(|ids| idx.value_sets(std::slice::from_ref(ids))).collect();
        let mut total = 0usize;
        for (g, &rows) in sizes.iter().enumerate() {
            let mut keep = vec![1u8; rows];
            for (ids, values) in id_sets.iter().zip(&value_sets) {
                let mut m = vec![0u8; rows];
                idx.mark_group(&mut t, g, std::slice::from_ref(ids), values, &mut m).unwrap();
                for (k, b) in keep.iter_mut().zip(m) {
                    *k &= b;
                }
            }
            total += keep.iter().filter(|&&k| k == 1).count();
        }
        let n_ids: usize = id_sets.iter().map(|s| s.len()).sum();
        println!("{:>28} | {:>9} rows | {:>6} words in prefix ranges | {:>7.2} ms", format!("'{q}'"), total, n_ids, t0.elapsed().as_secs_f64() * 1e3);
    }
}
