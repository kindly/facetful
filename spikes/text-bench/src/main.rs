//! Long-text search over a notes-type column: naive LIKE (engine today) vs
//! optimized substring fast path vs a small inverted index with basic stemming.
use std::collections::HashMap;
use std::time::Instant;

// deterministic rng
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn engine_like(pattern: &str, s: &str) -> bool {
    fn rec(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('%') => (0..=s.len()).any(|k| rec(&p[1..], &s[k..])),
            Some('_') => !s.is_empty() && rec(&p[1..], &s[1..]),
            Some(c) => !s.is_empty() && s[0].eq_ignore_ascii_case(c) && rec(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let sc: Vec<char> = s.chars().collect();
    rec(&p, &sc)
}

/// crude stemmer: lowercase + strip common English suffixes
fn stem(w: &str) -> String {
    let w = w.to_ascii_lowercase();
    for suf in ["ing", "edly", "ed", "es", "s", "ly"] {
        if w.len() > suf.len() + 2 && w.ends_with(suf) {
            return w[..w.len() - suf.len()].to_string();
        }
    }
    w
}

fn tokens(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| t.len() > 1)
}

fn main() {
    // ---- synthesize 200K notes averaging ~25 words ----
    let vocab: Vec<String> = {
        let stems = ["energy", "coal", "solar", "wind", "plant", "capacity", "operate", "close",
            "permit", "review", "expand", "grid", "connect", "retire", "propose", "build",
            "station", "turbine", "panel", "unit", "phase", "delay", "cancel", "approve"];
        let mut v = Vec::new();
        for s in stems {
            for suf in ["", "s", "ed", "ing", "ment", "ation"] {
                v.push(format!("{s}{suf}"));
            }
        }
        for i in 0..3000 {
            v.push(format!("word{i}"));
        }
        v
    };
    let mut rng = Rng(42);
    const N: usize = 200_000;
    let notes: Vec<String> = (0..N)
        .map(|_| {
            let words = 15 + (rng.next() % 25) as usize;
            (0..words)
                .map(|_| {
                    let z = (rng.next() % 1000) as f64 / 1000.0;
                    vocab[((z * z) * vocab.len() as f64) as usize % vocab.len()].as_str()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let total_bytes: usize = notes.iter().map(|s| s.len()).sum();
    println!("corpus: {N} notes, {:.1} MB, avg {} chars", total_bytes as f64 / 1e6, total_bytes / N);

    let term = "operat"; // matches operate/operated/operating via substring; stem 'operate'->'operat'

    // ---- 1. naive LIKE (engine today) ----
    let pat = format!("%{term}%");
    let t0 = Instant::now();
    let hits1: usize = notes.iter().filter(|s| engine_like(&pat, s)).count();
    let t_naive = t0.elapsed().as_secs_f64() * 1000.0;

    // ---- 2. optimized contains: pre-lowered corpus (image-time precompute) + str::contains ----
    let lowered: Vec<String> = notes.iter().map(|s| s.to_ascii_lowercase()).collect();
    let t0 = Instant::now();
    let hits2: usize = lowered.iter().filter(|s| s.contains(term)).count();
    let t_contains = t0.elapsed().as_secs_f64() * 1000.0;

    // (2b. contains with per-row lowercase — no precompute)
    let t0 = Instant::now();
    let hits2b: usize = notes.iter().filter(|s| s.to_ascii_lowercase().contains(term)).count();
    let t_contains_alloc = t0.elapsed().as_secs_f64() * 1000.0;

    // ---- 3. small inverted index with stemming ----
    let t0 = Instant::now();
    let mut lexicon: HashMap<String, u32> = HashMap::new();
    let mut postings: Vec<Vec<u32>> = Vec::new();
    for (row, note) in notes.iter().enumerate() {
        let mut last: Option<u32> = None;
        for tok in tokens(note) {
            let st = stem(tok);
            let id = *lexicon.entry(st).or_insert_with(|| {
                postings.push(Vec::new());
                (postings.len() - 1) as u32
            });
            // dedupe consecutive same-row inserts cheaply
            if postings[id as usize].last() != Some(&(row as u32)) {
                postings[id as usize].push(row as u32);
            }
            last = Some(id);
        }
        let _ = last;
    }
    let t_build = t0.elapsed().as_secs_f64() * 1000.0;
    let index_entries: usize = postings.iter().map(|p| p.len()).sum();
    // serialized cost estimate: delta-varint postings (~1.5B/entry avg) + lexicon
    let lex_bytes: usize = lexicon.keys().map(|k| k.len() + 6).sum();
    let est_bytes = index_entries * 3 / 2 + lex_bytes;

    let q = stem("operating"); // query-side stemming: operating -> operat
    let t0 = Instant::now();
    let mut hits3 = 0usize;
    for _ in 0..100 {
        hits3 = lexicon.get(&q).map(|&id| postings[id as usize].len()).unwrap_or(0);
    }
    let t_index = t0.elapsed().as_secs_f64() * 1000.0 / 100.0;

    // AND of two terms (posting intersection)
    let q2 = stem("solar");
    let t0 = Instant::now();
    let mut hits_and = 0usize;
    for _ in 0..100 {
        let (a, b) = (
            lexicon.get(&q).map(|&i| &postings[i as usize]),
            lexicon.get(&q2).map(|&i| &postings[i as usize]),
        );
        hits_and = match (a, b) {
            (Some(a), Some(b)) => {
                let (mut i, mut j, mut n) = (0, 0, 0);
                while i < a.len() && j < b.len() {
                    match a[i].cmp(&b[j]) {
                        std::cmp::Ordering::Less => i += 1,
                        std::cmp::Ordering::Greater => j += 1,
                        std::cmp::Ordering::Equal => {
                            n += 1;
                            i += 1;
                            j += 1;
                        }
                    }
                }
                n
            }
            _ => 0,
        };
    }
    let t_and = t0.elapsed().as_secs_f64() * 1000.0 / 100.0;

    println!("naive LIKE '%{term}%':            {t_naive:8.1} ms  ({hits1} hits)");
    println!("contains, pre-lowered corpus:     {t_contains:8.1} ms  ({hits2} hits)");
    println!("contains, lowercase per row:      {t_contains_alloc:8.1} ms  ({hits2b} hits)");
    println!("inverted index single term:       {t_index:8.3} ms  ({hits3} rows w/ 'operat*')");
    println!("inverted index AND two terms:     {t_and:8.3} ms  ({hits_and} rows)");
    println!("index build: {t_build:.0} ms | vocab {} stems | {} postings | est. serialized ~{:.1} MB ({:.0}% of corpus)",
        lexicon.len(), index_entries, est_bytes as f64 / 1e6, est_bytes as f64 * 100.0 / total_bytes as f64);
}
