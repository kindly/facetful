// Environment-agnostic marshalling over the facetful wasm exports.
// Used by worker.js in the browser and driven directly by the Node smoke test.

const KINDS = { 1: "int", 2: "float", 3: "bool", 4: "text", 5: "date", 6: "timestamp" };

// wasm32 pointers and lengths are u32, but exports returning them arrive in JS
// as signed i32. Once linear memory grows past 2 GB (large parquet compiles do
// this) every pointer above the halfway mark is negative and the typed-array
// constructor throws "Start offset ... is outside the bounds of the buffer".
// Re-interpret as unsigned before building any view over memory.
const u32 = (n) => n >>> 0;

// The wasm module declares one import: env.opfs_read(fileId, offset, len, destPtr)
// -> bytes read. The browser worker supplies a real implementation over OPFS
// sync access handles; environments without OPFS (Node smoke test) get a stub
// that fails any read (memory-backed tables never call it).
export async function instantiate(wasmBytes, opfsRead) {
  let engine = null;
  const env = {
    opfs_read: (fileId, offset, len, destPtr) => {
      if (!opfsRead || !engine) return -1;
      // the view must be built per call: memory.buffer detaches on growth
      return opfsRead(fileId, offset, new Uint8Array(engine.mem(), u32(destPtr), u32(len)));
    },
  };
  const { instance } = await WebAssembly.instantiate(wasmBytes, { env });
  engine = new Engine(instance.exports);
  return engine;
}

export class Engine {
  constructor(exports) {
    this.w = exports;
    this.scratch = u32(this.w.alloc(4096));
    this.enc = new TextEncoder();
    this.dec = new TextDecoder();
  }
  mem() {
    return this.w.memory.buffer;
  }

  /** Open a .facetful image from bytes; returns a table handle. */
  openTable(bytes) {
    const src = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    const ptr = u32(this.w.alloc(src.byteLength));
    new Uint8Array(this.mem(), ptr, src.byteLength).set(src);
    const handle = this.w.table_open(ptr, src.byteLength);
    if (!handle) throw new Error("not a valid .facetful image");
    return { handle, rows: this.w.table_total_rows(handle) };
  }

  /** Open a table backed by a registered OPFS file (reads go through opfs_read). */
  openOpfsTable(fileId, fileLen, cacheBytes) {
    const handle = this.w.table_open_opfs(fileId, fileLen, cacheBytes ?? 0);
    if (!handle) throw new Error("not a valid .facetful image (OPFS)");
    return { handle, rows: this.w.table_total_rows(handle) };
  }

  colByName(tableHandle, name) {
    const b = this.enc.encode(name);
    new Uint8Array(this.mem(), this.scratch, b.byteLength).set(b);
    return this.w.table_col_by_name(tableHandle, this.scratch, b.byteLength);
  }

  /** Pre-touch one column's segments into the cache; returns bytes read. */
  warmColumn(tableHandle, colIdx) {
    return this.w.table_warm(tableHandle, colIdx);
  }

  /** { segments, bytes } currently cached. */
  cacheStats(tableHandle) {
    const packed = this.w.table_cache_stats(tableHandle);
    return { segments: Number(packed >> 32n), bytes: Number(packed & 0xffffffffn) * 1024 };
  }

  /**
   * Compile typed columns into a .facetful image (the baseline compiler —
   * same Rust code path as the native CLI). Columns:
   *   { name, kind: "num", data: Float64Array, isInt?, temporal?: "date"|"timestamp",
   *     validity?: Uint8Array }   // temporal: data = days / ms since epoch
   *   { name, kind: "text", offsets: Uint32Array, bytes: Uint8Array, validity? }
   * validity is one byte per row, 0 = null. Returns an image handle; pass it
   * to imageBytes() (copy out, e.g. for OPFS) and/or openImage() (consumes).
   */
  compileTable(rows, columns, { groupTarget = 65536 } = {}) {
    const put = (src) => {
      const p = u32(this.w.alloc(src.byteLength));
      new Uint8Array(this.mem(), p, src.byteLength).set(
        new Uint8Array(src.buffer, src.byteOffset, src.byteLength),
      );
      return p;
    };
    const b = this.w.compile_begin(rows);
    for (const c of columns) {
      const nameB = this.enc.encode(c.name);
      const nameP = put(nameB);
      const validP = c.validity ? put(c.validity) : 0;
      if (c.kind === "num") {
        const dataP = put(c.data);
        const numKind =
          c.temporal === "date" ? 2 : c.temporal === "timestamp" ? 3 : c.isInt ? 1 : 0;
        this.w.compile_add_num(b, nameP, nameB.byteLength, dataP, validP, numKind);
        this.w.dealloc(dataP, c.data.byteLength);
      } else {
        const offP = put(c.offsets);
        const bytesP = put(c.bytes);
        this.w.compile_add_text(b, nameP, nameB.byteLength, offP, bytesP, c.bytes.byteLength, validP);
        this.w.dealloc(offP, c.offsets.byteLength);
        this.w.dealloc(bytesP, c.bytes.byteLength);
      }
      this.w.dealloc(nameP, nameB.byteLength);
      if (validP) this.w.dealloc(validP, c.validity.byteLength);
    }
    const img = this.w.compile_finish(b, groupTarget);
    if (!img) throw new Error("compile failed (mismatched column lengths?)");
    return img;
  }

  /** Copy a compiled image's bytes out (for OPFS persistence). */
  imageBytes(img) {
    return new Uint8Array(this.mem(), u32(this.w.image_ptr(img)), u32(this.w.image_len(img))).slice();
  }

  /** Open a table over a compiled image; consumes the image handle (no copy). */
  openImage(img) {
    const handle = this.w.image_open_table(img);
    if (!handle) throw new Error("compiled image failed to open");
    return { handle, rows: this.w.table_total_rows(handle) };
  }

  /** Run SQL; returns { columns, rowCount, stats } with copied-out buffers. */
  query(tableHandle, sql) {
    const sqlBytes = this.enc.encode(sql);
    const sqlPtr = u32(this.w.alloc(sqlBytes.byteLength));
    new Uint8Array(this.mem(), sqlPtr, sqlBytes.byteLength).set(sqlBytes);
    const h = this.w.query_run(tableHandle, sqlPtr, sqlBytes.byteLength);
    try {
      if (this.w.outcome_is_err(h)) {
        const n = this.w.outcome_error(h, this.scratch, 4096);
        throw new QueryError(this.dec.decode(new Uint8Array(this.mem(), this.scratch, n)));
      }
      const rowCount = this.w.outcome_rows(h);
      const nCols = this.w.outcome_cols(h);
      const stats = this.w.outcome_scan_stats(h);
      const columns = [];
      for (let i = 0; i < nCols; i++) {
        const kind = KINDS[this.w.col_kind(h, i)];
        const nameLen = this.w.col_name(h, i, this.scratch, 4096);
        const name = this.dec.decode(new Uint8Array(this.mem(), this.scratch, nameLen));
        const validity = new Uint8Array(
          this.mem(), u32(this.w.col_validity_ptr(h, i)), Math.ceil(rowCount / 8),
        ).slice();
        const col = { name, kind, validity };
        if (kind === "int" || kind === "float" || kind === "date" || kind === "timestamp") {
          col.values = new Float64Array(this.mem(), u32(this.w.col_f64_ptr(h, i)), rowCount).slice();
        } else if (kind === "bool") {
          col.values = new Uint8Array(this.mem(), u32(this.w.col_bools_ptr(h, i)), rowCount).slice();
        } else {
          col.offsets = new Uint32Array(this.mem(), u32(this.w.col_offsets_ptr(h, i)), rowCount + 1).slice();
          col.bytes = new Uint8Array(
            this.mem(), u32(this.w.col_bytes_ptr(h, i)), u32(this.w.col_bytes_len(h, i)),
          ).slice();
        }
        columns.push(col);
      }
      return {
        columns,
        rowCount,
        stats: { scannedGroups: Number(stats & 0xffffffffn), totalGroups: Number(stats >> 32n) },
      };
    } finally {
      this.w.outcome_free(h);
    }
  }
}

export class QueryError extends Error {}

/** Transferable list for a query payload (zero-copy postMessage). */
export function transferables(result) {
  const t = [];
  for (const c of result.columns) {
    t.push(c.validity.buffer);
    if (c.values) t.push(c.values.buffer);
    if (c.offsets) {
      t.push(c.offsets.buffer, c.bytes.buffer);
    }
  }
  return t;
}
