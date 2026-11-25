#!/usr/bin/env node
// The facetful command, on the same wasm engine the browser runs (design.sv
// d51): one `npm install facetful` gives the library and this. It loads
// facetful_cli.wasm, the full build with the publishing commands added
// (pack, unpack, split, sizes, order), which pages have no use for.
//
//   facetful convert in.csv|in.parquet out.facetful [--row-group-size N]
//   facetful inspect file.facetful [--json]
//   facetful query file.facetful ["select …"] [--table name=other.facetful …] [--udf module.mjs …]
//   facetful materialize in.facetful "select …" out.facetful [--table name=path …] [--udf …]
//   facetful pack|unpack in.facetful out.facetful
//   facetful split in.facetful first.facetful second.facetful --columns a,b
//   facetful sizes file.facetful [--gzip] [--quality N]
//   facetful order file.facetful [--gzip] [--quality N] [--depth N] [--confirm]
//
// CSV conversion streams: two passes over the CSV in 1 MB chunks, row groups
// written as they finish, memory bounded whatever the file size. Parquet is
// read whole through hyparquet (../parquet.js, the browser's own path), so it
// needs memory for the decoded table. Tables open
// lazily through positional reads (the browser's OPFS import, here fs.readSync).
// The ready-made functions (../udfs.js) are registered; a --udf module's
// default export adds more: an array of { name, signature, fn }.
import { readFileSync, openSync, readSync, writeSync, closeSync, fstatSync, statSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { instantiate, QueryError } from "../core.js";
import { udfs as builtinUdfs } from "../udfs.js";

const args = process.argv.slice(2);
const cmd = args.shift();
const usage = () => {
  console.error(`usage: facetful convert in.csv|in.parquet out.facetful [--row-group-size N]
       facetful inspect file.facetful [--json]
       facetful query file.facetful ["select …"] [--table name=path.facetful …] [--udf module.mjs …]
       facetful materialize in.facetful "select …" out.facetful [--table name=path …] [--udf module.mjs …]
       facetful pack in.facetful out.facetful      the packed form: smaller to download
       facetful unpack in.facetful out.facetful
       facetful split in.facetful first.facetful second.facetful --columns a,b
       facetful sizes file.facetful [--gzip] [--quality 0-11] [--jobs N]
       facetful order file.facetful [--gzip] [--quality 0-11] [--depth N] [--jobs N] [--confirm]`);
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
const engine = await instantiate(readFileSync(new URL("../facetful_cli.wasm", import.meta.url)), opfsRead);
for (const u of builtinUdfs) engine.registerFunction(u.name, u.signature, u.fn);

const packedFiles = new Set();
function openLazy(path) {
  const fd = openSync(path, "r");
  // a packed image (format version 2) is unpacked into memory: its bytes
  // are not the ones a table reads
  const head = Buffer.alloc(10);
  if (readSync(fd, head, 0, 10, 0) === 10 && head.toString("latin1", 0, 4) === "FCT1" && head.readUInt16LE(8) === 2) {
    closeSync(fd);
    packedFiles.add(path);
    return engine.openImage(engine.imageFrom(readFileSync(path)));
  }
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
  let groupSize = 65536, json = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--table") tables.push(argv[++i]);
    else if (a === "--udf") udfs.push(argv[++i]);
    else if (a === "--row-group-size") groupSize = Number(argv[++i]);
    else if (a === "--json") json = true;
    else if (a.startsWith("--")) usage();
    else pos.push(a);
  }
  return { pos, tables, udfs, groupSize, json };
}

const human = (n) => n >= 1 << 30 ? `${(n / (1 << 30)).toFixed(2)} GB` : n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : n >= 1024 ? `${(n / 1024).toFixed(0)} KB` : `${n} B`;

/** `facetful inspect`: the image's catalog, as a table or as JSON. */
function inspect(path, json) {
  const { handle } = openLazy(path);
  const info = engine.describe(handle);
  if (json) return console.log(JSON.stringify(info, null, 2));
  const size = statSync(path).size;
  console.log(`${path}: ${human(size)}${packedFiles.has(path) ? " packed" : ""}, format v${info.version}, ${info.rows} rows, ${info.groups} row group${info.groups === 1 ? "" : "s"} (target ${info.target})`);
  if (info.sortedBy.length) console.log(`sorted by: ${info.sortedBy.map((k) => k.column + (k.descending ? " desc" : "")).join(", ")}`);
  const range = (c) => {
    if (c.dict !== undefined) return `${c.dict} values`;
    if (c.min === undefined) return "";
    const f = c.kind === "date" ? isoDate : c.kind === "timestamp" ? isoTs : String;
    return `${f(c.min)} … ${f(c.max)}`;
  };
  const rows = info.columns.map((c) => [c.name, c.kind, human(c.bytes), c.nulls ? String(c.nulls) : "", range(c), c.clustered ? "clustered" : ""]);
  const head = ["column", "kind", "bytes", "nulls", "range / dictionary", "layout"];
  const widths = head.map((h, j) => Math.max(h.length, ...rows.map((r) => r[j].length)));
  const line = (cells) => cells.map((s, j) => j === 2 ? s.padStart(widths[j]) : s.padEnd(widths[j])).join("  ");
  console.log(line(head));
  console.log(widths.map((w) => "-".repeat(w)).join("  "));
  for (const r of rows) console.log(line(r));
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

/** Parquet files start with the magic "PAR1", whatever they are called. */
function isParquet(path) {
  const fd = openSync(path, "r");
  const magic = Buffer.alloc(4);
  const n = readSync(fd, magic, 0, 4, 0);
  closeSync(fd);
  return n === 4 && magic.toString("latin1") === "PAR1";
}

async function convertParquet(input, output, groupSize) {
  let hyparquet;
  try {
    hyparquet = await import("hyparquet");
  } catch {
    console.error("converting Parquet needs the hyparquet package, which installs with facetful: run npm install");
    process.exit(1);
  }
  const { parquetToColumns, zstdCompressors } = await import("../parquet.js");
  const t0 = performance.now();
  const file = readFileSync(input);
  const { rows, columns } = await parquetToColumns(
    hyparquet,
    file.buffer.slice(file.byteOffset, file.byteOffset + file.byteLength),
    { compressorsFor: async (codecs) => (codecs.has("ZSTD") ? zstdCompressors(await import("../fzstd.js")) : {}) },
  );
  const bytes = engine.imageBytes(engine.compileTable(rows, columns, { groupTarget: groupSize }));
  const out = openSync(output, "w");
  writeSync(out, bytes);
  closeSync(out);
  console.error(`${input}: ${rows} rows`);
  for (const c of columns) {
    console.error(`  ${c.name}: ${c.kind === "text" ? "text" : c.temporal ?? (c.isInt ? "int" : "float")}`);
  }
  console.error(`${output}: ${bytes.byteLength} bytes (${Math.ceil(rows / groupSize)} row groups, ${(performance.now() - t0).toFixed(0)} ms)`);
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
  // the header went out first: its image-length field is filled in now
  const len = new Uint8Array(8);
  new DataView(len.buffer).setBigUint64(0, BigInt(written), true);
  writeSync(out, len, 0, 8, engine.w.convert_len_offset(h));
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
    if (isParquet(pos[0])) await convertParquet(pos[0], pos[1], groupSize);
    else convert(pos[0], pos[1], groupSize);
  } else if (cmd === "inspect") {
    const { pos, json } = parse(args);
    if (pos.length !== 1) usage();
    inspect(pos[0], json);
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
  } else if (cmd === "pack" || cmd === "unpack") {
    (await import("./size.mjs")).pack(engine, args, cmd === "pack");
  } else if (cmd === "split") {
    (await import("./size.mjs")).split(engine, args);
  } else if (cmd === "sizes") {
    await (await import("./size.mjs")).sizes(engine, args);
  } else if (cmd === "order") {
    await (await import("./size.mjs")).order(engine, openLazy, args);
  } else usage();
} catch (e) {
  console.error(e instanceof QueryError ? e.message : e);
  process.exit(1);
}
