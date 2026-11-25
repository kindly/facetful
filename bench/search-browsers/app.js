// The search benchmark page: one dataset, one network profile, in phases
// that post their results to the server (and show them here).
//   ?dataset=units|grantnav|units-packed|grantnav-packed &p=local|100mbps|20mbps &run=<id> &phase=first|repeat|split
// first: stream the image in with its search index built as it arrives
// (persisted to OPFS when there is one), time typing, keep the index;
// repeat (a fresh page): reopen both from OPFS; split (grantnav): the first
// part, then the second attached.
import { Facetful } from "../../js/facetful/index.js";

const DATASETS = {
  units: {
    file: "units.facetful",
    cols: ["Plant / Project name", "Unit / Phase name", "Owner(s)", "Parent(s)", "Country/area", "City"],
    weights: [1, 2, 3, 3, 4, 4],
    queries: ["wind farm", "solar park", "coal power station", "jinsha"],
  },
  grantnav: {
    file: "grantnav.facetful",
    cols: ["Title", "Recipient Org:Name", "Grant Programme:Title", "Description Common", "Description Rare"],
    weights: [1, 3, 3, 5, 5],
    queries: ["mental health", "youth sport london", "climte chnage", "food bank"],
    split: { first: "grantnav-1.facetful", second: "grantnav-2.facetful", firstCols: ["Title", "Recipient Org:Name", "Grant Programme:Title", "Description Common"], secondCols: ["Description Rare"] },
  },
};

// the same data as packed images (facetful pack): smaller on the wire,
// unpacked as they arrive, kept in OPFS unpacked
DATASETS["units-packed"] = { ...DATASETS.units, file: "units-packed.facetful" };
DATASETS["grantnav-packed"] = { ...DATASETS.grantnav, file: "grantnav-packed.facetful", split: undefined };

const params = new URLSearchParams(location.search);
const name = params.get("dataset") ?? "units";
const ds = DATASETS[name];
const profile = params.get("p") ?? "local";
const phase = params.get("phase") ?? "first";
const run = params.get("run") ?? `manual-${Date.now()}`;
const logEl = document.getElementById("log");
const log = (s) => (logEl.textContent += s + "\n");
const status = (s) => (document.getElementById("status").textContent = s);
const ms = (t) => Math.round((performance.now() - t) * 10) / 10;
const ident = (c) => `"${c.replace(/"/g, '""')}"`;
const lit = (s) => `'${s.replace(/'/g, "''")}'`;
const path = (f) => `bench/${profile}-${f}`;

const result = {
  run, dataset: name, profile, phase,
  ua: navigator.userAgent, cores: navigator.hardwareConcurrency, deviceMemory: navigator.deviceMemory ?? null,
  crossOriginIsolated: self.crossOriginIsolated, secure: self.isSecureContext,
  opfs: !!navigator.storage?.getDirectory && self.isSecureContext,
  marks: {}, typing: [], memory: {},
};

async function post(r) {
  await fetch("/results", { method: "POST", body: JSON.stringify(r) });
}

async function memory(db, label) {
  const m = await db.memoryStats();
  result.memory[label] = { wasmMB: Math.round(m.wasmBytes / 1e6) };
  // the whole page, where the browser can say (Chromium, cross-origin isolated)
  if (label.startsWith("end") && performance.measureUserAgentSpecificMemory) {
    try {
      result.memory[label].pageMB = Math.round((await performance.measureUserAgentSpecificMemory()).bytes / 1e6);
    } catch { /* not allowed here */ }
  }
}

/** Every keystroke of each query: search(), searchSuggest, ranked top 20. */
async function typing(db, table, cols, weights, queries) {
  const list = cols.map(ident).join(", ");
  const ranked = cols.map((c, i) => `${ident(c)}, ${weights[i]}`).join(", ");
  for (const q of queries) {
    const t = { q, search: [], suggest: [], rank: [] };
    for (let i = 1; i <= q.length; i++) {
      const typed = q.slice(0, i);
      let s = performance.now();
      await db.query(`select count(*) as n from t where search(${lit(typed)}, ${list})`, { table });
      t.search.push(ms(s));
      s = performance.now();
      await db.searchSuggest(typed, { columns: cols, table, limit: 8 });
      t.suggest.push(ms(s));
      s = performance.now();
      await db.query(`select ${ident(cols[0])} from t where search(${lit(typed)}, ${list}) order by search_rank(${lit(typed)}, ${ranked}) limit 20`, { table });
      t.rank.push(ms(s));
    }
    result.typing.push(t);
    log(`  ${q}: search ${Math.max(...t.search)} / suggest ${Math.max(...t.suggest)} / rank ${Math.max(...t.rank)} ms at worst`);
  }
}

async function main() {
  const t0 = performance.now();
  const manifest = await (await fetch("/manifest.json")).json();
  const db = await Facetful.open({ wasmUrl: "/facetful.wasm", workerUrl: "/worker.js" });
  result.marks.engineReady = ms(t0);
  const url = (f) => `/data/${f}?p=${profile}`;
  const size = (f) => manifest.files[f].bytes;
  log(`${name}, ${profile}, ${phase}: ${navigator.userAgent}`);
  if (phase === "first") {
    let s = performance.now();
    const r = await db.loadUrl("t", url(ds.file), { searchColumns: ds.cols, persist: result.opfs ? path(ds.file) : undefined, expectedBytes: size(ds.file) });
    result.marks.tableReady = ms(s);
    await db.searchReady({ table: "t" });
    result.marks.searchReady = ms(s);
    result.marks.persisted = r.persisted;
    log(`table ready ${result.marks.tableReady} ms, search ready ${result.marks.searchReady} ms (${r.rows} rows)`);
    await memory(db, "loaded");
    await typing(db, "t", ds.cols, ds.weights, ds.queries);
    if (result.opfs) {
      s = performance.now();
      result.marks.indexSavedBytes = (await db.saveSearchIndex(path(`${name}.index`), { table: "t" })).bytes;
      result.marks.indexSave = ms(s);
    }
    await memory(db, "end");
  } else if (phase === "repeat") {
    let s = performance.now();
    await db.loadOpfs("t", path(ds.file));
    result.marks.opfsOpen = ms(s);
    s = performance.now();
    const b = await db.buildSearchIndex(ds.cols, { table: "t", cache: path(`${name}.index`) });
    result.marks.indexFromOpfs = ms(s);
    result.marks.indexCached = b.cached;
    log(`opened from OPFS ${result.marks.opfsOpen} ms, index ${b.cached ? "read back" : "REBUILT"} ${result.marks.indexFromOpfs} ms`);
    await typing(db, "t", ds.cols, ds.weights, ds.queries.slice(0, 2));
    await memory(db, "end");
  } else if (phase === "split") {
    const sp = ds.split;
    let s = performance.now();
    await db.loadUrl("t", url(sp.first), { searchColumns: sp.firstCols, expectedBytes: size(sp.first) });
    await db.searchReady({ table: "t" });
    result.marks.firstPartReady = ms(s);
    await db.attach(url(sp.second), { table: "t", searchColumns: sp.secondCols, expectedBytes: size(sp.second) });
    result.marks.allReady = ms(s);
    log(`first part ready ${result.marks.firstPartReady} ms, second attached ${result.marks.allReady} ms`);
    await memory(db, "end");
  }
  result.marks.total = ms(t0);
}

const next = () => (phase === "first" && result.opfs ? "repeat" : phase !== "split" && ds.split ? "split" : null);
main()
  .catch((e) => {
    result.error = String(e?.stack ?? e);
    log("ERROR " + result.error);
  })
  .then(async () => {
    await post(result);
    const n = result.error ? null : next();
    if (n) {
      status(`${phase} done; next: ${n}`);
      params.set("phase", n);
      location.search = params.toString();
    } else {
      await post({ run, done: true });
      status("done");
    }
  });
