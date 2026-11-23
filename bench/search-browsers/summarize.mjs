// Tables from a results file of run.mjs (or the standalone server).
//   node bench/search-browsers/summarize.mjs results.jsonl
import fs from "node:fs";

const rows = fs.readFileSync(process.argv[2], "utf8").trim().split("\n").map((l) => JSON.parse(l)).filter((r) => !r.done);
const browser = (ua) => (/Firefox\/(\d+)/.exec(ua) ? `Firefox ${RegExp.$1}` : /iPhone|iPad/.test(ua) ? "iOS Safari" : /Version\/[\d.]+ .*Safari/.test(ua) ? "Safari" : /Chrome\/(\d+)/.exec(ua) ? `Chrome ${RegExp.$1}` : ua.slice(0, 30));
const s = (v) => (v == null ? "" : v >= 1000 ? `${(v / 1000).toFixed(2)} s` : `${Math.round(v)} ms`);
const med = (a) => [...a].sort((x, y) => x - y)[a.length >> 1];
const keys = (r, k) => r.typing.flatMap((t) => t[k]);
const by = (phase) => rows.filter((r) => r.phase === phase && !r.error);

console.log("## First visit (streamed, index built as it arrives)\n");
console.log("| browser | dataset | network | table ready | search ready | keystroke: search / suggest / ranked, median (worst) | index saved | wasm memory | page memory |");
console.log("|---|---|---|---|---|---|---|---|---|");
for (const r of by("first")) {
  const k = ["search", "suggest", "rank"].map((x) => `${s(med(keys(r, x)))} (${s(Math.max(...keys(r, x)))})`).join(" / ");
  console.log(`| ${browser(r.ua)} | ${r.dataset} | ${r.profile} | ${s(r.marks.tableReady)} | ${s(r.marks.searchReady)} | ${k} | ${r.marks.indexSavedBytes ? `${(r.marks.indexSavedBytes / 1e6).toFixed(1)} MB in ${s(r.marks.indexSave)}` : "no OPFS"} | ${r.memory.end?.wasmMB} MB | ${r.memory.end?.pageMB ? r.memory.end.pageMB + " MB" : ""} |`);
}
console.log("\n## Repeat visit (from OPFS)\n");
console.log("| browser | dataset | network | open image | index | keystroke: search / ranked, median | wasm memory |");
console.log("|---|---|---|---|---|---|---|");
for (const r of by("repeat")) {
  console.log(`| ${browser(r.ua)} | ${r.dataset} | ${r.profile} | ${s(r.marks.opfsOpen)} | ${r.marks.indexCached ? "read back" : "rebuilt"} in ${s(r.marks.indexFromOpfs)} | ${s(med(keys(r, "search")))} / ${s(med(keys(r, "rank")))} | ${r.memory.end?.wasmMB} MB |`);
}
const split = by("split");
if (split.length) {
  console.log("\n## Split image (first part, then the second attached)\n");
  console.log("| browser | network | first part ready | all ready | wasm memory |");
  console.log("|---|---|---|---|---|");
  for (const r of split) console.log(`| ${browser(r.ua)} | ${r.profile} | ${s(r.marks.firstPartReady)} | ${s(r.marks.allReady)} | ${r.memory.end?.wasmMB} MB |`);
}
const errors = rows.filter((r) => r.error);
if (errors.length) {
  console.log("\n## Errors\n");
  for (const r of errors) console.log(`- ${browser(r.ua)} ${r.dataset} ${r.profile} ${r.phase}: ${r.error.split("\n")[0]}`);
}
