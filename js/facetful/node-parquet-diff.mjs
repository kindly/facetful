// The compiled-image correctness discipline, headless: transcode the real
// 200K Parquet through the SAME code the browser worker runs (hyparquet ->
// parquetToColumns -> wasm baseline compiler) and require query results to
// match the CLI-built .facetful image cell-for-cell.
import { readFileSync } from "node:fs";
import { instantiate } from "./core.js";
import { parquetToColumns } from "./parquet.js";

const root = new URL("../../", import.meta.url);
const hp = await import(
  new URL("web/spike-bench/node_modules/hyparquet/src/index.js", root).href
);

const engine = await instantiate(
  readFileSync(new URL("target/wasm32-unknown-unknown/release/facetful_wasm.wasm", root)),
);

// path A: CLI-built image
const cli = engine.openTable(
  readFileSync(new URL("spikes/facet-spike/data-200000.facetful", root)),
);

// path B: parquet -> browser baseline compiler
const pq = readFileSync(new URL("spikes/facet-spike/data-200000.parquet", root));
const t0 = performance.now();
const { rows, columns } = await parquetToColumns(hp, pq.buffer.slice(pq.byteOffset, pq.byteOffset + pq.byteLength));
const img = engine.compileTable(rows, columns);
const transcodeMs = performance.now() - t0;
const compiled = engine.openImage(img);
if (compiled.rows !== cli.rows) throw new Error(`row counts differ: ${compiled.rows} vs ${cli.rows}`);

const QUERIES = [
  "select count(*), count(capacity), count(distinct country) from t",
  "select country, count(*) as n, round(sum(capacity), 1) as total from t group by country order by n desc, country limit 10",
  "select fuel, avg(capacity) as a, min(capacity) as lo, max(capacity) as hi from t group by fuel order by fuel",
  "select id from t where capacity is null order by id limit 20",
  "select count(*) from t where country like 'country_1%' and owner not like '%7'",
  "select capacity, id from t order by capacity, id limit 25",
  "select median(capacity), round(stddev(capacity), 6) from t where id < 5000",
];

const dec = new TextDecoder();
function cells(r) {
  const out = [];
  for (let i = 0; i < r.rowCount; i++) {
    for (const c of r.columns) {
      if ((c.validity[i >> 3] & (1 << (i & 7))) === 0) out.push("NULL");
      else if (c.kind === "text") out.push(dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1])));
      else out.push(String(c.values[i]));
    }
  }
  return out;
}

for (const sql of QUERIES) {
  const a = cells(engine.query(cli.handle, sql));
  const b = cells(engine.query(compiled.handle, sql));
  if (a.length !== b.length || a.some((v, i) => v !== b[i])) {
    console.error("MISMATCH on:", sql);
    console.error("cli:     ", a.slice(0, 12).join(" | "));
    console.error("compiled:", b.slice(0, 12).join(" | "));
    process.exit(1);
  }
}
console.log(
  `parquet-path differential: OK — ${QUERIES.length} queries agree cell-for-cell ` +
  `(${rows} rows, transcode ${transcodeMs.toFixed(0)} ms in node)`,
);
