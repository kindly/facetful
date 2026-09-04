//! LIKE vs regex speed: our engine's naive matcher vs regex-lite vs regex,
//! LIKE patterns converted to anchored case-insensitive regex.
use std::time::Instant;

// ---- exact copy of the engine's matcher ----
fn like_match(pattern: &str, s: &str) -> bool {
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

fn like_to_regex(pattern: &str) -> String {
    let mut out = String::from("(?i)^");
    for c in pattern.chars() {
        match c {
            '%' => out.push_str(".*"),
            '_' => out.push('.'),
            c if "\\.+*?()|[]{}^$".contains(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('$');
    out
}

fn main() {
    // dict-shaped strings (like the owner column)
    let dict: Vec<String> = (0..2000).map(|i| format!("owner_{i}")).collect();
    let patterns = ["%7%", "owner_1%", "%_77", "owner_1_2%", "%wner%9"];

    for pat in patterns {
        let rx = like_to_regex(pat);
        let re_full = regex::Regex::new(&rx).unwrap();
        let re_lite = regex_lite::Regex::new(&rx).unwrap();

        let reps_dict = 500; // 500 * 2000 = 1M matcher calls
        let bench = |f: &dyn Fn(&str) -> bool| {
            let t0 = Instant::now();
            let mut hits = 0usize;
            for _ in 0..reps_dict {
                for s in &dict {
                    hits += f(s) as usize;
                }
            }
            (t0.elapsed().as_secs_f64() * 1e9 / (reps_dict * dict.len()) as f64, hits)
        };
        let (t_like, h1) = bench(&|s| like_match(pat, s));
        let (t_lite, h2) = bench(&|s| re_lite.is_match(s));
        let (t_full, h3) = bench(&|s| re_full.is_match(s));
        assert!(h1 == h2 && h2 == h3, "matchers disagree on {pat}: {h1} {h2} {h3}");
        println!(
            "{pat:12} naive-like {t_like:7.0} ns/op | regex-lite {t_lite:7.0} ns/op | regex {t_full:6.0} ns/op | dict-eval(2000): like {:.2}ms lite {:.2}ms regex {:.3}ms",
            t_like * 2000.0 / 1e6, t_lite * 2000.0 / 1e6, t_full * 2000.0 / 1e6
        );
    }
}
