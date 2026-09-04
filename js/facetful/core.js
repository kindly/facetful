// Environment-agnostic marshalling over the facetful wasm exports.
// Used by worker.js in the browser and driven directly by the Node smoke test.

const KINDS = { 1: "int", 2: "float", 3: "bool", 4: "text" };

export async function instantiate(wasmBytes) {
  const { instance } = await WebAssembly.instantiate(wasmBytes, {});
  return new Engine(instance.exports);
}

export class Engine {
  constructor(exports) {
    this.w = exports;
    this.scratch = this.w.alloc(4096);
    this.enc = new TextEncoder();
    this.dec = new TextDecoder();
  }
  mem() {
    return this.w.memory.buffer;
  }

  /** Open a .facetful image from bytes; returns a table handle. */
  openTable(bytes) {
    const src = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    const ptr = this.w.alloc(src.byteLength);
    new Uint8Array(this.mem(), ptr, src.byteLength).set(src);
    const handle = this.w.table_open(ptr, src.byteLength);
    if (!handle) throw new Error("not a valid .facetful image");
    return { handle, rows: this.w.table_total_rows(handle) };
  }

  /** Run SQL; returns { columns, rowCount, stats } with copied-out buffers. */
  query(tableHandle, sql) {
    const sqlBytes = this.enc.encode(sql);
    const sqlPtr = this.w.alloc(sqlBytes.byteLength);
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
          this.mem(), this.w.col_validity_ptr(h, i), Math.ceil(rowCount / 8),
        ).slice();
        const col = { name, kind, validity };
        if (kind === "int" || kind === "float") {
          col.values = new Float64Array(this.mem(), this.w.col_f64_ptr(h, i), rowCount).slice();
        } else if (kind === "bool") {
          col.values = new Uint8Array(this.mem(), this.w.col_bools_ptr(h, i), rowCount).slice();
        } else {
          col.offsets = new Uint32Array(this.mem(), this.w.col_offsets_ptr(h, i), rowCount + 1).slice();
          col.bytes = new Uint8Array(
            this.mem(), this.w.col_bytes_ptr(h, i), this.w.col_bytes_len(h, i),
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
