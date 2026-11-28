# facetful

A tiny columnar SQL engine for browser faceting. Rust compiled to 250–390 KB gz
of WebAssembly (three builds, below), nothing else to load in the browser,
running in a dedicated worker.

Built for one job and fast at it: **exploring 100K–5M row datasets in the
browser** — facet counts, pivots, top-k, filters — at interaction speed.

- **Ship a prepared image.** `facetful convert` builds a `.facetful` file ahead
  of time; the page fetches it from any static host and opens it as it
  arrives, with no decoding or conversion in the browser. Measured on a real
  183K×52 dataset: facet queries 1–4 ms.
- **Parquet when you need it.** `openParquet` converts in the browser on the
  first visit and caches the compiled image in OPFS for later visits.
- **SELECT-only SQL**, SQLite-flavored semantics, verified cell-for-cell
  against SQLite by a differential test suite. Friendly errors with carets and
  did-you-mean hints.
- **Larger-than-memory**: tables can live in OPFS and load column segments
  lazily through a byte-budgeted LRU cache.
- **Filter-mask cache**: WHERE conjuncts are cached as per-row-group bitmaps,
  so facet bursts sharing a filter evaluate it once, and a `LIKE '%needle%'`
  extending a cached needle verifies only the rows the shorter one matched.
- **Joins, CTEs and subqueries as cached materializations**: `JOIN`, `WITH`,
  `FROM (select …)`, `IN (select …)`, `EXISTS` all build a derived table once
  and serve every later facet from it — no per-query join cost.
- Dates, medians, stddev, group_concat, `select *`, LIKE fast paths — the
  boring things work.

## Quickstart

One install gives the browser library **and** the `facetful` command:

```
npm install facetful
npx facetful convert data.csv data.facetful        # CSV streams: any file size, ~20 MB of memory
npx facetful convert data.parquet data.facetful    # Parquet too
npx facetful query data.facetful "select country, count(*) as n from t group by country order by n desc limit 5"
npx facetful inspect data.facetful      # rows, row groups, sort keys; per column kind, bytes, nulls, range or dictionary size
```

The command runs the same wasm engine the browser does (Node is the second
runtime; its build, `facetful_cli.wasm`, adds the publishing commands, which
the build a page loads leaves out), so a `.facetful` built here is exactly what `openParquet` would have
built, and SQL — including your registered functions, via `--udf module.mjs` —
behaves identically in both places. CSV can also be opened directly in the
browser with `loadCsv` (below). The ready-made functions (`json_extract`,
`to_tz`, `date_trunc`, …, see "User-defined functions") are registered by
default in both.

```js
import { Facetful } from "facetful";

const db = await Facetful.open();
await db.loadUrl("plants", "data.facetful"); // opens as it downloads, no conversion

const r = await db.query(`
  select "Country/area", count(*) as n, round(sum("Capacity (MW)"), 1) as mw
  from t
  where "Status" = 'operating'
  group by "Country/area"
  order by mw desc limit 15
`);

for (const row of r.rows()) console.log(row);
r.columnRaw("mw"); // Float64Array + validity bitmap, near-zero copy (charts)

// big results: text as codes + dictionary, not a string per row — `true` for the image's
// dictionary columns (free), "all" to also encode other text columns where that pays (a hash pass)
const big = await db.query("select country, mw from t", { dictText: true });
big.columnRaw("country"); // { codes: Uint16Array, dict: { offsets, bytes }, validity }
big.dictionary("country"); // the decoded distinct values, indexed by code (decodes them all)
big.dictValue("country", big.columnRaw("country").codes[0]); // one value, decoded on first use — for big dictionaries
// a facet refresh in one call: the statements share masks and lanes and run fused (4-5x faster than
// one by one); send a latency-critical statement such as a grid page alone, just before the batch
const [byCountry, byStatus, totals] = await db.query([
  "select country, count(*) as n from t where status = 'operating' group by country order by n desc limit 20",
  "select status, count(*) as n from t where country = 'Germany' group by status",
  "select count(*) as n, sum(mw) as mw from t where country = 'Germany' and status = 'operating'",
]);
await db.describe(); // the table's catalog: rows, groups, per-column kind / bytes / nulls / min-max / dictionary size
await db.memoryStats(); // { wasmBytes, tables }: the worker's memory high-water, from the page
console.log(r.elapsedMs, r.stats); // ms in worker, row groups pruned
```

The SQL table is always named `t`. Multiple tables coexist:
`db.query(sql, { table: "plants" })`.

## Big results in a grid: `prepare`

`query()` copies the whole result to the page. For a grid that shows a few
hundred rows of a result of millions, keep the result in the worker and ask
for blocks:

```js
const h = await db.prepare(`select * from t where status = 'operating'`, { dictText: true });
h.rows;                                  // row count; h.columns: [{ name, kind }]
const page = await h.page(1024, 1536);   // rows 1024..1535 as a Result (columnRaw, rows(), …)
await h.sort("mw desc, country");        // the text after ORDER BY; "" for none
await h.page(0, 512, { columns: ["country", "mw"] });
await h.close();
```

For a plain SELECT over the table the worker keeps 4 bytes a row (nothing for
an unfiltered, unsorted one) and gathers each page from the table, so a
sorted block of 512 rows of every column takes 2 to 4 ms on a 1.5M-row
table, and a sort reads only its key columns. `search()` and `search_rank()`
work. Rows with equal sort keys stay in table order. Grouped and joined
results are kept whole in the worker and sliced. `h.generation` counts
sorts, and each page carries the value it was asked under, so a late page
from before a sort can be dropped. Close a handle
when its grid goes: 5 may be open at once (`maxHandles` at `open`), and
`prepare` rejects beyond that, naming the open statements. It is in every
build, lite included. Design and measurements: `docs/result-handles.sv`.

## Three builds: full, sql, lite

The package carries three engines, and a page downloads only the one it
opens:

```js
const db = await Facetful.open({ build: "lite" }); // "full" (the default), "sql" or "lite"
db.build; // "lite"
```

| build | gzipped | has |
|---|---|---|
| `full` | about 390 KB | everything |
| `sql` | about 300 KB | all but `search()`, `search_rank()` and the word index |
| `lite` | about 250 KB | single-table queries over images, result handles, functions you register; no joins, CTEs, subqueries, `materialize`, Parquet or CSV |

Calling what a build leaves out is an error naming the build that has it,
such as `search() is not in this build (sql): use the full build`. Bundlers
copy all three files, since each is written as its own `new URL(...)`; pass
`wasmUrl` instead to serve one from elsewhere.

## The ways in

| method | use when |
|---|---|
| `loadUrl(name, url)` | the default: a `.facetful` image built ahead of time by `facetful convert`, plain or packed (`facetful pack`), fetched from a static host |
| `attach(url, {table})` | the second file of a split image (`facetful split`): its columns join a table already loaded |
| `load(name, buf)` | you already hold a `.facetful` image as an ArrayBuffer |
| `openParquet(name, buf)` | the source is Parquet at runtime: converted in the browser on the first visit, OPFS-cached after |
| `loadCsv(name, fileOrBuffer, {persist})` | a CSV: streamed through the converter in the worker, types inferred, optionally persisted |
| `loadOpfs(name, path, {cacheBytes})` | the file is in OPFS; open lazily, spill-over for big data |

Persistence is always explicit (`storeOpfs`), never write-behind. OPFS needs a
secure context (https or localhost); everything degrades to memory-only
without one.

## Publishing an image: brotli, or gzip

The download is most of the wait, so serve images compressed. `publish`
packs the image first (see "Packed images" below), then compresses it. GEM
units, 183K rows, a 48 MB image:

| | brotli | gzip |
|---|---|---|
| as it is | 6.5 MB | 8.9 MB |
| packed (what `publish` writes) | 5.4 MB | 6.6 MB |

```
npx facetful publish data.facetful out/    # packs, then writes out/data.facetful.br and out/data.facetful.gz
npx facetful check-url https://…/data.facetful --origin https://your.site
```

`publish` packs, compresses (brotli at its best setting: 40 s for GEM units)
and prints the upload commands for the hosts below. A page on an engine from
before packed images can't read them; `--no-pack` writes copies of the image
as it is. `check-url` also says whether what arrives is packed. `check-url` downloads
the image as a browser would and says what the host does: compression, bytes
on the wire, whether a whole image arrives, `Content-Type`, `Cache-Control`
and CORS for `--origin`. It exits non-zero when something needs fixing.

| host | upload | load with |
|---|---|---|
| S3, DigitalOcean Spaces (or anything that keeps per-object headers) | the `.br` under the image's own name, with `Content-Encoding: br` (`aws s3 cp … --content-encoding br`; for Spaces add `--endpoint-url https://REGION.digitaloceanspaces.com`) | `loadUrl(name, ".../data.facetful")`: the browser decompresses as it streams |
| GitHub Pages (no custom headers) | the `.gz` | `loadUrl(name, ".../data.facetful.gz")`: a gzip file is decompressed in the page as it streams |

Notes:
- Browsers only ask for brotli over https, so serve the brotli copy over https.
- GitHub Pages refuses files over 100 MB.
- Long `max-age` with `immutable` is right only when the file's name changes
  whenever the data changes.
- A page on another origin needs CORS on the bucket.
- In-page decompression uses `DecompressionStream`, which every current
  browser has for gzip. Chrome has no in-page brotli, which is why the
  fallback is gzip.

## The `facetful` command

```
facetful convert in.csv|in.parquet out.facetful [--row-group-size N]
facetful inspect file.facetful [--json]
facetful query file.facetful ["select …"] [--table name=other.facetful …] [--udf module.mjs …]
facetful materialize in.facetful "select …" out.facetful [--table …] [--udf …]
facetful pack in.facetful out.facetful
facetful unpack in.facetful out.facetful
facetful split in.facetful first.facetful second.facetful --columns a,b
facetful sizes file.facetful [--gzip] [--quality 0-11]
facetful order file.facetful [--gzip] [--quality 0-11] [--depth N] [--confirm]
facetful publish file.facetful [out-dir] [--no-pack]
facetful check-url https://…/file.facetful [--origin https://your.site]
```

`convert` streams: two passes over the CSV (types and dictionaries, then
encoding), row groups written as they finish, memory bounded by the distinct
values plus one row group — a 200 MB / 3M-row CSV converts in ~4 s at ~70 MB
of process memory (Node included). Types: int (narrowed), float, ISO date and
datetime, text; repeated text is dictionary-encoded. `query` without SQL is a
REPL; tables open lazily, so large files don't load into memory. The
ready-made functions are registered; a `--udf` module's default export adds
your own, as an array of `{ name, signature, fn }`.

## A smaller, sooner download: `pack`, `split`, `sizes`, `order`

What a page waits for on a first visit is the download. Four commands cut
it, and none of them changes the SQL or the results.

**`facetful pack`** writes the image in a packed form that compresses
better. Serve it with brotli or gzip like any other file.

| after compression | as it is | packed |
|---|---|---|
| GEM units (183,125 rows, 52 columns), brotli 11 | 6.83 MB | 5.63 MB |
| the same, gzip 9 | 9.35 MB | 6.96 MB |
| grantnav (1.52M rows, long text), brotli 11 | 58.0 MB | 46.9 MB |
| the same, gzip 9 | 83.0 MB | 58.2 MB |

```js
await db.loadUrl("plants", "data.packed.facetful", { persist: "plants.facetful" });
```

- `loadUrl`, `attach` and `load` take either form and tell them apart from
  the first bytes. Nothing in the page changes.
- The engine unpacks each row group as it arrives into exactly the bytes of
  the plain image, so the table and a search index built while loading are
  ready as before.
- `persist` writes the unpacked image to OPFS. A repeat visit opens it with
  `loadOpfs` in a few milliseconds and never unpacks.
- Unpacking costs processor time, hidden behind the download on any
  ordinary connection. In Chromium on a laptop, grantnav was ready in 21.0 s
  packed against 27.3 s plain at 20 Mbit/s, and in 2.41 s against 2.32 s
  with no network at all.
- A packed image is format version 2. An engine older than the one that
  introduced it refuses the file with "unsupported format version 2", so
  publish packed images only with a matching `facetful` package.
- `facetful unpack` gives the original image back, byte for byte. `inspect`,
  `query` and `materialize` read a packed file directly.

**`facetful split`** writes an image as two files with the same rows. The
columns you name go to the second file; the first keeps the rest.

```
facetful split units.facetful first.facetful second.facetful \
  --columns "GEM.Wiki URL,Owner(s),Latitude,Longitude"
```

```js
await db.loadUrl("units", "first.facetful");   // filters, totals and charts answer from here
await db.attach("second.facetful", { table: "units" }); // the table's other columns arrive
```

This is the pattern for a dashboard. A table cannot answer anything until
its file has fully arrived, so keep the first file to the columns the
filters, totals and charts read, and put what only the result table shows
in the second. For GEM units, a first file with five filter columns,
capacity and start year is 0.4 MB compressed where the whole image is 5.6 MB
or more. A query that names a column not yet attached fails with a clear
error; after `attach` the SQL is unchanged, one table. Both parts can be
packed, and both take `persist` (`attachOpfs` on later visits).

**`facetful sizes`** prints the download size as it is and packed, then each
column's share, largest first. A column the page never reads is cheapest
left out altogether: `facetful materialize`, naming the columns to keep.

**`facetful order`** tries sort keys and reports the row order that makes
the packed download smallest, with the `materialize` command that writes
it. It is a greedy search over a few columns, not a proof. On grantnav it
found an order 4.8% smaller. Row order is also the order rows come back in
without `ORDER BY`, and what lets a filter on the sort key skip row groups,
so size is not the only thing to choose it by. `sizes` and `order` compress
with Node's zlib at brotli window 24 or gzip 9; `--quality` trades accuracy
for time (default 11 for `sizes`, 5 for `order`).

The measurements and the packed layout are in `docs/file-size.sv` in the
repository.

## Derived tables: `materialize`

```js
// a grouped result becomes a table of its own — queryable like any other,
// with the same typed columns, nulls and (here) sort metadata
await db.materialize("fuel_state", `
  select fuel, state, count(*) as n, round(sum(net_generation_mwh)/1e6, 3) as twh
  from t group by fuel, state order by twh desc`);
const r = await db.query("select fuel, sum(twh) as twh from t group by fuel", { table: "fuel_state" });

// persist the image to OPFS so a later visit can loadOpfs() it instead
await db.materialize("fuel_state", sql, { persist: "facetful/fuel_state.facetful" });
```

Pre-aggregate a large detail table once per session and run the facets
against the rollup; every SELECT item needs a distinct name. Row order is the
query's output order, so a materialized `ORDER BY` is recorded as the table's
sort. The CLI has the same verb: `facetful materialize in.facetful "select …" out.facetful`.

## Joins, CTEs, subqueries

Every table the worker holds — loaded, opened from Parquet, or materialized —
is registered under its name, and any query can name it:

```js
await db.materialize("plants", "select plant_id, state, count(*) as n_gens from t group by plant_id, state");
await db.query(`
  select t.fuel, p.state, sum(t.mwh) as mwh
  from t left join plants p on t.plant_id = p.plant_id
  where p.n_gens >= 3 group by t.fuel, p.state order by mwh desc`);
```

`[INNER|LEFT] JOIN … ON a.k = b.k [AND …]` and `USING (k)`; the right side's
join key must be unique (a dimension). `WITH name AS (…)`, `FROM (select …) s`,
`x IN (select …)`, `(a, b) IN (select …)`, `EXISTS (select … where d.k = t.k)`.
All of these are **materializations, not query-time operators**: the first
query builds the joined or derived table (milliseconds for a fact table
against a dimension), and it lives in a per-table cache (64 MB default) that
every later query with the same shape reuses — a facet burst over a join pays
for the join once. Correlated subqueries beyond `inner.k = outer.k` equalities,
`FULL`/`RIGHT` joins and non-unique right keys are refused with a clear error.
`NOT IN` follows SQL's NULL rule (a NULL in the set makes it select nothing);
`NOT EXISTS` doesn't, and is usually what you mean.

## User-defined functions

```js
// vectorized: called once per lane (a row group, a group table, or — for a
// dictionary column — the dictionary itself); `out.values` is the result lane
await db.registerFunction("regexp", { params: ["text", "text"], returns: "bool" }, (() => {
  const cache = new Map();
  return (args, len, out) => {
    const [s, pattern] = args;                 // pattern is a literal: broadcast, one value
    let re = cache.get(pattern.values[0]);
    if (!re) cache.set(pattern.values[0], (re = new RegExp(pattern.values[0])));
    for (let i = 0; i < len; i++) out.values[i] = re.test(s.values[i]) ? 1 : 0;
  };
})());
await db.query("select fuel, count(*) from t where regexp(plant_name, '^(Big|Little) ') group by fuel");

// per row: simpler, ~10x slower on large lanes
await db.registerFunction("mw_to_gw", { params: ["float"], returns: "float", perRow: true }, (mw) => mw / 1000);
```

**Ready-made functions**, registered by default (`Facetful.open({ udfs: false })`
opts out; the list is importable from `facetful/udfs`): `regexp(s, pattern[, flags])`,
`regexp_extract(s, pattern[, group])`, `regexp_replace(s, pattern, replacement)`,
`json_extract(doc, '$.a.b[0]')`, `to_tz(ts, 'Europe/London')`
(Intl's time-zone tables — hundreds of KB the wasm never has to carry),
`date_trunc('month', ts)`, `date_add(d, 1, 'month')`, `weekday`, `quarter`,
`country_name('DE')`, `format_number(x, 'en-US:compact')`, `unaccent('Zürich')`,
`url_host(url)`, `geo_distance(lat1, lon1, lat2, lon2)` (metres, haversine). Each is a few lines of ordinary JavaScript over what the
browser already ships; they're as much a set of patterns as a library.

Kinds: `int`, `float`, `bool`, `text`, `date`, `timestamp` (dates and timestamps
arrive as days / ms numbers). Functions bind like built-ins — wrong argument
types are caret-diagnosed, the declared return type is the column's type — and
their results go through the same caches, so a `regexp()` filter is evaluated
once per pattern and served from the mask cache after. `strict` (default)
gives NULL out for NULL in without calling you; `optional: n` makes the last
`n` parameters omittable, a parameter kind of `"any"` accepts every type, and
`variadic` repeats the last parameter; `unregisterFunction(name)` removes one. The function runs in the
worker: pass a self-contained function (its source is sent — an IIFE for
state, as above — no closures over your variables) or `{ moduleUrl }`. A
throwing function fails the query with its message. `regexp()` is the
browser's `RegExp` here and the `regex` crate in the native CLI — the same SQL
on both sides.

## Parquet support

Reading uses [hyparquet](https://github.com/hyparam/hyparquet) (~20 KB gz),
installed with the package so that `facetful convert` reads Parquet out of the
box. The browser never loads it unless a Parquet method is called: a page
that loads prepared images ships the engine and nothing else. Under a bundler
the dynamic import resolves by itself; without one, pass `hyparquetUrl` to
`Facetful.open`.

Compression: SNAPPY, ZSTD or none. ZSTD uses a vendored copy of
[fzstd](https://github.com/101arrowz/fzstd) (~3 KB gz), loaded only for a file
that needs it. ZSTD at a high level (DuckDB: `COMPRESSION ZSTD,
COMPRESSION_LEVEL 19`) makes a file a little over half the size of Snappy, so
the download is shorter, and costs little to decode. Other codecs fail with a
clear error.

The same reader serves `openParquet` in the browser and `facetful convert
in.parquet` on the command line. BOOLEAN/INT32/INT64 → integers (narrowed),
FLOAT/DOUBLE → float64, DECIMAL → integer (scale 0) or float64, strings →
dictionary-encoded when it pays, DATE/TIMESTAMP (INT96 included) → real
date/timestamp columns (days / ms since epoch, ISO strings on output).
Everything else becomes text: lists, structs and maps as JSON, JSON columns
as written, UUIDs in their usual form, fixed-length binary and geometry as
hex. Integers beyond 2^53 and decimals beyond 15 digits lose precision.

## SQL dialect, briefly

SELECT-only, SQLite semantics (3-valued logic, null-skipping aggregates,
truncating integer division, NULL-first ascending sorts). Idioms: `IN`,
`BETWEEN`, `IS [NOT] NULL`, `[NOT] LIKE` (case-insensitive for every
alphabet, as with SQLite's ICU build: `'%SÃO%'` finds "São"; accents are not
folded, so `'%sao%'` does not; `ILIKE` is accepted as the same operator),
`CASE WHEN`, `CAST`, `COUNT(DISTINCT x)`, `||`, `select *`,
`JOIN`/`WITH`/subqueries and user-defined functions as above. Aggregates: count, sum, avg, min, max,
count(distinct), median, stddev, group_concat. Scalars: math (abs, round,
floor, ceil, sqrt, pow, exp, ln, sign), text (lower, upper, length, substr,
trim/ltrim/rtrim, replace, instr, concat), null handling (coalesce, ifnull,
nullif), temporal (year, month, day, hour, minute, second, date, timestamp,
strftime). `GROUP BY` and `ORDER BY` accept select aliases or 1-based
positions. Numbers may use exponents (`1e6`). Quote column names with spaces:
`"Capacity (MW)"`.

## Building the wasm from source

The engine lives in the same repository (Rust workspace, zero dependencies).
`scripts/build-package.sh` builds the three wasm engines and the command's
own (with wasm-opt when available), copies them next to this package, and
runs `npm pack`.

## License

MIT
