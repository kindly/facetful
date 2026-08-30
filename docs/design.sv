<sv-page label="Facetful design">

<sv-prose id="d1">
# Facetful — design (v1 draft)

Name: **facetful** (file extension **`.facetful`**) — settled 2026-08-30: facets are the flagship workload, the word reads as data immediately, and npm, crates.io and the GitHub namespace were all free. The full-word extension follows the `.parquet`/PMTiles precedent and avoids reading as "just one facet".

*Revised after the [niche & format review](/p/01a05077-fb79-7993-ae6c-1eae55321763): the validation spike is now milestone M1 (front-loaded, with an explicit decision gate); `.facetful`'s rationale is execution-readiness rather than reader size (hyparquet reads Parquet in ~20 KB gz); a spike-contingent lazy Parquet adapter is the planned compatibility path; and the result-representation / sort-caching / streaming claims are stated precisely. Status: technical gap confirmed by research, value unproven until the M1 spike.*

**What this is** (settled in the research round): a read-only, columnar, SELECT-only SQL engine in Rust. Two binaries from one workspace — a native CLI that converts CSV/JSON/Parquet into a custom zero-decode columnar format, and a small wasm engine (budget: **≤ 300 KB gzipped**, target ~200 KB) that queries that format from memory, OPFS, or HTTP range requests. No SharedArrayBuffer, no COOP/COEP headers, wasm32, engine runs in a dedicated worker.

**Long-term aim**: a browser-based tool for small-to-medium analytics — usable by a person or an agent — with nothing installed outside the browser sandbox and nothing running server-side beyond static file hosting. Publish `.facetful` files like you publish images; the page queries them (range requests included) with no API to run, patch, scale, or pay for. SQL as the interface is deliberate: it's the query language both analysts and LLMs already speak. Lineage in one line: **`.facetful` aims to be to tabular analytics what PMTiles is to map tiles** — the cloud-native-geospatial pattern (COG, PMTiles, FlatGeobuf: one static file, range requests, no server) applied to SQL, with Arrow IPC's zero-copy layout and Parquet's skipping metadata.

Sections: file format → SQL surface → engine architecture → JS API → workspace layout → milestones → risks → sign-off round.
</sv-prose>

<sv-prose id="d2">
## File format (`.facetful` v1)

Design rule: **the on-disk bytes of a column segment are exactly the in-memory representation** — loading a segment is one read into WASM memory, zero decode. Everything variable lives in the footer so the data region stays raw.

**Scope of zero-decode — segment payloads only, not a file image.** Header, footer and the row-group directory are parsed *once at open* into an ordinary catalog struct (schema, `sorted_by`, per-group offsets + stats — a few KB); they never stay as raw bytes. Loaded segments live in a cache keyed by `(row group, column)` as independent aligned buffers — file adjacency is irrelevant once loaded. Only the *intra-segment* byte layout is shared between disk and memory, and that's the part that makes loads transform-free. This layering is also what keeps the door open if mutability is ever wanted: the standard delta-overlay path (immutable base + in-memory deletion bitmap + memory-backed insert groups + per-group compaction through the existing writer) never shuffles base segments at all — row groups being independent units is exactly the mutability-friendly shape.

```
[header: magic "FCT1", version, schema, sorted_by, row-group size]
[dictionary block: lens table + each dict column's offsets/bytes, written once]
[row-group 0: group header (row count, segment lengths) + column segments…]
[row-group 1: …]
…
[footer: row count, row-group directory (offsets + stats)]
[footer_len: u32 LE] [magic "FCT1"]
```

**Readable from both ends** (the Arrow IPC trick): the header carries everything known before writing begins (schema, sort order), each row group is **self-framing** (its group header gives row count + segment lengths), and the footer is the random-access index. That yields three consumption modes from one file:
1. **Streaming GET / no range support**: render the first 64K rows as soon as group 0 arrives, then group by group — no need for the whole file.
2. **HTTP range**: `tail(8) → footer → exactly the segments a query needs`.
3. **Whole file / OPFS**: footer read once, then indexed access.

Cost: a few dozen bytes per group duplicated between group headers and the footer. A sidecar footer file was considered and rejected — two artifacts to desync, two fetches per open, and self-framing groups cover the streaming case better.

**Row groups**: variable-sized by construction (every group records its own row count); the CLI writes to a default *target* of 65,536 rows. Big enough that per-group overhead vanishes, small enough that one group's working set stays comfortably inside WASM memory — the unit of larger-than-memory scanning and of min/max skipping. On sorted data the CLI can later align group boundaries to *value* boundaries (`--split-on col`: one group per month/letter/ID-range), making min/max pruning exact — with a max-rows guardrail so skewed buckets still split (the memory bound is per-group) and a min target so sparse buckets merge rather than bloat the directory.

**Column segment layout** (within a row group, every segment 8-byte aligned, padded):

| Type | Layout |
|---|---|
| `Int8/16/32/64` / `Float64` | raw little-endian values, `n × width` bytes — CLI picks the narrowest int width that fits the column |
| `Bool` | bit-packed, `⌈n/8⌉` bytes |
| `Utf8` | `(n+1) × u32` offsets, then UTF-8 bytes blob |
| `Utf8`, dictionary-encoded | `n × u8/u16` codes per group (u8 when cardinality ≤ 256 — flag `CODES_U8`); the dictionary itself (offsets + bytes) lives **once** in the file-level dictionary block after the header. **Operable without decoding**: filters/group-by run on the codes, strings materialize only at output. CLI applies automatically above a cardinality threshold; ~10-20x less memory for facet-type columns |
| `Date` / `Timestamp` | `Int64` (days / milliseconds since epoch) — stored identically, distinguished by schema type |
| nulls | validity bitmap segment (`⌈n/8⌉` bytes), **omitted entirely when null_count = 0** |

**Header** (everything known before writing begins — one-pass CLI writes stay possible; hand-rolled binary encoding, no serde anywhere near the wasm binary):
- magic, format version, row-group size
- schema: per column — name, type tag (u8), flags (u16, reserved bits for future dictionary/compression encodings)
- **`sorted_by`**: the column(s) + direction the file is globally sorted on (set by CLI `--sort-by`, absent if unsorted). Trusting it gives the engine precise group skipping (disjoint ranges → binary search instead of overlap checks) and free `ORDER BY` on that column (stream groups in order, no sort operator)

**Footer** (the random-access index, written last):
- total row count
- row-group directory: per group × column — segment offset u64, segment length u32, null count u32, **min/max stats** (numeric: the values; Utf8: first 16 bytes of min/max)

**Compression strategy** (three read paths, three answers):
- **Whole-file fetch** → transport compression (`Content-Encoding: gzip/br`), free from any CDN/static host, zero format work. Columnar layout already makes the bytes compress well — similar values sit adjacent.
- **HTTP range / OPFS** → transport compression doesn't apply (ranges address raw bytes; OPFS stores uncompressed). Likely evolution: deflate-compressed segments decompressed at ingest with the browser-native **`DecompressionStream`** (zero wasm bytes, async — fine at ingest time), stored raw in OPFS so the sync read path stays zero-decode; the reserved **per-segment LZ4 flag** (sync decode, ~2-5KB decoder) only if OPFS quota ever becomes the real pressure. zstd/brotli decoders rejected on size (~50-200KB+).
- **Measured estimate (200K-row spike dataset)**: per-segment deflate would store/transfer **1.93 MB vs 1.98 MB whole-file gzip** — segment granularity costs only ~2.5% at 65K-row segments, so the reserved flag buys transport-gzip-equivalent sizes for the OPFS and range paths (which today move raw bytes, ~2.9× more). With improved encodings (u8 codes where card ≤ 256, `Decimal(scale)` as scaled i32 for fixed-decimal measures — exact, and the biggest single win since raw f64 gzips terribly — and narrow ints): raw 5.69→3.09 MB, compressed 1.98→1.67 MB.
- **Making raw bytes smaller & more gzippable without breaking zero-decode**, all CLI-side: `--sort-by col` at conversion (long runs in every column — best single ratio win, and it supercharges min/max skipping too); **narrow integer widths** — Int8/16/32 type tags are in format v1, the CLI picks the narrowest width that fits; dictionary encoding for low-cardinality strings (defined in format v1 — see the segment-layout table; unlike byte compression it also cuts *query memory* ~10-20x, because the engine executes on the codes without decoding). Byte-shuffle and delta encoding are deliberately rejected: they'd break zero-decode for modest gains over sort + narrow widths.

**Deliberately reserved, not built**: per-segment compression flag (a compressed segment must be decompressed to scan, so per-segment *working set* is unchanged — but it earns its place twice: OPFS quota pressure, and the memory-source path, where the whole file is resident in the heap and compressed-at-rest + decompress-per-scan with a small LRU of hot segments cuts resident footprint ~2-3x),**per-group Bloom filters** and their finer-grained successor, **sub-group stripe filters** (min/max or blooms per ~8K-row stripe *within* a group — zero-decode makes reading just the matching slice of a segment pure arithmetic; this pair is the only "secondary index" this format will ever get; B-trees are permanently out, and a differently-sorted copy of the data via `--sort-by` covers the rest). The flag bits exist in v1 so old readers can reject files that use them; the code doesn't.

**Layout choice & compromises**: this is the "PAX" layout every columnar format converges on (Parquet, ORC, DuckDB's file format). The lineage is really **Arrow IPC's data layout** (raw vectors, zero-decode) **plus Parquet's metadata** (footer, row groups, min/max skipping — which Arrow IPC lacks entirely): skips like Parquet, reads like Arrow. Accepted compromises: whole-column scans are strided across groups rather than one sequential read (a fully column-contiguous layout would win there, but loses streaming one-pass imports); min/max skipping only bites when data is sorted/clustered on the filtered column (`--sort-by` helps twice). Row-group size is *the* tunable (bigger = contiguity + fewer range requests; smaller = finer skipping + less memory) — it's recorded in the footer, exposed as `--row-group-size`, tuned against the M1-spike/M6 benchmarks without a format change.

**Why a native format** (revised after the niche/format review — the earlier "Parquet readers are huge" argument was stale: hyparquet reads Parquet in ~20 KB gz, +~69 KB for the full codec set; parquet-wasm's 1.8 MB reflects arrow-rs, not an inherent cost of reading Parquet). The real rationale is **execution-readiness, end to end**:
- segments are already the executor's physical representation — no per-query page decode, where Parquet re-pays thrift + encodings + decompression on every read;
- **dictionary preservation**: hyparquet dereferences dictionary codes into JS strings before exposing data, destroying exactly the representation facet workloads need — `.facetful` keeps codes + dictionary as the operable form all the way into the executor;
- validity, offsets and stats arrive in the exact shape the engine consumes — no reader→executor conversion layer, no JS object churn.

`.facetful` is **not** a Parquet replacement and does not need to beat Parquet at compression; it is an execution-optimised publishing format. The deciding metric is **bytes + time to first correct result**, then repeat-query latency and peak memory — measured in the M1 spike, not asserted. Honest flip side: Parquet is already produced and understood, its encodings can transfer fewer bytes over ranges, and a custom format is an adoption ask on publishers — hence:

**Parquet strategy (product shape, contingent on the spike)**: `.facetful` preferred, plus a **lazily-imported hyparquet adapter** — `db.loadParquet(name, url, {mode: "direct"})` decodes one row group at a time into the same executor (push-shaped: JS awaits and decodes a group, bulk-copies typed arrays into wasm memory, executes, proceeds), or `{mode: "materialize", persist}` transcodes once to `.facetful` for repeat sessions. `.facetful` then becomes an optimisation publishers adopt after seeing a measured benefit, not a prerequisite for trying the engine. The M1 spike effectively measured the materialize path (the hyparquet lane's load *is* parquet→facetful-memory conversion): at 1M rows the transcode costs ~0.4-0.8 s (linear in N, transiently double memory) vs facetful's flat ~0.18 s open — so **"publish parquet, transcode once, persist `.facetful` in OPFS"** is the strongest pre-encodings configuration: first visit pays parquet's smaller wire + one transcode, every later visit gets the flat zero-decode open offline. If the compact encodings reach transfer parity, direct `.facetful` wins outright. Committed only if the spike shows the direct path is usable — strings/nulls/dictionaries are its known weak point, and two execution paths is real surface for a one-person project.
</sv-prose>

<sv-prose id="d3">
## SQL surface

**v1 grammar** (milestone M3 — the M1 spike deliberately ships *no* SQL, just a structured query API the parser later compiles to):

```
query     := SELECT select_list FROM ident
             [WHERE expr] [GROUP BY expr_list]
             [ORDER BY order_item (',' order_item)*] [LIMIT n [OFFSET n]]
select_list := '*' | sel_item (',' sel_item)*
sel_item  := expr [[AS] ident]
order_item := expr [ASC|DESC]
expr      := Pratt-parsed: literals, column refs, unary -/NOT,
             * / % + - || comparison, BETWEEN, IN (list), LIKE,
             IS [NOT] NULL, AND, OR, CASE WHEN, CAST(x AS type),
             function calls
```

**v1 functions** — aggregates: `count(*)`, `count(x)`, `count(distinct x)`, `sum`, `avg`, `min`, `max`; scalars: `abs`, `round`, `floor`, `ceil`, `lower`, `upper`, `length`, `substr`, `coalesce`. Nothing else until a benchmark or a real page needs it — every function is binary size.

**Later surfaces**: `JOIN` (inner + left, hash join) and subqueries in `FROM` at M8; window functions (`row_number`, `rank`, `sum/avg over (partition by … order by …)`) and CTEs at M9.

**Never** (or not until the project changes its mind): INSERT/UPDATE/DELETE into existing tables, transactions, correlated subqueries, PIVOT syntax. Derived tables are allowed — `db.materialize` creates *new immutable tables from queries* (optionally persisted to OPFS); `CREATE TABLE AS` syntax may later become sugar over it. The line that holds: nothing ever mutates existing data — that's what keeps transactions, locking, and cache invalidation permanently out of the binary.

Parser: hand-rolled lexer + recursive descent, Pratt for expressions — the research says 1–3 kLoC and tens of KB compiled. SQLite-compatible semantics wherever there's a choice (NULL handling, integer division, LIKE case rules), because that's what users and LLMs assume.
</sv-prose>

<sv-prose id="d4">
## Engine architecture

```
SQL text → lexer/parser → AST
        → binder/planner   (resolve names against footer schema;
                            predicate pushdown → row-group skipping via min/max;
                            projection pruning → only referenced columns are ever read)
        → vectorized executor
```

**Executor**: operates on **vectors of 2,048 values** sliced from row-group segments (the DuckDB vector size — fits L1/L2, amortizes dispatch, no codegen needed). Operators: `scan` (group-skip → segment reads → vectors), `filter` (computes a **selection vector** — indices, no copying), `project` (expression eval over vectors), `hash aggregate`, `sort` (only materializes post-filter rows), `limit`. Pull-based: each operator asks its child for the next vector batch.

**Storage access is one trait**: `trait SegmentSource { fn read(&self, offset: u64, len: u32, into: &mut [u8]); }` with three impls — memory slice, OPFS sync-access-handle (worker), HTTP range (async at the fetch layer, cached into memory chunks before the sync engine sees them). A small LRU of decoded-segment buffers sits above it; SELECT-only means **zero cache-invalidation logic**.

**Tables are columns behind an interface**, not ".facetful files": the binder resolves a table name to a set of column providers (file-backed via SegmentSource, or memory-backed). This one abstraction future-proofs materialized views / pre-aggregation (`db.materialize("cube", "select … group by …")` re-registers a result as a table) and pivot APIs — a pivot is a GROUP BY plus client-side reshaping, and the format writer already exists in the shared crate if a materialized table should be persisted to OPFS (write-once, immutability preserved).

**Larger-than-memory** falls out of the shape: a scan holds one row group's referenced segments at a time. A 2 GB file with `WHERE region = 'EU'` touches only the row groups whose min/max admit `'EU'`, one at a time, and only the queried columns.

**Precision notes** (claims tightened after the niche/format review — these bound what the marketing sentences may say):
- **Zero-decode ≠ zero-copy.** Fetch, OPFS reads, worker messages and placement into wasm memory can each copy bytes. The native format removes *encoding decode* and *object materialisation*; the actual copy count per path gets measured and documented in the spike, not assumed away.
- **Streaming ≠ query completion.** Self-framing groups give early *preview rows* and progressive scan-shaped results. A `GROUP BY`, aggregate, or global `ORDER BY` is only correct once all relevant groups have arrived — partial results for those are estimates and must be labelled as such or withheld.
- **Range requests aren't automatically small queries.** Min/max pruning bites on sorted/clustered columns; a correct multi-facet refresh may still scan most of the selected facet columns; and many small ranges can lose to one larger sequential fetch — the range source must coalesce adjacent segment reads.
- **`ORDER BY … LIMIT/OFFSET` virtual scroll is only "sort once"** on the file's sorted column, or with a cached **sort permutation per (filter, order) state** held in the worker — the executor caches that permutation and invalidates it on filter change; large OFFSETs walk the permutation, not re-sort.

**Extensibility: user-defined functions** (small core + functions supplied at load time — also the size-budget escape valve: function requests land in UDF-land, not in the core binary):
- **The UDF ABI is vectorized** — one call per 2,048-value vector, never per row, so the wasm↔JS boundary cost is amortized 2048×. Inputs arrive as typed-array views + validity bitmaps; the UDF fills an output vector. Dictionary columns are materialized to plain vectors before the call; the planner treats UDFs as opaque scalars (no pushdown through them, no stats).
- **Tier 1 — JS UDFs** (`db.registerFunction(name, signature, fn)`): the engine compiles in a single generic trampoline import (`call_udf(id, args_ptr, out_ptr, len)`); the JS wrapper keeps the id→function registry. Registration at load time never changes the wasm module. This ships first.
- **Tier 2 — wasm plug-in UDFs**: a separate `.wasm` with its own memory exporting `alloc` + functions; the JS glue copies vectors in/out (16KB per vector — microseconds, amortized). Isolated and robust (Extism-style). Shared-memory dynamic linking (`call_indirect` through a shared table + one linear memory) is **explicitly rejected**: two independently-compiled Rust modules sharing an allocator is real dynamic-linking pain for negligible gain over the copy ABI.
- **Tier 0 — compile-time Rust UDFs**: a trait + feature-flagged registration for people building their own engine binary; zero overhead, requires rebuild.
- **Custom aggregates** (init/update/merge/finish state) are deliberately later — scalar UDFs cover the common asks.

**Size discipline** (the thesis lives here):
- no serde, no sqlparser-rs, no chrono (hand-rolled date math), no regex (hand-rolled LIKE)
- `opt-level="z"`, fat LTO, `codegen-units=1`, `panic="abort"`, `wasm-opt -Oz` in release CI
- CI fails if engine wasm exceeds the gz budget; a tracked size report per PR from day one (M0)
</sv-prose>

<sv-prose id="d5">
## JS API sketch

```js
import { Facetful } from "facetful";          // ~KBs of glue + lazy worker

const db = await Facetful.open();              // spawns the worker, instantiates wasm
await db.load("sales", await fetch("/sales.facetful"));       // memory
await db.loadOpfs("events", "datasets/events.facetful");      // OPFS, larger-than-memory
await db.loadUrl("trips", "https://cdn…/trips.facetful");     // HTTP range, no full download
// self-framing groups: loadUrl on a range-less server streams instead —
// scan-shaped results over arrived groups immediately; aggregates wait for completeness

// compatibility path (post-spike, lazily imports hyparquet + codecs — core budget unaffected):
await db.loadParquet("legacy", "https://…/data.parquet", { mode: "direct" });
await db.loadParquet("legacy", "https://…/data.parquet", { mode: "materialize", persist: "datasets/legacy.facetful" });

const result = await db.query(
  "select region, sum(amount) as total from sales group by region order by total desc"
);

// derived tables: new immutable tables from queries (not a write path)
await db.materialize("top_regions", "select … group by region", {
  sortBy: "total desc",              // uses the existing sort operator
  persist: "datasets/top.facetful",       // optional: write a real .facetful to OPFS (~5-15KB of writer code)
});
result.columns   // { region: {codes, dict} | {offsets, bytes}, total: Float64Array }
                 // strings NEVER cross as Array<string> (not transferable):
                 // dictionary codes + dictionary buffers, or offsets + one UTF-8 blob
result.rows()    // convenience iterator — materializes JS objects/strings lazily,
                 // paying that cost knowingly in the wrapper, never in the transfer
```

Results cross the worker boundary as transferable ArrayBuffers (column-major), so a million-row result doesn't get JSON-serialized. Arrow IPC export is a possible later addition — it's an ecosystem door, not a v1 need.
</sv-prose>

<sv-prose id="d6">
## Workspace layout

```
facetful/            # repo dir is still ~/projects/browserdb — rename when convenient
  crates/
    facetful-format    # format read/write, footer codec, stats — shared by CLI & engine, no_std-friendly core
    facetful-engine    # lexer, parser, planner, vectorized executor — pure, no I/O, fuzzable
    facetful-wasm      # wasm-bindgen bindings, worker protocol, OPFS/range SegmentSources
    facetful-cli       # native: `facetful convert in.csv|json|parquet out.facetful`, `facetful export data.facetful out.parquet|csv|json`,
                  # `facetful inspect`, `facetful query` (native REPL for testing)
  js/facetful/     # npm package: worker glue, typed API, bundling
  web/demo/       # static demo page (also the manual test bed; works on Firefox from day one)
  benches/        # criterion (native) + browser bench harness vs SQLite WASM / DuckDB-WASM / Arquero
  docs/           # this design doc, format spec as it solidifies
```

`facetful-engine` depending only on `facetful-format` (and neither touching I/O directly) is what keeps the whole thing testable natively — the wasm layer stays a thin shell.
</sv-prose>

<sv-prose id="d7">
## Milestones

Restructured after the niche/format review: **the flagship benchmark moves to the front**. The old plan paid for format + parser + planner + executor + wasm + OPFS before measuring the thesis at M5; the spike answers the riskiest question at ~15% of that cost. (The SQL parser — the fun part — is deferred, not dropped; it doesn't rot by waiting.)

| # | Deliverable | Proves |
|---|---|---|
| **M0** | Workspace scaffold, CI: build wasm, **gz size budget check**, size report | discipline exists before code does |
| **M1** | **Validation spike (decision gate).** Real/anonymised ~200K-row map dataset. Minimal `.facetful` (fixed-width + validity + dictionary strings + row groups) and just-enough CLI to produce it; minimal wasm executor behind a tiny *structured* query API — **no SQL grammar**: filter masks, correct filters-except-own facet counts, totals, sort, LIMIT, in a dedicated worker. Race four paths — current hand-written JS, crossfilter2, hyparquet→same executor, `.facetful`→same executor — with DuckDB-WASM as reference row. Measure cold load (engine/reader/data bytes, requests, decode/copy, time to first correct facets), interaction p50/p95, main-thread long tasks, JS+wasm memory, full-fetch vs ranges with network throttling; Firefox first, then Chromium/Safari + one mid-range phone | **the niche claim, before the investment** — see decision gate below |
| **M2** | `facetful-format` v1 hardened (header/footer spec, golden-file tests) + CLI `convert` CSV/JSON + `facetful inspect` | the format is real; data gets in |
| **M3** | Engine v1 SQL (parser/planner over the spike executor) — native tests + `facetful query` REPL | the fun part works; correctness suite vs SQLite on same data |
| **M4** | wasm bindings + JS package + demo page (fetch → memory → query) | first end-to-end browser *product*; cold-cache time-to-first-query, the headline metric for embedded/MCP-app uses |
| **M5** | OPFS SegmentSource in worker; query a file bigger than WASM memory | the differentiator |
| **M6** | Dictionary encoding end-to-end in the full engine; bench harness extended to SQLite WASM / Arquero / 100K-row vanilla-JS baseline (load incl. JSON.parse + interactive filter/group/sort) | the spike's numbers hold in the real engine |
| **M7** | HTTP-range SegmentSource (with request coalescing); demo querying a CDN-hosted file | the static-hosting trick |
| **M8** | Hash joins (inner/left) | dashboard-grade SQL |
| **M9** | Window functions; Parquet input in CLI; `db.loadParquet` adapter (if the spike's direct path proved usable); **lazy Parquet export**: a standalone `facetful-parquet.wasm` fetched on first `db.exportParquet()` via dynamic `import()` — transcodes `.facetful` row group by row group; minimal PLAIN-encoding writer, not arrow-rs. **CSV export** is JS-side (streamed from result columns, zero wasm bytes) and can land much earlier | long tail |

**Language question: settled — Rust/wasm** (measured, not assumed; see the [Wasm vs JS bench](/p/bench) page, `spikes/lang-bench/`). The kernel benchmark ran identical algorithms over identical data in plain JS (typed arrays, best-case style) and Rust/wasm, in Firefox at up to 5M rows. Results: facet kernel only 1.36x — JS would have sufficed for the flagship page alone — but **hash GROUP BY 3.5x vs idiomatic JS / 1.6x vs JS's hand-rolled best**, **SIMD scans 3-10x** (for +1.7 KB of binary), and **4x better p95 tail latency** (238ms vs 90ms at 5M rows — GC jank is real and wasm's absence of it was measured, not theorized). Consequences: the **SIMD128 build is the default** (its browser floor, Chrome 91+/FF 89+/Safari 16.4+, matches OPFS's — no dual build); the plain-JS kernels stay alive in `spikes/lang-bench` as the benchmark control; and a hard-won note for the executor: a low-bits multiplicative hash clustered catastrophically until mixed (`x ^= x >> 15`) — algorithm bugs dominate language wins in both directions.

**M1 decision gate** (agreed outcomes, so the numbers decide and not sunk cost):
- `.facetful` clearly wins cold-start, repeated facets and memory → keep it as the preferred publishing format, build the SQL layer (M2+).
- Direct Parquet (hyparquet→executor) is close enough → Parquet becomes the main source; `.facetful` survives only as an internal cache/materialisation format.
- Neither engine path convincingly beats careful JS/crossfilter2 at 200K rows → narrow to a specialist faceting/table runtime, or stop before building general SQL.
- Engine wins for ad-hoc SQL but not the flagship UI → reposition around embedded querying/agents and pick benchmarks that represent that honestly.

Each milestone ends with the size report and the demo page still working — in Firefox first.

## Risks / open eyes

- **The performance claims are estimates until M1.** The ms-level facet-latency and load-time numbers in the design discussion are informed projections, not measurements — the spike exists to replace them with data, on throttled networks, not localhost.
- **Adoption friction is the biggest product risk**: asking publishers to create and maintain a custom-format asset. Mitigations: the CLI makes conversion one command in a publish pipeline; the (spike-contingent) `loadParquet` adapter makes `.facetful` an optimisation rather than a prerequisite.
- **High-cardinality GROUP BY / ORDER BY** materializes in memory — v1 accepts this (documented limit), spill-to-OPFS for operators is a possible M10.
- **Benchmark humility**: SQLite has 20 years of planner; the pitch is the size/start/scan quadrant, and the M1 gate + M6 harness exist to keep that claim honest.
- **String-heavy data**: low-cardinality columns are covered by dictionary encoding (v1 format, in the spike from day one); genuinely high-cardinality strings (names, URLs) stay offsets+bytes, bounded by the per-row-group working set.
- **Scope creep is the project-killer**: every SQL feature request gets weighed against the size budget in CI.

## Future endpoints (post-M7, noted so nothing blocks them)

- **MCP Apps UI**: the engine + a table/facet component embedded in an MCP app iframe, querying `.facetful` on S3 — first-load size is the whole game there (8MB engines are non-starters in on-demand iframes). Caveat to verify per host: iframe CSP may require data to flow via the MCP server rather than direct S3 fetch — the `SegmentSource` trait makes "fetch via tool-call proxy" a small adapter.
- **Native MCP tool**: the same pure core crates compiled into a small native MCP server — `query(dataset_url, sql)` over S3 range requests, for agents that want answers without a browser. A fourth consumer of `facetful-engine`/`facetful-format` alongside CLI, wasm, and benches.
- **Dataframe-flavoured JS API** (tidyverse/Arquero-style verbs compiling to SQL strings): pure JS sugar, a few KB, no wasm cost — for the SQL-averse.
</sv-prose>

<sv-prose id="d8">
## Sign-off record

Round 2 approved 2026-08-30, all as suggested: **result shape** = column-major transferable buffers (strings as dictionary codes or offsets+bytes) with a `rows()` convenience layer; **dates** = format + CLI recognize Date/Timestamp in v1, engine treats them as comparable Int64, date functions later; **design approved** — M0 (scaffold + size-budget CI) and the M1 validation spike are underway. The spike runs on synthetic facet-shaped data until a real/anonymised copy of the ~200K-row map CSV is provided, then re-runs on the real thing.
</sv-prose>

<sv-markup id="d9">
<div class="alert alert-info w-100">
  <strong>M1 spike benchmark — run it from any tailnet device:</strong>
  <a href="http://100.102.221.40:8765/web/spike-bench/index.html" target="_blank">http://100.102.221.40:8765/web/spike-bench/index.html</a>
  <div class="small text-muted mt-1">200K rows · three lanes (current-style JS objects, crossfilter2, .facetful→wasm), correctness-verified against each other before timing, all in a worker. Press "Run benchmark". Node/V8 preview: facetful 2.45ms vs crossfilter 5.5ms vs JS objects 35.7ms median per interaction. Serving from the repo via <code>python3 -m http.server 8765</code> on the lenovo box.</div>
</div>
<iframe src="http://100.102.221.40:8765/web/spike-bench/index.html" style="width:100%;height:34rem;border:1px solid #8884;border-radius:6px" title="facetful spike bench"></iframe>
</sv-markup>

<sv-prose id="d10">
## M1 spike — first results (2026-08-30, browser via tailnet, 200K rows)

All lanes verified to produce **identical facet counts and totals** before timing; every lane runs in a worker; interaction = correct filters-except-own counts over 6 dims + totals + top-50.

| lane | assets (raw — local server doesn't gzip) | load | interaction median | p95 | vs current-style JS |
|---|---|---|---|---|---|
| JS objects (current-style, correct facets) | 12.63 MB | 203 ms | 29.00 ms | 46.00 ms | 1.0× |
| crossfilter2 | 12.63 MB | 520 ms | 6.00 ms | 11.00 ms | 4.8× |
| **.facetful → wasm engine** | **5.51 MB** | **155 ms** | **3.00 ms** | **4.00 ms** | **9.7×** |

Reading against the decision gate:
- **Interaction: 9.7× over the realistic baseline, 2× over crossfilter** (the strongest specialist) — and the p95 gap is wider still (4 ms vs 46 ms): correct faceting at 200K rows is comfortably 60fps-budget territory only in the facetful lane.
- **Load: fastest of the three** (155 ms including wasm instantiation, table open, and dictionary extraction) while crossfilter pays 520 ms building its indexes.
- **Assets: 1.36× smaller transfer** on a real (gzipping) host — 1.98 MB vs 2.69 MB gz. (The bench table shows raw bytes because the local server doesn't compress; the earlier "2.3× smaller" framing was corrected — gzip closes most of the raw gap on text. The raw difference still matters *post-download*: the CSV lanes hold 12.6 MB of text and parse it into ~200K heap objects, while the 5.5 MB .facetful is already the queryable representation.) Known raw fat to reclaim later: per-group dictionary duplication (dict-once) and Int64-where-Int32-fits (narrow-width inference not yet in the CLI). The entire engine adds 19.7 KB gz.
- Engine binary: **50.7 KB raw / 19.7 KB gz = 6% of the 300 KB budget** with the whole format reader + facet executor inside.

Still open before the gate closes: the DuckDB-WASM reference lane, the hyparquet→JS-kernels lane, throttled cold-load measurement, and a re-run on the real map dataset when available.
</sv-prose>

<sv-prose id="d11">
## M1 spike — full five-lane results (2026-08-31, Firefox, 200K rows)

| lane | transfer (gz) | load | est. cold @4G | interaction median | p95 | vs JS objects |
|---|---|---|---|---|---|---|
| JS objects (current-style, correct facets) | 2.58 MB | 267 ms | 2.67 s | 28 ms | 53 ms | 1.0× |
| crossfilter2 | 2.59 MB | 626 ms | 3.04 s | 6 ms | 12 ms | 4.7× |
| .facetful → wasm engine | 1.90 MB | 161 ms | 1.93 s | 3 ms | 4 ms | 9.3× |
| parquet → hyparquet → JS typed kernels | 1.62 MB | 172 ms | 1.68 s | 2 ms | 3 ms | 14.0× |
| DuckDB-WASM (reference, async SQL) | 10.46 MB | 7,134 ms | 16.88 s | 53 ms | 67 ms | 0.5× |

### Honest gate reading

**Proven decisively: the niche vs DuckDB.** 17× faster interactions — DuckDB's 8 async SQL round trips per facet refresh cost more than the scans themselves, leaving it *slower than hand-written JS objects* on this workload — and ~9× faster cold start (1.9 s vs 16.9 s @4G). The "embeddable instant-start analytics below DuckDB" quadrant is real and this architecture owns it.

**Proven: the representation, not (yet) the wasm.** The two fast lanes share one design — dictionary codes in flat arrays + the correct one-pass facet algorithm — and at 200K rows they sit at timer-granularity parity (2 vs 3 ms on a 1 ms Firefox clock). Consistent with the language micro-bench (facet kernel 1.36×): at flagship scale the representation and algorithm deliver the win; JS-vs-wasm is a wash *for this kernel*. Wasm's measured edges (GROUP BY 3.5×, SIMD scans, GC-free tails) weren't exercised by this workload.

**Not yet tested — exactly where .facetful's reasons-to-exist live:**
1. **Scale.** The hyparquet lane's load materializes every row as a JS object before dict-encoding; at 1-5M rows that means seconds of parse and a heap spike while zero-decode load stays flat. The 200K parity is unlikely to survive 1M+ — measure, don't assume.
2. **HTTP range queries** (query without downloading) and **OPFS larger-than-memory** — neither exercised; both are .facetful-only capabilities in this design.
3. **Transfer:** parquet's encodings beat unoptimized .facetful (1.62 vs 1.90 MB); the measured encoding improvements (u8 codes, narrow ints) close most of that gap without breaking zero-decode.

### Chromium run (same device, finer timer)

| lane | load | interaction median | p95 | vs JS objects |
|---|---|---|---|---|
| JS objects | 441 ms | 27.20 ms | 47.0 ms | 1.0× |
| crossfilter2 | 711 ms | 4.50 ms | 7.9 ms | 6.0× |
| .facetful → wasm | 148 ms | **2.30 ms** | 3.0 ms | 11.8× |
| parquet → hyparquet → JS kernels | 109 ms | 2.80 ms | 4.0 ms | 9.7× |
| DuckDB-WASM | 9,153 ms | 55.70 ms | 67.4 ms | 0.5× |

Cross-browser synthesis: the wasm engine and the JS-typed-kernel lane are parity-class (within ±20% both ways — wasm ahead 1.2× in Chromium, quantization-tied in Firefox); hyparquet's load edges ahead at this scale; DuckDB is consistently ~20× behind on interactions and 60-90× on load. The engine-language evidence stands as measured in lang-bench: wasm's decisive edges (GROUP BY, SIMD, tails) live outside this kernel.

**Gate verdict: proceed** — with the review's Option C sharpened by evidence: parquet-in via hyparquet is a first-class *source* at small scale, not just a compatibility adapter; `.facetful` must earn its keep at scale and on the range/OPFS paths, which the next experiments measure before the SQL layer is built. DuckDB is no longer the competitor to watch; careful JS over a good representation is. (DuckDB fairness note: batching its 8 queries could improve it, but per-query round trips are how it is actually used from JS.)
</sv-prose>

<sv-prose id="d12">
## M1 spike — 1M-row scale run (2026-08-31) and gate closure

**Firefox, 1M rows:**

| lane | transfer (gz) | load | est. cold @4G | median | p95 | vs JS objects |
|---|---|---|---|---|---|---|
| JS objects | 12.92 MB | 1,264 ms | 13.3 s | 160 ms | 257 ms | 1.0× |
| crossfilter2 | 12.93 MB | 3,568 ms | 15.6 s | 31 ms | 77 ms | 5.2× |
| .facetful → wasm | 9.36 MB | **185 ms** | 8.9 s | 14 ms | 18 ms | 11.4× |
| parquet → hyparquet → JS kernels | 7.85 MB | 863 ms | 8.2 s | 11 ms | 16 ms | 14.5× |
| DuckDB-WASM | 20.80 MB | 10,055 ms | 29.4 s | 221 ms | 286 ms | 0.7× |

**Chrome, 1M rows** (no DuckDB): facetful load **164 ms**, median **11.2 ms**, p95 15.0; hyparquet load 567 ms, median 14.6 ms, p95 20.1; crossfilter 23.9 ms; JS objects 142.9 ms.

### What scale proved

1. **Zero-decode load is real and grows with data.** facetful's load is *flat* (161→185 ms Firefox, 148→164 ms Chrome going 200K→1M) while every decode-at-load lane scales linearly: hyparquet 5× (→863/567 ms), JS objects →1.3-1.8 s, crossfilter →2.1-3.6 s. At 1M rows facetful opens **3.5-5× faster than the parquet path** and the gap widens with N. This was the format's core claim; it is now measured.
2. **Interactions: parity-class, browser-split, wasm never behind by much.** Chrome: wasm 1.3× ahead (11.2 vs 14.6 ms). Firefox: JS kernels slightly ahead (11 vs 14 ms). Both stay comfortably interactive at 1M; crossfilter (24-31 ms, p95 46-77) and JS objects (~150 ms) drop out of the fluid range. Wasm's GROUP BY/SIMD headroom remains unspent.
3. **DuckDB at 1M:** 221 ms per facet refresh, 10 s load, 29 s cold @4G — behind even the objects baseline. Reference point settled at both scales.
4. **Transfer still favors parquet** (7.85 vs 9.36 MB gz) until the measured encoding work (u8 codes, narrow ints, Decimal) lands — expected to roughly close the gap.

### Gate: proceed — engine proven, publishing-format claim still open

*(Verdict corrected after the second review audit — the original "outcome 1, gate closed" overstated it.)*

What is proven: **the engine, the representation, and the fused faceting algorithm** (3 ms vs 28 ms objects / 6 ms crossfilter at 200K; still interactive at 1M; 19.7 KB gz engine), and **structural open-time advantage** (flat vs linear in N). What is *not* yet proven: `.facetful` as the preferred **cold-network publishing format** — by this page's own est.-cold metric, parquet+hyparquet currently wins end-to-end (1.68 vs 1.93 s @200K, 8.2 vs 8.9 s @1M): the transfer surplus cancels the open advantage on 4G. The compact encodings (u8 codes, narrow ints, dict-once, Decimal) are projected to close the transfer gap; if they do, the faster open wins end-to-end — measure, don't assume.

**Benchmark qualifications from the audit** (recorded so the numbers aren't over-read): facetful's ~11-22 ms first-query cost was hidden by warm-up (conclusion survives); the hyparquet lane is a *simple* adapter — a column-chunk version cut its Node ingest ~16%, so the 3.5-5× open gap is vs a naive adapter, not the best plausible one; memory is unmeasured (prototype holds file + segment copies + query vectors); the dataset is synthetic, non-null, dictionary-friendly, single-value-per-facet; DuckDB is a fair product reference, not a neutral compute comparison; and hyparquet does have range/chunked capabilities in principle — facetful's range/OPFS claim is "simpler and faster", which must be shown, not asserted.

**Next gate (entry criteria for building the SQL layer, folded into M2):**
1. Implement compact encodings; re-run **one end-to-end metric: network + open + first correct facets** (first query included, no warm-up shield).
2. Benchmark an optimized hyparquet **column-chunk** adapter as the fair rival.
3. Measure peak + steady memory per lane.
4. Real map dataset, with nulls and **true multi-value facet selections** (engine gains per-dim code *sets* — the current i32-per-dim API is single-select).
5. HTTP ranges + OPFS exercised before calling those differentiators proven.

Even in the worst case — parquet remains the best initial source — the product shape stands: Parquet as first-class input, `.facetful` as the fast browser execution/cache representation.</sv-prose>

<sv-prose id="d13">
## M2 progress: compact encodings landed (2026-08-31)

Format + CLI + engine now implement **u8 dictionary codes** (cardinality ≤ 256), **narrow integer widths** (Int8/16/32 inference), and **dictionary-once** (a file-level dictionary block after the header replaces per-group dictionary copies; dict columns carry only codes per group). All workspace tests + the five-lane correctness smoke pass; engine wasm still 6% of budget.

| | raw | gz | parquet gz | end-to-end cold @4G (est.) |
|---|---|---|---|---|
| 200K rows | 5.72 → **3.83 MB** (-33%) | 1.97 → **1.82 MB** | 1.64 MB | facetful ~1.73 s vs parquet+transcode ~1.65 s |
| 1M rows | 28.5 → **19.0 MB** (-33%) | 9.80 → **9.07 MB** | 8.18 MB | facetful ~8.2 s vs parquet+transcode ~7.9-8.2 s |

Side effects measured (Node): open time dropped 174→100 ms (less data to touch) and interactions 2.44→2.30 ms (denser codes). **The end-to-end cold gap is now a statistical tie** — parquet's remaining ~10% transfer edge is almost entirely the Float64 measure column (raw f64 bits gzip poorly). The two levers that would flip it, both deliberately parked: `Decimal(scale)` storage for fixed-decimal measures, and per-segment deflate (measured earlier at near-parity with whole-file gzip). Meanwhile facetful keeps the flat open (and with OPFS persistence, repeat visits skip the network entirely), the better p95, and the range/OPFS paths to come.

Remaining M2 gate items: multi-value facet selections (per-dim code sets), optimized hyparquet column-chunk rival, single first-query-included cold metric in the bench, memory measurement, real dataset.
</sv-prose>

<sv-prose id="d14">
## M2 progress: multi-select facets + fair-rival lanes (2026-08-31)

**Multi-value facet selections** are now the workload everywhere: the engine takes per-dim *code sets* (membership tables, one byte per code — the fails-counting algorithm unchanged), the wasm ABI passes per-dim lens + concatenated codes, and the benchmark script toggles values in per-dim sets (up to 4 per dim, occasional clears) like a user working facet checkboxes. Correctness re-verified across all lanes against naive recomputation and each other, including everything-selected edge cases.

**First multi-select numbers (Node, 200K)** — the workload change redraws the field:
- **crossfilter2 collapses: 5.5 → 37 ms** — multi-select forces its `filterFunction` path (a per-row JS predicate), forfeiting the sorted-index advantage that made it the 200K specialist. The "strongest specialist" from the single-select rounds is not one under real facet semantics.
- facetful: 3.3 ms (from 2.3 — membership indirection costs a little); hyparquet→JS kernels: 4.4-4.5 ms — **the wasm engine now leads best-case JS by ~1.4× on the real workload**, consistent with the language-bench prediction that JS parity was kernel-specific.
- The **optimized column-chunk hyparquet adapter** (the review's fair rival — no row objects) is implemented and verified: ingest 176 → 127 ms at 200K (~28% faster than the objects path); interaction identical, as expected (same executor).

**Benchmark honesty upgrades**: lanes are now benchmarked *before* correctness verification so each lane's **first query is honestly cold** and reported as its own column; est. cold @4G = transfer + load + first query (the review's single end-to-end metric); DuckDB uses `IN` lists for multi-select.

Remaining M2 gate items: memory measurement, real map dataset (with nulls).
</sv-prose>

<sv-prose id="d15">
## M2: 1M-row multi-select results (2026-08-31, Firefox + Chromium)

**Firefox, 1M rows, multi-select:**

| lane | transfer | load | first query | est. cold @4G | median | p95 |
|---|---|---|---|---|---|---|
| JS objects | 12.92 MB | 1,335 ms | 172 ms | 13.6 s | 137 ms | 444 ms |
| crossfilter2 | 12.93 MB | 3,569 ms | 392 ms | 16.0 s | 66 ms | 127 ms |
| .facetful → wasm | **8.63 MB** | **112 ms** | 39 ms | 8.19 s | **15 ms** | **20 ms** |
| hyparquet (objects) → JS | 7.85 MB | 810 ms | 14 ms | 8.14 s | 14 ms | 19 ms |
| hyparquet (chunks) → JS | 7.85 MB | 530 ms | 14 ms | 7.86 s | 14 ms | 19 ms |

**Chromium, 1M rows, multi-select:** facetful 106 ms load / **14.4 ms** median / 19.8 p95 (8.19 s cold); hyparquet objects 17.1 ms / chunks 19.5 ms medians (7.8-7.9 s cold); JS objects 165.7 ms; **crossfilter 191 ms — behind even the objects baseline**; DuckDB 415 ms median, 21.4 s cold.

### Synthesis

1. **crossfilter is eliminated as a rival under real facet semantics** — 66 ms (FF) / 191 ms (Chrome) medians with 127-365 ms tails; in Chrome it is *slower than hand-written objects*. Its single-select index magic does not survive multi-select.
2. **facetful is the only lane fast in both browsers**: 14.4-15 ms medians, ~20 ms p95, near-identical across engines and scales — the wasm predictability story, measured. The JS-kernel lanes match it in Firefox (14 ms) but drift in Chrome (17-19.5 ms, p95 to 27 ms).
3. **End-to-end cold has converged**: 8.19 s vs 7.78-7.86 s @4G — a ~5% gap (parquet's remaining transfer edge is ~0.78 MB, almost entirely the Float64 column; the parked Decimal/per-segment-deflate levers would erase it). **Repeat visits are not close**: facetful re-opens in ~110 ms; the parquet path re-pays 430-810 ms of transcode every session (or persists to OPFS as .facetful — becoming facetful).
4. **DuckDB at 1M multi-select: 415 ms per interaction, 21.4 s cold** — 29× behind on interactions.

With multi-select, first-query-included cold, the fair-rival chunks adapter, and both browsers at both scales, the review's benchmark-qualification list is cleared except: **memory measurement** and **the real map dataset (with nulls)**.
</sv-prose>

<sv-prose id="d16">
## M2: nulls + memory measurement (2026-08-31)

**Nulls are in, with a deliberate split**: empty cells in *string/facet* columns are ordinary dictionary values (the "(blank)" bucket a facet UI shows anyway — no null machinery needed), while empty cells in *numeric* columns are true nulls — a validity bitmap in segment slot 2 (present only when the chunk has nulls, per spec), placeholder zeros in the data, null-skipping min/max stats, SQL-semantics `sum()` (nulls don't contribute; `count(*)` still counts the row), and nulls-last top-k. The synthetic dataset now has ~3% empty measure cells; every lane handles them (NaN-null in the JS lanes, nullable DOUBLE in the parquet file) and all lanes still agree exactly. Cost: +125 KB raw at 1M rows.

**Memory measurement** (review item): the worker now creates each lane, benches it, verifies it against facetful, then drops it — reporting per-lane JS-heap growth (Chrome only, no forced GC, labeled approximate) and the wasm lane's linear-memory size. Run in Chromium to populate the column.

Remaining M2 gate item: **the real map dataset** — everything else from the review's checklist is done.
</sv-prose>

<sv-prose id="d17">
## Product posture (adopted 2026-08-31, per third review): Parquet public, .facetful internal

| layer | choice |
|---|---|
| Public input contract | **Parquet** (and CSV via the CLI) — publishers change nothing |
| Browser execution | facetful's dictionary-coded representation (the thing every benchmark validated) |
| Persistent cache | **versioned, discardable `.facetful` in OPFS** — transcode Parquet once, reopen at ~110 ms forever after |
| Advanced/experimental API | `loadFacetful()` — direct publishing stays available, unpromised |

Rationale from the measurements: first-visit cold still slightly favors Parquet (7.86 vs 8.19 s @4G); warm interactions are near-parity (facetful more predictable cross-browser); facetful's decisive edge is *preparation* (112 vs 530 ms) — and an OPFS cache captures that edge without asking anyone to publish a second format. "Parquet repays transcode every session" only holds if you deliberately don't cache. Public-format compatibility is a promise with real long-term cost; not made yet.

**Promotion criteria** — `.facetful` becomes a supported *publishing* format only if it proves at least one advantage Parquet+cache cannot reproduce:
1. materially lower peak memory;
2. substantially faster range-based queries on large remote datasets (the M7 experiment — zero-decode ranges vs hyparquet page decode);
3. clearly better time-to-first-facets across target devices/networks;
4. genuine publisher demand for precomputed, directly executable assets.

The format implementation and CLI stay — they are the transcoder, the cache writer, and the candidate for static-hosting/offline deployments. One-line story: **"Open Parquet instantly in a tiny, purpose-built faceting engine."** (The PMTiles analogy shifts accordingly: Parquet is the tiles; facetful is the renderer.)
</sv-prose>

<sv-prose id="d18">
## Internal-representation consequences (2026-08-31)

Demoting `.facetful` to internal changes what the format is *for* — the virtues survive, the constraints don't:

- **Kept (execution properties, not transfer ones):** zero-decode segment layout (the ~110 ms reopen *is* the cache's value), row groups + min/max stats (larger-than-memory scanning, filter skipping), u8 codes / narrow ints / dict-once (memory footprint and scan speed — they improved interactions, not just downloads).
- **Dropped obligations:** forward-compatibility discipline. The OPFS cache is keyed by `(engine version, source content hash)`; any layout change silently invalidates and re-transcodes. Reserved-bit ceremony and unknown-flag rejection stay in code but stop being promises. Self-framing group headers (streaming-GET consumption of published files) are no longer sacred — kept for now, cheap.
- **Newly permitted — a cache can contain answers, not just data** (transcode-time precomputations a public format could never carry):
  1. unfiltered facet counts → first facet paint without a scan;
  2. a sort permutation for the default table column → first table render without sorting;
  3. dictionary rank arrays → `ORDER BY <dim>` without comparing strings.
- **Engine fix surfaced by this review:** `codes()` currently widens u8 codes into a `Vec<u16>` per query — a per-query decode hiding in a zero-decode engine. The executor should scan u8 segments directly.
- **Considered and declined:** Arrow IPC as the internal representation — same zero-copy philosophy plus ecosystem interop, but ours is effectively Arrow's layout plus stats/groups, already built at 6% of the size budget. Revisit only if first-class Arrow export becomes a goal.
</sv-prose>
