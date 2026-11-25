// What an image costs to download, and what would make it cost less: the
// facetful command's pack, unpack, split, sizes and order (docs/file-size.sv).
//
// Size is what a host sends: the packed image after brotli (window 24, the
// `brotli` command's -w 24) or gzip 9, compressed here by Node's zlib.
import { readFileSync, writeFileSync, statSync } from "node:fs";
import { availableParallelism } from "node:os";
import { promisify } from "node:util";
import zlib from "node:zlib";

const brotli = promisify(zlib.brotliCompress);
const gzip = promisify(zlib.gzip);

const mb = (n) => (n >= 100_000 ? `${(n / 1e6).toFixed(2)} MB` : `${(n / 1e3).toFixed(1)} KB`);
const quote = (name) => `"${name.replaceAll('"', '""')}"`;
/** How much bigger `a` is than `b`, as "+1.2" or "-0.4". */
const pct = (a, b) => {
  const s = (100 * a / b - 100).toFixed(1);
  return s.startsWith("-") ? (Number(s) === 0 ? "0.0" : s) : `+${s}`;
};

function parse(argv, usage, quality) {
  const a = { path: "", gzip: false, quality, jobs: availableParallelism(), depth: 3, confirm: false };
  const fail = () => {
    console.error(`usage: ${usage}`);
    process.exit(2);
  };
  const num = (v) => (Number.isInteger(Number(v)) && Number(v) > 0 ? Number(v) : fail());
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--gzip") a.gzip = true;
    else if (arg === "--quality") a.quality = Math.min(num(argv[++i]), 11);
    else if (arg === "--jobs") a.jobs = num(argv[++i]);
    else if (arg === "--depth") a.depth = num(argv[++i]);
    else if (arg === "--confirm") a.confirm = true;
    else if (!a.path && !arg.startsWith("--")) a.path = arg;
    else fail();
  }
  if (!a.path) fail();
  // zlib compresses on libuv's pool, which is 4 threads unless told before
  // its first use
  process.env.UV_THREADPOOL_SIZE ??= String(Math.min(a.jobs, 64));
  return a;
}

const codecName = (a) => (a.gzip ? "gzip 9" : `brotli ${a.quality}`);

/** Bytes of `data` compressed. */
async function size(data, a) {
  const out = a.gzip
    ? await gzip(data, { level: 9 })
    : await brotli(data, {
        params: {
          [zlib.constants.BROTLI_PARAM_QUALITY]: a.quality,
          [zlib.constants.BROTLI_PARAM_LGWIN]: 24,
          [zlib.constants.BROTLI_PARAM_SIZE_HINT]: data.byteLength,
        },
      });
  return out.byteLength;
}

/** `make(i)` for i in 0..n, at most `jobs` of the promises it returns
 *  unsettled at once; results in order. */
async function pool(n, jobs, make) {
  const out = new Array(n);
  let next = 0;
  const worker = async () => {
    while (next < n) {
      const i = next++;
      out[i] = await make(i);
    }
  };
  await Promise.all(Array.from({ length: Math.max(1, Math.min(jobs, n)) }, worker));
  return out;
}

/**
 * `facetful pack in out`: the image in its packed form, which is smaller to
 * download (before and after brotli or gzip) and which every loader unpacks
 * into the same image as it arrives. `facetful unpack in out` gives the image
 * back, byte for byte. Either takes either form.
 */
export function pack(engine, argv, packing) {
  if (argv.length !== 2 || argv.some((a) => a.startsWith("--"))) {
    console.error(`usage: facetful ${packing ? "pack" : "unpack"} in.facetful out.facetful`);
    process.exit(2);
  }
  const [input, output] = argv;
  const onDisk = statSync(input).size;
  const img = engine.imageFrom(readFileSync(input));
  const out = packing ? engine.packImage(img) : engine.imageBytes(img);
  if (!packing) engine.openImage(img); // not an image: say so here, not when it is next opened
  writeFileSync(output, out);
  console.error(`${output}: ${out.byteLength} bytes (${packing ? "packed" : "unpacked"} from ${onDisk})`);
}

/**
 * `facetful split in first second --columns a,b`: the image as two files a
 * page loads one after the other (`loadUrl`, then `attach`) and queries as
 * one table. The columns named go to the second; the first is what the page
 * needs before it can answer. Either part can then be packed.
 */
export function split(engine, argv) {
  const pos = [];
  let columns;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--columns") columns = argv[++i];
    else pos.push(argv[i]);
  }
  if (pos.length !== 3 || !columns || pos.some((a) => a.startsWith("--"))) {
    console.error("usage: facetful split in.facetful first.facetful second.facetful --columns a,b");
    process.exit(2);
  }
  const parts = engine.splitImage(engine.imageFrom(readFileSync(pos[0])), columns.split(",").map((c) => c.trim()));
  parts.forEach((bytes, i) => {
    writeFileSync(pos[i + 1], bytes);
    console.error(`${pos[i + 1]}: ${bytes.byteLength} bytes`);
  });
}

/**
 * `facetful sizes`: the image's download size as it is and packed, and each
 * column's share of the packed one, largest first.
 */
export async function sizes(engine, argv) {
  const a = parse(argv, "facetful sizes file.facetful [--gzip] [--quality 0-11] [--jobs N]", 11);
  const img = engine.imageFrom(readFileSync(a.path));
  const image = engine.imageBytes(img);
  const packed = engine.packImage(img);
  const shares = engine.packShares(img);
  const info = engine.describe(engine.openImage(img).handle);
  console.error(`compressing with ${codecName(a)} (the whole image twice, and each column) …`);
  // the two whole images first: they take longest
  const all = [image, packed, ...shares];
  const sized = await pool(all.length, a.jobs, (i) => size(all[i], a));
  const [asIs, whole] = sized;
  console.log(`${a.path}: ${info.rows} rows, ${shares.length} columns`);
  console.log(`  as it is   ${mb(image.byteLength).padStart(10)}   ${mb(asIs).padStart(10)} after ${codecName(a)}`);
  console.log(`  packed     ${mb(packed.byteLength).padStart(10)}   ${mb(whole).padStart(10)} after ${codecName(a)}  (${(100 - 100 * whole / asIs).toFixed(1)}% less to download)`);
  console.log();
  const rows = shares.map((s, ci) => ({ ci, share: s.byteLength, down: sized[ci + 2] })).sort((x, y) => y.down - x.down);
  const sum = rows.reduce((t, r) => t + r.down, 0);
  const width = Math.min(40, Math.max(6, ...info.columns.map((c) => [...c.name].length)));
  const cells = (name, kind, stored, share, down) => `  ${name.padEnd(width)}  ${kind.padEnd(21)}  ${stored.padStart(10)}  ${share.padStart(10)}  ${down.padStart(10)}`;
  console.log(`${cells("column", "kind", "in image", "packed", "download")}  share`);
  for (const r of rows) {
    const c = info.columns[r.ci];
    const name = [...c.name].slice(0, width).join("");
    console.log(`${cells(name, c.kind, mb(c.bytes + (c.dictBytes ?? 0)), mb(r.share), mb(r.down))}  ${(100 * r.down / sum).toFixed(1).padStart(4)}%`);
  }
  console.log();
  console.log(`  The columns sum to ${mb(sum)}; compressed together they come to ${mb(whole)}.`);
  console.log("  A column the page never reads is cheapest left out: facetful materialize, naming the columns to keep.");
}

/**
 * `facetful order`: tries sort keys one after another, keeping the one that
 * makes the packed, compressed image smallest, then a second key under it,
 * and so on. A search, not a proof: the best order of all rows is out of
 * reach, and this finds a good one among sorts by a few columns.
 */
export async function order(engine, openTable, argv) {
  const a = parse(argv, "facetful order file.facetful [--gzip] [--quality 0-11] [--depth N] [--jobs N] [--confirm]", 5);
  const { handle } = openTable(a.path);
  const info = engine.describe(handle);
  // keys worth trying: columns whose values repeat or run in order. Floats
  // and free text rarely make a useful first key and are many.
  const candidates = info.columns.filter((c) => c.dict !== undefined || !/^(utf8|float)/.test(c.kind)).map((c) => c.name);
  // The rows in the order `keys` give, packed. Sorting and packing happen
  // here, one at a time; only the compressing runs alongside.
  const ordered = (keys) => {
    const sql = `select * from t${keys.length ? ` order by ${keys.map(quote).join(", ")}` : ""}`;
    const img = engine.materialize(handle, sql, { groupTarget: info.target });
    try {
      return engine.packImage(img);
    } finally {
      engine.freeImage(img);
    }
  };
  // each try waiting to be compressed is a packed image in memory
  const jobs = Math.min(a.jobs, 4);
  const run = (keys, codec = a) => size(ordered(keys), codec);
  console.error(`${a.path}: ${info.rows} rows; trying ${candidates.length} columns as sort keys, ${a.depth} deep, with ${codecName(a)}`);
  const base = await run([]);
  console.log(`  ${mb(base).padStart(10)}          as it is`);
  const tried = [];
  const keys = [];
  let best = base;
  for (let level = 0; level < a.depth; level++) {
    const next = candidates.filter((c) => !keys.includes(c));
    if (!next.length) break;
    const sized = await pool(next.length, jobs, (i) => run([...keys, next[i]]));
    next.forEach((c, i) => tried.push({ size: sized[i], keys: [...keys, c] }));
    const i = sized.indexOf(Math.min(...sized));
    // a key has to earn its place: half a percent
    if (sized[i] > best * 0.995) {
      console.error(`  key ${level + 1}: nothing gains half a percent, stopping`);
      break;
    }
    keys.push(next[i]);
    best = sized[i];
    console.error(`  key ${level + 1}: ${next[i]} (${mb(best)})`);
  }
  tried.sort((x, y) => x.size - y.size);
  for (const t of tried.slice(0, 12)) {
    console.log(`  ${mb(t.size).padStart(10)}  ${pct(t.size, base).padStart(5)}%  order by ${t.keys.join(", ")}`);
  }
  console.log();
  if (!keys.length) {
    console.log("  No order tried is half a percent smaller than the one the image has. Leave it.");
    return;
  }
  const by = keys.map(quote).join(", ");
  if (a.confirm && !a.gzip && a.quality < 11) {
    const full = { gzip: false, quality: 11 };
    const [plain, sorted] = await pool(2, 2, (i) => run(i ? keys : [], full));
    console.log(`  At brotli 11: ${mb(plain)} as it is, ${mb(sorted)} sorted (${pct(sorted, plain)}%).`);
  }
  console.log(`  Smallest found: order by ${by}`);
  console.log(`  To write it:    facetful materialize ${a.path} 'select * from t order by ${by}' sorted.facetful`);
  console.log("  Row order is also the order rows come back in without ORDER BY, the order a page");
  console.log("  sees them while the image loads, and what lets a filter on the sort key skip row");
  console.log("  groups. Smaller is not the only thing to choose it by.");
}
