// Generate spikes/facet-spike/data-${N}.parquet using hyparquet-writer
// (snappy-free default; a typical published parquet file).
import { readFileSync, writeFileSync } from "node:fs";
import { parquetWriteBuffer } from "hyparquet-writer";

const N = process.argv[2] || "200000";

const csv = readFileSync(new URL(`../../spikes/facet-spike/data-${N}.csv`, import.meta.url), "utf8");
const lines = csv.trim().split("\n");
const header = lines[0].split(",");
const cols = header.map(() => []);
for (let i = 1; i < lines.length; i++) {
  const parts = lines[i].split(",");
  for (let c = 0; c < header.length; c++) cols[c].push(parts[c]);
}
const columnData = header.map((name, c) => {
  if (name === "capacity") return { name, data: cols[c].map(Number), type: "DOUBLE" };
  if (name === "id") return { name, data: cols[c].map((v) => BigInt(v)), type: "INT64" };
  return { name, data: cols[c], type: "STRING" };
});
const buf = parquetWriteBuffer({ columnData });
writeFileSync(new URL(`../../spikes/facet-spike/data-${N}.parquet`, import.meta.url), new Uint8Array(buf));
console.log(`data-${N}.parquet: ${buf.byteLength} bytes`);
