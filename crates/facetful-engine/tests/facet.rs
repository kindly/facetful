//! Engine correctness: facet_refresh / sort_topk / gather against a file built
//! in-code, verified against a naive row-by-row recomputation.

use facetful_engine::{FacetQuery, GatherResult, Table};
use facetful_format::write::{ColumnChunk, DictData, SegmentData, Writer};
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
    // status uses u8 codes to exercise both widths
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "region".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY },
            ColumnDef {
                name: "status".into(),
                ty: ColumnType::Utf8,
                flags: flags::DICTIONARY | flags::CODES_U8,
            },
            ColumnDef { name: "amount".into(), ty: ColumnType::Float64, flags: 0 },
        ],
    };
    let (roff, rbytes) = utf8_offsets(&["eu", "us", "asia"]);
    let (soff, sbytes) = utf8_offsets(&["open", "closed"]);

    let region_codes: Vec<u16> = vec![0, 1, 2, 0, 1, 2, 0, 1, 2, 0, 1, 2, 0, 1, 2];
    let status_codes: Vec<u8> = vec![0, 0, 1, 1, 0, 1, 0, 1, 0, 0, 1, 1, 0, 0, 1];
    let amounts: Vec<f64> = (0..15).map(|i| (i as f64) * 1.25 + 0.5).collect();

    let dicts = vec![
        Some(DictData { offsets: roff, bytes: rbytes }),
        Some(DictData { offsets: soff, bytes: sbytes }),
        None,
    ];
    let mut w = Writer::new(schema, vec![], 5, &dicts);
    for g in 0..3 {
        let s = g * 5;
        let amount_bytes: Vec<u8> = amounts[s..s + 5].iter().flat_map(|x| x.to_le_bytes()).collect();
        w.write_group(
            5,
            &[
                ColumnChunk { data: SegmentData::Codes16(&region_codes[s..s + 5]), validity: None, null_count: 0 },
                ColumnChunk { data: SegmentData::Codes8(&status_codes[s..s + 5]), validity: None, null_count: 0 },
                ColumnChunk { data: SegmentData::Fixed(&amount_bytes), validity: None, null_count: 0 },
            ],
        );
    }
    let status_u16: Vec<u16> = status_codes.iter().map(|&c| c as u16).collect();
    (w.finish(), vec![region_codes, status_u16], amounts)
}

fn naive_facets(
    dims: &[Vec<u16>],
    cards: &[usize],
    selected: &[Vec<u16>],
    amounts: &[f64],
) -> (Vec<Vec<u32>>, u64, f64, Vec<u8>) {
    let n = amounts.len();
    let d = dims.len();
    let pass_dim = |j: usize, row: usize| -> bool {
        selected[j].is_empty() || selected[j].contains(&dims[j][row])
    };
    let mut counts: Vec<Vec<u32>> = cards.iter().map(|&c| vec![0; c]).collect();
    let mut mask = vec![0u8; n];
    let (mut pass, mut sum) = (0u64, 0f64);
    for row in 0..n {
        for k in 0..d {
            // counted under all filters except k's own
            if (0..d).all(|j| j == k || pass_dim(j, row)) {
                counts[k][dims[k][row] as usize] += 1;
            }
        }
        if (0..d).all(|j| pass_dim(j, row)) {
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

    let cases: Vec<Vec<Vec<u16>>> = vec![
        vec![vec![], vec![]],
        vec![vec![0], vec![]],
        vec![vec![], vec![1]],
        vec![vec![2], vec![0]],
        vec![vec![0, 2], vec![]],      // multi-select on region
        vec![vec![0, 1], vec![1]],     // multi-select + single
        vec![vec![0, 1, 2], vec![0, 1]], // everything selected = no-op filters
    ];
    for selected in cases {
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

    let q = FacetQuery { dims: vec![0, 1], selected: vec![vec![], vec![0]], measure: 2 };
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

#[test]
fn null_measure_sums_and_topk() {
    // 1 group x 8 rows; amounts 0..8, rows 2 and 5 are null
    let schema = Schema {
        columns: vec![
            ColumnDef {
                name: "status".into(),
                ty: ColumnType::Utf8,
                flags: flags::DICTIONARY | flags::CODES_U8,
            },
            ColumnDef { name: "amount".into(), ty: ColumnType::Float64, flags: 0 },
        ],
    };
    let (soff, sbytes) = utf8_offsets(&["a", "b"]);
    let dicts = vec![Some(DictData { offsets: soff, bytes: sbytes }), None];
    let mut w = Writer::new(schema, vec![], 8, &dicts);
    let codes: [u8; 8] = [0, 1, 0, 1, 0, 1, 0, 1];
    let amounts: Vec<f64> = (0..8).map(|i| i as f64).collect();
    let amount_bytes: Vec<u8> = amounts.iter().flat_map(|x| x.to_le_bytes()).collect();
    let validity: [u8; 1] = [0b1101_1011]; // rows 2 and 5 null
    w.write_group(
        8,
        &[
            ColumnChunk { data: SegmentData::Codes8(&codes), validity: None, null_count: 0 },
            ColumnChunk {
                data: SegmentData::Fixed(&amount_bytes),
                validity: Some(&validity),
                null_count: 2,
            },
        ],
    );
    let file = w.finish();
    let src: &[u8] = &file;
    let mut t = Table::open(src).unwrap();

    let q = FacetQuery { dims: vec![0], selected: vec![vec![]], measure: 1 };
    let r = t.facet_refresh(&q).unwrap();
    assert_eq!(r.pass_count, 8); // count(*) includes null-measure rows
    assert_eq!(r.sum, 0.0 + 1.0 + 3.0 + 4.0 + 6.0 + 7.0); // sum() skips nulls
    let top = t.sort_topk(1, &r.mask, 3).unwrap();
    assert_eq!(top, vec![7, 6, 4]); // 5 is null -> excluded

    // stats skip nulls: max is 7, min 0 (both valid rows)
    match t.catalog().groups[0].cols[1].stats {
        facetful_engine::format::Stats::Float { min, max } => {
            assert_eq!(min, 0.0);
            assert_eq!(max, 7.0);
        }
        _ => panic!("expected float stats"),
    }
}
