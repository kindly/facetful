// Drives worker.js in Node through its postMessage protocol. The other gates
// use core.js directly, which is how a worker-only bug (setTable calling
// itself, 0.4.0–0.5.0) shipped twice: nothing here talks to the wasm except
// through the worker's message handler.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import zlib from "node:zlib";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = new URL(".", import.meta.url);
const spike = new URL("../../spikes/facet-spike/", import.meta.url);

// the worker's globals: `self`, `postMessage`, and a fetch that reads files,
// handing the body out in 64 KB chunks like a network would, pausing 2 ms
// a megabyte (so the loader is kept waiting and indexes as it loads, as on
// a real network; without the pause that hung on timing and failed CI once)
const pending = new Map();
globalThis.self = globalThis;
globalThis.postMessage = (msg) => {
  const done = pending.get(msg.id);
  pending.delete(msg.id);
  done(msg);
};
// (`#1` on the URL makes the first chunk one byte, as a network may)
globalThis.fetch = async (url) => {
  const bytes = fs.readFileSync(fileURLToPath(url));
  let at = 0, next = 0;
  const body = new ReadableStream({
    async pull(c) {
      if (at >= bytes.length) return c.close();
      if (at >= next) { next += 1 << 20; await new Promise((r) => setTimeout(r, 2)); }
      const n = at === 0 && new URL(url).hash === "#1" ? 1 : 65536;
      c.enqueue(new Uint8Array(bytes.subarray(at, at + n)));
      at += n;
    },
  });
  return new Response(body, { headers: { "content-length": String(bytes.length) } });
};

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

// the search index builds in the background, a row group at a time: a
// query sent while it builds is answered first (scanning), with the same
// count as before and after
const sq = "select count(*) as n from t where search('country 1', country, owner)";
const before = (await ok({ cmd: "query", table: "t", sql: sq })).result.columns[0].values[0];
const order = [];
const building = ok({ cmd: "buildSearchIndex", table: "t", columns: ["country", "owner"] }).then((x) => (order.push("build"), x));
const during = ok({ cmd: "query", table: "t", sql: sq }).then((x) => (order.push("query"), x));
const [built, mid] = await Promise.all([building, during]);
if (order[0] !== "query") throw new Error(`a query waited for the whole index build: ${order}`);
const after = (await ok({ cmd: "query", table: "t", sql: sq })).result.columns[0].values[0];
if (before === 0 || mid.result.columns[0].values[0] !== before || after !== before) {
  throw new Error(`search counts before/during/after the build: ${before}/${mid.result.columns[0].values[0]}/${after}`);
}
if (!(built.bytes > 0) || typeof built.elapsedMs !== "number") throw new Error(`buildSearchIndex reply ${JSON.stringify(built)}`);
// an image streamed in and indexed as it arrives: the index is ready with
// the table and answers like the one built after loading
const streamed = await ok({ cmd: "loadUrl", name: "s", url: new URL("data-200000.facetful", spike).href, searchColumns: ["country", "owner"] });
if (streamed.rows !== 200000 || !(streamed.indexedWhileLoading > 0) || streamed.persisted || typeof streamed.searchReady !== "boolean") {
  throw new Error(`loadUrl reply ${JSON.stringify(streamed)}`);
}
await ok({ cmd: "searchReady", table: "s" });
const sqs = sq.replace("from t", "from s");
const viaStream = (await ok({ cmd: "query", table: "s", sql: sqs })).result.columns[0].values[0];
if (viaStream !== after) throw new Error(`search over the streamed table: ${viaStream} against ${after}`);
// a gzip file (facetful publish's .gz, for GitHub Pages) is decompressed
// as it streams, whatever the size of the first chunk (it replaces `s`,
// and the checks below run on it)
{
  const gz = path.join(os.tmpdir(), `facetful-smoke-${process.pid}.facetful.gz`);
  fs.writeFileSync(gz, zlib.gzipSync(image, { level: 9 }));
  try {
    for (const url of [pathToFileURL(gz).href, pathToFileURL(gz).href + "#1"]) {
      const z = await ok({ cmd: "loadUrl", name: "s", url, searchColumns: ["country", "owner"] });
      if (z.rows !== 200000) throw new Error(`loadUrl of a .gz: ${JSON.stringify(z)}`);
      await ok({ cmd: "searchReady", table: "s" });
      const n = (await ok({ cmd: "query", table: "s", sql: sq.replace("from t", "from s") })).result.columns[0].values[0];
      if (n !== after) throw new Error(`search over a .gz image: ${n} against ${after}`);
    }
  } finally {
    fs.rmSync(gz, { force: true });
  }
}
const sgs = (await ok({ cmd: "searchSuggest", table: "s", q: "countr", columns: ["country", "owner"] })).suggestions;
if (!sgs.indexed) throw new Error("the streamed table's index is not in use");
// a split image (facetful split): the first part streamed in, the second
// attached, each indexed as it arrives: the same answers as the whole image
const part = (n) => new URL(`data-200000.part${n}.facetful`, spike).href;
await ok({ cmd: "loadUrl", name: "p", url: part(1), searchColumns: ["country"] });
const early = await send({ cmd: "query", table: "p", sql: "select count(*) as n from p where owner = 'owner_1'" });
if (early.ok || !/owner/.test(early.error)) throw new Error(`a column of the unattached part answered: ${JSON.stringify(early)}`);
const att = await ok({ cmd: "attach", table: "p", url: part(2), searchColumns: ["owner"] });
if (att.columns !== 8 || att.persisted) throw new Error(`attach reply ${JSON.stringify(att)}`);
const viaParts = (await ok({ cmd: "query", table: "p", sql: sq.replace("from t", "from p") })).result.columns[0].values[0];
if (viaParts !== after) throw new Error(`search over the attached parts: ${viaParts} against ${after}`);
const across = "select fuel, count(*) as n, round(sum(capacity), 3) as mw from t where country = 'country_3' group by fuel order by fuel";
const cells = (r) => JSON.stringify(r.result.columns.map((c) => c.values ? [...c.values] : c.offsets ? [...c.offsets] : null));
const [p, w] = [await ok({ cmd: "query", table: "p", sql: across.replace("from t", "from p") }), await ok({ cmd: "query", table: "t", sql: across })];
if (cells(p) !== cells(w)) throw new Error("a query over both parts differs from the whole image");
if (!(await ok({ cmd: "searchSuggest", table: "p", q: "countr", columns: ["country", "owner"] })).suggestions.indexed) {
  throw new Error("the merged index is not in use");
}
const wrong = await send({ cmd: "attach", table: "s", url: part(2) });
if (wrong.ok) throw new Error("attached a part to a table that is not its split");
// two builds of one table's index at once: the later one wins, and the
// earlier says it was superseded instead of claiming the later's index
// (review, 5 October: both replied ok, with the index for the second)
{
  const first = send({ cmd: "buildSearchIndex", table: "t", columns: ["country"] });
  const second = send({ cmd: "buildSearchIndex", table: "t", columns: ["owner"] });
  const [r1, r2] = await Promise.all([first, second]);
  if (r1.ok || !/superseded/.test(r1.error) || !r2.ok) throw new Error(`overlapping builds: ${JSON.stringify([r1, r2])}`);
  const used = async (columns) => (await ok({ cmd: "searchSuggest", table: "t", q: "o", columns })).suggestions.indexed;
  if (!(await used(["owner"])) || (await used(["country"]))) throw new Error("the index in use is not the later request's");
  // a request the engine refuses takes nothing over: the build under way finishes
  const building = send({ cmd: "buildSearchIndex", table: "t", columns: ["country", "owner"] });
  const refused = await send({ cmd: "buildSearchIndex", table: "t", columns: ["capacity"] });
  const built = await building;
  if (refused.ok || !built.ok || !(await used(["country", "owner"]))) throw new Error(`a refused request stopped a build: ${JSON.stringify([refused, built])}`);
}

// a column that is not text cannot be indexed: an error reply, the worker fine
const notText = await send({ cmd: "buildSearchIndex", table: "t", columns: ["capacity"] });
if (notText.ok || !/text column/.test(notText.error)) throw new Error(`indexing a number column: ${JSON.stringify(notText)}`);
if ((await ok({ cmd: "query", table: "t", sql: "select count(*) as n from t" })).result.columns[0].values[0] !== 200000) throw new Error("the worker after a refused index");
const missing = await send({ cmd: "loadUrl", name: "x", url: new URL("data-200000.facetful", spike).href, searchColumns: ["nope"] });
if (missing.ok) throw new Error("loadUrl indexed a column that is not there");

// suggestions through the worker: completions of the word being typed,
// counted on the rows the filter leaves
const sg = (await ok({ cmd: "searchSuggest", table: "t", q: "countr", columns: ["country", "owner"], where: "status = 'status_0'", limit: 5, didYouMean: true })).suggestions;
if (!sg.indexed || !sg.completions.some((c) => c.word === "country" && c.query === "country")) {
  throw new Error(`searchSuggest: ${JSON.stringify(sg)}`);
}
const bad = await send({ cmd: "searchSuggest", table: "t", q: "x", columns: ["country"], where: "nope = 1" });
if (bad.ok || !/nope/.test(bad.error)) throw new Error(`searchSuggest with a bad filter: ${JSON.stringify(bad)}`);
await ok({ cmd: "dropSearchIndex", table: "t" });

// index requests that overlap. A typo only the index corrects tells whether
// it is in use. (No OPFS in Node: a cache path is looked for, not found.)
{
  const typo = "select count(*) as n from t where search('countr owner_1', country, owner)";
  const typoCount = async () => (await ok({ cmd: "query", table: "t", sql: typo })).result.columns[0].values[0];
  // a request that is refused, cache path or not, changes nothing: the
  // build under way finishes
  const building = ok({ cmd: "buildSearchIndex", table: "t", columns: ["country", "owner"] });
  const refused = await send({ cmd: "buildSearchIndex", table: "t", columns: ["capacity"], cache: "smoke/none.idx" });
  if (refused.ok || !/text column/.test(refused.error)) throw new Error(`a refused cached request: ${JSON.stringify(refused)}`);
  await building;
  if ((await typoCount()) !== 104258) throw new Error("the build a refused request overlapped is not in use");
  await ok({ cmd: "dropSearchIndex", table: "t" });
  // searchReady waits for a request with a cache path from its start,
  // while it looks for the kept index as well as while it builds
  const cached = ok({ cmd: "buildSearchIndex", table: "t", columns: ["country", "owner"], cache: "smoke/none.idx" });
  await ok({ cmd: "searchReady", table: "t" });
  if ((await typoCount()) !== 104258) throw new Error("searchReady resolved before the index was in use");
  await cached;
  await ok({ cmd: "dropSearchIndex", table: "t" });
}

// loadCsv with a buffer source (the Blob path needs a browser); it must
// reproduce the checked-in image's row count and be queryable by name
const csv = fs.readFileSync(new URL("data-200000.csv", spike));
const c = await ok({ cmd: "loadCsv", name: "c", source: toBuffer(csv) });
if (c.rows !== 200000) throw new Error(`loadCsv rows ${c.rows}`);
r = await ok({ cmd: "query", table: "c", sql: "select country from c order by country limit 1" });
if (text(r.result.columns[0], 0) !== "country_0") throw new Error(`loadCsv query ${text(r.result.columns[0], 0)}`);

// dictText: codes + dictionary cross the boundary, decoded on the page side
r = await ok({ cmd: "query", table: "t", sql: "select country from t where capacity > 3 order by id", dictText: true });
{
  const c = r.result.columns[0];
  if (!c.codes || c.codes.length !== r.result.rowCount || !c.dict) throw new Error("dictText through the worker");
  const first = dec.decode(c.dict.bytes.subarray(c.dict.offsets[c.codes[0]], c.dict.offsets[c.codes[0] + 1]));
  const plain = await ok({ cmd: "query", table: "t", sql: "select country from t where capacity > 3 order by id limit 1" });
  if (first !== text(plain.result.columns[0], 0)) throw new Error(`dictText decode ${first}`);
}

// registerFunction from source text (functions don't cross postMessage)
await ok({ cmd: "registerFunction", name: "twice", signature: { params: ["int"], returns: "int", perRow: true }, source: "(x) => x * 2" });
r = await ok({ cmd: "query", table: "t", sql: "select twice(count(*)) as n from t" });
if (r.result.columns[0].values[0] !== 400000) throw new Error("registered function");
if (!(await ok({ cmd: "unregisterFunction", name: "twice" })).ok) throw new Error("unregister");

// queryBatch: several statements, one message; results in order
r = await ok({ cmd: "queryBatch", table: "t", sqls: ["select country, count(*) as n from t where capacity > 3 group by country order by n desc limit 3", "select count(*) as n from t where capacity > 3"] });
if (r.results.length !== 2 || r.results[0].rowCount !== 3 || r.results[1].columns[0].values[0] !== 111597) throw new Error(`queryBatch ${JSON.stringify(r).slice(0, 200)}`);
const be = await send({ cmd: "queryBatch", table: "t", sqls: ["select 1 as x from t limit 1", "select contry from t"] });
if (be.ok || !be.isQueryError || !/statement 1: .*unknown column 'contry'/s.test(be.error)) throw new Error(`queryBatch error: ${be.error}`);

// describe: the catalog of a loaded table
{
  const { info } = await ok({ cmd: "describe", table: "t" });
  const country = info.columns.find((c) => c.name === "country");
  if (info.rows !== 200000 || info.groups !== 4 || country.kind !== "utf8/dict (u8 codes)" || country.dict !== 200) throw new Error(`describe ${JSON.stringify(info).slice(0, 200)}`);
  const cap = info.columns.find((c) => c.name === "capacity");
  if (cap.nulls !== 5983 || cap.min !== 0.1) throw new Error(`describe stats ${JSON.stringify(cap)}`);
}

// memoryStats: the worker's wasm high-water and table count
{
  const m = await ok({ cmd: "memoryStats" });
  if (!(m.wasmBytes > 1 << 20) || m.tables !== 5) throw new Error(`memoryStats ${JSON.stringify(m)}`);
}

// retained results (prepare / page / sortHandle / closeHandle): blocks of a
// handle equal the same rows of the query, before and after a sort
{
  const cells = (res) => {
    const out = [];
    for (let i = 0; i < res.rowCount; i++) {
      out.push(res.columns.map((c) => {
        if (!((c.validity[i >> 3] >> (i & 7)) & 1)) return null;
        if (c.values) return c.values[i];
        if (c.codes) return dec.decode(c.dict.bytes.subarray(c.dict.offsets[c.codes[i]], c.dict.offsets[c.codes[i] + 1]));
        return text(c, i);
      }).join("|"));
    }
    return out.join("\n");
  };
  const same = async (page, sql, what) => {
    const q = await ok({ cmd: "query", table: "t", sql });
    if (page.result.rowCount !== q.result.rowCount || cells(page.result) !== cells(q.result)) throw new Error(`handle ${what}: differs from ${sql}`);
  };
  const base = "select id, country, capacity from t where capacity > 3";
  const h = await ok({ cmd: "prepare", table: "t", sql: base });
  if (h.rows !== 111597 || h.kind !== "rows" || h.columns.map((c) => `${c.name}:${c.kind}`).join() !== "id:int,country:text,capacity:float") {
    throw new Error(`prepare ${JSON.stringify(h)}`);
  }
  let pg = await ok({ cmd: "page", handle: h.handle, start: 70000, end: 70512, dictText: true });
  if (!pg.result.columns[1].codes) throw new Error("page: dictText gives codes");
  await same(pg, `${base} limit 512 offset 70000`, "unsorted block");
  pg = await ok({ cmd: "page", handle: h.handle, start: 111500, end: 999999 });
  await same(pg, `${base} limit 512 offset 111500`, "last block, clamped");
  // (id repeats, so the order is made total: equal rows are whole duplicates)
  const sorted = await ok({ cmd: "sortHandle", handle: h.handle, orderBy: "capacity desc, id, country" });
  if (sorted.rows !== 111597) throw new Error(`sortHandle rows ${sorted.rows}`);
  pg = await ok({ cmd: "page", handle: h.handle, start: 50000, end: 50512 });
  await same(pg, `${base} order by capacity desc, id, country limit 512 offset 50000`, "sorted block");
  pg = await ok({ cmd: "page", handle: h.handle, start: 50000, end: 50100, columns: ["capacity", "id"] });
  await same(pg, `select capacity, id from t where capacity > 3 order by capacity desc, id, country limit 100 offset 50000`, "chosen columns");
  // a refused sort leaves the order as it was
  let bad = await send({ cmd: "sortHandle", handle: h.handle, orderBy: "nope" });
  if (bad.ok || !bad.isQueryError || !/unknown column 'nope'/.test(bad.error)) throw new Error(`sortHandle error: ${JSON.stringify(bad)}`);
  bad = await send({ cmd: "page", handle: h.handle, start: 0, end: 5, columns: ["nope"] });
  if (bad.ok || !/no result column 'nope'/.test(bad.error)) throw new Error(`page column error: ${JSON.stringify(bad)}`);
  pg = await ok({ cmd: "page", handle: h.handle, start: 50000, end: 50512 });
  await same(pg, `${base} order by capacity desc, id, country limit 512 offset 50000`, "block after a refused sort");
  // a grouped result is kept whole
  const g = await ok({ cmd: "prepare", table: "t", sql: "select country, count(*) as n from t group by country order by n desc, country" });
  if (g.rows !== 200 || g.kind !== "result") throw new Error(`prepare grouped ${JSON.stringify(g)}`);
  pg = await ok({ cmd: "page", handle: g.handle, start: 190, end: 300 });
  await same(pg, "select country, count(*) as n from t group by country order by n desc, country limit 10 offset 190", "grouped block");
  await ok({ cmd: "sortHandle", handle: g.handle, orderBy: "country desc" });
  pg = await ok({ cmd: "page", handle: g.handle, start: 0, end: 20 });
  await same(pg, "select country, count(*) as n from t group by country order by country desc limit 20", "grouped block, sorted");
  // closed: page and sort say so; closing twice is fine
  await ok({ cmd: "closeHandle", handle: g.handle });
  await ok({ cmd: "closeHandle", handle: g.handle });
  for (const msg of [{ cmd: "page", handle: g.handle, start: 0, end: 5 }, { cmd: "sortHandle", handle: g.handle, orderBy: "n" }]) {
    bad = await send(msg);
    if (bad.ok || !/result handle \d+ is closed/.test(bad.error)) throw new Error(`closed handle: ${JSON.stringify(bad)}`);
  }
  // stale: the table's name is loaded again, so the handle's rows are not that table's
  await ok({ cmd: "load", name: "again", buffer: toBuffer(image) });
  const st = await ok({ cmd: "prepare", table: "again", sql: "select id from again where capacity > 290" });
  const kept = await ok({ cmd: "prepare", table: "again", sql: "select count(*) as n from again" });
  await ok({ cmd: "page", handle: st.handle, start: 0, end: 5 });
  await ok({ cmd: "load", name: "again", buffer: toBuffer(image) });
  for (const msg of [{ cmd: "page", handle: st.handle, start: 0, end: 5 }, { cmd: "sortHandle", handle: st.handle, orderBy: "id" }, { cmd: "page", handle: kept.handle, start: 0, end: 1 }]) {
    bad = await send(msg);
    if (bad.ok || !/result handle is stale: its table was replaced/.test(bad.error)) throw new Error(`stale handle: ${JSON.stringify(bad)}`);
  }
  // a pool of 5: h, st and kept are open, two more fit, the sixth is
  // refused naming the open statements, and nothing open is taken away
  const more = [];
  for (let i = 0; i < 2; i++) more.push(await ok({ cmd: "prepare", table: "t", sql: `select id from t where capacity > ${i}` }));
  bad = await send({ cmd: "prepare", table: "t", sql: "select id from t" });
  if (bad.ok || !/^5 result handles are open and maxHandles is 5: close\(\) one/.test(bad.error) || !bad.error.includes("\n  select id from t where capacity > 1"))
    throw new Error(`full pool: ${JSON.stringify(bad)}`);
  await ok({ cmd: "page", handle: h.handle, start: 0, end: 1 });
  // closing one (a stale one counts until closed) makes room
  await ok({ cmd: "closeHandle", handle: st.handle });
  more.push(await ok({ cmd: "prepare", table: "t", sql: "select id from t" }));
  for (const m of [h, kept, ...more]) await ok({ cmd: "closeHandle", handle: m.handle });
  // a statement that does not parse: no handle
  bad = await send({ cmd: "prepare", table: "t", sql: "select contry from t" });
  if (bad.ok || !bad.isQueryError || !/unknown column 'contry'/.test(bad.error)) throw new Error(`prepare error: ${JSON.stringify(bad)}`);
}

// errors come back as replies, with the query flag set for SQL errors
let e = await send({ cmd: "query", table: "nope", sql: "select 1" });
if (e.ok || !/no table loaded: 'nope'/.test(e.error)) throw new Error(`missing table: ${e.error}`);
e = await send({ cmd: "query", sql: "select contry from t" });
if (e.ok || !e.isQueryError || !/unknown column 'contry'/.test(e.error)) throw new Error(`query error: ${e.error}`);

console.log("worker smoke: OK (load, query, materialize, join, loadCsv, dictText, registerFunction, result handles, errors)");
