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
  static async open({ wasmUrl, workerUrl, hyparquetUrl, udfs = true, maxHandles } = {}) {
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
      udfs,
      maxHandles,
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
   * Fetch a .facetful image and open it as it arrives. With `searchColumns`
   * the search index is built a row group at a time during the download and
   * is ready with the table (`indexedWhileLoading` row groups were done
   * before the last byte). When the download outruns indexing the table
   * opens anyway (`searchReady: false`): the rest is indexed in the
   * background, search() scans meanwhile (words as typed, without the
   * index's corrections and word forms), and `searchReady()` resolves when
   * the index is in use. `persist` (an OPFS path) writes the image there
   * as it arrives, for `loadOpfs` on a later visit; without OPFS it is
   * skipped (`persisted: false`). `expectedBytes` sizes the buffer once when
   * the server compresses the response (otherwise Content-Length does).
   */
  async loadUrl(name, url, { searchColumns, persist, expectedBytes } = {}) {
    const { rows, indexedWhileLoading, searchReady, persisted, elapsedMs } = await this._call({
      cmd: "loadUrl",
      name,
      url: String(new URL(url, globalThis.location?.href)),
      searchColumns,
      persist,
      expectedBytes,
    });
    return { name, rows, indexedWhileLoading, searchReady, persisted, elapsedMs };
  }

  /**
   * Fetch the other part of a table's split image (`facetful split`) and
   * append its columns to the table as one: same rows, and queries over
   * columns of both parts. With `searchColumns` (of this part) it is indexed
   * as it arrives, and the table's index and its become one over both, so
   * search() then takes the columns of both. `persist` writes it to OPFS
   * as it arrives (`attachOpfs` on a later visit).
   */
  async attach(url, { table, searchColumns, persist, expectedBytes } = {}) {
    const { columns, persisted, elapsedMs } = await this._call({
      cmd: "attach",
      table,
      url: String(new URL(url, globalThis.location?.href)),
      searchColumns,
      persist,
      expectedBytes,
    });
    return { columns, persisted, elapsedMs };
  }

  /** `attach` from a part stored in OPFS (opened lazily, like `loadOpfs`). */
  async attachOpfs(path, { table, cacheBytes } = {}) {
    const budget = cacheBytes ?? defaultCacheBudget();
    const { columns, elapsedMs } = await this._call({ cmd: "attachOpfs", table, path, cacheBytes: budget });
    return { columns, elapsedMs };
  }

  /** Resolves when the table's search index, from `loadUrl`, is in use. */
  async searchReady({ table } = {}) {
    await this._call({ cmd: "searchReady", table });
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

  /**
   * Convert a CSV (a File/Blob, or an ArrayBuffer) into a table named `name`,
   * streaming in bounded memory — two passes over the input, so a File is
   * read twice. Types are inferred (int, float, date, timestamp, text;
   * repeated text becomes a dictionary). Optionally persist the image to OPFS.
   */
  async loadCsv(name, source, { persist, groupTarget } = {}) {
    const transfer = source instanceof ArrayBuffer ? [source] : [];
    return this._call({ cmd: "loadCsv", name, source, persist, groupTarget }, transfer);
  }

  /**
   * Register a user-defined scalar function, callable from any query.
   * `signature` = { params: kind[], returns: kind, strict?, variadic?, perRow? }
   * with kinds "int" | "float" | "bool" | "text" | "date" | "timestamp".
   * `fn` runs in the worker, so pass a self-contained function (its source is
   * sent — no closures over your variables) or `{ moduleUrl }` whose default
   * export is the function. Vectorized by default: fn(args, len, out) is
   * called once per lane (see core.js Engine.registerFunction); with
   * `perRow: true` it is called per row with plain values and returns one.
   */
  async registerFunction(name, signature, fn) {
    const msg = { cmd: "registerFunction", name, signature };
    if (typeof fn === "function") msg.source = fn.toString();
    else if (fn && fn.moduleUrl) msg.moduleUrl = fn.moduleUrl;
    else if (typeof fn === "string") msg.source = fn;
    else throw new Error("registerFunction: pass a function, its source text, or { moduleUrl }");
    return this._call(msg);
  }

  async unregisterFunction(name) {
    return this._call({ cmd: "unregisterFunction", name });
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

  /**
   * Build the word index search() uses over these text columns, in the
   * background: a row group at a time, with queries answered in between
   * (search() scans until the index is finished). Resolves when it is in
   * use. Needs a build with search (not the lite build). With `cache` (an
   * OPFS path) an index kept there is read back instead when it fits this
   * table and these columns (`cached: true`); otherwise the index is built
   * and kept there for next time. Key the path by the data's version.
   */
  async buildSearchIndex(columns, { table, cache } = {}) {
    const { pairs, bytes, elapsedMs, cached, savedBytes } = await this._call({ cmd: "buildSearchIndex", columns, table, cache });
    return { pairs, bytes, elapsedMs, cached: !!cached, savedBytes: savedBytes ?? 0 };
  }

  /**
   * Keep the table's search index in OPFS at `path` (after `loadUrl` or
   * `attach` built it), for `buildSearchIndex(columns, { cache: path })` to
   * read back on a later visit. Resolves with the bytes written (0 without
   * an index, or without OPFS).
   */
  async saveSearchIndex(path, { table } = {}) {
    const { bytes } = await this._call({ cmd: "saveSearchIndex", path, table });
    return { bytes };
  }

  /**
   * What to offer for a search query typed so far over these columns:
   * completions of the word being typed, most common first, each found in
   * some row the other words and `where` (the app's facets, as a SQL
   * condition) leave; the words search() corrected by itself; and with
   * `didYouMean` a better query when there is one. No counts: run the query. Completions and corrections need the search
   * index over exactly these columns (`indexed` says whether it answered).
   */
  async searchSuggest(q, { columns, where = "", limit = 8, didYouMean = false, table } = {}) {
    const { suggestions } = await this._call({ cmd: "searchSuggest", q, columns, where, limit, didYouMean, table });
    return suggestions;
  }

  /** Drop a table's search index (search() scans again). */
  async dropSearchIndex({ table } = {}) {
    await this._call({ cmd: "dropSearchIndex", table });
  }

  /** Pre-touch columns into the cache (background; queries interleave). */
  async warm(cols, { table } = {}) {
    const { bytes } = await this._call({ cmd: "warm", cols, table });
    return { bytes };
  }

  /** { segments, bytes } currently held by a table's segment cache. */
  /** A loaded table's catalog: rows, row groups, sort keys, and per column its
   *  kind, on-disk bytes, null count, min/max and dictionary size. */
  async describe({ table } = {}) {
    const { info } = await this._call({ cmd: "describe", table });
    return info;
  }

  /** The worker's wasm memory size in bytes (its high-water: wasm memory never
   *  shrinks) and how many tables it holds. For reporting a page's memory
   *  split between the worker and the page's own result buffers. */
  async memoryStats() {
    const { wasmBytes, tables } = await this._call({ cmd: "memoryStats" });
    return { wasmBytes, tables };
  }

  async cacheStats({ table } = {}) {
    const { segments, bytes } = await this._call({ cmd: "cacheStats", table });
    return { segments, bytes };
  }

  /** Filter-mask cache byte budget for a table (default 16 MB); 0 disables it. */
  async setMaskBudget(bytes, { table } = {}) {
    await this._call({ cmd: "setMaskBudget", bytes, table });
  }

  /** Run SQL — one statement, or an array of statements in one call (the
   *  facet-shaped ones share masks and lanes and run fused; see index.d.ts).
   *  `table` selects a loaded table (defaults to the last loaded).
   *  `dictText` (true | "all"): text columns come back as codes + a dictionary
   *  on `columnRaw` — true for the image's dictionary columns (free), "all"
   *  for any text column where that pays (a hash pass); rows()/column() still
   *  return strings. */
  async query(sql, { table, dictText } = {}) {
    if (Array.isArray(sql)) {
      // a batch: one message, one engine call; facet-shaped statements fuse
      const { results } = await this._call({ cmd: "queryBatch", sqls: sql, table, dictText });
      return results.map((r) => new Result(r));
    }
    const { result } = await this._call({ cmd: "query", sql, table, dictText });
    return new Result(result);
  }

  /**
   * Run SQL and keep its result in the worker, for a grid that shows a few
   * hundred rows of a result of millions: `page(start, end)` gives any block
   * of rows, `sort(orderBy)` re-orders it, and nothing else crosses to this
   * thread. For a plain SELECT over the table the worker keeps only the row
   * ids (4 bytes a row; nothing for an unfiltered, unsorted SELECT) and
   * gathers each page from the table; a grouped or joined result is kept
   * whole. `dictText` is the default for its pages. See ResultHandle.
   */
  async prepare(sql, { table, dictText } = {}) {
    const { handle, rows, columns, kind, elapsedMs } = await this._call({ cmd: "prepare", sql, table });
    const h = new ResultHandle(this, { handle, rows, columns, kind, elapsedMs, dictText });
    forgotten?.register(h, { db: this, handle, sql }, h);
    return h;
  }

  close() {
    this._worker.terminate();
  }
}

// A ResultHandle collected without close(): free its place in the worker's
// pool and say so. Collection can come late or never, so this is a net
// under close(), not a replacement for it.
const forgotten =
  typeof FinalizationRegistry === "undefined"
    ? null
    : new FinalizationRegistry(({ db, handle, sql }) => {
        console.warn(`facetful: a result handle was dropped without close(): ${sql.replace(/\s+/g, " ").trim().slice(0, 80)}`);
        db._call({ cmd: "closeHandle", handle }).catch(() => {});
      });

/** A result kept in the worker (`Facetful.prepare`). */
export class ResultHandle {
  constructor(db, { handle, rows, columns, kind, elapsedMs, dictText }) {
    this._db = db;
    this._handle = handle;
    this._dictText = dictText;
    this.rows = rows;
    this.columns = columns;
    this.kind = kind;
    this.elapsedMs = elapsedMs;
    this.generation = 0;
    this.closed = false;
  }

  /** Rows start..end (end exclusive; clamped to the result) as a Result,
   *  with `start` and the handle's `generation` when it was asked for: a
   *  page that comes back with an older generation than the handle's was
   *  asked for before a `sort()` and shows the old order. `columns` picks
   *  result columns by name, in that order (default all). */
  async page(start, end, { columns, dictText = this._dictText } = {}) {
    const generation = this.generation;
    const { result } = await this._db._call({ cmd: "page", handle: this._handle, start, end, columns, dictText });
    const r = new Result(result);
    r.start = start;
    r.generation = generation;
    return r;
  }

  /** Re-order the result by `orderBy`, the text after ORDER BY ("" for the
   *  statement without one): what the statement would give with that ORDER
   *  BY in place of its own, under the same LIMIT/OFFSET. `generation`
   *  moves on at once; pages asked for from now on have the new order. When
   *  it is refused (a name that is no column) the order stays as it was. */
  async sort(orderBy) {
    const generation = ++this.generation;
    const { rows, elapsedMs } = await this._db._call({ cmd: "sortHandle", handle: this._handle, orderBy: orderBy ?? "" });
    this.rows = rows;
    return { rows, generation, elapsedMs };
  }

  /** Free the result in the worker. `page` and `sort` reject afterwards. */
  async close() {
    if (this.closed) return;
    this.closed = true;
    forgotten?.unregister(this);
    await this._db._call({ cmd: "closeHandle", handle: this._handle });
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

  /** Raw buffers: { kind, values | offsets+bytes | codes+dict, validity }. */
  columnRaw(name) {
    return this._find(name);
  }

  /** The whole decoded dictionary of a `dictText` column (index by a row's
   *  code), or null when the column came as per-row text. Decodes every value
   *  on first call — fine for hundreds or thousands of values; for a
   *  dictionary of a million (a `"all"`-encoded title column) use
   *  `dictValue`, which decodes one code at a time and caches it. */
  dictionary(name) {
    const c = this._find(name);
    return c.codes ? dictStrings(c) : null;
  }

  /** One dictionary value of a `dictText` column by code, decoded on first
   *  use and cached — the fast path for a reader that touches a few rows of
   *  a result with a big dictionary. Null when the column came as per-row
   *  text or the code is out of range. */
  dictValue(name, code) {
    const c = this._find(name);
    if (!c.codes || code < 0 || code >= c.dict.offsets.length - 1) return null;
    return dictValueAt(c, code);
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

// per column: its decoded dictionary values, filled lazily by code so that
// touching one row of a million-value dictionary decodes one string, not all
const dictCache = new WeakMap();
function dictSlots(c) {
  let strs = dictCache.get(c);
  if (!strs) {
    strs = new Array(c.dict.offsets.length - 1);
    dictCache.set(c, strs);
  }
  return strs;
}
function dictValueAt(c, k) {
  const strs = dictSlots(c);
  let s = strs[k];
  if (s === undefined) {
    s = dec.decode(c.dict.bytes.subarray(c.dict.offsets[k], c.dict.offsets[k + 1]));
    strs[k] = s;
  }
  return s;
}
function dictStrings(c) {
  const strs = dictSlots(c);
  for (let k = 0; k < strs.length; k++) if (strs[k] === undefined) strs[k] = dec.decode(c.dict.bytes.subarray(c.dict.offsets[k], c.dict.offsets[k + 1]));
  return strs;
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
      if (c.codes) return dictValueAt(c, c.codes[i]);
      return dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1]));
  }
}
