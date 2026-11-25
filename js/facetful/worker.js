// The engine's dedicated worker: owns the wasm instance, all tables, and the
// OPFS sync access handles (createSyncAccessHandle only exists in workers).
// Protocol: {id, cmd, ...} in, {id, ok|error, ...} out; query column buffers
// are transferred, not cloned.

import { instantiate, transferables, QueryError } from "./core.js";
import { parquetToColumns, zstdCompressors } from "./parquet.js";
import { udfs as builtinUdfs } from "./udfs.js";

let engine = null;
const tables = new Map(); // name -> handle
// register under a name for FROM / JOIN from other tables' queries
const setTable = (name, handle) => { tables.set(name, handle); engine.catalogRegister(name, handle); };
let lastTable = null;

// Retained results (prepare): id -> { h, sql }. A pool of maxHandles (set at
// init): when it is full prepare is refused, naming the open statements, so
// a page that forgets close() finds out at once and no handle in use is
// taken away. Each holds row ids (4 bytes a row) or a kept result.
const handles = new Map();
let nextHandle = 1;
let maxHandles = 5;
function useHandle(id) {
  const e = handles.get(id);
  if (!e) throw new Error(`result handle ${id} is closed`);
  return e;
}
const brief = (sql) => { const s = sql.replace(/\s+/g, " ").trim(); return s.length > 80 ? s.slice(0, 77) + "..." : s; };

// OPFS file registry: the wasm's opfs_read import addresses files by these ids.
const opfsFiles = new Map(); // fileId -> { handle: FileSystemSyncAccessHandle, path }
let nextFileId = 1;

function opfsRead(fileId, offset, dest) {
  const e = opfsFiles.get(fileId);
  if (!e) return -1;
  return e.handle.read(dest, { at: offset });
}

// Sync access handles are exclusive locks; deleting or rewriting a path
// requires closing ours first. Any table still reading through a closed
// handle gets clean read errors rather than corrupt data.
function closeHandlesFor(path) {
  for (const [id, e] of opfsFiles) {
    if (e.path === path) {
      try { e.handle.close(); } catch { /* already closed */ }
      opfsFiles.delete(id);
    }
  }
}

async function opfsDir(path, create) {
  let dir = await navigator.storage.getDirectory();
  const parts = path.split("/").filter(Boolean);
  const file = parts.pop();
  for (const p of parts) dir = await dir.getDirectoryHandle(p, { create });
  return { dir, file };
}

async function opfsWrite(path, bytes) {
  closeHandlesFor(path);
  const { dir, file } = await opfsDir(path, true);
  const fh = await dir.getFileHandle(file, { create: true });
  const h = await fh.createSyncAccessHandle();
  try {
    h.truncate(0);
    h.write(bytes, { at: 0 });
    h.flush();
  } finally {
    h.close();
  }
}

/** Search index builds, one controller per table: each request to build,
 *  load or drop a table's index takes a new generation, and a build steps
 *  only while its generation is the table's latest. A request a newer one
 *  superseded replies with an error, never with the newer index's stats,
 *  and never saves. `done` resolves when the table's latest build settles. */
const builds = new Map(); // handle -> { gen, done }
let buildGen = 0;
const supersede = (handle) => {
  const gen = ++buildGen;
  builds.set(handle, { gen, done: null });
  return gen;
};
const isLatest = (handle, gen) => builds.get(handle)?.gen === gen;
const SUPERSEDED = "search index: superseded by a later request for this table";

/** Index the build under way on `handle` to the end, a row group at a
 *  time, yielding between groups so queries run meanwhile (search() scans
 *  until the index is in use). Resolves false when a newer request took
 *  over the table's index first. */
function stepToEnd(handle, gen) {
  const done = (async () => {
    while (isLatest(handle, gen) && engine.stepSearchIndex(handle, 1) > 0) {
      await new Promise((r) => setTimeout(r, 0));
    }
    return isLatest(handle, gen);
  })();
  if (isLatest(handle, gen)) builds.get(handle).done = done;
  return done;
}

/** Wait until the table's builds have settled (the latest one finished). */
async function settled(handle) {
  for (let b = builds.get(handle); b?.done; b = builds.get(handle)) {
    await b.done;
    if (builds.get(handle) === b) return;
  }
}

/** A fetch body as an async iterable (not every browser iterates streams). */
async function* bodyChunks(body) {
  const reader = body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return;
    yield value;
  }
}

/** An OPFS file written chunk by chunk as an image arrives, or null when
 *  OPFS is unavailable. A file cut short does not open (no footer). */
async function opfsAppender(path) {
  try {
    closeHandlesFor(path);
    const { dir, file } = await opfsDir(path, true);
    const h = await (await dir.getFileHandle(file, { create: true })).createSyncAccessHandle();
    h.truncate(0);
    let at = 0;
    return {
      write(bytes) {
        h.write(bytes, { at });
        at += bytes.byteLength;
      },
      close() {
        h.flush();
        h.close();
      },
      async abort() {
        try { h.close(); } catch { /* closed */ }
        try { await dir.removeEntry(file); } catch { /* gone */ }
      },
    };
  } catch {
    return null; // non-secure context, no OPFS, or quota: load without persisting
  }
}

/** A response body as image bytes: a gzip file (`facetful publish`'s
 *  `.gz`, for hosts like GitHub Pages that can't send `Content-Encoding`)
 *  is decompressed here as it streams; anything else passes through (a
 *  host's own `Content-Encoding` is already undone by fetch). `gzip` is
 *  true when this page decompressed it. */
async function imageBody(body) {
  const reader = body.getReader();
  const head = [];
  let have = 0;
  while (have < 2) {
    const { done, value } = await reader.read();
    if (done) break;
    head.push(value);
    have += value.byteLength;
  }
  const first = head[0];
  const b1 = first?.byteLength > 1 ? first[1] : head[1]?.[0];
  const gzip = first?.[0] === 0x1f && b1 === 0x8b;
  // the bytes already read, then the rest of the body
  let stream = new ReadableStream({
    start(c) {
      for (const h of head) c.enqueue(h);
    },
    async pull(c) {
      const { done, value } = await reader.read();
      if (done) c.close();
      else c.enqueue(value);
    },
    cancel(why) {
      return reader.cancel(why);
    },
  });
  if (gzip) stream = stream.pipeThrough(new DecompressionStream("gzip"));
  return { stream, gzip };
}

/** Fetch an image and open it as it arrives (`openStream`): indexed as it
 *  comes with `searchColumns`, written to OPFS with `persist`. */
async function streamImage({ url, searchColumns, persist, expectedBytes }) {
  const r = await fetch(url);
  if (!r.ok || !r.body) throw new Error(`fetch ${url}: HTTP ${r.status}`);
  const { stream, gzip } = await imageBody(r.body);
  // Content-Length is the image's size only when nothing was compressed
  // (images also say their size in their header)
  expectedBytes ??= r.headers.get("content-encoding") || gzip ? 0 : Number(r.headers.get("content-length")) || 0;
  const out = persist ? await opfsAppender(persist) : null;
  let loaded;
  try {
    loaded = await engine.openStream(bodyChunks(stream), {
      expectedBytes,
      searchColumns,
      onChunk: out ? (c) => out.write(c) : undefined,
    });
  } catch (err) {
    await out?.abort();
    throw err;
  }
  out?.close();
  return { ...loaded, persisted: !!out };
}

/** Index the groups a download outran, in the background. */
function finishIndexLater(handle) {
  stepToEnd(handle, supersede(handle));
}

/** Use the search index kept in OPFS at `path` when there is one and it
 *  fits (this build, this table, these columns, whole); true when in use.
 *  It is read straight into wasm memory. */
async function loadSavedIndex(handle, columns, path, latest = () => true) {
  let h;
  try {
    closeHandlesFor(path);
    const { dir, file } = await opfsDir(path, false);
    h = await (await dir.getFileHandle(file)).createSyncAccessHandle();
  } catch {
    return false; // none kept, or no OPFS
  }
  try {
    const n = h.getSize();
    // a newer request may have come while the file opened: it wins
    return n > 0 && latest() && engine.loadSearchIndex(handle, columns, n, (view) => h.read(view, { at: 0 }));
  } finally {
    h.close();
  }
}

/** Keep the table's search index in OPFS at `path`, written straight from
 *  wasm memory; returns the bytes written (0 without an index or OPFS). */
async function saveIndex(handle, path, latest = () => true) {
  await settled(handle);
  const out = await opfsAppender(path);
  if (!out) return 0;
  if (!latest()) {
    await out.abort(); // superseded while the file opened: not this index
    return 0;
  }
  const n = engine.withSavedSearchIndex(handle, (view) => (out.write(view), view.byteLength));
  out.close();
  return n ?? 0;
}

/** Open an OPFS-backed table lazily (metadata now, segments on demand). */
async function opfsOpenTable(name, path, cacheBytes) {
  const { handle, rows, fileLen } = await opfsOpenHandle(path, cacheBytes);
  setTable(name, handle);
  lastTable = handle;
  return { rows, fileLen };
}

/** `opfsOpenTable` without registering the table: its handle. */
async function opfsOpenHandle(path, cacheBytes) {
  // sync access handles are exclusive — reuse ours if this path is already
  // open (positional reads are stateless, so tables can share a handle)
  let fileId = null;
  let h = null;
  for (const [id, e] of opfsFiles) {
    if (e.path === path) {
      fileId = id;
      h = e.handle;
      break;
    }
  }
  const opened = fileId === null;
  if (opened) {
    const { dir, file } = await opfsDir(path, false);
    const fh = await dir.getFileHandle(file);
    h = await fh.createSyncAccessHandle();
    fileId = nextFileId++;
    opfsFiles.set(fileId, { handle: h, path });
  }
  try {
    const { handle, rows } = engine.openOpfsTable(fileId, h.getSize(), cacheBytes);
    return { handle, rows, fileLen: h.getSize() };
  } catch (err) {
    if (opened) {
      opfsFiles.delete(fileId);
      h.close();
    }
    throw err;
  }
}

// ---------------- Parquet transcode (baseline browser compiler) ------------

let hyparquetUrl = "hyparquet"; // bare specifier for bundlers; override in init
let hp = null;

async function loadHyparquet() {
  if (hp) return hp;
  // literal specifier when unconfigured, so bundlers resolve + code-split the
  // loaded only when Parquet is opened; explicit URLs bypass the bundler entirely
  hp =
    hyparquetUrl === "hyparquet"
      ? await import("hyparquet")
      : await import(/* @vite-ignore */ hyparquetUrl);
  return hp;
}

// ZSTD pages need fzstd (vendored, ~3 KB gz), loaded only for a file that uses it
async function compressorsFor(codecs) {
  return codecs.has("ZSTD") ? zstdCompressors(await import("./fzstd.js")) : {};
}

/** Transcode Parquet -> image handle (in wasm memory). */
async function transcodeParquet(buffer) {
  const { rows, columns } = await parquetToColumns(await loadHyparquet(), buffer, { compressorsFor });
  return { rows, img: engine.compileTable(rows, columns) };
}

async function sha256hex(buffer) {
  const d = await crypto.subtle.digest("SHA-256", buffer);
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

self.onmessage = async (e) => {
  const { id, cmd } = e.data;
  const reply = (msg, transfer = []) => postMessage({ id, ...msg }, transfer);
  try {
    if (cmd === "init") {
      if (e.data.hyparquetUrl) hyparquetUrl = e.data.hyparquetUrl;
      const wasmBytes = await (await fetch(e.data.wasmUrl)).arrayBuffer();
      engine = await instantiate(wasmBytes, opfsRead);
      // the ready-made functions (udfs.js) are on by default: they cost
      // nothing until a query calls one, and agents expect date_trunc to exist
      if (e.data.udfs !== false) for (const u of builtinUdfs) engine.registerFunction(u.name, u.signature, u.fn);
      if (e.data.maxHandles != null) {
        if (!Number.isInteger(e.data.maxHandles) || e.data.maxHandles < 1) throw new Error(`maxHandles must be a whole number of 1 or more, not ${e.data.maxHandles}`);
        maxHandles = e.data.maxHandles;
      }
      reply({ ok: true });
    } else if (cmd === "load") {
      const { handle, rows } = engine.openTable(e.data.buffer);
      setTable(e.data.name, handle);
      lastTable = handle;
      reply({ ok: true, rows });
    } else if (cmd === "loadUrl") {
      // stream an image in: the table opens as it arrives, its word index is
      // built a row group at a time meanwhile, and with `persist` the bytes
      // go to OPFS as they come
      const t0 = performance.now();
      const loaded = await streamImage(e.data);
      setTable(e.data.name, loaded.handle);
      lastTable = loaded.handle;
      reply({
        ok: true,
        rows: loaded.rows,
        indexedWhileLoading: loaded.indexedWhileLoading,
        searchReady: loaded.indexLeft === 0,
        persisted: loaded.persisted,
        elapsedMs: performance.now() - t0,
      });
      if (loaded.indexLeft > 0) finishIndexLater(loaded.handle);
    } else if (cmd === "attach" || cmd === "attachOpfs") {
      // the other part of a split image, streamed in (indexed as it comes)
      // or reopened from OPFS, appended to the table's columns
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      const t0 = performance.now();
      const part = cmd === "attach" ? await streamImage(e.data) : await opfsOpenHandle(e.data.path, e.data.cacheBytes);
      // finishes any index build under way on either part, then merges them
      engine.attach(handle, part.handle);
      reply({
        ok: true,
        columns: engine.describe(handle).columns.length,
        persisted: !!part.persisted,
        elapsedMs: performance.now() - t0,
      });
    } else if (cmd === "searchReady") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      await settled(handle);
      reply({ ok: true });
    } else if (cmd === "loadParquet") {
      // transcode-only path: parquet buffer -> in-memory table
      const t0 = performance.now();
      const { rows, img } = await transcodeParquet(e.data.buffer);
      const { handle } = engine.openImage(img);
      setTable(e.data.name, handle);
      lastTable = handle;
      reply({ ok: true, rows, transcodeMs: performance.now() - t0 });
    } else if (cmd === "openParquet") {
      // cached path: reopen the compiled image from OPFS when the same source
      // (by content hash) was seen before; otherwise transcode + persist
      const t0 = performance.now();
      const hash = await sha256hex(e.data.buffer);
      const path = `facetful-cache/${hash}-v${engine.w.format_version()}.facetful`;
      try {
        const { rows } = await opfsOpenTable(e.data.name, path, e.data.cacheBytes);
        reply({ ok: true, rows, source: "cache", openMs: performance.now() - t0 });
        return;
      } catch {
        // cache miss (or OPFS unavailable) — fall through to transcode
      }
      const { rows, img } = await transcodeParquet(e.data.buffer);
      const transcodeMs = performance.now() - t0;
      let cached = false;
      let imageBytes = null;
      try {
        imageBytes = engine.imageBytes(img);
      } catch { /* copy-out failed: open uncached */ }
      const { handle } = engine.openImage(img);
      setTable(e.data.name, handle);
      lastTable = handle;
      if (imageBytes) {
        try {
          await opfsWrite(path, imageBytes);
          cached = true;
        } catch { /* non-secure context or quota: stay memory-only */ }
      }
      reply({ ok: true, rows, source: "transcode", cached, transcodeMs });
    } else if (cmd === "materialize") {
      // derived table: run the query on the source table, compile its result
      // into a new in-memory table under `name`; optionally persist the image
      const src = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!src) throw new Error(`no table loaded${e.data.table ? `: '${e.data.table}'` : ""}`);
      const t0 = performance.now();
      const img = engine.materialize(src, e.data.sql);
      let bytes = 0;
      if (e.data.persist) {
        const image = engine.imageBytes(img);
        bytes = image.byteLength;
        await opfsWrite(e.data.persist, image);
      }
      const { handle, rows } = engine.openImage(img);
      setTable(e.data.name, handle);
      reply({ ok: true, rows, bytes, elapsedMs: performance.now() - t0 });
    } else if (cmd === "loadCsv") {
      // a File/Blob streams twice through the converter; a buffer is one chunk
      const src = e.data.source;
      const chunks = typeof src.stream === "function"
        ? () => src.stream()
        : async function* () { yield new Uint8Array(src); };
      const t0 = performance.now();
      const { bytes, rows, schema } = await engine.convertCsv(chunks, { groupTarget: e.data.groupTarget });
      if (e.data.persist) await opfsWrite(e.data.persist, bytes);
      const { handle } = engine.openTable(bytes);
      setTable(e.data.name, handle);
      lastTable = handle;
      reply({ ok: true, rows, bytes: bytes.byteLength, schema, elapsedMs: performance.now() - t0 });
    } else if (cmd === "registerFunction") {
      // functions don't cross postMessage: `source` is the function's text (an
      // expression — an arrow function, or an IIFE returning one for state)
      const fn = e.data.moduleUrl
        ? (await import(e.data.moduleUrl)).default
        : new Function(`return (${e.data.source});`)();
      if (typeof fn !== "function") throw new Error(`registerFunction('${e.data.name}'): source is not a function`);
      engine.registerFunction(e.data.name, e.data.signature, fn);
      reply({ ok: true });
    } else if (cmd === "unregisterFunction") {
      reply({ ok: engine.unregisterFunction(e.data.name) });
    } else if (cmd === "storeOpfs") {
      await opfsWrite(e.data.path, new Uint8Array(e.data.buffer));
      reply({ ok: true, bytes: e.data.buffer.byteLength });
    } else if (cmd === "loadOpfs") {
      const { rows, fileLen } = await opfsOpenTable(e.data.name, e.data.path, e.data.cacheBytes);
      reply({ ok: true, rows, fileLen });
    } else if (cmd === "removeOpfs") {
      closeHandlesFor(e.data.path);
      const { dir, file } = await opfsDir(e.data.path, false);
      await dir.removeEntry(file);
      reply({ ok: true });
    } else if (cmd === "warm") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      let bytes = 0;
      for (const name of e.data.cols) {
        const idx = engine.colByName(handle, name);
        if (idx < 0) throw new Error(`no column '${name}'`);
        bytes += engine.warmColumn(handle, idx);
        // yield between columns so queued queries interleave with warming
        await new Promise((r) => setTimeout(r, 0));
      }
      reply({ ok: true, bytes });
    } else if (cmd === "buildSearchIndex") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      const t0 = performance.now();
      let gen = 0;
      const latest = () => isLatest(handle, gen);
      // a kept index read back instead of building, when it fits (this
      // request takes over the table's index before waiting on the file)
      if (e.data.cache) {
        gen = supersede(handle);
        if (await loadSavedIndex(handle, e.data.columns, e.data.cache, latest)) {
          reply({ ok: true, ...engine.searchIndexStats(handle), cached: true, elapsedMs: performance.now() - t0 });
          return;
        }
        if (!latest()) throw new Error(SUPERSEDED);
      }
      // a request the engine refuses (a column that is not text) changes
      // nothing: it takes over only once its build has begun
      engine.beginSearchIndex(handle, e.data.columns);
      gen = supersede(handle);
      if (!(await stepToEnd(handle, gen))) throw new Error(SUPERSEDED);
      const elapsedMs = performance.now() - t0;
      const savedBytes = e.data.cache ? await saveIndex(handle, e.data.cache, latest) : 0;
      reply({ ok: true, ...engine.searchIndexStats(handle), cached: false, savedBytes, elapsedMs });
    } else if (cmd === "saveSearchIndex") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      reply({ ok: true, bytes: await saveIndex(handle, e.data.path) });
    } else if (cmd === "searchSuggest") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      const { columns, q, where, limit, didYouMean } = e.data;
      reply({ ok: true, suggestions: engine.searchSuggest(handle, columns, q, { where, limit, didYouMean }) });
    } else if (cmd === "dropSearchIndex") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      supersede(handle); // a build under way stops, and says so
      engine.dropSearchIndex(handle);
      reply({ ok: true });
    } else if (cmd === "setMaskBudget") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      engine.setMaskBudget(handle, e.data.bytes);
      reply({ ok: true });
    } else if (cmd === "describe") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error(`no table loaded${e.data.table ? `: '${e.data.table}'` : ""}`);
      reply({ ok: true, info: engine.describe(handle) });
    } else if (cmd === "memoryStats") {
      // the worker's whole footprint in one number: wasm linear memory never
      // shrinks, so this is its high-water (image copies, caches, results)
      reply({ ok: true, wasmBytes: engine.mem().byteLength, tables: tables.size });
    } else if (cmd === "cacheStats") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      reply({ ok: true, ...engine.cacheStats(handle) });
    } else if (cmd === "prepare") {
      const table = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!table) throw new Error(`no table loaded${e.data.table ? `: '${e.data.table}'` : ""}`);
      if (handles.size >= maxHandles) {
        const open = [...handles.values()].map((o) => `\n  ${brief(o.sql)}`).join("");
        throw new Error(`${handles.size} result handles are open and maxHandles is ${maxHandles}: close() one before preparing another. Open:${open}`);
      }
      const t0 = performance.now();
      const { handle: h, rows, columns, kind } = engine.prepare(table, e.data.sql);
      const elapsedMs = performance.now() - t0;
      const handle = nextHandle++;
      handles.set(handle, { h, sql: e.data.sql });
      reply({ ok: true, handle, rows, columns, kind, elapsedMs });
    } else if (cmd === "page") {
      const { h } = useHandle(e.data.handle);
      const t0 = performance.now();
      const result = engine.preparedPage(h, e.data.start, e.data.end, { columns: e.data.columns, dictText: e.data.dictText ?? false });
      result.elapsedMs = performance.now() - t0;
      reply({ ok: true, result }, transferables(result));
    } else if (cmd === "sortHandle") {
      const { h } = useHandle(e.data.handle);
      const t0 = performance.now();
      const rows = engine.preparedSort(h, e.data.orderBy);
      reply({ ok: true, rows, elapsedMs: performance.now() - t0 });
    } else if (cmd === "closeHandle") {
      const entry = handles.get(e.data.handle);
      if (entry) {
        engine.preparedFree(entry.h);
        handles.delete(e.data.handle);
      }
      reply({ ok: true });
    } else if (cmd === "queryBatch") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error(`no table loaded${e.data.table ? `: '${e.data.table}'` : ""}`);
      const t0 = performance.now();
      const results = engine.queryBatch(handle, e.data.sqls, { dictText: e.data.dictText ?? false });
      // the batch's time on every result: the statements run interleaved,
      // so there is no per-statement time (index.d.ts says not to sum them)
      const elapsedMs = performance.now() - t0;
      for (const r of results) r.elapsedMs = elapsedMs;
      reply({ ok: true, results }, results.flatMap(transferables));
    } else if (cmd === "query") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error(`no table loaded${e.data.table ? `: '${e.data.table}'` : ""}`);
      const t0 = performance.now();
      const result = engine.query(handle, e.data.sql, { dictText: e.data.dictText ?? false });
      result.elapsedMs = performance.now() - t0;
      reply({ ok: true, result }, transferables(result));
    } else {
      throw new Error(`unknown command '${cmd}'`);
    }
  } catch (err) {
    reply({
      error: String(err && err.message ? err.message : err),
      isQueryError: err instanceof QueryError,
    });
  }
};
