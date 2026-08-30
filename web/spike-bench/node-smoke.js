// Correctness gate + rough V8 timings for the three lanes.
// Browser numbers come from index.html; this must pass before serving anything.
import { readFileSync } from "node:fs";
import crossfilter from "crossfilter2";
import {
  laneFacetful, laneJsObjects, laneCrossfilter,
  makeScript, runScript, sameResult,
} from "./lanes.js";

const root = new URL("../../", import.meta.url);
const wasmBytes = readFileSync(new URL("target/wasm32-unknown-unknown/release/facetful_wasm.wasm", root));
const fileBytes = readFileSync(new URL("spikes/facet-spike/data-200000.facetful", root));
const csvText = readFileSync(new URL("spikes/facet-spike/data-200000.csv", root), "utf8");

const fac = await laneFacetful(wasmBytes, fileBytes.buffer.slice(fileBytes.byteOffset, fileBytes.byteOffset + fileBytes.byteLength));
const jso = laneJsObjects(csvText);
const cfl = await laneCrossfilter(csvText, crossfilter);

console.log(`loaded: facetful ${fac.loadMs.toFixed(0)}ms (${fac.meta.rows} rows), js-objects ${jso.loadMs.toFixed(0)}ms, crossfilter ${cfl.loadMs.toFixed(0)}ms`);

const script = makeScript(fac.dicts, 140);

// correctness on a sample of interactions
for (const i of [0, 3, 7, 20, 60, 119]) {
  const a = fac.interact(script[i]);
  const b = jso.interact(script[i]);
  const c = cfl.interact(script[i]);
  let err = sameResult(a, fac.dicts, b, jso.dicts);
  if (err) throw new Error(`facetful vs js-objects @${i}: ${err}`);
  err = sameResult(a, fac.dicts, c, cfl.dicts);
  if (err) throw new Error(`facetful vs crossfilter @${i}: ${err}`);
}
console.log("correctness: all three lanes agree (counts, totals) on sampled interactions");

for (const lane of [jso, cfl, fac]) {
  const r = runScript(lane, script);
  console.log(`${r.name.padEnd(42)} load ${lane.loadMs.toFixed(0).padStart(5)}ms  median ${r.median.toFixed(2).padStart(7)}ms  p95 ${r.p95.toFixed(2).padStart(7)}ms`);
}
