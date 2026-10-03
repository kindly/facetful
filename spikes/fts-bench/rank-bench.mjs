// search_rank on grantnav (wasm): per keystroke, the top 20 rows ordered by
// field weights → exact → typo → amount, against the same query ordered by
// amount alone. Shows the top titles once the query is typed.
//   node spikes/fts-bench/rank-bench.mjs [engine.wasm]
import { readFileSync } from "node:fs";
import { IMAGE, COLS, ROOT, ident, lit } from "./common.mjs";

const { instantiate } = await import(`${ROOT}js/facetful/core.js`);
const engine = await instantiate(readFileSync(process.argv[2] || `${ROOT}js/facetful/facetful_wasm.wasm`));
const { handle } = engine.openTable(readFileSync(IMAGE));
engine.buildSearchIndex(handle, COLS);
const WEIGHTS = [1, 3, 3, 5, 5];
const cols = COLS.map(ident).join(", ");
const ranked = COLS.map((c, i) => `${ident(c)}, ${WEIGHTS[i]}`).join(", ");
const AMOUNT = ident("Amount Awarded");
const sql = (q, rank) =>
  `select "Title", ${ident("Recipient Org:Name")} as org from t where search(${lit(q)}, ${cols}) ` +
  `order by ${rank ? `search_rank(${lit(q)}, ${ranked}), ` : ""}${AMOUNT} desc limit 20`;

const dec = new TextDecoder();
// one text cell of a result column (plain: offsets + bytes; dictionary: codes + values)
const text = (c, i) =>
  c.offsets ? dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1])) : c.dict ? c.dict[c.codes[i]] : String(c.values?.[i]);
const median = (ts) => [...ts].sort((a, b) => a - b)[ts.length >> 1];
for (const q of ["mental health", "youth sport london", "climte chnage", "comunity project", "food bank", "arts council"]) {
  const t = { plain: [], rank: [] };
  for (const rank of [false, true]) {
    engine.setMaskBudget(handle, 0); engine.setMaskBudget(handle, 64 << 20); // fresh cache per sequence
    for (let i = 1; i <= q.length; i++) {
      const s = performance.now();
      engine.query(handle, sql(q.slice(0, i), rank));
      t[rank ? "rank" : "plain"].push(performance.now() - s);
    }
  }
  console.log(`\n'${q}': by amount median ${median(t.plain).toFixed(1)} / max ${Math.max(...t.plain).toFixed(1)} ms; ` +
    `ranked median ${median(t.rank).toFixed(1)} / max ${Math.max(...t.rank).toFixed(1)} ms per keystroke`);
  const r = engine.query(handle, sql(q + " ", true));
  for (let i = 0; i < Math.min(5, r.rowCount); i++) console.log(`  ${r.columns.map((c) => text(c, i)).join(" | ")}`);
}
