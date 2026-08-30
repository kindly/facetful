// Plain-JS mirror of the wasm kernels, over typed arrays.
// Deliberately the same algorithm — this is a language comparison, not an algorithm one.

/**
 * @param {number} nRows
 * @param {Uint16Array[]} codes    one array per dim
 * @param {Uint32Array[]} counts   one array per dim, length = cardinality (zeroed here)
 * @param {Int32Array} selected    -1 = no filter on that dim
 * @param {Float64Array} measure
 * @param {Uint8Array} outMask
 * @returns {{passCount: number, sum: number}}
 */
export function facetRefresh(nRows, codes, counts, selected, measure, outMask) {
  const d = codes.length;
  for (let k = 0; k < d; k++) counts[k].fill(0);
  let passCount = 0;
  let sum = 0;
  for (let row = 0; row < nRows; row++) {
    let fails = 0;
    let failDim = -1;
    for (let k = 0; k < d; k++) {
      const s = selected[k];
      if (s >= 0 && codes[k][row] !== s) {
        fails++;
        if (fails === 2) break;
        failDim = k;
      }
    }
    if (fails === 0) {
      outMask[row] = 1;
      passCount++;
      sum += measure[row];
      for (let k = 0; k < d; k++) counts[k][codes[k][row]]++;
    } else {
      outMask[row] = 0;
      if (fails === 1) counts[failDim][codes[failDim][row]]++;
    }
  }
  return { passCount, sum };
}

/**
 * Top-k row indices by measure (desc) among mask-set rows.
 * @returns {Uint32Array} written indices (length <= k)
 */
export function sortTopk(values, mask, nRows, k) {
  const idx = [];
  for (let row = 0; row < nRows; row++) if (mask[row]) idx.push(row);
  idx.sort((a, b) => values[b] - values[a]);
  const m = Math.min(idx.length, k);
  return Uint32Array.from(idx.slice(0, m));
}

// ---- synthetic data (seeded, identical across runs and languages) ----

export function makeRng(seed) {
  let s = seed >>> 0;
  return () => {
    // mulberry32
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export const DIMS = [
  { name: "country", card: 200 },
  { name: "status", card: 6 },
  { name: "fuel", card: 12 },
  { name: "region", card: 8 },
  { name: "owner", card: 2000 },
  { name: "year", card: 30 },
];

export function makeDataset(nRows, seed = 42) {
  const rng = makeRng(seed);
  const codes = DIMS.map(({ card }) => {
    const a = new Uint16Array(nRows);
    for (let i = 0; i < nRows; i++) {
      // skewed (zipf-ish): squaring the uniform biases toward low codes,
      // like real facet data where a few values dominate
      a[i] = Math.min(card - 1, Math.floor(rng() * rng() * card));
    }
    return a;
  });
  const measure = new Float64Array(nRows);
  for (let i = 0; i < nRows; i++) {
    measure[i] = Math.exp(rng() * 8) / 10; // lognormal-ish capacities
  }
  return { codes, measure };
}

/** Scripted interaction sequence: toggle one filter per step, like a user clicking facets. */
export function makeInteractions(steps, seed = 7) {
  const rng = makeRng(seed);
  const selected = new Int32Array(DIMS.length).fill(-1);
  const script = [];
  for (let i = 0; i < steps; i++) {
    const dim = Math.floor(rng() * DIMS.length);
    if (selected[dim] >= 0 && rng() < 0.35) {
      selected[dim] = -1; // clear this facet
    } else {
      selected[dim] = Math.floor(rng() * rng() * DIMS[dim].card);
    }
    script.push(Int32Array.from(selected));
  }
  return script;
}

// ---- scan kernels ----

export function maskEqU16(codes, n, target, outMask) {
  let cnt = 0;
  for (let i = 0; i < n; i++) {
    const hit = codes[i] === target ? 1 : 0;
    outMask[i] = hit;
    cnt += hit;
  }
  return cnt;
}

export function sumF64(values, n) {
  let s = 0;
  for (let i = 0; i < n; i++) s += values[i];
  return s;
}

// ---- hash GROUP BY (a, b) -> sum(values), count — two JS styles ----

/** Idiomatic JS: Map from composite key to group index. */
export function groupAggMap(a, b, mask, values, n) {
  const idx = new Map();
  const sums = [];
  const counts = [];
  for (let i = 0; i < n; i++) {
    if (!mask[i]) continue;
    const key = (a[i] << 16) | b[i];
    let g = idx.get(key);
    if (g === undefined) {
      g = sums.length;
      idx.set(key, g);
      sums.push(0);
      counts.push(0);
    }
    sums[g] += values[i];
    counts[g]++;
  }
  return { idx, sums, counts };
}

/** Best-case JS: hand-rolled open addressing over typed arrays (same algorithm as the Rust). */
export function groupAggHash(a, b, mask, values, n, cap, slotKeys, slotSums, slotCounts) {
  slotKeys.fill(0xffffffff);
  slotSums.fill(0);
  slotCounts.fill(0);
  const capm = cap - 1;
  let groups = 0;
  for (let i = 0; i < n; i++) {
    if (!mask[i]) continue;
    const key = ((a[i] << 16) | b[i]) >>> 0;
    let x = Math.imul(key, 2654435761) >>> 0;
    x = (x ^ (x >>> 15)) >>> 0; // spread high bits down before masking
    let h = x & capm;
    for (;;) {
      const k = slotKeys[h];
      if (k === key) break;
      if (k === 0xffffffff) {
        slotKeys[h] = key;
        groups++;
        break;
      }
      h = (h + 1) & capm;
    }
    slotSums[h] += values[i];
    slotCounts[h]++;
  }
  return groups;
}
