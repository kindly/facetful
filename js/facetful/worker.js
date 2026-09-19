// The engine's dedicated worker: owns the wasm instance, all tables, and the
// OPFS sync access handles (createSyncAccessHandle only exists in workers).
// Protocol: {id, cmd, ...} in, {id, ok|error, ...} out; query column buffers
// are transferred, not cloned.

import { instantiate, transferables, QueryError } from "./core.js";
import { parquetToColumns } from "./parquet.js";

let engine = null;
const tables = new Map(); // name -> handle
// register under a name for FROM / JOIN from other tables' queries
const setTable = (name, handle) => { setTable(name, handle); engine.catalogRegister(name, handle); };
let lastTable = null;

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

/** Open an OPFS-backed table lazily (metadata now, segments on demand). */
async function opfsOpenTable(name, path, cacheBytes) {
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
    setTable(name, handle);
    lastTable = handle;
    return { rows, fileLen: h.getSize() };
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
  // optional peer dependency; explicit URLs bypass the bundler entirely
  hp =
    hyparquetUrl === "hyparquet"
      ? await import("hyparquet")
      : await import(/* @vite-ignore */ hyparquetUrl);
  return hp;
}

/** Transcode Parquet -> image handle (in wasm memory). */
async function transcodeParquet(buffer) {
  const { rows, columns } = await parquetToColumns(await loadHyparquet(), buffer);
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
      reply({ ok: true });
    } else if (cmd === "load") {
      const { handle, rows } = engine.openTable(e.data.buffer);
      setTable(e.data.name, handle);
      lastTable = handle;
      reply({ ok: true, rows });
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
    } else if (cmd === "setMaskBudget") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      engine.setMaskBudget(handle, e.data.bytes);
      reply({ ok: true });
    } else if (cmd === "cacheStats") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error("no table loaded");
      reply({ ok: true, ...engine.cacheStats(handle) });
    } else if (cmd === "query") {
      const handle = e.data.table ? tables.get(e.data.table) : lastTable;
      if (!handle) throw new Error(`no table loaded${e.data.table ? `: '${e.data.table}'` : ""}`);
      const t0 = performance.now();
      const result = engine.query(handle, e.data.sql);
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
