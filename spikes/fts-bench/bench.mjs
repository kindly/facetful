// Measurement 6 (LIKE on 0.7.1) and the no-index alternative: spell-check each
// query word against the data's own vocabulary, then LIKE over the corrected
// words. Needs out/vocab.tsv from vocab.mjs.
//   node spikes/fts-bench/bench.mjs
import { readFileSync } from "node:fs";
import { openEngine, COLS, ident, lit, tokenize, tokenizeStr, median, OUT } from "./common.mjs";

const { engine, handle } = await openEngine();
const cold = () => engine.setMaskBudget(handle, 0);
const warm = () => engine.setMaskBudget(handle, 16 << 20);
const count = (where) => engine.query(handle, `select count(*) as n from t where ${where}`).columns[0].values[0];
const anyCol = (pat) => `(${COLS.map((c) => `${ident(c)} like ${lit(pat)}`).join(" or ")})`;
const f1 = (x) => x.toFixed(1);

// --- 6. LIKE baseline -----------------------------------------------------------
console.log("## 6. LIKE on facetful 0.7.1, grantnav, mask cache off (median of 7)\n");
cold();
console.log("per column, `climate`:\n\n| column | ms | rows |\n|---|---|---|");
for (const c of COLS) {
  const w = `${ident(c)} like '%climate%'`;
  console.log(`| ${c} | ${f1(median(() => count(w)))} | ${count(w).toLocaleString()} |`);
}
console.log("\nwhole queries:\n\n| query | one substring ms | rows | per-term AND ms | rows |\n|---|---|---|---|---|");
for (const q of ["climate", "mental health", "youth sport london"]) {
  const one = anyCol(`%${q}%`), per = q.split(" ").map((t) => anyCol(`%${t}%`)).join(" and ");
  console.log(`| \`${q}\` | ${f1(median(() => count(one)))} | ${count(one).toLocaleString()} | ${f1(median(() => count(per)))} | ${count(per).toLocaleString()} |`);
}
// typing, per-term, mask cache on (fresh cache per sequence)
console.log("\ntyping per-term with the mask cache on (ms per keystroke):\n");
for (const q of ["mental health", "youth sport london"]) {
  warm(); engine.setMaskBudget(handle, 0); warm(); // clear, then enable
  const ts = [];
  for (let i = 1; i <= q.length; i++) {
    const typed = q.slice(0, i), terms = typed.split(" ").filter(Boolean);
    const w = terms.map((t) => anyCol(`%${t}%`)).join(" and ");
    const s = performance.now(); count(w); ts.push(`${typed.endsWith(" ") ? "␣" : typed.at(-1)} ${f1(performance.now() - s)}`);
  }
  console.log(`- \`${q}\`: ${ts.join(", ")}`);
}

// --- vocabulary spell check -------------------------------------------------------
const t0 = performance.now();
const vocabText = readFileSync(`${OUT}vocab.tsv`, "utf8").split("\n");
const vocab = new Map(); // term -> rows
const byLen = []; // length -> [terms]
for (let i = 1; i < vocabText.length; i++) {
  const line = vocabText[i]; if (!line) continue;
  const tab = line.indexOf("\t"), t = line.slice(0, tab), n = parseInt(line.slice(tab + 1), 10);
  vocab.set(t, n);
  (byLen[t.length] ||= []).push(t);
}
const loadMs = performance.now() - t0;

/** Optimal-string-alignment distance, or k+1 once it must exceed k. */
const rowA = new Int32Array(64), rowB = new Int32Array(64), rowC = new Int32Array(64);
function osa(a, b, k) {
  const n = a.length, m = b.length;
  if (Math.abs(n - m) > k) return k + 1;
  let pp = rowA, p = rowB, c = rowC;
  for (let j = 0; j <= m; j++) p[j] = j;
  for (let i = 1; i <= n; i++) {
    c[0] = i; let best = i;
    const ai = a.charCodeAt(i - 1);
    for (let j = 1; j <= m; j++) {
      const bj = b.charCodeAt(j - 1);
      let v = Math.min(p[j] + 1, c[j - 1] + 1, p[j - 1] + (ai === bj ? 0 : 1));
      if (i > 1 && j > 1 && ai === b.charCodeAt(j - 2) && a.charCodeAt(i - 2) === bj) v = Math.min(v, pp[j - 2] + 1);
      c[j] = v; if (v < best) best = v;
    }
    if (best > k) return k + 1;
    const t = pp; pp = p; p = c; c = t;
  }
  return p[m];
}
const maxEdits = (w) => (w.length < 4 ? 0 : w.length < 8 ? 1 : 2);
const CAP = 5, RATIO = 50;
/** The accepted forms of one finished query word: [{term, rows, d}] */
function correct(w) {
  const own = vocab.get(w) || 0;
  const k = maxEdits(w);
  const cands = [];
  if (k > 0) for (let L = w.length - k; L <= w.length + k; L++) for (const t of byLen[L] || []) {
    if (t === w) continue;
    const d = osa(w, t, k);
    if (d <= k) cands.push({ term: t, rows: vocab.get(t), d });
  }
  cands.sort((a, b) => a.d - b.d || b.rows - a.rows);
  const forms = own ? [{ term: w, rows: own, d: 0 }] : [];
  const expand = !own || cands.some((c) => c.rows >= RATIO * own);
  if (expand) {
    // the far more common neighbours when the word exists; else the most frequent candidates
    const pool = own ? cands.filter((c) => c.rows >= RATIO * own) : cands;
    forms.push(...[...pool].sort((a, b) => b.rows - a.rows).slice(0, CAP));
  }
  return { forms, expanded: expand, scanned: cands.length };
}

console.log(`\n## Spell check against the vocabulary\n\nvocabulary: ${vocab.size.toLocaleString()} terms, loaded and bucketed in ${f1(loadMs)} ms. Brute-force OSA distance over terms within ±k length; k = 0 under 4 chars, 1 for 4-7, 2 for 8+. Expand when the word is missing or a neighbour has ≥${RATIO}× its rows; keep the ${CAP} most frequent.\n`);
console.log("| word | own rows | forms kept | ms |\n|---|---|---|---|");
const QUERIES = ["climate", "mental health", "youth sport london", "community project", "organisation support", "charities",
  "enviromental helth", "mental helth servics", "yuoth sprot londn", "climte chnage", "orgnisation suport", "comunity projcet", "chairty", "heath", "organization"];
const corrections = new Map();
for (const w of new Set(QUERIES.flatMap(tokenizeStr))) {
  const ms = median(() => correct(w), 5, 1);
  const r = correct(w); corrections.set(w, r);
  console.log(`| ${w} | ${(vocab.get(w) || 0).toLocaleString()} | ${r.forms.map((f) => `${f.term} ${f.rows.toLocaleString()}${f.d ? ` (d${f.d})` : ""}`).join(", ") || "none"} | ${f1(ms)} |`);
}

// --- LIKE over the corrected words --------------------------------------------------
console.log("\n## LIKE over corrected words vs exact word matching\n");
console.log("Each word becomes an OR of `col like '%form%'` over its forms and the five columns; words are ANDed. Mask cache off. `exact` re-checks the LIKE candidates by tokenizing their text in JS and requiring every word to appear as a whole token in one of its forms.\n");
console.log("| query | LIKE ms | LIKE rows | exact rows | fetch candidates' text ms | JS token check ms |\n|---|---|---|---|---|---|");
cold();
for (const q of QUERIES) {
  const words = tokenizeStr(q);
  const formSets = words.map((w) => corrections.get(w).forms.map((f) => f.term));
  if (formSets.some((s) => !s.length)) { console.log(`| \`${q}\` | no forms | | | | |`); continue; }
  const where = formSets.map((fs) => `(${fs.map((f) => anyCol(`%${f}%`)).join(" or ")})`).join(" and ");
  const ms = median(() => count(where), 5, 1);
  const n = count(where);
  const tf = performance.now();
  const r = engine.query(handle, `select ${COLS.map(ident).join(", ")} from t where ${where}`);
  const fetchMs = performance.now() - tf;
  const sets = formSets.map((fs) => new Set(fs));
  const tc = performance.now();
  let exact = 0;
  const toks = [];
  for (let i = 0; i < r.rowCount; i++) {
    const seen = new Array(sets.length).fill(false);
    for (const c of r.columns) {
      if (c.validity && !((c.validity[i >> 3] >> (i & 7)) & 1)) continue;
      toks.length = 0; tokenize(c.bytes, c.offsets[i], c.offsets[i + 1], toks);
      for (const t of toks) for (let k = 0; k < sets.length; k++) if (!seen[k] && sets[k].has(t)) seen[k] = true;
    }
    if (seen.every(Boolean)) exact++;
  }
  const checkMs = performance.now() - tc;
  console.log(`| \`${q}\` → ${formSets.map((s) => s.join("\\|")).join(" ")} | ${f1(ms)} | ${n.toLocaleString()} | ${exact.toLocaleString()} | ${f1(fetchMs)} | ${f1(checkMs)} |`);
}
