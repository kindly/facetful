# facetful

A tiny columnar SQL engine for browser faceting. Rust compiled to ~150 KB gz
of WebAssembly, zero runtime dependencies, running in a dedicated worker.

Built for one job and fast at it: **exploring 100K–5M row datasets in the
browser** — facet counts, pivots, top-k, filters — at interaction speed.

- **Open Parquet instantly.** First visit transcodes once (seconds) and caches
  the compiled image in OPFS; every visit after reopens in ~20–40 ms with zero
  decode. Measured on a real 183K×52 dataset: facet queries 1–4 ms.
- **SELECT-only SQL**, SQLite-flavored semantics, verified cell-for-cell
  against SQLite by a differential test suite. Friendly errors with carets and
  did-you-mean hints.
- **Larger-than-memory**: tables can live in OPFS and load column segments
  lazily through a byte-budgeted LRU cache.
- **Filter-mask cache**: WHERE conjuncts are cached as per-row-group bitmaps,
  so facet bursts sharing a filter evaluate it once, and a `LIKE '%needle%'`
  extending a cached needle verifies only the rows the shorter one matched.
- Dates, medians, stddev, group_concat, `select *`, LIKE fast paths — the
  boring things work.

## Quickstart

```js
import { Facetful } from "facetful";

const db = await Facetful.open();
const buf = await (await fetch("plants.parquet")).arrayBuffer();
await db.openParquet("plants", buf); // transcode once, OPFS-cached thereafter

const r = await db.query(`
  select "Country/area", count(*) as n, round(sum("Capacity (MW)"), 1) as mw
  from t
  where "Status" = 'operating'
  group by "Country/area"
  order by mw desc limit 15
`);

for (const row of r.rows()) console.log(row);
r.columnRaw("mw"); // Float64Array + validity bitmap, near-zero copy (charts)
console.log(r.elapsedMs, r.stats); // ms in worker, row groups pruned
```

The SQL table is always named `t`. Multiple tables coexist:
`db.query(sql, { table: "plants" })`.

## The three ways in

| method | use when |
|---|---|
| `openParquet(name, buf)` | the default: Parquet in, OPFS-cached compiled image, instant repeat visits |
| `load(name, buf)` | you already have a `.facetful` image (built by the native CLI) |
| `loadOpfs(name, path, {cacheBytes})` | the file is in OPFS; open lazily, spill-over for big data |

Persistence is always explicit (`storeOpfs`), never write-behind. OPFS needs a
secure context (https or localhost); everything degrades to memory-only
without one.

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

## Parquet support

Reading uses [hyparquet](https://github.com/hyparam/hyparquet) (~20 KB gz),
declared as an optional peer dependency and loaded dynamically only when a
Parquet method is called. Under a bundler, `npm install hyparquet` is enough;
without one, pass `hyparquetUrl` to `Facetful.open`.

Flat schemas only (no nested/repeated columns). BOOLEAN/INT32/INT64 →
integers (narrowed), FLOAT/DOUBLE → float64, strings → dictionary-encoded when
it pays, DATE/TIMESTAMP → real date/timestamp columns (days / ms since epoch,
ISO strings on output). Int64 values beyond 2^53 lose precision in the
browser transcoder (the native CLI has no such limit).

## SQL dialect, briefly

SELECT-only, single table, SQLite semantics (3-valued logic, null-skipping
aggregates, truncating integer division, NULL-first ascending sorts).
Idioms: `IN`, `BETWEEN`, `IS [NOT] NULL`, `[NOT] LIKE`, `CASE WHEN`, `CAST`,
`COUNT(DISTINCT x)`, `||`, `select *`. Aggregates: count, sum, avg, min, max,
count(distinct), median, stddev, group_concat. Scalars: math (abs, round,
floor, ceil, sqrt, pow, exp, ln, sign), text (lower, upper, length, substr,
trim/ltrim/rtrim, replace, instr, concat), null handling (coalesce, ifnull,
nullif), temporal (year, month, day, hour, minute, second, date, timestamp,
strftime). `GROUP BY` and `ORDER BY` accept select aliases or 1-based
positions. Numbers may use exponents (`1e6`). Quote column names with spaces:
`"Capacity (MW)"`.

## Building the wasm from source

The engine lives in the same repository (Rust workspace, zero dependencies).
`scripts/build-package.sh` builds the wasm (with wasm-opt when available),
copies it next to this package, and runs `npm pack`.

## License

MIT
