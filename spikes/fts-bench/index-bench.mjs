// The search() word index in wasm on grantnav: build time, memory, and
// typing with the index against the scan path (same rows every time).
//   node spikes/fts-bench/index-bench.mjs <engine.wasm>
import { readFileSync } from "node:fs";
import { IMAGE, COLS, ident, lit, median } from "./common.mjs";

const { instantiate } = await import(new URL("../../js/facetful/core.js", import.meta.url).href);
const engine = await instantiate(readFileSync(process.argv[2]));
const { handle } = engine.openTable(readFileSync(IMAGE));
const memMB = () => engine.mem().byteLength / 1e6;
const count = (q) => engine.query(handle, `select count(*) as n from t where search(${lit(q)}, ${COLS.map(ident).join(", ")})`).columns[0].values[0];
const f1 = (x) => x.toFixed(1);

const m0 = memMB();
let t = performance.now();
const { pairs, bytes } = engine.buildSearchIndex(handle, COLS);
const buildMs = performance.now() - t;
console.log(`build ${f1(buildMs)} ms | ${pairs.toLocaleString()} pairs | index holds ${f1(bytes / 1e6)} MB | wasm memory ${f1(m0)} -> ${f1(memMB())} MB`);

const SEQ = ["mental health", "youth sport london", "climate change adaptation", "community project"];
console.log("\n## typing, ms per keystroke (mask cache on, fresh per sequence)\n");
const run = (label) => {
  for (const q of SEQ) {
    engine.setMaskBudget(handle, 0); engine.setMaskBudget(handle, 64 << 20);
    const ts = [], counts = [];
    let total = 0;
    for (let i = 1; i <= q.length; i++) {
      const typed = q.slice(0, i);
      const s = performance.now(); counts.push(count(typed)); const ms = performance.now() - s;
      total += ms;
      ts.push(`${typed.endsWith(" ") ? "␣" : typed.at(-1)} ${f1(ms)}`);
    }
    console.log(`- ${label} \`${q}\` (total ${f1(total)} ms): ${ts.join(", ")}`);
    run.counts[label + q] = counts;
  }
};
run.counts = {};
run("index");
engine.dropSearchIndex(handle);
run("scan ");
for (const q of SEQ) if (JSON.stringify(run.counts["index" + q]) !== JSON.stringify(run.counts["scan " + q])) throw new Error(`counts differ for ${q}`);
console.log("\ncounts identical on both paths for every keystroke");

console.log("\n## cold whole queries (mask cache off, median of 7)\n\n| query | rows | scan ms | index ms |\n|---|---|---|---|");
engine.setMaskBudget(handle, 0);
const scanT = {};
for (const q of ["climate", "mental health", "youth sport london", "support", "community project", "m", "a"]) scanT[q] = [count(q), median(() => count(q))];
engine.buildSearchIndex(handle, COLS);
for (const [q, [n, ms]] of Object.entries(scanT)) {
  if (count(q) !== n) throw new Error(`cold ${q} differs`);
  console.log(`| \`${q}\` | ${n.toLocaleString()} | ${f1(ms)} | ${f1(median(() => count(q)))} |`);
}
