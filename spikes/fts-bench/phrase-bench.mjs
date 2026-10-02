// Quoted words, phrases and expanded words on grantnav through the word
// index (wasm): rows and time, mask cache off. Also the scan path.
//   node spikes/fts-bench/phrase-bench.mjs [engine.wasm]
import { readFileSync } from "node:fs";
import { IMAGE, COLS, ident, lit, median, ROOT } from "./common.mjs";

const { instantiate } = await import(`${ROOT}js/facetful/core.js`);
const engine = await instantiate(readFileSync(process.argv[2] || `${ROOT}js/facetful/facetful_wasm.wasm`));
const { handle } = engine.openTable(readFileSync(IMAGE));
const count = (q, facet = "") => engine.query(handle, `select count(*) as n from t where search(${lit(q)}, ${COLS.map(ident).join(", ")})${facet}`).columns[0].values[0];
const QS = [
  ["fund "], ["\"fund\""], ["organization "], ["\"organization\""], ["mental health "], ["\"mental health\""], ["\"health mental\""],
  ["\"climate change\""], ["\"youth sport\""], ["\"mental health\" london"], ["\"mental hea"], ["\"lead training\""],
  ["\"mental health\"", ` and "Best Available Region (additional data)" = 'North East'`],
];
engine.setMaskBudget(handle, 0);
const scan = QS.map(([q, f = ""]) => [count(q, f), median(() => count(q, f), 3, 1)]);
engine.buildSearchIndex(handle, COLS);
console.log("| query | index rows | index ms | scan rows | scan ms |\n|---|---|---|---|---|");
QS.forEach(([q, f = ""], i) => {
  console.log(`| \`${q}\`${f ? " + North East" : ""} | ${count(q, f).toLocaleString()} | ${median(() => count(q, f), 5, 1).toFixed(1)} | ${scan[i][0].toLocaleString()} | ${scan[i][1].toFixed(1)} |`);
});
