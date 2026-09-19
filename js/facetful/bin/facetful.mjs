#!/usr/bin/env node
// The facetful command, on the same wasm engine the browser runs (design.sv
// d51): one `npm install facetful` gives the library and this.
//
//   facetful convert in.csv out.facetful [--row-group-size N]
//   facetful query file.facetful ["select …"] [--table name=other.facetful …] [--udf module.mjs …]
//   facetful materialize in.facetful "select …" out.facetful [--table name=path …] [--udf …]
//
// Conversion streams: two passes over the CSV in 1 MB chunks, row groups
// written as they finish, memory bounded whatever the file size. Tables open
// lazily through positional reads (the browser's OPFS import, here fs.readSync).
// The ready-made functions (../udfs.js) are registered; a --udf module's
// default export adds more: an array of { name, signature, fn }.
import { readFileSync, openSync, readSync, writeSync, closeSync, fstatSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { instantiate, QueryError } from "../core.js";
import { udfs as builtinUdfs } from "../udfs.js";

const args = process.argv.slice(2);
const cmd = args.shift();
const usage = () => {
  console.error(`usage: facetful convert in.csv out.facetful [--row-group-size N]
       facetful query file.facetful ["select …"] [--table name=path.facetful …] [--udf module.mjs …]
       facetful materialize in.facetful "select …" out.facetful [--table name=path …] [--udf module.mjs …]`);
  process.exit(2);
};

// positional reads for lazily opened tables: fileId -> fd
const fds = new Map();
let nextId = 1;
const opfsRead = (fileId, offset, dest) => {
  const fd = fds.get(fileId);
  if (fd === undefined) return -1;
  return readSync(fd, dest, 0, dest.length, offset);
};
const engine = await instantiate(readFileSync(new URL("../facetful_wasm.wasm", import.meta.url)), opfsRead);
for (const u of builtinUdfs) engine.registerFunction(u.name, u.signature, u.fn);

function openLazy(path) {
  const fd = openSync(path, "r");
  const id = nextId++;
  fds.set(id, fd);
  return engine.openOpfsTable(id, fstatSync(fd).size, 256 << 20);
}

async function registerUdfs(paths) {
  for (const p of paths) {
    const mod = await import(pathToFileURL(p).href);
    const list = Array.isArray(mod.default) ? mod.default : Object.values(mod.default ?? mod);
    for (const u of list) engine.registerFunction(u.name, u.signature, u.fn);
  }
}

/** Split flags out of a positional list: --table name=path, --udf path, --row-group-size N. */
function parse(argv) {
  const pos = [], tables = [], udfs = [];
  let groupSize = 65536;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--table") tables.push(argv[++i]);
    else if (a === "--udf") udfs.push(argv[++i]);
    else if (a === "--row-group-size") groupSize = Number(argv[++i]);
    else if (a.startsWith("--")) usage();
    else pos.push(a);
  }
  return { pos, tables, udfs, groupSize };
}

const text = (c, i) => engine.dec.decode(c.bytes.subarray(c.offsets[i], c.offsets[i + 1]));
const isoDate = (d) => new Date(d * 86400000).toISOString().slice(0, 10);
const isoTs = (ms) => new Date(ms).toISOString().replace("T", " ").slice(0, 19);
function cell(c, i) {
  if (!((c.validity[i >> 3] >> (i & 7)) & 1)) return "";
  switch (c.kind) {
    case "text": return text(c, i);
    case "date": return isoDate(c.values[i]);
    case "timestamp": return isoTs(c.values[i]);
    case "bool": return c.values[i] ? "1" : "0";
    default: return String(c.values[i]);
  }
}
function printResult(r, ms) {
  const rows = [];
  for (let i = 0; i < r.rowCount; i++) rows.push(r.columns.map((c) => cell(c, i)));
  const widths = r.columns.map((c, j) => Math.max(c.name.length, ...rows.map((row) => row[j].length)));
  const line = (cells) => cells.map((s, j) => s.padEnd(widths[j])).join("  ");
  console.log(line(r.columns.map((c) => c.name)));
  console.log(widths.map((w) => "-".repeat(w)).join("  "));
  for (const row of rows) console.log(line(row));
  console.log(`(${r.rowCount} row${r.rowCount === 1 ? "" : "s"}, ${ms.toFixed(1)} ms)`);
}

function convert(input, output, groupSize) {
  const CHUNK = 1 << 20;
  const h = engine.w.convert_begin(groupSize);
  const fail = () => {
    const n = engine.w.convert_error(h, engine.scratch, 4096);
    console.error(engine.dec.decode(new Uint8Array(engine.mem(), engine.scratch, n)));
    process.exit(1);
  };
  const out = openSync(output, "w");
  let written = 0;
  const drain = () => {
    const n = engine.w.convert_output_len(h);
    if (!n) return;
    const p = engine.w.alloc(n);
    engine.w.convert_output_copy(h, p);
    writeSync(out, new Uint8Array(engine.mem(), p, n));
    engine.w.dealloc(p, n);
    written += n;
  };
  const feedFile = () => {
    const fd = openSync(input, "r");
    const buf = engine.w.alloc(CHUNK);
    for (;;) {
      // the view is rebuilt per read: memory may grow while the converter runs
      const n = readSync(fd, new Uint8Array(engine.mem(), buf, CHUNK), 0, CHUNK, null);
      if (n === 0) break;
      if (engine.w.convert_feed(h, buf, n) < 0) fail();
      drain();
    }
    engine.w.dealloc(buf, CHUNK);
    closeSync(fd);
  };
  const t0 = performance.now();
  feedFile();
  if (engine.w.convert_pass2(h) < 0) fail();
  const n = engine.w.convert_schema(h, engine.scratch, 4096);
  const schema = engine.dec.decode(new Uint8Array(engine.mem(), engine.scratch, n)).trimEnd();
  feedFile();
  if (engine.w.convert_finish(h) < 0) fail();
  drain();
  closeSync(out);
  const rows = engine.w.convert_rows(h);
  engine.w.convert_free(h);
  console.error(`${input}: ${rows} rows`);
  for (const l of schema.split("\n")) console.error("  " + l.replace("\t", ": "));
  console.error(`${output}: ${written} bytes (${Math.ceil(rows / groupSize)} row groups, ${(performance.now() - t0).toFixed(0)} ms)`);
}

try {
  if (cmd === "convert") {
    const { pos, groupSize } = parse(args);
    if (pos.length !== 2) usage();
    convert(pos[0], pos[1], groupSize);
  } else if (cmd === "query" || cmd === "materialize") {
    const { pos, tables, udfs } = parse(args);
    if (pos.length < 1) usage();
    await registerUdfs(udfs);
    const { handle } = openLazy(pos[0]);
    for (const spec of tables) {
      const eq = spec.indexOf("=");
      if (eq < 0) usage();
      engine.catalogRegister(spec.slice(0, eq), openLazy(spec.slice(eq + 1)).handle);
    }
    if (cmd === "materialize") {
      if (pos.length !== 3) usage();
      const t0 = performance.now();
      const img = engine.materialize(handle, pos[1]);
      const bytes = engine.imageBytes(img);
      writeSync(openSync(pos[2], "w"), bytes);
      const { rows } = engine.openImage(img);
      console.error(`${pos[2]}: ${rows} rows, ${bytes.byteLength} bytes (${(performance.now() - t0).toFixed(1)} ms)`);
    } else if (pos.length >= 2) {
      const t0 = performance.now();
      const r = engine.query(handle, pos[1]);
      printResult(r, performance.now() - t0);
    } else {
      // REPL: one statement per line
      const rl = (await import("node:readline")).createInterface({ input: process.stdin, output: process.stdout, prompt: "facetful> " });
      rl.prompt();
      rl.on("line", (line) => {
        const sql = line.trim();
        if (sql) {
          try {
            const t0 = performance.now();
            printResult(engine.query(handle, sql), performance.now() - t0);
          } catch (e) {
            console.error(e instanceof QueryError ? e.message : e);
          }
        }
        rl.prompt();
      });
      rl.on("close", () => process.exit(0));
    }
  } else usage();
} catch (e) {
  console.error(e instanceof QueryError ? e.message : e);
  process.exit(1);
}
