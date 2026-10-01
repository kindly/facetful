// Follow-ups to bench.mjs:
//  a. exact counts with inflections, to split LIKE's extra rows into wanted
//     forms (supports, organisations) and junk (fundamental for fund)
//  b. typo candidates pruned to >= 1% of the best candidate's rows: LIKE cost
//  c. a JS text check that searches instead of tokenizing everything
//     (one decode + lowercase per lane, indexOf, boundary check), and
//     next_words for phrase autocomplete, with and without a facet filter
import { readFileSync } from "node:fs";
import { openEngine, COLS, ident, lit, tokenizeStr, median, OUT } from "./common.mjs";

const { engine, handle } = await openEngine();
engine.setMaskBudget(handle, 0);
const count = (where) => engine.query(handle, `select count(*) as n from t where ${where}`).columns[0].values[0];
const anyCol = (pat) => `(${COLS.map((c) => `${ident(c)} like ${lit(pat)}`).join(" or ")})`;
const f1 = (x) => x.toFixed(1);
const vocab = new Map();
for (const line of readFileSync(`${OUT}vocab.tsv`, "utf8").split("\n").slice(1)) {
  if (!line) continue;
  const tab = line.indexOf("\t");
  vocab.set(line.slice(0, tab), parseInt(line.slice(tab + 1), 10));
}

// --- inflections, limited to forms in the vocabulary --------------------------------
const VOWEL = /[aeiou]/;
function bases(w) {
  const out = new Set([w]);
  const strip = (suf, add = "") => { if (w.length > suf.length + 2 && w.endsWith(suf)) out.add(w.slice(0, -suf.length) + add); };
  if (!/(ss|us|is)$/.test(w)) strip("s");
  strip("es"); strip("ies", "y"); strip("ed"); strip("ed", "e"); strip("ing"); strip("ing", "e"); strip("ers"); strip("er"); strip("er", "e");
  for (const b of [...out]) if (/(.)\1$/.test(b) && !/(ss|ll)$/.test(b)) out.add(b.slice(0, -1)); // planning -> plann -> plan
  return out;
}
function family(w) {
  const forms = new Set();
  for (const b of bases(w)) {
    if (!vocab.has(b) && b !== w) continue;
    const last = b.at(-1), dbl = b.length > 2 && !VOWEL.test(last) && VOWEL.test(b.at(-2)) && !VOWEL.test(b.at(-3) || "a") ? b + last : null;
    const cand = [b, b + "s", b + "es", b + "ed", b + "d", b + "ing", b + "er", b + "ers"];
    if (last === "y") cand.push(b.slice(0, -1) + "ies", b.slice(0, -1) + "ied");
    if (last === "e") cand.push(b.slice(0, -1) + "ing");
    if (dbl) cand.push(dbl + "ing", dbl + "ed", dbl + "er");
    for (const c of cand) if (vocab.has(c)) forms.add(c);
  }
  forms.add(w);
  return [...forms].filter((f) => vocab.has(f));
}

// --- the JS text check -----------------------------------------------------------------
const dec = new TextDecoder();
const isWordCode = (c) => (c >= 48 && c <= 57) || (c >= 97 && c <= 122) || c > 127;
/** A result lane as one lowercased string plus char offsets per row. */
function lane(c, n) {
  const whole = dec.decode(c.bytes);
  if (whole.length === c.bytes.length) return { s: whole.toLowerCase(), off: c.offsets, n };
  // non-ASCII: per-row decode, NFD + drop marks so it folds like the index
  const parts = new Array(n), off = new Uint32Array(n + 1);
  let pos = 0;
  for (let i = 0; i < n; i++) {
    let t = dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1]));
    for (let j = 0; j < t.length; j++) if (t.charCodeAt(j) > 127) { t = t.normalize("NFD").replace(/\p{M}+/gu, ""); break; }
    parts[i] = t.toLowerCase(); pos += parts[i].length; off[i + 1] = pos;
  }
  return { s: parts.join(""), off, n };
}
/** Visit every whole-word hit of `needle` in the lane: fn(row, endPos). */
function hits(L, needle, fn) {
  const { s, off } = L;
  let row = 0;
  for (let p = s.indexOf(needle); p >= 0; p = s.indexOf(needle, p + 1)) {
    while (off[row + 1] <= p) row++;
    const e = p + needle.length;
    if (e > off[row + 1]) continue;
    if (p > off[row] && isWordCode(s.charCodeAt(p - 1))) continue;
    if (e < off[row + 1] && isWordCode(s.charCodeAt(e))) continue;
    fn(row, e);
  }
}
/** Rows where every word has one of its forms as a whole word in some field. */
function exactRows(lanes, formSets, n) {
  const seen = formSets.map(() => new Uint8Array(n));
  formSets.forEach((fs, k) => { for (const L of lanes) for (const f of fs) hits(L, f, (row) => { seen[k][row] = 1; }); });
  let m = 0;
  for (let i = 0; i < n; i++) if (seen.every((a) => a[i])) m++;
  return m;
}
/** The next `k` words after the phrase, counted once per row. */
function nextWords(lanes, phrase, k = 2) {
  const counts = new Map();
  for (const L of lanes) {
    const { s, off } = L;
    let lastRow = -1, seenRow = new Set();
    hits(L, phrase, (row, e) => {
      if (row !== lastRow) { seenRow = new Set(); lastRow = row; }
      const rest = s.slice(e, Math.min(off[row + 1], e + 60)).split(/[^\p{L}\p{N}]+/u).filter(Boolean).slice(0, k);
      if (!rest.length) return;
      const key = rest.join(" ");
      if (seenRow.has(key)) return;
      seenRow.add(key);
      counts.set(key, (counts.get(key) || 0) + 1);
    });
  }
  return [...counts].sort((a, b) => b[1] - a[1]).slice(0, 8);
}

// --- a. LIKE vs exact vs exact + inflections -----------------------------------------
console.log("## a. LIKE substring vs whole words vs whole words + inflections\n");
console.log("| query | LIKE rows | exact rows | + inflections | inflection forms used |\n|---|---|---|---|---|");
const CLEAN = ["climate", "mental health", "youth sport london", "community project", "organisation support", "charities", "fund", "heath", "plan"];
const sel = (where) => engine.query(handle, `select ${COLS.map(ident).join(", ")} from t where ${where}`);
for (const q of CLEAN) {
  const words = tokenizeStr(q);
  const fams = words.map(family);
  // candidates: LIKE on the words themselves, which covers every family member here
  const where = words.map((w) => anyCol(`%${w.length > 4 && w.endsWith("s") ? w.slice(0, -1) : w}%`)).join(" and ");
  const r = sel(where), n = r.rowCount;
  const lanes = r.columns.map((c) => lane(c, n));
  const likeRows = count(words.map((w) => anyCol(`%${w}%`)).join(" and "));
  console.log(`| \`${q}\` | ${likeRows.toLocaleString()} | ${exactRows(lanes, words.map((w) => [w]), n).toLocaleString()} | ${exactRows(lanes, fams, n).toLocaleString()} | ${fams.map((f) => f.join("\\|")).join(" ")} |`);
}

// --- b. pruned typo candidates ------------------------------------------------------------
console.log("\n## b. typo queries with candidates pruned to >= 1% of the best candidate\n");
console.log("| query | forms | LIKE ms |\n|---|---|---|");
const PRUNED = {
  "yuoth sprot londn": [["youth"], ["sport"], ["london"]],
  "enviromental helth": [["enviromental", "environmental"], ["helth", "health", "heath"]],
  "mental helth servics": [["mental"], ["helth", "health", "heath"], ["servics", "services", "service"]],
};
for (const [q, fs] of Object.entries(PRUNED)) {
  const where = fs.map((f) => `(${f.map((x) => anyCol(`%${x}%`)).join(" or ")})`).join(" and ");
  console.log(`| \`${q}\` | ${fs.map((f) => f.join("\\|")).join(" ")} | ${f1(median(() => count(where), 5, 1))} |`);
}

// --- c. JS text check and next_words ------------------------------------------------------
console.log("\n## c. JS text check over candidate rows (the UDF's work, minus the UDF boundary)\n");
console.log("Candidates come from per-word LIKE (standing in for mask('q')). `fetch` = materializing their five text columns; `lanes` = decode + lowercase (+ NFD for non-ASCII rows); `exact` = whole-word check; `next` = next_words for the phrase.\n");
console.log("| phrase | filter | candidates | text MB | fetch ms | lanes ms | exact ms | next ms | top suggestions |\n|---|---|---|---|---|---|---|---|---|");
const FACET = `"Best Available Region (additional data)" = 'North East'`;
for (const phrase of ["mental health", "youth sport", "climate change", "community", "support"]) {
  for (const facet of [null, FACET]) {
    const words = tokenizeStr(phrase);
    const where = [...words.map((w) => anyCol(`%${w}%`)), ...(facet ? [facet] : [])].join(" and ");
    const tf = performance.now(); const r = sel(where); const fetchMs = performance.now() - tf - median(() => count(where), 3, 0);
    const n = r.rowCount;
    const mb = r.columns.reduce((a, c) => a + c.bytes.length, 0) / 1e6;
    const tl = performance.now(); const lanes = r.columns.map((c) => lane(c, n)); const laneMs = performance.now() - tl;
    const te = performance.now(); exactRows(lanes, words.map((w) => [w]), n); const exMs = performance.now() - te;
    const tn = performance.now(); const top = nextWords(lanes, phrase); const nxMs = performance.now() - tn;
    console.log(`| \`${phrase}\` | ${facet ? "North East" : "none"} | ${n.toLocaleString()} | ${mb.toFixed(1)} | ${f1(fetchMs)} | ${f1(laneMs)} | ${f1(exMs)} | ${f1(nxMs)} | ${top.slice(0, 4).map(([k, v]) => `${k} ${v}`).join("; ")} |`);
  }
}
