//! Filter-mask cache: per-conjunct, per-row-group bitmaps of the rows that
//! pass one top-level AND operand of a WHERE clause.
//!
//! Faceting interfaces fire bursts of queries over one immutable table that
//! share most of their filter, with each facet dropping only its own
//! predicate. Caching the mask per *conjunct* rather than per whole WHERE
//! means every one of those variants is composed from cached bitmaps with a
//! bitwise AND, and a text search (LIKE over a notes column, the expensive
//! predicate) is scanned once per keystroke instead of once per query.
//!
//! Semantics: a bit is set only when the conjunct is TRUE (not NULL), so
//! AND-composition of the bits equals the pass-bit of the conjunction under
//! three-valued logic. Negations are their own conjunct and are never
//! derived from a cached positive mask.
//!
//! Tables are compiled images and never change, so entries never go stale;
//! the only bound is bytes, evicted least-recently-used per conjunct. Masks
//! fill lazily per row group (LIMIT queries stop early, pruned groups are
//! never scanned), so partially filled entries are normal.
//!
//! LIKE narrowing: every row matching `%coal%` also matches `%coa%`. When a
//! contains-shape LIKE arrives whose needle extends a cached one on the same
//! column, the executor verifies only the rows set in the cached mask instead
//! of scanning the blob — search-as-you-type costs a fraction per keystroke.

use std::collections::HashMap;
use std::rc::Rc;

/// Default byte budget: ~25 whole-table masks at 5M rows, hundreds at 200K.
pub const DEFAULT_BUDGET: usize = 16 << 20;

/// A contains-shape LIKE over a plain (non-dictionary) text column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LikeKey {
    pub col: usize,
    /// ASCII-lowercased needle (the kernels fold case the same way).
    pub needle: String,
}

struct Entry {
    groups: Vec<Option<Rc<Vec<u8>>>>,
    last_used: u64,
    like: Option<LikeKey>,
}

pub struct MaskCache {
    entries: HashMap<String, Entry>,
    bytes: usize,
    budget: usize,
    tick: u64,
    pub hits: u64,
    pub misses: u64,
    /// LIKE masks computed by verifying a cached superset instead of scanning.
    pub narrowed: u64,
}

impl Default for MaskCache {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET)
    }
}

impl MaskCache {
    pub fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            budget,
            tick: 0,
            hits: 0,
            misses: 0,
            narrowed: 0,
        }
    }

    pub fn set_budget(&mut self, bytes: usize) {
        self.budget = bytes;
        self.evict_to_budget(0);
    }

    /// (conjuncts held, bitmap bytes held)
    pub fn stats(&self) -> (usize, usize) {
        (self.entries.len(), self.bytes)
    }

    fn touch(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Cached mask for conjunct `key` over row group `g`, if present.
    pub fn get(&mut self, key: &str, g: usize) -> Option<Rc<Vec<u8>>> {
        let tick = self.touch();
        let hit = self.entries.get_mut(key).and_then(|e| {
            e.last_used = tick;
            e.groups.get(g).cloned().flatten()
        });
        if hit.is_some() {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        hit
    }

    /// Store the mask of `key` over group `g` (`n_groups` sizes a new entry).
    pub fn put(
        &mut self,
        key: &str,
        g: usize,
        n_groups: usize,
        bits: Rc<Vec<u8>>,
        like: Option<LikeKey>,
    ) {
        self.evict_to_budget(bits.len());
        let tick = self.touch();
        let e = self.entries.entry(key.to_string()).or_insert_with(|| Entry {
            groups: vec![None; n_groups],
            last_used: tick,
            like,
        });
        e.last_used = tick;
        if g < e.groups.len() {
            if let Some(old) = e.groups[g].replace(bits) {
                self.bytes -= old.len();
            }
            self.bytes += e.groups[g].as_ref().map_or(0, |b| b.len());
        }
    }

    /// The tightest cached superset for a contains-LIKE: the longest cached
    /// needle on `col` that is a proper substring of `needle`, with group `g`
    /// filled. Every row matching `needle` is set in the returned mask.
    pub fn like_superset(&mut self, col: usize, needle: &str, g: usize) -> Option<Rc<Vec<u8>>> {
        let best_key = self
            .entries
            .iter()
            .filter_map(|(k, e)| {
                let lk = e.like.as_ref()?;
                (lk.col == col
                    && lk.needle.len() < needle.len()
                    && needle.contains(lk.needle.as_str())
                    && e.groups.get(g).is_some_and(|m| m.is_some()))
                .then_some((lk.needle.len(), k.clone()))
            })
            .max()
            .map(|(_, k)| k)?;
        let tick = self.touch();
        let e = self.entries.get_mut(&best_key)?;
        e.last_used = tick;
        self.narrowed += 1;
        e.groups[g].clone()
    }

    fn evict_to_budget(&mut self, incoming: usize) {
        while self.bytes + incoming > self.budget && !self.entries.is_empty() {
            let key = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
                .expect("non-empty");
            if let Some(e) = self.entries.remove(&key) {
                self.bytes -= e.groups.iter().flatten().map(|b| b.len()).sum::<usize>();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(n: u8) -> Rc<Vec<u8>> {
        Rc::new(vec![n])
    }

    #[test]
    fn get_put_and_lazy_groups() {
        let mut c = MaskCache::new(1024);
        assert!(c.get("a", 0).is_none());
        c.put("a", 1, 3, bits(0b101), None);
        assert!(c.get("a", 0).is_none(), "group 0 not filled yet");
        assert_eq!(*c.get("a", 1).unwrap(), vec![0b101]);
        assert_eq!(c.stats(), (1, 1));
        assert_eq!((c.hits, c.misses), (1, 2));
    }

    #[test]
    fn evicts_least_recently_used_conjunct() {
        let mut c = MaskCache::new(2);
        c.put("a", 0, 1, bits(1), None);
        c.put("b", 0, 1, bits(2), None);
        c.get("a", 0); // a is now more recent than b
        c.put("c", 0, 1, bits(3), None); // over budget: b goes
        assert!(c.get("b", 0).is_none());
        assert!(c.get("a", 0).is_some());
        assert!(c.get("c", 0).is_some());
        assert_eq!(c.stats(), (2, 2));
    }

    #[test]
    fn like_superset_picks_longest_proper_substring() {
        let mut c = MaskCache::new(1024);
        let lk = |n: &str| Some(LikeKey { col: 2, needle: n.into() });
        c.put("co", 0, 1, bits(0b1111), lk("co"));
        c.put("coa", 0, 1, bits(0b0111), lk("coa"));
        c.put("xyz", 0, 1, bits(0b1000), lk("xyz"));
        c.put("other-col", 0, 1, bits(0b1111), Some(LikeKey { col: 3, needle: "coa".into() }));
        assert_eq!(*c.like_superset(2, "coal", 0).unwrap(), vec![0b0111]);
        // the equal needle is not a proper superset; the shorter `co` is
        assert_eq!(*c.like_superset(2, "coa", 0).unwrap(), vec![0b1111]);
        assert!(c.like_superset(2, "zzz", 0).is_none());
        assert!(c.like_superset(2, "coal", 1).is_none(), "group not filled");
        assert_eq!(c.narrowed, 2);
    }
}
