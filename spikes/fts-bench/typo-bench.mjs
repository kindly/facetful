// search() with typos on grantnav, through the word index (wasm): rows and
// time against the correct spelling, mask cache off.
//   node spikes/fts-bench/typo-bench.mjs [engine.wasm]
import { readFileSync } from "node:fs";
import { IMAGE, COLS, ident, lit, median, ROOT } from "./common.mjs";

const { instantiate } = await import(`${ROOT}js/facetful/core.js`);
const engine = await instantiate(readFileSync(process.argv[2] || `${ROOT}js/facetful/facetful_wasm.wasm`));
const { handle } = engine.openTable(readFileSync(IMAGE));
engine.buildSearchIndex(handle, COLS);
engine.setMaskBudget(handle, 0);
const count = (q) => engine.query(handle, `select count(*) as n from t where search(${lit(q)}, ${COLS.map(ident).join(", ")})`).columns[0].values[0];
console.log("| typed | rows | ms | spelled right | rows | ms |\n|---|---|---|---|---|---|");
for (const [typo, right] of [
  ["helth ", "health "], ["enviromental helth ", "environmental health "], ["yuoth sprot londn ", "youth sport london "],
  ["mental helth servics ", "mental health services "], ["climte chnage ", "climate change "], ["comunity projcet ", "community project "],
  ["chairty ", "charity "], ["heath ", "heath "], ["youth londn", "youth london"],
]) {
  const a = count(typo), b = count(right);
  console.log(`| \`${typo}\` | ${a.toLocaleString()} | ${median(() => count(typo)).toFixed(1)} | \`${right}\` | ${b.toLocaleString()} | ${median(() => count(right)).toFixed(1)} |`);
}
