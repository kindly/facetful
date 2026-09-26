//! The haversine kernel alone, compiled to wasm: libm trig with no engine
//! around it — the floor a native `geo_distance` could reach.

const R: f64 = 6_371_008.8;

#[no_mangle]
pub extern "C" fn alloc_f64(n: usize) -> *mut f64 {
    let mut v = vec![0f64; n];
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}

#[inline(always)]
fn dist(p2: f64, cp2: f64, lon2: f64, la: f64, lo: f64) -> f64 {
    let p1 = la.to_radians();
    let dp = ((p2 - p1) * 0.5).sin();
    let dl = ((lon2 - lo).to_radians() * 0.5).sin();
    let h = dp * dp + p1.cos() * cp2 * dl * dl;
    2.0 * R * h.sqrt().min(1.0).asin()
}

/// out[i] = distance in metres from (lat0, lon0)
#[no_mangle]
pub unsafe extern "C" fn fill(lat: *const f64, lon: *const f64, n: usize, lat0: f64, lon0: f64, out: *mut f64) {
    let (lat, lon, out) = (
        core::slice::from_raw_parts(lat, n),
        core::slice::from_raw_parts(lon, n),
        core::slice::from_raw_parts_mut(out, n),
    );
    let p2 = lat0.to_radians();
    let cp2 = p2.cos();
    for i in 0..n {
        out[i] = dist(p2, cp2, lon0, lat[i], lon[i]);
    }
}

/// rows within r metres of (lat0, lon0)
#[no_mangle]
pub unsafe extern "C" fn count_within(lat: *const f64, lon: *const f64, n: usize, lat0: f64, lon0: f64, r: f64) -> u32 {
    let (lat, lon) = (core::slice::from_raw_parts(lat, n), core::slice::from_raw_parts(lon, n));
    let p2 = lat0.to_radians();
    let cp2 = p2.cos();
    let mut c = 0u32;
    for i in 0..n {
        c += (dist(p2, cp2, lon0, lat[i], lon[i]) <= r) as u32;
    }
    c
}
