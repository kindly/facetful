//! Retained row sets: the rows a plain SELECT keeps, in its order, as global
//! row ids (a u32 per row), so a grid can ask for any block of them without
//! the whole result being built. Pages gather only their rows; a new ORDER
//! BY re-sorts the ids reading only the key columns. A grouped or joined
//! result is not rows of the table: it is kept whole and sliced
//! (`slice_result`).

use super::*;

pub struct RowSet {
    /// the kept rows in order; None: every row, in table order
    ids: Option<Vec<u32>>,
    /// how many rows that is (before the query's LIMIT/OFFSET)
    total: usize,
}

/// The first global row id of each row group, then the table's row count.
fn group_starts<S: ReadAt>(table: &Table<S>) -> Vec<u32> {
    let mut starts = Vec::with_capacity(table.group_count() + 1);
    let mut acc = 0u32;
    for g in 0..table.group_count() {
        starts.push(acc);
        acc += table.group_rows(g) as u32;
    }
    starts.push(acc);
    starts
}

impl RowSet {
    /// The rows `q` (not an aggregate) keeps, in its ORDER BY; ties, and a
    /// query without one, in table order. With `seed`, the rows are the
    /// seed's and the WHERE is not evaluated again: the same query under a
    /// new ORDER BY.
    pub fn build<S: ReadAt>(table: &mut Table<S>, q: &BoundQuery, seed: Option<&RowSet>) -> Result<RowSet, FormatError> {
        let folded = fold_query(q, table.catalog());
        let q = folded.as_ref();
        let sh = Shared::new(table, q)?;
        let starts = group_starts(table);
        let all_rows = *starts.last().unwrap() as usize;
        let sorting = !q.order_by.is_empty();
        // the seed's rows as one byte per row of each group
        let seeded: Option<Vec<Option<Vec<u8>>>> = seed.and_then(|s| s.ids.as_ref()).map(|ids| {
            let mut masks: Vec<Option<Vec<u8>>> = (0..table.group_count()).map(|_| None).collect();
            for &id in ids {
                let g = starts.partition_point(|&s| s <= id) - 1;
                masks[g].get_or_insert_with(|| vec![0u8; table.group_rows(g)])[(id - starts[g]) as usize] = 1;
            }
            masks
        });
        let filtering = seed.is_none() && q.filter.is_some();
        if !sorting && !filtering && seeded.is_none() {
            return Ok(RowSet { ids: None, total: all_rows });
        }
        let mut groups: Vec<(Vec<VV>, Vec<u32>)> = Vec::new();
        let mut gids: Vec<usize> = Vec::new();
        for g in 0..table.group_count() {
            let rows = table.group_rows(g);
            let mut cols: Cols = HashMap::default();
            let keep: Option<Vec<u8>> = match &seeded {
                Some(masks) => match &masks[g] {
                    Some(k) => Some(k.clone()),
                    None => continue,
                },
                None if seed.is_some() => None,
                None => {
                    if group_prunable(table, g, &sh.constraints) {
                        continue;
                    }
                    let k = where_mask(table, g, rows, &sh.conjuncts, &sh.dicts, &mut cols, &sh.search)?;
                    // (an empty mask means nothing kept: see where_mask)
                    if k.as_ref().is_some_and(|k| k.iter().all(|&b| b == 0)) {
                        continue;
                    }
                    k
                }
            };
            let kept_rows: Vec<u32> = match &keep {
                Some(k) => (0..rows as u32).filter(|&i| k[i as usize] != 0).collect(),
                None => (0..rows as u32).collect(),
            };
            // the group whole, so search_rank() lines up with the word index
            let ord_vvs: Vec<VV> = if sorting {
                sh.load(table, g, &mut cols, &sh.ord_needed, rows)?;
                let ctx = GroupCtx { cols, rows, search: sh.search.in_group(g, keep.as_deref()) };
                q.order_by.iter().map(|(e, _)| eval_vec(e, &ctx)).collect()
            } else {
                Vec::new()
            };
            groups.push((ord_vvs, kept_rows));
            gids.push(g);
        }
        let ids: Vec<u32> = if sorting {
            sort::sort_refs(&groups, &q.order_by, &sh.ord_tys, true)
                .into_iter()
                .map(|(gs, r)| starts[gids[gs as usize]] + r)
                .collect()
        } else {
            let mut ids = Vec::with_capacity(groups.iter().map(|g| g.1.len()).sum());
            for (gs, (_, kept)) in groups.iter().enumerate() {
                ids.extend(kept.iter().map(|&r| starts[gids[gs]] + r));
            }
            ids
        };
        Ok(RowSet { total: ids.len(), ids: Some(ids) })
    }

    /// Rows the query gives: the kept rows inside its LIMIT/OFFSET.
    pub fn len(&self, q: &BoundQuery) -> usize {
        let offset = q.offset.unwrap_or(0) as usize;
        let limit = q.limit.map_or(usize::MAX, |l| l as usize);
        self.total.saturating_sub(offset).min(limit)
    }

    /// Whether row ids are held (false: every row, in table order).
    pub fn has_ids(&self) -> bool {
        self.ids.is_some()
    }

    /// Rows `start..end` of the query's result (clamped to it), with the
    /// select items `which` (indices into `q.select`) in that order: the
    /// same values the query itself gives for those rows. Only these rows
    /// are gathered, and expressions evaluate over them alone.
    pub fn page<S: ReadAt>(
        &self,
        table: &mut Table<S>,
        q: &BoundQuery,
        which: &[usize],
        start: usize,
        end: usize,
    ) -> Result<QueryResult, FormatError> {
        let len = self.len(q);
        let (start, end) = (start.min(len), end.min(len));
        let offset = q.offset.unwrap_or(0) as usize;
        let (lo, hi) = (offset + start, offset + end.max(start));
        let page_q = BoundQuery {
            select: which.iter().map(|&i| q.select[i].clone()).collect(),
            filter: None,
            group_by: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            is_aggregate: false,
        };
        let folded = fold_query(&page_q, table.catalog());
        let pq = folded.as_ref();
        let mut sh = Shared::new(table, pq)?;
        let starts = group_starts(table);

        // refs as (group slot, row); slots in order of first appearance
        let mut slot_of: Vec<u32> = vec![u32::MAX; table.group_count()];
        let mut groups: Vec<usize> = Vec::new();
        let mut rows_of: Vec<Vec<u32>> = Vec::new();
        let mut refs: Vec<(u32, u32)> = Vec::with_capacity(hi - lo);
        // the same rows as (slot, place among that slot's rows)
        let mut local: Vec<(u32, u32)> = Vec::with_capacity(hi - lo);
        for i in lo..hi {
            let id = match &self.ids {
                Some(ids) => ids[i],
                None => i as u32,
            };
            let g = starts.partition_point(|&s| s <= id) - 1;
            if slot_of[g] == u32::MAX {
                slot_of[g] = groups.len() as u32;
                groups.push(g);
                rows_of.push(Vec::new());
            }
            let (slot, row) = (slot_of[g], id - starts[g]);
            refs.push((slot, row));
            local.push((slot, rows_of[slot as usize].len() as u32));
            rows_of[slot as usize].push(row);
        }

        // per group: lanes as deep as its deepest row, gathered at the
        // page's rows at once (the lanes are dropped before the next group)
        let mut srcs: Vec<Option<SelSrc>> = (0..pq.select.len()).map(|_| None).collect();
        for (slot, &g) in groups.iter().enumerate() {
            let rows = &rows_of[slot];
            let cap = rows.iter().map(|&r| r as usize + 1).max().unwrap();
            let mut cols: Cols = HashMap::default();
            sh.load(table, g, &mut cols, &sh.proj_needed, cap)?;
            let sub = gather_ctx(&cols, rows, &sh.search);
            sel_srcs_for_group(pq, &sub, &mut srcs, false, &sh.direct_text);
        }
        let mut out = Vec::with_capacity(srcs.len());
        for (src, ty) in srcs.iter().zip(&sh.sel_tys) {
            out.push(match src {
                Some(SelSrc::Segments(col)) => gather_segments(table, *col, &refs, &groups)?,
                Some(src) => gather_outcol(src, &local, *ty),
                None => gather_outcol(&SelSrc::Vv(Vec::new()), &[], *ty), // an empty page
            });
        }
        sh.scanned_groups = groups.len();
        Ok(sh.result(table, Vec::new(), Some(out), refs.len()))
    }
}

/// Bits `start..start + n` of a validity bitmap, as a bitmap of their own.
fn slice_bits(valid: &[u8], start: usize, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n.div_ceil(8)];
    for i in 0..n {
        let s = start + i;
        if valid[s / 8] >> (s % 8) & 1 != 0 {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

/// Rows `start..end` (clamped) of a kept result, columns `which` in that
/// order: how a grouped or joined result is paged.
pub fn slice_result(r: &QueryResult, which: &[usize], start: usize, end: usize) -> QueryResult {
    let total = if r.cols.is_some() { r.out_rows } else { r.rows.len() };
    let start = start.min(total);
    let end = end.clamp(start, total);
    let n = end - start;
    let mut out = QueryResult {
        columns: which.iter().map(|&i| r.columns[i].clone()).collect(),
        col_types: which.iter().map(|&i| r.col_types[i]).collect(),
        rows: Vec::new(),
        cols: None,
        out_rows: n,
        scanned_groups: 0,
        total_groups: r.total_groups,
    };
    match &r.cols {
        Some(cols) => {
            out.cols = Some(
                which
                    .iter()
                    .map(|&i| match &cols[i] {
                        OutCol::I64 { v, valid } => OutCol::I64 { v: v[start..end].to_vec(), valid: slice_bits(valid, start, n) },
                        OutCol::F64 { v, valid } => OutCol::F64 { v: v[start..end].to_vec(), valid: slice_bits(valid, start, n) },
                        OutCol::Bool { v, valid } => OutCol::Bool { v: v[start..end].to_vec(), valid: slice_bits(valid, start, n) },
                        OutCol::Text { offsets, bytes, valid } => {
                            let base = offsets[start];
                            OutCol::Text {
                                offsets: offsets[start..=end].iter().map(|o| o - base).collect(),
                                bytes: bytes[base as usize..offsets[end] as usize].to_vec(),
                                valid: slice_bits(valid, start, n),
                            }
                        }
                        OutCol::Dict { codes, dict, valid } => {
                            OutCol::Dict { codes: codes[start..end].to_vec(), dict: dict.clone(), valid: slice_bits(valid, start, n) }
                        }
                    })
                    .collect(),
            );
        }
        None => {
            out.rows = r.rows[start..end].iter().map(|row| which.iter().map(|&i| row[i].clone()).collect()).collect();
        }
    }
    out
}
