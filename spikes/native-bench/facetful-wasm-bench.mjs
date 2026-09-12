// facetful wasm lane in Node (same V8 as the duckdb-wasm lane).
// Prints name\tcold_ms\twarm_ms; cold = mask cache disabled (budget 0).
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../../", import.meta.url));
const { instantiate } = await import(new URL("../../js/facetful/core.js", import.meta.url).href);
const engine = await instantiate(readFileSync(ROOT + "js/facetful/facetful_wasm.wasm"));
const { handle } = engine.openTable(readFileSync(ROOT + "data/units-2026-08.facetful"));

const text = readFileSync(ROOT + "bench/queries.sql", "utf8");
const median = (ts) => ts.sort((a, b) => a - b)[5];
for (const chunk of text.split("#").slice(1)) {
  const nl = chunk.indexOf("\n");
  const name = chunk.slice(0, nl).trim();
  const sql = chunk.slice(nl).trim().replace(/\s+/g, " ");
  if (!sql) continue;
  const run = () => {
    const s = performance.now();
    engine.query(handle, sql);
    return performance.now() - s;
  };
  for (let i = 0; i < 3; i++) run(); // warmup (also primes masks)
  const warm = median(Array.from({ length: 10 }, run));
  engine.setMaskBudget(handle, 0); // cold: no mask reuse possible
  const cold = median(Array.from({ length: 10 }, run));
  engine.setMaskBudget(handle, 16 << 20);
  console.log(`${name}\t${cold.toFixed(2)}\t${warm.toFixed(2)}`);
}
