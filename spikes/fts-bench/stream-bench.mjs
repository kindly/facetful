// grantnav (wasm, Node): how soon search is ready when the word index is
// built while the image arrives, against downloading it, opening it, then
// building the index. The network is simulated: raw image bytes arrive at
// the rate a brotli download of the given speed delivers them (the image is
// 357 MB raw, 58 MB brotli).
//   node spikes/fts-bench/stream-bench.mjs [engine.wasm] [Mbps…]
import { readFileSync } from "node:fs";
import { IMAGE, COLS, ROOT } from "./common.mjs";

const { instantiate } = await import(`${ROOT}js/facetful/core.js`);
const wasm = readFileSync(process.argv[2] || `${ROOT}js/facetful/facetful_wasm.wasm`);
const speeds = process.argv.slice(3).map(Number);
const img = readFileSync(IMAGE);
const BROTLI = 58.0e6;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** The image in 64 KB chunks, each released when the simulated network has
 *  delivered it (start: performance.now() at the request). */
async function* arriving(mbps, start) {
  const rawPerMs = (mbps * 1e6) / 8 / 1000 * (img.length / BROTLI);
  for (let at = 0; at < img.length; at += 65536) {
    const end = Math.min(at + 65536, img.length);
    const due = start + end / rawPerMs;
    const wait = due - performance.now();
    if (wait > 0) await sleep(wait);
    yield img.subarray(at, end);
  }
}

for (const mbps of speeds.length ? speeds : [20, 100, 400]) {
  const download = (img.length / (img.length / BROTLI)) * 8 / (mbps * 1e6);
  // after: wait for every byte, open, build the index
  {
    const engine = await instantiate(wasm);
    const t0 = performance.now();
    const chunks = [];
    for await (const c of arriving(mbps, t0)) chunks.push(c);
    const bytes = Buffer.concat(chunks);
    const t1 = performance.now();
    const { handle } = engine.openTable(bytes);
    const t2 = performance.now();
    engine.buildSearchIndex(handle, COLS);
    const t3 = performance.now();
    console.log(`${mbps} Mbps (download ${download.toFixed(1)} s) after:  table ready ${((t2 - t0) / 1000).toFixed(2)} s, search ready ${((t3 - t0) / 1000).toFixed(2)} s (open ${(t2 - t1).toFixed(0)} ms, index ${(t3 - t2).toFixed(0)} ms), wasm ${(engine.w.memory.buffer.byteLength / 1e6).toFixed(0)} MB`);
  }
  // while: open and index as it arrives
  for (const expectedBytes of [img.length, 0]) {
    const engine = await instantiate(wasm);
    const t0 = performance.now();
    const r = await engine.openStream(arriving(mbps, t0), { expectedBytes, searchColumns: COLS });
    const t1 = performance.now();
    engine.stepSearchIndex(r.handle, 0xffffffff); // what the worker does in the background
    const t2 = performance.now();
    console.log(`${mbps} Mbps (download ${download.toFixed(1)} s) while${expectedBytes ? "" : " (size unknown)"}: table ready ${((t1 - t0) / 1000).toFixed(2)} s, search ready ${((t2 - t0) / 1000).toFixed(2)} s, ${r.indexedWhileLoading} groups indexed before the last byte, wasm ${(engine.w.memory.buffer.byteLength / 1e6).toFixed(0)} MB`);
  }
}
