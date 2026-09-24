// Fused facet refresh (the M1 facet_refresh primitive: one pass over codes with
// filters-except-own) vs the SQL path a dashboard or agent issues today (one
// GROUP BY per facet + a totals query). Evidence for design.sv d59.
//   node bench/facet-fuse.mjs [image] [measure] ["dim;dim;…"] ["dim=v|v,dim=v"] ["…next click"]
// Default: the PUDL image, 8 facets, Region=Asia + Status + Type. Counts are
// cross-checked between the two paths before anything is timed.
import { readFileSync } from "node:fs";
const root = new URL("../", import.meta.url).pathname;
const { instantiate } = await import(root + "js/facetful/core.js");
const engine = await instantiate(readFileSync(root + "target/wasm32-unknown-unknown/release/facetful_wasm.wasm"));
const image = readFileSync(process.argv[2] ?? root + "data/units-2026-08.facetful");
const w = engine.w, u32 = (n) => n >>> 0, dec = engine.dec, enc = engine.enc;
const open = () => engine.openTable(image).handle;
const t0h = open();
console.log(`${(process.argv[2] ?? "PUDL").split("/").pop()}: ${engine.describe(t0h).rows} rows`);
const colIdx = (h, name) => { const b = enc.encode(name); const p = u32(w.alloc(b.length)); new Uint8Array(engine.mem(), p, b.length).set(b); const i = w.table_col_by_name(h, p, b.length); w.dealloc(p, b.length); return i; };
const dictOf = (h, col) => { const n = w.table_dict_len(h, col); const out = []; for (let c = 0; c < n; c++) { const len = w.table_dict_value(h, col, c, engine.scratch, 4096); out.push(dec.decode(new Uint8Array(engine.mem(), engine.scratch, len))); } return out; };

const DIMS = process.argv[4] ? process.argv[4].split(";") : ["Type", "Region", "Subregion", "Country/area", "Status", "Technology", "Fuel (combustion only)", "CHP"];
const MEASURE = process.argv[3] ?? "Capacity (MW)";
const cols = DIMS.map((d) => colIdx(t0h, d)), mcol = colIdx(t0h, MEASURE);
const dicts = cols.map((c) => dictOf(t0h, c));
const pick = (dim, wanted, fallback) => { const d = dicts[DIMS.indexOf(dim)]; const f = wanted.map((v) => d.indexOf(v)).filter((i) => i >= 0); return f.length ? f : fallback; };
let selA, selB;
if (process.argv[5]) { // "dim=val|val,dim=val" and a second spec for the next click
  const parse = (spec) => Object.fromEntries(spec.split(",").map((kv) => { const [d, v] = kv.split("="); return [d, v.split("|").map((x) => dicts[DIMS.indexOf(d)].indexOf(x)).filter((i) => i >= 0)]; }));
  selA = parse(process.argv[5]); selB = parse(process.argv[6] ?? process.argv[5]);
} else {
  selA = { Region: pick("Region", ["Asia"], [0]), Status: pick("Status", ["operating", "construction"], [0, 1]), Type: pick("Type", ["coal"], [0]) };
  selB = { ...selA, Type: pick("Type", ["gas"], [1]) }; // next click: one facet changes
}
const q = (s) => `"${s.replace(/"/g, '""')}"`, lit = (s) => `'${s.replace(/'/g, "''")}'`;
const whereExcept = (s, except) => {
  const parts = [];
  for (const [dim, codes] of Object.entries(s)) {
    if (dim === except || !codes.length) continue;
    const names = codes.map((c) => lit(dicts[DIMS.indexOf(dim)][c]));
    parts.push(names.length === 1 ? `${q(dim)} = ${names[0]}` : `${q(dim)} in (${names.join(", ")})`);
  }
  return parts.length ? ` where ${parts.join(" and ")}` : "";
};
const show = (s) => Object.entries(s).map(([d, c]) => `${d}=${c.map((x) => dicts[DIMS.indexOf(d)][x]).join("|")}`).join(", ");

// `withSum`: count + sum per group (what a dashboard shows); the fused
// primitive counts per group and sums the measure once overall, so the
// like-for-like comparison is the count-only form plus the totals query
function sqlRefresh(h, s, withSum = true) {
  const out = {};
  for (const dim of DIMS) {
    const r = engine.query(h, `select ${q(dim)} as k, count(*) as n${withSum ? `, sum(${q(MEASURE)}) as mw` : ""} from t${whereExcept(s, dim)} group by ${q(dim)}`);
    const k = r.columns[0], n = r.columns[1], m = new Map();
    for (let i = 0; i < r.rowCount; i++) m.set((k.validity[i >> 3] >> (i & 7)) & 1 ? dec.decode(k.bytes.subarray(k.offsets[i], k.offsets[i + 1])) : null, n.values[i]);
    out[dim] = m;
  }
  const tot = engine.query(h, `select count(*) as n, sum(${q(MEASURE)}) as mw from t${whereExcept(s, null)}`);
  out.pass = tot.columns[0].values[0]; out.sum = tot.columns[1].values[0];
  return out;
}
function fusedRefresh(h, s) {
  const codes = DIMS.map((d) => s[d] ?? []);
  const nd = DIMS.length, total = codes.reduce((a, c) => a + c.length, 0);
  const pd = u32(w.alloc(nd * 4)), pl = u32(w.alloc(nd * 4)), pv = u32(w.alloc(Math.max(2, total * 2)));
  new Uint32Array(engine.mem(), pd, nd).set(cols);
  new Uint32Array(engine.mem(), pl, nd).set(codes.map((c) => c.length));
  new Uint16Array(engine.mem(), pv, total).set(codes.flat());
  const res = w.facet_refresh(h, pd, nd, pl, pv, mcol);
  w.dealloc(pd, nd * 4); w.dealloc(pl, nd * 4); w.dealloc(pv, Math.max(2, total * 2));
  if (!res) throw new Error("facet_refresh failed");
  const out = {};
  for (let d = 0; d < nd; d++) {
    const n = w.result_counts_len(res, d);
    const counts = new Uint32Array(engine.mem(), u32(w.result_counts_ptr(res, d)), n), m = new Map();
    for (let c = 0; c < n; c++) if (counts[c]) m.set(c < dicts[d].length ? dicts[d][c] : null, counts[c]);
    out[DIMS[d]] = m;
  }
  out.pass = w.result_pass(res); out.sum = w.result_sum(res);
  w.result_free(res);
  return out;
}
// agreement
for (const s of [selA, selB]) {
  const a = sqlRefresh(t0h, s), b = fusedRefresh(t0h, s);
  for (const dim of DIMS) {
    for (const [k, v] of a[dim]) if (b[dim].get(k) !== v) throw new Error(`${dim}: ${k} sql ${v} fused ${b[dim].get(k)}`);
    for (const [k, v] of b[dim]) if ((a[dim].get(k) ?? 0) !== v) throw new Error(`${dim}: ${k} fused ${v} sql ${a[dim].get(k)}`);
  }
  if (a.pass !== b.pass || Math.abs(a.sum - b.sum) > 1e-6 * Math.abs(a.sum)) throw new Error(`totals ${a.pass}/${a.sum} vs ${b.pass}/${b.sum}`);
  console.log(`agree: ${show(s)} -> ${a.pass} rows, ${a.sum.toFixed(0)} MW`);
}
// timings
const best = (f, n = 9) => { let b = Infinity; for (let i = 0; i < n; i++) { const t0 = performance.now(); f(); b = Math.min(b, performance.now() - t0); } return b; };
const ms = (x) => `${x.toFixed(2)} ms`.padStart(10);
const line = (label, sql, fused) => console.log(label.padEnd(44), `sql ${ms(sql)}   fused ${ms(fused)}   ratio ${(sql / fused).toFixed(1)}x`);
// cold: a fresh table handle per run (empty mask cache), first interaction
let cs = Infinity, cf = Infinity;
for (let i = 0; i < (image.byteLength > 100e6 ? 1 : 5); i++) { const h = open(); let t0 = performance.now(); sqlRefresh(h, selA); cs = Math.min(cs, performance.now() - t0); const h2 = open(); t0 = performance.now(); fusedRefresh(h2, selA); cf = Math.min(cf, performance.now() - t0); }
line(`cold: first interaction (${DIMS.length + 1} queries)`, cs, cf);
// warm: same interaction repeated (mask cache fully hot)
const h = open(); sqlRefresh(h, selA); fusedRefresh(h, selA);
line("warm: identical interaction repeated", best(() => sqlRefresh(h, selA)), best(() => fusedRefresh(h, selA)));
line("warm, SQL count-only (like for like)", best(() => sqlRefresh(h, selA, false)), best(() => fusedRefresh(h, selA)));
// next click: A -> B -> A alternating (one facet's selection changes each time)
let flip = false;
const alt = (f) => { flip = !flip; return f(h, flip ? selB : selA); };
line("next click: one selection changes (A<->B)", best(() => alt(sqlRefresh), 10), best(() => alt(fusedRefresh), 10));
// per-query cost breakdown of the SQL path, warm
for (const d of [DIMS[0], DIMS[3]]) { const t1 = performance.now(); for (let i = 0; i < 20; i++) engine.query(h, `select ${q(d)} as k, count(*) as n, sum(${q(MEASURE)}) as mw from t${whereExcept(selA, d)} group by ${q(d)}`); console.log(`one warm facet query (${d}, ${dicts[DIMS.indexOf(d)].length} values):`.padEnd(44), ms((performance.now() - t1) / 20)); }

// anatomy of one warm SQL facet query: fixed overhead vs aggregation
const anat = (label, sql) => { const t0 = performance.now(); for (let i = 0; i < 30; i++) engine.query(h, sql); console.log(("  " + label).padEnd(44), ms((performance.now() - t0) / 30)); };
console.log("anatomy (warm):");
anat("select 1 from t limit 1", "select 1 as x from t limit 1");
anat("count(*) no where", "select count(*) as n from t");
anat("count(*) with the 2 cached filters", `select count(*) as n from t${whereExcept(selA, "Type")}`);
anat(`group by ${DIMS[0]} no where`, `select ${q(DIMS[0])} as k, count(*) as n from t group by ${q(DIMS[0])}`);
anat(`group by ${DIMS[0]} + filters, count only`, `select ${q(DIMS[0])} as k, count(*) as n from t${whereExcept(selA, DIMS[0])} group by ${q(DIMS[0])}`);
anat(`group by ${DIMS[0]} + filters, count+sum`, `select ${q(DIMS[0])} as k, count(*) as n, sum(${q(MEASURE)}) as mw from t${whereExcept(selA, DIMS[0])} group by ${q(DIMS[0])}`);
