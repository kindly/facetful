// Geo distance: Rust-wasm trig vs JavaScript-UDF trig, in Node (V8, same as Chrome).
//
//   node spikes/geo-bench/bench.mjs <engine.wasm> <table.facetful>...
//
// Engine lanes (mask cache off, so every run recomputes the predicate):
//   rust-composed  haversine as SQL over the engine's cos/asin/sqrt
//   js-composed    the same SQL with js_cos/js_asin UDFs (Math.*), engine sqrt
//   rust-fused     geo_distance(lat, lon, lat0, lon0)
//   js-fused       js_geo_distance(...) — one UDF, Math.* inside
//   sql-equirect   equirectangular approximation in plain arithmetic (no trig per row)
// Microbench (no engine): the haversine loop as a standalone Rust-wasm kernel vs plain JS.
//
// The rust-* lanes need an engine build with built-in cos/asin/geo_distance, which was
// measured (2026-09-26) and then not kept: a fused JS geo_distance matched it (6.9 vs 7.9 ms
// at 183K rows, 101 vs 106 ms at 2M) for 0 KB instead of +6.3 KB gz. geo_distance now
// ships in facetful/udfs; register it under another name here to compare against it.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const HERE = fileURLToPath(new URL(".", import.meta.url));
const [wasmPath, ...tables] = process.argv.slice(2);
const { instantiate } = await import(new URL("../../js/facetful/core.js", import.meta.url).href);
const engine = await instantiate(readFileSync(wasmPath));

const LAT0 = 51.5074, LON0 = -0.1278, R_M = 40233.6; // London, 25 miles
const K = Math.PI / 180, RE = 6371008.8, C0 = Math.cos(LAT0 * K);

// --- JS UDFs -------------------------------------------------------------------
const unary = (f) => (args, len, out) => {
  const a = args[0];
  if (a.broadcast) { out.values.fill(f(a.values[0])); return; }
  const v = a.values, o = out.values;
  for (let i = 0; i < len; i++) o[i] = f(v[i]);
};
const num1 = { params: ["float"], returns: "float" };
engine.registerFunction("js_cos", num1, unary(Math.cos));
engine.registerFunction("js_asin", num1, unary(Math.asin));
engine.registerFunction("js_geo_distance", { params: ["float", "float", "float", "float"], returns: "float" }, (args, len, out) => {
  const at = (a) => (a.broadcast ? () => a.values[0] : (i) => a.values[i]);
  const [la, lo, a2, b2] = args;
  const o = out.values;
  if (!la.broadcast && !lo.broadcast && a2.broadcast && b2.broadcast) {
    const lat = la.values, lon = lo.values, lat2 = a2.values[0], lon2 = b2.values[0];
    const p2 = lat2 * K, cp2 = Math.cos(p2);
    for (let i = 0; i < len; i++) {
      const p1 = lat[i] * K;
      const dp = Math.sin((p2 - p1) * 0.5), dl = Math.sin((lon2 - lon[i]) * K * 0.5);
      const h = dp * dp + Math.cos(p1) * cp2 * dl * dl;
      o[i] = 2 * RE * Math.asin(Math.min(Math.sqrt(h), 1));
    }
    return;
  }
  const f = [la, lo, a2, b2].map(at);
  for (let i = 0; i < len; i++) {
    const p1 = f[0](i) * K, p2 = f[2](i) * K;
    const dp = Math.sin((p2 - p1) * 0.5), dl = Math.sin((f[3](i) - f[1](i)) * K * 0.5);
    const h = dp * dp + Math.cos(p1) * Math.cos(p2) * dl * dl;
    o[i] = 2 * RE * Math.asin(Math.min(Math.sqrt(h), 1));
  }
});

// --- distance expressions ------------------------------------------------------
const LA = `"Latitude"`, LO = `"Longitude"`;
// haversine via sin²(x/2) = (1 - cos x)/2: three cos + one asin, no repeated subexpression
const composed = (cos, asin) =>
  `2*${RE}*${asin}(sqrt(0.5 - 0.5*${cos}((${LA} - ${LAT0})*${K}) + ${C0}*${cos}(${LA}*${K})*(0.5 - 0.5*${cos}((${LO} - ${LON0})*${K}))))`;
const DEG_M = RE * K; // metres per degree of arc
const equirect = `(${DEG_M}*sqrt(((${LO} - ${LON0})*${C0})*((${LO} - ${LON0})*${C0}) + (${LA} - ${LAT0})*(${LA} - ${LAT0})))`;
const lanes = {
  "rust-composed": composed("cos", "asin"),
  "js-composed": composed("js_cos", "js_asin"),
  "rust-fused": `geo_distance(${LA}, ${LO}, ${LAT0}, ${LON0})`,
  "js-fused": `js_geo_distance(${LA}, ${LO}, ${LAT0}, ${LON0})`,
  "sql-equirect": equirect,
  "scan-floor": `${LA}`, // no distance at all: the engine's per-query floor
};
// the filter the app would run: squared-degrees form for equirect (no sqrt at all)
const R_DEG = R_M / DEG_M;
const filterOf = (name, d) =>
  name === "scan-floor"
    ? `${LA} <= 1000`
    : name === "sql-equirect"
    ? `((${LO} - ${LON0})*${C0})*((${LO} - ${LON0})*${C0}) + (${LA} - ${LAT0})*(${LA} - ${LAT0}) <= ${R_DEG * R_DEG}`
    : `${d} <= ${R_M}`;

// the same 32-bit PRNG as the synthetic table (see gen-points.mjs)
export function mulberry32(a) {
  return () => {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const median = (f, n = 11) => {
  for (let i = 0; i < 3; i++) f();
  const ts = [];
  for (let i = 0; i < n; i++) { const s = performance.now(); f(); ts.push(performance.now() - s); }
  return ts.sort((a, b) => a - b)[n >> 1];
};

for (const path of tables) {
  const { handle } = engine.openTable(readFileSync(path));
  engine.setMaskBudget(handle, 0);
  const rows = engine.query(handle, "select count(*) as n from t").columns[0].values[0];
  console.log(`\n## ${path.split("/").pop()} — ${rows.toLocaleString()} rows`);
  console.log("lane           | filter count(*) ms | within | nearest-10 ms | nearest d (m)");
  for (const [name, d] of Object.entries(lanes)) {
    const qf = `select count(*) as n from t where ${filterOf(name, d)}`;
    const qn = `select ${d} as d from t order by d limit 10`;
    const n = engine.query(handle, qf).columns[0].values[0];
    const near = engine.query(handle, qn).columns[0].values[0];
    const tf = median(() => engine.query(handle, qf));
    const tn = median(() => engine.query(handle, qn));
    console.log(`${name.padEnd(14)} | ${tf.toFixed(2).padStart(18)} | ${String(n).padStart(6)} | ${tn.toFixed(2).padStart(13)} | ${near.toFixed(1)}`);
  }
  // the bounding-box prefilter the planner could add: does the exact test then run on survivors only?
  const dLat = R_DEG, dLon = R_DEG / C0;
  const bbox = `${LA} between ${LAT0 - dLat} and ${LAT0 + dLat} and ${LO} between ${LON0 - dLon} and ${LON0 + dLon}`;
  for (const name of ["rust-fused", "js-fused"]) {
    const q = `select count(*) as n from t where ${bbox} and ${lanes[name]} <= ${R_M}`;
    const n = engine.query(handle, q).columns[0].values[0];
    console.log(`${(name + "+bbox").padEnd(14)} | ${median(() => engine.query(handle, q)).toFixed(2).padStart(18)} | ${String(n).padStart(6)} |`);
  }
}

// --- microbench: the loop alone ------------------------------------------------
const kernel = (await WebAssembly.instantiate(readFileSync(HERE + "kernel/target/wasm32-unknown-unknown/release/geo_kernel.wasm"))).instance.exports;
console.log("\n## microbench — haversine count-within loop, no engine");
console.log("rows      | rust-wasm ms | js ms | ns/row rust | ns/row js");
for (const n of [183125, 2_000_000]) {
  const lp = kernel.alloc_f64(n), op = kernel.alloc_f64(n);
  const lat = new Float64Array(kernel.memory.buffer, lp, n), lon = new Float64Array(kernel.memory.buffer, op, n);
  const r = mulberry32(1);
  for (let i = 0; i < n; i++) { lat[i] = r() * 130 - 60; lon[i] = r() * 360 - 180; }
  const jsCount = () => {
    const p2 = LAT0 * K, cp2 = Math.cos(p2);
    let c = 0;
    for (let i = 0; i < n; i++) {
      const p1 = lat[i] * K;
      const dp = Math.sin((p2 - p1) * 0.5), dl = Math.sin((LON0 - lon[i]) * K * 0.5);
      const h = dp * dp + Math.cos(p1) * cp2 * dl * dl;
      c += 2 * RE * Math.asin(Math.min(Math.sqrt(h), 1)) <= R_M ? 1 : 0;
    }
    return c;
  };
  const a = kernel.count_within(lp, op, n, LAT0, LON0, R_M), b = jsCount();
  if (a !== b) console.log(`MISMATCH rust ${a} js ${b}`);
  const tr = median(() => kernel.count_within(lp, op, n, LAT0, LON0, R_M));
  const tj = median(jsCount);
  console.log(`${String(n).padEnd(9)} | ${tr.toFixed(2).padStart(12)} | ${tj.toFixed(2).padStart(5)} | ${(tr * 1e6 / n).toFixed(1).padStart(11)} | ${(tj * 1e6 / n).toFixed(1).padStart(9)}`);
}
