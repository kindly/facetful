//! Column vectors, lanes and the per-row-group evaluation context.

use super::*;

// ---------------- vectors ----------------

pub(super) type VStr = Rc<String>;

#[derive(Clone)]
pub(super) enum Data {
    I64(Rc<Vec<i64>>),
    F64(Rc<Vec<f64>>),
    /// dictionary column: codes + shared dict
    Codes { codes: Rc<Vec<u16>>, dict: Rc<Vec<VStr>> },
    Text(Rc<Vec<VStr>>),
    Bool(Rc<Vec<u8>>),
    /// broadcast literal
    Const(Val),
}

#[derive(Clone)]
pub(super) struct VV {
    pub(super) data: Data,
    /// validity bitmap (bit set = present); None = all valid
    pub(super) valid: Option<Rc<Vec<u8>>>,
}

#[inline]
pub(super) fn bit(v: &Option<Rc<Vec<u8>>>, i: usize) -> bool {
    v.as_ref().map_or(true, |b| b[i / 8] & (1 << (i % 8)) != 0)
}

impl VV {
    pub(super) fn all_valid(data: Data) -> VV {
        VV { data, valid: None }
    }
    pub(super) fn const_val(v: Val) -> VV {
        VV { data: Data::Const(v), valid: None }
    }
    #[inline]
    pub(super) fn is_valid(&self, i: usize) -> bool {
        if let Data::Const(v) = &self.data {
            return !v.is_null();
        }
        bit(&self.valid, i)
    }
    #[inline]
    pub(super) fn f64_at(&self, i: usize) -> f64 {
        match &self.data {
            Data::F64(v) => v[i],
            Data::I64(v) => v[i] as f64,
            Data::Const(c) => c.as_f64().unwrap_or(0.0),
            _ => 0.0,
        }
    }
    #[inline]
    pub(super) fn i64_at(&self, i: usize) -> i64 {
        match &self.data {
            Data::I64(v) => v[i],
            Data::F64(v) => v[i] as i64,
            Data::Const(Val::Int(x)) => *x,
            Data::Const(Val::Float(x)) => *x as i64,
            _ => 0,
        }
    }
    /// three-valued bool at lane i
    #[inline]
    pub(super) fn bool3_at(&self, i: usize) -> Option<bool> {
        if !self.is_valid(i) {
            return None;
        }
        match &self.data {
            Data::Bool(v) => Some(v[i] != 0),
            Data::Const(Val::Bool(b)) => Some(*b),
            _ => None,
        }
    }
    pub(super) fn text_at(&self, i: usize) -> Option<VStr> {
        if !self.is_valid(i) {
            return None;
        }
        match &self.data {
            Data::Codes { codes, dict } => dict.get(codes[i] as usize).cloned(),
            Data::Text(v) => Some(v[i].clone()),
            Data::Const(Val::Text(s)) => Some(s.clone()),
            _ => None,
        }
    }
    /// output materialization
    pub(super) fn val_at(&self, i: usize, ty: Ty) -> Val {
        if !self.is_valid(i) {
            return Val::Null;
        }
        match (&self.data, ty) {
            (Data::Const(v), _) => v.clone(),
            (Data::Bool(v), _) => Val::Bool(v[i] != 0),
            (Data::Codes { codes, dict }, _) => match dict.get(codes[i] as usize) {
                Some(s) => Val::Text(s.clone()),
                None => Val::Null,
            },
            (Data::Text(v), _) => Val::Text(v[i].clone()),
            (Data::I64(v), Ty::Float) => Val::Float(v[i] as f64),
            (Data::I64(v), _) => Val::Int(v[i]),
            (Data::F64(v), Ty::Int | Ty::Date | Ty::Timestamp) => Val::Int(v[i] as i64),
            (Data::F64(v), _) => Val::Float(v[i]),
        }
    }
}

/// `valid` at `rows`, as a fresh bitmap (None stays None).
fn gather_valid(valid: &Option<Rc<Vec<u8>>>, rows: &[u32]) -> Option<Rc<Vec<u8>>> {
    let b = valid.as_ref()?;
    let mut out = vec![0u8; rows.len().div_ceil(8)];
    for (i, &r) in rows.iter().enumerate() {
        let r = r as usize;
        if b[r / 8] >> (r % 8) & 1 != 0 {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    Some(Rc::new(out))
}

fn pick<T: Copy>(v: &[T], rows: &[u32]) -> Rc<Vec<T>> {
    Rc::new(rows.iter().map(|&r| v[r as usize]).collect())
}

impl VV {
    /// This lane at `rows`, in that order.
    pub(super) fn gather(&self, rows: &[u32]) -> VV {
        let data = match &self.data {
            Data::I64(v) => Data::I64(pick(v, rows)),
            Data::F64(v) => Data::F64(pick(v, rows)),
            Data::Bool(v) => Data::Bool(pick(v, rows)),
            Data::Codes { codes, dict } => Data::Codes { codes: pick(codes, rows), dict: dict.clone() },
            Data::Text(v) => Data::Text(Rc::new(rows.iter().map(|&r| v[r as usize].clone()).collect())),
            Data::Const(c) => Data::Const(c.clone()),
        };
        VV { data, valid: gather_valid(&self.valid, rows) }
    }
}

/// One group's lanes restricted to `rows` (in that order): the context in
/// which select expressions evaluate for the rows that survived the window,
/// not for every row the scan kept. Only the chosen rows' bytes are copied.
pub(super) fn gather_ctx(cols: &Cols, rows: &[u32]) -> GroupCtx {
    let cols = cols
        .iter()
        .map(|(&k, (c, valid))| {
            let sub = match c {
                GroupCol::Ready(vv) => GroupCol::Ready(vv.gather(rows)),
                GroupCol::I64(v) => GroupCol::I64(pick(v, rows)),
                GroupCol::F64(v) => GroupCol::F64(pick(v, rows)),
                GroupCol::Dict { codes, dict } => GroupCol::Dict { codes: pick(codes, rows), dict: dict.clone() },
                GroupCol::Text { offsets, bytes, .. } => {
                    let mut o = Vec::with_capacity(rows.len() + 1);
                    o.push(0u32);
                    let mut b = Vec::new();
                    for &r in rows {
                        let r = r as usize;
                        b.extend_from_slice(&bytes[offsets[r] as usize..offsets[r + 1] as usize]);
                        o.push(b.len() as u32);
                    }
                    GroupCol::Text { strs: std::cell::OnceCell::new(), offsets: Rc::new(o), bytes: Rc::new(b) }
                }
            };
            (k, (sub, gather_valid(valid, rows)))
        })
        .collect();
    GroupCtx { cols, rows: rows.len() }
}

/// intersect validities (arithmetic null propagation)
pub(super) fn valid_and(rows: usize, a: &VV, b: &VV) -> Option<Rc<Vec<u8>>> {
    let an = matches!(&a.data, Data::Const(v) if v.is_null());
    let bn = matches!(&b.data, Data::Const(v) if v.is_null());
    if an || bn {
        return Some(Rc::new(vec![0u8; (rows + 7) / 8]));
    }
    match (&a.valid, &b.valid) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (Some(x), Some(y)) => Some(Rc::new(x.iter().zip(y.iter()).map(|(p, q)| p & q).collect())),
    }
}

// ---------------- group context ----------------

#[derive(Clone)]
pub(super) enum GroupCol {
    /// an already-built lane (the group table's aggregate and key columns)
    Ready(VV),
    I64(Rc<Vec<i64>>),
    F64(Rc<Vec<f64>>),
    Dict { codes: Rc<Vec<u16>>, dict: Rc<Vec<VStr>> },
    /// raw blob feeds the LIKE scan kernel; strs materialize lazily, only
    /// when something actually evaluates the strings (a like-only filter
    /// never pays the per-string allocation)
    Text {
        strs: std::cell::OnceCell<Rc<Vec<VStr>>>,
        offsets: Rc<Vec<u32>>,
        bytes: Rc<Vec<u8>>,
    },
}

pub(super) struct GroupCtx {
    pub(super) cols: HashMap<usize, (GroupCol, Option<Rc<Vec<u8>>>)>,
    pub(super) rows: usize,
}

impl GroupCtx {
    pub(super) fn column(&self, idx: usize) -> VV {
        let (c, valid) = &self.cols[&idx];
        let data = match c {
            GroupCol::Ready(vv) => return vv.clone(),
            GroupCol::I64(v) => Data::I64(v.clone()),
            GroupCol::F64(v) => Data::F64(v.clone()),
            GroupCol::Dict { codes, dict } => {
                Data::Codes { codes: codes.clone(), dict: dict.clone() }
            }
            GroupCol::Text { strs, offsets, bytes } => Data::Text(
                strs.get_or_init(|| {
                    Rc::new(
                        offsets
                            .windows(2)
                            .map(|w| {
                                Rc::new(
                                    String::from_utf8_lossy(
                                        &bytes[w[0] as usize..w[1] as usize],
                                    )
                                    .into_owned(),
                                )
                            })
                            .collect(),
                    )
                })
                .clone(),
            ),
        };
        VV { data, valid: valid.clone() }
    }
}
