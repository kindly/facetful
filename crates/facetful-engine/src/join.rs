//! One-shot hash join of two tables on equal columns, producing a new table
//! in the left table's row order with the chosen right-side columns gathered
//! alongside (design.sv d41, step 3). LEFT keeps every left row and adds a
//! `matched` lane; INNER keeps the matched rows.
//!
//! The right key must be unique: a repeat is an error, never a row
//! explosion (the positional model needs the left row count preserved).
//!
//! The key map is perfect-hash first, the way the grouping code already is:
//! dictionary keys translate once per *dictionary entry* (left code → right
//! row) so the per-row probe is an array index; int keys index a dense lane
//! over the right table's footer min/max when the range allows, else an
//! open-addressed `GroupMap`; plain-text keys hash bytes into a `TextGroups`
//! arena. Composite keys pack the dense dims into one code.

use crate::format::compile::{compile_sorted, InCol};
use crate::format::read::ReadAt;
use crate::format::{ColumnType, FormatError};
use crate::sql::exec::{hash_bytes, int_range, mix64, GroupMap, TextGroups, DENSE_LANES};
use crate::Table;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinKind {
    Left,
    Inner,
}

pub struct JoinSpec {
    /// (left column, right column) pairs compared for equality
    pub keys: Vec<(String, String)>,
    /// right-side columns to carry; empty = every non-key right column
    pub columns: Vec<String>,
    pub kind: JoinKind,
}

/// Not matched.
const NONE: u32 = u32::MAX;

/// One key pair, classified by how the left row's value maps into the
/// right table's key space.
enum Dim {
    /// both dictionary columns: left code → right code (or NONE), then the
    /// right code is the dense value; card = right dict len + 1
    Dict { translate: Vec<u32>, card: u64 },
    /// both integer-like: value − right min; card = right range + 1
    Int { min: i64, card: u64 },
    /// plain text on either side: compared as bytes through the arena
    Text,
}

/// A whole column's values, read once, addressable by global row.
enum Col {
    Codes(Vec<u16>, Option<Vec<bool>>),
    Ints(Vec<i64>, Option<Vec<bool>>),
    Floats(Vec<f64>, Option<Vec<bool>>),
    Text(Vec<String>, Option<Vec<bool>>),
}

fn is_intish(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Int8 | ColumnType::Int16 | ColumnType::Int32 | ColumnType::Int64 | ColumnType::Date | ColumnType::Timestamp
    )
}

fn col_index<S: ReadAt>(t: &Table<S>, name: &str, side: &str) -> Result<usize, String> {
    t.catalog()
        .schema
        .columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| format!("join: no column '{name}' in the {side} table"))
}

/// Read a whole column across row groups.
fn read_col<S: ReadAt>(t: &mut Table<S>, c: usize) -> Result<Col, String> {
    let (ty, is_dict) = {
        let def = &t.catalog().schema.columns[c];
        (def.ty, def.is_dict())
    };
    let e = |x: FormatError| format!("join: {x}");
    let mut valid: Option<Vec<bool>> = None;
    let mut any_null = false;
    let mut codes = Vec::new();
    let mut ints = Vec::new();
    let mut floats = Vec::new();
    let mut texts = Vec::new();
    for g in 0..t.group_count() {
        let n = t.group_rows(g);
        let vb = t.validity(g, c).map_err(e)?;
        let v = valid.get_or_insert_with(Vec::new);
        match &vb {
            Some(bits) => {
                any_null = true;
                v.extend((0..n).map(|i| bits[i / 8] >> (i % 8) & 1 != 0));
            }
            None => v.extend(std::iter::repeat(true).take(n)),
        }
        if is_dict {
            codes.extend(t.codes(g, c, n).map_err(e)?);
        } else {
            match ty {
                ColumnType::Float64 => floats.extend(t.f64s(g, c, n).map_err(e)?),
                ColumnType::Utf8 => {
                    let (offs, bytes) = t.texts_raw(g, c, n).map_err(e)?;
                    texts.extend((0..n).map(|i| {
                        String::from_utf8_lossy(&bytes[offs[i] as usize..offs[i + 1] as usize]).into_owned()
                    }));
                }
                ColumnType::Bool => return Err("join: boolean columns are not supported yet".into()),
                _ => ints.extend(t.i64s(g, c, n).map_err(e)?),
            }
        }
    }
    let valid = if any_null { valid } else { None };
    Ok(if is_dict {
        Col::Codes(codes, valid)
    } else {
        match ty {
            ColumnType::Float64 => Col::Floats(floats, valid),
            ColumnType::Utf8 => Col::Text(texts, valid),
            _ => Col::Ints(ints, valid),
        }
    })
}

fn is_valid(valid: &Option<Vec<bool>>, i: usize) -> bool {
    valid.as_ref().map_or(true, |v| v[i])
}

/// Gather `col` by global row (`NONE` → null) into the compiler's input; a
/// dictionary column keeps its codes and dictionary, so no string is touched.
fn gather(col: &Col, rows: &[u32], ty: ColumnType, dict: Option<Vec<String>>) -> InCol {
    let n = rows.len();
    let valid_of = |src: &Option<Vec<bool>>| -> Option<Vec<bool>> {
        let v: Vec<bool> = rows.iter().map(|&r| r != NONE && is_valid(src, r as usize)).collect();
        if v.iter().all(|&b| b) { None } else { Some(v) }
    };
    match col {
        Col::Codes(codes, valid) => InCol::Dict {
            codes: rows.iter().map(|&r| if r == NONE { 0 } else { codes[r as usize] }).collect(),
            dict: dict.expect("dictionary for a codes column"),
            valid: valid_of(valid),
        },
        Col::Floats(v, valid) => InCol::Float {
            v: rows.iter().map(|&r| if r == NONE { 0.0 } else { v[r as usize] }).collect(),
            valid: valid_of(valid),
        },
        Col::Text(v, valid) => InCol::Text {
            v: rows.iter().map(|&r| if r == NONE { String::new() } else { v[r as usize].clone() }).collect(),
            valid: valid_of(valid),
        },
        Col::Ints(v, valid) => {
            let ints: Vec<i64> = rows.iter().map(|&r| if r == NONE { 0 } else { v[r as usize] }).collect();
            let valid = valid_of(valid);
            match ty {
                ColumnType::Date => InCol::Date { v: ints.into_iter().map(|x| x as i32).collect(), valid },
                ColumnType::Timestamp => InCol::Timestamp { v: ints, valid },
                _ => InCol::Int { v: ints, valid },
            }
        }
    }
    .with_len(n)
}

trait WithLen {
    fn with_len(self, n: usize) -> Self;
}
impl WithLen for InCol {
    fn with_len(self, _n: usize) -> Self {
        self
    }
}

/// Join `left` to `right`, returning the new table's image.
pub fn join<S: ReadAt>(
    left: &mut Table<S>,
    right: &mut Table<S>,
    spec: &JoinSpec,
    group_target: u32,
) -> Result<Vec<u8>, String> {
    if spec.keys.is_empty() {
        return Err("join: at least one key pair is required".into());
    }
    // ---- resolve names ----
    let mut key_cols = Vec::new();
    for (l, r) in &spec.keys {
        key_cols.push((col_index(left, l, "left")?, col_index(right, r, "right")?));
    }
    let right_cols: Vec<usize> = if spec.columns.is_empty() {
        (0..right.catalog().schema.columns.len())
            .filter(|c| !key_cols.iter().any(|(_, r)| r == c))
            .collect()
    } else {
        spec.columns.iter().map(|n| col_index(right, n, "right")).collect::<Result<_, _>>()?
    };
    let left_names: Vec<String> = left.catalog().schema.columns.iter().map(|c| c.name.clone()).collect();
    let mut names = left_names.clone();
    for &c in &right_cols {
        let n = right.catalog().schema.columns[c].name.clone();
        if names.contains(&n) {
            return Err(format!(
                "join: column '{n}' exists on both sides — pick right columns that don't clash"
            ));
        }
        names.push(n);
    }
    if spec.kind == JoinKind::Left {
        if names.iter().any(|n| n == "matched") {
            return Err("join: a column named 'matched' would clash with the match flag".into());
        }
        names.push("matched".into());
    }

    // ---- classify the key dims and read the key columns ----
    let mut dims = Vec::new();
    let mut lkeys = Vec::new();
    let mut rkeys = Vec::new();
    let mut product: u64 = 1;
    for &(lc, rc) in &key_cols {
        let (lt, ld) = { let d = &left.catalog().schema.columns[lc]; (d.ty, d.is_dict()) };
        let (rt, rd) = { let d = &right.catalog().schema.columns[rc]; (d.ty, d.is_dict()) };
        let dim = if ld && rd {
            // translate once per left dictionary entry, through the same
            // byte-hashing arena the text keys use (no HashMap instantiation)
            let rdict = right.dictionary(rc).map_err(|e| format!("join: {e}"))?;
            let mut index = TextGroups::text_only(1);
            for (i, s) in rdict.iter().enumerate() {
                let b = s.as_bytes();
                index.get_or_insert(mix64(hash_bytes(b)), 0, &[Some(b)], i as u32);
            }
            let ldict = left.dictionary(lc).map_err(|e| format!("join: {e}"))?;
            let translate = ldict
                .iter()
                .map(|s| index.get(mix64(hash_bytes(s.as_bytes())), 0, &[Some(s.as_bytes())]).unwrap_or(NONE))
                .collect();
            Dim::Dict { translate, card: rdict.len() as u64 + 1 }
        } else if is_intish(lt) && is_intish(rt) && !ld && !rd {
            let (min, max) = int_range(right, rc).ok_or("join: the right key column has no min/max stats")?;
            let range = (max - min) as u64 + 1;
            Dim::Int { min, card: range + 1 }
        } else if (lt == ColumnType::Utf8) && (rt == ColumnType::Utf8) {
            Dim::Text
        } else {
            return Err(format!(
                "join: cannot join on '{}' = '{}': keys must be dictionary text, integer/date, or text on both sides",
                left_names[lc], right.catalog().schema.columns[rc].name
            ));
        };
        if let Dim::Dict { card, .. } | Dim::Int { card, .. } = &dim {
            product = product.checked_mul(*card).ok_or("join: key space too large")?;
        }
        dims.push(dim);
        lkeys.push(read_col(left, lc)?);
        rkeys.push(read_col(right, rc)?);
    }
    let n_text = dims.iter().filter(|d| matches!(d, Dim::Text)).count();
    let n_right: usize = (0..right.group_count()).map(|g| right.group_rows(g)).sum();
    let n_left: usize = (0..left.group_count()).map(|g| left.group_rows(g)).sum();

    // right row's packed code over the dense dims (None: a NULL key)
    let right_code = |row: usize| -> Option<u64> {
        let mut code = 0u64;
        for (d, k) in dims.iter().zip(&rkeys) {
            match (d, k) {
                (Dim::Dict { card, .. }, Col::Codes(codes, valid)) => {
                    if !is_valid(valid, row) { return None; }
                    code = code * card + codes[row] as u64;
                }
                (Dim::Int { min, card }, Col::Ints(v, valid)) => {
                    if !is_valid(valid, row) { return None; }
                    code = code * card + (v[row] - min) as u64;
                }
                (Dim::Text, k) => { if !is_valid(k.valid(), row) { return None; } }
                _ => unreachable!("dim/column kinds agree"),
            }
        }
        Some(code)
    };
    // left row's code in the right key space (None: NULL or unmatchable)
    let left_code = |row: usize| -> Option<u64> {
        let mut code = 0u64;
        for (d, k) in dims.iter().zip(&lkeys) {
            match (d, k) {
                (Dim::Dict { translate, card }, Col::Codes(codes, valid)) => {
                    if !is_valid(valid, row) { return None; }
                    let r = translate[codes[row] as usize];
                    if r == NONE { return None; }
                    code = code * card + r as u64;
                }
                (Dim::Int { min, card }, Col::Ints(v, valid)) => {
                    if !is_valid(valid, row) { return None; }
                    let x = v[row].checked_sub(*min)?;
                    if x < 0 || x as u64 >= card - 1 { return None; }
                    code = code * card + x as u64;
                }
                (Dim::Text, k) => { if !is_valid(k.valid(), row) { return None; } }
                _ => unreachable!("dim/column kinds agree"),
            }
        }
        Some(code)
    };
    let text_at = |k: &Col, dict: &Option<Vec<String>>, row: usize| -> Vec<u8> {
        match k {
            Col::Text(v, _) => v[row].as_bytes().to_vec(),
            Col::Codes(codes, _) => dict.as_ref().expect("dict")[codes[row] as usize].as_bytes().to_vec(),
            _ => unreachable!("text dim over a text column"),
        }
    };
    // dictionaries of dictionary-encoded columns used as text keys
    let mut ldicts: Vec<Option<Vec<String>>> = Vec::new();
    let mut rdicts: Vec<Option<Vec<String>>> = Vec::new();
    for (i, &(lc, rc)) in key_cols.iter().enumerate() {
        let is_text = matches!(dims[i], Dim::Text);
        ldicts.push(if is_text && matches!(lkeys[i], Col::Codes(..)) { Some(left.dictionary(lc).map_err(|e| format!("join: {e}"))?) } else { None });
        rdicts.push(if is_text && matches!(rkeys[i], Col::Codes(..)) { Some(right.dictionary(rc).map_err(|e| format!("join: {e}"))?) } else { None });
    }

    // ---- build: right key → right row, uniqueness enforced ----
    enum Map { Dense(Vec<u32>), Hash(GroupMap), Text(TextGroups) }
    let mut map = if n_text > 0 {
        Map::Text(TextGroups::text_only(n_text))
    } else if product <= DENSE_LANES {
        Map::Dense(vec![NONE; product as usize])
    } else {
        Map::Hash(GroupMap::new_map())
    };
    let mut texts: Vec<Vec<u8>> = Vec::new();
    // the arena numbers its entries densely; rows with NULL keys are skipped,
    // so gid → right row goes through this
    let mut row_of_gid: Vec<u32> = Vec::new();
    for row in 0..n_right {
        let Some(code) = right_code(row) else { continue }; // NULL keys never match
        let dup = match &mut map {
            Map::Dense(lane) => {
                let slot = &mut lane[code as usize];
                if *slot != NONE { true } else { *slot = row as u32; false }
            }
            Map::Hash(m) => m.get_or_insert(code, row as u32) != row as u32,
            Map::Text(tg) => {
                texts.clear();
                let mut hash = code.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                for (i, d) in dims.iter().enumerate() {
                    if matches!(d, Dim::Text) {
                        let b = text_at(&rkeys[i], &rdicts[i], row);
                        hash = mix64(hash ^ hash_bytes(&b));
                        texts.push(b);
                    }
                }
                let refs: Vec<Option<&[u8]>> = texts.iter().map(|b| Some(b.as_slice())).collect();
                let next = row_of_gid.len() as u32;
                if tg.get_or_insert(hash, code, &refs, next) != next {
                    true
                } else {
                    row_of_gid.push(row as u32);
                    false
                }
            }
        };
        if dup {
            return Err(format!(
                "join: the right key is not unique (row {row} repeats an earlier key) — a join must be many-to-one"
            ));
        }
    }

    // ---- probe: every left row → right row or NONE ----
    let mut matches: Vec<u32> = Vec::with_capacity(n_left);
    for row in 0..n_left {
        let m = match left_code(row) {
            None => NONE,
            Some(code) => match &map {
                Map::Dense(lane) => lane[code as usize],
                Map::Hash(m) => m.get(code).unwrap_or(NONE),
                Map::Text(tg) => {
                    texts.clear();
                    let mut hash = code.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                    for (i, d) in dims.iter().enumerate() {
                        if matches!(d, Dim::Text) {
                            let b = text_at(&lkeys[i], &ldicts[i], row);
                            hash = mix64(hash ^ hash_bytes(&b));
                            texts.push(b);
                        }
                    }
                    let refs: Vec<Option<&[u8]>> = texts.iter().map(|b| Some(b.as_slice())).collect();
                    tg.get(hash, code, &refs).map_or(NONE, |gid| row_of_gid[gid as usize])
                }
            },
        };
        matches.push(m);
    }
    drop(lkeys);
    drop(rkeys);

    // ---- gather ----
    let kept: Vec<u32> = match spec.kind {
        JoinKind::Left => (0..n_left as u32).collect(),
        JoinKind::Inner => (0..n_left as u32).filter(|&i| matches[i as usize] != NONE).collect(),
    };
    let right_rows: Vec<u32> = kept.iter().map(|&i| matches[i as usize]).collect();
    let mut cols: Vec<InCol> = Vec::with_capacity(names.len());
    for c in 0..left_names.len() {
        let col = read_col(left, c)?;
        let (ty, dict) = {
            let def = &left.catalog().schema.columns[c];
            (def.ty, def.is_dict())
        };
        let dict = if dict { Some(left.dictionary(c).map_err(|e| format!("join: {e}"))?) } else { None };
        cols.push(gather(&col, &kept, ty, dict));
    }
    for &c in &right_cols {
        let col = read_col(right, c)?;
        let (ty, dict) = {
            let def = &right.catalog().schema.columns[c];
            (def.ty, def.is_dict())
        };
        let dict = if dict { Some(right.dictionary(c).map_err(|e| format!("join: {e}"))?) } else { None };
        cols.push(gather(&col, &right_rows, ty, dict));
    }
    if spec.kind == JoinKind::Left {
        cols.push(InCol::Int { v: right_rows.iter().map(|&r| (r != NONE) as i64).collect(), valid: None });
    }
    // left row order is preserved, so the left table's sort still holds
    let sorted_by = left.catalog().sorted_by.clone();
    compile_sorted(&names, cols, group_target, sorted_by).map(|(img, _)| img).map_err(|e| format!("join: {e}"))
}

impl Col {
    fn valid(&self) -> &Option<Vec<bool>> {
        match self {
            Col::Codes(_, v) | Col::Ints(_, v) | Col::Floats(_, v) | Col::Text(_, v) => v,
        }
    }
}

/// The FFI/CLI spec: line 1 `left`|`inner`; line 2 key pairs `l\x1er`
/// separated by `\x1f`; line 3 right columns separated by `\x1f` (may be empty).
pub fn parse_spec(text: &str) -> Result<JoinSpec, String> {
    let mut lines = text.split('\n');
    let kind = match lines.next().map(str::trim) {
        Some("left") => JoinKind::Left,
        Some("inner") => JoinKind::Inner,
        _ => return Err("join: kind must be 'left' or 'inner'".into()),
    };
    let keys: Vec<(String, String)> = lines
        .next()
        .unwrap_or("")
        .split('\x1f')
        .filter(|s| !s.is_empty())
        .map(|p| {
            let (l, r) = p.split_once('\x1e').unwrap_or((p, p));
            (l.to_string(), r.to_string())
        })
        .collect();
    let columns: Vec<String> =
        lines.next().unwrap_or("").split('\x1f').filter(|s| !s.is_empty()).map(str::to_string).collect();
    Ok(JoinSpec { keys, columns, kind })
}
