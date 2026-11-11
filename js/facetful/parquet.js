// Parquet -> compiler input columns, via a caller-supplied hyparquet module.
// Shared by the worker (browser) and the node differential test; keeping it
// environment-free is what makes the "parquet path === image path" check
// runnable headlessly.

const enc = new TextEncoder();
const dec = new TextDecoder();

/**
 * How a top-level Parquet column is stored: "int", "float", "date",
 * "timestamp" or "text". The common scalar types keep their meaning; anything
 * else (nested lists/structs/maps, JSON, UUID, geometry, fixed-length binary)
 * becomes text, so no file is refused for one unusual column.
 */
export function parquetKind(e) {
  if (e.num_children) return "text";
  const lt = e.logical_type?.type;
  const ct = e.converted_type;
  if (lt === "DATE" || ct === "DATE") return "date";
  if (lt === "TIMESTAMP" || ct === "TIMESTAMP_MILLIS" || ct === "TIMESTAMP_MICROS") return "timestamp";
  if (e.type === "INT96" && !ct) return "timestamp"; // the legacy timestamp encoding
  const int = e.type === "INT32" || e.type === "INT64";
  // hyparquet scales decimals to numbers, whatever they are stored in
  if (ct === "DECIMAL") return int && !e.scale ? "int" : "float";
  if (lt === "DECIMAL") return int && !e.logical_type.scale ? "int" : "text";
  if (int || e.type === "BOOLEAN") return "int";
  if (e.type === "FLOAT" || e.type === "DOUBLE" || lt === "FLOAT16") return "float";
  return "text";
}

/** The schema's top-level columns: it is a depth-first list, so a nested
 *  column's descendants are skipped over. */
function topLevel(schema) {
  const out = [];
  const skip = (i) => {
    let next = i + 1;
    for (let c = 0; c < Number(schema[i].num_children || 0); c++) next = skip(next);
    return next;
  };
  for (let i = 1; i < schema.length; i = skip(i)) out.push(schema[i]);
  return out;
}

/**
 * Every parser hyparquet calls, not only the ones that differ from its
 * defaults: some versions (1.29) replace their defaults with the caller's
 * object instead of merging, and a DATE column then fails outright.
 * Text and JSON stay as the file's UTF-8 bytes: decoding every value to a JS
 * string and encoding it back was most of the transcode (3.6 of 5 s on 183K
 * rows x 42 text columns); the compiler repairs invalid UTF-8. Geometry
 * stays as its WKB bytes (shown as hex).
 */
const PARSERS = {
  timestampFromMilliseconds: (ms) => new Date(Number(ms)),
  timestampFromMicroseconds: (us) => new Date(Number(us / 1000n)),
  timestampFromNanoseconds: (ns) => new Date(Number(ns / 1000000n)),
  dateFromDays: (days) => new Date(days * 86400000),
  stringFromBytes: (bytes) => bytes,
  jsonFromBytes: (bytes) => bytes,
  geometryFromBytes: (bytes) => bytes,
  geographyFromBytes: (bytes) => bytes,
  uuidFromBytes(bytes) {
    if (!bytes) return undefined;
    const h = hex(bytes);
    return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
  },
};

const NESTED_PARSERS = {
  ...PARSERS,
  stringFromBytes: (bytes) => bytes && dec.decode(bytes),
  jsonFromBytes: (bytes) => bytes && dec.decode(bytes),
};

const hex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

/** A value with no column type of its own, as text. Nested values are JSON. */
function textOf(v, binary) {
  if (typeof v === "string") return v;
  if (v instanceof Date) return v.toISOString();
  if (v instanceof Uint8Array) return binary ? hex(v) : dec.decode(v);
  if (typeof v === "object") {
    return JSON.stringify(v, (_, x) =>
      typeof x === "bigint" ? (Number.isSafeInteger(Number(x)) ? Number(x) : String(x))
        : x instanceof Uint8Array ? dec.decode(x)
        : x);
  }
  return String(v);
}

/** hyparquet decompressors from the fzstd module (ZSTD). */
export const zstdCompressors = (fzstd) => ({
  ZSTD: (input, outputLength) => fzstd.decompress(input, new Uint8Array(outputLength)),
});

/** Codecs hyparquet decodes by itself. */
const BUILTIN_CODECS = new Set(["UNCOMPRESSED", "SNAPPY"]);

/**
 * Read a Parquet buffer into compiler input columns. `compressorsFor(codecs)`
 * returns hyparquet decompressors (`{ ZSTD: (input, outputLength) => bytes }`)
 * for the codecs the file uses beyond the built-in ones, loaded only when a
 * file needs them.
 */
export async function parquetToColumns(h, buffer, { compressorsFor } = {}) {
  const meta = await h.parquetMetadataAsync(buffer);
  const codecs = new Set();
  for (const g of meta.row_groups) for (const c of g.columns) codecs.add(c.meta_data?.codec);
  const extra = [...codecs].filter((c) => c && !BUILTIN_CODECS.has(c));
  const compressors = extra.length && compressorsFor ? await compressorsFor(new Set(extra)) : {};
  const missing = extra.filter((c) => !compressors[c]);
  if (missing.length) {
    throw new Error(`parquet compression ${missing.join(", ")} is not supported (use ZSTD, SNAPPY or none)`);
  }
  const rows = Number(meta.num_rows);
  const elems = topLevel(meta.schema);

  // builders per column
  const byName = new Map();
  for (const e of elems) {
    const b = { elem: e, kind: parquetKind(e), validity: new Uint8Array(rows).fill(1), anyNull: false };
    if (b.kind === "text") {
      b.chunks = []; // { rowStart, data }, packed once all have arrived
      // plain BYTE_ARRAY values are the file's UTF-8; other bytes print as hex
      const lt = e.logical_type?.type;
      b.binary = e.type !== "BYTE_ARRAY" || lt === "GEOMETRY" || lt === "GEOGRAPHY";
    } else b.nums = new Float64Array(rows);
    byName.set(e.name, b);
  }

  const read = (cols, parsers) => cols.length && h.parquetRead({
    file: buffer,
    metadata: meta,
    columns: cols.map((e) => e.name),
    parsers,
    compressors,
    onChunk({ columnName, columnData, rowStart }) {
      const b = byName.get(columnName);
      if (!b) return;
      if (b.chunks) {
        b.chunks.push({ rowStart, data: columnData });
      } else {
        for (let i = 0; i < columnData.length; i++) {
          const v = columnData[i];
          if (v == null) {
            b.validity[rowStart + i] = 0;
            b.anyNull = true;
          } else {
            // BigInt (INT64), Date (DATE/TIMESTAMP -> ms), boolean all coerce
            b.nums[rowStart + i] =
              typeof v === "boolean" ? (v ? 1 : 0) : v instanceof Date ? v.getTime() : Number(v);
          }
        }
      }
    },
    onComplete() {},
  });
  await read(elems.filter((e) => !e.num_children), PARSERS);
  // nested values are assembled by hyparquet, which needs real strings (a
  // map's keys are object keys)
  await read(elems.filter((e) => e.num_children), NESTED_PARSERS);

  return {
    rows,
    columns: elems.map((e) => {
      const b = byName.get(e.name);
      // text nulls are found while packing
      const text = b.chunks && packText(b, rows);
      const validity = b.anyNull ? b.validity : undefined;
      if (text) return { name: e.name, kind: "text", ...text, validity };
      // int-vs-float comes from the parquet physical type, never from values
      // (a DOUBLE column of integral values must stay Float64, like the CLI);
      // DATE/TIMESTAMP become real temporal columns (days / ms since epoch)
      const temporal = b.kind === "date" || b.kind === "timestamp" ? b.kind : undefined;
      if (temporal === "date") {
        // hyparquet yields Date objects at UTC midnight; store days
        for (let i = 0; i < rows; i++) b.nums[i] = Math.round(b.nums[i] / 86400000);
      }
      return { name: e.name, kind: "num", data: b.nums, isInt: b.kind === "int", temporal, validity };
    }),
  };
}

/** One text column's chunks as offsets + bytes, in row order. Values are
 *  bytes; anything else (a string from a hyparquet without `parsers`, JSON,
 *  a nested value) is encoded from its text form, see `textOf`. Rows no
 *  chunk covers stay empty, as before. */
function packText(b, rows) {
  b.chunks.sort((x, y) => x.rowStart - y.rowStart);
  const vals = new Array(rows).fill(null);
  let total = 0;
  for (const { rowStart, data } of b.chunks) {
    for (let i = 0; i < data.length; i++) {
      let v = data[i];
      if (v == null) {
        b.validity[rowStart + i] = 0;
        b.anyNull = true;
        continue;
      }
      if (!(v instanceof Uint8Array) || b.binary) v = enc.encode(textOf(v, b.binary));
      vals[rowStart + i] = v;
      total += v.byteLength;
    }
  }
  b.chunks = null;
  const offsets = new Uint32Array(rows + 1);
  const bytes = new Uint8Array(total);
  let at = 0;
  for (let i = 0; i < rows; i++) {
    const v = vals[i];
    if (v) {
      bytes.set(v, at);
      at += v.byteLength;
    }
    offsets[i + 1] = at;
  }
  return { offsets, bytes };
}
