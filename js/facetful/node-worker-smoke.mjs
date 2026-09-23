// Drives worker.js in Node through its postMessage protocol. The other gates
// use core.js directly, which is how a worker-only bug (setTable calling
// itself, 0.4.0–0.5.0) shipped twice: nothing here talks to the wasm except
// through the worker's message handler.
import fs from "node:fs";
import { fileURLToPath } from "node:url";

const here = new URL(".", import.meta.url);
const spike = new URL("../../spikes/facet-spike/", import.meta.url);

// the worker's globals: `self`, `postMessage`, and a fetch that reads files
const pending = new Map();
globalThis.self = globalThis;
globalThis.postMessage = (msg) => {
  const done = pending.get(msg.id);
  pending.delete(msg.id);
  done(msg);
};
globalThis.fetch = async (url) => new Response(fs.readFileSync(fileURLToPath(url)));

await import("./worker.js");

let seq = 0;
const send = (msg) =>
  new Promise((resolve) => {
    const id = ++seq;
    pending.set(id, resolve);
    self.onmessage({ data: { id, ...msg } });
  });
const ok = async (msg) => {
  const r = await send(msg);
  if (!r.ok) throw new Error(`${msg.cmd}: ${r.error}`);
  return r;
};
const dec = new TextDecoder();
const text = (c, i) => dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1]));
const toBuffer = (u8) => u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);

await ok({ cmd: "init", wasmUrl: new URL("facetful_wasm.wasm", here).href });

// load: every table path goes through setTable, the 0.5.1 regression
const image = fs.readFileSync(new URL("data-200000.facetful", spike));
const loaded = await ok({ cmd: "load", name: "t", buffer: toBuffer(image) });
if (loaded.rows !== 200000) throw new Error(`load rows ${loaded.rows}`);

// query on the last table, with a ready-made function (registered on init)
let r = await ok({ cmd: "query", sql: "select count(*) as n from t where regexp(country, '^country_1[0-9]$')" });
if (r.result.columns[0].values[0] !== 25947) throw new Error(`regexp count ${r.result.columns[0].values[0]}`);

// materialize registers the derived table under its name; FROM finds it
const m = await ok({ cmd: "materialize", name: "by_country", table: "t", sql: "select country, count(*) as n from t group by country" });
if (m.rows !== 200) throw new Error(`materialize rows ${m.rows}`);
r = await ok({ cmd: "query", table: "by_country", sql: "select count(*) as n from by_country" });
if (r.result.columns[0].values[0] !== 200) throw new Error("query on materialized table");

// a join across two registered tables, from the first table's query
r = await ok({ cmd: "query", table: "t", sql: "select count(*) as n from t join by_country on t.country = by_country.country" });
if (r.result.columns[0].values[0] !== 200000) throw new Error(`join count ${r.result.columns[0].values[0]}`);

// loadCsv with a buffer source (the Blob path needs a browser); it must
// reproduce the checked-in image's row count and be queryable by name
const csv = fs.readFileSync(new URL("data-200000.csv", spike));
const c = await ok({ cmd: "loadCsv", name: "c", source: toBuffer(csv) });
if (c.rows !== 200000) throw new Error(`loadCsv rows ${c.rows}`);
r = await ok({ cmd: "query", table: "c", sql: "select country from c order by country limit 1" });
if (text(r.result.columns[0], 0) !== "country_0") throw new Error(`loadCsv query ${text(r.result.columns[0], 0)}`);

// registerFunction from source text (functions don't cross postMessage)
await ok({ cmd: "registerFunction", name: "twice", signature: { params: ["int"], returns: "int", perRow: true }, source: "(x) => x * 2" });
r = await ok({ cmd: "query", table: "t", sql: "select twice(count(*)) as n from t" });
if (r.result.columns[0].values[0] !== 400000) throw new Error("registered function");
if (!(await ok({ cmd: "unregisterFunction", name: "twice" })).ok) throw new Error("unregister");

// errors come back as replies, with the query flag set for SQL errors
let e = await send({ cmd: "query", table: "nope", sql: "select 1" });
if (e.ok || !/no table loaded: 'nope'/.test(e.error)) throw new Error(`missing table: ${e.error}`);
e = await send({ cmd: "query", sql: "select contry from t" });
if (e.ok || !e.isQueryError || !/unknown column 'contry'/.test(e.error)) throw new Error(`query error: ${e.error}`);

console.log("worker smoke: OK (load, query, materialize, join, loadCsv, registerFunction, errors)");
