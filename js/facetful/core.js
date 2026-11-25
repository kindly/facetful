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

  /**
   * Open a .facetful image while it arrives. `chunks` is an async iterable of
   * Uint8Array (a fetch body). With `searchColumns`, the word index is built
   * a row group at a time as groups complete, so it is ready with the table
   * instead of being built after. `expectedBytes` (the image's size, when
   * known) allocates the buffer once; `onChunk(bytes)` sees the image's next
   * bytes after each chunk is taken (to write them to OPFS, say): the chunk
   * itself, or for a packed image the bytes it unpacked to, so what is kept
   * opens on the next visit without unpacking. Returns { handle, rows,
   * indexedWhileLoading, indexLeft }: row groups indexed before the last
   * byte, and those still to index (when the download outran indexing;
   * `stepSearchIndex` finishes them, search() scans until then).
   */
  async openStream(chunks, { expectedBytes = 0, searchColumns, onChunk } = {}) {
    if (searchColumns && !this.w.load_index_columns) throw new Error("this facetful build has no search index (lite build)");
    const h = this.w.load_begin(expectedBytes);
    let indexed = 0;
    // image bytes already given to onChunk
    let kept = 0;
    try {
      if (searchColumns) {
        const names = this.enc.encode(searchColumns.join("\0"));
        const p = u32(this.w.alloc(names.byteLength));
        new Uint8Array(this.mem(), p, names.byteLength).set(names);
        const rc = this.w.load_index_columns(h, p, names.byteLength);
        this.w.dealloc(p, names.byteLength);
        if (rc < 0) throw new Error("search index: a column to index is not in the image");
      }
      // time spent waiting on the source since the last group was indexed
      let waited = 0;
      let t = performance.now();
      for await (const chunk of chunks) {
        waited += performance.now() - t;
        if (!chunk.byteLength) continue;
        const p = u32(this.w.alloc(chunk.byteLength));
        new Uint8Array(this.mem(), p, chunk.byteLength).set(chunk);
        const rc = this.w.load_push(h, p, chunk.byteLength);
        this.w.dealloc(p, chunk.byteLength);
        if (rc < 0) throw new Error("not a valid .facetful image, or a column to index is not a text column in it");
        if (onChunk) {
          // the engine's copy of the image so far, not the chunk: a packed
          // image's chunks are not the image's bytes. Copied out, since
          // wasm memory may move while onChunk is awaited.
          const len = this.w.load_image_len(h);
          if (len > kept) {
            const bytes = new Uint8Array(this.mem(), u32(this.w.load_image_ptr(h)) + kept, len - kept).slice();
            kept = len;
            await onChunk(bytes);
          }
        }
        // index a group when the source has kept us waiting (we are level
        // with the download, so indexing costs nothing); chunks that are
        // already there mean the download is ahead, and the table should not
        // wait for the index: what is left is indexed once it is open
        if (searchColumns && waited >= 1 && this.w.load_groups_ready(h) > indexed) {
          indexed = this.w.load_index_step(h, 1);
          if (indexed < 0) throw new Error("search index: could not read the image");
          waited = 0;
        }
        t = performance.now();
      }
    } catch (e) {
      this.w.load_free(h);
      throw e;
    }
    const handle = this.w.load_finish(h);
    if (!handle) throw new Error("not a complete .facetful image");
    // with every group indexed this finishes the index (renumbering)
    const indexLeft = searchColumns ? this.stepSearchIndex(handle, 0) : 0;
    return { handle, rows: this.w.table_total_rows(handle), indexedWhileLoading: indexed, indexLeft };
  }

  /** The table's search index as bytes, to keep (in OPFS) and give back to
   *  `loadSearchIndex` on a later visit: `use(view)` sees them, a view into
   *  wasm memory valid only during the call (write it out there, without a
   *  copy). Returns `use`'s result, or null when there is no index. */
  withSavedSearchIndex(tableHandle, use) {
    if (!this.w.table_search_index_save) throw new Error("this facetful build has no search index (lite build)");
    const h = this.w.table_search_index_save(tableHandle);
    if (!h) return null;
    try {
      return use(new Uint8Array(this.mem(), u32(this.w.image_ptr(h)), u32(this.w.image_len(h))));
    } finally {
      this.w.image_free(h);
    }
  }

  /** Use a search index over `columns` saved through
   *  `withSavedSearchIndex`: `fill(view)` writes its `len` bytes into wasm
   *  memory (an OPFS read, say). Returns false when it does not fit (saved
   *  by another version, for another table or other columns, or damaged):
   *  build the index instead. */
  loadSearchIndex(tableHandle, columns, len, fill) {
    if (!this.w.table_search_index_load) throw new Error("this facetful build has no search index (lite build)");
    const idx = columns.map((c) => {
      const i = this.colByName(tableHandle, c);
      if (i < 0) throw new Error(`search index: no column ${JSON.stringify(c)}`);
      return i;
    });
    const p = u32(this.w.alloc(len));
    const cp = u32(this.w.alloc(idx.length * 4));
    try {
      new Uint32Array(this.mem(), cp, idx.length).set(idx);
      fill(new Uint8Array(this.mem(), p, len));
      return this.w.table_search_index_load(tableHandle, p, len, cp, idx.length) === 0;
    } finally {
      this.w.dealloc(cp, idx.length * 4);
      this.w.dealloc(p, len);
    }
  }

  /** Append the columns of `otherHandle`, the other part of the table's
   *  split image (`facetful split`), to the table. `otherHandle` is used up
   *  either way. Search indexes on either part become one over both. */
  attach(tableHandle, otherHandle) {
    if (this.w.table_attach(tableHandle, otherHandle) < 0) {
      throw new Error("attach: not the other part of this table's split (split id or row groups differ, or a column name repeats)");
    }
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

  /** Build the search() word index over these text columns and keep it on
   *  the table: search() over exactly these columns then looks words up
   *  instead of scanning. In one go (blocks for the whole build); see
   *  beginSearchIndex/stepSearchIndex for a row group at a time. Returns
   *  { pairs, bytes }. Needs a build with the `search` feature. */
  buildSearchIndex(tableHandle, columns) {
    this.beginSearchIndex(tableHandle, columns);
    if (this.stepSearchIndex(tableHandle, 0xffffffff) !== 0) throw new Error("buildSearchIndex: did not finish");
    return { pairs: this.w.table_search_index_pairs(tableHandle), bytes: this.w.table_search_index_bytes(tableHandle) };
  }

  /** Start building the search index a row group at a time (replacing any
   *  index or build in progress). search() scans until it is finished. */
  beginSearchIndex(tableHandle, columns) {
    if (!this.w.table_search_index_begin) throw new Error("this facetful build has no search index (lite build)");
    const idx = columns.map((c) => {
      const i = this.colByName(tableHandle, c);
      if (i < 0) throw new Error(`search index: no column ${JSON.stringify(c)}`);
      return i;
    });
    const p = u32(this.w.alloc(idx.length * 4));
    new Uint32Array(this.mem(), p, idx.length).set(idx);
    const rc = this.w.table_search_index_begin(tableHandle, p, idx.length);
    this.w.dealloc(p, idx.length * 4);
    if (rc < 0) throw new Error("search index: every column must be a text column (or the table could not be read)");
  }

  /** Index up to `groups` more row groups; returns the row groups left (0:
   *  finished, the index is in use). */
  stepSearchIndex(tableHandle, groups = 1) {
    const left = this.w.table_search_index_step(tableHandle, groups);
    if (left < 0) throw new Error("search index: could not read the table");
    return left;
  }

  /** What to offer for a search query typed so far: completions of the word
   *  being typed (most common first, each found in some row the other words
   *  and `where` leave), the words corrected automatically, and on request a
   *  did-you-mean. `where` is a SQL condition (the app's facets), e.g.
   *  `region = 'London'`. No counts: run the query for those. Returns
   *  { indexed, prefixRows, completions: [{ word, query }], corrected:
   *  [{ word, to }], didYouMean: query | null }. */
  searchSuggest(tableHandle, columns, q, { where = "", limit = 8, didYouMean = false } = {}) {
    if (!this.w.table_search_suggest) throw new Error("this facetful build has no search (lite build)");
    const idx = columns.map((c) => {
      const i = this.colByName(tableHandle, c);
      if (i < 0) throw new Error(`searchSuggest: no column ${JSON.stringify(c)}`);
      return i;
    });
    const qb = this.enc.encode(q);
    const fb = this.enc.encode(where);
    const cp = u32(this.w.alloc(idx.length * 4 + 4));
    const qp = u32(this.w.alloc(qb.byteLength + 1));
    const fp = u32(this.w.alloc(fb.byteLength + 1));
    try {
      new Uint32Array(this.mem(), cp, idx.length).set(idx);
      new Uint8Array(this.mem(), qp, qb.byteLength).set(qb);
      new Uint8Array(this.mem(), fp, fb.byteLength).set(fb);
      const n = this.w.table_search_suggest(tableHandle, cp, idx.length, qp, qb.byteLength, fp, fb.byteLength, limit, didYouMean ? 1 : 0);
      const out = u32(this.w.alloc(n + 1));
      try {
        const m = this.w.search_suggest_read(out, n);
        const r = JSON.parse(this.dec.decode(new Uint8Array(this.mem(), out, m)));
        if (r.error) throw new QueryError(r.error);
        return r;
      } finally {
        this.w.dealloc(out, n + 1);
      }
    } finally {
      this.w.dealloc(cp, idx.length * 4 + 4);
      this.w.dealloc(qp, qb.byteLength + 1);
      this.w.dealloc(fp, fb.byteLength + 1);
    }
  }

  /** { pairs, bytes } of a table's finished search index (zeros without one). */
  searchIndexStats(tableHandle) {
    return { pairs: this.w.table_search_index_pairs(tableHandle), bytes: this.w.table_search_index_bytes(tableHandle) };
  }

  /** Drop a table's search index; search() scans again. */
  dropSearchIndex(tableHandle) {
    this.w.table_drop_search_index?.(tableHandle);
  }

  /** Filter-mask cache byte budget for a table; 0 disables it. */
  setMaskBudget(tableHandle, bytes) {
    this.w.table_set_mask_budget(tableHandle, bytes);
  }

  /** { segments, bytes } currently cached. */
  /** The table's catalog: { version, rows, groups, target, sortedBy, columns }
   *  with per-column { name, kind, bytes, nulls, min?, max?, dict? }. */
  describe(tableHandle) {
    let n = this.w.table_describe(tableHandle, this.scratch, 4096);
    if (n <= 4096) return JSON.parse(this.dec.decode(new Uint8Array(this.mem(), this.scratch, n)));
    const p = u32(this.w.alloc(n));
    try {
      n = this.w.table_describe(tableHandle, p, n);
      return JSON.parse(this.dec.decode(new Uint8Array(this.mem(), p, n)));
    } finally {
      this.w.dealloc(p, n);
    }
  }

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
    this._needsWrite("compiling a table (Parquet)");
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
    this._needsWrite("materialize");
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

  /**
   * An image handle over a copy of `bytes`, a .facetful file in either
   * form: a packed image is unpacked. Free it with freeImage(), or hand it
   * to openImage().
   */
  imageFrom(bytes) {
    this._needsPublish("reading an image");
    const img = this._image(this.w.image_new(bytes.byteLength));
    new Uint8Array(this.mem(), u32(this.w.image_ptr(img)), bytes.byteLength).set(bytes);
    if (!this.w.image_is_packed(img)) return img;
    try {
      return this._image(this.w.image_unpack(img));
    } finally {
      this.w.image_free(img);
    }
  }

  /** The packed form of an image (handle): smaller to download, and
   *  unpacked by the loader as it arrives. The handle stays the caller's. */
  packImage(img) {
    this._needsPublish("packing an image");
    const packed = this._image(this.w.image_pack(img));
    try {
      return this.imageBytes(packed);
    } finally {
      this.w.image_free(packed);
    }
  }

  /** Each column's share of the packed image, in column order (for size
   *  reports: compress each to see what the column costs to download). */
  packShares(img) {
    this._needsPublish("packing an image");
    return this._parts(this._image(this.w.image_pack_shares(img)));
  }

  /**
   * The image (handle) as the two parts of a split image: `columns` go to
   * the second part, the rest stay in the first. A page loads the first
   * and `attach`es the second, and queries them as one table. Returns
   * [first, second] as bytes.
   */
  splitImage(img, columns) {
    this._needsPublish("splitting an image");
    const names = this.enc.encode(columns.join("\0"));
    const p = u32(this.w.alloc(names.byteLength));
    new Uint8Array(this.mem(), p, names.byteLength).set(names);
    // the pair's id: random, so that parts of different splits never attach
    const id = crypto.getRandomValues(new BigUint64Array(1))[0];
    try {
      return this._parts(this._image(this.w.image_split(img, p, names.byteLength, id)));
    } finally {
      this.w.dealloc(p, names.byteLength);
    }
  }

  /** Byte strings out of one handle (a count, their lengths, then each),
   *  which is freed. */
  _parts(h) {
    try {
      const at = u32(this.w.image_ptr(h));
      const view = new DataView(this.mem(), at, u32(this.w.image_len(h)));
      const n = view.getUint32(0, true);
      let p = at + 4 + 8 * n;
      return Array.from({ length: n }, (_, i) => {
        const len = Number(view.getBigUint64(4 + 8 * i, true));
        const share = new Uint8Array(this.mem(), p, len).slice();
        p += len;
        return share;
      });
    } finally {
      this.w.image_free(h);
    }
  }

  freeImage(img) {
    this.w.image_free(img);
  }

  /** An image handle, or the engine's reason there is none. */
  _image(h) {
    if (h) return h;
    const n = this.w.image_error(this.scratch, 4096);
    throw new Error(this.dec.decode(new Uint8Array(this.mem(), this.scratch, n)));
  }

  /** Open a table over a compiled image; consumes the image handle (no copy). */
  openImage(img) {
    this._needsWrite("opening a compiled image");
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
    this._needsWrite("CSV conversion");
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
    const lenAt = this.w.convert_len_offset(h);
    this.w.convert_free(h);
    const bytes = new Uint8Array(total);
    let pos = 0;
    for (const q of parts) { bytes.set(q, pos); pos += q.byteLength; }
    // the header went out first: its image-length field is filled in now
    new DataView(bytes.buffer).setBigUint64(lenAt, BigInt(total), true);
    return { bytes, rows, schema };
  }

  /** The lite build writes no images: no compiling (Parquet), CSV
   *  conversion, materialize, joins, CTEs or subqueries. */
  _needsWrite(what) {
    if (!this.w.compile_begin) throw new Error(`${what} needs the full facetful build (this one is lite)`);
  }

  _needsPublish(what) {
    if (!this.w.image_pack) throw new Error(`${what} needs the facetful command's build (facetful_cli.wasm): the builds for pages leave the publishing commands out`);
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
   *  `dictText`: a text column arrives as `codes` (Uint16Array, or Uint32Array
   *  past 65,535 values; one per row) + `dict` ({ offsets, bytes }: the
   *  distinct values present) instead of a string per row. `true`: columns
   *  that are dictionary-encoded in the image (free — the wasm never builds
   *  the per-row form; dictionary order). `"all"`: also any other text column
   *  whose rows are less than half distinct, after a hash pass over its values
   *  (first-appearance order). */
  query(tableHandle, sql, { dictText = false } = {}) {
    if (dictText !== false && dictText !== true && dictText !== "all") throw new Error(`dictText: expected true, false or "all", got ${JSON.stringify(dictText)}`);
    const sqlBytes = this.enc.encode(sql);
    const sqlPtr = u32(this.w.alloc(sqlBytes.byteLength));
    new Uint8Array(this.mem(), sqlPtr, sqlBytes.byteLength).set(sqlBytes);
    // bit 0: keep dictionary columns as codes; bit 1: encode plain text too
    // (every allocation the result needs happens inside this call; the
    // col_* getters never grow memory)
    const flags = dictText === "all" ? 3 : dictText ? 1 : 0;
    const h = this.w.query_run_opts(tableHandle, sqlPtr, sqlBytes.byteLength, flags);
    this.w.dealloc(sqlPtr, sqlBytes.byteLength);
    return this._readOutcome(h);
  }

  /** Several statements in one call: the facet-shaped ones (single table,
   *  one dictionary GROUP BY key, count/sum items, any WHERE) run fused —
   *  masks and lanes shared, one lean pass each — and the rest run in order.
   *  Returns one result per statement; a failing statement throws a
   *  QueryError naming its index. */
  queryBatch(tableHandle, sqls, { dictText = false } = {}) {
    if (dictText !== false && dictText !== true && dictText !== "all") throw new Error(`dictText: expected true, false or "all", got ${JSON.stringify(dictText)}`);
    const bytes = this.enc.encode(sqls.join("\0"));
    const p = u32(this.w.alloc(bytes.byteLength));
    new Uint8Array(this.mem(), p, bytes.byteLength).set(bytes);
    const flags = dictText === "all" ? 3 : dictText ? 1 : 0;
    const h = this.w.query_batch(tableHandle, p, bytes.byteLength, flags);
    this.w.dealloc(p, bytes.byteLength);
    const n = this.w.batch_len(h);
    const handles = [];
    for (let i = 0; i < n; i++) handles.push(this.w.batch_outcome(h, i));
    this.w.batch_free(h);
    const out = [];
    let failed = null;
    for (let i = 0; i < n; i++) {
      try {
        out.push(this._readOutcome(handles[i]));
      } catch (e) {
        if (!failed) failed = new QueryError(`statement ${i}: ${e.message}`);
        for (let j = i + 1; j < n; j++) this.w.outcome_free(handles[j]); // free the rest, then fail
        break;
      }
    }
    if (failed) throw failed;
    return out;
  }

  /** Run SQL and keep its result in the wasm for paging (docs/result-handles.sv):
   *  the rows of a plain SELECT over the table as row ids (a u32 each; none
   *  for an unfiltered, unsorted one), a grouped or joined result whole.
   *  Returns { handle, rows, columns: [{ name, kind }], kind }, kind "rows"
   *  or "result". Free with `preparedFree`. */
  prepare(tableHandle, sql) {
    const [p, n] = this._put(sql);
    const h = this.w.prepared_run(tableHandle, p, n);
    this.w.dealloc(p, n);
    if (this.w.prepared_is_err(h)) {
      const len = this.w.prepared_error(h, this.scratch, 4096);
      const msg = this.dec.decode(new Uint8Array(this.mem(), this.scratch, len));
      this.w.prepared_free(h);
      throw new QueryError(msg);
    }
    const columns = [];
    for (let i = 0, nc = this.w.prepared_cols(h); i < nc; i++) {
      const len = this.w.prepared_col_name(h, i, this.scratch, 4096);
      columns.push({
        name: this.dec.decode(new Uint8Array(this.mem(), this.scratch, len)),
        kind: KINDS[this.w.prepared_col_kind(h, i)],
      });
    }
    return { handle: h, rows: this.w.prepared_rows(h), columns, kind: this.w.prepared_kind(h) ? "rows" : "result" };
  }

  /** Rows start..end (end exclusive, clamped to the result) of a prepared
   *  result, in the form `query` returns. `columns`: the result columns
   *  wanted, by name, in that order (default all). `dictText` as in `query`;
   *  a page's dictionary holds the values that page uses. */
  preparedPage(h, start, end, { columns, dictText = false } = {}) {
    if (dictText !== false && dictText !== true && dictText !== "all") throw new Error(`dictText: expected true, false or "all", got ${JSON.stringify(dictText)}`);
    if (!(Number.isInteger(start) && Number.isInteger(end) && start >= 0 && end >= start)) {
      throw new Error(`page: expected 0 <= start <= end, got ${start}, ${end}`);
    }
    const [p, n] = this._put(columns ? columns.join("\0") : "");
    const flags = dictText === "all" ? 3 : dictText ? 1 : 0;
    const o = this.w.prepared_page(h, Math.min(start, 0xffffffff), Math.min(end, 0xffffffff), p, n, columns ? 0 : 1, flags);
    this.w.dealloc(p, n);
    return this._readOutcome(o);
  }

  /** Re-order a prepared result: `orderBy` is the text after ORDER BY ("" for
   *  none). Rows of the table are re-sorted reading the key columns only,
   *  ties in table order; a kept result runs again. Returns the row count.
   *  Throws a QueryError, leaving the result as it was, when it cannot. */
  preparedSort(h, orderBy) {
    const [p, n] = this._put(String(orderBy ?? ""));
    const err = this.w.prepared_sort(h, p, n);
    this.w.dealloc(p, n);
    if (err) this._readOutcome(err); // throws its message, and frees it
    return this.w.prepared_rows(h);
  }

  preparedFree(h) {
    this.w.prepared_free(h);
  }

  /** A string's UTF-8 bytes in wasm memory: [pointer, length] (dealloc after). */
  _put(text) {
    const bytes = this.enc.encode(text);
    const p = u32(this.w.alloc(bytes.byteLength));
    new Uint8Array(this.mem(), p, bytes.byteLength).set(bytes);
    return [p, bytes.byteLength];
  }

  /** Copy an outcome's columns out of wasm memory and free it. */
  _readOutcome(h) {
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
          // pointers first, memory view last: a view taken before a call
          // into the wasm is detached if that call grows the memory
          const dn = this.w.col_dict_len(h, i);
          if (dn > 0) {
            const codesP = u32(this.w.col_codes_ptr(h, i));
            const wide = this.w.col_codes_width(h, i) === 4;
            const dictOffP = u32(this.w.col_dict_offsets_ptr(h, i));
            const dictBytesP = u32(this.w.col_dict_bytes_ptr(h, i));
            const dictBytesN = u32(this.w.col_dict_bytes_len(h, i));
            const mem = this.mem();
            col.codes = (wide ? new Uint32Array(mem, codesP, rowCount) : new Uint16Array(mem, codesP, rowCount)).slice();
            col.dict = {
              offsets: new Uint32Array(mem, dictOffP, dn + 1).slice(),
              bytes: new Uint8Array(mem, dictBytesP, dictBytesN).slice(),
            };
          } else {
            const offP = u32(this.w.col_offsets_ptr(h, i));
            const bytesP = u32(this.w.col_bytes_ptr(h, i));
            const bytesN = u32(this.w.col_bytes_len(h, i));
            const mem = this.mem();
            col.offsets = new Uint32Array(mem, offP, rowCount + 1).slice();
            col.bytes = new Uint8Array(mem, bytesP, bytesN).slice();
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
