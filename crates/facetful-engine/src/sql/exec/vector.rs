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
