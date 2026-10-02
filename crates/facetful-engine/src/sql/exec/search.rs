//! search('q', col, …): true when every word of `q` is a word in at least
//! one of the columns (docs/full-text-search.sv).
//!
//! Words are folded and split exactly as the word index splits text
//! (`search_index::for_each_word`). Finished words match whole words. The
//! last word is still being typed, so it matches as a word prefix (`men`
//! finds `mental` and `members`, not `government`), unless the query ends
//! in a space or other separator, which finishes it too.
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

/// Per (dictionary column, word): which values hold the word.
type DictTables = std::cell::RefCell<HashMap<(usize, Word), Rc<Vec<bool>>>>;

/// One query word: folded text, and whether it is the word still being
/// typed (matched as a prefix) or finished (matched whole).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Word {
    pub(super) text: String,
    pub(super) prefix: bool,
}

impl Word {
    /// Every row this word matches, `other` matches too.
    fn implied_by(&self, other: &Word) -> bool {
        if self.prefix {
            other.text.starts_with(&self.text)
        } else {
            !other.prefix && other.text == self.text
        }
    }
    /// The mask cache key; finished words and prefixes never share one.
    fn key(&self, scope: &str) -> String {
        format!("search:{scope}:{}{}", if self.prefix { "" } else { "=" }, self.text)
    }
}

/// A search() conjunct, split once per statement.
pub(super) struct Spec {
    pub(super) cols: Vec<usize>,
    pub(super) words: Vec<Word>,
    /// the column set, as the mask cache's narrowing scope
    pub(super) scope: String,
    /// computed once per statement rather than once per row group
    dict_tables: DictTables,
    /// per word, its forms in the index (corrections included) and the
    /// dictionary codes they admit (once per statement)
    #[allow(clippy::type_complexity)]
    index_forms: std::cell::RefCell<HashMap<Word, Rc<(crate::search_index::Forms, crate::search_index::ValueSets)>>>,
}

/// The query's words, folded and split as the index splits text. The last
/// one is a prefix unless the query ends in a separator. Deduplicated, and
/// without words another word implies (a row with a word starting `health`
/// has one starting `heal`; a finished `heal` is not implied by it).
pub(super) fn words(q: &str) -> Vec<Word> {
    let mut all: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    for_each_word(q.as_bytes(), &mut buf, |w| all.push(String::from_utf8_lossy(w).into_owned()));
    let last_open = q.chars().next_back().is_some_and(|c| c.is_alphanumeric());
    let n = all.len();
    let mut all: Vec<Word> = all
        .into_iter()
        .enumerate()
        .map(|(i, text)| Word { text, prefix: last_open && i + 1 == n })
        .collect();
    all.sort_by(|a, b| b.text.len().cmp(&a.text.len()).then(a.text.cmp(&b.text)).then(a.prefix.cmp(&b.prefix)));
    all.dedup();
    let mut out: Vec<Word> = Vec::new();
    for w in all {
        if !out.iter().any(|o| w.implied_by(o)) {
            out.retain(|o| !o.implied_by(&w));
            out.push(w);
        }
    }
    out
}

/// Does `text` hold the word `w` (as a whole word, or as a word prefix)?
fn has_word(text: &[u8], w: &Word, buf: &mut Vec<u8>) -> bool {
    if w.text.is_ascii() && text.is_ascii() {
        return crate::text::word_ci(text, w.text.as_bytes(), !w.prefix);
    }
    let mut hit = false;
    let wb = w.text.as_bytes();
    for_each_word(text, buf, |t| hit |= if w.prefix { t.starts_with(wb) } else { t == wb });
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
    Some(Spec { cols, words: words(q), scope, dict_tables: Default::default(), index_forms: Default::default() })
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
        // the word's forms in the index, corrections included; once per statement
        let forms = idx.as_ref().map(|idx| {
            spec.index_forms
                .borrow_mut()
                .entry(w.clone())
                .or_insert_with(|| {
                    let f = idx.word_forms(w.text.as_bytes(), w.prefix);
                    let v = idx.value_sets(&f.ranges);
                    Rc::new((f, v))
                })
                .clone()
        });
        // each word's mask is cached on its own, so a keystroke computes only
        // its last word. Both paths give the same rows for the word as typed,
        // so they share a key; a corrected word (index only) has its own
        let corrected = forms.as_ref().is_some_and(|f| !f.0.corrections.is_empty());
        let key = if corrected { format!("{}~", w.key(&spec.scope)) } else { w.key(&spec.scope) };
        let bits = match table.masks().get(&key, g) {
            Some(b) => b,
            None => {
                let m = match (&idx, &forms) {
                    (Some(idx), Some(f)) => {
                        let mut m = vec![0u8; rows];
                        idx.mark_group(table, g, &f.0.ranges, &f.1, &mut m)?;
                        m
                    }
                    _ => {
                        // narrow only through a selective superset: checking
                        // row by row is slower than one SIMD scan of the blob
                        // once most rows are candidates (`m` → `me`). Only
                        // cached prefixes are supersets: of longer prefixes,
                        // and of finished words that start with them
                        let sup = table
                            .masks()
                            .word_superset(&spec.scope, &w.text, g)
                            .filter(|b| b.iter().map(|x| x.count_ones() as usize).sum::<usize>() * NARROW_BELOW < rows);
                        word_mask(table, g, rows, spec, w, sup.as_ref().map(|v| v.as_slice()), dicts)?
                    }
                };
                let b = Rc::new(pack_bits(&m));
                if w.prefix && !corrected {
                    table.masks().put_word(&key, g, n_groups, b.clone(), &spec.scope, &w.text);
                } else {
                    table.masks().put(&key, g, n_groups, b.clone(), None);
                }
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
    w: &Word,
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
                .entry((c, w.clone()))
                .or_insert_with(|| Rc::new(dicts[&c].iter().map(|s| has_word(s.as_bytes(), w, &mut buf)).collect()))
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
                None if w.text.is_ascii() => {
                    let mut hits = vec![0u8; rows];
                    crate::text::mark_rows_word_ci(blob, off, rows, w.text.as_bytes(), !w.prefix, &mut hits);
                    for (i, (mi, h)) in m.iter_mut().zip(hits).enumerate() {
                        *mi |= h & present(i) as u8;
                    }
                }
                _ => {
                    for i in 0..rows {
                        if m[i] == 0 && sup.is_none_or(|s| set(s, i)) && present(i) {
                            m[i] = has_word(row(i), w, &mut buf) as u8;
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
                let t: Vec<bool> = dict.iter().map(|s| has_word(s.as_bytes(), &w, &mut buf)).collect();
                for (i, mi) in m.iter_mut().enumerate() {
                    *mi |= (v.is_valid(i) && t[codes[i] as usize]) as u8;
                }
                continue;
            }
            for (i, mi) in m.iter_mut().enumerate() {
                if *mi == 0 && v.is_valid(i) {
                    if let Some(s) = v.text_at(i) {
                        *mi = has_word(s.as_bytes(), &w, &mut buf) as u8;
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
    use super::{words, Word};

    fn w(text: &str, prefix: bool) -> Word {
        Word { text: text.into(), prefix }
    }

    #[test]
    fn query_words() {
        // the last word is a prefix while it is being typed
        assert_eq!(words("Mental  health"), [w("health", true), w("mental", false)]);
        assert_eq!(words("mental health "), [w("health", false), w("mental", false)]);
        assert_eq!(words("youth-sport, London!"), [w("london", false), w("sport", false), w("youth", false)]);
        // a prefix is implied by a longer word starting with it; a finished word is not
        assert_eq!(words("health heal"), [w("health", false)]);
        assert_eq!(words("heal health"), [w("health", true), w("heal", false)]);
        assert_eq!(words("health HEALTH health"), [w("health", false)]);
        assert_eq!(words("tal mental"), [w("mental", true), w("tal", false)]);
        assert!(words(" -- ").is_empty());
        assert_eq!(words("SÃO paulo"), [w("paulo", true), w("são", false)]);
    }
}
