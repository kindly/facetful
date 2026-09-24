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
    eq(r.column("country")[r.rowCount - 1], plain.column("country")[plain.rowCount - 1], "column() through the dictionary");
    eq([...r.rows()].length, r.rowCount, "rows() through the dictionary");
    eq((await step("materialize", () => db.materialize("m", "select country, count(*) as n from t group by country", { table: "t" }))).rows, 200, "materialize rows");
    eq((await db.query("select count(*) as n from m", { table: "m" })).column("n")[0], 200, "query on materialized");
    eq((await step("join", () => db.query("select count(*) as n from t join m on t.country = m.country", { table: "t" }))).column("n")[0], 200000, "join count");
    const csv = new Blob([await (await fetch("/spikes/facet-spike/data-200000.csv")).arrayBuffer()], { type: "text/csv" });
    eq((await step("loadCsv(Blob)", () => db.loadCsv("c", csv))).rows, 200000, "loadCsv rows");
    eq((await db.query("select count(*) as n from c where capacity is null", { table: "c" })).column("n")[0],
       (await db.query("select count(*) as n from t where capacity is null", { table: "t" })).column("n")[0], "csv table agrees with image");
    await step("storeOpfs", () => db.storeOpfs("smoke/t.facetful", imgCopy));
    eq((await step("loadOpfs", () => db.loadOpfs("o", "smoke/t.facetful"))).rows, 200000, "loadOpfs rows");
    eq((await db.query("select count(*) as n from o where capacity > 100", { table: "o" })).column("n")[0],
       (await db.query("select count(*) as n from t where capacity > 100", { table: "t" })).column("n")[0], "opfs table agrees");
    await step("removeOpfs", () => db.removeOpfs("smoke/t.facetful"));
    await step("registerFunction", () => db.registerFunction("twice", { params: ["int"], returns: "int", perRow: true }, (x) => x * 2));
    eq((await db.query("select twice(count(*)) as n from t", { table: "t" })).column("n")[0], 400000, "registered function");
    const info = await step("describe", () => db.describe({ table: "t" }));
    eq(info.rows, 200000, "describe rows");
    eq(info.columns.find((c) => c.name === "country").dict, 200, "describe dictionary size");
    const mem = await step("memoryStats", () => db.memoryStats());
    if (!(mem.wasmBytes > 1 << 20) || mem.tables !== 4) throw new Error("memoryStats: " + JSON.stringify(mem));
    let msg = "";
    try { await db.query("select contry from t", { table: "t" }); } catch (e) { msg = String(e.message); }
    if (!/unknown column 'contry'/.test(msg)) throw new Error("error path: " + msg);
    db.close();
    return { ok: true, log };
  } catch (e) {
    return { ok: false, error: String(e && e.stack || e), log };
  }
})();
</script>`;

// ---- static server for the repo (the package resolves worker + wasm relative to index.js)
const TYPES = { ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".html": "text/html", ".csv": "text/csv", ".json": "application/json" };
const server = createServer((req, res) => {
  const path = decodeURIComponent(new URL(req.url, "http://x").pathname);
  if (path === "/smoke.html") { res.writeHead(200, { "content-type": "text/html" }); return res.end(PAGE); }
  const file = join(root, path);
  if (!file.startsWith(root) || !existsSync(file)) { res.writeHead(404); return res.end(); }
  const ext = path.slice(path.lastIndexOf("."));
  res.writeHead(200, { "content-type": TYPES[ext] ?? "application/octet-stream" });
  res.end(readFileSync(file));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const url = `http://127.0.0.1:${server.address().port}/smoke.html`;

// ---- the browser
const candidates = [process.env.FACETFUL_BROWSER, "chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "chrome"].filter(Boolean);
const browser = candidates.find((b) => { try { execSync(`command -v ${b}`, { stdio: "ignore" }); return true; } catch { return false; } });
if (!browser) { console.error("browser smoke: no Chromium/Chrome found (set FACETFUL_BROWSER)"); process.exit(1); }
const profile = mkdtempSync(join(process.env.TMPDIR ?? tmpdir(), "facetful-smoke-"));
const port = 9300 + Math.floor(Math.random() * 500);
const chrome = spawn(browser, [
  "--headless=new", "--no-proxy-server", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`,
  "--no-first-run", "--no-default-browser-check", "--disable-gpu", "--disable-crash-reporter", `--crash-dumps-dir=${profile}`,
  ...(process.env.CI || process.env.FACETFUL_NO_CHROME_SANDBOX ? ["--no-sandbox"] : []),
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
if (!targets) fail(`${browser} did not open its DevTools port`);
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
