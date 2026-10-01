// Which character positions make selective mask keys? One counting pass over
// grantnav's five search columns (folded tokens, as the vocabulary).
//   node spikes/fts-bench/keys.mjs   -> out/key-stats.json
// Schemes, per token:
//   w12  first two characters        w123 first three characters
//   w23  2nd and 3rd characters      wEnd last two characters
//   any2 every 2-character sequence (substring matching)
// For typed prefixes, the rows a scheme's masks would admit, next to the rows
// that really match (a token starting with the prefix, or containing it).
import { writeFileSync } from "node:fs";
import { openEngine, COLS, ident, tokenize, OUT } from "./common.mjs";

const TYPED = ["me", "men", "ment", "mental", "he", "hea", "health", "cl", "cli", "clim", "climate", "yo", "you", "youth",
  "sp", "spo", "sport", "lo", "lon", "london", "co", "com", "community", "pr", "pro", "project", "su", "sup", "support",
  "en", "env", "environmental", "ad", "ada", "adaptation", "ch", "chi", "children"];
const SCHEMES = {
  w12: (t, out) => { if (t.length >= 2) out.add(t.slice(0, 2)); },
  w123: (t, out) => { if (t.length >= 3) out.add(t.slice(0, 3)); },
  w23: (t, out) => { if (t.length >= 3) out.add(t.slice(1, 3)); },
  wEnd: (t, out) => { if (t.length >= 2) out.add(t.slice(-2)); },
  any2: (t, out) => { for (let i = 0; i + 2 <= t.length; i++) out.add(t.slice(i, i + 2)); },
};
// the keys each typed prefix needs, per scheme (w23 is a start-anchored key only for the prefix's own 2nd-3rd chars)
const need = { w12: new Set(), w123: new Set(), w23: new Set(), any2: new Set() };
for (const p of TYPED) {
  need.w12.add(p.slice(0, 2));
  if (p.length >= 3) { need.w123.add(p.slice(0, 3)); need.w23.add(p.slice(1, 3)); }
  for (let i = 0; i + 2 <= p.length; i++) need.any2.add(p.slice(i, i + 2));
}

const { engine, handle } = await openEngine();
const rows = engine.query(handle, "select count(*) as n from t").columns[0].values[0];
const df = Object.fromEntries(Object.keys(SCHEMES).map((s) => [s, new Map()]));
const masks = Object.fromEntries(Object.entries(need).map(([s, ks]) => [s, new Map([...ks].map((k) => [k, new Uint8Array(rows)]))]));
const truthPrefix = new Map(TYPED.map((p) => [p, 0])), truthSub = new Map(TYPED.map((p) => [p, 0]));
const cache = [null, new Map(), new Map(), new Map(), null];
const CHUNK = 65536;
const t0 = performance.now();
for (let off = 0; off < rows; off += CHUNK) {
  const r = engine.query(handle, `select ${COLS.map(ident).join(", ")} from t limit ${CHUNK} offset ${off}`);
  for (let i = 0; i < r.rowCount; i++) {
    const toks = new Set();
    for (let f = 0; f < COLS.length; f++) {
      const c = r.columns[f];
      if (c.validity && !((c.validity[i >> 3] >> (i & 7)) & 1)) continue;
      const s = c.offsets[i], e = c.offsets[i + 1];
      if (e === s) continue;
      let tk;
      if (cache[f]) {
        const key = Buffer.from(c.bytes.buffer, c.bytes.byteOffset + s, e - s).toString("latin1");
        tk = cache[f].get(key);
        if (!tk) { tk = []; tokenize(c.bytes, s, e, tk); cache[f].set(key, tk); }
      } else { tk = []; tokenize(c.bytes, s, e, tk); }
      for (const t of tk) toks.add(t);
    }
    for (const [name, fn] of Object.entries(SCHEMES)) {
      const keys = new Set();
      for (const t of toks) fn(t, keys);
      const d = df[name], m = masks[name];
      for (const k of keys) { d.set(k, (d.get(k) || 0) + 1); const a = m && m.get(k); if (a) a[off + i] = 1; }
    }
    for (const p of TYPED) {
      let pre = false, sub = false;
      for (const t of toks) { if (!pre && t.startsWith(p)) pre = true; if (!sub && t.includes(p)) sub = true; if (pre && sub) break; }
      if (pre) truthPrefix.set(p, truthPrefix.get(p) + 1);
      if (sub) truthSub.set(p, truthSub.get(p) + 1);
    }
  }
  process.stderr.write(`\r${Math.min(off + CHUNK, rows).toLocaleString()} rows`);
}
process.stderr.write("\n");

const BITMAP = Math.ceil(rows / 8);
const summary = {};
for (const [name, d] of Object.entries(df)) {
  const counts = [...d.values()];
  const size = (pred) => counts.filter(pred).reduce((a, n) => a + Math.min(n * 1.5, BITMAP), 0) / 1e6;
  summary[name] = {
    keys: d.size,
    keysUnder25pct: counts.filter((n) => n / rows < 0.25).length,
    rowsCoveredByTop10Keys: [...d].sort((a, b) => b[1] - a[1]).slice(0, 10).map(([k, n]) => `${k} ${(100 * n / rows).toFixed(0)}%`).join(", "),
    storageMB_all: +size(() => true).toFixed(1),
    storageMB_under25pct: +size((n) => n / rows < 0.25).toFixed(1),
  };
}
const pct = (n) => (100 * n / rows).toFixed(1) + "%";
const countOf = (a) => { let n = 0; for (let i = 0; i < a.length; i++) n += a[i]; return n; };
const andOf = (arrs) => { const acc = new Uint8Array(rows).fill(1); for (const a of arrs) for (let i = 0; i < rows; i++) acc[i] &= a[i]; return countOf(acc); };
const table = TYPED.map((p) => ({
  typed: p,
  wordPrefixRows: pct(truthPrefix.get(p)),
  w12: pct(masks.w12.get(p.slice(0, 2)).reduce((a, b) => a + b, 0)),
  w123: p.length >= 3 ? pct(countOf(masks.w123.get(p.slice(0, 3)))) : "",
  w12_and_w23: p.length >= 3 ? pct(andOf([masks.w12.get(p.slice(0, 2)), masks.w23.get(p.slice(1, 3))])) : "",
  substringRows: pct(truthSub.get(p)),
  any2_and: pct(andOf([...new Set(Array.from({ length: p.length - 1 }, (_, i) => p.slice(i, i + 2)))].map((k) => masks.any2.get(k)))),
}));
const out = { rows, elapsedMs: Math.round(performance.now() - t0), schemes: summary, typed: table };
writeFileSync(`${OUT}key-stats.json`, JSON.stringify(out, null, 2));
console.log(JSON.stringify(summary, null, 1));
console.log("\n| typed | rows w/ a word starting so | w12 mask | w123 mask | w12 ∧ w23 | rows containing it anywhere | AND of any2 masks |\n|---|---|---|---|---|---|---|");
for (const r of table) console.log(`| ${r.typed} | ${r.wordPrefixRows} | ${r.w12} | ${r.w123} | ${r.w12_and_w23} | ${r.substringRows} | ${r.any2_and} |`);
