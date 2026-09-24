// Drives core.js directly (no worker) against the 200K spike image:
// protocol correctness incl. error path, nulls, text marshalling.
import { readFileSync } from "node:fs";
import { instantiate, QueryError, transferables } from "./core.js";

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

// SQL JOIN: the dimension is registered under a name and named in the query;
// the first query materializes the join, the second hits the cache
{
  const dimImg = engine.materialize(handle, "select country, count(*) as n_rows from t group by country");
  const dim = engine.openImage(dimImg);
  engine.catalogRegister("dim", dim.handle);
  const sql = "select t.country, d.n_rows, count(*) as n from t left join dim d on t.country = d.country group by t.country, d.n_rows order by n desc limit 3";
  const a = engine.query(handle, sql);
  const b = engine.query(handle, sql);
  if (a.rowCount !== 3 || a.columns[1].values[0] !== a.columns[2].values[0]) throw new Error("sql join: n_rows must equal the group count");
  if (JSON.stringify(Array.from(b.columns[2].values)) !== JSON.stringify(Array.from(a.columns[2].values))) throw new Error("sql join: cached rerun differs");
  let bad = false;
  try { engine.query(handle, "select count(*) from t join dim d on t.country > d.country"); } catch (e) { bad = e instanceof QueryError && /column equalities/.test(e.message); }
  if (!bad) throw new Error("a non-equality join condition must be a QueryError");
  // and a joined table persisted by name: materialize over a JOIN
  const jimg = engine.materialize(handle, "select t.country, t.capacity, d.n_rows from t left join dim d on t.country = d.country");
  const joined = engine.openImage(jimg);
  const r = engine.query(joined.handle, "select count(*) as n, count(n_rows) as m from t");
  if (r.columns[0].values[0] !== 200000 || r.columns[1].values[0] !== 200000) throw new Error("materialized join lost rows");
  console.log("sql join: OK (materialized join", joined.rows, "rows)");
}

// user-defined functions (design.sv d49): regexp() vectorized over lanes with a
// per-pattern RegExp cache, a per-row function, text out, NULLs, an error body
{
  engine.registerFunction("regexp", { params: ["text", "text"], returns: "bool" }, (() => {
    const cache = new Map();
    return (args, len, out) => {
      const [s, p] = args;
      const pat = p.values[0];
      let re = cache.get(pat);
      if (!re) cache.set(pat, (re = new RegExp(pat)));
      for (let i = 0; i < len; i++) out.values[i] = re.test(s.values[i]) ? 1 : 0;
    };
  })());
  engine.registerFunction("tag", { params: ["text", "int"], returns: "text", perRow: true }, (c, n) => `${c.toUpperCase()}#${n % 3}`);
  engine.registerFunction("half", { params: ["float"], returns: "float", perRow: true }, (x) => x / 2);
  engine.registerFunction("boom", { params: ["int"], returns: "int", perRow: true }, () => { throw new Error("kaboom"); });

  let r = engine.query(handle, "select count(*) as n from t where regexp(country, '^country_1[0-9]$')");
  const n1 = r.columns[0].values[0];
  r = engine.query(handle, "select count(*) as n from t where country like 'country_1_'");
  if (n1 !== r.columns[0].values[0] || n1 === 0) throw new Error(`regexp count ${n1} != between count ${r.columns[0].values[0]}`);
  // the same predicate again: served from the mask cache
  const t0 = performance.now();
  engine.query(handle, "select count(*) as n from t where regexp(country, '^country_1[0-9]$')");
  const cached = performance.now() - t0;
  // text out with a NULL-preserving strict path (capacity has NULLs; id never)
  r = engine.query(handle, "select tag(country, id) as g, count(*) as n from t where id < 6 group by g order by g");
  if (r.rowCount !== 6 || !/^COUNTRY_\d+#[0-2]$/.test(text(r.columns[0], 0))) throw new Error(`tag(): ${r.rowCount} rows, first ${text(r.columns[0], 0)}`);
  r = engine.query(handle, "select count(half(capacity)) as c, count(*) as n from t");
  const [c, total] = [r.columns[0].values[0], r.columns[1].values[0]];
  r = engine.query(handle, "select count(capacity) from t");
  if (c !== r.columns[0].values[0] || c === total) throw new Error("strict NULL handling: half() must be NULL where capacity is");
  let failed = false;
  try { engine.query(handle, "select boom(id) from t limit 1"); } catch (e) { failed = e instanceof QueryError && /kaboom/.test(e.message); }
  if (!failed) throw new Error("a throwing UDF must fail the query with its message");
  let typed = false;
  try { engine.query(handle, "select half(country) from t limit 1"); } catch (e) { typed = e instanceof QueryError && /argument 1 needs float/.test(e.message); }
  if (!typed) throw new Error("UDF argument types are checked at bind time");
  if (!engine.unregisterFunction("boom") || engine.unregisterFunction("boom")) throw new Error("unregister");
  console.log(`udf: OK (regexp matched ${n1} rows; cached rerun ${cached.toFixed(2)} ms)`);
}

// streaming CSV conversion through the wasm: chunked feeds, byte-identical to the CLI image
{
  const csv = readFileSync(new URL("spikes/facet-spike/data-200000.csv", root));
  const chunks = () => (async function* () { for (let i = 0; i < csv.byteLength; i += 700_001) yield csv.subarray(i, Math.min(i + 700_001, csv.byteLength)); })();
  const t0 = performance.now();
  const { bytes, rows, schema } = await engine.convertCsv(chunks);
  const ref = readFileSync(new URL("spikes/facet-spike/data-200000.facetful", root));
  if (rows !== 200000 || bytes.byteLength !== ref.byteLength || Buffer.compare(Buffer.from(bytes), ref) !== 0) throw new Error(`convertCsv: ${rows} rows, ${bytes.byteLength} bytes, differs from the CLI image`);
  if (schema.length !== 8 || schema[6].kind !== "float64") throw new Error(`convertCsv schema: ${JSON.stringify(schema)}`);
  let bad = false;
  try { await engine.convertCsv(() => (async function* () { yield new TextEncoder().encode("a,b\n1,2,3\n"); })()); } catch (e) { bad = /row 2 has 3 cells/.test(e.message); }
  if (!bad) throw new Error("convertCsv must report ragged rows");
  console.log(`csv convert: OK (${rows} rows, ${(performance.now() - t0).toFixed(0)} ms, byte-identical)`);
}

// the ready-made UDF module: JSON, Intl time zones and names, temporal long tail, Unicode, URLs
{
  const { udfs } = await import("./udfs.js");
  engine.unregisterFunction("regexp"); // the smoke's own copy above; the module's takes over
  for (const u of udfs) engine.registerFunction(u.name, u.signature, u.fn);
  const one = (sql) => { const r = engine.query(handle, sql + " from t limit 1"); const c = r.columns[0]; return c.kind === "text" ? text(c, 0) : c.values[0]; };
  const checks = [
    ["select json_extract('{\"a\":{\"b\":[10,20]}}', '$.a.b[1]')", "20"],
    ["select to_tz(timestamp('2024-07-01 12:00:00'), 'America/New_York')", "2024-07-01 08:00:00"],
    ["select date_trunc('quarter', timestamp('2024-08-15 10:30:00'))", Date.UTC(2024, 6, 1)],
    ["select date_add(date('2024-01-31'), 1, 'month')", Date.UTC(2024, 1, 29) / 86400000],
    ["select weekday(date('2024-09-19'))", 3],
    ["select quarter(date('2024-11-02'))", 4],
    ["select country_name('de')", "Germany"],
    ["select format_number(1234567.89, 'en-US:compact')", "1.2M"],
    ["select unaccent('Zürich São Tomé')", "Zurich Sao Tome"],
    ["select url_host('https://www.eia.gov/x?y=1')", "www.eia.gov"],
    ["select url_host('not a url')", undefined],
    ["select regexp('Coal Creek', '^coal', 'i')", 1],
    ["select regexp('Coal Creek', '^coal')", 0],
    ["select regexp_extract('Unit 12 of 30', '\\d+')", "12"],
    ["select regexp_extract('Unit 12 of 30', 'of (\\d+)', 1)", "30"],
    ["select regexp_extract('2024-07-01', '(?<y>\\d{4})-(?<m>\\d\\d)', 'm')", "07"],
    ["select regexp_extract('none', '\\d+')", undefined],
    ["select regexp_replace('a1b22c', '\\d+', '#')", "a#b#c"],
    ["select regexp_replace('2024-07-01', '(?<y>\\d{4})-(\\d\\d)-(\\d\\d)', '$3/$2/$<y>')", "01/07/2024"],
  ];
  for (const [sql, want] of checks) {
    const got = one(sql);
    const r = engine.query(handle, sql + " from t limit 1");
    const isNull = !((r.columns[0].validity[0]) & 1);
    if (want === undefined ? !isNull : got !== want) throw new Error(`${sql}: got ${isNull ? "NULL" : JSON.stringify(got)}, want ${JSON.stringify(want)}`);
  }
  // a text-returning UDF in GROUP BY, over a dictionary column (one call per dictionary)
  const r = engine.query(handle, "select unaccent(upper(country)) as c, count(*) as n from t group by c order by n desc limit 2");
  if (r.rowCount !== 2 || !/^COUNTRY_\d+$/.test(text(r.columns[0], 0))) throw new Error("udfs in GROUP BY");
  for (const u of udfs) engine.unregisterFunction(u.name);
  console.log(`udfs module: OK (${udfs.length} functions)`);
}

// dictionary results: a dict-backed text column as codes + compacted dictionary
{
  const sql = "select country, owner || '' as o, capacity from t where capacity > 3 order by id";
  const plain = engine.query(handle, sql);
  const d = engine.query(handle, sql, { dictText: true });
  const c = d.columns[0];
  if (!c.codes || c.codes.length !== d.rowCount || !c.dict || c.offsets) throw new Error("dictText: country should be codes + dict");
  if (d.columns[1].codes || !d.columns[1].offsets) throw new Error("dictText: a computed text column stays per-row text");
  const enc = engine.query(handle, "select owner || '' as o from t limit 20000", { dictText: "all" }).columns[0];
  if (!enc.codes || enc.dict.offsets.length - 1 > 1981 || enc.codes.BYTES_PER_ELEMENT !== 2) throw new Error('dictText "all": low-cardinality computed text is encoded');
  const uniq = engine.query(handle, "select country || '-' || id as u from t limit 20000", { dictText: "all" }).columns[0];
  if (uniq.codes || !uniq.offsets) throw new Error('dictText "all": unique text is left as text');
  const same = engine.query(handle, "select owner || '' as o from t limit 20000", { dictText: true }).columns[0];
  if (same.codes) throw new Error("dictText true: computed text stays per-row text");
  const dn = c.dict.offsets.length - 1;
  const strs = Array.from({ length: dn }, (_, k) => dec.decode(c.dict.bytes.subarray(c.dict.offsets[k], c.dict.offsets[k + 1])));
  const distinct = new Set();
  for (let i = 0; i < d.rowCount; i++) {
    const want = text(plain.columns[0], i);
    distinct.add(want);
    if (strs[c.codes[i]] !== want) throw new Error(`dictText: row ${i} decodes to ${strs[c.codes[i]]}, text path says ${want}`);
  }
  if (dn !== distinct.size) throw new Error(`dictText: dictionary has ${dn} entries for ${distinct.size} distinct values`);
  const one = engine.query(handle, "select country from t where country = 'country_7'", { dictText: true }).columns[0];
  if (one.dict.offsets.length !== 2 || one.codes.some((x) => x !== 0)) throw new Error("dictText: compaction to the one value present");
  const t = transferables(d);
  if (t.length !== 9) throw new Error(`dictText: ${t.length} transferables (3 validity + codes + dict offsets + dict bytes + text offsets + text bytes + float values)`);
  const bytesNow = (d.rowCount * 2) + c.dict.offsets.byteLength + c.dict.bytes.byteLength;
  const bytesText = plain.columns[0].offsets.byteLength + plain.columns[0].bytes.byteLength;
  console.log(`dict results: OK (${d.rowCount} rows, ${dn} values; ${(bytesText / 1e6).toFixed(1)} MB as text -> ${(bytesNow / 1e6).toFixed(2)} MB as codes)`);
}

console.log("js protocol smoke: OK");
