//! GROUP BY and aggregates: assign each kept row a group id, feed the
//! accumulators a batch at a time, then run the output phase as a
//! vectorized pass over the group table.

use super::*;

/// How rows become group ids.
enum Grouping {
    /// no GROUP BY: one group, aggregated straight off the WHERE mask
    Ungrouped,
    /// dict/int keys packed into one code: dense lane or GroupMap
    Packed { plan: PackedGroups, codes: Vec<u64> },
    /// plain-text keys hashed off the blob into an arena
    Text(TextGroups),
    /// expression keys: materialized tuples
    Expr { map: HashMap<Vec<Val>, usize>, keys: Vec<Vec<Val>> },
}

pub(super) struct Aggregate {
    /// the distinct aggregate calls, in first-seen order (SELECT then ORDER BY)
    calls: Vec<Bound>,
    accs: Vec<AggAcc>,
    grouping: Grouping,
    n_groups: usize,
    /// count(*)-only: the count fuses into the grouping loop, no gids pass
    count_only: bool,
}

impl Aggregate {
    pub(super) fn plan<S: ReadAt>(table: &mut Table<S>, sh: &Shared) -> Result<Aggregate, FormatError> {
        let q = sh.q;
        let mut calls = Vec::new();
        q.select.iter().for_each(|s| collect_aggs(&s.expr, &mut calls));
        q.order_by.iter().for_each(|(e, _)| collect_aggs(e, &mut calls));
        let arg_dict_len = |call: &Bound| -> Option<usize> {
            let Bound::Call { args, .. } = call else { return None };
            let Bound::Column { index, .. } = &args[0] else { return None };
            sh.dicts.get(index).map(|dict| dict.len())
        };
        let accs: Vec<AggAcc> = calls.iter().map(|c| AggAcc::new(c, arg_dict_len(c))).collect();
        let count_only = accs.iter().all(|a| matches!(a, AggAcc::Count(_)))
            && calls.iter().all(|c| {
                let Bound::Call { args, .. } = c else { return false };
                matches!(args[0], Bound::Number(..) | Bound::Str(_))
            });
        let grouping = if q.group_by.is_empty() {
            Grouping::Ungrouped
        } else if let Some(plan) = packed_plan(table, &q.group_by) {
            Grouping::Packed { plan, codes: Vec::new() }
        } else if let Some(tg) = text_plan(table, &q.group_by) {
            Grouping::Text(tg)
        } else {
            Grouping::Expr { map: HashMap::new(), keys: Vec::new() }
        };
        Ok(Aggregate { calls, accs, grouping, n_groups: 0, count_only })
    }

    pub(super) fn scan_group(&mut self, sh: &Shared, ctx: &GroupCtx, keep: Option<&[u8]>) {
        let Aggregate { calls, accs, grouping, n_groups, count_only } = self;
        let count_only = *count_only;
        let rows = ctx.rows;
        // a plain invariant bool unswitches out of the row loops more reliably
        // than matching the Option per row (measured 0.1 ms on 183K rows)
        let (keep_all, keep_bits) = (keep.is_none(), keep.unwrap_or(&[]));
        let kept = |i: usize| keep_all || keep_bits[i] != 0;

        let gids: Option<Vec<u32>> = match grouping {
            Grouping::Ungrouped => {
                // one group, no gids vector — aggregate off the mask
                if *n_groups == 0 {
                    *n_groups = 1;
                    for a in accs.iter_mut() {
                        a.grow(1);
                    }
                }
                None
            }
            Grouping::Packed { plan: d, codes: group_codes } => {
                let code_cols: Vec<(VV, usize)> =
                    d.cols.iter().zip(&d.cards).map(|(&c, &card)| (ctx.column(c), card)).collect();
                // hoist raw lanes out of the row loop
                let fast = fast_dims(&code_cols, &d.dims);
                let mut gids = if count_only { Vec::new() } else { vec![u32::MAX; rows] };
                // fused counting goes through a plain local buffer (merged
                // below) — touching the accumulator enum per row is slower
                // than the gids pass it replaces
                let mut local_counts: Vec<i64> = if count_only { vec![0; *n_groups] } else { Vec::new() };
                // one loop body, specialized per lookup: testing the Option
                // inside the row loop measurably slowed the wasm build
                macro_rules! group_rows {
                    (|$composite:ident| $lookup:expr) => {
                        for i in 0..rows {
                            if !kept(i) {
                                continue;
                            }
                            let $composite = composite_of(&fast, i);
                            let gid: u32 = $lookup;
                            if gid as usize == *n_groups {
                                group_codes.push($composite);
                                *n_groups += 1;
                                if count_only {
                                    local_counts.push(0);
                                }
                            }
                            if count_only {
                                local_counts[gid as usize] += 1;
                            } else {
                                gids[i] = gid;
                            }
                        }
                    };
                }
                match &mut d.dense {
                    Some(dense) => group_rows!(|composite| {
                        let slot = &mut dense[composite as usize];
                        if *slot < 0 {
                            *slot = *n_groups as i32;
                        }
                        *slot as u32
                    }),
                    None => group_rows!(|composite| d.map.get_or_insert(composite, *n_groups as u32)),
                }
                // one growth per batch, not one per group
                for a in accs.iter_mut() {
                    a.grow(*n_groups);
                }
                if count_only {
                    for a in accs.iter_mut() {
                        let AggAcc::Count(c) = a else { unreachable!("count_only") };
                        for (g, &n) in local_counts.iter().enumerate() {
                            c[g] += n;
                        }
                    }
                    return; // this group's aggregates are done
                }
                Some(gids)
            }
            Grouping::Text(tg) => {
                let code_cols: Vec<(VV, usize)> =
                    tg.pcols.iter().zip(&tg.pcards).map(|(&c, &card)| (ctx.column(c), card)).collect();
                let fast = fast_dims(&code_cols, &tg.pdims);
                // text dims: borrowed views of the blob, no strings
                struct TextLane<'a> {
                    offsets: &'a [u32],
                    bytes: &'a [u8],
                    valid: Option<&'a [u8]>,
                }
                let tlanes: Vec<TextLane> = tg
                    .dims
                    .iter()
                    .filter_map(|d| match d {
                        TextDim::Text(c) => Some(*c),
                        TextDim::Packed => None,
                    })
                    .map(|c| match &ctx.cols[&c] {
                        (GroupCol::Text { offsets, bytes, .. }, valid) => TextLane {
                            offsets,
                            bytes,
                            valid: valid.as_deref().map(|v| v.as_slice()),
                        },
                        _ => unreachable!("text plan over a plain Utf8 column"),
                    })
                    .collect();
                let mut texts: Vec<Option<&[u8]>> = vec![None; tlanes.len()];
                let mut gids = vec![u32::MAX; rows];
                for i in 0..rows {
                    if !kept(i) {
                        continue;
                    }
                    let composite = composite_of(&fast, i);
                    let mut hash = composite.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                    for (k, tl) in tlanes.iter().enumerate() {
                        let ok = tl.valid.map_or(true, |v| v[i / 8] >> (i % 8) & 1 != 0);
                        let b = ok.then(|| &tl.bytes[tl.offsets[i] as usize..tl.offsets[i + 1] as usize]);
                        // a NULL key hashes as a constant no byte string maps to
                        hash = mix64(hash ^ b.map_or(0x4E55_4C4C, hash_bytes));
                        texts[k] = b;
                    }
                    let gid = tg.get_or_insert(hash, composite, &texts, *n_groups as u32);
                    if gid as usize == *n_groups {
                        *n_groups += 1;
                    }
                    gids[i] = gid;
                }
                for a in accs.iter_mut() {
                    a.grow(*n_groups);
                }
                Some(gids)
            }
            Grouping::Expr { map, keys } => {
                let key_vvs: Vec<VV> = sh.q.group_by.iter().map(|e| eval_vec(e, ctx)).collect();
                let mut gids = vec![u32::MAX; rows];
                for i in 0..rows {
                    if !kept(i) {
                        continue;
                    }
                    let key: Vec<Val> = key_vvs.iter().map(|v| lane_val(v, i)).collect();
                    let next = *n_groups;
                    let gid = *map.entry(key).or_insert_with_key(|k| {
                        keys.push(k.clone());
                        next
                    });
                    if gid == next {
                        *n_groups += 1;
                    }
                    gids[i] = gid as u32;
                }
                for a in accs.iter_mut() {
                    a.grow(*n_groups);
                }
                Some(gids)
            }
        };

        let src = match &gids {
            Some(g) => RowsSrc::Gids(g),
            None => RowsSrc::Mask { keep, n: rows },
        };
        for (acc, call) in accs.iter_mut().zip(calls.iter()) {
            let Bound::Call { args, .. } = call else { unreachable!() };
            let arg = eval_vec(&args[0], ctx);
            acc.update_batch(src, &arg);
        }
    }

    /// The output phase is a vectorized pass over the GROUP TABLE — one row
    /// per group, aggregates and keys as columns under synthetic indices,
    /// select/order expressions rewritten onto them and run by the same
    /// kernels as the scan — not a per-group tree walk. Keys of a packed plan
    /// land as code lanes: no string is touched until the surviving rows are
    /// gathered.
    pub(super) fn finish<S: ReadAt>(mut self, table: &mut Table<S>, sh: &Shared) -> Result<QueryResult, FormatError> {
        let q = sh.q;
        if q.group_by.is_empty() && self.n_groups == 0 {
            self.n_groups = 1;
            for a in &mut self.accs {
                a.grow(1);
            }
        }
        let n_groups = self.n_groups;
        let base = table.catalog().schema.columns.len();
        let mut gcols: Cols = HashMap::new();
        for (i, (acc, call)) in self.accs.iter().zip(&self.calls).enumerate() {
            let vv = acc.finish_lane(n_groups, call.ty());
            gcols.insert(base + i, (GroupCol::Ready(vv), None));
        }
        let key_base = base + self.calls.len();
        match &self.grouping {
            Grouping::Ungrouped => {}
            Grouping::Packed { plan, codes } => {
                let lanes = packed_key_lanes(codes, &plan.cols, &plan.dims, &plan.cards, &sh.dicts);
                for (k, lane) in lanes.into_iter().enumerate() {
                    gcols.insert(key_base + k, lane);
                }
            }
            Grouping::Text(tg) => {
                let mut packed =
                    packed_key_lanes(&tg.codes, &tg.pcols, &tg.pdims, &tg.pcards, &sh.dicts).into_iter();
                let nt = tg.n_text();
                let mut tk = 0;
                for (k, dim) in tg.dims.iter().enumerate() {
                    let lane = match dim {
                        TextDim::Packed => packed.next().unwrap(),
                        TextDim::Text(_) => {
                            // the arena, compacted per text dim: a raw text column
                            let mut offsets = Vec::with_capacity(n_groups + 1);
                            offsets.push(0u32);
                            let mut bytes = Vec::new();
                            let mut valid = vec![0u8; n_groups.div_ceil(8)];
                            for g in 0..n_groups {
                                let (start, len) = tg.spans[g * nt + tk];
                                if len != u32::MAX {
                                    bytes.extend_from_slice(&tg.arena[start as usize..(start + len) as usize]);
                                    valid[g / 8] |= 1 << (g % 8);
                                }
                                offsets.push(bytes.len() as u32);
                            }
                            tk += 1;
                            (
                                GroupCol::Text {
                                    strs: std::cell::OnceCell::new(),
                                    offsets: Rc::new(offsets),
                                    bytes: Rc::new(bytes),
                                },
                                Some(Rc::new(valid)),
                            )
                        }
                    };
                    gcols.insert(key_base + k, lane);
                }
            }
            Grouping::Expr { keys, .. } => {
                for (k, g_expr) in q.group_by.iter().enumerate() {
                    let vv = lanes_to_vv(n_groups, g_expr.ty(), &|g| keys[g][k].clone());
                    gcols.insert(key_base + k, (GroupCol::Ready(vv), None));
                }
            }
        }
        let gctx = GroupCtx { cols: gcols, rows: n_groups };
        let rewrite = |b: &Bound| substitute_grouped(b, &q.group_by, &self.calls, base, key_base);

        // ORDER BY over every group; numeric keys pack to u64 and sort as
        // integers, anything else compares through cmp_sql. Ties keep group
        // discovery order.
        let order: Vec<u32> = if q.order_by.is_empty() {
            (0..n_groups as u32).collect()
        } else {
            let ord_vvs: Vec<VV> = q.order_by.iter().map(|(e, _)| eval_vec(&rewrite(e), &gctx)).collect();
            let nk = ord_vvs.len();
            let numeric = sh
                .ord_tys
                .iter()
                .all(|t| matches!(t, Ty::Int | Ty::Float | Ty::Date | Ty::Timestamp));
            let mut perm: Vec<u32> = (0..n_groups as u32).collect();
            if numeric && nk == 1 {
                let (_, dir) = &q.order_by[0];
                let desc = *dir == SortDir::Desc;
                // gid as the tiebreak: ties keep discovery order and the
                // sort can be unstable
                let mut keyed: Vec<(u8, u64, u32)> = (0..n_groups)
                    .map(|g| {
                        let (v, k) = encode_order(&ord_vvs[0], g, sh.ord_tys[0], desc);
                        (v, k, g as u32)
                    })
                    .collect();
                sort_keyed3(&mut keyed);
                perm = keyed.into_iter().map(|t| t.2).collect();
            } else if numeric {
                // the gid rides along as one more key: ties keep discovery
                // order, through the same sort as the row path
                let mut flat: Vec<(u8, u64)> = Vec::with_capacity(n_groups * (nk + 1));
                for g in 0..n_groups {
                    for (ki, (_, dir)) in q.order_by.iter().enumerate() {
                        flat.push(encode_order(&ord_vvs[ki], g, sh.ord_tys[ki], *dir == SortDir::Desc));
                    }
                    flat.push((0, g as u64));
                }
                sort_perm_packed(&mut perm, &flat, nk + 1);
            } else {
                let keys: Vec<Vec<Val>> = (0..n_groups)
                    .map(|g| ord_vvs.iter().zip(&sh.ord_tys).map(|(v, t)| v.val_at(g, *t)).collect())
                    .collect();
                sort_perm_keys(&mut perm, &keys, &q.order_by);
            }
            perm
        };
        // project only the survivors, straight into the columnar channel
        let (offset, limit) = sh.window();
        let refs: Vec<(u32, u32)> = order.into_iter().skip(offset).take(limit).map(|g| (0, g)).collect();
        let cols: Vec<OutCol> = q
            .select
            .iter()
            .zip(&sh.sel_tys)
            .map(|(sel, ty)| {
                let expr = rewrite(&sel.expr);
                // a text key selected as-is gathers off its raw lane: no
                // Rc<String> is ever built for it
                if let Bound::Column { index, ty: Ty::Text } = &expr {
                    match gctx.cols.get(index) {
                        Some((GroupCol::Text { offsets, bytes, .. }, valid)) => {
                            let src = SelSrc::RawText(vec![(offsets.clone(), bytes.clone(), valid.clone())]);
                            return gather_outcol(&src, &refs, *ty);
                        }
                        // a dictionary key stays codes: the group table already holds them
                        Some((GroupCol::Dict { codes, dict }, valid)) => {
                            let src = SelSrc::Dict { groups: vec![(codes.clone(), valid.clone())], dict: dict.clone() };
                            return gather_outcol(&src, &refs, *ty);
                        }
                        _ => {}
                    }
                }
                gather_outcol(&SelSrc::Vv(vec![eval_vec(&expr, &gctx)]), &refs, *ty)
            })
            .collect();
        let n = refs.len();
        Ok(sh.result(table, Vec::new(), Some(cols), n))
    }
}
