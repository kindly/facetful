// Environment-agnostic marshalling over the facetful wasm exports.
// Used by worker.js in the browser and driven directly by the Node smoke test.

const KINDS = { 1: "int", 2: "float", 3: "bool", 4: "text", 5: "date", 6: "timestamp" };

// wasm32 pointers and lengths are u32, but exports returning them arrive in JS
// as signed i32. Once linear memory grows past 2 GB (large parquet compiles do
// this) every pointer above the halfway mark is negative and the typed-array
// constructor throws "Start offset ... is outside the bounds of the buffer".
// Re-interpret as unsigned before building any view over memory.
const u32 = (n) => n >>> 0;

// The wasm module declares two imports. env.opfs_read(fileId, offset, len,
// destPtr) -> bytes read: the browser worker supplies a real implementation
// over OPFS sync access handles; environments without OPFS (Node smoke test)
// get a stub that fails any read (memory-backed tables never call it).
// env.udf_call(id, argc, argsPtr, outPtr, len) evaluates a registered
// user-defined function over whole lanes (see Engine.registerFunction).
export async function instantiate(wasmBytes, opfsRead) {
  let engine = null;
  const env = {
    opfs_read: (fileId, offset, len, destPtr) => {
      if (!opfsRead || !engine) return -1;
      // the view must be built per call: memory.buffer detaches on growth
      return opfsRead(fileId, offset, new Uint8Array(engine.mem(), u32(destPtr), u32(len)));
    },
    udf_call: (id, argc, argsPtr, outPtr, len) => engine._udfCall(id, argc, u32(argsPtr), u32(outPtr), len),
  };
  const { instance } = await WebAssembly.instantiate(wasmBytes, { env });
  engine = new Engine(instance.exports);
  return engine;
}

/** Lane kinds on the UDF wire (the numbers are the ABI). */
export const KIND = { int: 0, float: 1, bool: 2, text: 3, date: 4, timestamp: 5 };
/** parameter-only kind: accepts any argument type (the lane carries its real kind) */
const ANY = 6;
const KIND_NAMES = Object.keys(KIND);
// descriptor words: see facetful-wasm `WasmHost`
const D_KIND = 0, D_LEN = 1, D_FLAGS = 2, D_DATA = 3, D_AUX = 4, D_VALID = 5, D_BYTES = 6, D_ERR = 7, D_WORDS = 8;

export class Engine {
  constructor(exports) {
    this.w = exports;
    this.scratch = u32(this.w.alloc(4096));
    this.enc = new TextEncoder();
    this.dec = new TextDecoder();
    /** id -> { fn, sig, name } for registered user-defined functions */
    this.udfs = new Map();
  }

  /**
   * Register a user-defined scalar function (design.sv d49). `sig` is
   * `{ params: kind[], returns: kind, strict?, variadic?, optional?, perRow? }`
   * with kinds from KIND (by name; a parameter may also be "any"). `optional`
   * is how many trailing parameters may be omitted; `variadic` lets the last
   * repeat. The function is called ONCE per lane —
   * `fn(args, len, out)` where each arg is `{ kind, values, valid, broadcast }`
   * (`values` a Float64Array for numbers/dates/bools, `string[]` for text;
   * `broadcast` = a literal, one value; `valid` a bitmap or null) and `out` is
   * `{ values, valid }` to fill (`values` a typed array or `string[]`; set a
   * result to null to make it NULL). With `perRow: true` the function is
   * instead called per row with plain values and returns one (or null).
   * Strict (the default) skips NULL inputs and NULL-fills those outputs.
   */
  registerFunction(name, sig, fn) {
    const kinds = (sig.params || []).map((k) => (k === "any" ? ANY : kindOf(k)));
    const ret = kindOf(sig.returns);
    const nameB = this.enc.encode(name);
    const p = u32(this.w.alloc(nameB.byteLength + kinds.length + 1));
    new Uint8Array(this.mem(), p, nameB.byteLength).set(nameB);
    new Uint8Array(this.mem(), p + nameB.byteLength, kinds.length).set(kinds);
    const flags = (sig.strict === false ? 0 : 1) | (sig.variadic ? 2 : 0) | ((sig.optional || 0) << 2);
    const id = this.w.udf_register(p, nameB.byteLength, p + nameB.byteLength, kinds.length, ret, flags);
    this.w.dealloc(p, nameB.byteLength + kinds.length + 1);
    if (!id) {
      const n = this.w.udf_error(this.scratch, 4096);
      throw new Error(`registerFunction('${name}'): ${this.dec.decode(new Uint8Array(this.mem(), this.scratch, n))}`);
    }
    for (const [k, v] of this.udfs) if (v.name === name) this.udfs.delete(k);
    this.udfs.set(id, { fn, sig: { ...sig, strict: sig.strict !== false }, name });
    return id;
  }

  unregisterFunction(name) {
    const b = this.enc.encode(name);
    const p = u32(this.w.alloc(b.byteLength));
    new Uint8Array(this.mem(), p, b.byteLength).set(b);
    const ok = this.w.udf_unregister(p, b.byteLength) !== 0;
    this.w.dealloc(p, b.byteLength);
    for (const [k, v] of this.udfs) if (v.name === name) this.udfs.delete(k);
    return ok;
  }

  /** The udf_call import: lanes in wasm memory -> the registered function -> the output lane. */
  _udfCall(id, argc, argsPtr, outPtr, len) {
    const entry = this.udfs.get(id);
    try {
      if (!entry) throw new Error(`no function registered under id ${id}`);
      const mem = this.mem();
      const args = [];
      for (let i = 0; i < argc; i++) {
        const d = new Uint32Array(mem, argsPtr + i * D_WORDS * 4, D_WORDS);
        const n = d[D_LEN];
        const kind = KIND_NAMES[d[D_KIND]];
        const valid = d[D_VALID] ? new Uint8Array(mem, d[D_VALID], (n + 7) >> 3) : null;
        let values;
        if (kind === "text") {
          values = this._decodeLane(new Uint8Array(mem, d[D_DATA], d[D_BYTES]), new Uint32Array(mem, d[D_AUX], n + 1), n);
        } else {
          values = new Float64Array(mem, d[D_DATA], n);
        }
        args.push({ kind, values, valid, broadcast: (d[D_FLAGS] & 1) !== 0 });
      }
      const od = new Uint32Array(mem, outPtr, D_WORDS);
      const retKind = KIND_NAMES[od[D_KIND]];
      const outValid = new Uint8Array(mem, od[D_VALID], (len + 7) >> 3);
      const out = {
        values: retKind === "text" ? new Array(len).fill("") : retKind === "bool" ? new Uint8Array(mem, od[D_DATA], len) : new Float64Array(mem, od[D_DATA], len),
        valid: outValid,
      };
      const { fn, sig } = entry;
      if (sig.perRow) {
        const at = (a, i) => {
          const j = a.broadcast ? 0 : i;
          if (a.valid && !((a.valid[j >> 3] >> (j & 7)) & 1)) return null;
          return a.values[j];
        };
        for (let i = 0; i < len; i++) {
          const row = args.map((a) => at(a, i));
          if (sig.strict && row.includes(null)) { outValid[i >> 3] &= ~(1 << (i & 7)); continue; }
          const v = fn(...row);
          if (v === null || v === undefined) outValid[i >> 3] &= ~(1 << (i & 7));
          else out.values[i] = v;
        }
      } else {
        fn(args, len, out);
        // null results in a plain array mark NULL
        if (retKind === "text") for (let i = 0; i < len; i++) if (out.values[i] == null) { outValid[i >> 3] &= ~(1 << (i & 7)); out.values[i] = ""; }
      }
      if (retKind === "text") {
        const parts = out.values.map((s) => this.enc.encode(typeof s === "string" ? s : String(s)));
        let total = 0;
        for (const q of parts) total += q.byteLength;
        const bp = total ? u32(this.w.alloc(total)) : 0;
        const bytes = new Uint8Array(this.mem(), bp, total);
        const offs = new Uint32Array(this.mem(), od[D_AUX], len + 1);
        let pos = 0;
        offs[0] = 0;
        for (let i = 0; i < len; i++) { bytes.set(parts[i], pos); pos += parts[i].byteLength; offs[i + 1] = pos; }
        const od2 = new Uint32Array(this.mem(), outPtr, D_WORDS);
        od2[D_DATA] = bp;
        od2[D_BYTES] = total;
      }
      return 0;
    } catch (e) {
      const msg = this.enc.encode(`${entry ? entry.name : "udf"}(): ${e && e.message ? e.message : e}`);
      const p = u32(this.w.alloc(msg.byteLength));
      new Uint8Array(this.mem(), p, msg.byteLength).set(msg);
      new Uint32Array(this.mem(), outPtr, D_WORDS)[D_ERR] = p;
      return -msg.byteLength;
    }
  }

  /** A text lane to string[]: one decode when the bytes are ASCII (decoded
   *  length == byte length, so byte offsets are char offsets), else per string. */
  _decodeLane(bytes, offsets, n) {
    const whole = this.dec.decode(bytes);
    const out = new Array(n);
    if (whole.length === bytes.byteLength) {
      for (let i = 0; i < n; i++) out[i] = whole.slice(offsets[i], offsets[i + 1]);
    } else {
      for (let i = 0; i < n; i++) out[i] = this.dec.decode(bytes.subarray(offsets[i], offsets[i + 1]));
    }
    return out;
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

  /** Filter-mask cache byte budget for a table; 0 disables it. */
  setMaskBudget(tableHandle, bytes) {
    this.w.table_set_mask_budget(tableHandle, bytes);
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

  /**
   * Materialize a query's result as a compiled image: the derived-table
   * primitive. Returns an image handle (pass to openImage / imageBytes), or
   * throws QueryError with the engine's diagnostic.
   */
  materialize(tableHandle, sql, { groupTarget = 65536 } = {}) {
    const sqlBytes = this.enc.encode(sql);
    const sqlPtr = u32(this.w.alloc(sqlBytes.byteLength));
    new Uint8Array(this.mem(), sqlPtr, sqlBytes.byteLength).set(sqlBytes);
    const h = this.w.table_materialize(tableHandle, sqlPtr, sqlBytes.byteLength, groupTarget);
    try {
      if (this.w.outcome_is_err(h)) {
        const n = this.w.outcome_error(h, this.scratch, 4096);
        throw new QueryError(this.dec.decode(new Uint8Array(this.mem(), this.scratch, n)));
      }
      const img = this.w.outcome_image(h);
      if (!img) throw new Error("materialize produced no image");
      return img;
    } finally {
      this.w.dealloc(sqlPtr, sqlBytes.byteLength);
      this.w.outcome_free(h);
    }
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

  /**
   * Stream a CSV into a .facetful image (design.sv d51): `chunks()` returns a
   * fresh (async) iterable of Uint8Array chunks each time it is called — the
   * converter reads the input twice (types and dictionaries, then encoding)
   * in bounded memory; the image is assembled here. Returns
   * { bytes, rows, schema: [{ name, kind }] }.
   */
  async convertCsv(chunks, { groupTarget = 65536 } = {}) {
    const h = this.w.convert_begin(groupTarget);
    const fail = () => {
      const n = this.w.convert_error(h, this.scratch, 4096);
      const msg = this.dec.decode(new Uint8Array(this.mem(), this.scratch, n));
      this.w.convert_free(h);
      throw new Error(`convert: ${msg}`);
    };
    const parts = [];
    let total = 0;
    const drain = () => {
      const n = this.w.convert_output_len(h);
      if (!n) return;
      const p = u32(this.w.alloc(n));
      this.w.convert_output_copy(h, p);
      parts.push(new Uint8Array(this.mem(), p, n).slice());
      this.w.dealloc(p, n);
      total += n;
    };
    const feed = async () => {
      for await (const c of chunks()) {
        const bytes = c instanceof Uint8Array ? c : new Uint8Array(c);
        const p = u32(this.w.alloc(bytes.byteLength));
        new Uint8Array(this.mem(), p, bytes.byteLength).set(bytes);
        const rc = this.w.convert_feed(h, p, bytes.byteLength);
        this.w.dealloc(p, bytes.byteLength);
        if (rc < 0) fail();
        drain();
      }
    };
    await feed();
    if (this.w.convert_pass2(h) < 0) fail();
    const n = this.w.convert_schema(h, this.scratch, 4096);
    const schema = this.dec.decode(new Uint8Array(this.mem(), this.scratch, n)).trimEnd().split("\n")
      .filter(Boolean).map((l) => { const [name, kind] = l.split("\t"); return { name, kind }; });
    await feed();
    if (this.w.convert_finish(h) < 0) fail();
    drain();
    const rows = this.w.convert_rows(h);
    this.w.convert_free(h);
    const bytes = new Uint8Array(total);
    let pos = 0;
    for (const q of parts) { bytes.set(q, pos); pos += q.byteLength; }
    return { bytes, rows, schema };
  }

  /** Make `handle` reachable by `name` from other tables' queries (FROM / JOIN). */
  catalogRegister(name, handle) {
    const b = this.enc.encode(name);
    const p = u32(this.w.alloc(b.byteLength));
    new Uint8Array(this.mem(), p, b.byteLength).set(b);
    this.w.catalog_register(p, b.byteLength, handle);
    this.w.dealloc(p, b.byteLength);
  }

  /** Run SQL; returns { columns, rowCount, stats } with copied-out buffers.
   *  `dictText`: a text column backed by a dictionary arrives as `codes`
   *  (Uint16Array, one per row) + `dict` ({ offsets, bytes }: the distinct
   *  values present, in dictionary order) instead of a string per row — the
   *  wasm side never builds the per-row form. Plain text columns are
   *  unaffected. */
  query(tableHandle, sql, { dictText = false } = {}) {
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
          const dn = dictText ? this.w.col_dict_len(h, i) : 0;
          if (dn > 0) {
            col.codes = new Uint16Array(this.mem(), u32(this.w.col_codes_ptr(h, i)), rowCount).slice();
            col.dict = {
              offsets: new Uint32Array(this.mem(), u32(this.w.col_dict_offsets_ptr(h, i)), dn + 1).slice(),
              bytes: new Uint8Array(
                this.mem(), u32(this.w.col_dict_bytes_ptr(h, i)), u32(this.w.col_dict_bytes_len(h, i)),
              ).slice(),
            };
          } else {
            // asking for offsets expands a dictionary column inside the wasm
            col.offsets = new Uint32Array(this.mem(), u32(this.w.col_offsets_ptr(h, i)), rowCount + 1).slice();
            col.bytes = new Uint8Array(
              this.mem(), u32(this.w.col_bytes_ptr(h, i)), u32(this.w.col_bytes_len(h, i)),
            ).slice();
          }
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
    if (c.codes) {
      t.push(c.codes.buffer, c.dict.offsets.buffer, c.dict.bytes.buffer);
    }
  }
  return t;
}

function kindOf(k) {
  if (typeof k === "number") return k;
  if (k in KIND) return KIND[k];
  throw new Error(`unknown lane kind '${k}' (expected one of ${Object.keys(KIND).join(", ")})`);
}
