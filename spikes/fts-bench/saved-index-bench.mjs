// grantnav (wasm, Node): reading a kept search index back against building
// it. The OPFS read itself is not included (Node has none): the bytes are
// copied into wasm memory, as the worker's read does, then checked and used.
//   node spikes/fts-bench/saved-index-bench.mjs [image]
import { readFileSync } from "node:fs";
import { IMAGE, COLS, ROOT, ident, lit } from "./common.mjs";

const { instantiate } = await import(`${ROOT}js/facetful/core.js`);
const wasm = readFileSync(`${ROOT}target/wasm32-unknown-unknown/release/facetful_wasm.wasm`);
const img = readFileSync(process.argv[2] || IMAGE);
const mb = (e) => (e.w.memory.buffer.byteLength / 1e6).toFixed(0);
const q = `select count(*) as n from t where search(${lit("mental helth")}, ${COLS.map(ident).join(", ")})`;

let e = await instantiate(wasm);
let { handle } = e.openTable(img);
let t = performance.now();
e.buildSearchIndex(handle, COLS);
const build = performance.now() - t;
t = performance.now();
const saved = e.withSavedSearchIndex(handle, (v) => v.slice());
const save = performance.now() - t;
const want = e.query(handle, q).columns[0].values[0];
console.log(`build ${build.toFixed(0)} ms, wasm ${mb(e)} MB; saved ${(saved.byteLength / 1e6).toFixed(1)} MB in ${save.toFixed(0)} ms`);

for (let i = 0; i < 2; i++) {
  e = await instantiate(wasm);
  ({ handle } = e.openTable(img));
  const before = mb(e);
  t = performance.now();
  if (!e.loadSearchIndex(handle, COLS, saved.byteLength, (v) => v.set(saved))) throw new Error("refused");
  const load = performance.now() - t;
  if (e.query(handle, q).columns[0].values[0] !== want) throw new Error("answers differ");
  console.log(`read back ${load.toFixed(0)} ms (wasm ${before} MB before, ${mb(e)} MB after)`);
}
