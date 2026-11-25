// The browser gate: headless Chromium over the DevTools protocol, no
// dependencies. Serves the repo over localhost and drives the PUBLIC API
// (index.js -> module worker -> wasm) the way a page does — module workers,
// transferables, OPFS sync access handles and Blob.stream() exist only here.
// Everything Node can check is checked in Node; this is for what it can't.
//
//   node js/facetful/browser-smoke.mjs        FACETFUL_BROWSER=/path/to/chrome to pick the binary
import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { readFileSync, existsSync, mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";

const root = fileURLToPath(new URL("../../", import.meta.url));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- the page: every step through the public API, results into window.__smoke
const PAGE = `<!doctype html><meta charset="utf-8"><title>facetful browser smoke</title>
<script type="module">
import { Facetful } from "/js/facetful/index.js";
const log = [];
const step = async (name, f) => { const t0 = performance.now(); const v = await f(); log.push(name + " " + (performance.now() - t0).toFixed(0) + " ms"); return v; };
const eq = (a, b, what) => { if (a !== b) throw new Error(what + ": " + JSON.stringify(a) + " != " + JSON.stringify(b)); };
window.__smoke = (async () => {
  try {
    const db = await step("open", () => Facetful.open());
    const img = await (await fetch("/spikes/facet-spike/data-200000.facetful")).arrayBuffer();
    const imgCopy = img.slice(0);
    eq((await step("load", () => db.load("t", img))).rows, 200000, "load rows");
    let r = await step("query+udf", () => db.query("select count(*) as n from t where regexp(country, '^country_1[0-9]$')"));
    eq(r.column("n")[0], 25947, "regexp count");
    const plain = await db.query("select country, capacity from t where capacity > 3 order by id");
    r = await step("query dictText", () => db.query("select country, capacity from t where capacity > 3 order by id", { dictText: true }));
    const raw = r.columnRaw("country");
    if (!raw.codes || !raw.dict || raw.offsets) throw new Error("dictText: codes + dict expected on a dictionary column");
    eq(raw.codes.length, r.rowCount, "codes per row");
    eq(r.dictionary("country")[raw.codes[0]], plain.column("country")[0], "dictionary decode");
    eq(r.dictValue("country", raw.codes[1]), plain.column("country")[1], "dictValue decode");
    eq(r.dictValue("country", 100000), null, "dictValue out of range");
    eq(r.column("country")[r.rowCount - 1], plain.column("country")[plain.rowCount - 1], "column() through the dictionary");
    eq([...r.rows()].length, r.rowCount, "rows() through the dictionary");
    eq((await step("materialize", () => db.materialize("m", "select country, count(*) as n from t group by country", { table: "t" }))).rows, 200, "materialize rows");
    eq((await db.query("select count(*) as n from m", { table: "m" })).column("n")[0], 200, "query on materialized");
    eq((await step("join", () => db.query("select count(*) as n from t join m on t.country = m.country", { table: "t" }))).column("n")[0], 200000, "join count");
    // a retained result: blocks of it, sorted in the worker, through the public API
    const h = await step("prepare", () => db.prepare("select id, country, capacity from t where capacity > 3", { table: "t", dictText: true }));
    eq(h.rows, plain.rowCount, "prepare rows");
    eq(h.kind, "rows", "prepare kind");
    let pg = await step("page", () => h.page(70000, 70512));
    eq(pg.rowCount, 512, "page rows");
    eq(pg.start, 70000, "page start");
    eq(pg.generation, 0, "page generation");
    if (!pg.columnRaw("country").codes) throw new Error("page: dictText from prepare");
    const want = await db.query("select id, country, capacity from t where capacity > 3 limit 512 offset 70000", { table: "t" });
    eq(JSON.stringify([...pg.rows()]), JSON.stringify([...want.rows()]), "page equals the query's rows");
    const early = h.page(0, 5);
    const sorting = step("sort", () => h.sort("capacity desc, id, country"));
    eq(h.generation, 1, "generation moves on when sort is called");
    eq((await early).generation, 0, "a page asked for before the sort carries the old generation");
    eq((await sorting).generation, 1, "sort generation");
    pg = await h.page(100, 110, { columns: ["capacity", "id"] });
    eq(pg.generation, 1, "page generation after the sort");
    const top = await db.query("select capacity, id from t where capacity > 3 order by capacity desc, id, country limit 10 offset 100", { table: "t" });
    eq(JSON.stringify([...pg.rows()]), JSON.stringify([...top.rows()]), "sorted page equals the query's rows");
    await h.close();
    let closedMsg = "";
    try { await h.page(0, 5); } catch (e) { closedMsg = String(e.message); }
    if (!/is closed/.test(closedMsg)) throw new Error("page on a closed handle: " + closedMsg);
    const csv = new Blob([await (await fetch("/spikes/facet-spike/data-200000.csv")).arrayBuffer()], { type: "text/csv" });
    eq((await step("loadCsv(Blob)", () => db.loadCsv("c", csv))).rows, 200000, "loadCsv rows");
    eq((await db.query("select count(*) as n from c where capacity is null", { table: "c" })).column("n")[0],
       (await db.query("select count(*) as n from t where capacity is null", { table: "t" })).column("n")[0], "csv table agrees with image");
    await step("storeOpfs", () => db.storeOpfs("smoke/t.facetful", imgCopy));
    eq((await step("loadOpfs", () => db.loadOpfs("o", "smoke/t.facetful"))).rows, 200000, "loadOpfs rows");
    eq((await db.query("select count(*) as n from o where capacity > 100", { table: "o" })).column("n")[0],
       (await db.query("select count(*) as n from t where capacity > 100", { table: "t" })).column("n")[0], "opfs table agrees");
    await step("removeOpfs", () => db.removeOpfs("smoke/t.facetful"));
    // streamed in, indexed as it arrives, written to OPFS as it comes: the
    // persisted copy reopens, and its index built afterwards answers alike
    const cols = ["country", "owner"];
    const st = await step("loadUrl(index, persist)", () => db.loadUrl("u", "/spikes/facet-spike/data-200000.facetful", { searchColumns: cols, persist: "smoke/u.facetful" }));
    eq(st.rows, 200000, "loadUrl rows");
    eq(st.persisted, true, "loadUrl wrote OPFS");
    eq((await step("loadOpfs(streamed)", () => db.loadOpfs("v", "smoke/u.facetful"))).rows, 200000, "persisted copy opens");
    await db.buildSearchIndex(cols, { table: "v" });
    const sq = (t) => "select count(*) as n from " + t + " where search('countr owner_1', country, owner)";
    const viaStream = (await db.query(sq("u"), { table: "u" })).column("n")[0];
    if (!(viaStream > 0)) throw new Error("search on the streamed table found nothing");
    eq(viaStream, (await db.query(sq("v"), { table: "v" })).column("n")[0], "streamed index agrees");
    // a split image: part 1 streamed in, part 2 attached, both written to
    // OPFS as they arrive; reopened from OPFS they attach the same way
    await step("loadUrl(part 1)", () => db.loadUrl("p1", "/spikes/facet-spike/data-200000.part1.facetful", { persist: "smoke/p1.facetful" }));
    const at = await step("attach(part 2)", () => db.attach("/spikes/facet-spike/data-200000.part2.facetful", { table: "p1", persist: "smoke/p2.facetful" }));
    eq(at.columns, 8, "attached columns");
    eq(at.persisted, true, "attach wrote OPFS");
    await db.loadOpfs("q1", "smoke/p1.facetful");
    await step("attachOpfs", () => db.attachOpfs("smoke/p2.facetful", { table: "q1" }));
    const across = (t) => "select fuel, count(*) as n, round(sum(capacity), 3) as mw from " + t + " where country = 'country_3' group by fuel order by fuel";
    const res = async (t) => JSON.stringify((await db.query(across(t), { table: t })).column("mw"));
    const wholeRes = await res("t");
    eq(await res("p1"), wholeRes, "streamed parts agree with the whole image");
    eq(await res("q1"), wholeRes, "OPFS parts agree with the whole image");
    // a packed image (the server packs the same file): unpacked as it
    // arrives, indexed alike, and written to OPFS as the image it packs, so
    // the next visit opens it without unpacking
    const pk = await step("loadUrl(packed, index, persist)", () => db.loadUrl("pk", "/packed/spikes/facet-spike/data-200000.facetful", { searchColumns: cols, persist: "smoke/pk.facetful" }));
    eq(pk.rows, 200000, "packed loadUrl rows");
    eq(pk.persisted, true, "packed loadUrl wrote OPFS");
    eq(await res("pk"), wholeRes, "the packed image agrees with the whole image");
    await db.buildSearchIndex(cols, { table: "pk" }); // whatever the download outran
    eq((await db.query(sq("pk"), { table: "pk" })).column("n")[0], viaStream, "the packed image's index agrees");
    const kept = new Uint8Array(await (await (await (await navigator.storage.getDirectory()).getDirectoryHandle("smoke")).getFileHandle("pk.facetful")).getFile().then((f) => f.arrayBuffer()));
    const plainBytes = new Uint8Array(await (await fetch("/spikes/facet-spike/data-200000.facetful")).arrayBuffer());
    eq(kept.length, plainBytes.length, "OPFS holds the unpacked length");
    if (!kept.every((b, i) => b === plainBytes[i])) throw new Error("OPFS does not hold the image the packed one unpacks to");
    eq((await step("loadOpfs(packed, persisted)", () => db.loadOpfs("pk2", "smoke/pk.facetful"))).rows, 200000, "the copy kept from a packed image opens");
    eq(await res("pk2"), wholeRes, "the kept copy agrees with the whole image");
    // packed parts attach like plain ones
    await step("loadUrl(packed part 1)", () => db.loadUrl("pp1", "/packed/spikes/facet-spike/data-200000.part1.facetful", { persist: "smoke/pp1.facetful" }));
    eq((await step("attach(packed part 2)", () => db.attach("/packed/spikes/facet-spike/data-200000.part2.facetful", { table: "pp1", persist: "smoke/pp2.facetful" }))).columns, 8, "packed part attached");
    eq(await res("pp1"), wholeRes, "packed parts agree with the whole image");
    await db.loadOpfs("pq1", "smoke/pp1.facetful");
    await db.attachOpfs("smoke/pp2.facetful", { table: "pq1" });
    eq(await res("pq1"), wholeRes, "parts kept from packed ones agree with the whole image");
    for (const f of ["pk", "pp1", "pp2"]) await db.removeOpfs("smoke/" + f + ".facetful");
    // the search index kept in OPFS: built and saved on a first visit, read
    // back on the next (no rebuild), rebuilt when the columns change; one
    // built while streaming in is saved explicitly
    const sCols = ["country", "owner"];
    const first = await step("buildSearchIndex(cache)", () => db.buildSearchIndex(sCols, { table: "t", cache: "smoke/o.index" }));
    eq(first.cached, false, "first build");
    if (!(first.savedBytes > 0)) throw new Error("the index was not kept: " + JSON.stringify(first));
    eq((await db.loadUrl("o3", "/spikes/facet-spike/data-200000.facetful")).rows, 200000, "a fresh copy");
    const again = await step("buildSearchIndex(cached)", () => db.buildSearchIndex(sCols, { table: "o3", cache: "smoke/o.index" }));
    eq(again.cached, true, "read back, not rebuilt");
    const sq2 = (t) => "select count(*) as n from " + t + " where search('countr owner_1', country, owner)";
    eq((await db.query(sq2("o3"), { table: "o3" })).column("n")[0], (await db.query(sq2("t"), { table: "t" })).column("n")[0], "the kept index answers alike");
    eq((await db.buildSearchIndex(["country"], { table: "o3", cache: "smoke/o.index" })).cached, false, "other columns rebuild");
    if (!((await db.saveSearchIndex("smoke/u.index", { table: "u" })).bytes > 0)) throw new Error("saving the streamed table's index");
    await db.removeOpfs("smoke/o.index");
    await db.removeOpfs("smoke/u.index");
    await step("registerFunction", () => db.registerFunction("twice", { params: ["int"], returns: "int", perRow: true }, (x) => x * 2));
    eq((await db.query("select twice(count(*)) as n from t", { table: "t" })).column("n")[0], 400000, "registered function");
    const batch = await step("query batch", () => db.query([
      "select country, count(*) as n from t where capacity > 3 group by country order by n desc limit 5",
      "select status, count(*) as n, sum(capacity) as mw from t where capacity > 3 group by status order by status",
      "select count(*) as n from t where capacity > 3",
    ], { table: "t" }));
    eq(batch.length, 3, "batch length");
    eq(batch[0].rowCount, 5, "batch limit");
    eq(batch[2].column("n")[0], (await db.query("select count(*) as n from t where capacity > 3", { table: "t" })).column("n")[0], "batch totals");
    eq(batch[1].column("status").length, batch[1].rowCount, "batch dictionary key decodes");
    const info = await step("describe", () => db.describe({ table: "t" }));
    eq(info.rows, 200000, "describe rows");
    eq(info.columns.find((c) => c.name === "country").dict, 200, "describe dictionary size");
    const mem = await step("memoryStats", () => db.memoryStats());
    if (!(mem.wasmBytes > 1 << 20) || mem.tables !== 13) throw new Error("memoryStats: " + JSON.stringify(mem));
    let msg = "";
    try { await db.query("select contry from t", { table: "t" }); } catch (e) { msg = String(e.message); }
    if (!/unknown column 'contry'/.test(msg)) throw new Error("error path: " + msg);
    await db.removeOpfs("smoke/u.facetful");
    await db.removeOpfs("smoke/p1.facetful");
    await db.removeOpfs("smoke/p2.facetful");
    db.close();
    return { ok: true, log };
  } catch (e) {
    return { ok: false, error: String(e && e.stack || e), log };
  }
})();
</script>`;

// ---- packed images, made here by the command's build of the engine (the
// page's build can only unpack)
const packer = await (await import("./core.js")).instantiate(readFileSync(join(root, "js/facetful/facetful_cli.wasm")));
const packedCache = new Map();
function packed(file) {
  if (!packedCache.has(file)) {
    const img = packer.imageFrom(readFileSync(file));
    packedCache.set(file, packer.packImage(img));
    packer.freeImage(img);
  }
  return packedCache.get(file);
}

// ---- static server for the repo (the package resolves worker + wasm relative to index.js)
const TYPES = { ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".html": "text/html", ".csv": "text/csv", ".json": "application/json" };
const server = createServer((req, res) => {
  const path = decodeURIComponent(new URL(req.url, "http://x").pathname);
  if (path === "/smoke.html") { res.writeHead(200, { "content-type": "text/html" }); return res.end(PAGE); }
  // /packed/<image>: the image's packed form, as `facetful pack` writes it
  const packing = path.startsWith("/packed/");
  const file = join(root, packing ? path.slice(7) : path);
  if (!file.startsWith(root) || !existsSync(file)) { res.writeHead(404); return res.end(); }
  if (packing) { res.writeHead(200, { "content-type": "application/octet-stream" }); return res.end(packed(file)); }
  const ext = path.slice(path.lastIndexOf("."));
  res.writeHead(200, { "content-type": TYPES[ext] ?? "application/octet-stream" });
  res.end(readFileSync(file));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const url = `http://127.0.0.1:${server.address().port}/smoke.html`;

// ---- the browser
// the first candidate that really is a Chrome: `command -v` is not enough
// (on GitHub's Ubuntu image `chromium-browser` is a snap stub that exits
// without ever opening a port; the browser there is google-chrome)
const candidates = [process.env.FACETFUL_BROWSER, process.env.CHROME_BIN, "google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"].filter(Boolean);
const isChrome = (b) => { try { return /Chrom/.test(execSync(`${b} --version`, { stdio: ["ignore", "pipe", "ignore"], timeout: 15000 }).toString()); } catch { return false; } };
const browser = candidates.find(isChrome);
if (!browser) { console.error(`browser smoke: no Chromium/Chrome found among ${candidates.join(", ")} (set FACETFUL_BROWSER)`); process.exit(1); }
const profile = mkdtempSync(join(process.env.TMPDIR ?? tmpdir(), "facetful-smoke-"));
const port = 9300 + Math.floor(Math.random() * 500);
const chrome = spawn(browser, [
  "--headless=new", "--no-proxy-server", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`,
  "--no-first-run", "--no-default-browser-check", "--disable-gpu", "--disable-crash-reporter", `--crash-dumps-dir=${profile}`,
  // CI runners: no user namespaces for Chrome's sandbox, and a tiny /dev/shm
  ...(process.env.CI || process.env.FACETFUL_NO_CHROME_SANDBOX ? ["--no-sandbox", "--disable-dev-shm-usage"] : []),
  "about:blank",
], { stdio: ["ignore", "ignore", "pipe"], env: { ...process.env, HOME: profile, XDG_CONFIG_HOME: profile, XDG_CACHE_HOME: profile } });
let stderr = "";
chrome.stderr.on("data", (d) => { stderr += d; });
const cleanUp = () => { try { chrome.kill(); } catch {} try { rmSync(profile, { recursive: true, force: true }); } catch {} server.close(); };
process.on("exit", cleanUp);
const fail = (why) => { console.error(`browser smoke: FAIL — ${why}\n${stderr.split("\n").slice(-8).join("\n")}`); process.exit(1); };

let targets = null;
for (let i = 0; i < 80 && !targets; i++) {
  try { targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json(); } catch { await sleep(250); }
}
if (!targets) fail(`${browser} did not open its DevTools port (exit ${chrome.exitCode})`);
const page = targets.find((t) => t.type === "page");
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
let id = 0;
const pending = new Map();
const console_ = [];
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) { const { res, rej } = pending.get(m.id); pending.delete(m.id); m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); }
  else if (m.method === "Runtime.consoleAPICalled") console_.push(`[${m.params.type}] ${m.params.args.map((a) => a.value ?? a.description ?? "").join(" ")}`);
  else if (m.method === "Runtime.exceptionThrown") console_.push(`[exception] ${m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text}`);
};
const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });
const watchdog = setTimeout(() => fail(`timed out after 180 s\n${console_.join("\n")}`), 180_000);

await send("Runtime.enable");
await send("Page.enable");
await send("Page.navigate", { url });
// the module script sets window.__smoke once it runs; wait for it, then for its promise
const r = await send("Runtime.evaluate", {
  expression: "new Promise((done) => { const poll = () => window.__smoke ? done(window.__smoke) : setTimeout(poll, 50); poll(); })",
  awaitPromise: true, returnByValue: true,
});
clearTimeout(watchdog);
if (r.exceptionDetails) fail(r.exceptionDetails.exception?.description ?? "evaluate failed");
const out = r.result.value;
if (!out || !out.ok) fail(`${out?.error ?? "no result"}\n--- page console ---\n${console_.join("\n")}`);
console.log(`browser smoke: OK (${browser}; ${out.log.join(", ")})`);
process.exit(0);
