// Drives core.js directly (no worker) against the 200K spike image:
// protocol correctness incl. error path, nulls, text marshalling.
import { readFileSync } from "node:fs";
import { instantiate, QueryError } from "./core.js";

const root = new URL("../../", import.meta.url);
const engine = await instantiate(readFileSync(new URL("target/wasm32-unknown-unknown/release/facetful_wasm.wasm", root)));
const { handle, rows } = engine.openTable(readFileSync(new URL("spikes/facet-spike/data-200000.facetful", root)));
console.log(`opened: ${rows} rows`);

// text + numbers + group by
let r = engine.query(handle, "select country, count(*) as n, round(sum(capacity),1) as total from t group by country order by n desc limit 3");
const dec = new TextDecoder();
const text = (c, i) => dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1]));
console.log("cols:", r.columns.map((c) => `${c.name}:${c.kind}`).join(", "), "| rows:", r.rowCount);
console.log("row0:", text(r.columns[0], 0), r.columns[1].values[0], r.columns[2].values[0]);
if (r.columns[0].kind !== "text" || r.columns[1].kind !== "int" || r.columns[2].kind !== "float") throw new Error("kind mismatch");
if (r.rowCount !== 3) throw new Error("rowCount");

// nulls cross the boundary via validity
r = engine.query(handle, "select capacity from t where capacity is null limit 5");
const v = r.columns[0].validity;
if (r.rowCount !== 5 || (v[0] & 0b11111) !== 0) throw new Error("null validity marshalling");

// pruning stats visible
r = engine.query(handle, "select count(*) from t where id > 999999999");
if (r.stats.scannedGroups !== 0 || r.stats.totalGroups !== 4) throw new Error(`stats: ${JSON.stringify(r.stats)}`);

// error path: rendered diagnostic with hint
try {
  engine.query(handle, "select contry from t");
  throw new Error("expected a QueryError");
} catch (e) {
  if (!(e instanceof QueryError) || !e.message.includes("did you mean 'country'?")) throw e;
  console.log("error path ok:", e.message.split("\n")[0]);
}
console.log("js protocol smoke: OK");
