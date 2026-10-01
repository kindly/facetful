// Where do grantnav's download bytes go? Each column compressed on its own, in
// its stored form (plain text: u32 offsets + bytes; dictionary: codes at their
// stored width + the dictionary once; numbers at their stored width), with
// brotli q11/lgwin24 and zstd 19 + long-distance matching, in the image's row
// order and sorted by funder, programme, award date.
//   UV_THREADPOOL_SIZE=8 node spikes/fts-bench/colsize.mjs  -> out/colsize.json
import { writeFileSync } from "node:fs";
import { promisify } from "node:util";
import zlib from "node:zlib";
import { openEngine, ident, OUT } from "./common.mjs";

const brotli = promisify(zlib.brotliCompress), zstd = promisify(zlib.zstdCompress);
const br = (b) => brotli(b, { params: { [zlib.constants.BROTLI_PARAM_QUALITY]: 11, [zlib.constants.BROTLI_PARAM_LGWIN]: 24, [zlib.constants.BROTLI_PARAM_SIZE_HINT]: b.length } }).then((x) => x.length);
const zs = (b) => zstd(b, { params: { [zlib.constants.ZSTD_c_compressionLevel]: 19, [zlib.constants.ZSTD_c_windowLog]: 27, [zlib.constants.ZSTD_c_enableLongDistanceMatching]: 1 } }).then((x) => x.length);

const { engine, handle } = await openEngine();
const desc = engine.describe(handle);
const rows = desc.rows;
const get = (c) => engine.query(handle, `select ${ident(c)} from t`, { dictText: true }).columns[0];

// sort permutation: funder, programme, award date (codes compare as the dictionary orders them; any stable grouping does)
const fk = get("Funding Org:Name").codes, pk = get("Grant Programme:Title").codes, dk = get("Award Date").values;
const perm = new Uint32Array(rows).map((_, i) => i).sort((a, b) => fk[a] - fk[b] || pk[a] - pk[b] || dk[a] - dk[b] || a - b);

const widthOf = (kind) => ({ int8: 1, int16: 2, int32: 4, int64: 8, date: 4, timestamp: 8, float64: 8 })[kind];
function stored(col, def, order) {
  const n = rows, at = (i) => (order ? order[i] : i);
  if (col.codes) {
    const w = def.kind.includes("u8") ? 1 : def.kind.includes("u16") ? 2 : 4;
    const codes = Buffer.alloc(n * w);
    for (let i = 0; i < n; i++) { const c = col.codes[at(i)]; w === 1 ? codes.writeUInt8(c, i) : w === 2 ? codes.writeUInt16LE(c, i * 2) : codes.writeUInt32LE(c, i * 4); }
    return { parts: [codes], once: [Buffer.from(col.dict.offsets.buffer), Buffer.from(col.dict.bytes)] };
  }
  if (col.offsets) {
    const lens = new Uint32Array(n);
    let total = 0;
    for (let i = 0; i < n; i++) { const r = at(i); lens[i] = col.offsets[r + 1] - col.offsets[r]; total += lens[i]; }
    const bytes = Buffer.alloc(total), offs = Buffer.alloc((n + 1) * 4);
    let pos = 0;
    for (let i = 0; i < n; i++) { const r = at(i); bytes.set(col.bytes.subarray(col.offsets[r], col.offsets[r + 1]), pos); offs.writeUInt32LE(pos, i * 4); pos += lens[i]; }
    offs.writeUInt32LE(pos, n * 4);
    return { parts: [offs, bytes] };
  }
  const w = widthOf(def.kind);
  const b = Buffer.alloc(n * w);
  for (let i = 0; i < n; i++) {
    const v = col.values[at(i)];
    if (w === 8 && def.kind === "float64") b.writeDoubleLE(v, i * 8);
    else if (w === 8) b.writeBigInt64LE(BigInt(v), i * 8);
    else if (w === 4) b.writeInt32LE(v, i * 4);
    else if (w === 2) b.writeInt16LE(v, i * 2);
    else b.writeInt8(v, i);
  }
  return { parts: [b] };
}
const size = async (bufs, f) => (await Promise.all(bufs.map(f))).reduce((a, b) => a + b, 0);

const out = [];
for (const def of desc.columns) {
  const t0 = performance.now();
  const col = get(def.name);
  const res = { name: def.name, kind: def.kind };
  for (const [label, order] of [["file", null], ["sorted", perm]]) {
    const s = stored(col, def, order);
    const all = [...s.parts, ...(s.once || [])];
    res[label] = { raw: all.reduce((a, b) => a + b.length, 0), br: await size(all, br), zstd: await size(all, zs) };
  }
  out.push(res);
  console.log(`${def.name.padEnd(42)} raw ${(res.file.raw / 1e6).toFixed(1).padStart(6)} MB | br file ${(res.file.br / 1e6).toFixed(2).padStart(6)} sorted ${(res.sorted.br / 1e6).toFixed(2).padStart(6)} | zstd file ${(res.file.zstd / 1e6).toFixed(2).padStart(6)} sorted ${(res.sorted.zstd / 1e6).toFixed(2).padStart(6)} | ${((performance.now() - t0) / 1000).toFixed(0)} s`);
}
const tot = (k, m) => out.reduce((a, r) => a + r[k][m], 0) / 1e6;
console.log(`TOTAL raw ${tot("file", "raw").toFixed(1)} MB | br file ${tot("file", "br").toFixed(2)} sorted ${tot("sorted", "br").toFixed(2)} | zstd file ${tot("file", "zstd").toFixed(2)} sorted ${tot("sorted", "zstd").toFixed(2)}  (deployed whole-file .br: 58.04 MB)`);
writeFileSync(`${OUT}colsize.json`, JSON.stringify(out, null, 2));
