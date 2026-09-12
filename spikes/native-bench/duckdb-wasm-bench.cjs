// DuckDB-WASM lane: blocking Node bindings (single-threaded wasm, like the
// browser). Loads the real CSV, runs the canonical suite, prints name\tms.
const path = require("path");
const fs = require("fs");

const ROOT = path.join(__dirname, "../..");
const DIST = path.join(ROOT, "web/spike-bench/node_modules/@duckdb/duckdb-wasm/dist");
const duckdb = require(path.join(DIST, "duckdb-node-blocking.cjs"));

function tweak(sql) {
  return sql.replace(/ like /g, " ilike ").replace(/as int\)/g, "as bigint)");
}

async function main() {
  const logger = new duckdb.VoidLogger();
  const db = await duckdb.createDuckDB(
    { mvp: { mainModule: path.join(DIST, "duckdb-mvp.wasm") },
      eh: { mainModule: path.join(DIST, "duckdb-eh.wasm") } },
    logger,
    duckdb.NODE_RUNTIME
  );
  await db.instantiate();
  db.registerFileBuffer("units.csv", fs.readFileSync(path.join(ROOT, "data/units-2026-08.csv")));
  const conn = db.connect();
  const t0 = performance.now();
  conn.query("create table t as from read_csv('units.csv', sample_size=-1)");
  console.error(`load+create: ${(performance.now() - t0).toFixed(0)} ms`);

  const text = fs.readFileSync(path.join(ROOT, "bench/queries.sql"), "utf8");
  for (const chunk of text.split("#").slice(1)) {
    const nl = chunk.indexOf("\n");
    const name = chunk.slice(0, nl).trim();
    const sql = tweak(chunk.slice(nl).trim().replace(/\s+/g, " "));
    if (!sql) continue;
    for (let i = 0; i < 3; i++) conn.query(sql); // warmup
    const times = [];
    for (let i = 0; i < 10; i++) {
      const s = performance.now();
      conn.query(sql);
      times.push(performance.now() - s);
    }
    times.sort((a, b) => a - b);
    console.log(`${name}\t${times[5].toFixed(2)}`);
  }
}

main().catch((e) => { console.error(e); process.exit(1); });
