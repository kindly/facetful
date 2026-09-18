//! Aggregate accumulators and their batch update loops.

use super::*;

pub(super) enum AggAcc {
    Count(Vec<i64>),
    /// One bitmap per SQL group; dictionary codes are file-global.
    DistinctCodes { bits: Vec<Vec<u64>>, words: usize },
    /// Boxed: inline it and `AggAcc` grows by half, which measurably slows the
    /// bitmap arms sharing this match.
    DistinctNum(Box<DistinctU64>),
    DistinctStr { sets: Vec<std::collections::HashSet<VStr, FxBuild>>, last: Vec<Option<VStr>> },
    SumI { v: Vec<i64>, any: Vec<bool> },
    SumF { v: Vec<f64>, any: Vec<bool> },
    Avg { sum: Vec<f64>, n: Vec<i64> },
    MinMaxNum { v: Vec<f64>, seen: Vec<bool>, is_min: bool, int: bool },
    MinMaxStr { v: Vec<Option<VStr>>, is_min: bool },
    /// keeps every value; finish() selects — memory is O(group rows)
    Median(Vec<Vec<f64>>),
    /// sample stddev via (n, Σx, Σx²)
    Stddev { n: Vec<i64>, sum: Vec<f64>, sumsq: Vec<f64> },
    GroupConcat { v: Vec<Option<String>>, sep: String },
}

/// Row source for one aggregation pass: explicit per-row group ids, or (for
/// ungrouped queries) the raw keep mask with the row count.
#[derive(Clone, Copy)]
pub(super) enum RowsSrc<'a> {
    Gids(&'a [u32]),
    Mask { keep: Option<&'a [u8]>, n: usize },
}

impl AggAcc {
    /// `dict_len`: cardinality when the argument is a direct dictionary column.
    pub(super) fn new(call: &Bound, dict_len: Option<usize>) -> AggAcc {
        let Bound::Call { func, args, .. } = call else { unreachable!() };
        let aty = args[0].ty();
        match func.name {
            "count" => AggAcc::Count(Vec::new()),
            "count_distinct" => {
                if let Some(len) = dict_len {
                    AggAcc::DistinctCodes { bits: Vec::new(), words: (len + 63) / 64 }
                } else if matches!(aty, Ty::Int | Ty::Float | Ty::Bool | Ty::Date | Ty::Timestamp) {
                    AggAcc::DistinctNum(Box::new(DistinctU64::new()))
                } else {
                    AggAcc::DistinctStr { sets: Vec::new(), last: Vec::new() }
                }
            }
            "sum" => {
                if matches!(aty, Ty::Int | Ty::Date | Ty::Timestamp) {
                    AggAcc::SumI { v: Vec::new(), any: Vec::new() }
                } else {
                    AggAcc::SumF { v: Vec::new(), any: Vec::new() }
                }
            }
            "avg" => AggAcc::Avg { sum: Vec::new(), n: Vec::new() },
            "median" => AggAcc::Median(Vec::new()),
            "stddev" => AggAcc::Stddev { n: Vec::new(), sum: Vec::new(), sumsq: Vec::new() },
            "group_concat" => {
                // binder guarantees the separator is a text literal
                let sep = match args.get(1) {
                    Some(Bound::Str(s)) => s.clone(),
                    _ => ",".to_string(),
                };
                AggAcc::GroupConcat { v: Vec::new(), sep }
            }
            "min" | "max" => {
                let is_min = func.name == "min";
                if aty == Ty::Text {
                    AggAcc::MinMaxStr { v: Vec::new(), is_min }
                } else {
                    AggAcc::MinMaxNum {
                        v: Vec::new(),
                        seen: Vec::new(),
                        is_min,
                        int: matches!(aty, Ty::Int | Ty::Date | Ty::Timestamp),
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    pub(super) fn grow(&mut self, n: usize) {
        // Dense bitmaps win for few groups, but a large dictionary crossed
        // with many tiny groups would waste memory. Cap them at 8 MiB per
        // aggregate, then retain only observed codes in the sparse fallback.
        if let AggAcc::DistinctCodes { bits, words } = self {
            if n > (1 << 20) / (*words).max(1) {
                let mut set = DistinctU64::new();
                set.grow(bits.len());
                for (g, bitmap) in bits.iter().enumerate() {
                    for (word_index, &word) in bitmap.iter().enumerate() {
                        let mut remaining = word;
                        while remaining != 0 {
                            let code = word_index * 64 + remaining.trailing_zeros() as usize;
                            set.insert_unfiltered(g as u32, code as u64);
                            remaining &= remaining - 1;
                        }
                    }
                }
                *self = AggAcc::DistinctNum(Box::new(set));
            }
        }
        match self {
            AggAcc::Count(v) => v.resize(n, 0),
            AggAcc::DistinctCodes { bits, words } => bits.resize_with(n, || vec![0; *words]),
            AggAcc::DistinctNum(v) => v.grow(n),
            AggAcc::DistinctStr { sets, last } => {
                sets.resize_with(n, Default::default);
                last.resize(n, None);
            }
            AggAcc::SumI { v, any } => {
                v.resize(n, 0);
                any.resize(n, false);
            }
            AggAcc::SumF { v, any } => {
                v.resize(n, 0.0);
                any.resize(n, false);
            }
            AggAcc::Avg { sum, n: c } => {
                sum.resize(n, 0.0);
                c.resize(n, 0);
            }
            AggAcc::MinMaxNum { v, seen, .. } => {
                v.resize(n, 0.0);
                seen.resize(n, false);
            }
            AggAcc::MinMaxStr { v, .. } => v.resize(n, None),
            AggAcc::Median(v) => v.resize_with(n, Default::default),
            AggAcc::Stddev { n: c, sum, sumsq } => {
                c.resize(n, 0);
                sum.resize(n, 0.0);
                sumsq.resize(n, 0.0);
            }
            AggAcc::GroupConcat { v, .. } => v.resize(n, None),
        }
    }

    /// One pass over the group: specialized loops per (accumulator, arg shape).
    /// `src` is either per-row group ids or (for ungrouped queries) the raw
    /// keep mask — the latter never materializes a gids vector.
    /// (RowsSrc is defined just above `impl AggAcc`.)
    pub(super) fn update_batch(&mut self, src: RowsSrc<'_>, arg: &VV) {
        macro_rules! for_kept {
            (|$i:ident, $g:ident| $body:expr) => {
                match src {
                    RowsSrc::Gids(gids) => {
                        for $i in 0..gids.len() {
                            let $g = gids[$i];
                            if $g == u32::MAX {
                                continue;
                            }
                            let $g = $g as usize;
                            $body
                        }
                    }
                    RowsSrc::Mask { keep: None, n } => {
                        for $i in 0..n {
                            let $g = 0usize;
                            $body
                        }
                    }
                    RowsSrc::Mask { keep: Some(k), n } => {
                        for $i in 0..n {
                            if k[$i] == 0 {
                                continue;
                            }
                            let $g = 0usize;
                            $body
                        }
                    }
                }
            };
        }
        match (&mut *self, &arg.data) {
            (AggAcc::Count(c), Data::Const(v)) => {
                if !v.is_null() {
                    // ungrouped count(*): the mask popcount is the answer
                    if let RowsSrc::Mask { keep, n } = src {
                        c[0] += match keep {
                            None => n as i64,
                            Some(k) => k.iter().filter(|&&b| b != 0).count() as i64,
                        };
                        return;
                    }
                    for_kept!(|i, g| {
                        let _ = i;
                        c[g] += 1
                    });
                }
            }
            (AggAcc::Count(c), _) => match &arg.valid {
                None => for_kept!(|i, g| {
                    let _ = i;
                    c[g] += 1
                }),
                Some(vb) => for_kept!(|i, g| {
                    c[g] += (vb[i / 8] >> (i % 8) & 1) as i64;
                }),
            },
            (AggAcc::SumF { v, any }, Data::F64(x)) => match &arg.valid {
                None => for_kept!(|i, g| {
                    v[g] += x[i];
                    any[g] = true;
                }),
                Some(vb) => for_kept!(|i, g| {
                    if vb[i / 8] >> (i % 8) & 1 != 0 {
                        v[g] += x[i];
                        any[g] = true;
                    }
                }),
            },
            (AggAcc::SumF { v, any }, Data::I64(x)) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    v[g] += x[i] as f64;
                    any[g] = true;
                }
            }),
            (AggAcc::SumI { v, any }, Data::I64(x)) => match &arg.valid {
                None => for_kept!(|i, g| {
                    v[g] += x[i];
                    any[g] = true;
                }),
                Some(vb) => for_kept!(|i, g| {
                    if vb[i / 8] >> (i % 8) & 1 != 0 {
                        v[g] += x[i];
                        any[g] = true;
                    }
                }),
            },
            (AggAcc::Avg { sum, n }, Data::F64(x)) => match &arg.valid {
                None => for_kept!(|i, g| {
                    sum[g] += x[i];
                    n[g] += 1;
                }),
                Some(vb) => for_kept!(|i, g| {
                    if vb[i / 8] >> (i % 8) & 1 != 0 {
                        sum[g] += x[i];
                        n[g] += 1;
                    }
                }),
            },
            (AggAcc::Avg { sum, n }, Data::I64(x)) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    sum[g] += x[i] as f64;
                    n[g] += 1;
                }
            }),
            (AggAcc::MinMaxNum { v, seen, is_min, .. }, Data::F64(_) | Data::I64(_)) => {
                let is_min = *is_min;
                for_kept!(|i, g| {
                    if arg.is_valid(i) {
                        let x = arg.f64_at(i);
                        if !seen[g] || (is_min && x < v[g]) || (!is_min && x > v[g]) {
                            v[g] = x;
                            seen[g] = true;
                        }
                    }
                });
            }
            (AggAcc::DistinctCodes { bits, .. }, Data::Codes { codes, dict }) => match &arg.valid {
                None => for_kept!(|i, g| {
                    let code = codes[i] as usize;
                    // Invalid codes are NULL, including codes in bitmap padding.
                    if code < dict.len() {
                        bits[g][code / 64] |= 1u64 << (code % 64);
                    }
                }),
                Some(vb) => for_kept!(|i, g| {
                    if vb[i / 8] >> (i % 8) & 1 != 0 {
                        let code = codes[i] as usize;
                        if code < dict.len() {
                            bits[g][code / 64] |= 1u64 << (code % 64);
                        }
                    }
                }),
            },
            (AggAcc::DistinctNum(sets), Data::Codes { codes, dict }) => for_kept!(|i, g| {
                if arg.is_valid(i) && (codes[i] as usize) < dict.len() {
                    sets.insert(g, codes[i] as u64);
                }
            }),
            (AggAcc::DistinctNum(sets), Data::I64(x)) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    sets.insert(g, x[i] as u64);
                }
            }),
            (AggAcc::DistinctNum(sets), Data::F64(x)) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    sets.insert(g, x[i].to_bits());
                }
            }),
            (AggAcc::DistinctStr { sets, last }, _) => for_kept!(|i, g| {
                if let Some(t) = arg.text_at(i) {
                    // dictionary lanes hand back the same Rc on every row, so
                    // the repeat check is a pointer compare before it is a
                    // string compare
                    if let Some(prev) = &last[g] {
                        if Rc::ptr_eq(prev, &t) || **prev == *t {
                            continue;
                        }
                    }
                    last[g] = Some(t.clone());
                    sets[g].insert(t);
                }
            }),
            (AggAcc::Median(vs), _) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    vs[g].push(arg.f64_at(i));
                }
            }),
            (AggAcc::Stddev { n, sum, sumsq }, _) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    let x = arg.f64_at(i);
                    n[g] += 1;
                    sum[g] += x;
                    sumsq[g] += x * x;
                }
            }),
            (AggAcc::GroupConcat { v, sep }, _) => for_kept!(|i, g| {
                if arg.is_valid(i) {
                    let piece = match lane_val(arg, i) {
                        Val::Text(s) => s.to_string(),
                        Val::Int(x) => x.to_string(),
                        Val::Float(x) => x.to_string(),
                        Val::Bool(b) => (if b { "1" } else { "0" }).to_string(),
                        Val::Null => continue,
                    };
                    match &mut v[g] {
                        Some(acc) => {
                            acc.push_str(sep);
                            acc.push_str(&piece);
                        }
                        None => v[g] = Some(piece),
                    }
                }
            }),
            // generic fallbacks (consts, computed vectors, text min/max)
            (acc, _) => {
                for_kept!(|i, g| {
                    if !arg.is_valid(i) {
                        continue;
                    }
                    match acc {
                        AggAcc::SumI { v, any } => {
                            v[g] += arg.i64_at(i);
                            any[g] = true;
                        }
                        AggAcc::SumF { v, any } => {
                            v[g] += arg.f64_at(i);
                            any[g] = true;
                        }
                        AggAcc::Avg { sum, n } => {
                            sum[g] += arg.f64_at(i);
                            n[g] += 1;
                        }
                        AggAcc::MinMaxNum { v, seen, is_min, .. } => {
                            let x = arg.f64_at(i);
                            if !seen[g] || (*is_min && x < v[g]) || (!*is_min && x > v[g]) {
                                v[g] = x;
                                seen[g] = true;
                            }
                        }
                        AggAcc::MinMaxStr { v, is_min } => {
                            if let Some(t) = arg.text_at(i) {
                                let better = match &v[g] {
                                    None => true,
                                    Some(cur) => {
                                        if *is_min { t < *cur } else { t > *cur }
                                    }
                                };
                                if better {
                                    v[g] = Some(t);
                                }
                            }
                        }
                        AggAcc::DistinctNum(sets) => {
                            let bits = match lane_val(arg, i) {
                                Val::Int(x) => x as u64,
                                Val::Float(x) => x.to_bits(),
                                Val::Bool(b) => b as u64,
                                _ => continue,
                            };
                            sets.insert(g, bits);
                        }
                        AggAcc::Count(c) => c[g] += 1,
                        // these have shape-generic arms above the fallback
                        AggAcc::DistinctCodes { .. }
                        | AggAcc::DistinctStr { .. }
                        | AggAcc::Median(_)
                        | AggAcc::Stddev { .. }
                        | AggAcc::GroupConcat { .. } => unreachable!(),
                    }
                });
            }
        }
    }

    pub(super) fn finish(&self, gid: usize) -> Val {
        match self {
            AggAcc::Count(v) => Val::Int(v[gid]),
            AggAcc::DistinctCodes { bits, .. } => {
                Val::Int(bits[gid].iter().map(|word| word.count_ones() as i64).sum())
            }
            AggAcc::DistinctNum(v) => Val::Int(v.counts[gid]),
            AggAcc::DistinctStr { sets, .. } => Val::Int(sets[gid].len() as i64),
            AggAcc::SumI { v, any } => {
                if any[gid] { Val::Int(v[gid]) } else { Val::Null }
            }
            AggAcc::SumF { v, any } => {
                if any[gid] { Val::Float(v[gid]) } else { Val::Null }
            }
            AggAcc::Avg { sum, n } => {
                if n[gid] == 0 { Val::Null } else { Val::Float(sum[gid] / n[gid] as f64) }
            }
            AggAcc::MinMaxNum { v, seen, int, .. } => {
                if !seen[gid] {
                    Val::Null
                } else if *int {
                    Val::Int(v[gid] as i64)
                } else {
                    Val::Float(v[gid])
                }
            }
            AggAcc::MinMaxStr { v, .. } => {
                v[gid].as_ref().map(|s| Val::Text(s.clone())).unwrap_or(Val::Null)
            }
            AggAcc::Median(vs) => {
                let src = &vs[gid];
                if src.is_empty() {
                    return Val::Null;
                }
                let mut v = src.clone();
                v.sort_unstable_by(f64::total_cmp);
                let m = v.len() / 2;
                Val::Float(if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
            }
            AggAcc::Stddev { n, sum, sumsq } => {
                let c = n[gid];
                if c < 2 {
                    return Val::Null;
                }
                let mean = sum[gid] / c as f64;
                let var = (sumsq[gid] - sum[gid] * mean) / (c - 1) as f64;
                Val::Float(var.max(0.0).sqrt())
            }
            AggAcc::GroupConcat { v, .. } => {
                v[gid].as_ref().map(|s| Val::text(s.clone())).unwrap_or(Val::Null)
            }
        }
    }
}
