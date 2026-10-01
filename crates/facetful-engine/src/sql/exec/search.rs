//! search('q', col, …): true when every word of `q` occurs in at least one
//! of the columns (docs/full-text-search.sv, prototype steps 1-2).
//!
//! Words are folded the way LIKE folds case and matched as substrings, so
//! `search('Mental hea', …)` behaves like `(a like '%mental%' or b like
//! '%mental%' …) and (a like '%hea%' or …)`. The difference is caching: in a
//! WHERE clause each word's mask is cached on its own, keyed by the column
//! set, so typing `mental hea` → `mental heal` reuses `mental` and checks
//! `heal` only on the rows `hea` matched, across every column at once. An
//! OR of LIKEs across columns rescans them all on every keystroke.

use super::*;

/// Narrow through a cached shorter word only when it admits under 1/N of the rows.
const NARROW_BELOW: usize = 4;

/// A search() conjunct, split once per statement.
pub(super) struct Spec {
    pub(super) cols: Vec<usize>,
    pub(super) words: Vec<String>,
    /// the column set, as the mask cache's narrowing scope
    pub(super) scope: String,
}

/// The query's words: case-folded as LIKE folds, split on anything that is
/// not a letter or digit, deduplicated, and without words another word
/// contains (every row holding `health` holds `heal`).
pub(super) fn words(q: &str) -> Vec<String> {
    let folded = like_fold_str(q);
    let mut all: Vec<&str> = folded.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    all.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    all.dedup();
    let mut out: Vec<String> = Vec::new();
    for w in all {
        if !out.iter().any(|o| o.contains(w)) {
            out.push(w.to_string());
        }
    }
    out
}

/// `search('literal', column, …)` as a WHERE conjunct.
pub(super) fn spec_of(b: &Bound) -> Option<Spec> {
    let Bound::Call { func, args, .. } = b else { return None };
    if func.name != "search" {
        return None;
    }
    let Bound::Str(q) = &args[0] else { return None };
    let cols: Option<Vec<usize>> = args[1..]
        .iter()
        .map(|a| match a {
            Bound::Column { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    let mut cols = cols?;
    cols.sort_unstable();
    cols.dedup();
    let scope = format!("{cols:?}");
    Some(Spec { cols, words: words(q), scope })
}

/// The conjunct's mask for row group `g`: the AND of its words' masks, each
/// from the mask cache or computed (narrowed through a cached shorter word
/// when there is one) and cached.
pub(super) fn group_mask<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    spec: &Spec,
    dicts: &Dicts,
) -> Result<Vec<u8>, FormatError> {
    let n_groups = table.group_count();
    let mut keep = vec![1u8; rows];
    for w in &spec.words {
        let key = format!("search:{}:{w}", spec.scope);
        let bits = match table.masks().get(&key, g) {
            Some(b) => b,
            None => {
                // narrow only through a selective superset: checking row by
                // row is slower than one SIMD scan of the blob once most rows
                // are candidates (`m` → `me` matched nearly every row)
                let sup = table
                    .masks()
                    .word_superset(&spec.scope, w, g)
                    .filter(|b| b.iter().map(|x| x.count_ones() as usize).sum::<usize>() * NARROW_BELOW < rows);
                let m = word_mask(table, g, rows, &spec.cols, w, sup.as_ref().map(|v| v.as_slice()), dicts)?;
                let b = Rc::new(pack_bits(&m));
                table.masks().put_word(&key, g, n_groups, b.clone(), &spec.scope, w);
                b
            }
        };
        for (chunk, &b) in keep.chunks_mut(8).zip(bits.iter()) {
            for (j, k) in chunk.iter_mut().enumerate() {
                *k &= (b >> j) & 1;
            }
        }
    }
    Ok(keep)
}

/// Rows of group `g` where `w` occurs in any of `cols`. With `sup`, only the
/// rows set there are checked (every other row is known not to match).
fn word_mask<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    cols: &[usize],
    w: &str,
    sup: Option<&[u8]>,
    dicts: &Dicts,
) -> Result<Vec<u8>, FormatError> {
    let pat = format!("%{w}%");
    let shape = classify_like(&pat);
    let mut m = vec![0u8; rows];
    let set = |bits: &[u8], i: usize| bits[i / 8] >> (i % 8) & 1 == 1;
    for &c in cols {
        let valid = table.validity(g, c)?;
        let present = |i: usize| valid.as_ref().is_none_or(|v| set(v, i));
        if table.catalog().schema.columns[c].is_dict() {
            // one truth table per (dictionary, word), then a lookup per row
            let codes = table.codes(g, c, rows)?;
            let t = dict_like_table(&dicts[&c], &pat);
            for (i, (mi, &code)) in m.iter_mut().zip(&codes).enumerate() {
                if *mi == 0 && present(i) {
                    *mi = t[code as usize];
                }
            }
            continue;
        }
        let check = |m: &mut [u8], blob: &[u8], off: &dyn Fn(usize) -> usize| {
            let row = |i: usize| &blob[off(i)..off(i + 1)];
            let hit = |s: &[u8]| {
                if w.is_ascii() {
                    crate::text::contains_ci(s, w.as_bytes())
                } else {
                    like_shape_match(&shape, core::str::from_utf8(s).unwrap_or(""), &pat)
                }
            };
            match sup {
                Some(sup) => {
                    for i in 0..rows {
                        if m[i] == 0 && set(sup, i) && present(i) {
                            m[i] = hit(row(i)) as u8;
                        }
                    }
                }
                None if w.is_ascii() => {
                    let mut hits = vec![0u8; rows];
                    crate::text::mark_rows_ci(blob, off, rows, w.as_bytes(), &mut hits);
                    for (i, (mi, h)) in m.iter_mut().zip(hits).enumerate() {
                        *mi |= h & present(i) as u8;
                    }
                }
                None => {
                    for i in 0..rows {
                        if m[i] == 0 && present(i) {
                            m[i] = hit(row(i)) as u8;
                        }
                    }
                }
            }
        };
        let done = table.with_text_segments(g, c, |offs, blob, _| {
            let off = |i: usize| u32::from_le_bytes(offs[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
            check(&mut m, blob, &off);
        })?;
        if done.is_none() {
            let (offs, blob) = table.texts_raw(g, c, rows)?;
            check(&mut m, &blob, &|i| offs[i] as usize);
        }
    }
    Ok(m)
}

/// search() anywhere but a top-level WHERE conjunct (a SELECT item, under
/// OR or NOT): the same test through LIKE, one word and column at a time.
pub(super) fn eval_search(args: &[Bound], ctx: &GroupCtx) -> VV {
    let rows = ctx.rows;
    let Bound::Str(q) = &args[0] else { unreachable!("the binder checks the query literal") };
    let like = super::super::binder::FUNCS.iter().find(|f| f.name == "like").expect("like is built in");
    let mut keep = vec![1u8; rows];
    for w in words(q) {
        let pat = Bound::Str(format!("%{w}%"));
        let mut m = vec![0u8; rows];
        for col in &args[1..] {
            let v = eval_vec(&Bound::Call { func: like, args: vec![col.clone(), pat.clone()], ty: Ty::Bool }, ctx);
            for (i, mi) in m.iter_mut().enumerate() {
                *mi |= (v.bool3_at(i) == Some(true)) as u8;
            }
        }
        for (k, mi) in keep.iter_mut().zip(m) {
            *k &= mi;
        }
    }
    VV::all_valid(Data::Bool(Rc::new(keep)))
}

#[cfg(test)]
mod tests {
    use super::words;

    #[test]
    fn query_words() {
        assert_eq!(words("Mental  health"), ["health", "mental"]);
        assert_eq!(words("heal health HEALTH"), ["health"]);
        assert_eq!(words("youth-sport, London!"), ["london", "sport", "youth"]);
        assert!(words(" -- ").is_empty());
        assert_eq!(words("SÃO paulo"), ["paulo", "são"]);
    }
}
