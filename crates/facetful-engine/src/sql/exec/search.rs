//! search('q', col, …): true when every word of `q` is a word in at least
//! one of the columns, and every quoted phrase occurs in one of them
//! (docs/full-text-search.sv).
//!
//! Words are folded and split exactly as the word index splits text
//! (`search_index::for_each_word`). Finished words match whole words. The
//! last word is still being typed, so it matches as a word prefix (`men`
//! finds `mental` and `members`, not `government`), unless the query ends
//! in a space or other separator, which finishes it too.
//!
//! With a word index over exactly these columns, a finished word also
//! matches its other spellings (`organisation`/`organization`), its plural,
//! `-ed` and `-ing` forms, and corrections when it looks like a typo; the
//! word being typed is corrected when nothing starts with it. A quoted word
//! (`"heath"`) matches only itself. A quoted phrase (`"mental health"`)
//! matches its words next to each other, in order, within one field: the
//! rows holding every word are found first, then their text is checked. An
//! unclosed quote is a phrase still being typed (its last word a prefix).
//!
//! Without the index the columns are scanned, and words match as typed. In
//! a WHERE clause each word's mask is cached on its own, keyed by the column
//! set, so typing `mental hea` → `mental heal` reuses `mental` and checks
//! `heal` only on the rows `hea` matched.

use super::*;
use crate::search_index::{for_each_word, Forms, SearchIndex, ValueSets};

/// Narrow through a cached shorter word only when it admits under 1/N of the rows.
const NARROW_BELOW: usize = 4;
/// Words of a phrase checked at most (a phrase's state is a 64-bit set).
const PHRASE_WORDS: usize = 63;

/// One query word: folded text; whether it is still being typed (matched as
/// a prefix) or finished (matched whole); whether it was quoted (matched
/// only as typed, never corrected, respelled or inflected).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Word {
    pub(super) text: String,
    pub(super) prefix: bool,
    pub(super) literal: bool,
}

impl Word {
    /// The mask cache key of the word as typed: the scan and the index agree
    /// on it, quoted or not; finished words and prefixes never share one.
    fn key(&self, scope: &str) -> String {
        format!("search:{scope}:{}{}", if self.prefix { "" } else { "=" }, self.text)
    }
}

/// A quoted phrase: its words, the last a prefix while it is being typed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Phrase {
    pub(super) words: Vec<String>,
    pub(super) prefix: bool,
}

impl Phrase {
    fn key(&self, scope: &str) -> String {
        format!("search:{scope}:\"{}\"{}", self.words.join(" "), if self.prefix { "…" } else { "" })
    }
    /// The phrase's words as quoted words: the rows holding all of them are
    /// its candidates.
    fn word_items(&self) -> Vec<Word> {
        let n = self.words.len();
        self.words
            .iter()
            .enumerate()
            .map(|(i, w)| Word { text: w.clone(), prefix: self.prefix && i + 1 == n, literal: true })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Item {
    Word(Word),
    Phrase(Phrase),
}

/// Per (dictionary column, item key): which values hold the word or phrase.
type DictTables = std::cell::RefCell<HashMap<(usize, String), Rc<Vec<bool>>>>;

/// A search() conjunct, split once per statement.
pub(super) struct Spec {
    pub(super) cols: Vec<usize>,
    pub(super) items: Vec<Item>,
    /// the column set, as the mask cache's narrowing scope
    pub(super) scope: String,
    /// computed once per statement rather than once per row group
    dict_tables: DictTables,
    /// per word, its forms in the index and the dictionary codes they admit
    /// (once per statement)
    index_forms: std::cell::RefCell<HashMap<Word, Rc<(Forms, ValueSets)>>>,
    /// per (dictionary column, phrase key): which values hold the phrase,
    /// filled in as candidate rows need them
    phrase_values: std::cell::RefCell<HashMap<(usize, String), Vec<u8>>>,
}

/// The query's items: quoted stretches (one word: a quoted word; more: a
/// phrase), and the words between them, folded and split as the index
/// splits text. The last item's last word is a prefix unless the query ends
/// in a separator or a closing quote. An unclosed quote runs to the end.
/// Repeats are dropped.
pub(super) fn parse(q: &str) -> Vec<Item> {
    let open_end = q.chars().next_back().is_some_and(|c| c.is_alphanumeric());
    let mut buf = Vec::new();
    let mut items: Vec<Item> = Vec::new();
    for (k, part) in q.split('"').enumerate() {
        let mut words: Vec<String> = Vec::new();
        for_each_word(part.as_bytes(), &mut buf, |w| words.push(String::from_utf8_lossy(w).into_owned()));
        if k % 2 == 1 {
            // inside quotes
            words.truncate(PHRASE_WORDS);
            match words.len() {
                0 => {}
                1 => items.push(Item::Word(Word { text: words.pop().unwrap(), prefix: false, literal: true })),
                _ => items.push(Item::Phrase(Phrase { words, prefix: false })),
            }
        } else {
            items.extend(words.into_iter().map(|text| Item::Word(Word { text, prefix: false, literal: false })));
        }
    }
    if open_end {
        match items.last_mut() {
            Some(Item::Word(w)) => w.prefix = true,
            Some(Item::Phrase(p)) => p.prefix = true,
            None => {}
        }
    }
    let mut out: Vec<Item> = Vec::new();
    for it in items {
        if !out.contains(&it) {
            out.push(it);
        }
    }
    out
}

/// Does `text` hold the word `w` as typed (whole, or as a word prefix)?
fn has_word(text: &[u8], w: &Word, buf: &mut Vec<u8>) -> bool {
    if w.text.is_ascii() && text.is_ascii() {
        return crate::text::word_ci(text, w.text.as_bytes(), !w.prefix);
    }
    let mut hit = false;
    let wb = w.text.as_bytes();
    for_each_word(text, buf, |t| hit |= if w.prefix { t.starts_with(wb) } else { t == wb });
    hit
}

/// Does `text` hold the phrase's words consecutively, in order?
fn has_phrase(text: &[u8], p: &Phrase, buf: &mut Vec<u8>) -> bool {
    if text.is_ascii() && p.words.iter().all(|w| w.is_ascii()) {
        return has_phrase_ascii(text, p);
    }
    has_phrase_words(text, p, buf)
}

/// `has_phrase` by splitting the text into words (any text).
fn has_phrase_words(text: &[u8], p: &Phrase, buf: &mut Vec<u8>) -> bool {
    let n = p.words.len();
    let mut states = 0u64; // bit i: the first i words end at the previous word
    let mut found = false;
    for_each_word(text, buf, |t| {
        if found {
            return;
        }
        let mut next = 0u64;
        let mut live = states | 1;
        while live != 0 {
            let i = live.trailing_zeros() as usize;
            live &= live - 1;
            let w = p.words[i].as_bytes();
            let hit = if p.prefix && i + 1 == n { t.starts_with(w) } else { t == w };
            if hit {
                if i + 1 == n {
                    found = true;
                    return;
                }
                next |= 1 << (i + 1);
            }
        }
        states = next;
    });
    found
}

/// `has_phrase` for ASCII text: find the first word as a whole word (a SIMD
/// search), and at each hit compare the words that follow, skipping the
/// separators between them, instead of splitting the whole text into words.
fn has_phrase_ascii(text: &[u8], p: &Phrase) -> bool {
    use crate::text::{at_word_end, at_word_start, eq_ci, find_ci};
    let (n, len) = (p.words.len(), text.len());
    let first = p.words[0].as_bytes();
    let mut pos = 0;
    'hits: while let Some(hit) = find_ci(text, pos, first) {
        pos = hit + 1;
        let mut end = hit + first.len();
        if !at_word_start(text, 0, hit) || !at_word_end(text, len, end) {
            continue;
        }
        for (k, w) in p.words.iter().enumerate().skip(1) {
            let w = w.as_bytes();
            let mut s = end;
            while s < len && !text[s].is_ascii_alphanumeric() {
                s += 1;
            }
            let e = s + w.len();
            if e > len || !eq_ci(&text[s..e], w) || !(p.prefix && k + 1 == n || at_word_end(text, len, e)) {
                continue 'hits;
            }
            end = e;
        }
        return true;
    }
    false
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
    Some(Spec { cols, items: parse(q), scope, dict_tables: Default::default(), index_forms: Default::default(), phrase_values: Default::default() })
}

/// The conjunct's mask for row group `g`: the AND of its items' masks.
pub(super) fn group_mask<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    spec: &Spec,
    dicts: &Dicts,
) -> Result<Vec<u8>, FormatError> {
    let mut keep = vec![1u8; rows];
    let idx = table.search_index().filter(|i| i.cols == spec.cols);
    for item in &spec.items {
        let bits = match item {
            Item::Word(w) => word_bits(table, g, rows, spec, w, idx.as_deref(), dicts)?,
            Item::Phrase(p) => phrase_bits(table, g, rows, spec, p, idx.as_deref(), dicts)?,
        };
        for (chunk, &b) in keep.chunks_mut(8).zip(bits.iter()) {
            for (j, k) in chunk.iter_mut().enumerate() {
                *k &= (b >> j) & 1;
            }
        }
    }
    Ok(keep)
}

/// One word's rows in group `g` (packed bits), from the mask cache or
/// computed and cached: through the index when there is one, else scanned.
fn word_bits<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    spec: &Spec,
    w: &Word,
    idx: Option<&SearchIndex>,
    dicts: &Dicts,
) -> Result<Rc<Vec<u8>>, FormatError> {
    // the word's forms in the index; once per statement
    let forms = idx.map(|idx| {
        spec.index_forms
            .borrow_mut()
            .entry(w.clone())
            .or_insert_with(|| {
                let f = idx.word_forms(w.text.as_bytes(), w.prefix, w.literal);
                let v = idx.value_sets(&f.ranges);
                Rc::new((f, v))
            })
            .clone()
    });
    // each word's mask is cached on its own, so a keystroke computes only its
    // last word. Both paths give the same rows for the word as typed, so they
    // share a key; a word the index expands (respelled, inflected,
    // corrected) has its own
    let expanded = forms.as_ref().is_some_and(|f| f.0.expanded());
    let key = if expanded { format!("{}~", w.key(&spec.scope)) } else { w.key(&spec.scope) };
    if let Some(b) = table.masks().get(&key, g) {
        return Ok(b);
    }
    let m = match (idx, &forms) {
        (Some(idx), Some(f)) => {
            let mut m = vec![0u8; rows];
            idx.mark_group(table, g, &f.0.ranges, &f.1, &mut m)?;
            m
        }
        _ => {
            // narrow only through a selective superset: checking row by row
            // is slower than one SIMD scan of the blob once most rows are
            // candidates (`m` → `me`). Only cached prefixes are supersets: of
            // longer prefixes, and of finished words that start with them
            let sup = table
                .masks()
                .word_superset(&spec.scope, &w.text, g)
                .filter(|b| b.iter().map(|x| x.count_ones() as usize).sum::<usize>() * NARROW_BELOW < rows);
            word_mask(table, g, rows, spec, w, sup.as_ref().map(|v| v.as_slice()), dicts)?
        }
    };
    let b = Rc::new(pack_bits(&m));
    let n_groups = table.group_count();
    if w.prefix && !expanded {
        table.masks().put_word(&key, g, n_groups, b.clone(), &spec.scope, &w.text);
    } else {
        table.masks().put(&key, g, n_groups, b.clone(), None);
    }
    Ok(b)
}

/// A phrase's rows in group `g` (packed bits): the rows holding each of its
/// words as typed are the candidates, then their text is checked for the
/// words in a row, in order, within one field.
fn phrase_bits<S: ReadAt>(
    table: &mut Table<S>,
    g: usize,
    rows: usize,
    spec: &Spec,
    p: &Phrase,
    idx: Option<&SearchIndex>,
    dicts: &Dicts,
) -> Result<Rc<Vec<u8>>, FormatError> {
    let key = p.key(&spec.scope);
    if let Some(b) = table.masks().get(&key, g) {
        return Ok(b);
    }
    let mut cand = vec![1u8; rows];
    for w in p.word_items() {
        let bits = word_bits(table, g, rows, spec, &w, idx, dicts)?;
        for (chunk, &b) in cand.chunks_mut(8).zip(bits.iter()) {
            for (j, k) in chunk.iter_mut().enumerate() {
                *k &= (b >> j) & 1;
            }
        }
    }
    let mut m = vec![0u8; rows];
    let mut buf = Vec::new();
    if cand.contains(&1) {
        let set = |bits: &[u8], i: usize| bits[i / 8] >> (i % 8) & 1 == 1;
        for &c in &spec.cols {
            let valid = table.validity(g, c)?;
            let present = |i: usize| valid.as_ref().is_none_or(|v| set(v, i));
            if table.catalog().schema.columns[c].is_dict() {
                // only the values candidate rows use are checked, each once
                // per statement (0: not yet, 1: no, 2: yes); checking all of
                // Description Common's 61K values cost about 70 ms a phrase
                let codes = table.codes(g, c, rows)?;
                let dict = &dicts[&c];
                let mut lazy = spec.phrase_values.borrow_mut();
                let seen = lazy.entry((c, key.clone())).or_insert_with(|| vec![0u8; dict.len()]);
                for (i, (mi, &code)) in m.iter_mut().zip(&codes).enumerate() {
                    if *mi == 0 && cand[i] == 1 && present(i) {
                        let v = &mut seen[code as usize];
                        if *v == 0 {
                            *v = 1 + has_phrase(dict[code as usize].as_bytes(), p, &mut buf) as u8;
                        }
                        *mi = (*v == 2) as u8;
                    }
                }
                continue;
            }
            let mut check = |m: &mut [u8], blob: &[u8], off: &dyn Fn(usize) -> usize| {
                for i in 0..rows {
                    if m[i] == 0 && cand[i] == 1 && present(i) {
                        m[i] = has_phrase(&blob[off(i)..off(i + 1)], p, &mut buf) as u8;
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
    }
    let b = Rc::new(pack_bits(&m));
    let n_groups = table.group_count();
    table.masks().put(&key, g, n_groups, b.clone(), None);
    Ok(b)
}

/// Rows of group `g` where a word in any of the columns is `w` as typed
/// (whole, or as a prefix), by scanning. With `sup`, only the rows set there
/// are checked (every other row is known not to match).
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
                .entry((c, w.key("")))
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
/// OR or NOT): the same test row by row, with words matched as typed (the
/// word index, and so respelling, inflections and corrections, is only
/// reached through a WHERE conjunct).
pub(super) fn eval_search(args: &[Bound], ctx: &GroupCtx) -> VV {
    let rows = ctx.rows;
    let Bound::Str(q) = &args[0] else { unreachable!("the binder checks the query literal") };
    let cols: Vec<VV> = args[1..].iter().map(|a| eval_vec(a, ctx)).collect();
    let mut buf = Vec::new();
    let mut keep = vec![1u8; rows];
    for item in parse(q) {
        let test = |s: &[u8], buf: &mut Vec<u8>| match &item {
            Item::Word(w) => has_word(s, w, buf),
            Item::Phrase(p) => has_phrase(s, p, buf),
        };
        let mut m = vec![0u8; rows];
        for v in &cols {
            if let Data::Codes { codes, dict } = &v.data {
                let t: Vec<bool> = dict.iter().map(|s| test(s.as_bytes(), &mut buf)).collect();
                for (i, mi) in m.iter_mut().enumerate() {
                    *mi |= (v.is_valid(i) && t[codes[i] as usize]) as u8;
                }
                continue;
            }
            for (i, mi) in m.iter_mut().enumerate() {
                if *mi == 0 && v.is_valid(i) {
                    if let Some(s) = v.text_at(i) {
                        *mi = test(s.as_bytes(), &mut buf) as u8;
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

// ---- suggestions: completions of the word being typed, did-you-mean ----

/// Candidate words examined at most for completions, most common first.
const EXAMINE: usize = 1000;
/// Did-you-mean offers a word's neighbour when it has this many times the
/// word's rows (lower than the automatic correction's 200: here the user
/// chooses).
const DID_YOU_MEAN_RATIO: u64 = 20;

pub struct SuggestOptions {
    /// completions returned at most
    pub limit: usize,
    /// also look for a better query
    pub did_you_mean: bool,
}

/// One completion of the word being typed: the word, and the whole query
/// with it in place of what was typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub word: String,
    pub query: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Suggestions {
    /// whether a word index over these columns answered (without one: no
    /// completions, corrections or did-you-mean)
    pub indexed: bool,
    /// rows the word being typed has as a prefix over the whole table, from
    /// the index's per-word totals (a row with the word in two columns counts
    /// twice): the app's measure of "too broad"
    pub prefix_rows: u64,
    /// completions of the word being typed, most common words first
    pub completions: Vec<Completion>,
    /// words the search corrected by itself: (as typed, corrections)
    pub corrected: Vec<(String, Vec<String>)>,
    /// a better query, when asked for and one with rows was found
    pub did_you_mean: Option<String>,
}

fn spec_for(cols: &[usize], items: Vec<Item>) -> Spec {
    Spec { cols: cols.to_vec(), items, scope: format!("{cols:?}"), dict_tables: Default::default(), index_forms: Default::default(), phrase_values: Default::default() }
}

/// The query again from its items, the last word left open if it was.
fn render(items: &[Item]) -> String {
    let mut out: Vec<String> = Vec::new();
    for it in items {
        out.push(match it {
            Item::Word(w) if w.literal => format!("\"{}\"", w.text),
            Item::Word(w) => w.text.clone(),
            Item::Phrase(p) => format!("\"{}{}", p.words.join(" "), if p.prefix { "" } else { "\"" }),
        });
    }
    let mut q = out.join(" ");
    if !matches!(items.last(), Some(Item::Word(w)) if w.prefix) && !matches!(items.last(), Some(Item::Phrase(p)) if p.prefix) && !q.is_empty() && !q.ends_with('"') {
        q.push(' ');
    }
    q
}

/// Rows `spec` matches in each group, ANDed with `filter`.
fn matching<S: ReadAt>(
    table: &mut Table<S>,
    spec: &Spec,
    filter: Option<&[Option<Vec<u8>>]>,
    dicts: &Dicts,
) -> Result<Vec<Vec<u8>>, FormatError> {
    let mut masks = Vec::with_capacity(table.group_count());
    for g in 0..table.group_count() {
        let rows = table.group_rows(g);
        let mut m = if spec.items.is_empty() { vec![1u8; rows] } else { group_mask(table, g, rows, spec, dicts)? };
        if let Some(Some(f)) = filter.map(|f| &f[g]) {
            for (a, &b) in m.iter_mut().zip(f) {
                *a &= b;
            }
        }
        masks.push(m);
    }
    Ok(masks)
}

/// What to offer for query `q` typed so far over `cols`, with the app's
/// other conditions keeping `filter`. No counts: the app runs the query.
///
/// - completions of the word being typed (the last word, unquoted, with no
///   separator after it): the prefix's words (and, when it looks like a
///   typo, those of prefixes one edit away), most common first, keeping
///   those found in at least one row the other words and the filter leave.
///   Each check stops at the first such row.
/// - the words corrected automatically, for "including results for".
/// - did-you-mean, on request: the query with each corrected word replaced
///   by its best correction (the word being typed by its first completion),
///   and each real word whose neighbour has `DID_YOU_MEAN_RATIO` times its
///   rows replaced by it; offered when it finds a row.
pub(super) fn suggest<S: ReadAt>(
    table: &mut Table<S>,
    cols: &[usize],
    q: &str,
    filter: Option<&[Option<Vec<u8>>]>,
    opts: &SuggestOptions,
) -> Result<Suggestions, FormatError> {
    let mut cols = cols.to_vec();
    cols.sort_unstable();
    cols.dedup();
    let idx = table.search_index().filter(|i| i.cols == cols);
    let mut out = Suggestions { indexed: idx.is_some(), ..Default::default() };
    let Some(idx) = idx else { return Ok(out) };
    let mut dicts: Dicts = HashMap::new();
    for &c in &cols {
        if table.catalog().schema.columns[c].is_dict() {
            dicts.insert(c, table.dictionary_rc(c)?);
        }
    }
    let items = parse(q);

    // automatic corrections, as search() applied them
    for it in &items {
        if let Item::Word(w) = it {
            let f = idx.word_forms(w.text.as_bytes(), w.prefix, w.literal);
            if !f.corrections.is_empty() {
                let fixes = f.corrections.iter().map(|c| String::from_utf8_lossy(&c.0).into_owned() + if w.prefix { "…" } else { "" }).collect();
                out.corrected.push((w.text.clone(), fixes));
            }
        }
    }

    // completions of the word being typed
    if let Some(Item::Word(w)) = items.last().filter(|it| matches!(it, Item::Word(w) if w.prefix && !w.literal)) {
        // the prefix's words, and those of its corrections when it looks like a typo
        let ranges = idx.word_forms(w.text.as_bytes(), true, false).ranges;
        let mut cands: Vec<u32> = ranges.iter().flat_map(|r| r.clone()).collect();
        cands.sort_unstable();
        cands.dedup();
        out.prefix_rows = cands.iter().map(|&id| idx.rows_of(id) as u64).sum();
        cands.sort_unstable_by(|&a, &b| idx.rows_of(b).cmp(&idx.rows_of(a)).then(a.cmp(&b)));
        cands.truncate(EXAMINE);
        let context = spec_for(&cols, items[..items.len() - 1].to_vec());
        let mut found: Vec<u32> = Vec::new();
        if context.items.is_empty() && filter.is_none() {
            found = cands.into_iter().take(opts.limit).collect();
        } else {
            let ctx = matching(table, &context, filter, &dicts)?;
            // per dictionary column, the values some context row has
            let mut present: Vec<(usize, Vec<bool>)> = Vec::new();
            for &c in &cols {
                if !dicts.contains_key(&c) {
                    continue;
                }
                let mut seen = vec![false; dicts[&c].len()];
                for (g, m) in ctx.iter().enumerate() {
                    if !m.contains(&1) {
                        continue;
                    }
                    let codes = table.codes(g, c, m.len())?;
                    let valid = table.validity(g, c)?;
                    for (r, (&k, &code)) in m.iter().zip(&codes).enumerate() {
                        if k == 1 && valid.as_ref().is_none_or(|v| v[r / 8] >> (r % 8) & 1 == 1) {
                            seen[code as usize] = true;
                        }
                    }
                }
                present.push((c, seen));
            }
            for id in cands {
                if found.len() == opts.limit {
                    break;
                }
                let in_dict = idx.dict_codes(id).any(|(c, codes)| {
                    present.iter().find(|p| p.0 == c).is_some_and(|(_, seen)| codes.iter().any(|&k| seen[k as usize]))
                });
                if in_dict || ctx.iter().enumerate().any(|(g, m)| idx.any_row(g, id, |r| m[r] == 1)) {
                    found.push(id);
                }
            }
        }
        let head = render(&items[..items.len() - 1]);
        let head = if head.is_empty() || head.ends_with(' ') { head } else { head + " " };
        for id in found {
            let word = String::from_utf8_lossy(idx.word(id)).into_owned();
            out.completions.push(Completion { query: format!("{head}{word}"), word });
        }
    }

    // did-you-mean: corrections made explicit, and real words with a far more common neighbour
    if opts.did_you_mean {
        let mut alt = items.clone();
        let mut changed = false;
        for it in alt.iter_mut() {
            let Item::Word(w) = it else { continue };
            if w.literal {
                continue;
            }
            let f = idx.word_forms(w.text.as_bytes(), w.prefix, false);
            let best = if w.prefix && !f.corrections.is_empty() {
                // the word being typed, misspelt: its first completion, a
                // whole word (`climt` → `climate`, not the prefix `clim`)
                out.completions.first().map(|c| c.word.clone().into_bytes())
            } else if !f.corrections.is_empty() {
                f.corrections.first().map(|c| c.0.clone())
            } else if !w.prefix && f.own_rows > 0 {
                let variants = idx.spelling_variants(w.text.as_bytes());
                let own = w.text.as_bytes();
                idx.neighbors(own, false)
                    .into_iter()
                    .filter(|c| !variants.contains(&c.0) && !crate::search_index::plural_pair(&c.0, own) && !f.ranges.iter().any(|r| r.contains(&c.1.start)))
                    .filter(|c| c.2 >= DID_YOU_MEAN_RATIO * f.own_rows)
                    .max_by_key(|c| c.2)
                    .map(|c| c.0)
            } else {
                None
            };
            if let Some(b) = best {
                w.text = String::from_utf8_lossy(&b).into_owned();
                w.prefix = false;
                changed = true;
            }
        }
        if changed {
            let alt_spec = spec_for(&cols, alt.clone());
            if matching(table, &alt_spec, filter, &dicts)?.iter().any(|m| m.contains(&1)) {
                out.did_you_mean = Some(render(&alt));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{has_phrase, has_phrase_ascii, has_phrase_words, parse, Item, Phrase, Word};

    fn w(text: &str, prefix: bool) -> Item {
        Item::Word(Word { text: text.into(), prefix, literal: false })
    }
    fn q(text: &str, prefix: bool) -> Item {
        Item::Word(Word { text: text.into(), prefix, literal: true })
    }
    fn p(words: &[&str], prefix: bool) -> Item {
        Item::Phrase(Phrase { words: words.iter().map(|s| s.to_string()).collect(), prefix })
    }

    #[test]
    fn query_items() {
        // the last word is a prefix while it is being typed
        assert_eq!(parse("Mental  health"), [w("mental", false), w("health", true)]);
        assert_eq!(parse("mental health "), [w("mental", false), w("health", false)]);
        assert_eq!(parse("youth-sport, London!"), [w("youth", false), w("sport", false), w("london", false)]);
        assert_eq!(parse("health HEALTH health"), [w("health", false), w("health", true)]);
        assert!(parse(" -- ").is_empty());
        assert_eq!(parse("SÃO paulo"), [w("são", false), w("paulo", true)]);
        // quotes: one word is a quoted word, more a phrase; a closing quote finishes
        assert_eq!(parse("\"heath\""), [q("heath", false)]);
        assert_eq!(parse("\"Mental Health\" london"), [p(&["mental", "health"], false), w("london", true)]);
        assert_eq!(parse("london \"mental health\""), [w("london", false), p(&["mental", "health"], false)]);
        // an unclosed quote is a phrase still being typed
        assert_eq!(parse("\"mental hea"), [p(&["mental", "hea"], true)]);
        assert_eq!(parse("\"mental hea "), [p(&["mental", "hea"], false)]);
        assert_eq!(parse("\"\" x"), [w("x", true)]);
    }

    #[test]
    fn phrases_in_text() {
        let ph = |words: &[&str], prefix: bool| Phrase { words: words.iter().map(|s| s.to_string()).collect(), prefix };
        let mut buf = Vec::new();
        let mut t = |s: &str, p: &Phrase| has_phrase(s.as_bytes(), p, &mut buf);
        assert!(t("Funding for mental-health services", &ph(&["mental", "health"], false)));
        assert!(t("MENTAL HEALTH and wellbeing", &ph(&["mental", "health"], false)));
        assert!(!t("health and mental", &ph(&["mental", "health"], false)), "order counts");
        assert!(!t("mental and health", &ph(&["mental", "health"], false)), "next to each other");
        assert!(t("mental heal", &ph(&["mental", "hea"], true)), "the last word typed so far");
        assert!(!t("mental heal", &ph(&["mental", "hea"], false)));
        // a repeated word does not lose a match that starts inside an earlier attempt
        assert!(t("a a a b", &ph(&["a", "a", "b"], false)));
        assert!(t("São Paulo youth", &ph(&["são", "paulo"], false)));
    }

    #[test]
    fn ascii_phrase_check_agrees_with_splitting() {
        // every phrase over a small vocabulary against texts built from it,
        // with mixed case, separators, and words that start alike
        let vocab = ["a", "ab", "b", "mental", "health", "healthy", "x1"];
        let seps = [" ", "-", ", ", "  ", "/"];
        let mut texts = Vec::new();
        let mut seed = 7u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 8) as usize
        };
        for _ in 0..400 {
            let n = 1 + next() % 6;
            let mut t = String::new();
            for i in 0..n {
                if i > 0 {
                    t.push_str(seps[next() % seps.len()]);
                }
                let w = vocab[next() % vocab.len()];
                t.push_str(&if next() % 3 == 0 { w.to_uppercase() } else { w.to_string() });
            }
            texts.push(t);
        }
        let mut buf = Vec::new();
        for a in vocab {
            for b in vocab {
                for c in [None, Some("b"), Some("health")] {
                    for prefix in [false, true] {
                        let mut words = vec![a.to_string(), b.to_string()];
                        words.extend(c.map(String::from));
                        let p = Phrase { words, prefix };
                        for t in &texts {
                            assert_eq!(has_phrase_ascii(t.as_bytes(), &p), has_phrase_words(t.as_bytes(), &p, &mut buf), "{p:?} in {t:?}");
                        }
                    }
                }
            }
        }
    }
}
