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
  static async open({ wasmUrl, workerUrl, hyparquetUrl } = {}) {
    // the no-argument form must stay a literal `new Worker(new URL(...))`
    // expression: bundlers (Vite, webpack) statically analyze exactly that
    // pattern to compile the worker graph
    const worker = workerUrl
      ? new Worker(workerUrl, { type: "module" })
      : new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
    const db = new Facetful(worker);
    await db._call({
      cmd: "init",
      wasmUrl: String(wasmUrl ?? new URL("./facetful_wasm.wasm", import.meta.url)),
      hyparquetUrl,
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

  /**
   * Open a Parquet file (ArrayBuffer) — the headline path. The image compiled
   * from it is cached in OPFS keyed by content hash + format version, so a
   * repeat visit with the same file reopens zero-decode without transcoding.
   * Returns { rows, source: "cache" | "transcode", ... timings }.
   */
  async openParquet(name, buffer, { cacheBytes } = {}) {
    return this._call(
      { cmd: "openParquet", name, buffer, cacheBytes: cacheBytes ?? defaultCacheBudget() },
      [buffer],
    );
  }

  /** Transcode-only variant: Parquet -> in-memory table, no OPFS involved. */
  async loadParquet(name, buffer) {
    return this._call({ cmd: "loadParquet", name, buffer }, [buffer]);
  }

  /**
   * Materialize a query's result as a new table named `name`: a derived,
   * immutable table you then query like any other (`{ table: name }`).
   * Runs against `table` (default: the last loaded). With `persist`, the
   * compiled image is also written to OPFS at that path, so a later visit
   * can `loadOpfs(name, path)` instead of recomputing. Returns { rows, bytes }.
   *
   * The SQL may name other loaded tables in FROM and JOIN, so a joined,
   * persisted table is `materialize("x", "select … from t join dims d using (k)")`.
   */
  async materialize(name, sql, { table, persist } = {}) {
    return this._call({ cmd: "materialize", name, sql, table, persist });
  }

  /** Persist a .facetful image into OPFS at `path` (e.g. "facetful/plants.facetful"). */
  async storeOpfs(path, buffer) {
    return this._call({ cmd: "storeOpfs", path, buffer }, [buffer]);
  }

  /**
   * Open a table over an OPFS file — the spill-over path. Only metadata is
   * read up front; column segments load on demand into an LRU cache bounded
   * by `cacheBytes` (default: min(deviceMemory/4, 1GB)).
   */
  async loadOpfs(name, path, { cacheBytes } = {}) {
    const budget = cacheBytes ?? defaultCacheBudget();
    const { rows, fileLen } = await this._call({ cmd: "loadOpfs", name, path, cacheBytes: budget });
    return { name, rows, fileLen, cacheBytes: budget };
  }

  async removeOpfs(path) {
    return this._call({ cmd: "removeOpfs", path });
  }

  /** Pre-touch columns into the cache (background; queries interleave). */
  async warm(cols, { table } = {}) {
    const { bytes } = await this._call({ cmd: "warm", cols, table });
    return { bytes };
  }

  /** { segments, bytes } currently held by a table's segment cache. */
  async cacheStats({ table } = {}) {
    const { segments, bytes } = await this._call({ cmd: "cacheStats", table });
    return { segments, bytes };
  }

  /** Filter-mask cache byte budget for a table (default 16 MB); 0 disables it. */
  async setMaskBudget(bytes, { table } = {}) {
    await this._call({ cmd: "setMaskBudget", bytes, table });
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

// Cache budget when the caller doesn't set one: a quarter of device memory,
// capped at 1GB. navigator.deviceMemory is Chromium-only; assume 4GB elsewhere.
function defaultCacheBudget() {
  const gb = (typeof navigator !== "undefined" && navigator.deviceMemory) || 4;
  return Math.min((gb / 4) * 1024 ** 3, 1024 ** 3);
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
    case "date": // days since epoch -> "YYYY-MM-DD"
      return new Date(c.values[i] * 86400000).toISOString().slice(0, 10);
    case "timestamp": // ms since epoch -> ISO, UTC
      return new Date(c.values[i]).toISOString().replace("T", " ").slice(0, 19);
    default:
      return dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1]));
  }
}
