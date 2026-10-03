//! Time search() queries ordered by search_rank against the same queries
//! ordered by a column alone, per keystroke, on a .facetful image.
//! usage: cargo run --profile native -p facetful-engine --example rank_time -- file.facetful order_col "col" "col" …
use facetful_engine::sql::run_query;
use facetful_engine::Table;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: rank_time file.facetful order_col col…");
    let order = args.next().unwrap();
    let names: Vec<String> = args.collect();
    let bytes = std::fs::read(&path).unwrap();
    let src: &[u8] = &bytes;
    let mut t = Table::open(src).unwrap();
    let cols: Vec<usize> = names.iter().map(|n| t.column_index(n).unwrap()).collect();
    t.build_search_index(&cols).unwrap();
    let ident = |c: &str| format!("\"{}\"", c.replace('"', "\"\""));
    let list = names.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ");
    let ranked = names.iter().enumerate().map(|(i, c)| format!("{}, {}", ident(c), [1, 3, 3, 5, 5][i.min(4)])).collect::<Vec<_>>().join(", ");
    let reps: usize = std::env::var("REPS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let mut seen_plain;
    for q in ["mental health", "youth sport london", "climte chnage", "comunity project"] {
        seen_plain = false;
        for rank in [false, true, false] {
            let ts_expr = !rank && seen_plain;
            seen_plain |= !rank;
            t.masks().set_budget(0);
            t.masks().set_budget(64 << 20);
            let mut ts = Vec::new();
            for i in 1..=q.len() {
                let typed = &q[..i];
                let by = if rank { format!("search_rank('{typed}', {ranked}), ") } else if ts_expr { "\"Amount Awarded\" + 0, ".to_string() } else { String::new() };
                let sql = format!("select 1 from t where search('{typed}', {list}) order by {by}{} desc limit 20", ident(&order));
                let s = Instant::now();
                for _ in 0..reps {
                    run_query(&mut t, &sql).unwrap_or_else(|d| panic!("{}", d.render(&sql)));
                }
                ts.push(s.elapsed().as_secs_f64() * 1e3 / reps as f64);
            }
            let mut s = ts.clone();
            s.sort_by(f64::total_cmp);
            println!("'{q}' {}: median {:.1} ms, max {:.1} ms", if rank { "ranked" } else if ts_expr { "by amount+0, column" } else { "by column" }, s[s.len() / 2], s[s.len() - 1]);
        }
    }
}
