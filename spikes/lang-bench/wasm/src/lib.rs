// Kernel micro-benchmark: the two operations the facet workload lives on,
// implemented the way the real engine would run them (data resident in wasm
// memory, one boundary call per user interaction).

use std::mem;
use std::slice;

#[no_mangle]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(n);
    let p = v.as_mut_ptr();
    mem::forget(v);
    p
}

/// One facet-interface refresh with correct filters-except-own semantics.
///
/// For every dimension d: count per code, applying all filters except d's own.
/// A row contributes to dim d iff it passes every other dim's filter, i.e.
/// fail_count == 0 (passes everything), or fail_count == 1 with the failure on d.
/// Rows with fail_count == 0 also feed the totals and the output mask.
///
/// codes_ptrs / counts_ptrs are arrays of wasm pointers (u32 on wasm32).
#[no_mangle]
pub extern "C" fn facet_refresh(
    n_rows: u32,
    n_dims: u32,
    codes_ptrs: *const u32,
    cards: *const u32,
    selected: *const i32,
    measure: *const f64,
    counts_ptrs: *const u32,
    out_mask: *mut u8,
    out_totals: *mut f64, // [pass_count, sum(measure over passing rows)]
) {
    let n = n_rows as usize;
    let d = n_dims as usize;
    unsafe {
        let codes_ptrs = slice::from_raw_parts(codes_ptrs, d);
        let cards = slice::from_raw_parts(cards, d);
        let sels = slice::from_raw_parts(selected, d);
        let measure = slice::from_raw_parts(measure, n);
        let counts_ptrs = slice::from_raw_parts(counts_ptrs, d);
        let mask = slice::from_raw_parts_mut(out_mask, n);

        let mut codes: Vec<&[u16]> = Vec::with_capacity(d);
        let mut counts: Vec<&mut [u32]> = Vec::with_capacity(d);
        for k in 0..d {
            codes.push(slice::from_raw_parts(codes_ptrs[k] as *const u16, n));
            let c = slice::from_raw_parts_mut(counts_ptrs[k] as *mut u32, cards[k] as usize);
            c.fill(0);
            counts.push(c);
        }

        let mut pass_count: u64 = 0;
        let mut sum = 0f64;
        for row in 0..n {
            let mut fails = 0u32;
            let mut fail_dim = usize::MAX;
            for k in 0..d {
                let s = sels[k];
                if s >= 0 && codes[k][row] as i32 != s {
                    fails += 1;
                    if fails == 2 {
                        break;
                    }
                    fail_dim = k;
                }
            }
            if fails == 0 {
                mask[row] = 1;
                pass_count += 1;
                sum += measure[row];
                for k in 0..d {
                    counts[k][codes[k][row] as usize] += 1;
                }
            } else {
                mask[row] = 0;
                if fails == 1 {
                    counts[fail_dim][codes[fail_dim][row] as usize] += 1;
                }
            }
        }
        *out_totals = pass_count as f64;
        *out_totals.add(1) = sum;
    }
}

/// Top-k rows by measure (desc) among rows where mask is set — the table
/// viewer's "sort by column header" over the current filter state.
/// Returns the number of indices written (<= k).
#[no_mangle]
pub extern "C" fn sort_topk(
    values: *const f64,
    mask: *const u8,
    n_rows: u32,
    k: u32,
    out_idx: *mut u32,
) -> u32 {
    let n = n_rows as usize;
    unsafe {
        let values = slice::from_raw_parts(values, n);
        let mask = slice::from_raw_parts(mask, n);
        let mut idx: Vec<u32> = Vec::with_capacity(n);
        for row in 0..n {
            if mask[row] != 0 {
                idx.push(row as u32);
            }
        }
        idx.sort_unstable_by(|&a, &b| {
            values[b as usize].total_cmp(&values[a as usize])
        });
        let m = idx.len().min(k as usize);
        let out = slice::from_raw_parts_mut(out_idx, m);
        out.copy_from_slice(&idx[..m]);
        m as u32
    }
}

/// Single-predicate scan: mask = (codes == target), returns hit count.
#[no_mangle]
pub extern "C" fn mask_eq_u16(codes: *const u16, n: u32, target: u16, out_mask: *mut u8) -> u32 {
    let n = n as usize;
    unsafe {
        let c = slice::from_raw_parts(codes, n);
        let m = slice::from_raw_parts_mut(out_mask, n);
        let mut cnt = 0u32;
        for i in 0..n {
            let hit = (c[i] == target) as u8;
            m[i] = hit;
            cnt += hit as u32;
        }
        cnt
    }
}

/// Full-column sum.
#[no_mangle]
pub extern "C" fn sum_f64(values: *const f64, n: u32) -> f64 {
    unsafe { slice::from_raw_parts(values, n as usize).iter().sum() }
}

/// General hash aggregation: GROUP BY (a, b) -> sum(values), count, over mask-set rows.
/// Open addressing, linear probing, capacity must be a power of two.
/// Direct-array indexing is deliberately NOT used even though this key space is
/// small — the point is to measure the hashing machinery a general GROUP BY needs.
#[no_mangle]
pub extern "C" fn group_agg(
    keys_a: *const u16,
    keys_b: *const u16,
    mask: *const u8,
    values: *const f64,
    n: u32,
    cap: u32,
    slot_keys: *mut u32,
    slot_sums: *mut f64,
    slot_counts: *mut u32,
) -> u32 {
    let n = n as usize;
    let cap = cap as usize;
    let capm = cap - 1;
    unsafe {
        let a = slice::from_raw_parts(keys_a, n);
        let b = slice::from_raw_parts(keys_b, n);
        let mask = slice::from_raw_parts(mask, n);
        let vals = slice::from_raw_parts(values, n);
        let keys = slice::from_raw_parts_mut(slot_keys, cap);
        let sums = slice::from_raw_parts_mut(slot_sums, cap);
        let counts = slice::from_raw_parts_mut(slot_counts, cap);
        keys.fill(u32::MAX);
        sums.fill(0.0);
        counts.fill(0);
        let mut groups = 0u32;
        for i in 0..n {
            if mask[i] == 0 {
                continue;
            }
            let key = ((a[i] as u32) << 16) | b[i] as u32;
            let mut x = key.wrapping_mul(2654435761);
            x ^= x >> 15; // spread high bits down before masking
            let mut h = x as usize & capm;
            loop {
                let k = keys[h];
                if k == key {
                    break;
                }
                if k == u32::MAX {
                    keys[h] = key;
                    groups += 1;
                    break;
                }
                h = (h + 1) & capm;
            }
            sums[h] += vals[i];
            counts[h] += 1;
        }
        groups
    }
}

// ---- SIMD variants (present only in the +simd128 build) ----

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
mod simd {
    use core::arch::wasm32::*;

    #[no_mangle]
    pub extern "C" fn mask_eq_u16_simd(
        codes: *const u16,
        n: u32,
        target: u16,
        out_mask: *mut u8,
    ) -> u32 {
        unsafe {
            let n = n as usize;
            let splat = u16x8_splat(target);
            let mut cnt = 0u32;
            let mut i = 0usize;
            while i + 16 <= n {
                let v0 = v128_load(codes.add(i) as *const v128);
                let v1 = v128_load(codes.add(i + 8) as *const v128);
                let m0 = i16x8_eq(v0, splat);
                let m1 = i16x8_eq(v1, splat);
                // signed narrow keeps 0xFFFF as 0xFF
                let packed = i8x16_narrow_i16x8(m0, m1);
                v128_store(out_mask.add(i) as *mut v128, packed);
                cnt += (i8x16_bitmask(packed) as u32).count_ones();
                i += 16;
            }
            while i < n {
                let hit = *codes.add(i) == target;
                *out_mask.add(i) = if hit { 0xFF } else { 0 };
                cnt += hit as u32;
                i += 1;
            }
            cnt
        }
    }

    #[no_mangle]
    pub extern "C" fn sum_f64_simd(values: *const f64, n: u32) -> f64 {
        unsafe {
            let n = n as usize;
            let mut acc0 = f64x2_splat(0.0);
            let mut acc1 = f64x2_splat(0.0);
            let mut i = 0usize;
            while i + 4 <= n {
                acc0 = f64x2_add(acc0, v128_load(values.add(i) as *const v128));
                acc1 = f64x2_add(acc1, v128_load(values.add(i + 2) as *const v128));
                i += 4;
            }
            let mut s = f64x2_extract_lane::<0>(acc0)
                + f64x2_extract_lane::<1>(acc0)
                + f64x2_extract_lane::<0>(acc1)
                + f64x2_extract_lane::<1>(acc1);
            while i < n {
                s += *values.add(i);
                i += 1;
            }
            s
        }
    }
}
