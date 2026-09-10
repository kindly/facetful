<sv-page label="Facetful vs Squirreling">

<sv-prose id="sq-verdict">
## Facetful vs Squirreling — source and measured comparison

5 September 2026. Squirreling **0.16.3**, npm gitHead `37b59175de9ddb2960caea18477407ccd8dc0d38`; Facetful is the current working tree, including the user's existing uncommitted changes.

**Verdict:** Squirreling is a stronger general browser query layer today. Facetful has a substantial measured advantage for the repeated aggregation/filtering workloads tested here. Squirreling does not make the existing engine redundant, but it makes a broad small-browser-SQL pitch much less distinctive.

Two corrections to the first market review: the current Squirreling release has a planner and column-batch execution, not just async rows; and a measured current query bundle is about **40 KB gzipped**, not the old advertised 13 KB. Its `executeSql` + `collect` entry bundles to 148,383 bytes minified / 39,511 bytes with system `gzip -9 -n`. The full public entry is about 40.5 KB gzipped. Facetful's WASM is about 134 KB gzipped. Neither figure includes the data; Squirreling excludes Parquet adapters/codecs, and Facetful excludes JS glue and adapters. This is a core-size comparison, not a whole-application cold-load comparison.

The benchmark deliberately compares **one SQL call with one SQL call**, fully consuming the same small outputs. It does not use Facetful's fused multi-facet API against multiple Squirreling queries.
</sv-prose>

<sv-prose id="sq-architecture">
## Architecture and present capabilities

Squirreling 0.16.3 parses SQL, creates an explicit operator plan, and executes with both batch and row paths. Prepared sources supply schemas, typed numeric vectors, column demands, row selections and residual filters. Supported expressions compile into column evaluators, avoiding per-cell promises; unsupported expressions fall back to row evaluation. Async columns and UDFs remain useful for network or model calls. [Pinned executor source](https://github.com/hyparam/squirreling/blob/37b59175de9ddb2960caea18477407ccd8dc0d38/src/execute/execute.js), [pinned batch compiler](https://github.com/hyparam/squirreling/blob/37b59175de9ddb2960caea18477407ccd8dc0d38/src/expression/batch.js).

Facetful parses and binds SQL against an immutable columnar table, then runs Rust/WASM vector kernels. Its distinguishing choices include dictionary-code predicates, direct grouping for dictionary dimensions, min/max pruning and a separate fused faceting operation. Squirreling's current public column-vector union does not include a dictionary-code vector; the benchmark gives it already-decoded string columns and typed numeric columns. A new adapter or kernel could change that boundary. [Pinned Squirreling vector contract](https://github.com/hyparam/squirreling/blob/37b59175de9ddb2960caea18477407ccd8dc0d38/src/types.d.ts).

Squirreling already implements joins, CTEs, subqueries, HAVING, set operations, window functionality, arrays/JSON, date functions, regex and extensible UDFs. Facetful's implemented SQL is substantially narrower: single-table SELECT/filter/group/order/limit, expressions and a useful scalar/aggregate set. It has no public JS UDF registration yet. Broad syntax coverage does not guarantee that all Squirreling combinations take the fast path. [Pinned planner](https://github.com/hyparam/squirreling/blob/37b59175de9ddb2960caea18477407ccd8dc0d38/src/plan/plan.js), [project documentation](https://github.com/hyparam/squirreling).

Squirreling is an ordinary JS package with type declarations and pluggable sources; Parquet access comes through the surrounding Hyparquet ecosystem. Facetful has a native CLI and an execution-image reader, browser worker glue and OPFS-backed reads. Its public Parquet and HTTP-range loading paths remain unimplemented. Those future capabilities cannot be counted as present competitive advantages.

Squirreling accepts AbortSignal and yields to the event loop. Facetful keeps the UI thread free through its dedicated worker, but its synchronous query call does not expose cooperative cancellation. Worker isolation and incremental/cancellable execution solve different problems.
</sv-prose>

<sv-prose id="sq-method">
## Measured SQL comparison

Environment: Linux, AMD Ryzen 7 5800H, Node v26.4.0, existing 200K- and 1M-row facet-shaped CSV/image fixtures. Data has dictionary-friendly strings and nullable numeric capacity. Each process runs one engine/adapter; the measured benchmark processes ran sequentially. Minor source inspection and bundle-building work also occurred on the machine, so this is not an isolated performance lab.

Squirreling batch input uses a custom `prepareScan` source, 16,384-row batches, prebuilt column slices, Float64 numeric arrays and validity bytes. No per-row objects or cell promises are required to supply this lane. Squirreling also gets a separate array-of-objects lane at 200K. The custom batch source performs projection but leaves predicate and limit execution to Squirreling; it does not implement storage statistics, indexes or predicate pushdown.

Facetful uses its actual release WASM through `js/facetful/core.js`. Both paths parse SQL and materialize returned values during timing. Data preparation, network and worker messaging are excluded. The first invocation is recorded separately; the table uses medians of subsequent calls (three Squirreling samples, seven Facetful samples). These are local CPU measurements, not browser network timings or reliable p95 estimates.

All 11 shared queries matched in row order, row count and values across both scales and the 200K row adapter. Floating outputs use relative tolerance 1e-8. Top-k adds `id` as a deterministic secondary sort key. Equivalent-expression experiments below also matched. These checks establish agreement on the fixtures, not full SQL conformance.
</sv-prose>

<sv-csv id="sq-timings" src=".sideview/research/squirreling/timings.csv" freeze="1" height="28rem">
</sv-csv>

<sv-prose id="sq-interpretation">
## Interpreting the results

At 1M rows, the country-grouped aggregate takes about **6.3 ms Facetful / 121 ms Squirreling batches**; two-dimensional grouping is **7.1 / 359 ms**; numeric arithmetic plus filtering is **13.7 / 85 ms**; top-50 numeric sorting is **16.5 / 1,214 ms**. These are substantive CPU advantages for Facetful on the tested data, without invoking the fused faceting API.

The label `group_high_card` comes from the existing benchmark: it groups the synthetic owner dimension, not a unique-string-per-row adversarial dataset. Do not generalize it to arbitrary high-cardinality text.

**Squirreling wins the preview query:** about **0.31 ms versus 47.8 ms** for `SELECT id, country, capacity FROM t LIMIT 50` on the 1M dataset. Source inspection explains the gap: Facetful's ordinary projection path materializes all matching output rows and applies LIMIT afterward, whereas Squirreling can stop its scan. This is an actionable Facetful implementation gap; it is not inherent to WASM.

**The largest Facetful ratios need qualification.** Squirreling's batch compiler currently declines `IN` value-list expressions. The original facet-count query therefore runs at about **1,831 ms**, versus **2.8 ms** in Facetful. Rewriting the same predicate as `a = x OR a = y OR a = z` puts Squirreling on its batch path and reduces it to **194 ms**. Facetful takes **14.1 ms** on that identical OR spelling. The other filtered-total query drops from **3,164 to 273 ms** in Squirreling when rewritten; Facetful takes **32.8 ms** on the equivalent spelling. These are correct-result optimizations, independently checked. A comparison quoting only the enormous IN gap would exaggerate the durability of Facetful's lead.

The late-ID query is dominated by **source pruning**: Facetful skips groups using the sorted ID statistics; this Squirreling source has no statistics. Its 0.19 / 64.7 ms result is not proof that Squirreling with a suitable storage adapter cannot prune.

At 200K, Squirreling's country grouping improves from **354 ms through rows to 30 ms through batches**. This is why the simple array API alone is an inadequate performance baseline. Batch size and source pushdown remain further tuning dimensions; this is a reasonable column adapter, not a claim to have exhausted every optimization.
</sv-prose>

<sv-prose id="sq-memory">
## Memory, streaming and SQL behavior

Squirreling can stream basic scans and retains accumulators for supported aggregates. Its current streaming set includes COUNT/COUNTIF/SUM/AVG/MIN/MAX; distinct counts additionally retain sets. Other aggregates and some expression combinations fall back to buffered rows. Its bounded top-k periodically sorts and trims candidates. An AsyncGenerator API therefore does not imply constant memory or early final answers for every query. [Streaming aggregation implementation](https://github.com/hyparam/squirreling/blob/37b59175de9ddb2960caea18477407ccd8dc0d38/src/execute/streamingAggregate.js), [sort implementation](https://github.com/hyparam/squirreling/blob/37b59175de9ddb2960caea18477407ccd8dc0d38/src/execute/sort.js).

Facetful has explicit OPFS segment-cache budgeting and compact dictionary storage, but query vectors, dictionaries, output rows, sort projections and aggregate state consume memory outside that cache. The preview issue is also a memory issue because its current ordinary projection builds rows later discarded by LIMIT. This comparison did not measure peak process or browser memory; no overall memory winner is claimed.

A small semantic probe also shows why swapping engines requires tests: `7 / 2` gives **3 in Facetful** and **3.5 in Squirreling**. Both return NULL for division by zero and for `NULL = NULL` on the tested expressions; both match the ASCII-case-insensitive LIKE probe. A decimal integer literal above JS's exact range rounded in both current public paths. Squirreling's vector types support BigInt, but that does not establish exactness for every literal/parser/export route. These are narrow observations, not a comprehensive correctness audit.
</sv-prose>

<sv-prose id="sq-udf">
## Where Squirreling's async model pays off

Using 1,000 synthetic rows and an instrumented async `score(id)` function, with no external API calls:

- `SELECT id, score(id) FROM t WHERE id % 100 = 0 LIMIT 5` called the function **5 times**.
- `SELECT id, score(id) FROM t ORDER BY id DESC LIMIT 5` also called it **5 times**.
- `SELECT id FROM t ORDER BY score(id) DESC LIMIT 5` called it **1,000 times**, because every score is needed to establish the winners.

This is a meaningful advantage for expensive remote/model computations: defer them until the query actually needs their values. It is already implemented, whereas Facetful's vectorized UDF interface is a design. The probe counts calls; it does not measure model latency or costs.

For exploring a few records, enriching selected records and composing SQL across asynchronous sources, I would start with Squirreling. For repeated aggregation over a prepared numeric/categorical snapshot, Facetful currently looks much stronger on measured CPU time.
</sv-prose>

<sv-prose id="sq-direction">
## What this means for the project

I would keep Facetful's specialization and stop treating Squirreling as merely a lightweight curiosity. It has broader features, easier JS integration and a credible route toward faster column execution. Your lead is real for these queries, but individual missing fast paths can close quickly.

The strongest Facetful claim to pursue is **fast, repeatable interaction over prepared data: coordinated facets, summaries, sorting and filtering with a small runtime**. Let Parquet remain the public source contract; make preparation and repeat visits easy. Avoid trying to match Squirreling's entire general SQL surface before users need it.

Priorities suggested by this comparison:

1. Fix ordinary LIMIT early termination when you next authorize implementation work; it directly affects the first table paint.
2. Expose the fused facet API through the public JS wrapper and build the visible explorer around it.
3. Add Squirreling's batch lane and equivalent OR queries to the lasting benchmark, then validate real Parquet/network paths and a phone.
4. Pursue reliable cancellation and the Parquet adapter before broad SQL expansion.

A hybrid is possible but not free. A Squirreling `prepareScan` source over Facetful could supply columns and push filters down, but its current scan contract does not push an entire GROUP BY into Facetful automatically. Preserving your aggregate speed would require query-level routing or more integration. SQL dialect differences also need handling. I would not undertake that complexity just to avoid maintaining your already-working parser.

The bounded experiment here supports continuing the specialist engine. It does not establish product-market fit, mobile performance, network superiority or that Squirreling cannot improve to cover the same workload.

### Reproduction and evidence

Saved under `.sideview/research/squirreling/`: `compare.mjs`, `verify.mjs`, `probes.mjs`, pinned package metadata, complete timings and result values, the equivalent-query experiments and the semantic/UDF probe output. The downloaded package and bundler remain at `/tmp/facetful-squirreling-xrucYD/`.

To reproduce from the project root, download and extract the `squirreling@0.16.3` npm tarball into a `package/` directory beside `compare.mjs`, build the release WASM, then run the harness with `200000 batch 3`, `200000 rows 3`, `200000 facetful 7`, `1000000 batch 3`, and `1000000 facetful 7`. `ONLY=facet_count_or,filtered_total_or SUFFIX=-rewrites` selects the rewrite experiments. The supplied verification script checks outputs. The harness also supports `FACETFUL_ROOT` and `BATCH_ROWS`.

No engine or application source was changed for this review.
</sv-prose>

</sv-page>
