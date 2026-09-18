//! Full-table ORDER BY with no usable top-k bound: defer everything — keep
//! the ORDER BY lanes and kept rows per group, sort packed keys + row refs,
//! project only afterwards.

use super::*;

pub(super) struct FullSort {
    /// per scanned group: its ORDER BY lanes and the rows the WHERE kept
    groups: Vec<(Vec<VV>, Vec<u32>)>,
    srcs: Vec<Option<SelSrc>>,
}

impl FullSort {
    pub(super) fn new(n_select: usize) -> FullSort {
        FullSort { groups: Vec::new(), srcs: (0..n_select).map(|_| None).collect() }
    }

    pub(super) fn scan_group(&mut self, sh: &Shared, ctx: &GroupCtx, keep: Option<&[u8]>) {
        sel_srcs_for_group(sh.q, ctx, &mut self.srcs);
        let ord_vvs: Vec<VV> = sh.q.order_by.iter().map(|(e, _)| eval_vec(e, ctx)).collect();
        // a plain invariant bool unswitches out of the row loops more reliably
        // than matching the Option per row (measured 0.1 ms on 183K rows)
        let (keep_all, keep_bits) = (keep.is_none(), keep.unwrap_or(&[]));
        let kept = |i: usize| keep_all || keep_bits[i] != 0;
        let kept_rows: Vec<u32> = (0..ctx.rows).filter(|&i| kept(i)).map(|i| i as u32).collect();
        self.groups.push((ord_vvs, kept_rows));
    }

    pub(super) fn finish<S: ReadAt>(self, table: &Table<S>, sh: &Shared) -> QueryResult {
        let q = sh.q;
        let groups = &self.groups;
        // flatten refs: (group slot, row)
        let total: usize = groups.iter().map(|(_, k)| k.len()).sum();
        let mut refs: Vec<(u32, u32)> = Vec::with_capacity(total);
        for (gslot, (_, kept_rows)) in groups.iter().enumerate() {
            for &r in kept_rows {
                refs.push((gslot as u32, r));
            }
        }
        let numeric_keys = sh
            .ord_tys
            .iter()
            .all(|t| matches!(t, Ty::Int | Ty::Float | Ty::Date | Ty::Timestamp));
        let nk = q.order_by.len();

        let order_refs: Vec<(u32, u32)> = if numeric_keys && nk == 1 {
            let (_, dir) = &q.order_by[0];
            let desc = *dir == SortDir::Desc;
            let ty = sh.ord_tys[0];
            // payload = index into refs, keeping the element at 16 bytes
            let mut keyed: Vec<(u8, u64, u32)> = refs
                .iter()
                .enumerate()
                .map(|(i, &(g, r))| {
                    let (v, k) = encode_order(&groups[g as usize].0[0], r as usize, ty, desc);
                    (v, k, i as u32)
                })
                .collect();
            sort_keyed2(&mut keyed);
            keyed.into_iter().map(|t| refs[t.2 as usize]).collect()
        } else if numeric_keys {
            let mut flat: Vec<(u8, u64)> = Vec::with_capacity(refs.len() * nk);
            for &(g, r) in &refs {
                for (ki, (_, dir)) in q.order_by.iter().enumerate() {
                    flat.push(encode_order(
                        &groups[g as usize].0[ki],
                        r as usize,
                        sh.ord_tys[ki],
                        *dir == SortDir::Desc,
                    ));
                }
            }
            let mut perm: Vec<u32> = (0..refs.len() as u32).collect();
            sort_perm_packed(&mut perm, &flat, nk);
            perm.into_iter().map(|p| refs[p as usize]).collect()
        } else {
            // text keys: materialize the (small) key tuples, sort refs by them
            let keys: Vec<Vec<Val>> = refs
                .iter()
                .map(|&(g, r)| {
                    groups[g as usize]
                        .0
                        .iter()
                        .zip(&sh.ord_tys)
                        .map(|(v, t)| v.val_at(r as usize, *t))
                        .collect()
                })
                .collect();
            let mut perm: Vec<u32> = (0..refs.len() as u32).collect();
            sort_perm_keys(&mut perm, &keys, &q.order_by);
            perm.into_iter().map(|p| refs[p as usize]).collect()
        };

        let (offset, limit) = sh.window();
        let final_refs: Vec<(u32, u32)> = order_refs.into_iter().skip(offset).take(limit).collect();
        let cols: Vec<OutCol> = self
            .srcs
            .iter()
            .zip(&sh.sel_tys)
            .map(|(src, ty)| match src {
                Some(src) => gather_outcol(src, &final_refs, *ty),
                None => gather_outcol(&SelSrc::Vv(Vec::new()), &[], *ty),
            })
            .collect();
        sh.result(table, Vec::new(), Some(cols), final_refs.len())
    }
}
