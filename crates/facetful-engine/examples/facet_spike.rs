//! Native spike timing: scripted facet interactions over a .facetful file.
//! usage: cargo run --release -p facetful-engine --example facet_spike -- file.facetful
use facetful_engine::{FacetQuery, Table};
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: facet_spike file.facetful");
    let bytes = std::fs::read(&path).unwrap();
    let src: &[u8] = &bytes;
    let mut t = Table::open(src).unwrap();
    let dims: Vec<usize> = ["country", "status", "fuel", "region", "owner", "year"]
        .iter().map(|n| t.column_index(n).unwrap()).collect();
    let measure = t.column_index("capacity").unwrap();
    let cards: Vec<i32> = dims.iter().map(|&d| t.dictionary(d).unwrap().len() as i32).collect();

    // scripted interactions (deterministic LCG)
    let mut state = 12345u64;
    let mut rng = move || { state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (state >> 33) as u32 };
    let mut selected = vec![-1i32; dims.len()];
    let mut times = Vec::new();
    for it in 0..120 {
        let k = (rng() as usize) % dims.len();
        selected[k] = if selected[k] >= 0 && rng() % 100 < 35 { -1 } else { (rng() as i32) % cards[k] };
        let q = FacetQuery { dims: dims.clone(), selected: selected.clone(), measure };
        let t0 = Instant::now();
        let r = t.facet_refresh(&q).unwrap();
        let top = t.sort_topk(measure, &r.mask, 50).unwrap();
        let el = t0.elapsed().as_secs_f64() * 1000.0;
        if it >= 20 { times.push(el); }
        std::hint::black_box((r.pass_count, top.len()));
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("facet_refresh+top50 over {}: median {:.2}ms  p95 {:.2}ms (native, {} measured)",
        path, times[times.len()/2], times[(times.len() as f64 * 0.95) as usize], times.len());
}
