# facetful

A read-only, columnar SQL engine for the browser, written in Rust and
compiled to WebAssembly. Queries run locally in a dedicated worker over
data files that can be served from a static host, without a database server.

The focus is faceted exploration: filtering a dataset, counting categories,
building pivots and selecting top results. The storage format and executor
are designed around repeated queries over immutable datasets.

## How

- **Columnar format**: `.facetful` files are compiled images — the on-disk
  segment layout *is* the in-memory execution layout. Opening a table reads a
  footer; column segments load lazily. Dictionary-encoded strings execute as
  integer scans (predicates like `LIKE`/`IN`/`=` evaluate once per distinct
  value, not once per row).
- **Filter-mask cache**: each WHERE conjunct's row bitmap is cached per row
  group (LRU, 16 MB). A burst of facet queries sharing a filter evaluates it
  once while the mask remains cached. A `LIKE '%needle%'` that extends a
  cached needle verifies only the rows the shorter one matched.
- **Prepared images**: `facetful convert` builds the image ahead of time, at
  build or publish time. The page fetches it with `loadUrl()` and opens it as
  it arrives, with no decoding or conversion in the browser. This is the
  primary path.
- **Packed images**: `facetful pack` rewrites an image in a form that
  compresses better, about 20% less to download after brotli and 25 to 30%
  after gzip. `loadUrl()` unpacks it a row group at a time as it arrives,
  into the same bytes the plain image has, so queries, OPFS and repeat
  visits are unchanged. `facetful sizes` shows what each column costs to
  download and `facetful order` looks for a row order that compresses
  better.
- **Split images**: `facetful split` writes an image as two files with the
  same rows. The page loads the first (the columns its filters and totals
  need) and can answer, then `attach()`es the second; SQL sees one table.
- **Parquet input**: when the source can only be Parquet at runtime,
  `openParquet()` transcodes in the browser (hyparquet + the same Rust
  compiler the CLI uses), caches the image in OPFS keyed by content hash, and
  reuses that image on subsequent opens while it remains cached. The first
  visit pays for the conversion.
- **OPFS storage**: tables can live in OPFS and load segments through
  a byte-budgeted LRU cache, limiting how much source data stays in memory.
- **SELECT-only SQL** with SQLite-compatible semantics for the supported
  query subset, checked by a differential test suite. Includes caret
  diagnostics, did-you-mean hints, dates, `median`/`stddev`/`group_concat`,
  `select *` and SIMD substring search.
- **Safe Rust** in the executor (`unsafe` only at the wasm FFI edge);
  the wasm SIMD128 kernels use the safe intrinsics.

## Quickstart

Convert the data once, ahead of time, and host the image next to the page:

```
npm install facetful
npx facetful convert plants.csv plants.facetful   # CSV or Parquet in
npx facetful query plants.facetful                # REPL; or one-shot with a SQL arg
npx facetful publish plants.facetful out/         # packed, brotli and gzip copies, where to upload each
```

Serve the brotli copy with `Content-Encoding: br` (S3, DigitalOcean Spaces),
or the gzip copy where headers can't be set (GitHub Pages): see "Publishing an
image" in [`js/facetful`](js/facetful/README.md#publishing-an-image-brotli-or-gzip).

Then load it in the browser:

```js
import { Facetful } from "facetful";

const db = await Facetful.open();
await db.loadUrl("plants", "plants.facetful"); // opens as it downloads, no conversion

const r = await db.query(`
  select "Country", count(*) as n, round(sum("Capacity (MW)"), 1) as mw
  from t
  where "Status" = 'operating'
  group by "Country" order by mw desc limit 15
`);
for (const row of r.rows()) console.log(row);
r.columnRaw("mw"); // Float64Array + validity bitmap, near-zero copy (charts)
```

The npm package lives in [`js/facetful`](js/facetful) — see its README for
the full API (OPFS persistence, `warm()`, cache budgets, transferable column
buffers).

`convert` reads CSV (streamed, bounded memory, types inferred) and Parquet.
Parquet integers, floats, decimals,
booleans, dates, timestamps and strings keep their type; anything else
(lists, structs, maps, JSON, UUIDs, binary) becomes text.

If the page only has a Parquet file at runtime, `openParquet(name, buffer)`
converts it in the browser and caches the result in OPFS. The first visit is
slower than loading a prepared image, so prefer `facetful convert` when the
data is known ahead of time.

## The `facetful` command

The command is the wasm engine under Node, so images, SQL and user-defined
functions are identical wherever they run. It loads `facetful_cli.wasm`: the
module the browser loads, built with the publishing commands added (the
`publish` feature), so that pages do not download code only a publisher
runs. CSV conversion streams (`facetful-format::stream`): two passes over the
input, row groups written as they finish, bounded memory whatever the file
size, byte-identical to what the browser's own `loadCsv`/`openParquet` build.

Besides `convert`, `query`, `materialize` and `inspect` it has the
publishing commands `pack`, `unpack`, `split`, `sizes` and `order`
(`js/facetful/README.md`; measurements in `docs/file-size.sv`).

The Rust CLI (`crates/facetful-cli`) is a **development tool**, not a user
path: benchmarks, the SQLite differential, `inspect`, native profiling. It
knows no user-defined functions — those live in JavaScript by design
(`docs/design.sv` d51–d53).

```
cargo build --release -p facetful-cli
facetful query data.facetful --bench qs.sql # 3 warmups + 10 runs, medians
facetful inspect data.facetful
```

## Workspace

| crate / dir | what |
|---|---|
| `crates/facetful-format` | `.facetful` read/write, the shared baseline compiler, calendar/time |
| `crates/facetful-engine` | vectorized executor, SQL front-end, text-search kernels |
| `crates/facetful-wasm` | hand-rolled `extern "C"` boundary (no wasm-bindgen), OPFS source |
| `crates/facetful-cli` | native convert / inspect / query / bench |
| `js/facetful` | the npm package: worker, main-thread API, TypeScript types |
| `web/demo` | SQL box + Parquet/OPFS demo panels |
| `web/spike-bench` | the browser benchmark harness (vs hand-written JS, crossfilter, hyparquet, DuckDB-WASM) |
| `docs/*.sv` | the living design doc and build logs — the full history of every decision and measurement |

## Building and testing

```
cargo test --workspace          # engine, format, SQL, differential vs sqlite3 (if on PATH)
./scripts/size-check.sh         # wasm build + wasm-opt, enforces the size budgets (gzipped: full 500 KB, sql 350 KB, lite 300 KB)
./scripts/build-package.sh      # the release gate: budget + smokes + differentials + npm pack
node js/facetful/node-smoke.mjs # JS boundary protocol, compile round-trip
```

Correctness rests on two differential suites: the same SQL run through
facetful and SQLite must agree cell-for-cell, and a Parquet file pushed
through the browser transcoder must answer identically to the CLI-built
image.

## Performance testing

The [benchmark suite](bench/queries.sql) covers filters, grouping, sorting,
text search and projection. The CLI's `--bench` mode reports cold and warm
WHERE-mask timings: cold runs clear the filter cache before each query;
warm runs reuse cached masks. Data and dictionaries stay resident in both
modes, so these measurements do not include initial loading.

Performance varies with query shape, data distribution, result size and
runtime. The [benchmark script](scripts/bench.sh) compares runs against a
local baseline; the [design notes](docs/design.sv) record measurements and
the reasoning behind optimizations.

## Status

Implemented: format, vectorized engine, SQL layer, wasm/JS package,
OPFS spill-over, Parquet ingest with caching, temporal columns, SIMD text
search. Designed but deliberately not built (recorded in `docs/design.sv`):
HTTP-range reads + per-segment compression (one package, awaiting a workload
that needs it), restricted joins, a stemmed token index, JS UDFs.

Published on npm as [`facetful`](https://www.npmjs.com/package/facetful)
(`npm install facetful`). Releases go out from a version tag through
`.github/workflows/release.yml`; `./scripts/build-package.sh` builds and
checks the same package locally in `dist/`.

## License

MIT
