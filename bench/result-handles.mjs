// Retained results against today's full query() (docs/result-handles.sv):
// prepare, sort and 512-row blocks of every column, on a real image.
//
//   cargo build --release --target wasm32-unknown-unknown -p facetful-wasm
//   node bench/result-handles.mjs <image.facetful> [--wasm path] [--sort "col" …] [--where "sql condition"]
//
// Runs the wasm through core.js in Node, as the worker does (results are
// copied out of wasm memory; the hop to the page transfers them, no copy).
import fs from "node:fs";
import { instantiate } from "../js/facetful/core.js";

const args = process.argv.slice(2);
const file = args.shift();
const opt = { wasm: "target/wasm32-unknown-unknown/release/facetful_wasm.wasm", sort: [], where: null };
while (args.length) {
  const k = args.shift().replace(/^--/, "");
  if (k === "sort") opt.sort.push(args.shift());
  else opt[k] = args.shift();
}

const BLOCK = 512;
const ms = (t) => (t < 10 ? t.toFixed(2) : t.toFixed(0));
const now = () => performance.now();
const median = (xs) => [...xs].sort((a, b) => a - b)[xs.length >> 1];
const time = (f) => {
  const t = now();
  const r = f();
  return [now() - t, r];
};
/** median ms of `f(i)` over `n` runs, after one warm-up */
const med = (n, f) => {
  f(0);
  return median(Array.from({ length: n }, (_, i) => time(() => f(i))[0]));
};
const bytesOf = (r) =>
  r.columns.reduce((n, c) => {
    for (const b of [c.validity, c.values, c.offsets, c.bytes, c.codes, c.dict?.offsets, c.dict?.bytes]) n += b?.byteLength ?? 0;
    return n;
  }, 0);
const mb = (n) => (n / 1048576).toFixed(n < 10485760 ? 2 : 0) + " MB";
const q = (c) => `"${c.replace(/"/g, '""')}"`;
const out = (name, value, note = "") => console.log(`${name}\t${value}\t${note}`);

const engine = await instantiate(fs.readFileSync(opt.wasm), null);
const [loadMs, { handle: t, rows }] = time(() => engine.openTable(fs.readFileSync(file)));
const info = engine.describe(t);
const cols = info.columns.map((c) => c.name);
const big = info.columns.reduce((a, c) => (c.bytes > a.bytes ? c : a));
console.log(`# ${file}: ${rows} rows, ${info.groups} row groups, ${cols.length} columns; biggest column ${q(big.name)} ${mb(big.bytes)}`);
console.log(`# what\tms\tnote`);
out("load", ms(loadMs), `wasm memory ${mb(engine.mem().byteLength)}`);
const where = opt.where ? ` where ${opt.where}` : "";
const all = `select * from t${where}`;

// ---- the handle, measured first: a full query() result raises the wasm
// memory high-water mark, which would hide what the handle itself needs ----
const mem0 = engine.mem().byteLength;
let [prepMs, h] = time(() => engine.prepare(t, all));
out("prepare, unsorted", ms(prepMs), `${h.rows} rows, kind ${h.kind}`);
const n = h.rows;
const offsets = (k) => Array.from({ length: k }, (_, i) => Math.floor(((n - BLOCK) * i) / (k - 1)));
const at = offsets(21);
const block = (handle, i, o) => engine.preparedPage(handle, at[i % at.length], at[i % at.length] + BLOCK, { dictText: true, ...o });
let page = block(h.handle, 10);
out(`block of ${BLOCK}, unsorted, all columns`, ms(med(21, (i) => block(h.handle, i))), `${mb(bytesOf(page))} a block, ${page.stats.scannedGroups} of ${page.stats.totalGroups} row groups`);
out(`block of ${BLOCK}, unsorted, without ${q(big.name)}`, ms(med(21, (i) => block(h.handle, i, { columns: cols.filter((c) => c !== big.name) }))));

for (const by of opt.sort) {
  const [sortMs] = time(() => engine.preparedSort(h.handle, q(by)));
  const kind = info.columns.find((c) => c.name === by)?.kind;
  out(`sort by ${q(by)}`, ms(sortMs), `${kind}; wasm memory now ${mb(engine.mem().byteLength)}`);
  const [descMs] = time(() => engine.preparedSort(h.handle, `${q(by)} desc`));
  out(`sort by ${q(by)} desc`, ms(descMs));
  page = block(h.handle, 10);
  out(`block of ${BLOCK}, sorted by ${q(by)} desc, all columns`, ms(med(21, (i) => block(h.handle, i))), `${mb(bytesOf(page))} a block, ${page.stats.scannedGroups} of ${page.stats.totalGroups} row groups`);
  const runs = at.map((_, i) => time(() => block(h.handle, i))[0]);
  out(`  the slowest of those blocks`, ms(Math.max(...runs)));
  out(`block of ${BLOCK}, sorted by ${q(by)} desc, without ${q(big.name)}`, ms(med(21, (i) => block(h.handle, i, { columns: cols.filter((c) => c !== big.name) }))));
}
out("wasm memory with the handle", mb(engine.mem().byteLength), `${mb(engine.mem().byteLength - mem0)} more than after load (4 bytes a row is ${mb(4 * n)})`);
engine.preparedFree(h.handle);

// prepared with its ORDER BY, as a page opening on a sorted grid would
if (opt.sort.length) {
  [prepMs, h] = time(() => engine.prepare(t, `${all} order by ${q(opt.sort[0])} desc`));
  out(`prepare, order by ${q(opt.sort[0])} desc`, ms(prepMs));
  engine.preparedFree(h.handle);
}

// ---- today: the whole result, every column, copied out of the wasm ----
const full = (sql, label) => {
  const [t1, r] = time(() => engine.query(t, sql, { dictText: true }));
  out(label, ms(t1), `${mb(bytesOf(r))} copied out, wasm memory ${mb(engine.mem().byteLength)}`);
};
const without = cols.filter((c) => c !== big.name).map(q).join(", ");
full(`select ${without} from t${where}`, `query(), every column but ${q(big.name)}`);
for (const by of opt.sort) full(`select ${without} from t${where} order by ${q(by)} desc`, `query(), every column but ${q(big.name)}, order by ${q(by)} desc`);
full(all, "query(), every column");
for (const by of opt.sort) full(`${all} order by ${q(by)} desc`, `query(), every column, order by ${q(by)} desc`);
