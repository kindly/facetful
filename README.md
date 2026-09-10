# facetful

A tiny, read-only, columnar SQL engine for the browser — Rust compiled to
~170 KB gz of WebAssembly, zero runtime dependencies, running in a dedicated
worker. Publish data files on any static host; do the analytics at the user,
with no server.

Built for one job and fast at it: **faceted exploration of 100K–5M row
datasets** — facet counts, pivots, top-k, filters — at interaction speed.

Measured on a real 183,125 × 52 dataset (native / in-browser):

| | |
|---|---|
| facet refresh (counts + sums per dimension) | ~1 ms |
| two-dimension pivot | ~4 ms |
| top-100 of the whole filtered set, any sort column | ~2–5 ms |
| case-insensitive substring search across 3 columns | ~8 ms |
| open Parquet, first visit | seconds (transcode once, cached in OPFS) |
| open Parquet, every visit after | **~20–40 ms, zero decode** |

Against the incumbents at 1M rows: ahead of single-threaded DuckDB on 7 of 9
benchmark queries (facet counts 3 ms vs 14), at ~2% of DuckDB-WASM's download
size; ahead of SQLite on all 9.

## How

- **Zero-decode format**: `.facetful` files are compiled images — the on-disk
  segment layout *is* the in-memory execution layout. Opening a table reads a
  footer; column segments load lazily. Dictionary-encoded strings execute as
  integer scans (predicates like `LIKE`/`IN`/`=` evaluate once per distinct
  value, not once per row).
- **Parquet is the public contract**: `openParquet()` transcodes once in the
  browser (hyparquet + the same Rust compiler the CLI uses), caches the image
  in OPFS keyed by content hash, and reopens it instantly forever after.
- **Larger than memory**: tables can live in OPFS and stream segments through
  a byte-budgeted LRU cache (~2 GB/s through the sync-access-handle path).
- **SELECT-only SQL, SQLite semantics**, verified cell-for-cell against
  SQLite by a differential test suite. Hand-rolled lexer/parser with caret
  diagnostics and did-you-mean hints. Dates, `median`/`stddev`/`group_concat`,
  `select *`, SIMD substring search — the boring things work.
- **100% safe Rust** in the executor (`unsafe` only at the wasm FFI edge);
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

## Quickstart (native CLI)

```
cargo build --release -p facetful-cli
facetful convert data.csv data.facetful     # type inference, dictionaries, stats
facetful inspect data.facetful
facetful query data.facetful                # REPL; or one-shot with a SQL arg
facetful query data.facetful --bench qs.sql # 3 warmups + 10 runs, medians
```

The CLI compiler and the browser transcoder are the same Rust code path
(`facetful-format::compile`), so images are identical wherever they're built.

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

## Status

Working and measured: format, vectorized engine, SQL layer, wasm/JS package,
OPFS spill-over, Parquet ingest with caching, temporal columns, SIMD text
search. Designed but deliberately not built (recorded in `docs/design.sv`):
HTTP-range reads + per-segment compression (one package, awaiting a workload
that needs it), restricted joins, a stemmed token index, JS UDFs.

Not published to npm yet; the package installs from `dist/` via
`build-package.sh`.

## License

MIT
