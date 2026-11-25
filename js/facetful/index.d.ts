// Type definitions for facetful — the main-thread API (index.js).
// The engine runs in a dedicated worker; all methods are asynchronous.

export type ColumnKind = "int" | "float" | "bool" | "text" | "date" | "timestamp";

export interface OpenOptions {
  /** URL of the engine wasm. Default: facetful_wasm.wasm next to the package. */
  wasmUrl?: string | URL;
  /** URL of the worker module. Default: worker.js next to the package. */
  workerUrl?: string | URL;
  /**
   * Module specifier or URL for hyparquet (needed only by the Parquet
   * methods). Default "hyparquet" — resolved by your bundler; pass an
   * explicit URL when running without one.
   */
  hyparquetUrl?: string;
  /** register the ready-made functions from `facetful/udfs` at open (default true) */
  udfs?: boolean;
  /** how many `prepare` results may be open at once (default 5): when that
   *  many are, `prepare` rejects until one is closed */
  maxHandles?: number;
}

export interface QueryStats {
  /** Row groups actually scanned (min/max pruning and early exit skip the rest). */
  scannedGroups: number;
  totalGroups: number;
}

export interface RawColumn {
  name: string;
  kind: ColumnKind;
  /** Bitmap, bit i set = row i is non-null. */
  validity: Uint8Array;
  /** int/float/date/timestamp: f64 lanes (date = days since epoch, timestamp = ms). bool: 0/1 bytes. */
  values?: Float64Array | Uint8Array;
  /** text only: rowCount+1 byte offsets into `bytes`. */
  offsets?: Uint32Array;
  /** text only: UTF-8 blob. */
  bytes?: Uint8Array;
  /** text queried with `dictText`, when a dictionary pays (see `query`): one
   *  code per row into `dict` (NULL rows per `validity`); `offsets`/`bytes`
   *  are then absent. Uint32Array when the dictionary has more than 65,535
   *  values. */
  codes?: Uint16Array | Uint32Array;
  /** with `codes`: the distinct values present, laid out like a text column
   *  (`offsets` has one more entry than there are values) — in the image's
   *  dictionary order for a dictionary column, first appearance otherwise. */
  dict?: { offsets: Uint32Array; bytes: Uint8Array };
}

export type CellValue = number | string | boolean | null;

export interface TableInfo {
  version: number;
  rows: number;
  groups: number;
  target: number;
  sortedBy: { column: string; descending: boolean }[];
  columns: {
    name: string;
    kind: string;
    bytes: number;
    nulls: number;
    min?: number;
    max?: number;
    dict?: number;
    /** True when each non-NULL value forms one run in file order: then
     *  `count(distinct)` over it counts runs instead of hashing values. */
    clustered?: boolean;
  }[];
}

export declare class Result {
  columns: { name: string; kind: ColumnKind }[];
  rowCount: number;
  stats: QueryStats;
  /** Execution time inside the worker, ms (excludes the message hop). */
  elapsedMs: number;
  /** Raw transferred buffers for a column — near-zero copy, ideal for charts. */
  columnRaw(name: string): RawColumn;
  /** The whole decoded dictionary of a `dictText` column (index it by a row's
   *  code), or null when the column came as per-row text. Decodes every value
   *  on first call: right for hundreds or thousands of values, wrong for a
   *  million (an `"all"`-encoded title column took 0.7 s this way) — there,
   *  use `dictValue` or read `dict.offsets`/`dict.bytes` yourself. */
  dictionary(name: string): string[] | null;
  /** One dictionary value by code, decoded on first use and cached: the fast
   *  path for readers that touch a few rows of a result with a big
   *  dictionary. Null for a per-row text column or an out-of-range code. */
  dictValue(name: string, code: number): string | null;
  /** Materialized values with nulls; date/timestamp as ISO strings. */
  column(name: string): CellValue[];
  /** Row objects, materialized lazily. */
  rows(): Generator<Record<string, CellValue>>;
}

export type LaneKind = "int" | "float" | "bool" | "text" | "date" | "timestamp";
export interface UdfSignature {
  /** parameter types; "any" accepts every type (the lane still carries its real kind) */
  params: (LaneKind | "any")[];
  returns: LaneKind;
  /** how many trailing parameters may be omitted */
  optional?: number;
  /** NULL in → NULL out without calling the function for that row (default true) */
  strict?: boolean;
  /** the last parameter type repeats */
  variadic?: boolean;
  /** call per row with plain values instead of once per lane */
  perRow?: boolean;
}
export interface UdfLane {
  kind: LaneKind;
  /** Float64Array for int/float/bool/date/timestamp, string[] for text */
  values: Float64Array | string[];
  /** validity bitmap (bit set = present), or null when all present */
  valid: Uint8Array | null;
  /** a literal argument: one value, applies to every row */
  broadcast: boolean;
}
export type UdfVectorFn = (
  args: UdfLane[],
  len: number,
  out: { values: Float64Array | Uint8Array | (string | null)[]; valid: Uint8Array },
) => void;
export type UdfRowFn = (...values: (number | string | null)[]) => number | string | boolean | null;

export interface LoadResult {
  name: string;
  rows: number;
}

export interface OpenParquetResult {
  rows: number;
  /** "cache" = compiled image reopened from OPFS (no transcode); "transcode" = first visit. */
  source: "cache" | "transcode";
  /** Present when source = "cache". */
  openMs?: number;
  /** Present when source = "transcode". */
  transcodeMs?: number;
  /** Whether the compiled image was persisted to OPFS for next time. */
  cached?: boolean;
}

export declare class Facetful {
  /** Start the engine (spawns the worker, instantiates wasm). */
  static open(options?: OpenOptions): Promise<Facetful>;

  /** Load a .facetful image from an ArrayBuffer (transferred). */
  load(name: string, buffer: ArrayBuffer): Promise<LoadResult>;

  /**
   * Fetch a .facetful image, plain or packed (`facetful pack`: unpacked a
   * row group at a time), and open it as it arrives. With `searchColumns`
   * the search index is built a row group at a time during the download and
   * is ready with the table, or finishes in the background when the download
   * outruns indexing (`searchReady: false`; see `searchReady()`). `persist` (an OPFS path) writes the image there
   * as it arrives (a packed image is written unpacked), for `loadOpfs` on a later visit. `expectedBytes` sizes the
   * buffer once when the server compresses the response.
   */
  loadUrl(
    name: string,
    url: string | URL,
    options?: { searchColumns?: string[]; persist?: string; expectedBytes?: number },
  ): Promise<{ name: string; rows: number; indexedWhileLoading: number; searchReady: boolean; persisted: boolean; elapsedMs: number }>;

  /**
   * Fetch the other part of a table's split image (`facetful split`) and
   * append its columns to the table. With `searchColumns` (of this part) it
   * is indexed as it arrives and the two indexes become one over both
   * parts' columns. `persist` writes it to OPFS as it arrives.
   */
  attach(
    url: string | URL,
    options?: { table?: string; searchColumns?: string[]; persist?: string; expectedBytes?: number },
  ): Promise<{ columns: number; persisted: boolean; elapsedMs: number }>;

  /** `attach` from a part stored in OPFS (opened lazily, like `loadOpfs`). */
  attachOpfs(path: string, options?: { table?: string; cacheBytes?: number }): Promise<{ columns: number; elapsedMs: number }>;

  /**
   * Resolves when the table's search index, from `loadUrl`, is in use.
   * Until then (`searchReady: false`: the download outran indexing),
   * `search()` scans the rows and matches words as typed, the last as a
   * prefix: no spelling corrections and no other forms of a word, so
   * `countr owner` finds nothing that the index would correct to
   * `country owner`. `searchSuggest` says which it was (`indexed`). Await
   * this before a search that should behave as the index does.
   */
  searchReady(options?: { table?: string }): Promise<void>;

  /**
   * Open a Parquet file (ArrayBuffer, transferred). The compiled image is
   * cached in OPFS keyed by content hash + format version: repeat visits
   * reopen in ~tens of ms with no transcode. Requires hyparquet (see
   * OpenOptions.hyparquetUrl) and a secure context for the OPFS cache
   * (degrades to memory-only otherwise).
   */
  openParquet(
    name: string,
    buffer: ArrayBuffer,
    options?: { cacheBytes?: number },
  ): Promise<OpenParquetResult>;

  /** Transcode-only variant: Parquet -> in-memory table, no OPFS. */
  loadParquet(name: string, buffer: ArrayBuffer): Promise<{ rows: number; transcodeMs: number }>;

  /** Persist a .facetful image into OPFS at `path` (buffer transferred). */
  /**
   * Materialize a query's result as a new table `name`, queryable via
   * `{ table: name }`. With `persist`, the image is also written to OPFS at
   * that path for a later `loadOpfs`. `bytes` is the persisted image size (0
   * when not persisted).
   */
  materialize(
    name: string,
    sql: string,
    options?: { table?: string; persist?: string },
  ): Promise<{ rows: number; bytes: number; elapsedMs: number }>;
  storeOpfs(path: string, buffer: ArrayBuffer): Promise<{ bytes: number }>;
  /**
   * Register a user-defined scalar function (runs in the worker; pass a
   * self-contained function, its source, or `{ moduleUrl }`). Vectorized by
   * default — `fn(args, len, out)` once per lane; `perRow: true` calls
   * `fn(...values)` per row, returning a value or null.
   */
  /** Stream a CSV (File/Blob or ArrayBuffer) into a table; two passes, bounded memory. */
  loadCsv(
    name: string,
    source: Blob | ArrayBuffer,
    options?: { persist?: string; groupTarget?: number },
  ): Promise<{ rows: number; bytes: number; schema: { name: string; kind: string }[]; elapsedMs: number }>;
  registerFunction(
    name: string,
    signature: UdfSignature,
    fn: UdfVectorFn | UdfRowFn | string | { moduleUrl: string },
  ): Promise<{ ok: true }>;
  unregisterFunction(name: string): Promise<{ ok: boolean }>;

  /**
   * Open a table over an OPFS file: metadata reads now, column segments load
   * lazily into an LRU bounded by cacheBytes (default min(deviceMemory/4, 1GB)).
   */
  loadOpfs(
    name: string,
    path: string,
    options?: { cacheBytes?: number },
  ): Promise<{ name: string; rows: number; fileLen: number; cacheBytes: number }>;

  /** Delete an OPFS file (closes any open handles on it first). */
  removeOpfs(path: string): Promise<void>;

  /**
   * Build the word index search() uses over these text columns, in the
   * background: a row group at a time, with queries answered in between
   * (search() scans until the index is finished). Resolves when it is in use.
   * Not in the lite build. With `cache` (an OPFS path) an index kept there is
   * read back instead when it fits this table and these columns (`cached`);
   * otherwise it is built and kept there. Key the path by the data's version.
   */
  buildSearchIndex(
    columns: string[],
    options?: { table?: string; cache?: string },
  ): Promise<{ pairs: number; bytes: number; elapsedMs: number; cached: boolean; savedBytes: number }>;

  /** Keep the table's search index in OPFS at `path` (after `loadUrl` or
   *  `attach` built it), for `buildSearchIndex(columns, { cache: path })`. */
  saveSearchIndex(path: string, options?: { table?: string }): Promise<{ bytes: number }>;

  /**
   * What to offer for a search query typed so far over these columns:
   * completions of the word being typed, most common first, each found in
   * some row the other words and `where` (the app's facets, as a SQL
   * condition) leave; the words search() corrected by itself; and with
   * `didYouMean` a better query when there is one. No counts: run the query. Completions and corrections need the search
   * index over exactly these columns (`indexed` says whether it answered).
   */
  searchSuggest(
    q: string,
    options: { columns: string[]; where?: string; limit?: number; didYouMean?: boolean; table?: string },
  ): Promise<SearchSuggestions>;

  /** Drop a table's search index (search() scans again). */
  dropSearchIndex(options?: { table?: string }): Promise<void>;

  /** Pre-touch columns into the segment cache; queries interleave with warming. */
  warm(cols: string[], options?: { table?: string }): Promise<{ bytes: number }>;

  /** Current segment-cache occupancy for a table. */
  cacheStats(options?: { table?: string }): Promise<{ segments: number; bytes: number }>;

  /**
   * A loaded table's catalog (what `facetful inspect` prints): total rows,
   * row-group count and target, the sort keys the image records, and per
   * column its kind as the converter names it ("int32", "float64", "utf8",
   * "utf8/dict (u16 codes)", "date", …), on-disk bytes across all row groups,
   * null count, min/max folded over the row groups (numeric columns), and
   * the dictionary's entry count (dictionary columns).
   */
  describe(options?: { table?: string }): Promise<TableInfo>;

  /**
   * The worker's wasm memory size in bytes and the number of tables it holds.
   * Wasm linear memory never shrinks, so `wasmBytes` is the worker's
   * high-water mark: image copies, segment and mask caches, and the largest
   * result built so far. Attaching DevTools to a dedicated worker is flaky;
   * this answers the same question from the page.
   */
  memoryStats(): Promise<{ wasmBytes: number; tables: number }>;

  /** Filter-mask cache byte budget for a table (default 16 MB); 0 disables it. */
  setMaskBudget(bytes: number, options?: { table?: string }): Promise<void>;

  /**
   * Run SQL (SELECT-only; the table is always `t`). `table` picks a loaded
   * table by name, defaulting to the most recently loaded. Rejects with an
   * Error whose message is a rendered diagnostic (caret + hint) on SQL errors.
   *
   * `dictText`: text columns come back as `codes` + `dict` on `columnRaw`
   * instead of one string per row. `true` does it for columns that are
   * dictionary-encoded in the image and selected as-is — free, since the
   * per-row form is never built: a 1.5M-row result over a 372-value column is
   * 3 MB instead of 52 MB. `"all"` also encodes any other text column whose
   * rows are less than half distinct, after a hash pass over its values that
   * costs about 100 ms per million rows per column (a 664K-value title column
   * over 1.5M rows: 62 MB → 38 MB). `rows()`, `column()` and `dictionary()`
   * decode either; existing `columnRaw` readers only see a change when they
   * pass the option.
   */
  query(sql: string, options?: { table?: string; dictText?: boolean | "all" }): Promise<Result>;
  /**
   * Several statements in one call: one worker message, one engine call, and
   * the statements a facet UI sends run fused — single table, at most one
   * GROUP BY key that is a dictionary column or a narrow-range integer/date
   * column, items among the key, `count(*)`, `count(col)`, `count(distinct
   * dictcol)`, `sum`/`min`/`max` of a numeric column, any WHERE, ORDER BY
   * over the select items, LIMIT/OFFSET. Their WHERE masks and column lanes
   * are shared and each is a few lean passes over them. Anything else in the
   * array runs as it would alone, in order. Results come back in order,
   * identical to running each statement by itself; a failing statement
   * rejects the whole call with a diagnostic naming its index. On a 1.5M-row
   * table eight facets plus totals went from 70 ms one by one to 17 ms.
   *
   * Two things to know. Every result of a batch carries the batch's whole
   * `elapsedMs` (the statements run interleaved; there is no per-statement
   * time), so do not sum them. And a batch answers all at once, so anything
   * latency-critical — a grid's first page — should be sent alone, just
   * before the batch, rather than wait for the batch's slowest member.
   */
  query(sqls: string[], options?: { table?: string; dictText?: boolean | "all" }): Promise<Result[]>;

  /**
   * Run SQL and keep its result in the worker, for a grid over a result of
   * millions of rows: `page(start, end)` returns any block of it and
   * `sort(orderBy)` re-orders it, so neither the first query nor a sort
   * copies every column to this thread. In the lite build too.
   *
   * A plain SELECT over the table (any WHERE, ORDER BY, LIMIT/OFFSET,
   * expressions, `search()`, `search_rank()`) keeps only its row ids, 4
   * bytes a row, and none when it has no WHERE and no ORDER BY; each page is
   * gathered from the table and its expressions run on that page's rows
   * (`kind: "rows"`). A grouped or joined result is not rows of the table:
   * it is computed once and kept whole in the worker (`kind: "result"`).
   *
   * The rows are fixed when this resolves, and a handle whose table was
   * loaded again under the same name rejects.
   *
   * Close a handle when its grid goes. The worker holds a pool of 5
   * (`maxHandles` at `open`): with that many open, `prepare` rejects and
   * names the open statements, so a forgotten `close()` shows up at once.
   * A handle that is garbage collected unclosed is closed with a
   * `console.warn`, but collection can come late, so don't lean on it.
   * `dictText` is the default for the handle's pages.
   */
  prepare(sql: string, options?: { table?: string; dictText?: boolean | "all" }): Promise<ResultHandle>;

  /** Terminate the worker. */
  close(): void;
}

/** A block of a retained result: a Result plus where it sits. */
export interface Page extends Result {
  /** the `start` asked for */
  start: number;
  /** the handle's `generation` when the page was asked for */
  generation: number;
}

/** A result kept in the worker; see `Facetful.prepare`. */
export declare class ResultHandle {
  /** Rows in the result. */
  rows: number;
  columns: { name: string; kind: ColumnKind }[];
  /** "rows": rows of the table, gathered a page at a time. "result": a
   *  grouped or joined result, kept whole in the worker. */
  kind: "rows" | "result";
  /** Time `prepare` took inside the worker, ms. */
  elapsedMs: number;
  /** Counts the `sort()` calls. A page carries the value it was asked
   *  under: one older than this shows the order before the last sort (it
   *  matches the `generation` a facetful-grid `need()` passes on). */
  generation: number;
  closed: boolean;
  /**
   * Rows `start..end` (end exclusive, clamped to the result) in the form
   * `query` returns: the same rows the statement gives with
   * `LIMIT end - start OFFSET start`. `columns` picks result columns by
   * name, in that order. With `dictText` a page's dictionary holds the
   * values that page uses, so codes are not comparable between pages.
   */
  page(start: number, end: number, options?: { columns?: string[]; dictText?: boolean | "all" }): Promise<Page>;
  /**
   * Re-order by `orderBy`, the text after ORDER BY (`'"Amount" desc, "Title"'`;
   * `""` for no ORDER BY): the statement's result with that ORDER BY in place
   * of its own, same LIMIT/OFFSET. Rows of the table are re-sorted by reading
   * only the key columns, without running the WHERE again, and rows with
   * equal keys stay in table order; a kept result runs again. Rejects, and
   * keeps the old order, when `orderBy` does not bind.
   */
  sort(orderBy: string): Promise<{ rows: number; generation: number; elapsedMs: number }>;
  /** Free the result in the worker. `page` and `sort` reject afterwards. */
  close(): Promise<void>;
}

/** What `searchSuggest` offers for a query typed so far. */
export interface SearchSuggestions {
  /** whether a search index over these columns answered */
  indexed: boolean;
  /** rows the word being typed has as a prefix over the whole table, roughly: a measure of "too broad" */
  prefixRows: number;
  /** completions of the word being typed, most common words first */
  completions: { word: string; query: string }[];
  /** words search() corrected by itself ("including results for …") */
  corrected: { word: string; to: string[] }[];
  /** a better query, when asked for and one with results exists */
  didYouMean: string | null;
}
