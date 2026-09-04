// The engine's dedicated worker: owns the wasm instance and all tables.
// Protocol: {id, cmd, ...} in, {id, ok|error, ...} out; query column buffers
// are transferred, not cloned.

import { instantiate, transferables, QueryError } from "./core.js";

let engine = null;
const tables = new Map(); // name -> handle
let lastTable = null;

self.onmessage = async (e) => {
  const { id, cmd } = e.data;
  const reply = (msg, transfer = []) => postMessage({ id, ...msg }, transfer);
  try {
    if (cmd === "init") {
      const wasmBytes = await (await fetch(e.data.wasmUrl)).arrayBuffer();
      engine = await instantiate(wasmBytes);
      reply({ ok: true });
    } else if (cmd === "load") {
      const { handle, rows } = engine.openTable(e.data.buffer);
      tables.set(e.data.name, handle);
      lastTable = handle;
      reply({ ok: true, rows });
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
