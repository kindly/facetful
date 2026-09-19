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
- **Parquet input**: `openParquet()` transcodes in the
  browser (hyparquet + the same Rust compiler the CLI uses), caches the image
  in OPFS keyed by content hash, and reuses that image on subsequent opens
  while it remains cached.
- **OPFS storage**: tables can live in OPFS and load segments through
  a byte-budgeted LRU cache, limiting how much source data stays in memory.
- **SELECT-only SQL** with SQLite-compatible semantics for the supported
  query subset, checked by a differential test suite. Includes caret
  diagnostics, did-you-mean hints, dates, `median`/`stddev`/`group_concat`,
  `select *` and SIMD substring search.
- **Safe Rust** in the executor (`unsafe` only at the wasm FFI edge);
  the wasm SIMD128 kernels use the safe intrinsics.

## Quickstart (browser)

```js
import { Facetful } from "facetful";

const db = await Facetful.open();
const buf = await (await fetch("plants.parquet")).arrayBuffer();
await db.openParquet("plants", buf); // transcode once, OPFS-cached thereafter

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

## Quickstart (the `facetful` command)

```
npm install facetful
npx facetful convert data.csv data.facetful   # streaming, bounded memory, type inference
npx facetful query data.facetful              # REPL; or one-shot with a SQL arg
```

The command is the wasm engine under Node — the same module the browser
loads, so images and SQL are identical wherever they run. The native Rust CLI
is the reference implementation and the development tool:

```
cargo build --release -p facetful-cli
facetful convert data.csv data.facetful     # the same streaming converter, native speed
facetful inspect data.facetful
facetful query data.facetful --bench qs.sql # 3 warmups + 10 runs, medians
```

Both drive one converter (`facetful-format::stream`): two passes over the
input, row groups written as they finish, ~20 MB of memory whatever the file
size — and byte-identical output to the browser's Parquet transcoder for the
same data.

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
./scripts/size-check.sh         # wasm build + wasm-opt, enforces the 300 KB gz budget
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

Not published to npm yet; the package installs from `dist/` via
`build-package.sh`.

## License

MIT
