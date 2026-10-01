//! search('q', col, …): true when every word of `q` starts a word in at
//! least one of the columns (docs/full-text-search.sv).
//!
//! Words are folded and split exactly as the word index splits text
//! (`search_index::for_each_word`), and each query word matches as a word
//! prefix: `men` finds `mental` and `members`, not `government`.
//!
//! Two paths give the same rows. When the table holds a word index over
//! exactly these columns, each word is a range of the index's vocabulary
//! and its rows are a union of per-group lists. Otherwise the columns are
//! scanned. In a WHERE clause each word's scanned mask is cached on its own,
//! keyed by the column set, so typing `mental hea` → `mental heal` reuses
//! `mental` and checks `heal` only on the rows `hea` matched.

use super::*;
use crate::search_index::for_each_word;

/// Narrow through a cached shorter word only when it admits under 1/N of the rows.
const NARROW_BELOW: usize = 4;

/// Per (dictionary column, word): which values have a word starting so.
type DictTables = std::cell::RefCell<HashMap<(usize, String), Rc<Vec<bool>>>>;

/// A search() conjunct, split once per statement.
pub(super) struct Spec {
    pub(super) cols: Vec<usize>,
    pub(super) words: Vec<String>,
    /// the column set, as the mask cache's narrowing scope
    pub(super) scope: String,
    /// computed once per statement rather than once per row group
    dict_tables: DictTables,
    /// per word, the dictionary codes the index admits (once per statement)
    value_sets: std::cell::RefCell<HashMap<String, Rc<crate::search_index::ValueSets>>>,
}

/// The query's words, folded and split as the index splits text,
/// deduplicated, and without words that another word starts with (a row
/// with a word starting `health` has one starting `heal`).
pub(super) fn words(q: &str) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    for_each_word(q.as_bytes(), &mut buf, |w| all.push(String::from_utf8_lossy(w).into_owned()));
    all.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    all.dedup();
    let mut out: Vec<String> = Vec::new();
    for w in all {
        if !out.iter().any(|o| o.starts_with(w.as_str())) {
            out.push(w);
        }
    }
    out
}

/// Does a word of `text` start with folded word `w`?
fn has_word_start(text: &[u8], w: &str, buf: &mut Vec<u8>) -> bool {
    if w.is_ascii() && text.is_ascii() {
        return crate::text::word_start_ci(text, w.as_bytes());
    }
    let mut hit = false;
    for_each_word(text, buf, |t| hit |= t.starts_with(w.as_bytes()));
    hit
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
    Some(Spec { cols, words: words(q), scope, dict_tables: Default::default(), value_sets: Default::default() })
}

/// The conjunct's mask for row group `g`: the AND of its words' masks.
pub(super) fn group_mask<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    spec: &Spec,
    dicts: &Dicts,
) -> Result<Vec<u8>, FormatError> {
    let mut keep = vec![1u8; rows];
    let idx = table.search_index().filter(|i| i.cols == spec.cols);
    let n_groups = table.group_count();
    for w in &spec.words {
        // each word's mask is cached on its own (same key on both paths: they
        // give the same rows), so a keystroke computes only its last word
        let key = format!("search:{}:{w}", spec.scope);
        let bits = match table.masks().get(&key, g) {
            Some(b) => b,
            None => {
                let m = match &idx {
                    // the index: the word a range of the vocabulary, its rows a union
                    Some(idx) => {
                        let ids = idx.prefix_ids(w.as_bytes());
                        let values = spec
                            .value_sets
                            .borrow_mut()
                            .entry(w.clone())
                            .or_insert_with(|| Rc::new(idx.value_sets(ids)))
                            .clone();
                        let mut m = vec![0u8; rows];
                        idx.mark_group(table, g, ids, &values, &mut m)?;
                        m
                    }
                    None => {
                        // narrow only through a selective superset: checking
                        // row by row is slower than one SIMD scan of the blob
                        // once most rows are candidates (`m` → `me`)
                        let sup = table
                            .masks()
                            .word_superset(&spec.scope, w, g)
                            .filter(|b| b.iter().map(|x| x.count_ones() as usize).sum::<usize>() * NARROW_BELOW < rows);
                        word_mask(table, g, rows, spec, w, sup.as_ref().map(|v| v.as_slice()), dicts)?
                    }
                };
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

/// Rows of group `g` where a word in any of `cols` starts with `w`, by
/// scanning. With `sup`, only the rows set there are checked (every other
/// row is known not to match).
fn word_mask<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    spec: &Spec,
    w: &str,
    sup: Option<&[u8]>,
    dicts: &Dicts,
) -> Result<Vec<u8>, FormatError> {
    let mut m = vec![0u8; rows];
    let mut buf = Vec::new();
    let set = |bits: &[u8], i: usize| bits[i / 8] >> (i % 8) & 1 == 1;
    for &c in &spec.cols {
        let valid = table.validity(g, c)?;
        let present = |i: usize| valid.as_ref().is_none_or(|v| set(v, i));
        if table.catalog().schema.columns[c].is_dict() {
            // the word checked once per dictionary value, then a lookup per row
            let codes = table.codes(g, c, rows)?;
            let t = spec
                .dict_tables
                .borrow_mut()
                .entry((c, w.to_string()))
                .or_insert_with(|| Rc::new(dicts[&c].iter().map(|s| has_word_start(s.as_bytes(), w, &mut buf)).collect()))
                .clone();
            for (i, (mi, &code)) in m.iter_mut().zip(&codes).enumerate() {
                if *mi == 0 && present(i) && t[code as usize] {
                    *mi = 1;
                }
            }
            continue;
        }
        let mut check = |m: &mut [u8], blob: &[u8], off: &dyn Fn(usize) -> usize| {
            let row = |i: usize| &blob[off(i)..off(i + 1)];
            match sup {
                None if w.is_ascii() => {
                    let mut hits = vec![0u8; rows];
                    crate::text::mark_rows_word_start_ci(blob, off, rows, w.as_bytes(), &mut hits);
                    for (i, (mi, h)) in m.iter_mut().zip(hits).enumerate() {
                        *mi |= h & present(i) as u8;
                    }
                }
                _ => {
                    for i in 0..rows {
                        if m[i] == 0 && sup.is_none_or(|s| set(s, i)) && present(i) {
                            m[i] = has_word_start(row(i), w, &mut buf) as u8;
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
/// OR or NOT): the same test, row by row.
pub(super) fn eval_search(args: &[Bound], ctx: &GroupCtx) -> VV {
    let rows = ctx.rows;
    let Bound::Str(q) = &args[0] else { unreachable!("the binder checks the query literal") };
    let cols: Vec<VV> = args[1..].iter().map(|a| eval_vec(a, ctx)).collect();
    let mut buf = Vec::new();
    let mut keep = vec![1u8; rows];
    for w in words(q) {
        let mut m = vec![0u8; rows];
        for v in &cols {
            if let Data::Codes { codes, dict } = &v.data {
                let t: Vec<bool> = dict.iter().map(|s| has_word_start(s.as_bytes(), &w, &mut buf)).collect();
                for (i, mi) in m.iter_mut().enumerate() {
                    *mi |= (v.is_valid(i) && t[codes[i] as usize]) as u8;
                }
                continue;
            }
            for (i, mi) in m.iter_mut().enumerate() {
                if *mi == 0 && v.is_valid(i) {
                    if let Some(s) = v.text_at(i) {
                        *mi = has_word_start(s.as_bytes(), &w, &mut buf) as u8;
                    }
                }
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
        // `tal` is inside `mental` but is not a prefix of it: both stay
        assert_eq!(words("tal mental"), ["mental", "tal"]);
        assert_eq!(words("youth-sport, London!"), ["london", "sport", "youth"]);
        assert!(words(" -- ").is_empty());
        assert_eq!(words("SÃO paulo"), ["paulo", "são"]);
    }
}
