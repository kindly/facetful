// Module worker hosting the in-worker lanes. The DuckDB reference lane runs on
// the main thread (it manages its own worker); the parquet/hyparquet lane runs
// here on request. Reports per-lane load + interaction timings.
import {
  laneFacetful, laneJsObjects, laneCrossfilter, laneHyparquet, laneHyparquetChunks,
  makeScript, runScript, sameResult,
} from "./lanes.js";
import * as hyparquet from "./node_modules/hyparquet/src/index.js";

const status = (m) => postMessage({ type: "status", m });
let facDicts = null;
let facLane = null;

self.onmessage = async (e) => {
  try {
    if (e.data.cmd === "run") {
      const n = e.data.n || 200000;
      status("fetching assets…");
      const [wasmRes, fileRes, csvRes, cfRes] = await Promise.all([
        fetch("/target/wasm32-unknown-unknown/release/facetful_wasm.wasm"),
        fetch(`/spikes/facet-spike/data-${n}.facetful`),
        fetch(`/spikes/facet-spike/data-${n}.csv`),
        fetch("/web/spike-bench/node_modules/crossfilter2/crossfilter.min.js"),
      ]);
      const wasmBytes = await wasmRes.arrayBuffer();
      const fileBytes = await fileRes.arrayBuffer();
      const csvText = await csvRes.text();
      (0, eval)(await cfRes.text()); // UMD -> self.crossfilter

      const heap = () => (performance.memory ? performance.memory.usedJSHeapSize : null);

      // facetful first (cold), kept alive as the correctness reference
      status("benchmarking .facetful → wasm…");
      const h0 = heap();
      const fac = await laneFacetful(wasmBytes, fileBytes);
      facLane = fac;
      facDicts = fac.dicts;
      const script = makeScript(fac.dicts, 140);
      const rFac = runScript(fac, script);
      const results = [{
        name: rFac.name, loadMs: fac.loadMs, firstMs: rFac.firstMs,
        median: rFac.median, p95: rFac.p95,
        heapMB: heap() !== null && h0 !== null ? (heap() - h0) / 1048576 : null,
        wasmMB: fac.wasmMemoryBytes ? fac.wasmMemoryBytes() / 1048576 : null,
      }];

      // other lanes: create -> bench -> verify vs facetful -> drop (heap honesty)
      const others = [
        ["JS objects", () => laneJsObjects(csvText)],
        ["crossfilter2", async () => laneCrossfilter(csvText, self.crossfilter)],
      ];
      for (const [label, make] of others) {
        status(`benchmarking ${label}…`);
        await new Promise((r) => setTimeout(r, 30));
        const hBefore = heap();
        let lane = await make();
        const r = runScript(lane, script);
        const hAfter = heap();
        for (const i of [0, 60, 119]) {
          const err = sameResult(fac.interact(script[i]), fac.dicts, lane.interact(script[i]), lane.dicts);
          if (err) throw new Error(`facetful vs ${label} @${i}: ${err}`);
        }
        results.push({
          name: r.name, loadMs: lane.loadMs, firstMs: r.firstMs,
          median: r.median, p95: r.p95,
          heapMB: hAfter !== null && hBefore !== null ? (hAfter - hBefore) / 1048576 : null,
        });
        lane = null;
      }
      // stable presentation order: JS objects, crossfilter, facetful
      results.push(results.shift());
      postMessage({
        type: "lanes-done",
        results,
        dicts: fac.dicts,
        meta: {
          rows: fac.meta.rows,
          wasmBytes: wasmBytes.byteLength,
          facetfulBytes: fileBytes.byteLength,
          csvBytes: csvText.length,
          ua: navigator.userAgent,
        },
      });
    }

    if (e.data.cmd === "hyparquet") {
      const script = makeScript(facDicts, 140);
      const out = [];
      const heap = () => (performance.memory ? performance.memory.usedJSHeapSize : null);
      for (const [label, maker] of [
        ["objects", laneHyparquet],
        ["column chunks", laneHyparquetChunks],
      ]) {
        status(`hyparquet (${label}): decoding parquet…`);
        const hBefore = heap();
        let lane = await maker(e.data.parquet.slice(0), hyparquet);
        status(`benchmarking hyparquet (${label})…`);
        const r = runScript(lane, script);
        const hAfter = heap();
        if (facLane) {
          const a = facLane.interact(script[60]);
          const err = sameResult(a, facDicts, lane.interact(script[60]), lane.dicts);
          if (err) throw new Error(`facetful vs hyparquet (${label}) @60: ${err}`);
        }
        out.push({
          name: r.name, loadMs: lane.loadMs, firstMs: r.firstMs, median: r.median, p95: r.p95,
          heapMB: hAfter !== null && hBefore !== null ? (hAfter - hBefore) / 1048576 : null,
        });
        lane = null;
      }
      postMessage({ type: "hyparquet-done", results: out });
    }
  } catch (err) {
    postMessage({ type: "error", m: (err && err.message ? err.message : String(err)) + "\n" + (err && err.stack || "") });
  }
};
