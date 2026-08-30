// Writes sizes.json: gzipped transfer sizes per lane's assets, used by the page
// for estimated cold-load-at-network-profile columns.
import { readFileSync, writeFileSync, readdirSync, existsSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const root = new URL("../../", import.meta.url);
const gz = (p) => gzipSync(readFileSync(p), { level: 9 }).length;

const dd = (f) => require.resolve(`@duckdb/duckdb-wasm/dist/${f}`);
const hySrc = new URL("./node_modules/hyparquet/src/", import.meta.url);
const hyGz = readdirSync(hySrc)
  .filter((f) => f.endsWith(".js"))
  .reduce((a, f) => a + gz(new URL(f, hySrc)), 0);

const parquetPath = new URL("spikes/facet-spike/data-200000.parquet", root);
const sizes = {
  csvGz: gz(new URL("spikes/facet-spike/data-200000.csv", root)),
  facetfulGz: gz(new URL("spikes/facet-spike/data-200000.facetful", root)),
  engineWasmGz: gz(new URL("target/wasm32-unknown-unknown/release/facetful_wasm.wasm", root)),
  crossfilterGz: gz(new URL("./node_modules/crossfilter2/crossfilter.min.js", import.meta.url)),
  hyparquetGz: hyGz,
  parquetGz: existsSync(parquetPath) ? gz(parquetPath) : null,
  duckdbGz: gz(dd("duckdb-eh.wasm")) + gz(dd("duckdb-browser.mjs")) + gz(dd("duckdb-browser-eh.worker.js")),
};
writeFileSync(new URL("./sizes.json", import.meta.url), JSON.stringify(sizes, null, 2));
console.log(sizes);
