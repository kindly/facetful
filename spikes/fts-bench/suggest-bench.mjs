// searchSuggest on grantnav (wasm): what it offers while typing, and how
// long each keystroke takes, with and without a facet filter.
//   node spikes/fts-bench/suggest-bench.mjs [engine.wasm]
import { readFileSync } from "node:fs";
import { IMAGE, COLS, ROOT } from "./common.mjs";

const { instantiate } = await import(`${ROOT}js/facetful/core.js`);
const engine = await instantiate(readFileSync(process.argv[2] || `${ROOT}js/facetful/facetful_wasm.wasm`));
const { handle } = engine.openTable(readFileSync(IMAGE));
engine.buildSearchIndex(handle, COLS);
const NE = `"Best Available Region (additional data)" = 'North East'`;
const show = (r) => r.completions.slice(0, 5).map((c) => `${c.word} ${c.rows.toLocaleString()}`).join(", ");

for (const where of ["", NE]) {
  console.log(`\n## ${where ? "Region = North East" : "no filter"}`);
  for (const q of ["mental health", "youth sport london", "climte chnage", "comunity projcet"]) {
    engine.setMaskBudget(handle, 0); engine.setMaskBudget(handle, 64 << 20); // fresh cache per sequence
    const ts = [];
    for (let i = 1; i <= q.length; i++) {
      const typed = q.slice(0, i);
      const t = performance.now();
      const r = engine.searchSuggest(handle, COLS, typed, { where, limit: 8, didYouMean: true });
      ts.push(performance.now() - t);
      if ([3, 5].includes(i) || typed.endsWith(" ") || i === q.length) {
        const dym = r.didYouMean ? ` | did you mean "${r.didYouMean.query}" ${r.didYouMean.rows.toLocaleString()}` : "";
        const fix = r.corrected.length ? ` | corrected ${r.corrected.map((c) => `${c.word}→${c.to.join("/")}`).join(" ")}` : "";
        console.log(`  '${typed}' rows ${r.rows.toLocaleString()} | ${show(r) || "(none)"}${fix}${dym}`);
      }
    }
    const s = [...ts].sort((a, b) => a - b);
    console.log(`  → '${q}': median ${s[s.length >> 1].toFixed(1)} ms, max ${s.at(-1).toFixed(1)} ms per keystroke`);
  }
}
