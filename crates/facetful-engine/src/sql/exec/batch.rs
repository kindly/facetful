//! Fused execution of a family of facet-shaped statements (design.sv d59/d60).
//!
//! A facet UI's interaction is N `select dim, count(*) … group by dim`
//! statements that differ only in their WHERE, plus a totals query. Run one
//! by one they each plan, load lanes, build a group table and marshal a
//! result; run here they share masks (the cache) and lanes (one load per
//! column per row group), and each statement is a few lean passes over
//! (mask, codes, measures) into dense per-lane accumulators. The results
//! are exactly what `execute` would produce, column for column.

use super::*;

/// The GROUP BY key, when there is one.
#[derive(Clone, Copy)]
enum Key {
    /// a dictionary column: lane = code
    Dict(usize),
    /// a narrow-range integer/date column: lane = value - min
    Int { col: usize, min: i64 },
}

/// What one select item emits.
#[derive(Clone, Copy)]
enum Item {
    /// the group key itself
    Key,
    CountStar,
    /// `count(col)`: rows where `col` is not NULL
    CountCol(usize),
    /// `count(distinct col)` over a dictionary column (a bitset per lane)
    CountDistinct(usize),
    /// `sum` / `min` / `max` over an int lane (NULL when no row contributes)
    SumInt(usize),
    MinInt(usize),
    MaxInt(usize),
    /// … and over a float lane
    SumFloat(usize),
    MinFloat(usize),
    MaxFloat(usize),
}

/// A statement the fused kernel can run.
pub(super) struct Shape {
    key: Option<Key>,
    /// lanes including the NULL lane (the last)
    lanes: usize,
    items: Vec<Item>,
    /// per item: the dictionary size behind a `CountDistinct`
    cards: Vec<usize>,
    /// (select item index, descending) per ORDER BY key
    order: Vec<(usize, bool)>,
}

/// Dense accumulators past this many lanes are not worth their memory.
const MAX_LANES: usize = 1 << 20;
/// Bits budget for one statement's `count(distinct)` bitsets.
const MAX_DISTINCT_BITS: usize = 1 << 24;

/// Classify a bound query; None when the fused kernel cannot run it
/// (the caller executes it the ordinary way).
pub(super) fn shape_of<S: ReadAt>(table: &mut Table<S>, q: &BoundQuery) -> Option<Shape> {
    if !q.is_aggregate || q.group_by.len() > 1 {
        return None;
    }
    // an unfiltered totals query shares nothing with the family and the
    // ordinary path accumulates all its items in one pass; here each item
    // would be a pass over every row (measured slower at 1.5M rows)
    if q.group_by.is_empty() && q.filter.is_none() {
        return None;
    }
    let (key, lanes) = match q.group_by.first() {
        None => (None, 1),
        Some(Bound::Column { index, .. }) => match dense_dim(table, *index)? {
            (DirectDim::Dict, lanes) => (Some(Key::Dict(*index)), lanes),
            (DirectDim::Int { min }, lanes) => (Some(Key::Int { col: *index, min }), lanes),
        },
        Some(_) => return None,
    };
    if lanes > MAX_LANES {
        return None;
    }
    let key_col = key.map(|k| match k {
        Key::Dict(c) | Key::Int { col: c, .. } => c,
    });
    let schema = table.catalog().schema.clone();
    let numeric = |index: usize| -> Option<bool> {
        let def = &schema.columns[index];
        if def.is_dict() {
            return None;
        }
        match def.ty {
            ColumnType::Float64 => Some(true),
            ColumnType::Int8 | ColumnType::Int16 | ColumnType::Int32 | ColumnType::Int64 | ColumnType::Date | ColumnType::Timestamp => Some(false),
            _ => None,
        }
    };
    let mut items = Vec::with_capacity(q.select.len());
    let mut cards = Vec::with_capacity(q.select.len());
    let mut distinct_bits = 0usize;
    for s in &q.select {
        let mut card = 0;
        let item = match &s.expr {
            Bound::Column { index, .. } if Some(*index) == key_col => Item::Key,
            Bound::Call { func, args, .. } if func.kind == FuncKind::Aggregate && args.len() == 1 => match (func.name, &args[0]) {
                ("count", Bound::Number(_, false)) => Item::CountStar,
                ("count", Bound::Column { index, .. }) => Item::CountCol(*index),
                ("count_distinct", Bound::Column { index, .. }) => {
                    if !schema.columns[*index].is_dict() {
                        return None;
                    }
                    card = table.dictionary(*index).ok()?.len();
                    distinct_bits += lanes * card;
                    if distinct_bits > MAX_DISTINCT_BITS {
                        return None;
                    }
                    Item::CountDistinct(*index)
                }
                (name @ ("sum" | "min" | "max"), Bound::Column { index, .. }) => match (name, numeric(*index)?) {
                    ("sum", true) => Item::SumFloat(*index),
                    ("sum", false) => Item::SumInt(*index),
                    ("min", true) => Item::MinFloat(*index),
                    ("min", false) => Item::MinInt(*index),
                    ("max", true) => Item::MaxFloat(*index),
                    (_, false) => Item::MaxInt(*index),
                    _ => unreachable!(),
                },
                _ => return None,
            },
            _ => return None,
        };
        items.push(item);
        cards.push(card);
    }
    let mut order = Vec::with_capacity(q.order_by.len());
    for (e, dir) in &q.order_by {
        let si = q.select.iter().position(|s| &s.expr == e)?;
        order.push((si, *dir == SortDir::Desc));
    }
    Some(Shape { key, lanes, items, cards, order })
}

/// Dense per-lane accumulators for one statement (the last lane = NULL key;
/// a totals query has one lane).
struct Acc {
    rows: Vec<u64>,
    /// per select item, by lane: counts / int and float sums or extremes /
    /// contributing rows (NULL when 0) / distinct bitsets (lanes × card bits)
    counts: Vec<Vec<u64>>,
    ints: Vec<Vec<i64>>,
    floats: Vec<Vec<f64>>,
    contrib: Vec<Vec<u64>>,
    distinct: Vec<Vec<u64>>,
}

impl Acc {
    fn new(shape: &Shape) -> Acc {
        let lanes = shape.lanes;
        let on = |f: &dyn Fn(&Item) -> bool| -> Vec<Vec<u64>> { shape.items.iter().map(|i| if f(i) { vec![0; lanes] } else { Vec::new() }).collect() };
        Acc {
            rows: vec![0; lanes],
            counts: on(&|i| matches!(i, Item::CountCol(_))),
            ints: shape
                .items
                .iter()
                .map(|i| match i {
                    Item::SumInt(_) => vec![0i64; lanes],
                    Item::MinInt(_) => vec![i64::MAX; lanes],
                    Item::MaxInt(_) => vec![i64::MIN; lanes],
                    _ => Vec::new(),
                })
                .collect(),
            floats: shape
                .items
                .iter()
                .map(|i| match i {
                    Item::SumFloat(_) => vec![0f64; lanes],
                    Item::MinFloat(_) => vec![f64::INFINITY; lanes],
                    Item::MaxFloat(_) => vec![f64::NEG_INFINITY; lanes],
                    _ => Vec::new(),
                })
                .collect(),
            contrib: on(&|i| matches!(i, Item::SumInt(_) | Item::SumFloat(_) | Item::MinInt(_) | Item::MaxInt(_) | Item::MinFloat(_) | Item::MaxFloat(_))),
            distinct: shape
                .items
                .iter()
                .zip(&shape.cards)
                .map(|(i, &card)| if matches!(i, Item::CountDistinct(_)) { vec![0u64; (lanes * card).div_ceil(64)] } else { Vec::new() })
                .collect(),
        }
    }
}

/// A loaded lane for the kernel: values by row plus validity (all ones when
/// the column has no nulls).
#[derive(Clone, Copy)]
enum Lane<'a> {
    I64(&'a [i64], &'a [u8]),
    F64(&'a [f64], &'a [u8]),
    Codes(&'a [u16], &'a [u8]),
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
    let mut accs: Vec<Acc> = shapes.iter().map(Acc::new).collect();
    let mut scanned = vec![0usize; qs.len()];

    for g in 0..table.group_count() {
        let rows = table.group_rows(g);
        // lanes and masks shared by every statement of the family for this group
        let mut cols: Cols = HashMap::new();
        let ones = vec![0xFFu8; rows.div_ceil(8)];
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
            let mut need: Vec<usize> = Vec::new();
            if let Some(Key::Dict(c) | Key::Int { col: c, .. }) = shape.key {
                need.push(c);
            }
            for it in &shape.items {
                match it {
                    Item::Key | Item::CountStar => {}
                    Item::CountCol(c) | Item::CountDistinct(c) | Item::SumInt(c) | Item::MinInt(c) | Item::MaxInt(c) | Item::SumFloat(c) | Item::MinFloat(c) | Item::MaxFloat(c) => need.push(*c),
                }
            }
            need.sort_unstable();
            need.dedup();
            sh.load(table, g, &mut cols, &need, rows)?;

            let lane_for = |c: usize| -> Lane<'_> {
                match &cols[&c] {
                    (GroupCol::I64(v), valid) => Lane::I64(v, valid.as_deref().map_or(ones.as_slice(), |v| v.as_slice())),
                    (GroupCol::F64(v), valid) => Lane::F64(v, valid.as_deref().map_or(ones.as_slice(), |v| v.as_slice())),
                    (GroupCol::Dict { codes, .. }, valid) => Lane::Codes(codes, valid.as_deref().map_or(ones.as_slice(), |v| v.as_slice())),
                    _ => unreachable!("a loaded lane"),
                }
            };
            // the key: lane per row; a missing validity bitmap reads as all
            // ones, so one loop shape serves every case
            let null_lane = shape.lanes - 1;
            let key_lane = shape.key.map(|k| match k {
                Key::Dict(c) => (lane_for(c), 0i64),
                Key::Int { col, min } => (lane_for(col), min),
            });
            let lane_of = |row: usize| -> usize {
                match key_lane {
                    None => 0,
                    Some((Lane::Codes(codes, valid), _)) => if valid[row / 8] >> (row % 8) & 1 != 0 { codes[row] as usize } else { null_lane },
                    Some((Lane::I64(vals, valid), min)) => if valid[row / 8] >> (row % 8) & 1 != 0 { (vals[row] - min) as usize } else { null_lane },
                    Some((Lane::F64(..), _)) => unreachable!("a float key is never dense"),
                }
            };
            let acc = &mut accs[i];
            // unfiltered: a plain loop; filtered: walk the set bits, so a
            // selective filter costs its rows, not the group's
            let dense = keep.iter().enumerate().all(|(wi, &w)| w == if wi + 1 == keep.len() && rows % 64 != 0 { (1u64 << (rows % 64)) - 1 } else { u64::MAX });
            macro_rules! visit {
                ($body:expr) => {
                    if dense {
                        for row in 0..rows {
                            $body(row);
                        }
                    } else {
                        for (wi, &w0) in keep.iter().enumerate() {
                            let mut w = w0;
                            while w != 0 {
                                let row = wi * 64 + w.trailing_zeros() as usize;
                                w &= w - 1;
                                $body(row);
                            }
                        }
                    }
                };
            }
            if key_lane.is_none() {
                acc.rows[0] += keep.iter().map(|w| w.count_ones() as u64).sum::<u64>();
            } else {
                let counts = &mut acc.rows;
                visit!(|row: usize| counts[lane_of(row)] += 1);
            }
            for (si, it) in shape.items.iter().enumerate() {
                let valid_of = |c: usize| match lane_for(c) {
                    Lane::I64(_, v) | Lane::F64(_, v) | Lane::Codes(_, v) => v,
                };
                match it {
                    Item::Key | Item::CountStar => {}
                    Item::CountCol(c) => {
                        let v = valid_of(*c);
                        let out = &mut acc.counts[si];
                        visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { out[lane_of(row)] += 1 });
                    }
                    Item::CountDistinct(c) => {
                        let Lane::Codes(codes, v) = lane_for(*c) else { unreachable!() };
                        let card = shape.cards[si];
                        let bits = &mut acc.distinct[si];
                        visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 {
                            let b = lane_of(row) * card + codes[row] as usize;
                            bits[b / 64] |= 1u64 << (b % 64);
                        });
                    }
                    Item::SumInt(c) | Item::MinInt(c) | Item::MaxInt(c) => {
                        let Lane::I64(vals, v) = lane_for(*c) else { unreachable!() };
                        let (out, nn) = (&mut acc.ints[si], &mut acc.contrib[si]);
                        match it {
                            Item::SumInt(_) => visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { let l = lane_of(row); out[l] += vals[row]; nn[l] += 1 }),
                            Item::MinInt(_) => visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { let l = lane_of(row); out[l] = out[l].min(vals[row]); nn[l] += 1 }),
                            _ => visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { let l = lane_of(row); out[l] = out[l].max(vals[row]); nn[l] += 1 }),
                        }
                    }
                    Item::SumFloat(c) | Item::MinFloat(c) | Item::MaxFloat(c) => {
                        let Lane::F64(vals, v) = lane_for(*c) else { unreachable!() };
                        let (out, nn) = (&mut acc.floats[si], &mut acc.contrib[si]);
                        match it {
                            Item::SumFloat(_) => visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { let l = lane_of(row); out[l] += vals[row]; nn[l] += 1 }),
                            Item::MinFloat(_) => visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { let l = lane_of(row); out[l] = out[l].min(vals[row]); nn[l] += 1 }),
                            _ => visit!(|row: usize| if v[row / 8] >> (row % 8) & 1 != 0 { let l = lane_of(row); out[l] = out[l].max(vals[row]); nn[l] += 1 }),
                        }
                    }
                }
            }
        }
    }

    // results: present lanes in lane order (NULL last), then ORDER BY / window
    let mut out = Vec::with_capacity(qs.len());
    for (i, (q, shape)) in qs.iter().zip(shapes).enumerate() {
        let sh = &shs[i];
        let acc = &accs[i];
        let lanes = shape.lanes;
        let null_lane = lanes - 1;
        let mut present: Vec<usize> = match shape.key {
            Some(_) => (0..lanes).filter(|&l| acc.rows[l] > 0).collect(),
            None => vec![0], // a totals query always has one row
        };
        let distinct_count = |si: usize, l: usize| -> i64 {
            let card = shape.cards[si];
            let (from, to) = (l * card, (l + 1) * card);
            let bits = &acc.distinct[si];
            (from..to).filter(|&b| bits[b / 64] >> (b % 64) & 1 != 0).count() as i64
        };
        let val = |si: usize, l: usize| -> Val {
            match shape.items[si] {
                Item::Key => match shape.key {
                    _ if l == null_lane => Val::Null,
                    Some(Key::Int { min, .. }) => Val::Int(min + l as i64),
                    _ => Val::Int(l as i64), // a dictionary key sorts by its string, handled below
                },
                Item::CountStar => Val::Int(acc.rows[l] as i64),
                Item::CountCol(_) => Val::Int(acc.counts[si][l] as i64),
                Item::CountDistinct(_) => Val::Int(distinct_count(si, l)),
                Item::SumInt(_) | Item::MinInt(_) | Item::MaxInt(_) => if acc.contrib[si][l] > 0 { Val::Int(acc.ints[si][l]) } else { Val::Null },
                Item::SumFloat(_) | Item::MinFloat(_) | Item::MaxFloat(_) => if acc.contrib[si][l] > 0 { Val::Float(acc.floats[si][l]) } else { Val::Null },
            }
        };
        if !shape.order.is_empty() {
            let key_dict = match shape.key {
                Some(Key::Dict(k)) => Some(sh.dicts[&k].clone()),
                _ => None,
            };
            let keys: Vec<Vec<Val>> = present
                .iter()
                .map(|&l| {
                    shape
                        .order
                        .iter()
                        .map(|&(si, _)| match (shape.items[si], &key_dict) {
                            (Item::Key, Some(d)) if l < null_lane => Val::Text(d[l].clone()),
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
        let valid_where = |f: &dyn Fn(usize) -> bool| -> Vec<u8> {
            let mut v = vec![0u8; n.div_ceil(8)];
            for (b, &l) in present.iter().enumerate() {
                if f(l) {
                    v[b / 8] |= 1 << (b % 8);
                }
            }
            v
        };
        let cols: Vec<OutCol> = shape
            .items
            .iter()
            .enumerate()
            .map(|(si, it)| match it {
                Item::Key => match shape.key.expect("a key item needs a key") {
                    Key::Dict(k) => OutCol::Dict {
                        codes: present.iter().map(|&l| if l < null_lane { l as u16 } else { 0 }).collect(),
                        dict: sh.dicts[&k].clone(),
                        valid: valid_where(&|l| l < null_lane),
                    },
                    Key::Int { min, .. } => OutCol::I64 {
                        v: present.iter().map(|&l| if l < null_lane { min + l as i64 } else { 0 }).collect(),
                        valid: valid_where(&|l| l < null_lane),
                    },
                },
                Item::CountStar => OutCol::I64 { v: present.iter().map(|&l| acc.rows[l] as i64).collect(), valid: valid_all.clone() },
                Item::CountCol(_) => OutCol::I64 { v: present.iter().map(|&l| acc.counts[si][l] as i64).collect(), valid: valid_all.clone() },
                Item::CountDistinct(_) => OutCol::I64 { v: present.iter().map(|&l| distinct_count(si, l)).collect(), valid: valid_all.clone() },
                Item::SumInt(_) | Item::MinInt(_) | Item::MaxInt(_) => OutCol::I64 {
                    v: present.iter().map(|&l| acc.ints[si][l]).collect(),
                    valid: valid_where(&|l| acc.contrib[si][l] > 0),
                },
                Item::SumFloat(_) | Item::MinFloat(_) | Item::MaxFloat(_) => OutCol::F64 {
                    v: present.iter().map(|&l| acc.floats[si][l]).collect(),
                    valid: valid_where(&|l| acc.contrib[si][l] > 0),
                },
            })
            .collect();
        let mut r = sh.result(table, Vec::new(), Some(cols), n);
        r.scanned_groups = scanned[i];
        out.push(r);
    }
    Ok(out)
}
