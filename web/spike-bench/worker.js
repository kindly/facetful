// Module worker hosting all three lanes — everything runs off the main thread,
// exactly as the real engine would. Reports per-lane load + interaction timings.
import {
  laneFacetful, laneJsObjects, laneCrossfilter,
  makeScript, runScript, sameResult,
} from "./lanes.js";

const status = (m) => postMessage({ type: "status", m });

self.onmessage = async (e) => {
  if (e.data.cmd !== "run") return;
  try {
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
        assetBytes:
          lane === fac ? wasmBytes.byteLength + fileBytes.byteLength : csvText.length,
      });
    }
    postMessage({
      type: "done",
      results,
      meta: {
        rows: fac.meta.rows,
        wasmBytes: wasmBytes.byteLength,
        facetfulBytes: fileBytes.byteLength,
        csvBytes: csvText.length,
        ua: navigator.userAgent,
      },
    });
  } catch (err) {
    postMessage({ type: "error", m: String(err.stack || err) });
  }
};
