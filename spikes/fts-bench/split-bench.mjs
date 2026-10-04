// grantnav split in two (facetful split), wasm in Node: when the app can
// work, against the whole image. One connection downloads the first part,
// then the second; each streams in with its search columns indexed as it
// arrives, and the second is attached (indexes merged) when it is in. The
// network is simulated at each file's brotli size. The attached table must
// answer search queries like the whole image.
//   node spikes/fts-bench/split-bench.mjs whole.facetful:MB first.facetful:MB second.facetful:MB [Mbps…]
// (MB: the file's brotli size)
import { readFileSync } from "node:fs";
import { COLS, ROOT, ident, lit } from "./common.mjs";

const { instantiate } = await import(`${ROOT}js/facetful/core.js`);
const wasm = readFileSync(`${ROOT}target/wasm32-unknown-unknown/release/facetful_wasm.wasm`);
const [whole, first, second] = process.argv.slice(2, 5).map((a) => {
  const [path, mb] = a.split(":");
  return { bytes: readFileSync(path), wire: Number(mb) * 1e6 };
});
const speeds = process.argv.slice(5).map(Number);
const QUERIES = ["mental health", "youth sport london", "climte chnage", "communty", "food bank"];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const s = (ms) => (ms / 1000).toFixed(2);

/** Column names from an image's header. */
function columnsOf(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const names = [];
  let p = 18;
  for (let i = 0, n = v.getUint16(16, true); i < n; i++) {
    const len = v.getUint16(p, true);
    names.push(new TextDecoder().decode(bytes.subarray(p + 2, p + 2 + len)));
    p += 2 + len + 3;
  }
  return names;
}

/** `file`'s bytes in 64 KB chunks, released as a download that starts at
 *  `start` (ms) at `mbps` delivers them. */
async function* arriving(file, mbps, start) {
  const rawPerMs = (mbps * 1e6) / 8 / 1000 * (file.bytes.length / file.wire);
  for (let at = 0; at < file.bytes.length; at += 65536) {
    const end = Math.min(at + 65536, file.bytes.length);
    const wait = start + end / rawPerMs - performance.now();
    if (wait > 0) await sleep(wait);
    yield file.bytes.subarray(at, end);
  }
}

const counts = (engine, h) =>
  QUERIES.map((q) => engine.query(h, `select count(*) as n from t where search(${lit(q)}, ${COLS.map(ident).join(", ")})`).columns[0].values[0]);
const firstCols = COLS.filter((c) => columnsOf(first.bytes).includes(c));
const secondCols = COLS.filter((c) => columnsOf(second.bytes).includes(c));
console.log(`first part searches ${firstCols.join(", ")}; the second adds ${secondCols.join(", ")}`);

for (const mbps of speeds.length ? speeds : [20, 100]) {
  // whole: one image, every search column indexed as it arrives
  let e = await instantiate(wasm);
  let t0 = performance.now();
  const w = await e.openStream(arriving(whole, mbps, t0), { searchColumns: COLS });
  e.stepSearchIndex(w.handle, 0xffffffff);
  const wReady = performance.now() - t0;
  const want = counts(e, w.handle);
  // split: the first part, then the second, attached
  e = await instantiate(wasm);
  t0 = performance.now();
  const a = await e.openStream(arriving(first, mbps, t0), { searchColumns: firstCols });
  e.stepSearchIndex(a.handle, 0xffffffff);
  const aReady = performance.now() - t0;
  const b = await e.openStream(arriving(second, mbps, performance.now()), { searchColumns: secondCols });
  e.attach(a.handle, b.handle);
  const allReady = performance.now() - t0;
  const got = counts(e, a.handle);
  if (JSON.stringify(got) !== JSON.stringify(want)) throw new Error(`search over the attached parts: ${got} against ${want}`);
  console.log(
    `${mbps} Mbps: whole image, table and search ready ${s(wReady)} s | split: first part ready ${s(aReady)} s, second attached ${s(allReady)} s; wasm ${(e.w.memory.buffer.byteLength / 1e6).toFixed(0)} MB`,
  );
}
