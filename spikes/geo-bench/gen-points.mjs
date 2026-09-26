// node spikes/geo-bench/gen-points.mjs <rows> <out.csv> — uniform lat [-60,70), lon [-180,180)
import { writeFileSync } from "node:fs";
const [n, out] = [Number(process.argv[2]), process.argv[3]];
let a = 1;
const r = () => {
  a |= 0; a = (a + 0x6d2b79f5) | 0;
  let t = Math.imul(a ^ (a >>> 15), 1 | a);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const lines = ["Latitude,Longitude"];
for (let i = 0; i < n; i++) lines.push((r() * 130 - 60).toFixed(5) + "," + (r() * 360 - 180).toFixed(5));
writeFileSync(out, lines.join("\n") + "\n");
