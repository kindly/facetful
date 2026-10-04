// Parquet -> compiler input columns, via a caller-supplied hyparquet module.
// Shared by the worker (browser) and the node differential test; keeping it
// environment-free is what makes the "parquet path === image path" check
// runnable headlessly.

const enc = new TextEncoder();

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
  const root = meta.schema[0];
  const elems = meta.schema.slice(1, 1 + Number(root.num_children));
  for (const e of elems) {
    if (e.num_children) {
      throw new Error(`nested parquet column '${e.name}' is not supported`);
    }
  }
  const isText = (e) => e.type === "BYTE_ARRAY";
  const isInt = (e) => e.type === "INT32" || e.type === "INT64" || e.type === "BOOLEAN";
  const temporalOf = (e) => {
    const lt = e.logical_type?.type;
    const ct = e.converted_type;
    if (lt === "DATE" || ct === "DATE") return "date";
    if (lt === "TIMESTAMP" || ct === "TIMESTAMP_MILLIS" || ct === "TIMESTAMP_MICROS") {
      return "timestamp";
    }
    return undefined;
  };

  // builders per column
  const byName = new Map();
  for (const e of elems) {
    const b = { elem: e, validity: new Uint8Array(rows).fill(1), anyNull: false };
    if (isText(e)) b.chunks = []; // { rowStart, data }, packed once all have arrived
    else b.nums = new Float64Array(rows);
    byName.set(e.name, b);
  }

  await h.parquetRead({
    file: buffer,
    metadata: meta,
    columns: elems.map((e) => e.name),
    // text stays as the file's UTF-8 bytes: decoding every value to a JS
    // string and encoding it back was most of the transcode (3.6 of 5 s on
    // 183K rows x 42 text columns); the compiler repairs invalid UTF-8
    parsers: { stringFromBytes: (bytes) => bytes },
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
      const temporal = temporalOf(e);
      if (temporal === "date") {
        // hyparquet yields Date objects at UTC midnight; store days
        for (let i = 0; i < rows; i++) b.nums[i] = Math.round(b.nums[i] / 86400000);
      }
      return { name: e.name, kind: "num", data: b.nums, isInt: isInt(e), temporal, validity };
    }),
  };
}

/** One text column's chunks as offsets + bytes, in row order. Values are
 *  bytes; anything else (a string from a hyparquet without `parsers`, a
 *  decimal, JSON) is encoded from its string form. Rows no chunk covers stay
 *  empty, as before. */
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
      if (!(v instanceof Uint8Array)) v = enc.encode(String(v));
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
