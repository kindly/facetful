// facetful — main-thread API. The engine runs in a dedicated worker; results
// arrive as transferable column buffers and are wrapped for convenience here.
//
//   const db = await Facetful.open({ wasmUrl });
//   await db.load("plants", await (await fetch("plants.facetful")).arrayBuffer());
//   const r = await db.query("select country, count(*) n from t group by country order by n desc");
//   r.column("country")        // Array<string|null> (materialized on demand)
//   r.columnRaw("n")           // { values: Float64Array, validity } — near-zero copy
//   [...r.rows()]              // row objects, materialized lazily

export class Facetful {
  static async open({ wasmUrl, workerUrl } = {}) {
    const url = workerUrl ?? new URL("./worker.js", import.meta.url);
    const worker = new Worker(url, { type: "module" });
    const db = new Facetful(worker);
    await db._call({
      cmd: "init",
      wasmUrl: String(wasmUrl ?? new URL("./facetful_wasm.wasm", import.meta.url)),
    });
    return db;
  }

  constructor(worker) {
    this._worker = worker;
    this._pending = new Map();
    this._nextId = 1;
    worker.onmessage = (e) => {
      const { id, ...msg } = e.data;
      const p = this._pending.get(id);
      if (!p) return;
      this._pending.delete(id);
      if (msg.error) p.reject(new Error(msg.error));
      else p.resolve(msg);
    };
  }

  _call(msg, transfer = []) {
    const id = this._nextId++;
    return new Promise((resolve, reject) => {
      this._pending.set(id, { resolve, reject });
      this._worker.postMessage({ id, ...msg }, transfer);
    });
  }

  /** Load a .facetful image (ArrayBuffer). Registered under `name`. */
  async load(name, buffer) {
    const { rows } = await this._call({ cmd: "load", name, buffer }, [buffer]);
    return { name, rows };
  }

  /** Run SQL. `table` selects a loaded table (defaults to the last loaded). */
  async query(sql, { table } = {}) {
    const { result } = await this._call({ cmd: "query", sql, table });
    return new Result(result);
  }

  close() {
    this._worker.terminate();
  }
}

const dec = new TextDecoder();

export class Result {
  constructor(r) {
    this.columns = r.columns.map((c) => ({ name: c.name, kind: c.kind }));
    this.rowCount = r.rowCount;
    this.stats = r.stats;
    this.elapsedMs = r.elapsedMs;
    this._cols = r.columns;
  }

  _find(name) {
    const c = this._cols.find((c) => c.name === name);
    if (!c) throw new Error(`no result column '${name}'`);
    return c;
  }

  /** Raw buffers: { kind, values | offsets+bytes, validity }. */
  columnRaw(name) {
    return this._find(name);
  }

  /** Materialized values with nulls, in row order. */
  column(name) {
    const c = this._find(name);
    const out = new Array(this.rowCount);
    for (let i = 0; i < this.rowCount; i++) {
      out[i] = cellValue(c, i);
    }
    return out;
  }

  *rows() {
    for (let i = 0; i < this.rowCount; i++) {
      const o = {};
      for (const c of this._cols) o[c.name] = cellValue(c, i);
      yield o;
    }
  }
}

function cellValue(c, i) {
  if ((c.validity[i >> 3] & (1 << (i & 7))) === 0) return null;
  switch (c.kind) {
    case "int":
      return c.values[i];
    case "float":
      return c.values[i];
    case "bool":
      return c.values[i] !== 0;
    default:
      return dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1]));
  }
}
