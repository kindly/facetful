// Module worker hosting the in-worker lanes. The DuckDB reference lane runs on
// the main thread (it manages its own worker); the parquet/hyparquet lane runs
// here on request. Reports per-lane load + interaction timings.
import {
  laneFacetful, laneJsObjects, laneCrossfilter, laneHyparquet,
  makeScript, runScript, sameResult,
} from "./lanes.js";
import * as hyparquet from "./node_modules/hyparquet/src/index.js";

const status = (m) => postMessage({ type: "status", m });
let facDicts = null;
let facLane = null;

self.onmessage = async (e) => {
  try {
    if (e.data.cmd === "run") {
      status("fetching assets…");
      const [wasmRes, fileRes, csvRes, cfRes] = await Promise.all([
        fetch("/target/wasm32-unknown-unknown/release/facetful_wasm.wasm"),
        fetch("/spikes/facet-spike/data-200000.facetful"),
        fetch("/spikes/facet-spike/data-200000.csv"),
        fetch("/web/spike-bench/node_modules/crossfilter2/crossfilter.min.js"),
      ]);
      const wasmBytes = await wasmRes.arrayBuffer();
      const fileBytes = await fileRes.arrayBuffer();
      const csvText = await csvRes.text();
      (0, eval)(await cfRes.text()); // UMD -> self.crossfilter

      status("loading lanes…");
      const fac = await laneFacetful(wasmBytes, fileBytes);
      facLane = fac;
      facDicts = fac.dicts;
      const jso = laneJsObjects(csvText);
      const cfl = await laneCrossfilter(csvText, self.crossfilter);

      const script = makeScript(fac.dicts, 140);

      status("verifying correctness…");
      for (const i of [0, 7, 60, 119]) {
        const a = fac.interact(script[i]);
        let err = sameResult(a, fac.dicts, jso.interact(script[i]), jso.dicts);
        if (err) throw new Error(`facetful vs js-objects @${i}: ${err}`);
        err = sameResult(a, fac.dicts, cfl.interact(script[i]), cfl.dicts);
        if (err) throw new Error(`facetful vs crossfilter @${i}: ${err}`);
      }

      const results = [];
      for (const lane of [jso, cfl, fac]) {
        status(`benchmarking ${lane.name}…`);
        await new Promise((r) => setTimeout(r, 30));
        const r = runScript(lane, script);
        results.push({
          name: r.name,
          loadMs: lane.loadMs,
          median: r.median,
          p95: r.p95,
          lastPass: r.last.pass,
          lastSum: r.last.sum,
        });
      }
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
      status("hyparquet lane: decoding parquet…");
      const lane = await laneHyparquet(e.data.parquet, hyparquet);
      const script = makeScript(facDicts, 140);
      // verify against facetful before timing
      if (facLane) {
        const a = facLane.interact(script[60]);
        const err = sameResult(a, facDicts, lane.interact(script[60]), lane.dicts);
        if (err) throw new Error(`facetful vs hyparquet @60: ${err}`);
      }
      status("benchmarking hyparquet lane…");
      const r = runScript(lane, script);
      postMessage({
        type: "hyparquet-done",
        result: {
          name: r.name,
          loadMs: lane.loadMs,
          median: r.median,
          p95: r.p95,
          lastPass: r.last.pass,
          lastSum: r.last.sum,
        },
      });
    }
  } catch (err) {
    postMessage({ type: "error", m: String(err.stack || err) });
  }
};
