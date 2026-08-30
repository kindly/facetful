// Synthetic facet-shaped CSV, same seeded generator as spikes/lang-bench.
import { writeFileSync } from "node:fs";
import { makeDataset, DIMS } from "../lang-bench/js/kernels.js";

const N = parseInt(process.argv[2] || "200000", 10);
const { codes, measure } = makeDataset(N);
// Human-ish dictionary values per dim so the CSV looks like real facet data.
const dictFor = (name, card) => Array.from({ length: card }, (_, i) => `${name}_${i}`);
const dicts = DIMS.map((d) => dictFor(d.name, d.card));

const out = [];
out.push([...DIMS.map((d) => d.name), "capacity", "id"].join(","));
for (let i = 0; i < N; i++) {
  const row = DIMS.map((d, k) => dicts[k][codes[k][i]]);
  row.push(measure[i].toFixed(3), String(i));
  out.push(row.join(","));
}
writeFileSync(new URL(`./data-${N}.csv`, import.meta.url), out.join("\n") + "\n");
console.log(`data-${N}.csv written`);
