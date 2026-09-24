//! Fused execution of a family of facet-shaped statements (design.sv d59).
//!
//! A facet UI's interaction is N `select dim, count(*) … group by dim`
//! statements that differ only in their WHERE, plus a totals query. Run one
//! by one they each plan, load lanes, build a group table and marshal a
//! result; run here they share masks (the cache) and lanes (one load per
//! column per row group), and each statement is one lean pass over
//! (mask, codes, measures) into dense per-code accumulators. The results
//! are exactly what `execute` would produce, column for column.

use super::*;

/// What one select item emits.
#[derive(Clone, Copy)]
enum Item {
    /// the group key itself
    Key,
    CountStar,
    /// `count(col)`: rows where `col` is not NULL
    CountCol(usize),
    /// `sum(col)` over an int lane (SQL: NULL when no row contributes)
    SumInt(usize),
    /// `sum(col)` over a float lane
    SumFloat(usize),
}

/// A statement the fused kernel can run: at most one dictionary GROUP BY key,
/// select items from `Item`, any WHERE (masks are general), ORDER BY over
/// select items, LIMIT/OFFSET.
pub(super) struct Shape {
    key: Option<usize>,
    items: Vec<Item>,
    /// (select item index, descending) per ORDER BY key
    order: Vec<(usize, bool)>,
}

fn column_of(b: &Bound) -> Option<usize> {
    match b {
        Bound::Column { index, .. } => Some(*index),
        _ => None,
    }
}

/// Classify a bound query; None when the fused kernel cannot run it
/// (the caller executes it the ordinary way).
pub(super) fn shape_of<S: ReadAt>(table: &mut Table<S>, q: &BoundQuery) -> Option<Shape> {
    if !q.is_aggregate || q.group_by.len() > 1 {
        return None;
    }
    let key = match q.group_by.first() {
        None => None,
        Some(b) => {
            let index = column_of(b)?;
            match dense_dim(table, index)? {
                (DirectDim::Dict, _) => Some(index),
                _ => return None,
            }
        }
    };
    let numeric = |index: usize| -> Option<Item> {
        let def = &table.catalog().schema.columns[index];
        if def.is_dict() {
            return None;
        }
        match def.ty {
            ColumnType::Float64 => Some(Item::SumFloat(index)),
            ColumnType::Int8 | ColumnType::Int16 | ColumnType::Int32 | ColumnType::Int64 => Some(Item::SumInt(index)),
            _ => None,
        }
    };
    let mut items = Vec::with_capacity(q.select.len());
    for s in &q.select {
        let item = match &s.expr {
            Bound::Column { index, .. } if Some(*index) == key => Item::Key,
            Bound::Call { func, args, .. } if func.kind == FuncKind::Aggregate && args.len() == 1 => match (func.name, &args[0]) {
                ("count", Bound::Number(_, false)) => Item::CountStar,
                ("count", Bound::Column { index, .. }) => Item::CountCol(*index),
                ("sum", Bound::Column { index, .. }) => numeric(*index)?,
                _ => return None,
            },
            _ => return None,
        };
        items.push(item);
    }
    let mut order = Vec::with_capacity(q.order_by.len());
    for (e, dir) in &q.order_by {
        let si = q.select.iter().position(|s| &s.expr == e)?;
        order.push((si, *dir == SortDir::Desc));
    }
    Some(Shape { key, items, order })
}

/// Dense per-lane accumulators for one statement (lane = dictionary code,
/// the last lane = NULL key; a totals query has one lane).
struct Acc {
    rows: Vec<u64>,
    /// per select item: counts / sums by lane (`Key` and `CountStar` use `rows`)
    counts: Vec<Vec<u64>>,
    sums_i: Vec<Vec<i64>>,
    sums_f: Vec<Vec<f64>>,
    /// per select item: rows that contributed to a sum (NULL sum when 0)
    contrib: Vec<Vec<u64>>,
}

impl Acc {
    fn new(lanes: usize, items: &[Item]) -> Acc {
        let per = |on: bool| if on { vec![0; lanes] } else { Vec::new() };
        Acc {
            rows: vec![0; lanes],
            counts: items.iter().map(|i| per(matches!(i, Item::CountCol(_)))).collect(),
            sums_i: items.iter().map(|i| if matches!(i, Item::SumInt(_)) { vec![0i64; lanes] } else { Vec::new() }).collect(),
            sums_f: items.iter().map(|i| if matches!(i, Item::SumFloat(_)) { vec![0f64; lanes] } else { Vec::new() }).collect(),
            contrib: items.iter().map(|i| per(matches!(i, Item::SumInt(_) | Item::SumFloat(_)))).collect(),
        }
    }
}

/// A loaded lane for the kernel: values by row plus validity (all ones when
/// the column has no nulls).
#[derive(Clone, Copy)]
enum Lane<'a> {
    I64(&'a [i64], &'a [u8]),
    F64(&'a [f64], &'a [u8]),
}

/// AND a cached packed mask (bit i = byte i/8, bit i%8) into 64-bit words.
fn and_words(words: &mut [u64], bits: &[u8]) {
    for (i, w) in words.iter_mut().enumerate() {
        let s = &bits[(i * 8).min(bits.len())..(i * 8 + 8).min(bits.len())];
        let mut b = [0u8; 8];
        b[..s.len()].copy_from_slice(s);
        *w &= u64::from_le_bytes(b);
    }
}

/// The rows a statement keeps in group `g`, as packed words (all set when
/// there is no WHERE): the cached conjunct masks ANDed word by word; a cache
/// miss goes through `where_mask`, which computes and caches it.
fn keep_words<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    sh: &Shared,
    cols: &mut Cols,
) -> Result<Vec<u64>, FormatError> {
    let nw = rows.div_ceil(64);
    let mut words = vec![u64::MAX; nw];
    let mut hit = true;
    for c in &sh.conjuncts {
        match table.masks().get(&c.key, g) {
            Some(bits) => and_words(&mut words, &bits),
            None => {
                hit = false;
                break;
            }
        }
    }
    if !hit {
        let bytes = where_mask(table, g, rows, &sh.conjuncts, &sh.dicts, cols)?.expect("conjuncts present");
        words = vec![0u64; nw];
        for (i, &b) in bytes.iter().enumerate() {
            if b != 0 {
                words[i / 64] |= 1u64 << (i % 64);
            }
        }
    }
    if rows % 64 != 0 {
        words[nw - 1] &= (1u64 << (rows % 64)) - 1;
    }
    Ok(words)
}

/// Run a family of fusable statements (`shapes[i]` classifies `qs[i]`).
pub(super) fn execute_family<S: ReadAt>(
    table: &mut Table<S>,
    qs: &[&BoundQuery],
    shapes: &[Shape],
) -> Result<Vec<QueryResult>, FormatError> {
    let shs: Vec<Shared> = qs.iter().map(|q| Shared::new(table, q)).collect::<Result<_, _>>()?;
    let lanes_of: Vec<usize> = shapes
        .iter()
        .map(|sh| match sh.key {
            Some(k) => table.dictionary(k).map(|d| d.len() + 1),
            None => Ok(1),
        })
        .collect::<Result<_, _>>()?;
    let mut accs: Vec<Acc> = shapes.iter().zip(&lanes_of).map(|(sh, &l)| Acc::new(l, &sh.items)).collect();
    let mut scanned = vec![0usize; qs.len()];

    for g in 0..table.group_count() {
        let rows = table.group_rows(g);
        // lanes and masks shared by every statement of the family for this group
        let mut cols: Cols = HashMap::new();
        for (i, sh) in shs.iter().enumerate() {
            if group_prunable(table, g, &sh.constraints) {
                continue;
            }
            scanned[i] += 1;
            let keep = keep_words(table, g, rows, sh, &mut cols)?;
            if keep.iter().all(|&x| x == 0) {
                continue;
            }
            let shape = &shapes[i];
            let mut need: Vec<usize> = shape.key.into_iter().collect();
            for it in &shape.items {
                if let Item::CountCol(c) | Item::SumInt(c) | Item::SumFloat(c) = it {
                    need.push(*c);
                }
            }
            need.sort_unstable();
            need.dedup();
            sh.load(table, g, &mut cols, &need, rows)?;

            // one loop shape for everything: a missing validity bitmap reads
            // as all ones, a missing key as lane 0 — the extra load per row
            // costs less than the code of the specialized variants it saves
            let ones = vec![0xFFu8; rows.div_ceil(8)];
            let lanes = lanes_of[i];
            let null_lane = lanes - 1;
            let (key_codes, key_valid): (&[u16], &[u8]) = match shape.key {
                Some(k) => match &cols[&k] {
                    (GroupCol::Dict { codes, .. }, valid) => (codes.as_slice(), valid.as_deref().map_or(ones.as_slice(), |v| v.as_slice())),
                    _ => unreachable!("a dense dictionary key loads as codes"),
                },
                None => (&[], &ones),
            };
            let lane_of = |row: usize| -> usize {
                if key_codes.is_empty() {
                    0
                } else if key_valid[row / 8] >> (row % 8) & 1 != 0 {
                    key_codes[row] as usize
                } else {
                    null_lane
                }
            };
            let acc = &mut accs[i];
            // one pass over the kept rows for the count, then one per further
            // item, each a loop with nothing in it but the lookups and the
            // increment; word skipping over the mask makes a selective filter
            // cost its rows, not the group's
            macro_rules! visit {
                ($body:expr) => {
                    for (wi, &w0) in keep.iter().enumerate() {
                        let mut w = w0;
                        while w != 0 {
                            let row = wi * 64 + w.trailing_zeros() as usize;
                            w &= w - 1;
                            $body(row);
                        }
                    }
                };
            }
            if key_codes.is_empty() {
                acc.rows[0] += keep.iter().map(|w| w.count_ones() as u64).sum::<u64>();
            } else {
                let counts = &mut acc.rows;
                visit!(|row: usize| counts[lane_of(row)] += 1);
            }
            for (si, it) in shape.items.iter().enumerate() {
                let lane = |c: usize| match &cols[&c] {
                    (GroupCol::I64(v), valid) => Lane::I64(v, valid.as_deref().map_or(ones.as_slice(), |v| v.as_slice())),
                    (GroupCol::F64(v), valid) => Lane::F64(v, valid.as_deref().map_or(ones.as_slice(), |v| v.as_slice())),
                    _ => unreachable!("a numeric lane"),
                };
                match it {
                    Item::Key | Item::CountStar => {}
                    Item::CountCol(c) => {
                        let (Lane::I64(_, v) | Lane::F64(_, v)) = lane(*c);
                        let out = &mut acc.counts[si];
                        visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { out[lane_of(row)] += 1 });
                    }
                    Item::SumInt(c) => {
                        let Lane::I64(vals, v) = lane(*c) else { unreachable!() };
                        let (out, nn) = (&mut acc.sums_i[si], &mut acc.contrib[si]);
                        visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { let l = lane_of(row); out[l] += vals[row]; nn[l] += 1 });
                    }
                    Item::SumFloat(c) => {
                        let Lane::F64(vals, v) = lane(*c) else { unreachable!() };
                        let (out, nn) = (&mut acc.sums_f[si], &mut acc.contrib[si]);
                        visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { let l = lane_of(row); out[l] += vals[row]; nn[l] += 1 });
                    }
                }
            }
        }
    }

    // results: present lanes in code order (NULL last), then ORDER BY / window
    let mut out = Vec::with_capacity(qs.len());
    for (i, (q, shape)) in qs.iter().zip(shapes).enumerate() {
        let sh = &shs[i];
        let acc = &accs[i];
        let lanes = lanes_of[i];
        let mut present: Vec<usize> = match shape.key {
            Some(_) => (0..lanes).filter(|&l| acc.rows[l] > 0).collect(),
            None => vec![0], // a totals query always has one row
        };
        let val = |si: usize, l: usize| -> Val {
            match shape.items[si] {
                Item::Key => {
                    if l == lanes - 1 { Val::Null } else { Val::Int(l as i64) } // ordered by code: dictionary order
                }
                Item::CountStar => Val::Int(acc.rows[l] as i64),
                Item::CountCol(_) => Val::Int(acc.counts[si][l] as i64),
                Item::SumInt(_) => if acc.contrib[si][l] > 0 { Val::Int(acc.sums_i[si][l]) } else { Val::Null },
                Item::SumFloat(_) => if acc.contrib[si][l] > 0 { Val::Float(acc.sums_f[si][l]) } else { Val::Null },
            }
        };
        if !shape.order.is_empty() {
            // a text key sorts by its string, not its code
            let key_dict = shape.key.map(|k| sh.dicts[&k].clone());
            let keys: Vec<Vec<Val>> = present
                .iter()
                .map(|&l| {
                    shape
                        .order
                        .iter()
                        .map(|&(si, _)| match (shape.items[si], &key_dict) {
                            (Item::Key, Some(d)) if l < lanes - 1 => Val::Text(d[l].clone()),
                            _ => val(si, l),
                        })
                        .collect()
                })
                .collect();
            let mut perm: Vec<u32> = (0..present.len() as u32).collect();
            sort_perm_keys(&mut perm, &keys, &q.order_by);
            present = perm.into_iter().map(|p| present[p as usize]).collect();
        }
        let (offset, limit) = sh.window();
        let present: Vec<usize> = present.into_iter().skip(offset).take(limit).collect();
        let n = present.len();
        let mut valid_all = vec![0u8; n.div_ceil(8)];
        for b in 0..n {
            valid_all[b / 8] |= 1 << (b % 8);
        }
        let cols: Vec<OutCol> = shape
            .items
            .iter()
            .enumerate()
            .map(|(si, it)| match it {
                Item::Key => {
                    let k = shape.key.expect("a key item needs a key");
                    let mut valid = vec![0u8; n.div_ceil(8)];
                    let codes = present
                        .iter()
                        .enumerate()
                        .map(|(b, &l)| {
                            if l < lanes - 1 {
                                valid[b / 8] |= 1 << (b % 8);
                                l as u16
                            } else {
                                0
                            }
                        })
                        .collect();
                    OutCol::Dict { codes, dict: sh.dicts[&k].clone(), valid }
                }
                Item::CountStar => OutCol::I64 { v: present.iter().map(|&l| acc.rows[l] as i64).collect(), valid: valid_all.clone() },
                Item::CountCol(_) => OutCol::I64 { v: present.iter().map(|&l| acc.counts[si][l] as i64).collect(), valid: valid_all.clone() },
                Item::SumInt(_) | Item::SumFloat(_) => {
                    let mut valid = vec![0u8; n.div_ceil(8)];
                    for (b, &l) in present.iter().enumerate() {
                        if acc.contrib[si][l] > 0 {
                            valid[b / 8] |= 1 << (b % 8);
                        }
                    }
                    if matches!(it, Item::SumInt(_)) {
                        OutCol::I64 { v: present.iter().map(|&l| acc.sums_i[si][l]).collect(), valid }
                    } else {
                        OutCol::F64 { v: present.iter().map(|&l| acc.sums_f[si][l]).collect(), valid }
                    }
                }
            })
            .collect();
        let mut r = sh.result(table, Vec::new(), Some(cols), n);
        r.scanned_groups = scanned[i];
        out.push(r);
    }
    Ok(out)
}
