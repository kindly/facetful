//! Output values and the query result (rows or the columnar channel).

use super::*;

// ---------------- values (output + group keys only) ----------------

#[derive(Debug, Clone)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(Rc<String>),
}

impl Val {
    pub fn text(s: impl Into<String>) -> Val {
        Val::Text(Rc::new(s.into()))
    }
    pub(super) fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }
    pub(super) fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Int(i) => Some(*i as f64),
            Val::Float(f) => Some(*f),
            _ => None,
        }
    }
    pub(super) fn cmp_sql(&self, other: &Val) -> core::cmp::Ordering {
        use core::cmp::Ordering::*;
        use Val::*;
        match (self, other) {
            (Null, Null) => Equal,
            (Null, _) => Less,
            (_, Null) => Greater,
            (Bool(a), Bool(b)) => a.cmp(b),
            (Text(a), Text(b)) => a.cmp(b),
            (a, b) => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => Equal,
            },
        }
    }
    pub(super) fn eq_sql(&self, other: &Val) -> Option<bool> {
        if self.is_null() || other.is_null() {
            return None;
        }
        Some(self.cmp_sql(other) == core::cmp::Ordering::Equal)
    }
}

impl PartialEq for Val {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Val::Null, Val::Null) => true, // grouping semantics
            (Val::Null, _) | (_, Val::Null) => false,
            _ => self.cmp_sql(other) == core::cmp::Ordering::Equal,
        }
    }
}
impl Eq for Val {}
impl core::hash::Hash for Val {
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        match self {
            Val::Null => 0u8.hash(h),
            Val::Bool(b) => (1u8, b).hash(h),
            Val::Int(i) => {
                2u8.hash(h);
                (*i as f64).to_bits().hash(h);
            }
            Val::Float(f) => {
                2u8.hash(h);
                f.to_bits().hash(h);
            }
            Val::Text(s) => (3u8, s).hash(h),
        }
    }
}

pub struct QueryResult {
    pub columns: Vec<String>,
    /// bound type per output column (Date/Timestamp make marshalling
    /// format the underlying days/ms as ISO strings where appropriate)
    pub col_types: Vec<Ty>,
    pub rows: Vec<Vec<Val>>,
    /// Columnar channel: the non-aggregate projection paths fill this instead
    /// of `rows` (typed vectors gather at memcpy-like speed; per-row Vals cost
    /// ~5x). Consumers that want rows call `ensure_rows()`.
    pub cols: Option<Vec<OutCol>>,
    pub out_rows: usize,
    pub scanned_groups: usize,
    pub total_groups: usize,
}

/// One projected output column plus its validity bitmap (bit set = non-null).
pub enum OutCol {
    I64 { v: Vec<i64>, valid: Vec<u8> },
    F64 { v: Vec<f64>, valid: Vec<u8> },
    Bool { v: Vec<u8>, valid: Vec<u8> },
    Text { offsets: Vec<u32>, bytes: Vec<u8>, valid: Vec<u8> },
}

impl QueryResult {
    pub fn n_rows(&self) -> usize {
        if self.cols.is_some() { self.out_rows } else { self.rows.len() }
    }

    /// Materialize `rows` from the columnar channel (no-op when already rows).
    pub fn ensure_rows(&mut self) {
        let Some(cols) = self.cols.take() else { return };
        let n = self.out_rows;
        let mut rows: Vec<Vec<Val>> = (0..n).map(|_| Vec::with_capacity(cols.len())).collect();
        for (ci, c) in cols.iter().enumerate() {
            let ty = self.col_types[ci];
            for (i, row) in rows.iter_mut().enumerate() {
                row.push(outcol_val(c, i, ty));
            }
        }
        self.rows = rows;
    }
}

pub(super) fn outcol_val(c: &OutCol, i: usize, ty: Ty) -> Val {
    let ok = |valid: &Vec<u8>| valid[i / 8] >> (i % 8) & 1 != 0;
    match c {
        OutCol::I64 { v, valid } => {
            if !ok(valid) {
                Val::Null
            } else if ty == Ty::Float {
                Val::Float(v[i] as f64)
            } else {
                Val::Int(v[i])
            }
        }
        OutCol::F64 { v, valid } => {
            if !ok(valid) {
                Val::Null
            } else if ty == Ty::Int {
                Val::Int(v[i] as i64)
            } else {
                Val::Float(v[i])
            }
        }
        OutCol::Bool { v, valid } => {
            if !ok(valid) { Val::Null } else { Val::Bool(v[i] != 0) }
        }
        OutCol::Text { offsets, bytes, valid } => {
            if !ok(valid) {
                Val::Null
            } else {
                Val::text(
                    String::from_utf8_lossy(&bytes[offsets[i] as usize..offsets[i + 1] as usize])
                        .into_owned(),
                )
            }
        }
    }
}
