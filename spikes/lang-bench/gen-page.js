// Generates bench.sv: a self-contained sideview page embedding both wasm builds
// (base + SIMD, base64) and the JS kernels, runnable in the browser viewing the page.
import { readFileSync, writeFileSync } from "node:fs";

const wasmBase = readFileSync(
  new URL("./wasm/target/wasm32-unknown-unknown/release/lang_bench.wasm", import.meta.url),
);
const wasmSimd = readFileSync(
  new URL("./wasm/target-simd/wasm32-unknown-unknown/release/lang_bench.wasm", import.meta.url),
);
const kernels = readFileSync(new URL("./js/kernels.js", import.meta.url), "utf8")
  .replaceAll("export function", "function")
  .replaceAll("export const", "const");

const html = `<!doctype html>
<meta charset="utf-8">
<div class="p-3">
  <div class="d-flex gap-2 align-items-center mb-3">
    <select id="nsel" class="form-select form-select-sm" style="width:auto">
      <option value="200000" selected>200,000 rows</option>
      <option value="1000000">1,000,000 rows</option>
      <option value="5000000">5,000,000 rows</option>
    </select>
    <button id="run" class="btn btn-primary btn-sm">Run benchmark</button>
    <span id="status" class="text-muted small"></span>
  </div>
  <table class="table table-sm" id="results" style="display:none">
    <thead><tr><th>operation</th><th>implementation</th><th class="text-end">median</th><th class="text-end">vs JS baseline</th></tr></thead>
    <tbody></tbody>
  </table>
  <div class="small text-muted" id="env"></div>
  <div class="small text-muted mt-2">
    All implementations share one algorithm per operation and identical seeded data; data is resident
    per world (typed arrays / wasm linear memory), one boundary call per operation. Scalar wasm build:
    32.8&nbsp;KB raw / ~11.5&nbsp;KB gz; SIMD build +1.7&nbsp;KB. GROUP BY uses open addressing with a
    mixed multiplicative hash — deliberately no direct-array shortcut, since a general GROUP BY can't
    assume dense keys. Batched timing defeats timer-precision clamping.
  </div>
</div>
<script type="module">
${kernels}

const B64_BASE = "${wasmBase.toString("base64")}";
const B64_SIMD = "${wasmSimd.toString("base64")}";
const load = async (b64) =>
  (await WebAssembly.instantiate(Uint8Array.from(atob(b64), c => c.charCodeAt(0)), {})).instance.exports;
const w = await load(B64_BASE);
const ws = await load(B64_SIMD);
const D = DIMS.length;
const TOPK = 50;
const CAP = 131072;
const OWNER = 4, YEAR = 5;

const el = (id) => document.getElementById(id);
el("env").textContent = navigator.userAgent;

function setup(N) {
  const { codes, measure } = makeDataset(N);
  const cards = Uint32Array.from(DIMS.map(d => d.card));
  const p = {
    codes: codes.map(a => w.alloc(a.byteLength)),
    measure: w.alloc(measure.byteLength),
    counts: DIMS.map(d => w.alloc(d.card * 4)),
    codesPtrs: w.alloc(D * 4), cardsArr: w.alloc(D * 4), selected: w.alloc(D * 4),
    countsPtrs: w.alloc(D * 4), mask: w.alloc(N), totals: w.alloc(16), topk: w.alloc(TOPK * 4),
    gKeys: w.alloc(CAP * 4), gSums: w.alloc(CAP * 8), gCounts: w.alloc(CAP * 4),
    allMask: w.alloc(N), scanMask: w.alloc(N),
  };
  const mem = () => w.memory.buffer;
  codes.forEach((a, k) => new Uint16Array(mem(), p.codes[k], N).set(a));
  new Float64Array(mem(), p.measure, N).set(measure);
  new Uint32Array(mem(), p.codesPtrs, D).set(p.codes);
  new Uint32Array(mem(), p.cardsArr, D).set(cards);
  new Uint32Array(mem(), p.countsPtrs, D).set(p.counts);
  new Uint8Array(mem(), p.allMask, N).fill(1);
  const sp = { codes0: ws.alloc(codes[0].byteLength), measure: ws.alloc(measure.byteLength), mask: ws.alloc(N) };
  new Uint16Array(ws.memory.buffer, sp.codes0, N).set(codes[0]);
  new Float64Array(ws.memory.buffer, sp.measure, N).set(measure);
  return {
    N, codes, measure, p, sp, mem,
    jsCounts: DIMS.map(d => new Uint32Array(d.card)),
    jsMask: new Uint8Array(N),
    jsAllMask: new Uint8Array(N).fill(1),
    jsScanMask: new Uint8Array(N),
    jsSlotKeys: new Uint32Array(CAP), jsSlotSums: new Float64Array(CAP), jsSlotCounts: new Uint32Array(CAP),
  };
}

const sleep = (ms) => new Promise(r => setTimeout(r, ms));

async function bench(fn, args, warmup, batches, perBatch) {
  for (let i = 0; i < warmup; i++) fn(args[i % args.length]);
  const times = [];
  for (let b = 0; b < batches; b++) {
    const t0 = performance.now();
    for (let i = 0; i < perBatch; i++) fn(args[(b * perBatch + i) % args.length]);
    times.push((performance.now() - t0) / perBatch);
    await sleep(0);
  }
  times.sort((a, z) => a - z);
  return { med: times[Math.floor(times.length / 2)], p95: times[Math.floor(times.length * 0.95)] };
}

let currentBaseline = null;
function row(op, impl, r, isBaseline) {
  if (isBaseline) currentBaseline = r.med;
  const tb = el("results").querySelector("tbody");
  const tr = document.createElement("tr");
  const cmp = isBaseline ? "<span class='text-muted'>baseline</span>"
    : "<strong>" + (currentBaseline / r.med).toFixed(2) + "x</strong>";
  tr.innerHTML = "<td>" + op + "</td><td>" + impl + "</td>" +
    "<td class='text-end'>" + r.med.toFixed(2) + " ms <span class='text-muted'>(p95 " + r.p95.toFixed(2) + ")</span></td>" +
    "<td class='text-end'>" + cmp + "</td>";
  tb.appendChild(tr);
}

el("run").onclick = async () => {
  el("run").disabled = true;
  el("results").style.display = "";
  el("results").querySelector("tbody").innerHTML = "";
  const N = parseInt(el("nsel").value, 10);
  const status = (t) => { el("status").textContent = t; return sleep(20); };
  await status("generating " + N.toLocaleString() + " rows…");
  const S = setup(N);
  const script = makeInteractions(140);
  const batches = N > 1_000_000 ? 8 : 12, per = N > 1_000_000 ? 4 : 10;
  const noArgs = [null];

  const runJs = (sel) => { facetRefresh(S.N, S.codes, S.jsCounts, sel, S.measure, S.jsMask); sortTopk(S.measure, S.jsMask, S.N, TOPK); };
  const runWasm = (sel) => {
    new Int32Array(S.mem(), S.p.selected, D).set(sel);
    w.facet_refresh(S.N, D, S.p.codesPtrs, S.p.cardsArr, S.p.selected, S.p.measure, S.p.countsPtrs, S.p.mask, S.p.totals);
    w.sort_topk(S.p.measure, S.p.mask, S.N, TOPK, S.p.topk);
  };

  await status("facet interaction…");
  row("facet refresh + top-50", "plain JS", await bench(runJs, script, 20, batches, per), true);
  row("facet refresh + top-50", "wasm", await bench(runWasm, script, 20, batches, per));

  await status("hash GROUP BY…");
  row("GROUP BY owner×year (~38K groups)", "JS Map (idiomatic)",
    await bench(() => groupAggMap(S.codes[OWNER], S.codes[YEAR], S.jsAllMask, S.measure, S.N), noArgs, 5, batches, Math.max(2, per / 2)), true);
  row("GROUP BY owner×year (~38K groups)", "JS typed-array hash",
    await bench(() => groupAggHash(S.codes[OWNER], S.codes[YEAR], S.jsAllMask, S.measure, S.N, CAP, S.jsSlotKeys, S.jsSlotSums, S.jsSlotCounts), noArgs, 5, batches, Math.max(2, per / 2)));
  row("GROUP BY owner×year (~38K groups)", "wasm",
    await bench(() => w.group_agg(S.p.codes[OWNER], S.p.codes[YEAR], S.p.allMask, S.p.measure, S.N, CAP, S.p.gKeys, S.p.gSums, S.p.gCounts), noArgs, 5, batches, Math.max(2, per / 2)));

  await status("scan kernels…");
  row("scan: mask = (country == c)", "plain JS", await bench(() => maskEqU16(S.codes[0], S.N, 3, S.jsScanMask), noArgs, 10, batches, per * 2), true);
  row("scan: mask = (country == c)", "wasm scalar", await bench(() => w.mask_eq_u16(S.p.codes[0], S.N, 3, S.p.scanMask), noArgs, 10, batches, per * 2));
  row("scan: mask = (country == c)", "wasm SIMD", await bench(() => ws.mask_eq_u16_simd(S.sp.codes0, S.N, 3, S.sp.mask), noArgs, 10, batches, per * 2));
  row("sum(measure), full column", "plain JS", await bench(() => sumF64(S.measure, S.N), noArgs, 10, batches, per * 2), true);
  row("sum(measure), full column", "wasm scalar", await bench(() => w.sum_f64(S.p.measure, S.N), noArgs, 10, batches, per * 2));
  row("sum(measure), full column", "wasm SIMD", await bench(() => ws.sum_f64_simd(S.sp.measure, S.N), noArgs, 10, batches, per * 2));

  el("status").textContent = "done — " + N.toLocaleString() + " rows";
  el("run").disabled = false;
};
</script>`;

const sv = `<sv-page label="Wasm vs JS bench">

<sv-prose id="w1">
# Rust/wasm vs plain JS — kernel benchmark

The language question from the design discussion, made empirical. Every operation runs the
**identical algorithm over identical seeded data** in each implementation; data is resident per
world; one boundary call per operation.

**Operations** (in decision-relevance order):
1. **Facet interaction** — correct filters-except-own counts over 6 dims + totals + top-50 sort:
   the flagship workload's hot loop.
2. **Hash GROUP BY** — \`owner × year → sum, count\` (~38K groups via open addressing; deliberately
   no direct-array shortcut, since a general GROUP BY can't assume dense keys): the op a real SQL
   engine cannot avoid.
3. **Scan kernels** — single-predicate mask build and full-column sum, where wasm SIMD128 gets to
   play and JS has no counterpart.

**Node/V8 reference numbers** (200K rows; run below in Firefox for the ones that count):
facet interaction JS 2.6ms vs wasm 1.6ms (**1.6x**); GROUP BY: JS Map 10.5ms, JS typed-array hash
3.6ms, wasm 1.65ms (**2.2x vs best JS, 6.4x vs idiomatic JS**); scans: wasm scalar ~2x, **wasm SIMD
4-8x** over JS. Kernel sizes: 32.8 KB raw / ~11.5 KB gz scalar, +1.7 KB for SIMD.

A lesson worth keeping from building this: the first typed-array hash had a subtle hash-function bug
(masking the low bits of a multiplicative hash → probe clustering) that made *both* hand-rolled paths
4x slower than JS Map. Algorithm quality dominates language choice — in both directions.
</sv-prose>

<sv-html id="w2" height="44rem">
${html}
</sv-html>

<sv-prose id="w3">
## How to read the result

- The **facet interaction** row is your flagship workload: if JS is within ~1.5x there, language
  barely matters *for that page* at 200K rows — both are imperceptible.
- The **GROUP BY** rows are the general-SQL-engine question: idiomatic JS (Map) is what a quick JS
  engine would do; the typed-array hash is JS's best case; wasm's margin over both is the real
  argument for Rust once the engine grows past facet counting.
- The **SIMD** rows are headroom only wasm has — relevant for scan-heavy SQL (\`WHERE\` over many
  groups, aggregates over wide selections), and it comes nearly free in binary size (+1.7 KB).
- Still in JS's favor, unmeasured here: no result-copy across a boundary, no instantiation on cold
  start, one toolchain. Still in wasm's favor, unmeasured: no GC pauses in long sessions, i64.
- Run at 1M and 5M rows too — headroom is part of the decision. Firefox numbers matter most
  (it's the daily browser); Chrome as the second point.
</sv-prose>

</sv-page>
`;

writeFileSync(new URL("./bench.sv", import.meta.url), sv);
console.log("bench.sv written,", (sv.length / 1024).toFixed(0), "KB");
