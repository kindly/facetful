//! Grouping plans: packed codes over dense dims, GroupMap past the lane budget, text keys off the blob.

use super::*;

// ---------------- grouping plans ----------------

/// How one group-by dimension maps to a dense code.
pub(super) enum DirectDim {
    /// dictionary column: the code lane is the dense code
    Dict,
    /// integer column with a small global value range: `value - min`
    Int { min: i64 },
}

/// Packed grouping: every GROUP BY is a dict or narrow-int column, so a
/// group's key is ONE mixed-radix code over the dimensions' cardinalities
/// (each with a null lane). A small product indexes a dense lane; a larger
/// one probes `GroupMap` on the code. Either way the key stays packed until
/// the groups that survive ORDER BY / LIMIT are projected — no `Vec<Val>`
/// per group during the scan.
pub(super) struct PackedGroups {
    pub(super) cols: Vec<usize>,
    pub(super) dims: Vec<DirectDim>,
    pub(super) cards: Vec<usize>,
    /// composite -> gid, `-1` free; `None` when the product exceeds the lane
    /// budget, in which case `map` holds the groups
    pub(super) dense: Option<Vec<i32>>,
    pub(super) map: GroupMap,
}

/// Open-addressed map from a packed group code to its gid: the same table
/// shape and hash as `DistinctU64`, and the code is the whole key, so a hit
/// needs no indirection to verify.
pub(super) struct GroupMap {
    pub(super) slots: Vec<GSlot>,
    pub(super) mask: usize,
    pub(super) shift: u32,
    pub(super) used: usize,
}

#[derive(Clone, Copy)]
pub(super) struct GSlot {
    pub(super) key: u64,
    pub(super) gid: u32,
}

impl GroupMap {
    pub(super) fn new() -> GroupMap {
        GroupMap { slots: Vec::new(), mask: 0, shift: 0, used: 0 }
    }
    /// The gid of `key`, inserting it as `next` when unseen.
    #[inline]
    pub(super) fn get_or_insert(&mut self, key: u64, next: u32) -> u32 {
        if (self.used + 1) * 4 >= self.slots.len() * 3 {
            self.resize();
        }
        let mut i = (mix64(key) >> self.shift) as usize;
        loop {
            let slot = self.slots[i];
            if slot.gid == EMPTY {
                self.slots[i] = GSlot { key, gid: next };
                self.used += 1;
                return next;
            }
            if slot.key == key {
                return slot.gid;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[cold]
    #[inline(never)]
    pub(super) fn resize(&mut self) {
        let len = self.slots.len();
        let cap = if len < 1 << 16 { (len * 4).max(1024) } else { len * 2 };
        let old = std::mem::replace(&mut self.slots, vec![GSlot { key: 0, gid: EMPTY }; cap]);
        self.mask = cap - 1;
        self.shift = (cap as u64).leading_zeros() + 1;
        for slot in old {
            if slot.gid != EMPTY {
                let mut i = (mix64(slot.key) >> self.shift) as usize;
                while self.slots[i].gid != EMPTY {
                    i = (i + 1) & self.mask;
                }
                self.slots[i] = slot;
            }
        }
    }
}

/// Global (all-groups) integer min/max from the footer stats, if every group
/// has them.
pub(super) fn int_range<S: ReadAt>(table: &Table<S>, col: usize) -> Option<(i64, i64)> {
    use crate::format::Stats;
    let mut r: Option<(i64, i64)> = None;
    for g in &table.catalog().groups {
        match g.cols[col].stats {
            Stats::Int { min, max } => {
                let (lo, hi) = r.unwrap_or((min, max));
                r = Some((lo.min(min), hi.max(max)));
            }
            _ => return None,
        }
    }
    r
}

/// One GROUP BY position of a `TextGroups` plan.
pub(super) enum TextDim {
    /// dict/int column: packs into the plan's composite code
    Packed,
    /// plain Utf8 column: hashed and compared as bytes
    Text(usize),
}

/// Grouping when a GROUP BY column is plain Utf8 (cardinality past the
/// dictionary, so no codes exist). Dict/int dims pack into a composite code
/// exactly as in `PackedGroups`; text dims hash their bytes straight off the
/// borrowed blob. A slot holds the 64-bit key hash and a hit is verified
/// against the group's stored key — its packed code plus byte spans in one
/// arena — so no `Rc<String>` exists during the scan, and the group table's
/// key lane is the arena itself, gathered as raw text.
pub(super) struct TextGroups {
    pub(super) dims: Vec<TextDim>,
    /// the packed dims in GROUP BY order, as `PackedGroups` keeps them
    pub(super) pcols: Vec<usize>,
    pub(super) pdims: Vec<DirectDim>,
    pub(super) pcards: Vec<usize>,
    /// `key` is the hash; `gid == EMPTY` marks a free slot
    pub(super) slots: Vec<GSlot>,
    pub(super) mask: usize,
    pub(super) shift: u32,
    pub(super) used: usize,
    /// per group: the packed code of the non-text dims
    pub(super) codes: Vec<u64>,
    /// per group × text dim, row-major: (start, len) into `arena`; a NULL
    /// key is `len == u32::MAX`
    pub(super) spans: Vec<(u32, u32)>,
    pub(super) arena: Vec<u8>,
}

impl TextGroups {
    pub(super) fn n_text(&self) -> usize {
        self.dims.iter().filter(|d| matches!(d, TextDim::Text(_))).count()
    }
    /// The gid of the key `(composite, texts)` whose hash is `hash`,
    /// inserting it as `next` when unseen.
    #[inline]
    pub(super) fn get_or_insert(
        &mut self,
        hash: u64,
        composite: u64,
        texts: &[Option<&[u8]>],
        next: u32,
    ) -> u32 {
        if (self.used + 1) * 4 >= self.slots.len() * 3 {
            self.resize();
        }
        let mut i = (mix64(hash) >> self.shift) as usize;
        loop {
            let slot = self.slots[i];
            if slot.gid == EMPTY {
                self.slots[i] = GSlot { key: hash, gid: next };
                self.used += 1;
                self.codes.push(composite);
                for t in texts {
                    match t {
                        Some(b) => {
                            let start = self.arena.len() as u32;
                            self.arena.extend_from_slice(b);
                            self.spans.push((start, b.len() as u32));
                        }
                        None => self.spans.push((0, u32::MAX)),
                    }
                }
                return next;
            }
            if slot.key == hash && self.matches(slot.gid as usize, composite, texts) {
                return slot.gid;
            }
            i = (i + 1) & self.mask;
        }
    }
    pub(super) fn matches(&self, gid: usize, composite: u64, texts: &[Option<&[u8]>]) -> bool {
        if self.codes[gid] != composite {
            return false;
        }
        let nt = texts.len();
        texts.iter().enumerate().all(|(k, t)| {
            let (start, len) = self.spans[gid * nt + k];
            match *t {
                None => len == u32::MAX,
                Some(b) => {
                    let (start, len) = (start as usize, len as usize);
                    len != u32::MAX as usize
                        && len == b.len()
                        && &self.arena[start..start + len] == b
                }
            }
        })
    }
    #[cold]
    #[inline(never)]
    pub(super) fn resize(&mut self) {
        let len = self.slots.len();
        let cap = if len < 1 << 16 { (len * 4).max(1024) } else { len * 2 };
        let old = std::mem::replace(&mut self.slots, vec![GSlot { key: 0, gid: EMPTY }; cap]);
        self.mask = cap - 1;
        self.shift = (cap as u64).leading_zeros() + 1;
        for slot in old {
            if slot.gid != EMPTY {
                let mut i = (mix64(slot.key) >> self.shift) as usize;
                while self.slots[i].gid != EMPTY {
                    i = (i + 1) & self.mask;
                }
                self.slots[i] = slot;
            }
        }
    }
}

/// FNV-free byte hash for text keys: the Fx mixer over 8-byte words.
#[inline]
pub(super) fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = FxHasher::default();
    h.write(b);
    h.finish()
}

/// A group-by dimension's dense mapping and lane count (with the null lane),
/// if the column can be a dense dim: a dictionary column, or an integer/date
/// column with a small global range from the footer stats.
pub(super) fn dense_dim<S: ReadAt>(table: &mut Table<S>, index: usize) -> Option<(DirectDim, usize)> {
    let (cty, is_dict) = {
        let def = &table.catalog().schema.columns[index];
        (def.ty, def.is_dict())
    };
    if is_dict {
        return Some((DirectDim::Dict, table.dictionary(index).ok()?.len() + 1));
    }
    let int_lane = matches!(
        cty,
        ColumnType::Int8
            | ColumnType::Int16
            | ColumnType::Int32
            | ColumnType::Int64
            | ColumnType::Date
            | ColumnType::Timestamp
    );
    // narrow-range integers (years, small ids, dates) group densely too
    let (min, max) = int_lane.then(|| int_range(table, index)).flatten()?;
    let range = usize::try_from(max.checked_sub(min)?).ok()?.checked_add(1)?;
    Some((DirectDim::Int { min }, range + 1))
}

/// Dense-lane budget for packed grouping; larger products go through `GroupMap`.
pub(super) const DENSE_LANES: u64 = 1 << 22;

pub(super) fn packed_plan<S: ReadAt>(table: &mut Table<S>, group_by: &[Bound]) -> Option<PackedGroups> {
    let mut cols = Vec::new();
    let mut dims = Vec::new();
    let mut cards = Vec::new();
    let mut product: u64 = 1;
    for g in group_by {
        let Bound::Column { index, .. } = g else { return None };
        let (dim, card) = dense_dim(table, *index)?;
        // the code must fit u64; beyond that the Vec<Val> hash path takes over
        product = product.checked_mul(card as u64)?;
        cols.push(*index);
        dims.push(dim);
        cards.push(card);
    }
    if cols.is_empty() {
        return None;
    }
    let dense = (product <= DENSE_LANES).then(|| vec![-1; product as usize]);
    Some(PackedGroups { cols, dims, cards, dense, map: GroupMap::new() })
}

/// Every GROUP BY is a direct column, at least one of them plain Utf8, the
/// rest dense dims.
pub(super) fn text_plan<S: ReadAt>(table: &mut Table<S>, group_by: &[Bound]) -> Option<TextGroups> {
    let mut dims = Vec::new();
    let (mut pcols, mut pdims, mut pcards) = (Vec::new(), Vec::new(), Vec::new());
    let mut product: u64 = 1;
    for g in group_by {
        let Bound::Column { index, .. } = g else { return None };
        let def = &table.catalog().schema.columns[*index];
        if def.ty == ColumnType::Utf8 && !def.is_dict() {
            dims.push(TextDim::Text(*index));
            continue;
        }
        let (dim, card) = dense_dim(table, *index)?;
        product = product.checked_mul(card as u64)?;
        pcols.push(*index);
        pdims.push(dim);
        pcards.push(card);
        dims.push(TextDim::Packed);
    }
    dims.iter().any(|d| matches!(d, TextDim::Text(_))).then(|| TextGroups {
        dims,
        pcols,
        pdims,
        pcards,
        slots: Vec::new(),
        mask: 0,
        shift: 0,
        used: 0,
        codes: Vec::new(),
        spans: Vec::new(),
        arena: Vec::new(),
    })
}

/// Raw lanes of the dense dims, hoisted out of the row loop.
pub(super) enum FastLane<'a> {
    Codes(&'a [u16]),
    Ints { vals: &'a [i64], min: i64 },
}

pub(super) struct FastDim<'a> {
    pub(super) lane: FastLane<'a>,
    pub(super) valid: Option<&'a [u8]>,
    pub(super) card: usize,
}

pub(super) fn fast_dims<'a>(code_cols: &'a [(VV, usize)], dims: &[DirectDim]) -> Vec<FastDim<'a>> {
    code_cols
        .iter()
        .zip(dims)
        .map(|((vv, card), dim)| {
            let lane = match (&vv.data, dim) {
                (Data::Codes { codes, .. }, DirectDim::Dict) => FastLane::Codes(codes),
                (Data::I64(vals), DirectDim::Int { min }) => FastLane::Ints { vals, min: *min },
                _ => unreachable!("dense dims are dict/int columns"),
            };
            FastDim { lane, valid: vv.valid.as_deref().map(|v| v.as_slice()), card: *card }
        })
        .collect()
}

/// Row `i`'s mixed-radix code over the dense dims. u64: the product may
/// exceed usize on wasm32.
#[inline]
pub(super) fn composite_of(fast: &[FastDim<'_>], i: usize) -> u64 {
    let mut composite = 0u64;
    for fd in fast {
        let ok = fd.valid.map_or(true, |v| v[i / 8] >> (i % 8) & 1 != 0);
        let code = if !ok {
            fd.card - 1
        } else {
            match &fd.lane {
                FastLane::Codes(codes) => codes[i] as usize,
                FastLane::Ints { vals, min } => (vals[i] - min) as usize,
            }
        };
        composite = composite * fd.card as u64 + code as u64;
    }
    composite
}

/// Group-table key lanes for packed codes: peel each group's mixed-radix
/// code (last dimension first) into a dictionary code lane or an int lane,
/// with the null lane as invalid. No string is touched.
pub(super) fn packed_key_lanes(
    codes: &[u64],
    cols: &[usize],
    dims: &[DirectDim],
    cards: &[usize],
    dicts: &HashMap<usize, Rc<Vec<VStr>>>,
) -> Vec<(GroupCol, Option<Rc<Vec<u8>>>)> {
    let (nd, n) = (cols.len(), codes.len());
    let mut per_dim: Vec<Vec<usize>> = vec![vec![0; n]; nd];
    for (g, &code) in codes.iter().enumerate() {
        let mut code = code;
        for k in (0..nd).rev() {
            let card = cards[k] as u64;
            per_dim[k][g] = (code % card) as usize;
            code /= card;
        }
    }
    per_dim
        .into_iter()
        .enumerate()
        .map(|(k, dim_codes)| {
            let null_lane = cards[k] - 1;
            let mut valid = vec![0u8; n.div_ceil(8)];
            for (g, &c) in dim_codes.iter().enumerate() {
                if c != null_lane {
                    valid[g / 8] |= 1 << (g % 8);
                }
            }
            let col = match dims[k] {
                DirectDim::Dict => GroupCol::Dict {
                    codes: Rc::new(dim_codes.iter().map(|&c| c as u16).collect()),
                    dict: dicts[&cols[k]].clone(),
                },
                DirectDim::Int { min } => {
                    GroupCol::I64(Rc::new(dim_codes.iter().map(|&c| min + c as i64).collect()))
                }
            };
            (col, Some(Rc::new(valid)))
        })
        .collect()
}
