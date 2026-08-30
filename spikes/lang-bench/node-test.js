// Correctness check (JS results must equal wasm results exactly) + rough V8 timing.
// Real numbers come from the browser page; this is the sanity gate.
import { readFileSync } from "node:fs";
import {
  facetRefresh, sortTopk, makeDataset, makeInteractions, DIMS,
} from "./js/kernels.js";

const N = 200_000;
const TOPK = 50;
const WARMUP = 20;
const MEASURED = 100;

const wasmBytes = readFileSync(
  new URL("./wasm/target/wasm32-unknown-unknown/release/lang_bench.wasm", import.meta.url),
);
const { instance } = await WebAssembly.instantiate(wasmBytes, {});
const w = instance.exports;

// ---- load dataset into both worlds ----
const { codes, measure } = makeDataset(N);
const cards = Uint32Array.from(DIMS.map((d) => d.card));
const D = DIMS.length;

// wasm side: allocate everything first, then create views (memory may grow)
const ptrs = {
  codes: codes.map((a) => w.alloc(a.byteLength)),
  measure: w.alloc(measure.byteLength),
  counts: DIMS.map((d) => w.alloc(d.card * 4)),
  codesPtrs: w.alloc(D * 4),
  cardsArr: w.alloc(D * 4),
  selected: w.alloc(D * 4),
  countsPtrs: w.alloc(D * 4),
  mask: w.alloc(N),
  totals: w.alloc(16),
  topk: w.alloc(TOPK * 4),
};
const mem = () => w.memory.buffer;
codes.forEach((a, k) => new Uint16Array(mem(), ptrs.codes[k], N).set(a));
new Float64Array(mem(), ptrs.measure, N).set(measure);
new Uint32Array(mem(), ptrs.codesPtrs, D).set(ptrs.codes);
new Uint32Array(mem(), ptrs.cardsArr, D).set(cards);
new Uint32Array(mem(), ptrs.countsPtrs, D).set(ptrs.counts);

// JS side working buffers
const jsCounts = DIMS.map((d) => new Uint32Array(d.card));
const jsMask = new Uint8Array(N);

const script = makeInteractions(WARMUP + MEASURED);

function runWasm(selected) {
  new Int32Array(mem(), ptrs.selected, D).set(selected);
  w.facet_refresh(
    N, D, ptrs.codesPtrs, ptrs.cardsArr, ptrs.selected, ptrs.measure,
    ptrs.countsPtrs, ptrs.mask, ptrs.totals,
  );
  const written = w.sort_topk(ptrs.measure, ptrs.mask, N, TOPK, ptrs.topk);
  const totals = new Float64Array(mem(), ptrs.totals, 2);
  return {
    passCount: totals[0],
    sum: totals[1],
    counts: DIMS.map((d, k) => new Uint32Array(mem(), ptrs.counts[k], d.card)),
    topk: new Uint32Array(mem(), ptrs.topk, written),
  };
}

function runJs(selected) {
  const { passCount, sum } = facetRefresh(N, codes, jsCounts, selected, measure, jsMask);
  const topk = sortTopk(measure, jsMask, N, TOPK);
  return { passCount, sum, counts: jsCounts, topk };
}

// ---- correctness ----
let checked = 0;
for (const sel of script.slice(0, 10)) {
  const a = runJs(sel);
  const b = runWasm(sel);
  if (a.passCount !== b.passCount) throw new Error(`passCount mismatch: ${a.passCount} vs ${b.passCount}`);
  if (Math.abs(a.sum - b.sum) > Math.abs(a.sum) * 1e-12) throw new Error(`sum mismatch: ${a.sum} vs ${b.sum}`);
  for (let k = 0; k < D; k++) {
    for (let c = 0; c < cards[k]; c++) {
      if (a.counts[k][c] !== b.counts[k][c]) throw new Error(`counts mismatch dim ${k} code ${c}`);
    }
  }
  if (a.topk.length !== b.topk.length) throw new Error("topk length mismatch");
  for (let i = 0; i < a.topk.length; i++) {
    // ties may order differently; values must match
    if (measure[a.topk[i]] !== measure[b.topk[i]]) throw new Error(`topk value mismatch at ${i}`);
  }
  checked++;
}
console.log(`correctness: OK (${checked} interactions, ${N} rows, ${D} dims)`);

// ---- timing ----
function bench(label, fn) {
  for (const sel of script.slice(0, WARMUP)) fn(sel);
  const times = [];
  for (const sel of script.slice(WARMUP)) {
    const t0 = performance.now();
    fn(sel);
    times.push(performance.now() - t0);
  }
  times.sort((x, y) => x - y);
  const med = times[Math.floor(times.length / 2)];
  const p95 = times[Math.floor(times.length * 0.95)];
  console.log(`${label}: median ${med.toFixed(2)}ms  p95 ${p95.toFixed(2)}ms`);
  return med;
}

const mJs = bench("JS   facet_refresh+topk", runJs);
const mWasm = bench("wasm facet_refresh+topk", runWasm);
console.log(`speedup (Node/V8, indicative only): ${(mJs / mWasm).toFixed(2)}x`);

// per-op split
const selMid = script[WARMUP + 5];
runJs(selMid); runWasm(selMid); // ensure state
bench("JS   facet_refresh only", (sel) => facetRefresh(N, codes, jsCounts, sel, measure, jsMask));
bench("wasm facet_refresh only", (sel) => {
  new Int32Array(mem(), ptrs.selected, D).set(sel);
  w.facet_refresh(N, D, ptrs.codesPtrs, ptrs.cardsArr, ptrs.selected, ptrs.measure, ptrs.countsPtrs, ptrs.mask, ptrs.totals);
});
bench("JS   sort_topk only", () => sortTopk(measure, jsMask, N, TOPK));
bench("wasm sort_topk only", () => w.sort_topk(ptrs.measure, ptrs.mask, N, TOPK, ptrs.topk));

// ---- new kernels: correctness + timing ----
import { maskEqU16, sumF64, groupAggMap, groupAggHash } from "./js/kernels.js";

const CAP = 131072; // pow2 > 2 * 60000 possible groups
const gp = {
  keys: w.alloc(CAP * 4), sums: w.alloc(CAP * 8), counts: w.alloc(CAP * 4),
  allMask: w.alloc(N), scanMask: w.alloc(N),
};
new Uint8Array(mem(), gp.allMask, N).fill(1);
const jsAllMask = new Uint8Array(N).fill(1);
const jsSlotKeys = new Uint32Array(CAP), jsSlotSums = new Float64Array(CAP), jsSlotCounts = new Uint32Array(CAP);
const OWNER = 4, YEAR = 5; // dim indices

// correctness: wasm group_agg vs JS Map vs JS hash
{
  const ref = groupAggMap(codes[OWNER], codes[YEAR], jsAllMask, measure, N);
  const g2 = groupAggHash(codes[OWNER], codes[YEAR], jsAllMask, measure, N, CAP, jsSlotKeys, jsSlotSums, jsSlotCounts);
  const g3 = w.group_agg(ptrs.codes[OWNER], ptrs.codes[YEAR], gp.allMask, ptrs.measure, N, CAP, gp.keys, gp.sums, gp.counts);
  if (g2 !== ref.idx.size || g3 !== ref.idx.size) throw new Error(`group count mismatch: map=${ref.idx.size} hash=${g2} wasm=${g3}`);
  const wKeys = new Uint32Array(mem(), gp.keys, CAP), wSums = new Float64Array(mem(), gp.sums, CAP), wCounts = new Uint32Array(mem(), gp.counts, CAP);
  for (let h = 0; h < CAP; h++) {
    if (wKeys[h] === 0xffffffff) continue;
    const g = ref.idx.get(wKeys[h]);
    if (g === undefined) throw new Error("wasm produced unknown group key");
    if (wCounts[h] !== ref.counts[g]) throw new Error("group count mismatch");
    if (Math.abs(wSums[h] - ref.sums[g]) > Math.abs(ref.sums[g]) * 1e-9) throw new Error("group sum mismatch");
  }
  console.log(`group_agg correctness: OK (${ref.idx.size} groups)`);
}
// correctness: scan kernels
{
  const jsMaskBuf = new Uint8Array(N);
  const c1 = maskEqU16(codes[0], N, 3, jsMaskBuf);
  const c2 = w.mask_eq_u16(ptrs.codes[0], N, 3, gp.scanMask);
  if (c1 !== c2) throw new Error(`mask_eq count mismatch ${c1} vs ${c2}`);
  const s1 = sumF64(measure, N), s2 = w.sum_f64(ptrs.measure, N);
  if (Math.abs(s1 - s2) > Math.abs(s1) * 1e-9) throw new Error("sum mismatch");
  console.log(`scan kernels correctness: OK (mask hits ${c1})`);
}
// timing
bench("JS   groupby Map        ", () => groupAggMap(codes[OWNER], codes[YEAR], jsAllMask, measure, N));
bench("JS   groupby typed-hash ", () => groupAggHash(codes[OWNER], codes[YEAR], jsAllMask, measure, N, CAP, jsSlotKeys, jsSlotSums, jsSlotCounts));
bench("wasm groupby            ", () => w.group_agg(ptrs.codes[OWNER], ptrs.codes[YEAR], gp.allMask, ptrs.measure, N, CAP, gp.keys, gp.sums, gp.counts));
const jsScanMask = new Uint8Array(N);
bench("JS   mask_eq scan       ", () => maskEqU16(codes[0], N, 3, jsScanMask));
bench("wasm mask_eq scan       ", () => w.mask_eq_u16(ptrs.codes[0], N, 3, gp.scanMask));
bench("JS   sum(f64)           ", () => sumF64(measure, N));
bench("wasm sum(f64)           ", () => w.sum_f64(ptrs.measure, N));

// SIMD build (separate instance/memory)
const simdBytes = readFileSync(new URL("./wasm/target-simd/wasm32-unknown-unknown/release/lang_bench.wasm", import.meta.url));
const simdInst = (await WebAssembly.instantiate(simdBytes, {})).instance.exports;
const sPtrs = { codes0: simdInst.alloc(codes[0].byteLength), measure: simdInst.alloc(measure.byteLength), mask: simdInst.alloc(N) };
new Uint16Array(simdInst.memory.buffer, sPtrs.codes0, N).set(codes[0]);
new Float64Array(simdInst.memory.buffer, sPtrs.measure, N).set(measure);
const cSimd = simdInst.mask_eq_u16_simd(sPtrs.codes0, N, 3, sPtrs.mask);
if (cSimd !== maskEqU16(codes[0], N, 3, jsScanMask)) throw new Error("simd mask count mismatch");
const sSimd = simdInst.sum_f64_simd(sPtrs.measure, N);
if (Math.abs(sSimd - sumF64(measure, N)) > Math.abs(sSimd) * 1e-9) throw new Error("simd sum mismatch");
console.log("SIMD correctness: OK");
bench("wasm mask_eq SIMD       ", () => simdInst.mask_eq_u16_simd(sPtrs.codes0, N, 3, sPtrs.mask));
bench("wasm sum(f64) SIMD      ", () => simdInst.sum_f64_simd(sPtrs.measure, N));
