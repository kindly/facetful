// Shared bits for the fts-bench spike: engine loading, the grantnav search
// columns, and the planned tokenizer (Unicode lowercase, NFD, drop combining
// marks, split on non-alphanumerics).
import { readFileSync } from "node:fs";

export const ROOT = new URL("../../", import.meta.url).pathname;
export const IMAGE = process.env.IMAGE || `${process.env.HOME}/projects/gratnav/data/site/grantnav.facetful`;
export const COLS = ["Title", "Recipient Org:Name", "Grant Programme:Title", "Description Common", "Description Rare"];
export const OUT = new URL("./out/", import.meta.url).pathname;

export async function openEngine() {
  const { instantiate } = await import(`${ROOT}js/facetful/core.js`);
  const engine = await instantiate(readFileSync(`${ROOT}js/facetful/facetful_wasm.wasm`));
  const { handle } = engine.openTable(readFileSync(IMAGE));
  return { engine, handle };
}

export const ident = (c) => `"${c.replace(/"/g, '""')}"`;
export const lit = (s) => `'${s.replace(/'/g, "''")}'`;

const dec = new TextDecoder();
const NON_ALNUM = /[^\p{L}\p{N}]+/u;
const MARKS = /\p{M}+/gu;

/** Tokens of one UTF-8 field, pushed into `out`. ASCII takes a byte loop. */
export function tokenize(bytes, start, end, out) {
  let ascii = true;
  for (let i = start; i < end; i++) if (bytes[i] >= 128) { ascii = false; break; }
  if (!ascii) {
    const s = dec.decode(bytes.subarray(start, end)).normalize("NFD").replace(MARKS, "").toLowerCase();
    for (const t of s.split(NON_ALNUM)) if (t) out.push(t);
    return false;
  }
  let w = -1;
  let buf = "";
  for (let i = start; i <= end; i++) {
    const b = i < end ? bytes[i] : 32;
    const alnum = (b >= 48 && b <= 57) || (b >= 65 && b <= 90) || (b >= 97 && b <= 122);
    if (alnum) { if (w < 0) w = i; }
    else if (w >= 0) {
      buf = "";
      for (let j = w; j < i; j++) { const c = bytes[j]; buf += String.fromCharCode(c >= 65 && c <= 90 ? c + 32 : c); }
      out.push(buf);
      w = -1;
    }
  }
  return true;
}

/** A string's tokens (queries). */
export function tokenizeStr(s) {
  return s.normalize("NFD").replace(MARKS, "").toLowerCase().split(NON_ALNUM).filter(Boolean);
}

export const median = (f, n = 7, warm = 2) => {
  for (let i = 0; i < warm; i++) f();
  const ts = [];
  for (let i = 0; i < n; i++) { const s = performance.now(); f(); ts.push(performance.now() - s); }
  return ts.sort((a, b) => a - b)[n >> 1];
};
