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
//   facetful publish file.facetful [out-dir] [--no-pack]
//   facetful check-url https://…/file.facetful [--origin https://your.site]
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
/** A problem to report in one line, not as a stack. */
class CliError extends Error {}
const usage = () => {
  console.error(`usage: facetful convert in.csv|in.parquet out.facetful [--row-group-size N]
       facetful inspect file.facetful [--json]
       facetful query file.facetful ["select …"] [--table name=path.facetful …] [--udf module.mjs …]
       facetful materialize in.facetful "select …" out.facetful [--table name=path …] [--udf module.mjs …]
       facetful pack in.facetful out.facetful      the packed form: smaller to download
       facetful unpack in.facetful out.facetful
       facetful split in.facetful first.facetful second.facetful --columns a,b
       facetful sizes file.facetful [--gzip] [--quality 0-11] [--jobs N]
       facetful order file.facetful [--gzip] [--quality 0-11] [--depth N] [--jobs N] [--confirm]
       facetful publish file.facetful [out-dir] [--no-pack]
       facetful check-url https://…/file.facetful [--origin https://your.site]`);
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

/** `facetful publish`: the copies of an image to put on a static host:
 *  `.br` (brotli 11, 16 MB window) for hosts that can send it with
 *  `Content-Encoding: br` (S3, DigitalOcean Spaces), `.gz` for hosts that
 *  can't set headers (GitHub Pages), which `loadUrl` decompresses in the page.
 *  Both are of the packed form (`facetful pack`), which compresses about a
 *  fifth smaller and which `loadUrl` unpacks as it arrives; `--no-pack`
 *  compresses the image as it is (for pages on an engine before 0.8). The
 *  packed image is held in memory; compression streams. */
async function publish(input, outDir, noPack) {
  const { basename, join, resolve } = await import("node:path");
  const { mkdirSync, statSync, createReadStream, createWriteStream } = await import("node:fs");
  const { Readable } = await import("node:stream");
  const { pipeline } = await import("node:stream/promises");
  const zlib = await import("node:zlib");
  openLazy(input); // refuses anything that is not an image
  const onDisk = statSync(input).size;
  let packed = null;
  if (packedFiles.has(input)) {
    console.error(`${input} is already packed`);
  } else if (!noPack) {
    const t0 = performance.now();
    const img = engine.imageFrom(readFileSync(input));
    try {
      packed = engine.packImage(img);
    } finally {
      engine.freeImage(img);
    }
    console.error(`packed: ${human(packed.byteLength)} from ${human(onDisk)} (${((performance.now() - t0) / 1000).toFixed(1)} s)`);
  }
  const raw = packed ? packed.byteLength : onDisk;
  const dir = outDir ?? resolve(input, "..");
  mkdirSync(dir, { recursive: true });
  const name = basename(input);
  const C = zlib.constants;
  const out = {};
  for (const [ext, make] of [
    ["br", () => zlib.createBrotliCompress({ params: { [C.BROTLI_PARAM_QUALITY]: 11, [C.BROTLI_PARAM_LGWIN]: 24, [C.BROTLI_PARAM_SIZE_HINT]: raw } })],
    ["gz", () => zlib.createGzip({ level: 9 })],
  ]) {
    const path = join(dir, `${name}.${ext}`);
    const t0 = performance.now();
    let done = 0, shown = -1;
    const from = packed
      ? Readable.from((function* () { for (let i = 0; i < packed.byteLength; i += 1 << 20) yield packed.subarray(i, i + (1 << 20)); })())
      : createReadStream(input, { highWaterMark: 1 << 20 });
    from.on("data", (c) => {
      done += c.length;
      const pct = Math.floor((done / raw) * 100);
      if (process.stderr.isTTY && pct !== shown) process.stderr.write(`\r${ext}: ${(shown = pct)}%`);
    });
    await pipeline(from, make(), createWriteStream(path));
    out[ext] = { path, bytes: statSync(path).size };
    if (process.stderr.isTTY) process.stderr.write("\r");
    console.error(`${path}: ${human(out[ext].bytes)} (${((out[ext].bytes / onDisk) * 100).toFixed(1)}% of the ${human(onDisk)} image, ${((performance.now() - t0) / 1000).toFixed(1)} s)`);
  }
  console.error(`${packed || packedFiles.has(input) ? `
Both are packed: the page needs facetful 0.8 or later (an older one says
"unsupported format version 2"). --no-pack makes copies any version reads.
` : ""}
Where to put which (check the result with: facetful check-url <url>):

  S3 (or anything S3-like): upload the .br under the image's own name, saying
  it is brotli; browsers undo that themselves as it streams. Needs https.
    aws s3 cp ${out.br.path} s3://BUCKET/PATH/${name} \\
      --content-encoding br --content-type application/octet-stream \\
      --cache-control "public, max-age=3600"

  DigitalOcean Spaces: the same, against the Spaces endpoint.
    aws s3 cp ${out.br.path} s3://SPACE/PATH/${name} \\
      --endpoint-url https://REGION.digitaloceanspaces.com --acl public-read \\
      --content-encoding br --content-type application/octet-stream \\
      --cache-control "public, max-age=3600"

  GitHub Pages: headers can't be set there, so commit the .gz and load that
  URL; loadUrl decompresses a gzip file in the page as it streams.
    db.loadUrl("t", "${name}.gz")
  Pages refuses files over 100 MB.

  "immutable" with a long max-age is right only when the file's name changes
  whenever the data does. A page on another origin needs CORS on the bucket.`);
}

/** `facetful check-url`: fetch an image the way a browser would and say
 *  what the host does: compression, bytes on the wire, headers, CORS, and
 *  whether what arrives is a whole image. */
async function checkUrl(url, origin) {
  const zlib = await import("node:zlib");
  const u = new URL(url);
  const get = (await import(u.protocol === "https:" ? "node:https" : "node:http")).get;
  const t0 = performance.now();
  const res = await new Promise((ok, fail) => {
    const headers = { "accept-encoding": "br, gzip, deflate", ...(origin ? { origin } : {}) };
    get(u, { headers }, ok).on("error", (e) => fail(new CliError(`${url}: can't connect (${e.code ?? e.errors?.[0]?.code ?? e.message})` +
      ((process.env.HTTPS_PROXY || process.env.https_proxy) && !process.env.NODE_USE_ENV_PROXY ? "; behind a proxy, run with NODE_USE_ENV_PROXY=1" : ""))));
  });
  const h = res.headers;
  if (res.statusCode !== 200) {
    res.resume();
    throw new CliError(`${url}: HTTP ${res.statusCode}`);
  }
  const enc = (h["content-encoding"] ?? "").trim().toLowerCase();
  const decoders = { br: () => zlib.createBrotliDecompress(), gzip: () => zlib.createGunzip(), deflate: () => zlib.createInflate() };
  if (enc && !decoders[enc]) throw new CliError(`${url}: Content-Encoding ${enc}, which browsers don't decode`);
  // what fetch hands the page (the host's encoding undone), then a gzip
  // file undone as loadUrl would
  let wire = 0, head = Buffer.alloc(0), tail = Buffer.alloc(0), image = 0, fileGzip = null;
  const stream = enc ? res.pipe(decoders[enc]()) : res;
  res.on("data", (c) => (wire += c.length));
  let inner = null;
  const take = (c) => {
    if (image < 10) head = Buffer.concat([head, c]).subarray(0, 10);
    image += c.length;
    tail = Buffer.concat([tail, c]).subarray(-4);
  };
  let broken = null;
  await new Promise((ok) => {
    const fail = (e) => ((broken = e), ok());
    stream.on("error", fail);
    stream.on("data", (c) => {
      if (fileGzip === null) {
        fileGzip = c[0] === 0x1f && c[1] === 0x8b;
        if (fileGzip) {
          inner = zlib.createGunzip();
          inner.on("data", take);
          inner.on("error", fail);
          inner.on("end", ok);
        }
      }
      if (fileGzip) inner.write(c);
      else take(c);
    });
    stream.on("end", () => (fileGzip ? inner.end() : ok()));
  });
  const secs = (performance.now() - t0) / 1000;
  const whole = head.toString("latin1", 0, 4) === "FCT1" && tail.toString("latin1") === "FCT1";
  // the format version: 2 is the packed form
  const packed = head.length === 10 && head.readUInt16LE(8) === 2;
  const pct = (a, b) => `${((a / b) * 100).toFixed(1)}%`;
  const lines = [];
  const say = (ok, s) => lines.push(`${ok === true ? "ok  " : ok === false ? "FIX " : "    "}${s}`);
  say(null, `${url}`);
  say(null, `${human(wire)} on the wire for a ${human(image)} ${packed ? "packed " : ""}image (${pct(wire, image)}), ${secs.toFixed(1)} s`);
  if (enc === "br") say(true, "Content-Encoding: br: browsers decompress it as it streams (the smallest)");
  else if (enc === "gzip" || enc === "deflate") say(null, `Content-Encoding: ${enc}: works, but brotli is usually 10-30% smaller (facetful publish)`);
  else if (fileGzip) say(true, "a gzip file: loadUrl decompresses it in the page as it streams (the GitHub Pages way)");
  else say(false, "not compressed: run facetful publish and serve the .br (S3, Spaces) or the .gz (GitHub Pages)");
  if (enc === "br" && u.protocol !== "https:") say(false, "brotli over plain http: browsers only ask for brotli over https");
  if (broken) say(false, `it doesn't decompress (${broken.code ?? broken.message}): cut short, or not really ${fileGzip ? "gzip" : enc}`);
  else if (!whole) say(false, `what arrives is not a whole facetful image${fileGzip ? " after gunzip" : ""}: wrong file, or cut short`);
  else say(true, `a whole facetful image arrives${packed ? ", packed (the page needs facetful 0.8 or later)" : ""}`);
  if (whole && !broken && !packed) say(null, "not packed: facetful publish packs it first, about a fifth less to download");
  say(null, `Content-Type: ${h["content-type"] ?? "(none)"}`);
  say(null, `Cache-Control: ${h["cache-control"] ?? "(none: browsers guess)"}`);
  const acao = h["access-control-allow-origin"];
  if (origin) say(acao === "*" || acao === origin, `CORS for ${origin}: ${acao ? `Access-Control-Allow-Origin: ${acao}` : "no Access-Control-Allow-Origin (a page there can't fetch it)"}`);
  else say(null, `CORS: ${acao ? `Access-Control-Allow-Origin: ${acao}` : "none sent (fine from the same origin; pass --origin https://your.site to check another)"}`);
  console.log(lines.join("\n"));
  if (lines.some((l) => l.startsWith("FIX"))) process.exitCode = 1;
}

try {
  if (cmd === "publish") {
    const i = args.indexOf("--no-pack");
    if (i >= 0) args.splice(i, 1);
    const { pos } = parse(args);
    if (pos.length < 1 || pos.length > 2) usage();
    await publish(pos[0], pos[1], i >= 0);
  } else if (cmd === "check-url") {
    const i = args.indexOf("--origin");
    const origin = i >= 0 ? args.splice(i, 2)[1] : undefined;
    if (args.length !== 1) usage();
    await checkUrl(args[0], origin);
  } else if (cmd === "convert") {
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
  console.error(e instanceof QueryError || e instanceof CliError ? e.message : e);
  process.exit(1);
}
