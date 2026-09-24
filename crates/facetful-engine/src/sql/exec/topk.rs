//! Bounded top-k: ORDER BY + a small LIMIT. Keep ~2·cap candidates with a
//! cheap first-key reject for most rows; scan only the ORDER BY columns;
//! after the scan, load the winners' select lanes and project just them
//! (true late materialization).

use super::*;

/// The candidates as parallel arrays — keys and (group, row) refs — so the
/// sort is the shared permutation sort over `&[Vec<Val>]`.
struct Cands {
    keys: Vec<Vec<Val>>,
    refs: Vec<(u32, u32)>,
}

impl Cands {
    fn len(&self) -> usize {
        self.keys.len()
    }
    /// Stable sort by key.
    fn sort(&mut self, order_by: &[(Bound, SortDir)]) {
        let mut perm: Vec<u32> = (0..self.keys.len() as u32).collect();
        sort_perm_keys(&mut perm, &self.keys, order_by);
        let mut keys: Vec<Option<Vec<Val>>> = self.keys.drain(..).map(Some).collect();
        let refs = std::mem::take(&mut self.refs);
        for &i in &perm {
            self.keys.push(keys[i as usize].take().unwrap());
            self.refs.push(refs[i as usize]);
        }
    }
    fn truncate(&mut self, n: usize) {
        self.keys.truncate(n);
        self.refs.truncate(n);
    }
}

pub(super) struct TopK {
    cap: usize,
    cands: Cands,
    /// full key of the current cutoff
    bound_key: Option<Vec<Val>>,
}

impl TopK {
    pub(super) fn new(cap: usize) -> TopK {
        TopK { cap, cands: Cands { keys: Vec::new(), refs: Vec::new() }, bound_key: None }
    }

    pub(super) fn scan_group(&mut self, sh: &Shared, g: usize, ctx: &GroupCtx, keep: Option<&[u8]>) {
        let q = sh.q;
        let ord_vvs: Vec<VV> = q.order_by.iter().map(|(e, _)| eval_vec(e, ctx)).collect();
        for i in 0..ctx.rows {
            if keep.is_some_and(|k| k[i] == 0) {
                continue;
            }
            // cheap reject on the first order key against the cutoff
            if let Some(bk) = &self.bound_key {
                let k0 = ord_vvs[0].val_at(i, sh.ord_tys[0]);
                let ord = k0.cmp_sql(&bk[0]);
                let ord = if q.order_by[0].1 == SortDir::Desc { ord.reverse() } else { ord };
                if ord == core::cmp::Ordering::Greater {
                    continue;
                }
            }
            let keys: Vec<Val> =
                ord_vvs.iter().zip(&sh.ord_tys).map(|(v, t)| v.val_at(i, *t)).collect();
            self.cands.keys.push(keys);
            self.cands.refs.push((g as u32, i as u32));
            if self.cands.len() >= self.cap * 2 + 16 {
                self.cands.sort(&q.order_by);
                self.cands.truncate(self.cap);
                self.bound_key = self.cands.keys.last().cloned();
            }
        }
    }

    pub(super) fn finish<S: ReadAt>(
        mut self,
        table: &mut Table<S>,
        sh: &Shared,
    ) -> Result<QueryResult, FormatError> {
        let q = sh.q;
        self.cands.sort(&q.order_by);
        let offset = q.offset.unwrap_or(0) as usize;
        let limit = q.limit.unwrap_or(0) as usize;
        // winners as (group, row) refs; keys are no longer needed
        let winners: Vec<(u32, u32)> = self.cands.refs.into_iter().skip(offset).take(limit).collect();

        // Plain-text columns selected directly are gathered per winner row
        // straight off the borrowed segments — loading the lane would copy
        // the whole blob up to the deepest winner for a handful of rows.
        let is_direct_text = |b: &Bound| -> Option<usize> {
            let Bound::Column { index, .. } = b else { return None };
            let def = &table.catalog().schema.columns[*index];
            (def.ty == ColumnType::Utf8 && !def.is_dict()).then_some(*index)
        };
        let mut direct_text: Vec<Option<usize>> =
            q.select.iter().map(|s| is_direct_text(&s.expr)).collect();
        // gathered[sel_idx][winner_idx]
        let mut gathered: HashMap<usize, Vec<Val>> = HashMap::new();
        for (si, ci) in direct_text.clone().into_iter().enumerate() {
            let Some(ci) = ci else { continue };
            let mut vals = vec![Val::Null; winners.len()];
            let mut ok = true;
            for g in winners.iter().map(|w| w.0).collect::<std::collections::BTreeSet<_>>() {
                let got = table.with_text_segments(g as usize, ci, |offs, blob, valid| {
                    for (wi, &(wg, wrow)) in winners.iter().enumerate() {
                        if wg != g {
                            continue;
                        }
                        let i = wrow as usize;
                        if valid.is_some_and(|v| v[i / 8] >> (i % 8) & 1 == 0) {
                            continue; // stays Null
                        }
                        let at = |n: usize| {
                            u32::from_le_bytes(offs[n * 4..n * 4 + 4].try_into().unwrap()) as usize
                        };
                        let s = core::str::from_utf8(&blob[at(i)..at(i + 1)])
                            .expect("image text is utf8");
                        vals[wi] = Val::Text(Rc::new(s.to_string()));
                    }
                })?;
                if got.is_none() {
                    ok = false; // segments not resident: fall back to the lane path
                    break;
                }
            }
            if ok {
                gathered.insert(si, vals);
            } else {
                direct_text[si] = None;
            }
        }
        // lanes still needed: any select expr that isn't a gathered direct text
        let mut lane_cols = Vec::new();
        for (si, s) in q.select.iter().enumerate() {
            if direct_text[si].is_none() || !gathered.contains_key(&si) {
                collect_columns(&s.expr, &mut lane_cols);
            }
        }
        lane_cols.sort_unstable();
        lane_cols.dedup();

        // per group: lanes as deep as its deepest winner, then gathered at the
        // winners' rows so expressions evaluate over those rows only (a page
        // of 50 spread over 40 groups is 50 evaluations, not 40 groups' worth)
        let mut sel_cache: HashMap<u32, (HashMap<u32, u32>, Vec<Option<VV>>)> = HashMap::new();
        let mut rows: Vec<Vec<Val>> = Vec::with_capacity(winners.len());
        for (wi, &(cg, crow)) in winners.iter().enumerate() {
            if !sel_cache.contains_key(&cg) {
                let g = cg as usize;
                let rows_g: Vec<u32> = winners.iter().filter(|w| w.0 == cg).map(|w| w.1).collect();
                let cap = rows_g.iter().map(|&r| r as usize + 1).max().unwrap();
                let mut cols: Cols = HashMap::new();
                sh.load(table, g, &mut cols, &lane_cols, cap)?;
                let ctx = gather_ctx(&cols, &rows_g);
                let sel_vvs: Vec<Option<VV>> = q
                    .select
                    .iter()
                    .enumerate()
                    .map(|(si, s)| (!gathered.contains_key(&si)).then(|| eval_vec(&s.expr, &ctx)))
                    .collect();
                let pos: HashMap<u32, u32> = rows_g.iter().enumerate().map(|(i, &r)| (r, i as u32)).collect();
                sel_cache.insert(cg, (pos, sel_vvs));
            }
            let (pos, sel) = &sel_cache[&cg];
            let at = pos[&crow] as usize;
            rows.push(
                sel.iter()
                    .zip(&sh.sel_tys)
                    .enumerate()
                    .map(|(si, (v, t))| match v {
                        Some(v) => v.val_at(at, *t),
                        None => gathered[&si][wi].clone(),
                    })
                    .collect(),
            );
        }
        let n = rows.len();
        Ok(sh.result(table, rows, None, n))
    }
}
