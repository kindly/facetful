// The compiled-image correctness discipline, headless: transcode the real
// 200K Parquet through the SAME code the browser worker runs (hyparquet ->
// parquetToColumns -> wasm baseline compiler) and require query results to
// match the CLI-built .facetful image cell-for-cell.
import { readFileSync } from "node:fs";
import { instantiate } from "./core.js";
import { parquetToColumns, zstdCompressors } from "./parquet.js";

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

// path B: parquet -> browser baseline compiler, from the Snappy file and the
// same data written with ZSTD (the worker's lazy decompressor wiring)
const compressorsFor = async (codecs) => (codecs.has("ZSTD") ? zstdCompressors(await import("./fzstd.js")) : {});
async function transcode(file) {
  const pq = readFileSync(new URL(`spikes/facet-spike/${file}`, root));
  const t0 = performance.now();
  const { rows, columns } = await parquetToColumns(hp, pq.buffer.slice(pq.byteOffset, pq.byteOffset + pq.byteLength), { compressorsFor });
  const img = engine.compileTable(rows, columns);
  const ms = performance.now() - t0;
  const compiled = engine.openImage(img);
  if (compiled.rows !== cli.rows) throw new Error(`${file}: row counts differ: ${compiled.rows} vs ${cli.rows}`);
  return { file, compiled, ms };
}
const paths = [await transcode("data-200000.parquet"), await transcode("data-200000-zstd.parquet")];
const rows = cli.rows;

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

for (const { file, compiled } of paths) for (const sql of QUERIES) {
  const a = cells(engine.query(cli.handle, sql));
  const b = cells(engine.query(compiled.handle, sql));
  if (a.length !== b.length || a.some((v, i) => v !== b[i])) {
    console.error(`MISMATCH (${file}) on:`, sql);
    console.error("cli:     ", a.slice(0, 12).join(" | "));
    console.error("compiled:", b.slice(0, 12).join(" | "));
    process.exit(1);
  }
}
// column types: the common scalars keep their kind, everything else is text
// (spikes/facet-spike/types.parquet, written by DuckDB: 5 rows, i = 1..5)
{
  const pq = readFileSync(new URL("spikes/facet-spike/types.parquet", root));
  const { rows: n, columns } = await parquetToColumns(hp, pq.buffer.slice(pq.byteOffset, pq.byteOffset + pq.byteLength), { compressorsFor });
  const kinds = Object.fromEntries(columns.map((c) => [c.name, c.kind === "text" ? "text" : c.temporal ?? (c.isInt ? "int" : "float")]));
  const wantKinds = {
    i32: "int", i64: "int", b: "int", dbl: "float", dec2: "float", dec0: "int", s: "text", s_null: "text",
    d: "date", ts: "timestamp", ts_ns: "timestamp", u_s: "text", u: "text", lst: "text", slst: "text",
    st: "text", mp: "text", u64: "int", i_null: "int", tail: "text",
  };
  for (const [name, want] of Object.entries(wantKinds)) {
    if (kinds[name] !== want) throw new Error(`types.parquet: column ${name} is ${kinds[name]}, expected ${want}`);
  }
  const t = engine.openImage(engine.compileTable(n, columns));
  const got = cells(engine.query(t.handle,
    "select i32, i64, b, dbl, dec2, dec0, s, s_null, d, ts, ts_ns, u = u_s, lst, slst, st, mp, u64, i_null, tail from t where i32 = 3 or i32 = 4 order by i32"));
  const want = [
    "3", "30000000000", "0", "0.75", "3.75", "3", "name_3", "NULL", "19726", "1704114000000", "1704114000000", "1", "[3,4]", '["a3",null]', '{"a":3,"b":"q3"}', '{"k":3}', "3", "3", "tail_3",
    "4", "40000000000", "1", "1", "5", "4", "name_4", "x4", "19727", "1704117600000", "1704117600000", "1", "[4,5]", '["a4",null]', '{"a":4,"b":"q4"}', '{"k":4}', "4", "NULL", "tail_4",
  ];
  if (got.length !== want.length || got.some((v, i) => v !== want[i])) {
    console.error("MISMATCH (types.parquet)");
    console.error("got: ", got.join(" | "));
    console.error("want:", want.join(" | "));
    process.exit(1);
  }
}

console.log(
  `parquet-path differential: OK — ${QUERIES.length} queries agree cell-for-cell ` +
  `(${rows} rows; transcode in node ${paths.map((p) => `${p.file} ${p.ms.toFixed(0)} ms`).join(", ")})`,
);
