// search() prototype (steps 1-2) against per-term LIKE on grantnav.
//   node spikes/fts-bench/search-bench.mjs <engine.wasm>
// Typing runs with the mask cache on, from an empty cache per sequence.
import { readFileSync } from "node:fs";
import { IMAGE, COLS, ident, lit, median } from "./common.mjs";

const { instantiate } = await import(new URL("../../js/facetful/core.js", import.meta.url).href);
const engine = await instantiate(readFileSync(process.argv[2]));
const { handle } = engine.openTable(readFileSync(IMAGE));
const count = (where) => engine.query(handle, `select count(*) as n from t where ${where}`).columns[0].values[0];
const anyCol = (pat) => `(${COLS.map((c) => `${ident(c)} like ${lit(pat)}`).join(" or ")})`;
const likeForm = (q) => q.split(/\s+/).filter(Boolean).map((t) => anyCol(`%${t}%`)).join(" and ") || "1 = 1";
const searchForm = (q) => `search(${lit(q)}, ${COLS.map(ident).join(", ")})`;
const reset = () => { engine.setMaskBudget(handle, 0); engine.setMaskBudget(handle, 64 << 20); };
const f1 = (x) => x.toFixed(1);

console.log("## cold whole queries (mask cache off, median of 7)\n\n| query | LIKE ms | search ms | rows (both) |\n|---|---|---|---|");
engine.setMaskBudget(handle, 0);
for (const q of ["climate", "mental health", "youth sport london", "community project"]) {
  const a = count(likeForm(q)), b = count(searchForm(q));
  if (a !== b) throw new Error(`${q}: LIKE ${a} vs search ${b}`);
  console.log(`| \`${q}\` | ${f1(median(() => count(likeForm(q))))} | ${f1(median(() => count(searchForm(q))))} | ${a.toLocaleString()} |`);
}

console.log("\n## typing, mask cache on (ms per keystroke; ␣ = space)\n");
for (const q of ["mental health", "youth sport london", "climate change adaptation"]) {
  for (const [name, form] of [["LIKE  ", likeForm], ["search", searchForm]]) {
    reset();
    const ts = [];
    let total = 0;
    for (let i = 1; i <= q.length; i++) {
      const typed = q.slice(0, i);
      const s = performance.now(); count(form(typed)); const ms = performance.now() - s;
      total += ms;
      ts.push(`${typed.endsWith(" ") ? "␣" : typed.at(-1)} ${f1(ms)}`);
    }
    console.log(`- ${name} \`${q}\` (total ${f1(total)} ms): ${ts.join(", ")}`);
  }
}
