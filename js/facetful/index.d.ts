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
}

export type CellValue = number | string | boolean | null;

export declare class Result {
  columns: { name: string; kind: ColumnKind }[];
  rowCount: number;
  stats: QueryStats;
  /** Execution time inside the worker, ms (excludes the message hop). */
  elapsedMs: number;
  /** Raw transferred buffers for a column — near-zero copy, ideal for charts. */
  columnRaw(name: string): RawColumn;
  /** Materialized values with nulls; date/timestamp as ISO strings. */
  column(name: string): CellValue[];
  /** Row objects, materialized lazily. */
  rows(): Generator<Record<string, CellValue>>;
}

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
  storeOpfs(path: string, buffer: ArrayBuffer): Promise<{ bytes: number }>;

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

  /** Pre-touch columns into the segment cache; queries interleave with warming. */
  warm(cols: string[], options?: { table?: string }): Promise<{ bytes: number }>;

  /** Current segment-cache occupancy for a table. */
  cacheStats(options?: { table?: string }): Promise<{ segments: number; bytes: number }>;

  /** Filter-mask cache byte budget for a table (default 16 MB); 0 disables it. */
  setMaskBudget(bytes: number, options?: { table?: string }): Promise<void>;

  /**
   * Run SQL (SELECT-only; the table is always `t`). `table` picks a loaded
   * table by name, defaulting to the most recently loaded. Rejects with an
   * Error whose message is a rendered diagnostic (caret + hint) on SQL errors.
   */
  query(sql: string, options?: { table?: string }): Promise<Result>;

  /** Terminate the worker. */
  close(): void;
}
