//! ASCII-case-insensitive substring search kernels for the LIKE fast paths.
//!
//! `find_ci` scans raw bytes for a lowercase needle: SIMD128 on wasm (safe
//! intrinsics — 16 haystack bytes per step, first+last byte prefilter, verify
//! survivors), a first-byte scalar scan elsewhere. Case folding is `| 0x20`
//! applied only where the needle byte is an ASCII letter — safe because UTF-8
//! continuation bytes are never ASCII letters, and exact equality is used for
//! non-letter needle bytes (so no `0x10 == '0'`-style false matches).

/// Does `hay` case-insensitively equal lowercase `needle_lower`?
#[inline]
pub fn eq_ci(hay: &[u8], needle_lower: &[u8]) -> bool {
    hay.len() == needle_lower.len() && verify(hay, needle_lower)
}

/// Case-insensitive `starts_with` (needle lowercase).
#[inline]
pub fn prefix_ci(hay: &[u8], needle_lower: &[u8]) -> bool {
    hay.len() >= needle_lower.len() && verify(&hay[..needle_lower.len()], needle_lower)
}

/// Case-insensitive `ends_with` (needle lowercase).
#[inline]
pub fn suffix_ci(hay: &[u8], needle_lower: &[u8]) -> bool {
    hay.len() >= needle_lower.len() && verify(&hay[hay.len() - needle_lower.len()..], needle_lower)
}

/// Case-insensitive `contains` (needle lowercase).
#[inline]
pub fn contains_ci(hay: &[u8], needle_lower: &[u8]) -> bool {
    find_ci(hay, 0, needle_lower).is_some()
}

/// `hay[i..]` equals the needle at every position? — the verify step.
#[inline]
fn verify(hay: &[u8], needle_lower: &[u8]) -> bool {
    hay.iter().zip(needle_lower).all(|(&h, &n)| {
        if n.is_ascii_lowercase() { h | 0x20 == n } else { h == n }
    })
}

/// Position of the next case-insensitive occurrence of `needle_lower` in
/// `hay` at or after `from`. Empty needles match at `from`.
pub fn find_ci(hay: &[u8], from: usize, needle_lower: &[u8]) -> Option<usize> {
    let n = needle_lower.len();
    if n == 0 {
        return (from <= hay.len()).then_some(from);
    }
    if from + n > hay.len() {
        return None;
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        find_ci_simd(hay, from, needle_lower)
    }
    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    {
        find_ci_scalar(hay, from, needle_lower)
    }
}

#[cfg_attr(all(target_arch = "wasm32", target_feature = "simd128"), allow(dead_code))]
fn find_ci_scalar(hay: &[u8], from: usize, needle_lower: &[u8]) -> Option<usize> {
    let n = needle_lower.len();
    let first = needle_lower[0];
    let fold_first = first.is_ascii_lowercase();
    let mut i = from;
    while i + n <= hay.len() {
        let h = hay[i];
        let hit = if fold_first { h | 0x20 == first } else { h == first };
        if hit && verify(&hay[i..i + n], needle_lower) {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
fn find_ci_simd(hay: &[u8], from: usize, needle_lower: &[u8]) -> Option<usize> {
    use core::arch::wasm32::*;
    let n = needle_lower.len();
    let end = hay.len() - n; // inclusive last start position
    let first = needle_lower[0];
    let last = needle_lower[n - 1];
    // fold masks: 0x20 where that needle byte is a letter, else 0
    let f_fold = if first.is_ascii_lowercase() { 0x20u8 } else { 0 };
    let l_fold = if last.is_ascii_lowercase() { 0x20u8 } else { 0 };
    let v_first = u8x16_splat(first);
    let v_last = u8x16_splat(last);
    let v_ffold = u8x16_splat(f_fold);
    let v_lfold = u8x16_splat(l_fold);

    #[inline]
    fn load16(s: &[u8]) -> v128 {
        // safe 16-byte load: LLVM lowers the array round-trip to v128.load
        let a: [u8; 16] = s[..16].try_into().unwrap();
        u8x16(
            a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12],
            a[13], a[14], a[15],
        )
    }

    let mut i = from;
    while i + 16 <= end + 1 {
        // 16 candidate positions per step, prefiltered on first+last byte
        let a = load16(&hay[i..]);
        let b = load16(&hay[i + n - 1..]);
        let eq_a = u8x16_eq(v128_or(a, v_ffold), v_first);
        let eq_b = u8x16_eq(v128_or(b, v_lfold), v_last);
        let mut mask = u8x16_bitmask(v128_and(eq_a, eq_b)) as u32;
        while mask != 0 {
            let off = mask.trailing_zeros() as usize;
            let pos = i + off;
            if verify(&hay[pos..pos + n], needle_lower) {
                return Some(pos);
            }
            mask &= mask - 1;
        }
        i += 16;
    }
    // scalar tail
    find_ci_scalar(hay, i, needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_kernels() {
        assert!(contains_ci(b"Boreal Green Energy", b"boreal"));
        assert!(contains_ci(b"BOREAL", b"boreal"));
        assert!(!contains_ci(b"Borea", b"boreal"));
        assert_eq!(find_ci(b"xxBorealxxborEALxx", 0, b"boreal"), Some(2));
        assert_eq!(find_ci(b"xxBorealxxborEALxx", 3, b"boreal"), Some(10));
        assert_eq!(find_ci(b"xxBorealxxborEALxx", 11, b"boreal"), None);
        // non-letter needle bytes use exact equality (0x10 must not match '0')
        assert!(!contains_ci(&[0x10, 0x20], b"0"));
        assert!(contains_ci(b"plant 03", b" 0"));
        assert!(prefix_ci(b"Solar Plant", b"sol"));
        assert!(suffix_ci(b"Solar Plant", b"PLANT".to_ascii_lowercase().as_slice()));
        assert!(eq_ci(b"SOLAR", b"solar"));
        assert!(!eq_ci(b"SOLARx", b"solar"));
        // utf-8 content: continuation bytes are never letters, so folding is safe
        assert!(contains_ci("Suchitepéquez".as_bytes(), b"quez"));
        assert!(!contains_ci("é".as_bytes(), b"e"));
        // empty needle matches everywhere
        assert_eq!(find_ci(b"abc", 1, b""), Some(1));
        // long haystack exercising the (native-)scalar/simd paths + tail
        let hay = "ab".repeat(100) + "NeedleX" + &"cd".repeat(100);
        assert_eq!(find_ci(hay.as_bytes(), 0, b"needlex"), Some(200));
    }
}
