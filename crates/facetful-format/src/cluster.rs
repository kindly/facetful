//! Clustering detection. A column is *clustered* when each of its non-NULL
//! values forms one contiguous run in file order: once a value's run ends, it
//! never appears again. Sorted data is the common case, but a column ordered
//! by some other key also qualifies (ids numbered in the file's sort order;
//! the leading key of a multi-column sort).
//!
//! The writer feeds every group through a tracker per column and records the
//! verdict in the footer. The engine counts `count(distinct col)` over a
//! clustered column with one "last value" per group instead of a set: the
//! rows a query keeps are a subsequence of the file, so they stay clustered.

use crate::*;
use std::collections::HashSet;

/// Run heads kept to prove an unsorted integer column clustered. Past this
/// many runs the set is dropped and only a sorted column (non-decreasing or
/// non-increasing) still qualifies.
const MAX_RUNS: usize = 1 << 20;

pub(crate) enum Cluster {
    /// not tracked (floats, bools, plain text), or proven not clustered
    Off,
    Ints {
        last: Option<i64>,
        asc: bool,
        desc: bool,
        /// values whose run has ended; None once past `MAX_RUNS`
        ended: Option<HashSet<i64>>,
    },
    Codes {
        last: Option<u16>,
        /// codes whose run has ended, one bit per dictionary entry
        ended: Vec<u64>,
        card: usize,
    },
}

impl Cluster {
    pub(crate) fn new(def: &ColumnDef, dict_len: usize) -> Cluster {
        if def.is_dict() {
            return Cluster::Codes { last: None, ended: vec![0; dict_len.div_ceil(64)], card: dict_len };
        }
        match def.ty {
            ColumnType::Int8 | ColumnType::Int16 | ColumnType::Int32 | ColumnType::Int64 | ColumnType::Date | ColumnType::Timestamp => {
                Cluster::Ints { last: None, asc: true, desc: true, ended: Some(HashSet::new()) }
            }
            _ => Cluster::Off,
        }
    }

    pub(crate) fn clustered(&self) -> bool {
        !matches!(self, Cluster::Off)
    }

    #[inline]
    fn int(&mut self, v: i64) {
        let Cluster::Ints { last, asc, desc, ended } = self else { return };
        let fail = match *last {
            Some(p) if p == v => return,
            None => false,
            Some(p) => {
                *asc &= v > p;
                *desc &= v < p;
                let mut repeat = false;
                if let Some(set) = ended {
                    set.insert(p);
                    repeat = set.contains(&v);
                    if set.len() > MAX_RUNS {
                        *ended = None;
                    }
                }
                repeat || (ended.is_none() && !*asc && !*desc)
            }
        };
        if fail {
            *self = Cluster::Off;
        } else {
            *last = Some(v);
        }
    }

    #[inline]
    fn code(&mut self, c: u16) {
        let Cluster::Codes { last, ended, card } = self else { return };
        let c = c as usize;
        // out-of-range codes read as NULL
        if c >= *card {
            return;
        }
        let fail = match *last {
            Some(p) if p as usize == c => return,
            None => false,
            Some(p) => {
                ended[p as usize / 64] |= 1 << (p % 64);
                ended[c / 64] >> (c % 64) & 1 != 0
            }
        };
        if fail {
            *self = Cluster::Off;
        } else {
            *last = Some(c as u16);
        }
    }

    /// Feed one group's chunk of this column, in row order.
    pub(crate) fn feed(&mut self, data: &write::SegmentData<'_>, ty: ColumnType, validity: Option<&[u8]>, rows: usize) {
        use write::SegmentData;
        if matches!(self, Cluster::Off) {
            return;
        }
        let valid = |i: usize| validity.map_or(true, |v| v[i / 8] >> (i % 8) & 1 != 0);
        match data {
            SegmentData::Codes8(codes) => {
                for (i, &c) in codes.iter().enumerate() {
                    if valid(i) {
                        self.code(c as u16);
                    }
                }
            }
            SegmentData::Codes16(codes) => {
                for (i, &c) in codes.iter().enumerate() {
                    if valid(i) {
                        self.code(c);
                    }
                }
            }
            SegmentData::Fixed(bytes) => {
                let Some(w) = ty.fixed_width() else { return };
                for i in 0..rows {
                    if !valid(i) {
                        continue;
                    }
                    let b = &bytes[i * w..i * w + w];
                    let v = match w {
                        1 => b[0] as i8 as i64,
                        2 => i16::from_le_bytes([b[0], b[1]]) as i64,
                        4 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64,
                        _ => i64::from_le_bytes(b.try_into().unwrap()),
                    };
                    self.int(v);
                }
            }
            SegmentData::Bool(_) | SegmentData::Utf8 { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(vals: &[Option<i64>]) -> bool {
        let def = ColumnDef { name: "x".into(), ty: ColumnType::Int64, flags: 0 };
        let mut c = Cluster::new(&def, 0);
        for v in vals.iter().flatten() {
            c.int(*v);
        }
        c.clustered()
    }

    #[test]
    fn runs_of_integers() {
        assert!(ints(&[]));
        assert!(ints(&[Some(1), Some(1), Some(2), Some(3), Some(3)]));
        assert!(ints(&[Some(5), Some(4), Some(4), Some(1)]));
        // unsorted but clustered
        assert!(ints(&[Some(7), Some(7), Some(2), Some(9), Some(9), Some(3)]));
        // NULLs inside a run do not split it
        assert!(ints(&[Some(7), None, Some(7), Some(2)]));
        assert!(!ints(&[Some(1), Some(2), Some(1)]));
        assert!(!ints(&[Some(7), Some(2), Some(9), Some(2)]));
    }

    #[test]
    fn capped_run_set_keeps_sorted_columns_only() {
        let def = ColumnDef { name: "x".into(), ty: ColumnType::Int32, flags: 0 };
        let mut sorted = Cluster::new(&def, 0);
        let mut unsorted = Cluster::new(&def, 0);
        for v in 0..(MAX_RUNS as i64 + 10) {
            sorted.int(v);
            // clustered (every value once), but not monotone
            unsorted.int(if v % 2 == 0 { v } else { -v });
        }
        assert!(sorted.clustered());
        assert!(!unsorted.clustered());
    }

    #[test]
    fn runs_of_codes() {
        let def = ColumnDef { name: "d".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY };
        let run = |codes: &[u16]| {
            let mut c = Cluster::new(&def, 3);
            for &x in codes {
                c.code(x);
            }
            c.clustered()
        };
        assert!(run(&[2, 2, 0, 1, 1]));
        // code 3 is out of range: NULL
        assert!(run(&[2, 3, 2, 0]));
        assert!(!run(&[2, 0, 2]));
    }
}
