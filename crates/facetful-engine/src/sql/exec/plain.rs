//! Plain projection: no ORDER BY, no aggregates. Columnar refs + per-group
//! sources; with a LIMIT the scan stops as soon as enough rows exist, and
//! each group loads only as deep as its last contributing row.

use super::*;

pub(super) struct Plain {
    /// offset + limit, when there is a limit
    scan_cap: Option<usize>,
    srcs: Vec<Option<SelSrc>>,
    refs: Vec<(u32, u32)>,
    gslot: u32,
    /// with a window: every scanned group's lanes (Rc clones), so select
    /// expressions run over the rows in the window only, at finish
    ctxs: Option<Vec<Cols>>,
}

impl Plain {
    pub(super) fn new(n_select: usize, scan_cap: Option<usize>, defer: bool) -> Plain {
        Plain {
            scan_cap,
            srcs: (0..n_select).map(|_| None).collect(),
            refs: Vec::new(),
            gslot: 0,
            ctxs: defer.then(Vec::new),
        }
    }

    /// How deep this group's projection lanes must go: just far enough to
    /// yield the rows still missing under the cap.
    pub(super) fn depth(&self, keep: Option<&[u8]>, rows: usize) -> usize {
        let Some(cap) = self.scan_cap else { return rows };
        let rem = cap.saturating_sub(self.refs.len());
        match keep {
            None => rem.min(rows),
            Some(k) => {
                let mut cnt = 0usize;
                for (i, &b) in k.iter().enumerate() {
                    if b != 0 {
                        cnt += 1;
                        if cnt == rem {
                            return i + 1;
                        }
                    }
                }
                rows
            }
        }
    }

    pub(super) fn scan_group(&mut self, sh: &Shared, ctx: &GroupCtx, keep: Option<&[u8]>) -> Flow {
        sel_srcs_for_group(sh.q, ctx, &mut self.srcs, self.ctxs.is_some());
        if let Some(c) = &mut self.ctxs {
            c.push(ctx.cols.clone());
        }
        // a plain invariant bool unswitches out of the row loops more reliably
        // than matching the Option per row (measured 0.1 ms on 183K rows)
        let (keep_all, keep_bits) = (keep.is_none(), keep.unwrap_or(&[]));
        let kept = |i: usize| keep_all || keep_bits[i] != 0;
        let gslot = self.gslot;
        self.refs.extend((0..ctx.rows).filter(|&i| kept(i)).map(|i| (gslot, i as u32)));
        self.gslot += 1;
        if let Some(c) = self.scan_cap {
            if self.refs.len() >= c {
                self.refs.truncate(c);
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    pub(super) fn finish<S: ReadAt>(self, table: &Table<S>, sh: &Shared) -> QueryResult {
        let (offset, limit) = sh.window();
        let refs: Vec<(u32, u32)> = self.refs.into_iter().skip(offset).take(limit).collect();
        let cols = project(sh.q, &sh.sel_tys, &self.srcs, &refs, self.ctxs.as_deref());
        sh.result(table, Vec::new(), Some(cols), refs.len())
    }
}
