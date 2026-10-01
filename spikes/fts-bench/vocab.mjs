// Pass 1: the vocabulary of grantnav's search columns, straight from the image.
//   node spikes/fts-bench/vocab.mjs     -> out/vocab.tsv, out/vocab-stats.json
// Row counts are per row (a word in the title and the description counts once).
import { mkdirSync, writeFileSync } from "node:fs";
import { gzipSync, brotliCompressSync, constants } from "node:zlib";
import { openEngine, COLS, ident, tokenize, OUT } from "./common.mjs";

const { engine, handle } = await openEngine();
const rows = engine.query(handle, "select count(*) as n from t").columns[0].values[0];
const CHUNK = 65536;
const vocab = new Map(); // term -> Int32Array [rows, per-field rows x5, occurrences]
const fieldBytes = new Array(COLS.length).fill(0), fieldTokens = new Array(COLS.length).fill(0), fieldNonAscii = new Array(COLS.length).fill(0);
const cache = [new Map(), new Map(), new Map(), new Map(), new Map()]; // dictionary-ish columns: tokenize each distinct value once
const CACHED = new Set([1, 2, 3]);
let occurrences = 0, pairs = 0;
const t0 = performance.now();
let tQuery = 0;
for (let off = 0; off < rows; off += CHUNK) {
  const tq = performance.now();
  const r = engine.query(handle, `select ${COLS.map(ident).join(", ")} from t limit ${CHUNK} offset ${off}`);
  tQuery += performance.now() - tq;
  const n = r.rowCount;
  const perRow = new Map(); // term -> field bitmask, for one row
  for (let i = 0; i < n; i++) {
    perRow.clear();
    for (let f = 0; f < COLS.length; f++) {
      const c = r.columns[f];
      if (c.validity && !((c.validity[i >> 3] >> (i & 7)) & 1)) continue;
      const s = c.offsets[i], e = c.offsets[i + 1];
      if (e === s) continue;
      fieldBytes[f] += e - s;
      let toks;
      if (CACHED.has(f)) {
        const key = Buffer.from(c.bytes.buffer, c.bytes.byteOffset + s, e - s).toString("latin1");
        toks = cache[f].get(key);
        if (!toks) { toks = []; if (!tokenize(c.bytes, s, e, toks)) toks.nonAscii = true; cache[f].set(key, toks); }
      } else {
        toks = [];
        if (!tokenize(c.bytes, s, e, toks)) toks.nonAscii = true;
      }
      if (toks.nonAscii) fieldNonAscii[f] += e - s;
      fieldTokens[f] += toks.length;
      occurrences += toks.length;
      for (const t of toks) {
        perRow.set(t, (perRow.get(t) || 0) | (1 << f));
        let v = vocab.get(t);
        if (!v) { v = new Int32Array(7); vocab.set(t, v); }
        v[6]++;
      }
    }
    for (const [t, mask] of perRow) {
      const v = vocab.get(t);
      v[0]++;
      pairs++;
      for (let f = 0; f < COLS.length; f++) if (mask & (1 << f)) v[1 + f]++;
    }
  }
  process.stderr.write(`\r${Math.min(off + CHUNK, rows).toLocaleString()} / ${rows.toLocaleString()} rows, ${vocab.size.toLocaleString()} terms`);
}
process.stderr.write("\n");
const elapsed = performance.now() - t0;

const terms = [...vocab.keys()].sort();
mkdirSync(OUT, { recursive: true });
const lines = terms.map((t) => { const v = vocab.get(t); return `${t}\t${v[0]}\t${v[1]}\t${v[2]}\t${v[3]}\t${v[4]}\t${v[5]}\t${v[6]}`; });
writeFileSync(`${OUT}vocab.tsv`, `term\trows\t${COLS.join("\t")}\toccurrences\n` + lines.join("\n") + "\n");

// the artifact a no-index design would ship: term + row count, sorted
const small = Buffer.from(terms.map((t) => `${t}\t${vocab.get(t)[0]}`).join("\n"));
const hist = { "1": 0, "2": 0, "3-9": 0, "10-99": 0, "100-999": 0, "1K-9K": 0, "10K-99K": 0, "100K+": 0 };
const pairsBy = { ...hist };
for (const v of vocab.values()) {
  const d = v[0];
  const k = d === 1 ? "1" : d === 2 ? "2" : d < 10 ? "3-9" : d < 100 ? "10-99" : d < 1000 ? "100-999" : d < 1e4 ? "1K-9K" : d < 1e5 ? "10K-99K" : "100K+";
  hist[k]++; pairsBy[k] += d;
}
const stats = {
  rows, elapsedMs: Math.round(elapsed), queryMs: Math.round(tQuery),
  distinctTerms: vocab.size, occurrences, termRowPairs: pairs,
  fields: COLS.map((c, f) => ({ col: c, bytes: fieldBytes[f], tokens: fieldTokens[f], nonAsciiBytes: fieldNonAscii[f], distinctValuesTokenized: CACHED.has(f) ? cache[f].size : null })),
  rowCountHistogram: hist, termRowPairsByBucket: pairsBy,
  vocabArtifact: { rawBytes: small.length, gzip: gzipSync(small, { level: 9 }).length, brotli: brotliCompressSync(small, { params: { [constants.BROTLI_PARAM_QUALITY]: 11 } }).length },
};
writeFileSync(`${OUT}vocab-stats.json`, JSON.stringify(stats, null, 2));
console.log(JSON.stringify(stats, null, 2));
