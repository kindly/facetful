// Parquet -> compiler input columns, via a caller-supplied hyparquet module.
// Shared by the worker (browser) and the node differential test; keeping it
// environment-free is what makes the "parquet path === image path" check
// runnable headlessly.

const enc = new TextEncoder();

/** Read a Parquet buffer into compiler input columns. */
export async function parquetToColumns(h, buffer) {
  const meta = await h.parquetMetadataAsync(buffer);
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
    if (isText(e)) b.strs = new Array(rows).fill("");
    else b.nums = new Float64Array(rows);
    byName.set(e.name, b);
  }

  await h.parquetRead({
    file: buffer,
    metadata: meta,
    columns: elems.map((e) => e.name),
    onChunk({ columnName, columnData, rowStart }) {
      const b = byName.get(columnName);
      if (!b) return;
      if (b.strs) {
        for (let i = 0; i < columnData.length; i++) {
          const v = columnData[i];
          if (v == null) {
            b.validity[rowStart + i] = 0;
            b.anyNull = true;
          } else {
            b.strs[rowStart + i] = String(v);
          }
        }
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
      const validity = b.anyNull ? b.validity : undefined;
      if (b.strs) {
        const offsets = new Uint32Array(rows + 1);
        const parts = new Array(rows);
        let total = 0;
        for (let i = 0; i < rows; i++) {
          parts[i] = enc.encode(b.strs[i]);
          total += parts[i].byteLength;
          offsets[i + 1] = total;
        }
        const bytes = new Uint8Array(total);
        for (let i = 0; i < rows; i++) bytes.set(parts[i], offsets[i]);
        return { name: e.name, kind: "text", offsets, bytes, validity };
      }
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

