//! Engine correctness: facet_refresh / sort_topk / gather against a file built
//! in-code, verified against a naive row-by-row recomputation.

use facetful_engine::{FacetQuery, GatherResult, Table};
use facetful_format::write::{ColumnChunk, SegmentData, Writer};
use facetful_format::{flags, ColumnDef, ColumnType, Schema};

fn utf8_offsets(strings: &[&str]) -> (Vec<u32>, Vec<u8>) {
    let mut offsets = vec![0u32];
    let mut bytes = Vec::new();
    for s in strings {
        bytes.extend_from_slice(s.as_bytes());
        offsets.push(bytes.len() as u32);
    }
    (offsets, bytes)
}

/// 3 groups x 5 rows, 2 dict dims + measure.
fn build_file() -> (Vec<u8>, Vec<Vec<u16>>, Vec<f64>) {
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "region".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY },
            ColumnDef { name: "status".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY },
            ColumnDef { name: "amount".into(), ty: ColumnType::Float64, flags: 0 },
        ],
    };
    let (roff, rbytes) = utf8_offsets(&["eu", "us", "asia"]);
    let (soff, sbytes) = utf8_offsets(&["open", "closed"]);

    let region_codes: Vec<u16> = vec![0, 1, 2, 0, 1, 2, 0, 1, 2, 0, 1, 2, 0, 1, 2];
    let status_codes: Vec<u16> = vec![0, 0, 1, 1, 0, 1, 0, 1, 0, 0, 1, 1, 0, 0, 1];
    let amounts: Vec<f64> = (0..15).map(|i| (i as f64) * 1.25 + 0.5).collect();

    let mut w = Writer::new(schema, vec![], 5);
    for g in 0..3 {
        let s = g * 5;
        let amount_bytes: Vec<u8> = amounts[s..s + 5].iter().flat_map(|x| x.to_le_bytes()).collect();
        w.write_group(
            5,
            &[
                ColumnChunk {
                    data: SegmentData::Dict { codes: &region_codes[s..s + 5], dict_offsets: &roff, dict_bytes: &rbytes },
                    validity: None,
                    null_count: 0,
                },
                ColumnChunk {
                    data: SegmentData::Dict { codes: &status_codes[s..s + 5], dict_offsets: &soff, dict_bytes: &sbytes },
                    validity: None,
                    null_count: 0,
                },
                ColumnChunk { data: SegmentData::Fixed(&amount_bytes), validity: None, null_count: 0 },
            ],
        );
    }
    (w.finish(), vec![region_codes, status_codes], amounts)
}

fn naive_facets(
    dims: &[Vec<u16>],
    cards: &[usize],
    selected: &[i32],
    amounts: &[f64],
) -> (Vec<Vec<u32>>, u64, f64, Vec<u8>) {
    let n = amounts.len();
    let d = dims.len();
    let mut counts: Vec<Vec<u32>> = cards.iter().map(|&c| vec![0; c]).collect();
    let mut mask = vec![0u8; n];
    let (mut pass, mut sum) = (0u64, 0f64);
    for row in 0..n {
        for k in 0..d {
            // counted under all filters except k's own
            let ok = (0..d).all(|j| j == k || selected[j] < 0 || dims[j][row] as i32 == selected[j]);
            if ok {
                counts[k][dims[k][row] as usize] += 1;
            }
        }
        if (0..d).all(|j| selected[j] < 0 || dims[j][row] as i32 == selected[j]) {
            mask[row] = 1;
            pass += 1;
            sum += amounts[row];
        }
    }
    (counts, pass, sum, mask)
}

#[test]
fn facet_refresh_matches_naive() {
    let (file, dims, amounts) = build_file();
    let src: &[u8] = &file;
    let mut t = Table::open(src).unwrap();
    assert_eq!(t.dictionary(0).unwrap(), vec!["eu", "us", "asia"]);
    assert_eq!(t.dictionary(1).unwrap(), vec!["open", "closed"]);

    for selected in [vec![-1, -1], vec![0, -1], vec![-1, 1], vec![2, 0], vec![1, 1]] {
        let q = FacetQuery { dims: vec![0, 1], selected: selected.clone(), measure: 2 };
        let r = t.facet_refresh(&q).unwrap();
        let (counts, pass, sum, mask) = naive_facets(&dims, &[3, 2], &selected, &amounts);
        assert_eq!(r.counts, counts, "selected={selected:?}");
        assert_eq!(r.pass_count, pass);
        assert!((r.sum - sum).abs() < 1e-9);
        assert_eq!(r.mask, mask);
    }
}

#[test]
fn topk_and_gather() {
    let (file, _, amounts) = build_file();
    let src: &[u8] = &file;
    let mut t = Table::open(src).unwrap();

    let q = FacetQuery { dims: vec![0, 1], selected: vec![-1, 0], measure: 2 };
    let r = t.facet_refresh(&q).unwrap();
    let top = t.sort_topk(2, &r.mask, 3).unwrap();
    // "open" rows: 0,1,4,6,8,9,12,13 — top 3 amounts descending = rows 13,12,9
    assert_eq!(top, vec![13, 12, 9]);

    match t.gather(2, &top).unwrap() {
        GatherResult::Float(v) => {
            assert_eq!(v, vec![amounts[13], amounts[12], amounts[9]]);
        }
        _ => panic!("expected floats"),
    }
    match t.gather(0, &top).unwrap() {
        GatherResult::Text(v) => assert_eq!(v, vec!["us", "eu", "eu"]),
        _ => panic!("expected text"),
    }
}
