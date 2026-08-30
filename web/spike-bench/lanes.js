// M1 spike lanes. Each lane exposes:
//   { name, loadMs, meta, interact(selectedValues) -> {pass, sum, counts?} }
// selectedValues: array of (string|null) per facet dim, in DIM order.
// All lanes implement CORRECT filters-except-own facet semantics + totals +
// top-50 by measure. Same work, different worlds.

export const DIM_NAMES = ["country", "status", "fuel", "region", "owner", "year"];
export const MEASURE = "capacity";
export const TOPK = 50;

// ---------- shared: seeded rng + interaction script ----------

export function mulberry32(seed) {
  let s = seed >>> 0;
  return () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Value-based interaction script (same shape across all lanes). */
export function makeScript(dicts, steps, seed = 7) {
  const rng = mulberry32(seed);
  const selected = DIM_NAMES.map(() => null);
  const script = [];
  for (let i = 0; i < steps; i++) {
    const k = Math.floor(rng() * DIM_NAMES.length);
    if (selected[k] !== null && rng() < 0.35) {
      selected[k] = null;
    } else {
      const card = dicts[k].length;
      selected[k] = dicts[k][Math.min(card - 1, Math.floor(rng() * rng() * card))];
    }
    script.push([...selected]);
  }
  return script;
}

// ---------- lane: facetful (.facetful -> wasm executor) ----------

export async function laneFacetful(wasmBytes, fileBytes) {
  const t0 = performance.now();
  const { instance } = await WebAssembly.instantiate(wasmBytes, {});
  const w = instance.exports;
  const mem = () => w.memory.buffer;

  const filePtr = w.alloc(fileBytes.byteLength);
  new Uint8Array(mem(), filePtr, fileBytes.byteLength).set(new Uint8Array(fileBytes));
  const table = w.table_open(filePtr, fileBytes.byteLength);
  if (!table) throw new Error("table_open failed");

  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const scratch = w.alloc(1024);
  const colByName = (name) => {
    const b = enc.encode(name);
    new Uint8Array(mem(), scratch, b.length).set(b);
    return w.table_col_by_name(table, scratch, b.length);
  };
  const dimCols = DIM_NAMES.map(colByName);
  const measureCol = colByName(MEASURE);

  // dictionaries (value list per dim) + value->code maps
  const dicts = [];
  const codeMaps = [];
  for (const col of dimCols) {
    const len = w.table_dict_len(table, col);
    const values = [];
    const map = new Map();
    for (let c = 0; c < len; c++) {
      const n = w.table_dict_value(table, col, c, scratch, 1024);
      const v = dec.decode(new Uint8Array(mem(), scratch, n));
      values.push(v);
      map.set(v, c);
    }
    dicts.push(values);
    codeMaps.push(map);
  }

  const nd = DIM_NAMES.length;
  const dimsPtr = w.alloc(nd * 4);
  new Uint32Array(mem(), dimsPtr, nd).set(dimCols);
  const selPtr = w.alloc(nd * 4);
  const topkPtr = w.alloc(TOPK * 4);
  const loadMs = performance.now() - t0;

  return {
    name: ".facetful → wasm engine",
    loadMs,
    dicts,
    meta: { rows: w.table_total_rows(table), fileBytes: fileBytes.byteLength },
    interact(values) {
      const sel = new Int32Array(mem(), selPtr, nd);
      for (let k = 0; k < nd; k++) {
        sel[k] = values[k] === null ? -1 : codeMaps[k].get(values[k]);
      }
      const res = w.facet_refresh(table, dimsPtr, nd, selPtr, measureCol);
      // copy out what a UI would keep: all facet counts
      const counts = [];
      for (let k = 0; k < nd; k++) {
        const p = w.result_counts_ptr(res, k);
        const l = w.result_counts_len(res, k);
        counts.push(new Uint32Array(mem(), p, l).slice());
      }
      const pass = w.result_pass(res);
      const sum = w.result_sum(res);
      const nTop = w.sort_topk(table, res, measureCol, TOPK, topkPtr);
      const top = new Uint32Array(mem(), topkPtr, nTop).slice();
      w.result_free(res);
      return { pass, sum, counts, top };
    },
  };
}

// ---------- lane: current-style JS (objects + correct N-pass facets) ----------

export function parseCsv(text) {
  // spike CSV is machine-generated: no quoted fields
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  const header = lines[0].split(",");
  const rows = new Array(lines.length - 1);
  for (let i = 1; i < lines.length; i++) {
    const parts = lines[i].split(",");
    const o = {};
    for (let c = 0; c < header.length; c++) o[header[c]] = parts[c];
    o[MEASURE] = +o[MEASURE];
    rows[i - 1] = o;
  }
  return rows;
}

export function laneJsObjects(csvText) {
  const t0 = performance.now();
  const rows = parseCsv(csvText);
  // facet value lists (insertion order = first appearance; sorted for stability)
  const dicts = DIM_NAMES.map((d) => {
    const s = new Set();
    for (const r of rows) s.add(r[d]);
    return [...s];
  });
  const loadMs = performance.now() - t0;

  return {
    name: "JS objects (current-style, correct facets)",
    loadMs,
    dicts,
    meta: { rows: rows.length },
    interact(values) {
      const nd = DIM_NAMES.length;
      // per-dim counts under all filters except own — one pass per dim
      const counts = DIM_NAMES.map((d, k) => {
        const m = new Map();
        outer: for (const r of rows) {
          for (let j = 0; j < nd; j++) {
            if (j !== k && values[j] !== null && r[DIM_NAMES[j]] !== values[j]) continue outer;
          }
          const v = r[d];
          m.set(v, (m.get(v) || 0) + 1);
        }
        return m;
      });
      // totals + filtered set
      let pass = 0;
      let sum = 0;
      const filtered = [];
      outer: for (const r of rows) {
        for (let j = 0; j < nd; j++) {
          if (values[j] !== null && r[DIM_NAMES[j]] !== values[j]) continue outer;
        }
        pass++;
        sum += r[MEASURE];
        filtered.push(r);
      }
      filtered.sort((a, b) => b[MEASURE] - a[MEASURE]);
      const top = filtered.slice(0, TOPK);
      // normalize counts to arrays in dict order for comparison
      const countArrays = counts.map((m, k) => dicts[k].map((v) => m.get(v) || 0));
      return { pass, sum, counts: countArrays, top };
    },
  };
}

// ---------- lane: crossfilter2 ----------

export async function laneCrossfilter(csvText, crossfilterFactory) {
  const rows = parseCsv(csvText);
  const t0 = performance.now();
  const cf = crossfilterFactory(rows);
  const dims = DIM_NAMES.map((d) => cf.dimension((r) => r[d]));
  const groups = dims.map((d) => d.group());
  const measureDim = cf.dimension((r) => r[MEASURE]);
  const totals = cf.groupAll().reduce(
    (p, r) => ({ n: p.n + 1, s: p.s + r[MEASURE] }),
    (p, r) => ({ n: p.n - 1, s: p.s - r[MEASURE] }),
    () => ({ n: 0, s: 0 }),
  );
  const dicts = DIM_NAMES.map((d, k) => groups[k].all().map((g) => g.key));
  const loadMs = performance.now() - t0;

  return {
    name: "crossfilter2",
    loadMs,
    dicts,
    meta: { rows: rows.length },
    interact(values) {
      for (let k = 0; k < dims.length; k++) {
        if (values[k] === null) dims[k].filterAll();
        else dims[k].filterExact(values[k]);
      }
      // group.all() reflects every filter except the group's own dimension
      const counts = groups.map((g, k) => {
        const m = new Map(g.all().map((e) => [e.key, e.value]));
        return dicts[k].map((v) => m.get(v) || 0);
      });
      const t = totals.value();
      const top = measureDim.top(TOPK);
      return { pass: t.n, sum: t.s, counts, top };
    },
  };
}

// ---------- runner ----------

export function runScript(lane, script, warmup = 20) {
  const times = [];
  let last = null;
  for (let i = 0; i < script.length; i++) {
    const t0 = performance.now();
    last = lane.interact(script[i]);
    const el = performance.now() - t0;
    if (i >= warmup) times.push(el);
  }
  times.sort((a, b) => a - b);
  return {
    name: lane.name,
    loadMs: lane.loadMs,
    median: times[Math.floor(times.length / 2)],
    p95: times[Math.floor(times.length * 0.95)],
    measured: times.length,
    last,
  };
}

/** Deep-compare two interact() results. Each lane's counts follow its own dict
 * order, so comparison is value-keyed. */
export function sameResult(a, aDicts, b, bDicts) {
  if (a.pass !== b.pass) return `pass ${a.pass} vs ${b.pass}`;
  // float sums accumulate in different orders across lanes; counts are the exact check
  if (Math.abs(a.sum - b.sum) > Math.abs(a.sum) * 1e-7 + 1e-7) return `sum ${a.sum} vs ${b.sum}`;
  for (let k = 0; k < aDicts.length; k++) {
    const bm = new Map(bDicts[k].map((v, c) => [v, b.counts[k][c] || 0]));
    for (let c = 0; c < aDicts[k].length; c++) {
      const v = aDicts[k][c];
      if ((a.counts[k][c] || 0) !== (bm.get(v) || 0)) {
        return `counts[dim ${k}][${v}]: ${a.counts[k][c]} vs ${bm.get(v) || 0}`;
      }
    }
  }
  return null;
}

// ---------- lane: hyparquet -> JS typed-array kernels ----------
// The "standard format + best-case JS compute" pairing: parquet decoded by
// hyparquet (as its README shows, to objects), dict-encoded into typed arrays
// at load (the honest ingest cost of that path), then the same correct-facets
// algorithm over Uint16Array codes.

export async function laneHyparquet(parquetBuffer, hp) {
  const t0 = performance.now();
  const rows = await hp.parquetReadObjects({ file: parquetBuffer });
  const n = rows.length;
  const dicts = [];
  const codesArr = [];
  for (const d of DIM_NAMES) {
    const idx = new Map();
    const dict = [];
    const codes = new Uint16Array(n);
    for (let i = 0; i < n; i++) {
      const v = rows[i][d];
      let c = idx.get(v);
      if (c === undefined) {
        c = dict.length;
        idx.set(v, c);
        dict.push(v);
      }
      codes[i] = c;
    }
    dicts.push(dict);
    codesArr.push(codes);
  }
  const measure = new Float64Array(n);
  for (let i = 0; i < n; i++) measure[i] = Number(rows[i][MEASURE]);
  const loadMs = performance.now() - t0;

  const nd = DIM_NAMES.length;
  const counts = dicts.map((d) => new Uint32Array(d.length));
  const mask = new Uint8Array(n);
  const codeMaps = dicts.map((d) => new Map(d.map((v, c) => [v, c])));

  return {
    name: "parquet → hyparquet → JS typed-array kernels",
    loadMs,
    dicts,
    meta: { rows: n },
    interact(values) {
      const sel = values.map((v, k) => (v === null ? -1 : codeMaps[k].get(v)));
      for (let k = 0; k < nd; k++) counts[k].fill(0);
      let pass = 0;
      let sum = 0;
      for (let row = 0; row < n; row++) {
        let fails = 0;
        let failDim = -1;
        for (let k = 0; k < nd; k++) {
          if (sel[k] >= 0 && codesArr[k][row] !== sel[k]) {
            fails++;
            if (fails === 2) break;
            failDim = k;
          }
        }
        if (fails === 0) {
          mask[row] = 1;
          pass++;
          sum += measure[row];
          for (let k = 0; k < nd; k++) counts[k][codesArr[k][row]]++;
        } else {
          mask[row] = 0;
          if (fails === 1) counts[failDim][codesArr[failDim][row]]++;
        }
      }
      const idx = [];
      for (let row = 0; row < n; row++) if (mask[row]) idx.push(row);
      idx.sort((a, b) => measure[b] - measure[a]);
      const top = idx.slice(0, TOPK);
      return { pass, sum, counts: counts.map((c) => Array.from(c)), top };
    },
  };
}
