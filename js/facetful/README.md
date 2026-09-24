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
- **Joins, CTEs and subqueries as cached materializations**: `JOIN`, `WITH`,
  `FROM (select …)`, `IN (select …)`, `EXISTS` all build a derived table once
  and serve every later facet from it — no per-query join cost.
- Dates, medians, stddev, group_concat, `select *`, LIKE fast paths — the
  boring things work.

## Quickstart

One install gives the browser library **and** the `facetful` command:

```
npm install facetful
npx facetful convert data.csv data.facetful        # streaming: any file size, ~20 MB of memory
npx facetful query data.facetful "select country, count(*) as n from t group by country order by n desc limit 5"
npx facetful inspect data.facetful      # rows, row groups, sort keys; per column kind, bytes, nulls, range or dictionary size
```

The command runs the same wasm engine the browser does (Node is the second
runtime), so a `.facetful` built here is exactly what `openParquet` would have
built, and SQL — including your registered functions, via `--udf module.mjs` —
behaves identically in both places. CSV can also be opened directly in the
browser with `loadCsv` (below). The ready-made functions (`json_extract`,
`to_tz`, `date_trunc`, …, see "User-defined functions") are registered by
default in both.

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

// big results: text as codes + dictionary, not a string per row — `true` for the image's
// dictionary columns (free), "all" to also encode other text columns where that pays (a hash pass)
const big = await db.query("select country, mw from t", { dictText: true });
big.columnRaw("country"); // { codes: Uint16Array, dict: { offsets, bytes }, validity }
big.dictionary("country"); // the decoded distinct values, indexed by code
await db.describe(); // the table's catalog: rows, groups, per-column kind / bytes / nulls / min-max / dictionary size
await db.memoryStats(); // { wasmBytes, tables }: the worker's memory high-water, from the page
console.log(r.elapsedMs, r.stats); // ms in worker, row groups pruned
```

The SQL table is always named `t`. Multiple tables coexist:
`db.query(sql, { table: "plants" })`.

## The three ways in

| method | use when |
|---|---|
| `openParquet(name, buf)` | the default: Parquet in, OPFS-cached compiled image, instant repeat visits |
| `load(name, buf)` | you already have a `.facetful` image (built by `facetful convert`) |
| `loadCsv(name, fileOrBuffer, {persist})` | a CSV: streamed through the converter in the worker, types inferred, optionally persisted |
| `loadOpfs(name, path, {cacheBytes})` | the file is in OPFS; open lazily, spill-over for big data |

Persistence is always explicit (`storeOpfs`), never write-behind. OPFS needs a
secure context (https or localhost); everything degrades to memory-only
without one.

## The `facetful` command

```
facetful convert in.csv out.facetful [--row-group-size N]
facetful query file.facetful ["select …"] [--table name=other.facetful …] [--udf module.mjs …]
facetful materialize in.facetful "select …" out.facetful [--table …] [--udf …]
```

`convert` streams: two passes over the CSV (types and dictionaries, then
encoding), row groups written as they finish, memory bounded by the distinct
values plus one row group — a 200 MB / 3M-row CSV converts in ~4 s at ~70 MB
of process memory (Node included). Types: int (narrowed), float, ISO date and
datetime, text; repeated text is dictionary-encoded. `query` without SQL is a
REPL; tables open lazily, so large files don't load into memory. The
ready-made functions are registered; a `--udf` module's default export adds
your own, as an array of `{ name, signature, fn }`.

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
`url_host(url)`. Each is a few lines of ordinary JavaScript over what the
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
declared as an optional peer dependency and loaded dynamically only when a
Parquet method is called. Under a bundler, `npm install hyparquet` is enough;
without one, pass `hyparquetUrl` to `Facetful.open`.

Flat schemas only (no nested/repeated columns). BOOLEAN/INT32/INT64 →
integers (narrowed), FLOAT/DOUBLE → float64, strings → dictionary-encoded when
it pays, DATE/TIMESTAMP → real date/timestamp columns (days / ms since epoch,
ISO strings on output). Int64 values beyond 2^53 lose precision in the
browser transcoder (the native CLI has no such limit).

## SQL dialect, briefly

SELECT-only, SQLite semantics (3-valued logic, null-skipping aggregates,
truncating integer division, NULL-first ascending sorts). Idioms: `IN`,
`BETWEEN`, `IS [NOT] NULL`, `[NOT] LIKE`, `CASE WHEN`, `CAST`,
`COUNT(DISTINCT x)`, `||`, `select *`, `JOIN`/`WITH`/subqueries and
user-defined functions as above. Aggregates: count, sum, avg, min, max,
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
