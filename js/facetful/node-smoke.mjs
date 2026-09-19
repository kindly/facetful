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

// ---- baseline compiler round-trip: columns in -> image -> queries ----------
// Mirrors what the worker's parquet path does after hyparquet decode.
{
  const enc = new TextEncoder();
  const strs = ["eu", "us", "eu", "asia", "us", "eu", "eu", "asia"];
  const offsets = new Uint32Array(strs.length + 1);
  let total = 0;
  const parts = strs.map((s) => enc.encode(s));
  parts.forEach((b, i) => { total += b.byteLength; offsets[i + 1] = total; });
  const bytes = new Uint8Array(total);
  parts.forEach((b, i) => bytes.set(b, offsets[i]));

  const img = engine.compileTable(8, [
    { name: "region", kind: "text", offsets, bytes },
    {
      name: "capacity", kind: "num",
      data: new Float64Array([1.5, 2.5, 0, 4.5, 5.5, 6.5, 7.5, 8.5]),
      validity: new Uint8Array([1, 1, 0, 1, 1, 1, 1, 1]), // row 2 null
    },
    { name: "year", kind: "num", isInt: true, data: new Float64Array([2000, 2001, 2000, 2002, 2001, 2000, 2002, 2001]) },
  ], { groupTarget: 5 }); // force two row groups
  const imageBytes = engine.imageBytes(img);
  if (imageBytes.length < 100 || dec.decode(imageBytes.subarray(0, 4)) !== "FCT1") {
    throw new Error("compiled image lacks magic");
  }
  const t2 = engine.openImage(img);
  if (t2.rows !== 8) throw new Error("compiled table rows");
  let rr = engine.query(t2.handle, "select region, count(*) as n, round(sum(capacity),1) as c from t group by region order by n desc");
  if (text(rr.columns[0], 0) !== "eu" || rr.columns[1].values[0] !== 4 || rr.columns[2].values[0] !== 15.5) {
    throw new Error("compiled table group-by wrong");
  }
  // null survived the compile (row 2 capacity)
  rr = engine.query(t2.handle, "select count(*) as k from t where capacity is null");
  if (rr.columns[0].values[0] !== 1) throw new Error("compiled null lost");
  // int narrowing + pruning stats work on the compiled image
  rr = engine.query(t2.handle, "select count(*) from t where year > 2100");
  if (rr.stats.totalGroups !== 2 || rr.stats.scannedGroups !== 0) {
    throw new Error(`compiled pruning: ${JSON.stringify(rr.stats)}`);
  }
  console.log("compile round-trip: OK (image", imageBytes.length, "bytes, 2 groups)");

  // temporal columns through the compile ABI: days/ms in, date/timestamp kinds out
  const img2 = engine.compileTable(3, [
    { name: "d", kind: "num", temporal: "date", data: new Float64Array([18276, 18322, 18993]) },
    { name: "ts", kind: "num", temporal: "timestamp",
      data: new Float64Array([18276 * 86400000 + 37800000, 18322 * 86400000, 18993 * 86400000]) },
  ]);
  const t3 = engine.openImage(img2);
  let tr = engine.query(t3.handle, "select d, ts, year(d) as y, strftime('%H:%M', ts) as hm from t order by d limit 1");
  if (tr.columns[0].kind !== "date" || tr.columns[1].kind !== "timestamp") {
    throw new Error(`temporal kinds: ${tr.columns.map((c) => c.kind)}`);
  }
  if (tr.columns[0].values[0] !== 18276 || tr.columns[2].values[0] !== 2020) {
    throw new Error("temporal values wrong");
  }
  if (text(tr.columns[3], 0) !== "10:30") throw new Error("strftime wrong");
  tr = engine.query(t3.handle, "select count(*) from t where d >= date('2020-03-01')");
  if (tr.columns[0].values[0] !== 2) throw new Error("date literal compare wrong");
  console.log("temporal round-trip: OK");
}

// materialize: a grouped, ordered result becomes a table; types and the
// sort metadata survive; the derived table answers the same query
{
  const img = engine.materialize(handle, "select country, count(*) as n, round(sum(capacity), 1) as mw from t group by country order by mw desc");
  const derived = engine.openImage(img);
  const a = engine.query(derived.handle, "select country, n, mw from t order by mw desc limit 3");
  const b = engine.query(handle, "select country, count(*) as n, round(sum(capacity), 1) as mw from t group by country order by mw desc limit 3");
  for (let i = 0; i < 3; i++) {
    if (text(a.columns[0], i) !== text(b.columns[0], i) || a.columns[1].values[i] !== b.columns[1].values[i]
        || a.columns[2].values[i] !== b.columns[2].values[i]) throw new Error(`materialize row ${i} differs`);
  }
  if (a.columns[1].kind !== "int" || a.columns[2].kind !== "float") throw new Error(`materialize kinds: ${a.columns.map((c) => c.kind)}`);
  let bad = false;
  try { engine.materialize(handle, "select country, country from t"); } catch (e) { bad = e instanceof QueryError && /duplicate column/.test(e.message); }
  if (!bad) throw new Error("duplicate select names must be a QueryError");
  console.log(`materialize round-trip: OK (${derived.rows} rows)`);
}

// join: a materialized per-country dimension joined back onto the facts on a
// dictionary key; per-row n_rows equals that country's group count
{
  const dimImg = engine.materialize(handle, "select country, count(*) as n_rows from t group by country order by n_rows");
  const dim = engine.openImage(dimImg);
  const joined = engine.openImage(engine.join(handle, dim.handle, { on: "country", columns: ["n_rows"] }));
  const chk = engine.query(joined.handle, "select country, min(n_rows) as lo, max(n_rows) as hi, count(*) as n, sum(matched) as m from t group by country order by country limit 3");
  for (let i = 0; i < chk.rowCount; i++) {
    const [lo, hi, n, m] = [1, 2, 3, 4].map((c) => chk.columns[c].values[i]);
    if (lo !== n || hi !== n || m !== n) throw new Error(`join row ${i}: lo=${lo} hi=${hi} n=${n} matched=${m}`);
  }
  let rejected = false;
  try { engine.join(dim.handle, handle, { on: "country", columns: ["capacity"] }); } catch (e) { rejected = e instanceof QueryError && /not unique/.test(e.message); }
  if (!rejected) throw new Error("a non-unique right key must be a QueryError");
  console.log(`join round-trip: OK (${joined.rows} rows)`);
}

console.log("js protocol smoke: OK");
