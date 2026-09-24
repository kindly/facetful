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

**OPFS spillover, the exact mechanism (M5)**: the file never enters wasm wholesale. The worker opens it once via `createSyncAccessHandle`; the wasm imports one host function, `opfs_read(table, offset, len, dest_ptr)`, which JS serves with a synchronous positional read into wasm memory — Rust-side it's just another `ReadAt` impl, so engine code is identical to the memory path. The spillover itself is the **segment cache turning into a bounded LRU** keyed by (row group, column) with a byte budget: miss → one read; over budget → drop the least-recently-used buffers (SELECT-only means eviction is `drop` — no dirty pages, no writeback, ever). Since scans are group-at-a-time, the cache budget ≈ peak memory; hot columns stay resident by reuse, cold ones stream through. Catalog + dictionaries always resident. Wasm footnote: linear memory never shrinks, freed buffers recycle via the allocator — so the LRU budget effectively *is* the footprint.

**Write-behind adoption — considered and scrapped (2026-09-04)**: the idea was one logical path (query the fetched buffer immediately, persist to OPFS in the background, swap sources). Scrapped because (a) no-OPFS environments (Safari private mode, restricted iframes, Node) force a real memory-only mode to exist forever anyway, so the "single path" was illusory; (b) a library silently writing gigabytes into origin storage nobody asked for is impolite — quota consumption and eviction interplay belong to the app's judgment. **Placement stays explicit**: `db.load()` = memory, `db.loadOpfs()` = opt-in persistence. The measured conclusions survive: once cached, disk-backed execution is byte-identical to resident; the disk route's whole cost is first-touch reads (~50-150 ms cold at 1M rows, single-digit ms at 200K, hideable by `warm()`); M5 still measures the real `opfs_read` round-trip first.

**Memory-vs-disk decision policy**: the risk is asymmetric — wrongly choosing OPFS costs one first-touch read per segment (the LRU keeps hot columns resident, so steady-state is near-identical); wrongly choosing resident kills the tab. Hence conservative defaults, layered:
1. `totalBudget = min(deviceMemory/4, 1 GB)` (no `deviceMemory`: 512 MB desktop UA, 256 MB mobile UA).
2. **Resident if file ≤ ~half the budget** — the other half is reserved for segment activity, aggregation state and result buffers.
3. **Forced OPFS above that**; above ~1 GB resident isn't a choice at all (browsers cap wasm growth well under the theoretical 4 GB — ~2 GB in practice — and query state needs room).
4. **Caller override always wins** (`placement: "memory" | "opfs"`).

**Cache fill is lazy, never byte-order prefill**: the file is columnar, so "the first gigabyte" is all columns of the earliest groups — no relationship to the workload. Demand-fill reads exactly what queries touch (a cold first query over a 2 GB file reads ~the referenced columns once — a few hundred ms — then it's cached), and never-referenced columns are never read. Deliberate warming has two sanctioned forms: an explicit `db.warm([cols])` that runs in the background after ready (one group per tick, so a fast first click still wins), and compiled-image **cache hints** (the AOT compiler names the hot columns); the strongest warm is the image's precomputed first-paint answers, which need zero reads.
No automatic mid-session demotion; "spill this table" would be an explicit one-shot image write + source swap. Aggregation state and results never spill in v1 — bounded by output cardinality, documented; operator spill is the hypothetical M10. In the multi-GB regime the real answer is the M7 range path — query without possessing the file. **M5 fix noted**: the memory source currently *copies* touched segments into the cache (resident cost can approach 2× file size); it should borrow aligned slices zero-copy, making resident cost ≈ file size.

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
- **Measured example — regex** (spikes/regex-bench, shipping profile): the `regex` crate is disqualifying for core at **349 KB gz** (256 KB even with unicode stripped — the meta-engine machinery, not the tables, is the bulk); **`regex-lite` costs ~26 KB gz net** (PikeVM, linear-time, no unicode property classes) and is the right engine if `regexp()` ever ships — as a **Tier-2 plug-in module**, costing core zero. Dictionary-aware predicate evaluation makes its slower matching moot on dict columns (pattern runs per dictionary entry, not per row). A ~300-LoC Kernighan-style hand-rolled subset remains the fallback below that. **LIKE-vs-regex speed, measured** (spikes/regex-bench/cmp): full regex is 5-8× *faster* than our naive LIKE matcher (15-23 ns vs 100-143 ns per string — the 349 KB buys memmem prefilters), regex-lite is 2-5× *slower* (240-576 ns, PikeVM without literal optimizations) — so LIKE-via-regex conversion is not a speed path at acceptable size. On dict columns all three are noise (0.05-1.2 ms per query over a 2,000-entry dictionary). **Long-text ("notes") fields, measured and addressed** (spikes/text-bench, 200K×~230-char notes): naive LIKE 203 ms → **literal fast paths now implemented in the engine** (`%x%`→contains, `x%`/`%x`→prefix/suffix, exact; ascii-case-insensitive; general matcher only for real wildcards): ~10 ms, 20× better, zero dependencies. The next tier is designed but not built: a **compiler-emitted stemmed token index** as an optional feature-bit image section (AOT tokenize + ~30-line suffix stemmer, lexicon + delta-varint postings; measured: ~0 ms single-term / 0.07 ms two-term-AND queries, 276 ms build, ~17-25% of corpus size). Fits the compiled-image rules exactly: heavy machinery at compile time, runtime only intersects postings behind a `match(col, 'terms')` function; rebuildable, discardable, zero cost when absent.

**Whole-table text search** needs no concatenated column — an inverted index never stores its source text, so the "everything column" exists only virtually: **one shared lexicon per table** (compiler tokenizes every searchable column), per-column posting lists sharing it (or a cheaper column-blind union list). Dict columns join nearly free: their dictionaries are their vocabularies, and a matching token resolves to a code-scan mask with no postings at all. Cross-column search is then a sorted-postings merge (microseconds), not N scans. SQL surface: `search('terms')` table-level (the global search box) and `search(col, 'terms')` scoped; later, the planner may rewrite LLM-emitted `a LIKE '%x%' OR b LIKE '%x%'` chains onto the index. Index-less fallback is already tolerable: dict columns cost ~nothing via dictionary evaluation, so global search ≈ the fast-path contains over the one or two genuine notes columns (~10 ms/200K each). **Multi-token semantics** (all built on one principle — *the index generates candidates; the scan machinery is the truth*, so index path ≡ scan path is differential-testable):
- `search('a b')` = AND of stemmed postings (both words present, anywhere) — the measured 0.07 ms merge.
- Exact phrase `"a b"` = **filter-then-verify**: postings AND → candidates (typically 1-5 % of rows) → fast-path `contains` on candidates only (~0.03 ms per thousand). No positional index; exactness comes from the verifier.
- `LIKE '%…%'` acceleration = the **safe-token rule**: only tokens bounded by separators *inside* the pattern are guaranteed whole in matching rows and may drive the candidate AND (edge tokens can be fragments); no safe tokens → the 10 ms fast-path scan is the floor.
- Arbitrary mid-word fragments at index speed = the **trigram variant** (pg_trgm design), ~2-4× token-index size, strictly a compiler-profile option.

Deliberately out: relevance ranking — this is a filter engine, not an IR system ("order by match count" is the ceiling).

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
  <a href="/web/spike-bench/index.html" target="_blank">/web/spike-bench/index.html</a>
  <div class="small text-muted mt-1">200K rows · three lanes (current-style JS objects, crossfilter2, .facetful→wasm), correctness-verified against each other before timing, all in a worker. Press "Run benchmark". Node/V8 preview: facetful 2.45ms vs crossfilter 5.5ms vs JS objects 35.7ms median per interaction. Serving from the repo via <code>python3 -m http.server 8765</code> on the lenovo box.</div>
</div>
<iframe src="/web/spike-bench/index.html" style="width:100%;height:34rem;border:1px solid #8884;border-radius:6px" title="facetful spike bench"></iframe>
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

*Superseded/refined 2026-09-01 by the [compiled-image conclusion](/p/facetful-compiled-image): Parquet = source, .facetful = rebuildable engine-ABI-keyed execution image; AOT native compiler + optional lazy browser compiler; sidecar publishing for controlled deployments; rebuild-not-migrate. Review disciplines recorded on that page.*

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

<sv-prose id="d19">
## Build log 10 — M5: OPFS spill-over landed (2026-09-04)

The mechanism recorded above is now code, in the shape the doc promised:

- **The wasm gained its first import.** `env.opfs_read(file_id, offset: f64, len, dest_ptr) → bytes_read`, served by the worker over `createSyncAccessHandle` (offset travels as f64 — exact to 2⁵³ — to keep BigInt off the boundary). The engine-side source is a two-variant enum (`Mem` / `Opfs`); `table_open_opfs(file_id, file_len, cache_budget)` reads header + dictionaries + footer through it and defers every column segment.
- **The segment cache became a bounded LRU** keyed `(row group, column, segment)` with a byte budget; eviction is a plain drop (SELECT-only — nothing to write back). Dictionaries and the catalog stay resident and budget-exempt. A counting-source test proves the budget holds, evictions re-read, and results stay identical.
- **Zero-copy fix for memory tables**: `ReadAt` grew `read_ref(offset, len) → Option<&[u8]>`; in-memory sources borrow segments straight out of the file bytes — no cache entry, no 2× resident cost. OPFS sources return `None` and use the LRU.
- **JS API**: `db.storeOpfs(path, buffer)` (explicit persistence — the scrapped write-behind stays scrapped), `db.loadOpfs(name, path, {cacheBytes})` (default budget `min(deviceMemory/4, 1GB)`), `db.warm(cols)` (worker yields between columns so queries interleave), `db.cacheStats()`. The demo page grew a three-button OPFS panel: persist the 1M image → open lazily (metadata only) → cold/warm query timings + cache stats + derived OPFS read throughput.

**The benchmark detour worth recording.** First re-run after the zero-copy change showed scan-heavy queries 2× slower (arith 10→21 ms) — stable, reproducible, and entirely fake. The chase (microbenches exonerated memcpy source and alignment; a nine-block convergence test ruled out cumulative warmup) ended at glibc: **malloc trims the heap back to the OS between queries**, so each query re-faults its lane vectors; the old copy-cache had been *accidentally* pinning the heap high-water mark with resident segment copies. `mallopt(M_TRIM_THRESHOLD/M_MMAP_THRESHOLD)` in the CLI (what sqlite/duckdb effectively do via their own buffer managers) restores everything — and reveals the M6 finals were understating us: **facet_count 3.0 ms** (was 4.7; duckdb@1 14), arith_scan 10.5, rest at parity. Wasm is immune by construction — linear memory never returns pages.

**Still to measure (needs a browser):** the real `opfs_read` round-trip. The demo panel computes it from cold-vs-warm deltas. One deployment note: OPFS requires a **secure context** — `http://localhost:8765` qualifies; the tailnet IP over plain http does not (Chromium: `#unsafely-treat-insecure-origin-as-secure`; or `tailscale serve` for real https).

Engine wasm: 126.8 KB gz — 41% of budget (+1 KB for the source enum + LRU). 37 native tests + node smoke green. Not built (unchanged plan): M7 HTTP-range source; u8 direct scan; kernel fusion for the like/arith stragglers.
</sv-prose>

<sv-prose id="d20">
## Build log 11 — function batch: 14 scalars, 3 aggregates (2026-09-05)

Exercise in "how easy is a new function": the registry design held up — a typical scalar was one `FUNCS` line + one `scalar_fn` match arm, and it works everywhere (WHERE, projections, GROUP BY keys, over aggregates) because the cold vector path wraps any scalar automatically, nulls included.

**New scalars** (all SQLite-compatible): `trim`/`ltrim`/`rtrim` (1- and 2-arg — default trims *spaces only*, SQLite semantics), `replace` (empty needle returns input unchanged), `instr` (1-based, character-counted), `nullif`, `ifnull`, `sign`, `sqrt`, `exp`, `ln`, `pow`/`power` (out-of-domain → NULL, matching SQLite). One new signature shape (`NumericToInt` for `sign`).

**New aggregates**, one `AggAcc` variant each: `median` (keeps values per group, selects at finish — memory is O(group rows), fine for the target sizes), `stddev` (sample, n−1, via n/Σx/Σx² — n<2 → NULL), `group_concat(x[, sep])` (separator must be a text literal, enforced at bind with a proper caret diagnostic; element order = scan order, which matches SQLite's insertion order on identically-loaded data — the differential suite proves it).

Costing check against the estimate: the whole batch — 17 registry lines, ~120 lines of exec, 6 test functions, 4 differential rows — was about an hour, and **43 tests + a 19-query SQLite differential** all pass. Size cost: +7 KB gz (133.9 KB, 43% of budget) — median/stddev/group_concat and the text scalars monomorphize through the agg loops, acceptable.

Still absent by decision, not difficulty: date/time functions (blocked on picking the Date/Timestamp encodings — the format has the types, nothing produces or consumes them yet) and the JS UDF trampoline (designed, unbuilt).
</sv-prose>

<sv-prose id="d21">
## Build log 12 — loadParquet: the headline path exists (2026-09-06)

*"Open Parquet instantly in a tiny purpose-built faceting engine"* is now a real API, and the browser compiler is not a second implementation:

- **The CLI's convert logic moved into `facetful-format::compile`** — one baseline compiler (type narrowing, dict-vs-plain, u8 codes, validity, group slicing) behind both `facetful convert` and the wasm exports. The refactored CLI produces a **byte-identical** image to the pre-refactor binary on the 200K CSV, so nothing drifted in the move.
- **Wasm compile ABI**: `compile_begin/add_num/add_text/finish` + image handles; JS marshals raw typed columns in (f64 lanes + validity bytes; offsets + UTF-8 blob for text), the image opens in place with no copy back out. Int64 crosses as f64 (exact to 2⁵³) — a documented baseline-profile limit the native CLI doesn't share. +12 KB gz (145.7 KB, 47% of budget).
- **Worker pipeline**: hyparquet (lazy `import()`, URL injectable — bare `"hyparquet"` for bundlers) → `parquetToColumns` (a shared, environment-free module) → wasm compiler. Physical parquet types decide int-vs-float (a DOUBLE of integral values stays Float64, matching the CLI); DATE/TIMESTAMP become epoch-ms ints until date functions land; nested columns are a clear error.
- **The cache flow is the product story**: `db.openParquet(name, buffer)` hashes the source (SHA-256), looks for `facetful-cache/<hash>-v<version>.facetful` in OPFS — hit: zero-decode lazy reopen through the M5 LRU machinery; miss: transcode, open in memory, persist in the background. Non-secure contexts degrade to memory-only transparently. `db.loadParquet` is the transcode-only variant.
- **Correctness is a differential, per the compiled-image doc**: a new headless test (`node-parquet-diff.mjs`) pushes the real 200K Parquet through the exact worker code path and requires cell-for-cell agreement with the CLI-built image across 7 queries (nulls, dict columns, median/stddev included). Green, alongside the 43 Rust tests and the 19-query SQLite differential.

The demo grew a Parquet panel: open the 1M parquet → first visit reports transcode+persist time; reload and click again → "CACHE HIT" with the reopen milliseconds. That side-by-side is the pitch in one screen. Node transcode of 200K runs ~1s (hyparquet decode dominates); browser numbers await the same click-through as the M5 panel.

Not in scope, unchanged: nested/decimal parquet columns (clear error), streaming group-at-a-time compile (whole columns are resident during transcode — fine at 5M target), fetch-by-URL sugar.
</sv-prose>

<sv-prose id="d22">
## Build log 13 — temporal columns: the last SQL gap closes (2026-09-07)

**The encoding decision** (the part that was blocking): `Date` = days since 1970-01-01, i32 on disk; `Timestamp` = milliseconds since epoch, i64, UTC — the exact value JS `Date.getTime()` produces, so the wasm boundary converts nothing. Calendar math is Hinnant's civil-days routines (~30 lines, `facetful-format::time`, proleptic Gregorian, unit-tested through negative days and century leap rules).

**The type-system move**: `Ty::Date`/`Ty::Timestamp` are *ints with meaning* — `numeric()` includes them, so comparison, BETWEEN, GROUP BY, min/max, pruning and top-k all worked the moment the types existed; they coerce to Int/Float one-way. The audit that mattered was the wildcard `match ty` arms (a Date column was about to fall into `lanes_to_vv`'s *text* arm). `min(d)` keeps its temporal type through aggregation, and `QueryResult` now carries `col_types`, which is how date-ness survives to the boundary.

**Functions**: `year/month/day` (Date or Timestamp), `hour/minute/second` (Timestamp), `date()`/`timestamp()` constructors (ISO text, raw ints, or each other — so `where d >= date('2020-01-15')` reads naturally), `strftime` (%Y %m %d %H %M %S %s %%). The one design wrinkle — days-vs-ms share `Val::Int` — is resolved at the *bound-type* level: temporal calls dispatch on their argument's `Ty`, in both the vector path and the grouped-expression path (`year(min(d))` works). Misuse gets caret diagnostics with hints.

**Ingest, all three doors**: CLI infers strict ISO `YYYY-MM-DD` / datetime columns (after int/float, before text — a 3-row CSV with a date column now stores i32 days and prints ISO in the REPL, nulls as blanks); Parquet DATE/TIMESTAMP logical types map to real temporal columns in the browser compiler (days from hyparquet's UTC-midnight Dates, ms otherwise); the compile ABI grew a kind tag (float/int/date/timestamp). Result marshalling: wasm kinds 5/6 → JS materializes `"YYYY-MM-DD"` / `"YYYY-MM-DD HH:MM:SS"` strings in `column()`/`rows()` while `columnRaw()` keeps the raw f64 days/ms lanes for charting.

49 tests green (4 new temporal exec tests incl. type errors; time-module unit tests), node smoke has a temporal round-trip, parquet differential unchanged. Wasm 48% of budget (+1.5KB). Not differential-testable against SQLite (it has no date column type — our `year(d)` vs their `strftime('%Y', text)` isn't the same SQL), so correctness rests on the civil-math unit tests + end-to-end exec tests. Not done, by choice: date arithmetic modifiers (SQLite's `'+1 month'` strings), timezone handling (everything is UTC), pruning through `date()` literals (needs const-folding — noted).
</sv-prose>

<sv-prose id="d23">
## First browser numbers: M5 + the parquet path (2026-09-07, David's run, Firefox)

| measurement | number | reading |
|---|---|---|
| `opfs_read` throughput | 64 segments / 10.1 MB in ~5 ms ≈ **2.0 GB/s** (~78 µs/segment round-trip) | the import + sync-handle path costs nothing that matters; matches the literature's tens-of-µs claims |
| OPFS lazy open, 1M rows | **13 ms** (metadata only, 19.2 MB file) | vs ~110 ms whole-image reopen recorded in M2 — ~10x better repeat-visit open |
| cold vs warm query @1M | 28 ms → 23 ms | lazy-disk penalty on a first-touch query ≈ 5 ms — **OPFS spill-over is viable as a default**, not just an escape hatch |
| parquet transcode @1M (browser) | 2314 ms, cached to OPFS | once per file; consistent with node's ~1 s @200K (hyparquet decode dominates) |
| first query after transcode | 32 ms | 1M in-memory wasm, expected range |
| OPFS write, 19.2 MB | 76 ms | persist cost is trivial next to fetch (1844 ms on the tailnet link) |

| **CACHE HIT reopen** (reload + click) | **43 ms** @1M (fetch 24 ms from HTTP cache; first query 40 ms) | **2314 ms → 43 ms = 54x repeat-visit win, zero transcode** — the product pitch, measured. ~30 ms of the 43 is SHA-256 over the 19 MB source for the cache key (the lazy open alone is 13 ms); keying by (URL, ETag) later would reclaim most of it |

**Chromium (partial, 2026-09-07):** transcode **7063 ms** @1M (3x slower than Firefox's 2314 — identical wasm, so the gap is the JS half: hyparquet decode + TextEncoder loops under V8; once-per-file, not chased). First query 23.7 ms (faster than Firefox's 32). OPFS write 54 ms. Chromium CACHE HIT: **20.9 ms** @1M (7063 → 20.9 = **338x**; first query 30.4 ms). Firefox re-ran at 41 ms (consistent with its 43; a mislabeled paste briefly attributed it to Chrome). Chrome reopens ~2x faster — likely quicker `crypto.subtle` SHA-256 over the 19 MB source plus a faster OPFS open. The verdict across both: **transcode once (2.3–7.1 s, browser-dependent), reopen at 21–43 ms forever, query at 23–40 ms.** Optional remainder: Chromium's OPFS-panel cold/warm line (Firefox already established the ~2 GB/s read path).
</sv-prose>

<sv-prose id="d24">
## The real dataset arrives (2026-09-07)

David shared the actual workload — a public energy-infrastructure tracker release (August 2026): **183,125 units × 52 columns** (xlsx → `data/units-2026-08.csv` → `data/units-2026-08.facetful`). The long-awaited validation against reality:

**Inference judged all 52 columns correctly, unassisted.** Facet dimensions (Type, Country, Region, Status, Technology, Location accuracy…) → u8 dict codes; high-cardinality entities (Owners, Operators, Cities, subnational units) → u16 dicts; unique IDs, URLs and plant names correctly *rejected* from dictionary encoding (plain utf8); Start/Retired year → int16; Capacity/Lat/Long → float64. Convert: **1.7 s** for the 67 MB CSV → 50.4 MB image, **9.4 MB gzipped transfer** for the entire 52-column dataset.

**Native interaction speeds on real data** (--bench medians @183K):

| query | ms |
|---|---|
| facet counts by Type (+ capacity sums) | 1.1 |
| filtered facet (Type IN … AND Region) | 1.0 |
| country top-20 by capacity | 1.5 |
| Region × Status pivot (CASE sums) | 3.7 |
| median + stddev capacity by Type | 2.8 |
| Start-year histogram | 6.6 |
| detail page (filter + sort + 50 rows of names) | 13.4 |
| owner LIKE contains (u16 dict, ~60K entries) | 27.1 |

Facet refreshes at **1 ms native** — the map UI's whole interaction loop fits inside a frame with room to spare. Sanity checks against the published summary figures pass (103,940 utility-scale solar units; an owner search finds 248 units across 6 countries).

**What the real data teaches:** (1) the sparse per-technology columns ("… (hydropower only)", "… (oil/gas only)" — 20+ of them, mostly null) are exactly the shape the hstore/map-type discussion anticipated; dict-encoding absorbs them fine at this scale, so the map type stays unbuilt-by-choice. (2) The owner LIKE at 27 ms is the one interaction worth watching — dictionary-aware LIKE already saves it (evaluating ~60K entries once, not 183K rows), and the designed token index would cut it further if it matters in practice. (3) No date columns in this export (years are ints) — temporal machinery unexercised by this dataset.

The demo grew a "load real data" button (fetches the image, retargets the SQL box with a real facet query) — browser numbers to follow from David's clicks.
</sv-prose>

<sv-prose id="d25">
## The real dataset in the browser: the fetch-once story on real data (2026-09-07)

Firefox, 183K × 52 real image (50.4 MB): first visit **fetch ~2.5 s + persist+open ~110 ms** (reproduced on a verified-cold run: 2525 + 115 ms; the initially recorded 7.1 s was an outlier — likely a cold tailscale tunnel — so Firefox and Chrome's 1.9 s are close, not 3x apart); every visit after — **reopened from OPFS in ~0 ms** (sub-millisecond timer resolution; no fetch, segments lazy), queries live immediately. Open was 16 ms in both browsers before persistence landed. The uncompressed 50 MB fetch is the only slow part; a compressing file server would ship the 9.4 MB gz instead. Deployment shape confirmed: **users pay one fetch per data release; the map is instant every day after.**

David's clicking also flushed out two real bugs, both fixed: (1) deleting or overwriting an OPFS path failed while a sync-access handle was open on it — the worker now closes its handles for a path before `removeEntry`/rewrite; (2) reopening a path already open in the same session hit our own exclusive lock — the worker now shares one handle across tables on the same path (positional reads are stateless). The demo grew "Forget stored copy" next to load-real-data for genuine cold-run testing.
</sv-prose>

<sv-prose id="d26">
## Build log 14 — select * + the limit-only fast path (2026-09-07)

David hit two things on the real 52-column table, both now fixed:

**`select *` didn't exist** — the binder's error message promised star-as-select-list but nothing expanded it. Now a `*` item expands to all columns in schema order (alone or beside expressions, SQLite-style), with GROUP BY validation applied per expanded column — which exposed a latent bug: that validation zipped the *unexpanded* AST list against the bound list, silently checking only the first item. Two new SQLite-differential rows prove expansion order.

**`select * from t limit 1` took ~1 s** — three stacked costs, each fixed:
1. *No limit-only early exit*: every group's every row was projected into output Vals (183K × 52 ≈ 9.5M values) before LIMIT applied. Now a scan cap (no ORDER BY, no aggregates) stops the scan the moment offset+limit rows exist.
2. *Eager full-width loading*: all 52 columns' lanes materialized per group before the filter even ran. Now loading is two-phase — filter columns first, mask evaluated, and a group that yields nothing (or a query past its cap) never touches the projection columns. Lane accessors take a row cap, so a limit-1 query materializes one row's worth of strings, not 65K. Bonus: the wide detail-page query dropped 13.4 → 8.8 ms.
3. *Per-query dictionary rebuild* (the sleeper): `Table::dictionary()` cloned the whole `Vec<String>` and exec re-wrapped every entry in `Rc` — on the real dataset that re-allocated ~20 dictionaries (Owners ≈ 60K strings) *every query*, a flat ~10 ms tax on everything. The Rc form is now built once and cached on the table.

Result: `select * limit 1` warm = **0.05 ms** (was ~10 ms warm / ~1 s cold-in-browser); filtered variant 0.19 ms. Cold one-shot ≈ 26 ms, all of it genuine one-time dictionary decode. No regressions: 50 tests, both differentials, synthetic + the real dataset benches unchanged (facet 1.2 ms, facet_count 3.0 ms). Wasm 49%.
</sv-prose>

<sv-prose id="d27">
## Build log 15 — top-k learns true late materialization (2026-09-07)

David: `select * from t order by "Country/area" limit 1` took 305 ms in the browser. Not the sort — the bounded top-k was already O(n) with cheap rejects — but the scan **eagerly evaluated all 52 select columns for every group** (strings included) so it could "late-project" winners at the end. Late projection was deferring only the output rows, not the lane loads.

Now the scan loads **only the ORDER BY (and filter) columns** — for this query, one u8-code lane — and after the winners are known, only the winning groups' select lanes load (capped at each group's deepest winner row) for projection. Native: 305 ms-class → **2.6 ms**; capacity-desc top-10 over all 52 columns 17 ms; the the real dataset detail-page query dropped again, 8.8 → **4.9 ms** (13.4 before the limit-path work started). 50 tests + both differentials green; synthetic bench unchanged; wasm 49%.

The pattern now holds everywhere it can: aggregation touches only aggregate+group columns, limit-only touches projection lanes up to its cap, top-k touches order keys then winners. The remaining known wart in this area: ordering by a dict column compares strings per candidate rather than precomputed dictionary ranks — noted, cheap, not yet needed.
</sv-prose>

<sv-prose id="d28">
## M7 reframed: ranges and segment compression are one feature (2026-09-07, discussion with David)

David's skepticism, quantified on his own data, demoted plain HTTP-range reads: the real dataset's facet working set is ~3 MB of the 50 MB image, but ranges fetch raw bytes while whole-file transfer gets gzip (9.4 MB) — so ranges alone win only ~2-3x, on the first visit only, since OPFS persistence eliminates every later fetch. Not worth a milestone for the target workload. His counter-observation completes the picture: **per-segment compression restores the win** — compressed range fetches of just the hot columns, which is Parquet's actual architectural trick and was always the substance of the promotion experiment.

Design position recorded for when a real oversized/many-dataset use case appears — **M7 = ranges + per-segment compression + request coalescing, one package**:

- **Zero-decode survives precisely**: decompression happens at cache-fill, not read — compressed bytes land from the network, inflate once into the LRU, and the executor reads today's aligned layout unchanged. The sync `ReadAt` never sees a codec. Differentiator vs Parquet narrows but holds: we decompress *into* the execution format; Parquet decompresses and then still decodes (bit-unpack, dict rebuild, arrow).
- **Zero-wasm-bytes codec split**: range fills run on the JS side, so the browser decompresses with native `DecompressionStream('deflate-raw')`; the compressor lives only in the native CLI compiler, where compiled-image doctrine already permits dependencies. The runtime binary does not grow.
- **Format cost**: per-segment compressed+uncompressed lengths, per-column codec flag — a LAYOUT_ABI bump, routine under rebuild-not-migrate. (The v1 `COMPRESSED` reserved flag anticipated this.)

For the map-class workload the standing answer is: compressed whole-file transfer + OPFS persistence + (optional) compiler-embedded first-paint facet counts — the "cache can contain answers" item, which attacks first-visit latency harder than ranges would.
</sv-prose>

<sv-prose id="d29">
## Joins position (recorded 2026-09-10, not yet scheduled)

Assessment from the 2026-09-07 discussion — real `JOIN`/`LEFT JOIN` support, honestly costed against this engine's architecture:

- **Parser** (~1 day): FROM-list with aliases + ON expressions — trivial for the Pratt parser.
- **Binder** (~1–2 days): `Bound::Column` gains a table slot; qualified names, ambiguity diagnostics, two-schema did-you-mean. Wide but mechanical.
- **Executor — the decision that matters**: the engine's speed rests on the positional model (row = position in one group of one table; every mask/gid/lane assumes it). **Many-to-one joins preserve it**: per group, hash-probe the dim key once into a `dim_row_id` lane, then gather each referenced dim column into a fact-positional lane — after which the entire existing executor (filters, aggregates, direct grouping, top-k, two-phase loading) runs untouched. Dict dim columns gather as codes + dictionary, so joined attributes facet at native speed; a dict-encoded fact key collapses the probe to once-per-dictionary-entry (the "dictionary join" falls out as a special case, so no separate `lookup()` sugar is needed). LEFT vs INNER = validity bits vs keep-mask; multi-way chains compose.
- **The restriction that keeps it tractable: unique right-side join keys, enforced with a clear diagnostic.** Non-unique right sides mean row expansion — output cardinality ≠ fact cardinality — which breaks every positional structure and is the road to a general (DuckDB-sized) executor. Declined, permanently or until a real workload demands it.
- Plumbing: multi-table query ABI across the wasm boundary (~½ day); differential testing extends directly (SQLite has joins; the the real dataset regions sheet is the ready-made second table); ~+10 KB gz.
- Accepted losses: no min/max pruning from dim-side predicates; dim key-hash cached per table pair.

**Estimate: a focused week for honest restricted SQL joins.** Replaces the earlier tier-2 `lookup()` idea. Compile-time denormalization (`convert --join`) remains the zero-runtime-cost alternative for publish-time flattening.
</sv-prose>

<sv-prose id="d30">
## Build log 16 — packaging: facetful becomes an npm package (2026-09-10)

The research phase closed; the first productization step is done. `js/facetful` is now a publishable package:

- **`facetful-0.1.0.tgz`, 164 KB** — nine files: the JS API (`index.js` + full TypeScript definitions), `core.js`/`worker.js`/`parquet.js`, the optimized wasm (49% of budget), README, MIT LICENSE. `exports` map exposes `.`, `./core`, `./worker`, `./parquet`, and the wasm asset; worker and wasm resolve via `import.meta.url`, so a bundler needs no configuration.
- **hyparquet is an optional peer dependency**, dynamically imported only when a Parquet method is called — non-Parquet users ship zero extra bytes.
- **`scripts/build-package.sh`** is the release gate in one command: size-budget check (wasm-opt when present) → node protocol smoke → parquet-path differential → `npm pack`. A consumer-style test runs against the *extracted tarball* (not the repo layout) and reproduces real-data results (183,125 rows, correct facet counts), so what's shipped is what's tested.
- README documents the three ways in (openParquet / load / loadOpfs), the dialect surface, the secure-context caveat, and the 2^53 int64 transcoder limit.

Not yet done, deliberately: actual `npm publish` (name was verified free in the naming round; publish when the Svelte app consumes it and shakes the API), git init / repo rename (`~/projects/browserdb` → facetful), CI. **Next: scaffold the new Svelte app** (name candidates offered: the explorer app, (app name candidates)) — SvelteKit static + facetful + Observable Plot (decided; TanStack Charts assessed 2026-09-10: promising Plot-derived grammar with a Svelte host, but alpha/0.x — revisit at 1.0; the panel→chart compiler stays isolated so the swap is contained; TanStack Table v9's Svelte-runes adapter is the grid-panel candidate).
</sv-prose>

<sv-prose id="d31">
## The explorer app: the first consumer, and the deployment pattern (2026-09-10)

The Svelte explorer app (separate repo — SvelteKit static + facetful-from-tarball + Observable Plot) went from scaffold to working dashboard in a day: four sortable/scrollable facet panels, two filter-reactive charts, a top-100 units grid where every column sort is a fresh bounded top-k over the whole filtered set, URL-param filter state, and a data-source footer with reset. Package shakedown found and fixed two real bundler bugs (worker must be a literal `new Worker(new URL(...))` expression; hyparquet import made statically resolvable).

**The transfer question closed with the gzip-sidecar pattern** predicted in the M7 discussion: publish `data.facetful.gz` next to the raw file, fetch it, inflate with native `DecompressionStream('gzip')`, store decompressed in OPFS. Works on any static host (no server-side compression of 50 MB binaries needed). Measured end state on the real 183K×52 dataset: **first visit ≈ 0.2 s fetch (9.4 MB) + inflate + persist; every later visit ≈ 0 ms open, no fetch.**

Measurement post-mortem: David found his NIC's powersave throttling transfers — fixing it (plus gzip) collapsed fetch times and retroactively explains every earlier fetch anomaly (Firefox's 7.1 s "outlier", the apparent FF-vs-Chrome 3x transfer gap). Engine-side numbers were never affected.
</sv-prose>

<sv-prose id="d32">
## Build log 17 — three profiling items from the app-side agent (2026-09-10)

The app-side agent profiled three engine shapes; all three fixed, measured on the real dataset 183K native:

| shape | before | after | how |
|---|---|---|---|
| full-table ORDER BY (name by capacity) | 65 ms | **9.2 ms** | deferred sort: refs + **packed order-preserving u64 keys** (float-bits trick, validity byte first so NULLs order like `cmp_sql`, DESC = bit-flip), `sort_unstable` on (key, ref) tuples; text keys keep a comparator path; projection happens once, after |
| `cast(substr(id, 2) as int)` | 47 ms | **4.6 ms** | vectorized `substr` (char-boundary byte slicing; per-dictionary-entry for dict columns) + vectorized `int`/`float` casts over Text/Codes lanes, plus a **fused blob path**: `int(substr(col, lit))` parses digits straight out of the column's offsets+bytes with zero intermediate allocation |
| 3-float-column projection | 8.7 ms | **2.0 ms** | **columnar output channel**: `QueryResult.cols: Option<Vec<OutCol>>` — the plain and full-sort paths gather typed vectors (direct text columns gather blob slices, never touching `Rc<String>` lanes); wasm `columnize` moves them into ColBufs near-memcpy; row-based consumers call `ensure_rows()` (CLI, tests, differential — 6 sites) |

Side effects, all good: text projection without sort 19 → 2.8 ms; sorted text projection 43 → 7.7 ms; synthetic `like_scan` halved again (6.5 → 3.35 ms — the blob scan reaching it). Zero regressions across both sweeps; 51 tests + both differentials green. Wasm 168.8 KB gz (54% of budget, +5 pts for the new kernels — the fair price for the batch).

New tarball in `dist/` — the app-side agent should reinstall (`rm -rf node_modules/facetful node_modules/.vite && npm i ../browserdb/dist/facetful-0.1.0.tgz`).
</sv-prose>

<sv-prose id="d33">
## Build log 18 — published: github.com/kindly/facetful + facetful@0.1.0 on npm (2026-09-11)

The two "deliberately not yet" items from build log 16 are done:

- **Scrub first**: all work references removed from the tree *and* the unpushed history (dataset renamed `data/units-2026-08`, demo page neutralized, docs generically reworded); author email kept by decision. History rewritten via soft-reset + re-commit before anything left the machine.
- **GitHub**: pushed to `github.com/kindly/facetful` — **private for now**. Root README rewritten from the stale M0 stub to the shipped-engine state (numbers table, quickstarts, workspace map, status).
- **npm**: `facetful@0.1.0` published (automation token, 2FA bypass; token file deleted after). Tagged `v0.1.0`. Release recipe from here: bump `js/facetful/package.json` → `./scripts/build-package.sh` (the full gate) → publish → tag.
- Open loose end: the npm page links to the still-private repo (404 for visitors). Flipping visibility is David's call, not taken autonomously.
</sv-prose>

<sv-prose id="d34">
## Filter-mask cache landed; the IN (select …) / star-schema position (2026-09-12)

**The cache** (commit `37d238b`, built in a parallel session): the executor splits WHERE into top-level AND conjuncts and caches each conjunct's pass bitmap per row group on the table (`Table::masks`, byte-bounded LRU, 16 MB default ≈ 25 whole-table masks at 5M rows). A query composes its mask from cached conjuncts with bitwise AND and evaluates only the misses. Bits are set only for TRUE (not NULL), so AND-composition respects three-valued semantics; negations are their own conjunct, never a complement. **LIKE needle narrowing**: a contains-needle extending a cached one on the same plain-text column verifies only the rows the cached superset admits, read through a borrowed segment accessor — no blob scan. Measured (200K × 88-byte notes): cold scan 4.8 ms, narrowed keystroke ~1–2 ms, cached repeat 0.3 ms.

Why it matters, quantified on the real dataset (183K): with a search term active, the WHERE was **~8.6 ms of every ~8.7 ms facet query — ~99%** — and a facet refresh fires 10+ queries sharing that identical WHERE (~105 ms of which ~95 ms was evaluating the same filter repeatedly). Now the filter is paid once per distinct conjunct, ever (tables are immutable — nothing invalidates, only the LRU budget evicts). A separate whole-WHERE cache tier was considered and dropped: composing cached conjunct bitmaps costs microseconds, so the conjunct tier alone captures both the repeat-identical-WHERE case and the one-conjunct-changed case (facet toggle during search, keystroke during facets).

**`IN (select …)` position** (discussion 2026-09-12; recorded, not yet scheduled):

- Today `IN` takes only a parenthesized expression list (evaluated per-distinct on dict columns); a subquery fails cleanly at parse.
- Why IN-subqueries are slow in general engines: per-outer-row hash/probe (string hash per row; SQLite adds ephemeral-index and per-row interpreter overhead), correlation machinery (proving the subquery *isn't* correlated before running it once), and NULL three-valued logic blocking clean semi-/anti-join rewrites.
- Why this engine sidesteps it: **no correlation by design** (SELECT-only, no outer references) so the inner query runs exactly once and materializes a set + had-NULL flag; **the dictionary trick** turns membership into once-per-distinct probes → a code→bool bitmap → the row scan is a vectorized integer lookup (est. 1–2 ms on the real data — cheaper than a LIKE blob scan); the same-column self-IN case compares dict codes with zero string work; and the materialized result is just another mask-cacheable conjunct.
- **Cross-table `IN (select … from other)` is a semi-join, not a join**: no columns from the second table reach the output, so none of the positional-model hazards from the joins assessment (d29) apply. Needs a catalog (multi-table query ABI across the wasm boundary — the same plumbing d29 costed at ~½ day) plus one exec arm. Days, not the joins-week; also de-risks half of restricted joins if that ever lands.

**Star schema fit**: the semi-join covers the *filter* direction — fact rows filtered by dimension predicates, including many-to-many through a bridge/exploded table (note `Owner(s)` is already comma-separated, i.e. many-to-many in disguise). It does not cover **group-by/display of dimension attributes** (membership answers yes/no, not which). Escape hatches, in order: (1) app-side rollup — `group by fk` on the fact table, decode fk→attribute in JS from the small dim table, re-aggregate; (2) the d29 restricted join (unique right key *is* the dimension lookup) when rollup hurts; (3) compile-time denormalization for facet columns known up front — under dict encoding a pre-joined attribute costs one code per row, so the classic storage argument for star schemas mostly evaporates.

**Adopted ranking**: denormalize known facet attributes at build time; add `IN (select …)` + the catalog for cross-table filters; restricted joins only when a real workload demands dimension-attribute group-bys that rollup can't serve.
</sv-prose>

<sv-prose id="d35">
## Build log 19 — bench harness, rival sweep, and the two losses fixed (2026-09-12)

**Benchmarking became regression-tested.** The mask cache silently made the old `--bench` numbers warm-only (warmups prime every WHERE mask), so: `--bench` now reports **cold and warm medians** per query; `bench/queries.sql` is the canonical 14-shape suite (every shape we ever optimized); `scripts/bench.sh` compares runs against a machine-local baseline (fail at 1.5× + 0.3 ms absolute). The mask-cache budget is configurable end to end (`--mask-cache <bytes>`, JS `setMaskBudget`, wasm export; **budget 0 = disabled**, `put` refuses entries that can never fit). The SQLite differential now runs every query cold *and* mask-cached and requires both to agree.

**Rival sweep on the real dataset** (183K × 52, Ryzen 7 5800H; `spikes/native-bench/run-real.py` + wasm lanes under the same Node V8; duckdb got ILIKE/bigint tweaks): facetful-wasm beat DuckDB-WASM on **all 14 queries** — typically 3–10× cold, up to ~175× warm (`like_2col_facet` 0.5 vs 84 ms) — at ~1/40th the download and 963 ms less per-session load. SQLite native: 26–148 ms, not in contention. Multicore verdict: 16 threads bought DuckDB only 1.4–1.7× on the expensive scans and nothing on the fast shapes (pivot got *worse*) — 183K rows can't amortize morsel parallelism, so wasm's single-thread constraint costs little exactly where facetful lives.

**The two native-DuckDB losses, fixed** (commit `2b9ab9a`):

| shape | before | after | duckdb@16 | how |
|---|---|---|---|---|
| year_hist (group by int year) | 6.7 ms | **0.9 ms** | 1.0 | direct dense grouping extended to integer/date columns with small global min–max range (footer stats); `value − min` is the code, null lane as in dict dims |
| topk (top-100 by capacity, text cols) | 12.0 ms | **1.4 ms** | 5.0 | winner phase sliced whole text lanes up to the deepest winner (~the entire 6.5 MB name blob for 100 rows); direct plain-text selects now gather per winner row off the borrowed segments |

facetful now leads every canonical shape against every rival lane, native and wasm. 61 tests + both differentials green; wasm 179 KB gz (58%).

**Addendum, same day — filter fast paths (commit `af441cf`)**: chasing the year_hist *cold* residue exposed three general wins. (1) `cmp_vec` numeric-lane-vs-literal comparisons were per-row `f64_at`/`cmp_ord` closures that defeated auto-vectorization — now specialized loops (exact i64 vs int literals). (2) `int-col <cmp> literal` conjunct masks now sweep the **raw narrow segment** (borrowed, via new `with_fixed_segments`) as an inclusive-range test — no `Vec<i64>` widening. (3) `update_batch` takes a `RowsSrc` (gids | raw keep mask): ungrouped queries never materialize gids, ungrouped `count(*)` is a popcount, and count(*)-only direct group-bys fuse counting into the gid loop. Net on the real data: **every warm filtered shape is now ~0.2 ms** (was ~0.5 floor), filtered count 1.25/0.52 → 0.72/0.20, year_hist 2.2/1.2 → **1.45/0.91** — warm now beats duckdb@16's 1.0, cold within its 1-ms timer resolution. Zero regressions across the suite; wasm 185 KB gz (60%).

Two follow-ups closed it out (`e288d85`, `afa73cf`): David asked whether the wasm *actually* contained SIMD compares — disassembly said the sweep was widening to i64x2 (2 lanes/op); **clamping the bounds to the stored width** made it compare at i16x8 (8 lanes/op; wasm year_hist 2.7 → 1.6 cold). Then bulk-measuring duckdb@1 without its 1-ms CLI timer rounding gave its true year_hist number, **1.20 ms** — and profiling our remaining cold cost found `pack_bits` (bit-indexed RMW + branch per row) cost more than the filter eval it stored; byte-parallel packing landed year_hist at **1.11 cold / 0.93 warm — ahead of duckdb on both**. Filter-only conjuncts: 0.38 cold / 0.21 warm; in_list 0.34/0.21. Lesson recorded: verify codegen with wasm-dis, and measure rivals in bulk, not through their timers.
</sv-prose>

<sv-prose id="d36">
## Build log 20 — count(distinct) was a SipHash floor, not a distinct-count floor (2026-09-18)

**Where it came from.** The PUDL explorer (two shipped images: `generator_tech_wide` 42K rows × 17 year columns, `plant_tech_year` 231K rows) had `count(distinct plant_id_eia)` as the largest single cost in every facet query — 3.6× a plain `count(*)` on the plant table — and David's note proposed two ways out: a *clustered-column* flag detected at open time (both images are written in plant order; runs = distinct values exactly) and an *array type* collapsing to one row per plant. Reading the engine first: both distinct columns are **integers** (`gen_key` is a `dense_rank()`), ints never dictionary-encode, so neither touched the 0.3.0 bitmap path (`eadc9f8`) — they went to `DistinctNum(Vec<HashSet<u64>>)`: one `std` SipHash set per group, an insert per row, ~20–33 ns/row. The measured floor was the hasher.

**What landed, in `exec.rs` (the commit carrying this entry):**

1. **`LastSeen`** — per group, the value on the previous kept row; a repeat skips the accumulator. A pure filter in front of an exact structure: it can only drop a genuine duplicate, so it is correct under any row order and only its *hit rate* depends on layout. Key-ordered images (a plant's rows contiguous) turn per-row work into per-(group, value) work: plant_tech_year grouped by fuel goes from 230,891 inserts to ~21,000. This is the clustered-column idea without the file flag, the open-time pass, or the silent cliff when someone rewrites the table in another order.
2. **`DistinctU64`** — ONE open-addressed table keyed by `(group, value)` (16-byte slot, `u32::MAX` gid as the empty sentinel since `for_kept` already skips it), linear probing, per-group counters; replaces the Vec of hash sets. 4× growth below 64K slots (rehashing dominated mid-sized counts; a measured third of the work), 2× above (memory).
3. **`FxHasher`** for the text set — the one distinct path still needing a map — with an `Rc::ptr_eq` repeat check in front.

**Two things measurement overruled.** The filter must **not** guard the dictionary bitmap: there it cost 15% on `distinct_grouped` even when it hit — the load-compare-store through one group's slot is a longer dependency chain than the `or` it saves. And `DistinctNum` is **boxed**: inline, `AggAcc` grew 80 → 112 bytes and the bitmap arms sharing the match slowed measurably.

**The bug the benchmarks hid.** First version took the index from bits 32–52 of `v × FIB`. A product bit depends only on input bits at or below it, so keys whose entropy sits above bit 52 all hash alike until the table has 2^21 slots — which is exactly what *round doubles* look like (50.0 / 100.0 / 200.0 differ only in exponent and top mantissa). `count(distinct capacity_mw)` (2,356 values) went 0.61 → **2.40 ms**, 4× *slower* than the SipHash it replaced, while every int-keyed benchmark looked great because int entropy is in the low bits. Fix: fold (`h ^= h >> 32`), multiply, take the **top** bits via a stored shift — and `(cap as u64).leading_zeros()`, because `usize` is 32-bit on wasm32. Regression test: 4096 values of `k × 0.5` must spread over >3000 home slots (old index: 32). Now 0.26 ms. Lesson recorded next to the year_hist one: **a multiply-shift hash indexes from its top bits, and a hash change is tested on float keys as well as ints.**

**Numbers** (wasm in Node, best of 15 warm, ms; `bench/pudl-*-distinct.sql`):

| plant_tech_year (231K) | before | after | | generator_tech_wide (42K) | before | after | |
|---|---|---|---|---|---|---|---|
| fuel + 1 distinct | 5.55 | **2.13** | 2.6× | fuel + 2 distinct | 3.57 | **1.56** | 2.3× |
| fuel, full facet | 6.31 | 2.82 | 2.2× | fuel, full facet | 5.77 | 4.02 | 1.4× |
| utility + 1 distinct, top 300 | 8.25 | 4.47 | 1.8× | state + 2 distinct | 3.58 | 1.50 | 2.4× |
| header totals, unfiltered | 5.13 | **0.79** | 6.5× | header totals, unfiltered | 3.58 | 0.86 | 4.2× |
| year × fuel pivot | 12.21 | 9.75 | 1.3× | utility + 2 distinct, top 300 | 6.09 | 3.94 | 1.5× |
| map, group by plant | 10.28 | 7.23 | 1.4× | technology + 2 distinct | 3.51 | 1.53 | 2.3× |

Result rows byte-identical before/after on ten cross-check queries. Dictionary-bitmap suite (`null-distinct-queries.sql`, 1M rows): 1.00× on all eight in wasm (native shows +0.2 ms/1M on the ungrouped shape from code layout; not present in wasm). 1M-row unique-int distinct: 51 → 32 ms, peak RSS 58 → 85 MB — the 16-byte slot is a deliberate memory-for-locality trade. Canonical suite: no regressions. 68 tests. wasm 194 KB gz (63%).

**Position on the two proposals.** The clustered flag is subsumed for scan cost; its residual value (dropping the exact structure's memory) doesn't justify a file property that can silently stop applying. The array type stays a *modelling* feature — a two-technology plant being two rows is a real wart — but its perf argument is gone: 0.48× elements is a 2× constant on a path now ~5× cheaper, and the measure doesn't collapse with the count anyway (two images, app-side merge). **Next on this path**: dense-code bitmaps for int columns from footer `Stats::Int{min,max}` (`gen_key` is 1..40,740 and near-unique, so the filter can't help it — still ~1 ms of pure probing), then a compile-time per-column distinct count so unfiltered header totals become a footer read. After that the 17-column measure sum is the floor on the wide table, exactly as the note predicted.
</sv-prose>

<sv-prose id="d37">
## Build log 21 — multi-column GROUP BY: the cost was per group, not per column (2026-09-18)

**Position, recorded because it kept getting lost.** David asked what we had concluded about slow multi-column GROUP BY; the honest answer was nothing — four places held measurements and no decision: the M6 direct-index design (dict dims → composite code → dense lane, 4M-lane cap, `HashMap<Vec<Val>>` beyond it), build log 19's int-dim extension, the v1 risk line "high-cardinality GROUP BY materializes in memory — accepted", and the 2026-09-12 extended bench in `bench/reports/` where `group by "GEM unit/phase ID"` (183K groups) took **100 ms vs DuckDB 9.6** — the one shape we lose badly — with the note "result construction is part of this workload" and no follow-up. The PUDL map (`group by plant, fuel`, 21K groups) was flagged "worth a look" and left.

**Measured first** (plant_tech_year, 231K rows, native ms): `group by fuel, state` + sum, ~500 groups, **2.0** — two columns cost nothing. `group by plant, fuel`: count only 3.3 → + sum 6.0 → + `order by round(sum/1000,1) desc` (the map query) **11.2**. Add `state` (product 41M, past the 4M lanes) → **22.5**. So two things, neither of them "columns": (1) **cost per group** — a `Vec<Val>` key per new group, every accumulator grown per group, a `Vec<Val>` output row per group, `cmp_sql` over those rows, and above all `eval_grouped`, a per-group tree-walking interpreter (~90 ns per expression per group, run twice for anything in both SELECT and ORDER BY); (2) **the dense-lane cliff** into `Vec<Val>` SipHash grouping.

**What landed** (the commit carrying this entry), `exec.rs`:

- **`PackedGroups`** replaces `DirectGroups`: dict/int GROUP BY columns pack to one mixed-radix `u64` code (null lane per dim); ≤ 4M lanes indexes a dense `Vec<i32>` as before, larger products probe **`GroupMap`**, an open-addressed code → gid table (same shape and hash as `DistinctU64`). Keys stay packed for the whole scan: `group_codes: Vec<u64>`, no `Vec<Val>`. Accumulators grow once per batch. The row loop is a macro instantiated twice (dense / map) — a single loop testing the `Option` per row was a measurable wasm-only loss.
- **The output phase is a vectorized pass over the group table.** Aggregates finish into lanes (`GroupCol::Ready`), packed keys peel into **code lanes** (`GroupCol::Dict` on the real dictionary — no string touched) or int lanes, hash-path keys into `Ready` lanes; SELECT and ORDER BY expressions are rewritten by `substitute_grouped` (aggregate call → its lane's synthetic column, GROUP BY expression → its key lane) and evaluated by `eval_vec`, the same kernels the scan uses. ORDER BY on numeric keys packs to `(validity, u64, gid)` tuples and `sort_unstable`s (gid last = the old stable tie order); text falls back to `cmp_sql`. Only OFFSET/LIMIT survivors are gathered, straight into the **columnar channel** (`cols: Some`, like every other path now; `ensure_rows()` for row consumers). `Overrides`/`eval_grouped`/`scalar_binary` are gone.

**Numbers** (`bench/pudl-grouping.sql`, plant_tech_year; native best-of-3 / wasm best-of-15, warm ms):

| shape | groups | native before → after | wasm before → after |
|---|---|---|---|
| map query (`plant, fuel`, min lat/lon, order by float expr) | 21,098 | 13.4 → **5.8** (2.3×) | 20.3 → **9.3** (2.2×) |
| `plant, fuel, state` (41M product, was the cliff) | 20,577 | 22.8 → **3.3** (7.0×) | 35.1 → **4.7** (7.5×) |
| `plant, fuel` + sum, order by int | 20,577 | 6.2 → 3.2 | 8.9 → 4.6 |
| … order by float expr, limit 100 | 20,577 | 10.9 → 4.3 | 14.2 → 5.7 |
| `plant_name (dict), fuel` count | 20,577 | 3.7 → 2.6 | 5.4 → 3.5 |
| utility facets (8,261 groups; both PUDL suites) | 8,261 | 1.5–2.1× | 1.5–2.3× |
| `fuel, state` (~500 groups) | ~500 | 2.0 → 1.8 | 2.6 → 2.3 |

Ten-group facets, the canonical 14-shape suite, the dictionary-distinct suite and the SQLite differential: unchanged (within noise) or slightly faster. New test `multi_column_grouping_matches_reference` checks packed null lanes, the GroupMap path, float-expression and text ordering, LIMIT/OFFSET and an expression key against an independent computation over the same arrays. 69 tests. The phase timers that guided this (group table / order / gather) were temporary and are not in the tree.

**Stage 2, same day — text keys hashed off the blob.** `"GEM unit/phase ID"` and `"Plant / Project name"` are plain Utf8 (cardinality past 65,536, no dictionary), so they had stayed on the `HashMap<Vec<Val>>` path: an `Rc<String>` lane materialized per row group, a `Vec<Val>` allocated and SipHashed per row. Now **`TextGroups`**: every GROUP BY a direct column, at least one plain Utf8, the rest dense dims. Dense dims pack into a composite exactly as in `PackedGroups`; each text dim is a borrowed view of the blob (`GroupCol::Text` offsets + bytes + validity) and hashes its bytes with the Fx mixer, NULL as a constant. A slot holds the 64-bit hash; a hit verifies against the group's stored key — packed code plus `(start, len)` spans into one **arena** — so no string exists during the scan. For the group table the arena is compacted per text dim into a raw text lane, and a text key selected as-is gathers through `SelSrc::RawText` — an `Rc<String>` is never built for it. Same commit as stage 1's follow-up (`bench/…/hc.sql` shapes, GEM 183K rows):

| shape | groups | native before → after | wasm before → after |
|---|---|---|---|
| `group by unit_id`, count | 183,125 | 120 → **15** (8×) | 124 → **20** (6×) |
| … + sum | 183,125 | 95 → 15 | 84 → 20 |
| … + sum, order by sum desc | 183,125 | 150 → 27 | 150 → 31 |
| … order by sum desc limit 100 | 183,125 | 164 → 22 | 142 → 25 |
| `plant_name, country` count | 150,622 | 117 → 19 | 130 → 25 |
| `plant_name, country, status` count | 150,622 | 132 → 21 | 158 → 30 |

Against the 2026-09-12 rival numbers for this shape — DuckDB native 9.6 ms, DuckDB-WASM 26.3 — facetful-wasm now leads DuckDB-WASM and sits within 2× of native DuckDB where it was 10× behind. Full result sets of four text-grouped queries (183K, 150K and 30K rows) are identical before and after; `text_key_grouping_matches_reference` checks text × int with NULL text, a lone text key, two text keys, ordering by the key and LIMIT/OFFSET against an independent computation. 70 tests, canonical suite clean, wasm 66% of budget.

**Left on the table.** ORDER BY on a text key itself still materializes strings for `cmp_sql` (a rank lane over the arena would do it as an integer sort); a text key composed with a dense dim whose product overflows u64 falls to the old path; expression keys (`group by lower(x)`, `x || y`) stay on `Vec<Val>`. None of these are on the PUDL or GEM pages.
</sv-prose>

<sv-prose id="d38">
## Build log 22 — the executor restructured: one strategy per module, a driver you can read (2026-09-18)

**Why now.** David: the engine "seems to take a long time to update now and it seems to have a lot of match / conditionals" — and asked for a refactor even at some initial cost in speed. Measured before touching anything: `exec.rs` was 3,947 lines, 38% of the engine. The kernel tables (`eval_call_vec` 360 lines, `update_batch` 256) are wide but flat — one arm per operation × lane type, the price of specialized loops. The real problem was **`execute`: 969 lines, nesting 12 deep, five programs interleaved in one row-group loop** — aggregate, top-k, full sort, plain, limit-capped scan — selected by a chain of `bool`s (`is_aggregate`, `topk_cap`, `full_sort`, `plain`, `scan_cap`), all their mutable state alive at once (`accs`, `direct`, `textg`, `hash_groups`, `cands`, `bound_key`, `sort_groups`, `plain_refs`…), each finishing with its own early `return`. Every fast path this month went in as another `else if` at three places in that function. That, not the kernels, is what made changes slow.

**Two commits, both behaviour-preserving.**

1. **`883e6b6` — pure moves.** `exec.rs` cut along its own section markers into `exec/{value, vector, eval, scalar, distinct, agg, output, filter, grouping}.rs`; moved items `pub(super)`, the public surface (`Val`, `QueryResult`, `OutCol`, `execute`) unchanged at `sql::exec`. No logic touched.
2. **This commit — `Strategy`.** The booleans became a type, chosen once before the scan:

   `Strategy::{Aggregate, TopK, FullSort, Plain}` — each a struct in its own module (`aggregate.rs`, `topk.rs`, `sort.rs`, `plain.rs`) owning only its own state, with `scan_group(&mut self, …)` and `finish(self, …)`. `Shared` holds what all of them read: the bound query, the column needs (`proj_needed`, `ord_needed`), dictionaries, WHERE conjuncts, output types. `Strategy::load_plan` answers the one cross-cutting question — which lanes, how deep, how many rows to evaluate — so top-k's ORDER-BY-only load and plain's limit-capped depth stop being special cases in the loop. The WHERE phase (mask cache, LIKE needle narrowing, the width-clamped int sweep, full evaluation) is `filter::where_mask`. The driver, `execute`, is **35 lines**: fold, plan, then per row group `where_mask → load_plan → load → scan_group`, then `finish`. `Aggregate` in turn makes the grouping choice a type: `Grouping::{Ungrouped, Packed, Text, Expr}`.

   Top-k's finish previously re-implemented column loading inline; it now calls the same `load_columns` as everything else. `cmp_keys` moved from a closure to a function. The `Vec<(Vec<Val>, Vec<Val>)>` "row seed" that every path had stopped using is gone.

**Shape after** (lines): `exec.rs` 433 (driver + `Shared` + `Strategy` + the two tree walkers + null-folding), `aggregate.rs` 366, `grouping.rs` 406, `agg.rs` 452, `eval.rs` 799, `filter.rs` 328, `topk.rs` 152, `sort.rs` 105, `plain.rs` 71. Longest functions: `eval_call_vec` 360 and `update_batch` 256 (the kernel tables, unchanged), then `Aggregate::scan_group` 171 and `Aggregate::finish` 137. Maximum nesting fell from 12 to 10, and that 10 is in the kernel tables. Adding a strategy or a grouping is now a new variant: the compiler lists every place that must handle it.

**Cost: none measurable.** The hot loops did not move — only the scaffolding around them, which runs once per row group. Native best-of-3 on `bench/pudl-grouping.sql`, both PUDL suites and the GEM text-key shapes: every number within run-to-run noise of the pre-refactor binary (map query 6.23 → 6.01, `plant, fuel, state` 3.26 → 3.30, 183K-group text 17.5 → 16.9, utility facets 4.77 → 4.98 / 3.78 → 3.74). Canonical gate: no regressions. Nine cross-check queries spanning every strategy (top-k with and without offset, plain with limit, `select *`, ungrouped aggregates, grouped with text keys, the map query, a filtered pivot) return byte-identical result sets. 70 tests. wasm 202.7 KB gzipped (67% of budget; +~1 KB — the driver's per-variant dispatch and `Shared`).

**Still on the list, in order.** (1) Expression keys (`group by lower(x)`) are the last user of the `Vec<Val>` hash path: evaluate them to lanes and feed `TextGroups`, and `Grouping::Expr`, `lane_val` on keys and `HashMap<Vec<Val>>` all go — a net deletion. (2) `update_batch`'s accumulator × lane matrix and `eval_call_vec`'s function table are the remaining width; both are flat and each arm is independently testable, so they are the right kind of big. (3) `QueryResult.rows` is now produced only by top-k's finish; once that gathers columnar too, `rows` becomes `ensure_rows()`-only and the row representation leaves the executor entirely.
</sv-prose>

<sv-prose id="d39">
## Build log 23 — wasm size: the bytes were monomorphization, not type handling (2026-09-18)

**The question.** David asked whether the wasm could shrink by "reducing particular type handling and using wider types". Measured before answering: no size tooling installed, so the wasm was built unstripped and its code section attributed per function from the name section with a 60-line Python parser (v0 mangling grouped by path). Answer: the executor already evaluates every int as `i64` and every float as `f64`; narrow widths live only in the storage readers and the filter sweep, and none of that appears in the top of the list. What is large is **monomorphization** — generic code stamped out once per closure or key type.

| code family | before | after | |
|---|---|---|---|
| `core::slice::sort` | **86 KB, ~68 fns** | **46 KB** | one full quicksort/driftsort per distinct closure handed to `sort_by`: 3 in `FullSort::finish`, 3 in `Aggregate::finish`, 2 for top-k `Cand`, `total_cmp` for median and the spike top-k, … |
| `exec::eval` | 52 KB | 42 KB | `lanes_to_vv(impl Fn)` re-instantiated at every call site |
| `exec::agg` | 36 KB | 41 KB | `update_batch` 28 KB (the accumulator × lane × validity matrix) + the new `finish_lane` |
| parser + binder + lexer | 50 KB | 50 KB | needed |
| float → text (`flt2dec`) | 16 KB | 16 KB | `{}` on f64, reached by `text()`/`\|\|`/`group_concat` |
| Unicode case tables | 9 KB | 9 KB | `lower()`/`upper()` on non-ASCII — GEM names need it |
| **whole module, stripped** | **569 KB raw / 207.6 KB gz** | **513 KB raw / 195.1 KB gz** | 67% → 63% of budget; below v0.3.0's 193 KB with everything since |

**What landed (this commit).** Every sort in the crate goes through four functions in `output.rs`, so there are four instantiations of the algorithm instead of ~14: `sort_keyed2`/`sort_keyed3` over 16-byte `(validity, bits, payload)` tuples (median and the spike top-k route through them too), `sort_perm_packed` for multi-key numeric permutations, and `sort_perm_keys(perm, &[Vec<Val>], order_by)` for text keys — top-k's candidates became parallel `keys`/`refs` arrays so they fit that signature. `lanes_to_vv` takes `&dyn Fn` (the cold per-value path; six copies → one). `AggAcc::finish_lane` builds each aggregate's group-table column directly per variant — `Count` is a `to_vec`, `SumF` is the vector plus a validity bitmap from `any` — with no `Val` per group, which is faster than what it replaced. The cached-mask AND in `where_mask` is byte-parallel (eight `keep` bytes per mask byte).

**Six traps, each measured, each the reason for a line of code above:**

1. A derived `Ord` on a 4-tuple as the sort key was **1.7× slower** than `sort_unstable_by_key(|t| (t.0, t.1))` on the same tuple: `full_sort` 10 → 17 ms. Keys are explicit two- or three-field tuples.
2. An index tiebreak on the multi-key sort cost **+9%** (22.5 → 25 ms): making every key distinct defeats the sort's equal-element fast path, and real data (capacities, years) ties constantly. `sort_perm_packed` has no tiebreak; the aggregate path appends the gid as one more key instead.
3. The same reasoning splits `sort_keyed2` from `sort_keyed3`: a third key field is only worth paying for where the rows are groups a UI will show, so ties stay deterministic there and nowhere else.
4. A `dyn` call per comparison in the text-key sort cost **+10%** (40 → 44.5 ms on 183K rows). The concrete `&[Vec<Val>]` signature is one instantiation *and* static dispatch.
5. A 24-byte tuple element instead of 16 cost **−15% in wasm** on the 183K-group ordered shapes (memory moved by the sort). Payload is one `u32`; the full-sort path indexes its refs through it.
6. A `dyn` producer for the aggregate finish lanes — the one *warm* `lanes_to_vv` site — cost **−15% in wasm** (`call_indirect` is far dearer than a native indirect call) while native showed nothing. Hence `finish_lane`.

And one from the previous commit, found by bisecting wasm builds: `like_2col_facet` 0.32 → 0.42 ms arrived with the restructure — the per-row `bits[i / 8] >> (i % 8)` AND stopped vectorizing once it moved out of the driver. Byte-parallel, it now makes every cached-filter shape faster than before the restructure (`like_dict` 0.23 → 0.19, `in_list` 0.23 → 0.18, `facet_filtered` 1.15 → 1.09); `like_2col_facet` sits at 0.38, the residual 0.06 ms being the fixed cost of the vectorized output phase (a `HashMap` and a few `Vec`s) on a ten-group query.

**Net speed.** Native, interleaved best-of-3 against the pre-size-work binary: `full_sort` 10.7 → 9.3 ms, `agg_order_1key` 3.6 → 3.2, two-key and text sorts within noise; wasm text-key shapes 0.96–1.05×; the canonical suite at parity or faster (`facet_count` 1.95 → 1.38 from the restructure). Gate clean; 70 tests; eight cross-check queries covering every changed sort path — full sorts by one, two and text keys, grouped ordering with ties, median, the 183K text-key top-100, the PUDL map — return byte-identical result sets.

**Remaining size, if it is ever wanted.** `update_batch`'s no-null / check-the-bit arm pairs (~10 KB, and the no-null arms are what vectorize unfiltered sums — measure first); `flt2dec` (16 KB) if `text()` on floats ever gets a leaner formatter; the parser/binder is what it is. The fixed ~0.06 ms of the group-table output phase would come out by giving `GroupCtx` a `Vec` indexed by synthetic offset instead of a `HashMap`.
</sv-prose>

<sv-prose id="d40">
## Build log 24 — the middle ground between opt-level 3 and z (2026-09-18)

**The question.** David remembered the M6 decision (`opt-level=3`, not `"z"`: 2–3× faster for +15 KB gz) as "one extreme to the other" and asked whether a compiler flag sits in between. Measured the same way as then: every variant built, then every one of the 30 shapes (canonical 14 on GEM, the 6 text-key shapes, the 10 PUDL grouping shapes) run in Node, best of 15 warm.

| variant | gz (local) | speed vs `O3` | verdict |
|---|---|---|---|
| `opt-level=3` (current) | 195.1 KB | — | |
| **`3` + `-inline-threshold=50`** | **187.8 KB** | **parity; several shapes 2–5% faster** (`full_sort` 14.2 → 13.7, `like_2col_facet` 0.39 → 0.35) | **adopted** |
| `3` + `-inline-threshold=25` | 189.2 KB | projections 5–9% slower | past the knee — and no smaller |
| `opt-level=2` | 190.3 KB | parity | dominated by the line above |
| `2` + `-inline-threshold=50` | 183.3 KB | top-k +11%, text projection +5% | 4.5 KB not worth a slower keystroke |
| `opt-level=s` | 176.3 KB | **10–27% slower** on grouping and projection (`map` 7.8 → 9.0, `pf_sum_order_limit` 5.2 → 6.4, `topk` 1.9 → 2.4) | no |
| `opt-level=z` | 163.2 KB | **1.5–4× slower** everywhere (`projection_floats` 2.8 → 10.5) | the 2026-09-04 decision stands |
| per-crate `z` for `facetful-format` | 194.8 KB | — | −0.3 KB: under fat LTO a per-crate level barely exists |
| `wasm-opt -O3` vs `-Os` vs `-Oz` post-pass | within 0.5 KB of each other | — | CI's `-O3` is right; the pass itself is worth ~5 KB |

**Adopted:** `-C llvm-args=-inline-threshold=50` in the wasm target's rustflags (`.cargo/config.toml`), beside `+simd128`. LLVM's default at O3 is 250; the flag trims the *duplication* that inlining creates in the big `execute`/`Strategy` bodies while the kernels — which are small and hot — still inline. It is an LLVM-internal flag rather than a rustc guarantee; `size-check.sh` and `bench.sh` are the tripwires if a toolchain bump changes its meaning. Shipped size after CI's `wasm-opt -O3`: **190.3 → 184.5 KB gz**. Node smoke, temporal round-trip and the Parquet-path differential pass on the new build; three interleaved rounds against the plain O3 build show no shape outside noise.

**Position.** Two of the three levers are now settled by measurement: code shape (build log 23: monomorphization, not type width) and compiler flags (this entry). The third, `opt-level=s`, is what to reach for only if the budget ever genuinely binds — it buys 11 KB more for a 10–27% cost the pages would feel. At 61% of budget with headroom of ~120 KB, nothing argues for it.
</sv-prose>

<sv-prose id="d41">
## Position — materialize is the primitive; JOIN and CTEs are cached materializations (2026-09-18, discussion with David)

**Where it started.** Two worries met: (1) the size forecast — David, correctly, that the executor is what costs, "especially if you optimise", and that a query-time join done in this codebase's style (a specialized arm per key type, per lane type, composite keys, both tables scanned together) would run 16–20 KB gz, not the 8–10 first estimated; (2) the long-standing aim of making new tables from existing ones in the browser. The two resolve into one design.

**What DuckDB does, for reference** (asked and recorded): hash join for every equality join (inner/outer/semi/anti/mark, radix-partitioned, spilling), a **perfect hash join** when the build key is a small-range integer (a direct-indexed array on `key − min` — our `PackedGroups`), piecewise merge join and IEJoin for one and two *inequality* predicates, nested-loop for the rest, ASOF and POSITIONAL as specialised operators. **No sort-merge join for equi-joins**: one good hash join plus dynamic min/max filter pushdown beat a second code path. That settled merge-vs-hash here: one algorithm, hash, even though both PUDL images happen to be plant-ordered — that is a property of those files, not of the model.

**The decision.**

1. **`materialize` is the primitive.** A query's columnar result becomes a new immutable table: the compiler and writer already in the wasm for Parquet ingest take a `QueryResult` instead of Parquet rows — dictionary-encode text (cardinality ≤ 65,536), narrow ints by range, per-segment stats, 65K-row groups. Row order is the query's output order, so a materialized `ORDER BY` yields `sorted_by` metadata and free ordering later, and a join preserves fact order so clustering (and the last-seen distinct filter) survives. `db.materialize(name, sql, {persist})` names it and optionally writes it to OPFS keyed by source content hash + SQL — the fetch-once story extended to derived tables. This is also the PUDL "derive after load" case: pre-aggregate a 5M-row detail image into a 0.5M-row facet table in the browser instead of at build time.

2. **`JOIN` is accepted syntax and is a cached anonymous materialization.** The query `select … from fact f join dim d on f.k = d.k where …` runs as: materialize the joined table (once), then run the rest of the query against it as an ordinary table. David's instinct was materialize-then-discard with a recommendation against the syntax; the refinement is **don't discard, cache**: a byte-budgeted LRU of derived tables — the mask cache's shape (`Table::masks`, 16 MB) applied one level up — keyed by (left table, right table, key columns, join type, right-side columns touched; a superset of columns satisfies a subset). The facet workload runs 10–30 queries per interaction against the same join; with the cache the first pays ~20 ms and the rest pay nothing, so `JOIN` needs no warning — the docs recommend `materialize` only for a *named* or *persisted* result, and warn against join conditions that vary per query (each variant is its own materialization).
   - **Restricted to a unique right-side key** (the d29 decision, unchanged), now enforced cheaply and loudly: the build side is fully seen, so a non-unique key is an error at first use, never a silent row explosion. Many-to-many stays declined.
   - **LEFT is the base, INNER is a mask.** The LEFT join preserves the fact row count (positional model intact) and adds a `matched` lane; INNER is the same cached table with `matched` as an implicit conjunct. One cached table serves both.
   - **Dimension-side WHERE needs nothing special.** `where d.region = 'EU'` is a filter on a gathered lane: mask-cacheable, dictionary fast path, like any conjunct. The semi-join pushdown a query-time join would need never exists as a concept.
   - **Dictionary code translation**: two tables' dictionaries differ, so a dict-to-dict key equality maps codes once per dictionary pair (O(|dict|) lookups), then probes on the fact side's codes. Int keys probe directly (perfect-hash style through `PackedGroups` when the range is small); text keys hash bytes through the `TextGroups` arena. **The join's key machinery is the grouping's key machinery** — build = group the dimension table by its key — which is what keeps the estimate at the low end.

3. **CTEs and `FROM (subquery)` are materialized temporaries — the optimization fence, on purpose.** `WITH x AS (select …) select … from x` materializes `x` (anonymous, cached by canonical SQL + source table identities), then binds the outer query against it as a table; a CTE may reference an earlier one (sequential); a `FROM` subquery is an unnamed CTE. This is what PostgreSQL did unconditionally before 12 and still does under `MATERIALIZED`, and what DuckDB does for a CTE referenced more than once; here it is simply the only mode, and it is the right one for repeated queries over immutable data — the CTE is computed once per session, not once per query, and never re-planned. It also costs almost nothing: the executor is untouched, the work is a binder scope stack and the cache. Recursive CTEs stay out.

4. **`IN (select k from y)`** (d34) rides the same catalog: materialize the distinct `k` of the inner query, then a membership mask on the outer column — the semi-join, through the dictionary trick where both sides are dictionaries. **Row values are part of this step** (added 2026-09-19): `(a, b) IN (select x, y from …)` and the literal form `(state, fuel) IN (('TX','gas'), …)` — SQL:1999 row value constructors, supported by PostgreSQL, SQLite ≥ 3.15, DuckDB and MySQL (not SQL Server), so the differential covers them. A composite key is already one packed code (`PackedGroups`) or a hashed byte tuple (`TextGroups`), so the inner query's distinct tuples become a code set or a hash set and the outer WHERE is a mask like any conjunct; the literal form needs no materialization at all — each tuple translates to codes once. Semantics follow the standard: `(a, b) = (x, y)` is `a = x AND b = y` under three-valued logic, so a NULL in any position makes the row unknown; a facet UI that wants "unknown matches unknown" says so explicitly (`IS NOT DISTINCT FROM`, if ever added), it is not the default. It is also the form LLM-written SQL reaches for to say "rows matching these pairs".

**Size, re-estimated honestly against today's measured analogs** (grouping subsystem 20 KB raw / 8 KB gz; `gather_outcol` 3 KB raw for four lane kinds; binder 21 KB raw; FFI 24 KB raw):

| piece | gz | reuses |
|---|---|---|
| materialize (QueryResult → table) | ~3 KB | compiler + writer already present |
| derived-table cache | ~1 KB | the mask cache's shape |
| one-shot LEFT hash join into a table | ~6–8 KB | `PackedGroups`/`TextGroups` for build and probe, `gather_outcol` for the four lane kinds, plus dict translation and the `matched` lane |
| catalog + two-table binder scope + `JOIN`/`WITH` syntax | ~4–5 KB | |
| FFI: multi-table open/query | ~2 KB | |
| **total** | **~16–19 KB** | vs 25–35 for optimized query-time join + windows-style CTE execution |

At 184.5 KB shipped this lands near 203 KB, 68% of budget, with the optimization tiers that history says cost 5–10 KB each never needed for joins at all.

**Order of work.** (1) `materialize` — the primitive, plus OPFS persist; it is independently valuable and makes everything else a binder change. (2) The derived-table cache and `WITH` / `FROM (subquery)` through it — small, and it exercises the cache before joins depend on it. (3) The one-shot LEFT hash join as a query shape `materialize` accepts, then `JOIN` syntax as its cached anonymous form; INNER as the mask. (4) `IN (select …)`. The SQLite differential tests every step directly: `WITH`, `JOIN` and `IN (select)` queries run on both engines as written.

**Not in this design:** query-time join execution, filter pushdown into a dimension scan, merge join, many-to-many, recursive CTEs, windows. Each is a separate decision if a workload ever asks; none is needed for a facet page over a star schema.
</sv-prose>

<sv-prose id="d42">
## Build log 25 — `materialize`: a query result is a table (2026-09-18)

Step (1) of the d41 plan, the primitive the rest stands on. `facetful_engine::materialize::materialize(table, sql, group_target) -> image` parses and binds the query, executes it, and hands the result's typed columns to the compiler Parquet ingest already uses (`compile_sorted`, the existing `compile` with a `sorted_by` parameter): text dictionary-encodes when the cardinality pays (the compiler's existing `dict × 2 < rows` rule), ints narrow to their range, every segment gets min/max stats. Two things are new in kind rather than plumbing: **the leading `ORDER BY` keys that are SELECT items become the table's `sorted_by`**, so a materialized ordering prunes by disjoint ranges and is free to re-sort by later; and **row order is the query's output order**, so a projection of a plant-ordered table keeps its clustering, which is what the d41 join design relies on. Duplicate SELECT names are an error at materialize time ("alias it"); booleans land as 0/1 ints (the compiler's input has no bool lane); an empty result is a valid zero-row table.

Surfaces: wasm `table_materialize(t, sql, group_target)` → an `Outcome::Image` taken by `outcome_image` into the existing image handle (`image_open_table` / `image_ptr` — no copy back through JS to open it); worker command `materialize`; `db.materialize(name, sql, {table, persist})` registers the derived table under `name` and, with `persist`, writes the image to OPFS for a later `loadOpfs`; CLI `facetful materialize in.facetful "sql" out.facetful`, which is also how the differential can test it.

Measured on PUDL: `group by fuel, state` with a distinct count and a sum over 231K rows → a 466-row, 7.5 KB image in **3.9 ms**; the fuel facet against it answers in **0.1 ms** where the source takes 1.8, with identical numbers. Tests: types, nulls, multi-group output, the recorded sort prefix, an empty result and the duplicate-name error; the node smoke runs a grouped materialize round-trip and the `QueryError` path. wasm **191.2 KB gz locally (62%)** — +3.4 KB for the module, in line with the d41 estimate. 73 tests, canonical gate clean.

Next per d41: the derived-table cache and `WITH` / `FROM (subquery)` through it.
</sv-prose>

<sv-prose id="d43">
## Build log 26 — WITH and FROM (subquery) as cached materializations (2026-09-18)

Step (2) of d41. `WITH name AS (query) [, …] select … from name` and `select … from (query) alias` parse as full nested queries (`ast::Cte`, `Query::from_subquery`, whole-query spans). Resolution lives in `sql::exec_query`: each CTE body is materialized through `materialize::compile_result` into the table's new **derived-table cache** — `Table::derived`, the mask cache's shape one level up: keyed by the body's **token stream re-spelled from its spans** (whitespace vanishes, keywords and bare identifiers lowercase, string literals and quoted identifiers keep their case — not `{:?}` of the token enum, which is wasm bytes) plus the key of the table it reads from, byte-bounded (64 MB default, `set_derived_budget`, wasm `table_set_derived_budget`), least-recently-used eviction, never stale because tables never change. The final query then binds against whichever table its FROM names — a CTE in scope, else the table itself — and runs unchanged; a CTE may shadow the table's name, bodies chain (each sees the CTEs before it) and nest. `materialize` itself now goes through `execute_sql`, so a `db.materialize` body may use CTEs.

**One instantiation, not two.** A derived table must have the *same* source type `S` as its parent, or the wasm would carry a second copy of the whole executor (~20 KB gz). `ReadAt::from_memory(Vec<u8>) -> Option<Self>` wraps an image as the source's own type — `Vec<u8>` and the wasm `Src::Mem` say `Some`, borrowed sources keep the `None` default and report "derived tables need a source that can own memory". The cache is a `Vec` with a linear scan, not a `HashMap<String, _>` — a handful of entries, and the map instantiation alone measured 0.8 KB. Net wasm cost of the whole step, measured against the materialize commit: **+4.2 KB gz** (191.2 → 195.4; 64%) — the parser's recursive query, the resolver and the cache.

**Measured** (plant_tech_year, native): the fuel facet written as `with r as (fuel × state rollup) select … from r group by fuel` — **3.7 ms on first use, 0.01 ms after**; a *different* query over the same CTE body (`state` totals) hits the cache at 0.01 ms; the direct facet is 2.1 ms every time. The optimization fence, which most databases apply to CTEs as a compromise, is here the whole point: over immutable data queried in bursts, computing a CTE once per session is simply faster.

Tests: results equal the unfolded query, chained CTEs, `FROM (subquery)` under an outer aggregate, cache hit on a re-spelled body, shadowing, types preserved, an error inside a body rendered with its span. Three CTE / subquery queries joined the SQLite differential, which inlines them and agrees. 74 tests, canonical gate clean.

Next per d41: the one-shot LEFT hash join as a shape `materialize` accepts, then `JOIN` syntax as its cached anonymous form.
</sv-prose>

<sv-prose id="d44">
## Build log 27 — the one-shot hash join (d41 step 3, form B) (2026-09-19)

David chose the two-image form first — "a small change to test whether it works" — with the JS shape explicitly provisional, expected to go once `JOIN` syntax lands (the agents write SQL better than they write option objects). Landed as `facetful_engine::join::join(left, right, spec) -> image`, wasm `table_join`, `db.materialize(name, { join: { left, right, on, columns?, type? } })`, CLI `facetful join left right out --on l=r[,…] [--columns …] [--inner]`.

**The algorithm, as built — perfect-hash first, the grouping machinery reused:**

- **Dictionary key on both sides** (the star-schema case): translate once per *left dictionary entry* — each left string looked up in the right dictionary through a `TextGroups` arena (one hash per distinct value, not per row) — giving `left code → right row`. The per-row probe is an array index; no hashing in the row loop. Codes are dense by construction, so this is DuckDB's perfect hash join with the "is the range dense?" question answered by the format.
- **Integer key:** the right table's footer min/max gives the range; within the 4M-lane budget the map is a dense lane indexed by `k − min`, else the open-addressed `GroupMap`. Same fork `PackedGroups` takes.
- **Plain-text key on either side:** bytes hashed into a `TextGroups` arena; a dictionary column on the other side hashes its dictionary string per row. Gids are dense and rows with NULL keys are skipped, so gid → row goes through an indirection (the bug the first PUDL run found: a dimension with one NULL fuel put gid 9 past a 9-entry table).
- **Composite keys** pack the dense dims into one code, exactly as multi-column `GROUP BY` does; text dims join the arena hash.
- **Uniqueness enforced, loudly:** a repeated right key is `join: the right key is not unique (row N repeats an earlier key) — a join must be many-to-one`, never a row explosion (d29). NULL keys never match, on either side.
- **Gather:** every left column passes through; the chosen right columns are gathered by matched row into fact-positional lanes — a dictionary column carries its *codes* and the right table's dictionary (new compiler input `InCol::Dict`), so no string is touched; LEFT adds a `matched` 0/1 lane, INNER keeps matched rows and adds nothing. The left table's `sorted_by` carries over because row order is preserved. Name clashes are an error; the right key is not carried.

**Measured.** PUDL, `generator_tech_wide` (42,257 rows × 128 columns) joined to a materialized 18,937-plant dimension on the int key: **85 ms**, every row matched, the dimension's state agrees with the fact's own state column on all 42,257 rows. The same facts joined to a 10-row fuel dimension whose key is plain text (below the dictionary payoff): 80 ms, 76 unmatched = the 76 NULL-fuel generators, `plants_in_fuel` equal to a direct count. Both times are dominated by re-compiling the 128 pass-through columns into a 34 MB image — which is the argument for the SQL form (step 3b): a `JOIN` inside a query knows which columns the query touches and need carry only those. Tests: dictionary keys with different code orders, dense int keys under INNER, plain-text and composite keys, NULL keys, and each error; the node smoke joins the 200K-row spike table to a materialized dimension and rejects a non-unique right key. 78 tests, gate clean.

**Size: +11.2 KB gz (195.4 → 206.6, 67%), against an estimate of 6–8.** Honest accounting: the `join` body is 14.6 KB raw — reading and gathering every lane kind for two tables, three map variants, the closures over the dims — plus `parse_spec` at 4.2 KB raw (string splitting for the FFI text form, which the SQL form will not need), `read_col` 2.9, `gather` 1.6. Two things were trimmed before landing (`{:?}` in error paths; a `HashMap<&str, u32>` for translation replaced by the arena, −0.6 KB). The lesson for the estimate column of d41: a feature that touches *every lane kind for two tables* costs double a feature over one table, and an FFI text protocol is not free.

**Against DuckDB** (same machine, same PUDL tables in memory, single-threaded, best-of-N warm; DuckDB native bulk-timed with setup subtracted, DuckDB-WASM 1.5.4 under Node via the September harness, CSV-loaded since its Parquet extension can't autoload offline):

| shape | facetful wasm | facetful native | DuckDB-WASM 1T | DuckDB native 1T | DuckDB native 16T |
|---|---|---|---|---|---|
| materialize the join, all 131 columns | **38 ms** | 64 | 87 | 89 | 159 |
| materialize a 4-column join (narrow facts, then join) | **4.6** (3.6 + 1.0) | 6.6 | 6.2 | 4.8 | 8.0 |
| facet (state) on the pre-joined table | **0.4** | 0.29 | 2.1 | 1.3 | — |
| facet (fuel × state) on the pre-joined table | **0.5** | 0.36 | 3.0 | 2.1 | — |
| facet via a *query-time* `LEFT JOIN` | n/a | n/a | 3.7 / 4.5 | 3.4 / 2.7 | 3.3 / 4.6 |

Reading it: the one-shot join itself is 2.3× faster than DuckDB's `CREATE TABLE … AS SELECT … JOIN` in either lane; the narrow join is parity; and once joined, a facet is 3–5× faster than DuckDB's on its own pre-joined table. DuckDB's query-time join facet — the shape LLM-written SQL produces — costs 3.4–3.7 ms every time; facetful's model pays 38 ms once (4.6 with projection) and 0.4 after, so it is ahead after ~12 facet queries on the full-width join and after 2 on the narrow one — inside the first interaction of a page that runs 10–30 per click. This is the measured case for 3b carrying only the columns a query references. Sixteen threads made DuckDB *slower* on the materialization (42K rows cannot amortize morsel parallelism), as in build log 19. One oddity recorded, not chased: facetful's native join (64 ms) is slower than its wasm build (38 ms) on the 34 MB all-columns image — the M5 glibc heap-trim pattern is the suspect; the CLI now reports join time separately from open + write.

**Next, per d41 step 3b:** the catalog and `JOIN` syntax, whose materialization calls this function with the projected right columns derived from the query and the cache key from the join's identity; `parse_spec` and the `{ join }` shape retire when it lands. `IN (select …)` with row values follows on the same catalog.
</sv-prose>

<sv-prose id="d45">
## Build log 28 — JOIN syntax: a cached materialization behind ordinary SQL (d41 step 3b) (2026-09-19)

`select f.fuel, u.name, sum(f.x) from facts f left join utilities u on f.utility_id = u.id …` now runs. `[INNER | LEFT [OUTER]] JOIN source [AS] alias ON a.k = b.k [AND …] | USING (k, …)`, chained left to right; the source is a loaded table by name, a CTE in scope, or a subquery. Qualified names `alias.column` lex as one token (`.` not followed by a digit is a qualifier); `FROM t alias` and bare aliases parse. A join condition is a key list, not a predicate — `on a.x > b.y` is an error pointing at WHERE — because the join is the d41 many-to-one materialization, not a query-time operator.

**How it runs.** `sql::exec_query` resolves the query's FROM through a new `Catalog` trait (other loaded tables, by name, with a per-registration identity) — a CTE in scope first, then a catalog table, else the query's own table. Each `JOIN` becomes `resolve_joins`: the right side resolves the same way; the key pairs are checked against the two sides' schemas in either order; **only the columns the query touches are carried** — right-side columns named as `alias.col` or bare-and-not-on-the-left, and (the fix the PUDL timing found) left-side columns too, plus the keys; `select *` carries everything, `count(*)` carries nothing. Right columns that clash with a left name become `alias_column`. The join materializes through `join::join` into the left table's derived cache under a key of (left identity, right identity, kind, keys, left columns, right columns), so an identical join in the next query is a hit and a different column set is its own table (a superset match is a later refinement). Then the query is rewritten onto the joined table — joins removed, qualified names resolved through the alias map, renamed clashes substituted — and bound and executed as an ordinary single-table query. Two derived tables joining each other are taken out of the cache for the duration (`derived_take` / `derived_put`), which is how a join to a CTE, or a CTE to another, borrows both sides at once. LEFT in SQL adds no `matched` column: `right.key IS NULL` is the anti-join idiom, and a referenced right key is carried, so it works; the flag stays on the one-shot form only (and a second LEFT JOIN in one query no longer collides).

**Surfaces.** wasm: `catalog_register(name, handle)` / `catalog_unregister(handle)` mirror the worker's table map, and `WasmCatalog` serves every registered table except the query's own (a join to itself is a clear error, never two `&mut` to one table). The worker registers on every `tables.set`. CLI: `facetful query main.facetful "sql" --table plants=plants.facetful …`. Engine: `run_query_with(table, sql, &mut dyn Catalog)`, `TableSet` for the CLI and tests, `materialize_with`.

**Measured** (PUDL, `generator_tech_wide` 42K × 128 joined to the 18.9K-plant dimension, native): the state facet written as a `LEFT JOIN` — **2.9 ms on first use** (the join materializes with two left columns and one right), **0.30 ms after**; the fuel × state facet over the same join 0.39; an anti-join count 0.04. Against build log 27's DuckDB numbers — 3.4–3.7 ms for the same facet as a query-time join, every time — the model is ahead from the second query. Before the left-column projection the first use was 65 ms (all 128 columns re-compiled); that gap is the whole argument for the SQL form knowing the query.

**Correctness.** Three JOIN queries joined the SQLite differential: SQLite loads the same dimension (`create table dim as select … group by country`), facetful materializes it and registers it under the same name, and LEFT with grouping, INNER with a dimension-side WHERE, and `USING` with `count(right)` agree cell for cell. Engine tests cover aliases and qualified names, a clash rename, cache hit vs. new column set, USING + INNER, the anti-join idiom, chained joins including to a CTE and to a subquery, `select *` column naming, and the errors (unknown table, non-equality condition, unknown alias, a condition on one side). The node smoke registers a materialized dimension and runs the SQL join twice. 82 tests, canonical gate clean.

**Size: +12.3 KB gz (206.6 → 218.9, 71%).** Against the d41 estimate of 4–5 for "catalog + binder scope + syntax". Where it went: `exec_query` with the resolver inlined 11.7 KB raw (String-keyed alias maps, cache-key formatting, the six borrow cases of `join_targets`), `Parser::query` grew 7.1 KB raw with JOIN/alias parsing, the catalog and registry ~1 KB. The d41 total for materialize + CTEs + join + JOIN syntax is now **~28 KB gz against 16–19 estimated**; the two misses are the same lesson twice — string-manipulating resolver code in Rust is not small — and the honest place to record it is the estimate column. The provisional `{ join }` JS shape, `parse_spec` and `table_join` were retired the same day (David: redundant once SQL says it, and the agents write SQL better than option objects) — **−1.8 KB, 217.2 KB, 71%**; a joined, persisted table is now `materialize("x", "select … from t join dims d using (k)")`. The CLI's `facetful join` verb stays (native, no wasm bytes; it drove the DuckDB comparison). A superset cache match remains available as a trim.

**Left for later:** joins whose FROM is another catalog table (run the query against that table instead — a clear error says so), superset cache matching, the `*` naming when both sides share many columns, and `IN (select …)` with row values on the same catalog (d41 step 4).
</sv-prose>

<sv-prose id="d46">
## Build log 29 — IN (select …) with row values: the semi-join as a rewrite (d41 step 4) (2026-09-19)

The last d41 step, and the one that shows what the materialization approach buys: **no executor code at all.** `x IN (select k from y)` is a semi-join, and a semi-join is a LEFT JOIN plus `IS NOT NULL` — both of which exist. So `sql::expand_in_subqueries` rewrites, before binding:

1. the inner query materializes (a cached derived table, like any CTE body);
2. its **distinct key set** materializes over that — `select "k1", "k2" from __in_src group by "k1", "k2"`, generated text through the same `derive`, so it is cached and deduplicated like everything else;
3. one query asks whether the key set holds a NULL (`count(*) where k1 is null or …`);
4. a synthetic `LEFT JOIN __inN_d AS __inN ON a = __inN.k1 AND b = __inN.k2` is appended to the query's joins — and from there build log 28's machinery does the rest: only the touched columns carried, clash renames, the derived cache;
5. the predicate becomes `__inN.k1 IS NOT NULL`; for `NOT IN`, `__inN.k1 IS NULL AND a IS NOT NULL AND b IS NOT NULL`; and when the key set holds a NULL, `NOT IN` becomes constant FALSE — SQL's three-valued rule, the one that surprises people (`x NOT IN (…NULL…)` selects nothing), reproduced exactly because SQLite is the reference.

**Row values** are SQL:1999 row value constructors. `(a, b) IN (select x, y …)` is the same rewrite with two keys (composite keys were already a packed code or an arena hash). The literal form `(a, b) IN ((1,'x'), (2,'y'))` desugars **at parse time** to `(a = 1 AND b = 'x') OR (a = 2 AND b = 'y')`, because the three-valued logic of AND/OR is precisely the row-value comparison rule: a NULL component leaves the row unknown *unless another component is definitely unequal* — the test that caught my own wrong expectation: `(NULL, 20) NOT IN (('eu',10), ('asia',30))` is TRUE, and SQLite says so too. `Expr::Row` exists only as the left side of IN and as an element of an IN list; anywhere else it is an error, as is `IN (select …)` outside WHERE or over expressions (plain columns only, since join keys are columns).

**Measured** (PUDL, generator facts with the 18.9K-plant dimension registered, native): `plant_id_eia IN (select plant_id_eia from plants where plant_rows >= 30)` grouped by fuel — 0.1 ms warm, count 8,351 = the equivalent `INNER JOIN`; `(plant_id_eia, fuel) IN (select … where gen_2024 > 1e6)` 0.05 ms; `state NOT IN (select pstate …)` 2,284 = the anti-join complement; the literal list `(state, fuel) IN (('TX','gas'), ('CA','solar'), ('WY','coal'))` 1.0 ms cold / 0.04 warm, 3,672 = its OR-of-ANDs expansion. Four queries joined the SQLite differential — scalar IN over the dimension, row-value IN over the same table, `NOT IN` against a set containing NULL, a literal tuple list — and agree. Engine tests cover scalar and row-value IN including through a CTE, cache reuse across reruns, `NOT IN` with a NULL outer key and a NULL inner set, literal lists and their errors; parser tests fix the desugar shapes. 86 tests, gate clean.

**Size: +7.9 KB gz (217.2 → 225.1, 73%).** `expand_in_expr` 5 KB raw, `Parser::infix` +6.9 KB raw for the row-value desugar (AND/OR tree building), `exec_query` +0.8. The same shape as the last two steps: resolver code is string-and-AST manipulation and costs more than its logic suggests. **The d41 set is complete at ~36 KB gz against 16–19 estimated** — each step justified by measurement, each estimate low by about 2×, and the correction recorded where the next estimate gets made.

**Position after d41.** Four features share one mechanism — a query result becomes a cached table — and none added an arm to the executor. That was the bet in d41 and it held. Open on this line, none urgent: superset matching in the derived cache, `EXISTS` (the same rewrite if a workload asks), `IN (select …)` in SELECT items, and the feature gates from the lite-build discussion, which the module boundaries already support.
</sv-prose>

<sv-prose id="d47">
## Build log 30 — EXISTS / NOT EXISTS, and superset matching in the derived cache (2026-09-19)

Two of the three items build log 29 left open, done together because they touch the same forty lines.

**EXISTS is the IN rewrite with the keys found elsewhere.** `x IN (select k …)` names its keys on both sides; `EXISTS (select … where d.k = t.k AND …)` hides them in the subquery's WHERE. So `sql::correlate` splits the inner WHERE's top-level AND conjuncts: `inner.k = outer.k` (either order, either side qualified by the inner alias or bare) becomes a key pair, everything else is the residual filter, and any *other* conjunct that mentions the outer table is an error — `d.id > t.id` is a real correlated subquery, which this engine does not run and says so. The inner query is then re-spelled as `select <inner keys> from … where <residual>` (the inner alias stripped, since the residual is a plain single-table query now), and from there it is `IN`: cached inner materialization, distinct key set, synthetic `LEFT JOIN`, `IS NOT NULL`. The parser produces the same `Expr::InSubquery` with `cols` empty and `exists: true`; there is no new AST node and no new binder arm. Uncorrelated `EXISTS` is a constant, computed by running the subquery once. **NOT EXISTS is the part worth having:** it is the anti-join with *no* NULL trap — two-valued, "no partner" is simply TRUE — so `NOT EXISTS (… d.k = t.k)` keeps the rows `k NOT IN (select k …)` throws away when the key set holds a NULL, or when the outer key is NULL. Agents reach for `NOT EXISTS` for exactly that reason, and it now means what they think it means.

**Superset matching** in the derived cache: the join key was an opaque string — sides, kind, key pairs, *exact* left and right column lists — so a dashboard whose facets differ only in which columns they touch built a joined table per facet. Now the key's prefix (sides, kind, keys) is scanned over `Table::derived_keys()` and the narrowest cached entry whose left set (or `*`) and right set contain the needed columns is reused; the query is bound against *its* columns. One subtlety made it structural rather than a string compare: a clash-renamed right column carries the alias of the query that built it (`d_v`), so a later query spelling the alias `x` would look for `x_v`. The right column list in the key is therefore stored as `col` or `col>stored_name`, and a superset hit hands back the stored names as this query's renames. The `IN`/`EXISTS` synthetic joins go through the same path, so a wide semi-join query (`select id, sum(v) … where exists …`) serves the narrow ones after it (`count(*)`, `group by cat`) — tested. What superset matching does *not* do is share the inner materialization between the `IN` and `EXISTS` spellings of one predicate: the inner cache is keyed by source text and the EXISTS inner is synthesized. Sharing it would need an AST printer, whose bytes buy a case that does not arise (an agent picks one spelling).

**Measured** (PUDL generator facts, 42,257 rows, 18.9K-plant dimension registered, native, warm): `EXISTS (… p.plant_id_eia = t.plant_id_eia and p.plant_rows >= 30)` grouped by fuel 0.11 ms — the `IN` form 0.10, the same 8,351 rows; two-key `EXISTS` on (plant, state) 0.06; `NOT EXISTS` 0.22, 33,906 = 42,257 − 8,351; a wide `EXISTS` facet then its narrow subset 0.10 / 0.08 with no second join built. Engine tests: one- and two-key correlation in either order over an aliased outer table, `NOT EXISTS` with a NULL outer key and a NULL inner set (unchanged answers, unlike `NOT IN`), uncorrelated constants, the non-equality error, superset reuse for joins across aliases through a clash rename and for semi-joins across select lists, and that a different join kind is never reused. Two `EXISTS`/`NOT EXISTS` queries joined the SQLite differential, correlated on the dimension — SQLite runs a correlated subquery as a nested loop, and the self-correlated form on 200K rows did not finish in a minute there, a reminder that "the reference implementation" is a reference for answers, not for shapes. 88 tests, gate clean.

**Size: +0.8 KB gz (225.1 → 225.9, 73%).** `correlate` 3.8 KB raw, `superset_join` folded into `exec_query`; against build log 29's +7.9 for a feature of similar surface. The difference is that this one added no expression-tree building and no new node — it rearranges what exists. The unoptimized build read +5.8, which is why the size gate runs `wasm-opt` first.

**Left open:** `IN (select …)` in SELECT items; the lite-build gates.
</sv-prose>

<sv-prose id="d48">
## Position — user-defined functions: the vectorized ABI, temporal plug-in first (2026-09-19, discussion with David)

**Where it stands.** The founding design (above, "Extensibility") set the shape: a vectorized ABI, one call per vector never per row; Tier 1 JS functions through a single trampoline import; Tier 2 wasm plug-ins with their own memory, vectors copied in and out, shared-memory linking rejected; Tier 0 compile-time Rust; custom aggregates later. Nothing was built. Build log 13 meanwhile settled the one prerequisite this position needs: **Date = i32 days since 1970-01-01, Timestamp = i64 ms since epoch, UTC** — the number `Date.getTime()` produces, so a temporal lane crosses any boundary as itself. The core has `year/month/day/hour/minute/second`, `date()`/`timestamp()`, `strftime`, temporal typing through comparison/BETWEEN/GROUP BY/min-max/pruning. It does not have `date_trunc`, `date_add`/`date_diff`, `weekday`/`week`/`quarter`, or timezones.

**The question David raised** was whether dates are the case to build Tier 2 on, given the encoding is agreed. Yes — with the honest caveat recorded here: the temporal long tail in *core* would cost ~2 KB gz, less than the UDF marshalling itself. The argument for Tier 2 is not the size of these functions; it is that the next fifty function requests (regex at 26 KB gz for `regex-lite` is the one already measured) land outside the binary, and dates are a proving workload with no text on the wire. If the motivation were only "we need `date_trunc`", core would be the cheaper answer and the trampoline would wait for regex.

**The ABI**, refined from the founding sketch by what the engine now looks like:

- **Unit of work**: the vector `eval_call_vec` already evaluates scalar calls over (2,048 values, in practice the segment or group-table lane), so a UDF is one more arm of that function, not a new evaluation path. Dictionary text is decoded to plain vectors before the call, as designed.
- **Typed lanes, not values.** Each argument is a descriptor `{kind, data_ptr, validity_ptr, len}` with kind ∈ {int (i64), float (f64), text (u32 offsets + bytes), date (i32), timestamp (i64)}; the output is one such lane the callee fills, validity included. NULL handling is the callee's: a NULL in, NULL out default is provided by the host for functions that declare `strict`, so most plug-ins never see a bitmap.
- **Declared signature**: registration says `date_trunc(text, timestamp) -> timestamp`. It becomes a dynamic `FuncDef` with `Sig::Udf(params, ret)` in the binder's registry, so a misuse gets the same caret diagnostic `year('x')` gets today, and the return type is what keeps `date_add(d, 30)` a `Date` — still ISO on output, still comparable, still prunable when const-folding lands. Two-arity and variadic are declared, not inferred.
- **Tier 2 entry point**: the plug-in exports `alloc`, `free` and `facetful_call(fn_id, argc, args_ptr, out_ptr, len) -> status`; the JS glue owns the copy (16 KB per lane is microseconds, amortized across the lane). Tier 1 is the same descriptor layout viewed through typed arrays, with the id→function registry in JS — the two tiers share one marshalling arm in the engine, which is where the size goes.
- **First cut is numeric and temporal in and out.** Text lanes are the awkward kind both ways (offsets + bytes, and the output needs a second allocation); they follow when a text UDF asks. This keeps the first arm small and lets the temporal plug-in be the proving case end to end.

**Native execution.** The CLI and the test suite are native Rust; a `.wasm` plug-in there needs a runtime (wasmtime — a dependency the project does not want) *or* the plug-in is also an ordinary Rust crate. Decision: **one crate, two doors** — `facetful-temporal` compiles as a `cdylib` for the browser and as a plain dependency behind a feature for native tests and the CLI (Tier 0, which is why Tier 0 exists). The ABI is exercised in the browser through node-smoke; the semantics are tested natively.

**Size estimate, with the d41 correction applied.** Marshalling arm for int/float/date/timestamp + registry + import: 3–4 KB gz by inspection, so **budget 6–8** — the last four estimates each came in at about 2× (build logs 27–29). Text lanes later: another 2–3, budget 5. The temporal plug-in itself: ~3–4 KB of civil-days math, costing core nothing. Current core is 225.9 KB gz (73%); the whole line fits without touching the lite-build discussion.

**Order.** 0.4.0 first (released today). Then: (1) the ABI with numeric/temporal lanes and Tier 2 loading through `worker.js`; (2) `facetful-temporal` — `date_trunc(unit, ts)`, `date_add(d, n, unit)`, `date_diff(a, b, unit)`, `weekday`, `week`, `quarter`, `age` — natively tested against the civil-days routines and, where SQLite's `strftime` modifiers agree, the differential; (3) text lanes when regex or a formatter asks; (4) Tier 1 JS registration, which is the same arm with a different caller. Custom aggregates stay later; `median` and `stddev` showed each one is an `AggAcc` arm, which is executor code and cannot be a plug-in under this ABI without an init/update/merge/finish protocol — that protocol is the design work if it ever matters.

**Not decided, flagged**: whether a plug-in may declare a function that the core also has (shadowing — probably refused, a clear error); whether temporal plug-in functions should ever move into core once they prove stable (the answer the size budget gives is "only if they're free"); timezones, which are a data question (store UTC, convert at the edge) before they are a function question.
</sv-prose>

<sv-prose id="d49">
## Position — the UDF ABI, Tier 1 first, with regexp() as its proving function (2026-09-19, discussion with David)

Refines d48 after a measurement round. Three things changed: the order (regex before dates), the unit (segment, not 2,048-vector), and text lanes (first cut, not later). The ABI itself is independent of any one function; `regexp()` is the first thing to ride it and the reason text lanes come first.

**Why regex first.** It is the capability gap — the engine has `LIKE` and nothing else for text, while dates already have `year/…/strftime` in core. Its function body is free (the browser's `RegExp`), so the work *is* the ABI with nothing else to debug beside it. And Tier 1 was always the tier to ship first: one import, no second module to fetch. Dates then become Tier 2's proving cargo on numeric lanes, the easy part by then. Caveat recorded: if the dashboard needs `date_trunc`/`date_add` before Tier 2 lands, they go in core (~2 KB) — waiting on a mechanism to ship a 2 KB function is the wrong purity.

**The middle ground for regex nobody had measured is the browser's own engine.** `regex` (349 KB gz) and `regex-lite` (26 KB, PikeVM, 240–576 ns/string) were the two poles. Irregexp, JIT-compiled, measured in node 26 on 200K plant-name-shaped strings: `/^Sand/` 14 ns, `/Dam|Creek|Farm/` 18, `/station/i` 19, `/^[A-Z][a-z]+ \d{3}$/` 21, `/Creek/` 33, a nested-quantifier pattern 44 — the full-crate tier, at zero wasm bytes, in the syntax agents already write. Construction of a never-seen pattern 10–17 µs; interpreter→JIT tier-up worth ≤1 ms once per pattern; a `Map<(pattern, flags), RegExp>` in the worker captures all of it. There is no way to supply precompiled regex code (it is isolate-bound machine code, unexposed), and at 15 µs + 1 ms once it would not matter. Natively the CLI uses the full `regex` crate — no size concern there, linear-time by construction, and a second engine on the reference side is a free differential for the JS path's semantics. `regex-automata` feature-trimmed to the lazy DFA (est. 100–150 KB gz, unmeasured) stays the Tier 2 answer if anyone needs regex without JS in the loop; a hand-rolled lazy DFA (~600 lines, ~10 KB) is the fallback below that and is not expected to be needed.

**Where the cost actually is: making JS strings, not matching, and not transfer.** Numeric lanes are zero-copy views over wasm memory at any size; an import call is ~10–30 ns, so at segment granularity the boundary is unmeasurable. Text: `TextDecoder` over one contiguous 3 MB buffer is **3 ns/row** (5 GB/s) — the 66–122 ns/row first measured was the *JS-side joining of 200K subarrays*, not decoding; `split` into 200K strings is 24 ns/row; the `test()` loop 14–34. Per-string vectorized (one call per segment, a JS loop inside) is therefore ~40–60 ns/row cold and **14–34 ns/row with the decoded strings cached per segment** — a `string[]` LRU beside the mask cache, ~2 MB per 65K-row segment.

**The one-pass scan, measured and then withdrawn.** Decode a segment into one `\n`-joined string, run the regex once with `/gm`, map hit positions to rows: 16–24 ns/row all-in on selective patterns, 4 ns/row when nothing matches — a 2× over the cold per-string path. Then `(\w+\s?)+\d$`, which ran at 44 ns/string per-string, **hung the process**: `\s` consumes `\n`, so the nested quantifier backtracked across 3 MB instead of 15 bytes. Two lessons that hold for any whole-lane convention: a backtracker's worst case is the pattern's property, and what we control is the *input size it applies to* — per-string bounds every pathological pattern to one row; and no separator is unmatchable (`[^x]`, `.` under `s`), so cross-row matches are a correctness hole as well. The one-pass could be guarded (refuse patterns that can reach the separator or have nested quantifiers; verify hits contain no separator; fall back) — but it buys 2× only on a *cold, non-dictionary, large* text column, the rare case, since with the string cache warm per-string equals it. **Withdrawn from the first cut**; recorded as a measured optimization with its hazards named, for a workload that shows the cold case mattering.

**The ABI.**

- **Registration** (JS, in the worker — functions do not cross `postMessage`, so `registerFunction(name, signature, source)` takes source text or a module URL; the main-thread API forwards it): `signature` = `{ params: [kind…], returns: kind, strict?: bool, variadic?: bool }`, kind ∈ int | float | text | bool | date | timestamp. The worker assigns `fn_id`, keeps `id → function`, and calls `udf_register(name_ptr, len, fn_id, params_ptr, argc, ret_kind, flags)` on the wasm, which adds a **dynamic `FuncDef`** with `Sig::Udf { params, ret }` beside the static `FUNCS` table. The binder therefore treats a UDF like any scalar: arity and kind checks with the same caret diagnostics `year('x')` gets, return type known at bind time, so `date_add(d, 30)` *is* a `Date` — ISO on output, comparable, and prunable when const-folding lands. Shadowing a core name is refused.
- **Invocation**: one new import beside `opfs_read`: `udf_call(fn_id, argc, args_ptr, out_ptr, len) -> i32`. `eval_call_vec` gains one arm: when the bound call's `FuncDef` is a `Sig::Udf`, it materializes each argument `VV` into a **lane descriptor** in wasm memory — `{ kind: u8, data_ptr: u32, aux_ptr: u32, valid_ptr: u32 }` — and an output descriptor of the declared kind, then calls the import; on return the output lane becomes the arm's `VV`. `len` is the evaluation unit's row count: a **row group (≤65,536) in WHERE, the group table in SELECT/HAVING, the dictionary for a dict column** (see below). Nothing in the executor changes shape; a UDF is a scalar `Call` whose body happens to live across the boundary.
- **Lanes**: int → i64 lane (dates as i32 days, timestamps as i64 ms, tagged by kind so JS can wrap them as `Date` on request); float → f64; bool → u8; text → **contiguous UTF-8 bytes + u32 offsets (`aux_ptr`)**, which is a text segment's storage already, so the copy is at most a concatenation; validity → the bitmap the engine holds, or null for all-valid. `Codes` (dictionary) lanes are handed over as the *dictionary* text lane with the codes lane alongside — the JS side evaluates once per distinct value and the engine gathers the result through the codes; for a predicate this is a bitmap over codes feeding the existing mask machinery, so a 4M-row dict column costs one pass over ≤65,536 strings. Output text (for text-returning UDFs) is written by JS into wasm memory via `alloc`: bytes + offsets, the same shape back.
- **NULLs**: `strict` (default) means the host skips NULL inputs and NULL-fills the output; JS sees validity only if it declares `strict: false`. Errors: the import returns non-zero and the worker keeps the message; the query fails with a diagnostic pointing at the call.
- **JS side**: `Float64Array`/`BigInt64Array`-free — i64 lanes are exposed as two `Int32Array` views or as `Float64Array` when the signature says the values fit (dates/timestamps/ordinary ints do; the engine already keeps ms as f64 across the boundary elsewhere). Text lanes are decoded once per (segment, column) into a cached `string[]`, then passed as an array to the function, which returns an array or fills a typed array. A UDF is called with `(args: lanes[], len, out)` — vectorized by contract; a per-row helper wrapper exists for lazy authors and is documented as slower.
- **Tier 2 later** is the same descriptor layout with the copy done by the glue into the plug-in's memory, and `udf_call` routed to `facetful_call` in the plug-in. Tier 0 is the same `FuncDef` registered from Rust. One marshalling arm serves all three, which is where the bytes go.

**`regexp(text, pattern[, flags]) -> bool`**: the pattern and flags must be literals (bind-time check, like `group_concat`'s separator), which keys the `RegExp` cache and the mask cache; the JS body is `re.test(s)` per string over the cached decoded lane; on a dictionary column it runs over the dictionary. Native: the `regex` crate behind the CLI (never the wasm), with ECMAScript-vs-Rust syntax differences pinned by the differential for the patterns agents write.

**Size budget**: marshalling arm (5 kinds in, 5 out) + dynamic registry + import + `regexp` bind rule: 3–4 KB gz by inspection, **6–8 budgeted** (build logs 27–29's 2×). Core is at 225.9 KB (73%).

**Order**: (1) engine `Sig::Udf` + dynamic registry + `udf_call` arm + lane descriptors; (2) worker registry, decoded-lane cache, `registerFunction` end to end in node-smoke; (3) `regexp()` in JS and `regex` in the CLI, tests + differential; (4) design entry with measured size and speed; then d48's Tier 2 loader with the temporal plug-in.
</sv-prose>

<sv-prose id="d50">
## Build log 31 — the UDF ABI lands: Tier 1, segment-granular lanes, regexp() in JS and native (2026-09-19)

d49 built as written, with two small deviations recorded below.

**Engine** (`facetful_engine::udf`): a registry of `&'static FuncDef`s with the new `Sig::Udf { id, params, ret, strict }` shape, consulted by the binder after `FUNCS` — so a UDF binds like any scalar: arity, per-argument `coerces_to` checks with the same caret diagnostics, return type known at bind time (`plus(x, 1)` on a float `x` says "argument 1 needs int, this is float"), unknown-name suggestions include registered names, built-in names are refused. `Host` is a trait with one method, `call(id, &[Arg], len, &mut Output)`; the evaluator's single dispatch point (`eval_vec`'s `Bound::Call` arm) routes `Sig::Udf` to `udf_call_vec`, which evaluates the argument vectors as usual and hands them over as they are — numbers (ints, days, ms, bools) as f64, text as offsets + bytes, literals as one broadcast value, validity as the bitmap — and turns the output lane back into a `VV`. **The dictionary path** is where it pays: when one argument is a dictionary column and every other is a literal, the call runs over the dictionary and the result is gathered through the codes. A failing body is noted mid-scan and becomes the query's error afterwards, with the mask cache cleared so nothing evaluated with it survives. Re-registering a name gets a fresh id, and since a conjunct's cache key is the bound tree's Debug form, masks built with the old body are never served for the new one.

**wasm**: a second import beside `opfs_read` — `udf_call(id, argc, args, out, len)` — and exports `udf_register(name, params, ret, flags)`, `udf_unregister`, `udf_error`. Lane descriptors are 8 u32 words (kind, len, flags, data, aux, valid, bytes, err); for a text *output* the engine allocates the offsets and JS allocates the bytes with `alloc`, handing the pointer back in the descriptor; an error is the same trick with a message. **JS** (`core.js`): `Engine.registerFunction(name, sig, fn)` and the `_udfCall` marshaller — typed-array views over the numeric lanes (zero copy), text decoded with one `TextDecoder` call when the bytes are ASCII (decoded length == byte length, so byte offsets are char offsets) and per string otherwise, `perRow: true` wrapped into a loop with plain values and nulls. The worker takes the function's *source* (`fn.toString()` from the main thread, or `{ moduleUrl }`), because functions do not cross `postMessage`.

**Native**: the CLI registers `regexp(text, text) -> bool` at startup through the full `regex` crate (`native_udfs()`), compiled once per pattern. The browser gets the same SQL from `RegExp` in twelve lines (node-smoke registers it that way); both sides agree with `LIKE` on the patterns both can spell — a CLI test pins three, the smoke test one.

**Measured**, 200K spike, node 26. Every text column in the spike is dictionary-encoded (even `owner`: 1,981 values), which is the shape our data has, so `regexp()` runs over dictionaries: `regexp(owner, '^owner_2.*7$')` **2.5 ms cold / 0.29 cached** against the engine's own `LIKE 'owner_2%7'` at 2.3 / 0.25 — the cold cost is the mask machinery, not the regex; a pattern matching nothing is the same 2.5. The boundary itself: `plus(capacity, k)` vectorized over 200K floats 1.95 ms vs `capacity + k` in the engine 1.76 — **~1 ns/row for the whole wasm→JS→wasm round trip**; `perRow` 26 ms, 13× slower, documented as the convenience it is. A cached `regexp` predicate reruns in 0.26 ms. 91 tests (two engine UDF tests through a Rust host, one CLI test), node-smoke covers registration, vectorized and per-row functions, text out, strict NULLs, a throwing body, bind-time type errors and unregister. Gate clean.

**Size: +7.3 KB gz (225.9 → 233.1, 75%).** Against d49's 3–4 by inspection and 6–8 budgeted: inside the budget, twice the inspection — the fifth time running. `udf_call_vec` inlined into `eval_vec` (now 10.6 KB raw), the wasm host and exports the rest. The correction factor is now a rule: **resolver- and boundary-shaped code costs 2× what it reads as.**

**Deviations from d49**: (1) no decoded-lane cache on the JS side yet — lanes are ephemeral pointers with no segment identity in the ABI, and the dictionary path made it moot for every column we have; it returns if a non-dictionary text column ever shows up cold and hot. (2) Bools travel as f64 in, u8 out — one numeric view in JS for inputs, the engine's own bool lane for outputs; harmless, noted for the Tier 2 layout.

**Next on this line**: Tier 2 (a plug-in `.wasm` with its own memory behind the same descriptors) with the temporal plug-in as its cargo; `IN (select …)` in SELECT items; the lite-build gates when size asks.
</sv-prose>

<sv-prose id="d51">
## Position — one `npm install`: the streaming converter, Node as the second runtime, packaging (2026-09-19, discussion with David)

**What prompted it.** Three threads met. (1) UDF examples: the functions cheap in JS and dear in wasm are JSON (`JSON.parse` vs 80–150 KB of serde), the whole `Intl` surface (timezones — `chrono-tz` is ~200 KB of IANA data the browser already has; `DisplayNames`, `NumberFormat`), Unicode normalization and `\p{…}` classes, URL parsing. Hashing and compression, the intuitive candidates, are promise-only in browsers (`SubtleCrypto`, `CompressionStream`) and our import is synchronous, so they are out. (2) That list made Tier 2's proving case evaporate: the temporal long tail is a few lines each over `Date.UTC`/`getUTC*`, and timezones — the one thing dates need that wasm can never afford — come with `Intl`. **Tier 2 is parked** with its residual case named: compute-heavy code that already exists in Rust (geospatial, statistics, a wasm regex for someone who wants no JS in the loop); the ABI is shaped for it (same descriptors, copied into another memory) and nobody is asking. `date_trunc`/`date_add`/`date_diff` still go in core eventually (~2 KB, both sides, prunable later); JS owns the long tail and timezones. (3) Then the question that reframed everything: how does a user *get* the converter? Answer, checked: they don't. The release publishes npm only; the README's "quickstart" is `cargo build --release`. **Nobody without a Rust toolchain has ever converted a CSV.** Parquet's `openParquet` is the only ingest that ships.

**Decision: everything is one `npm install facetful`.** The package carries the browser library (as now), a `facetful` command (`bin`), and a Node API — converter, `query` with `--udf`, `materialize`. Runtimes, in order:

1. **Node on the wasm build first.** The engine module is the same one the browser loads — same V8, same irregexp, same ICU, so a JS UDF behaves identically by construction, not by re-implementation. Large files work through the existing `opfs_read` import: positional read, which in Node is `fs.readSync(fd, buf, 0, len, offset)`. Zero build matrix.
2. **A `napi-rs` native addon next** (PyO3 + maturin, for Node): `#[napi]` bindings, `optionalDependencies` per platform (six to eight targets, cross-compiled with `cargo-zigbuild` against glibc 2.17 — the manylinux move), **the wasm as the fallback** when no native package loads. N-API is ABI-stable, so one binary per platform for every Node version — abi3 for everyone. It buys native conversion speed for very large files and in-process queries; paid for against a demonstrated need, not up front. The Rust CLI stays as the cargo artifact for the non-Node audience and the native reference (benchmarks, the SQLite differential, `regexp` through the `regex` crate); a `cargo install` / GitHub Release binary for it is nearly free and overdue.

**The converter is the prerequisite for all of it, and it is David's next feature anyway: streaming.** Today both paths hold everything — the CLI as per-column `Vec<String>` (5–10× the file resident), the browser as typed columns. The format never required that: the `Writer` is header → dictionary block → self-framing groups → footer, streaming by construction. What forces whole-file residency is two decisions that need every row before group 0: **type inference** and **dictionaries** (global, in the header). So: **two passes, bounded memory.** Pass 1 streams the file through a per-column inference state machine (int → float → date → timestamp → text, plus int min/max for narrowing) and a bounded distinct set (stop at 65,535 and mark the column plain text). Pass 2 streams again, encodes against the decided plan, emits a row group every 65,536 rows straight to the sink. Memory is O(distinct values) then O(one group); a 10 GB CSV converts in ~100 MB. Two reads of the file — cheap on disk, and how DuckDB's sniffer works with a larger sample.

**Written sans-I/O, or wasm would hinder it.** The converter is a state machine fed bytes and drained of output: `CsvReader::push(bytes)` yields rows across chunk boundaries; `Sniffer::row()` / `finish() -> Plan`; `Encoder::row()` / `take_output()` / `finish()`. The `Writer` gains `take_output()` (pull model; `finish()` unchanged for existing callers). Then: native feeds it from a `BufReader` and writes to a `File`; wasm exports `convert_begin/feed/pass2/take/finish` and Node feeds it from `fs.readSync` chunks, the browser from `File.stream()` into an OPFS handle; napi later wraps the native loop. Same Rust, same algorithm, I/O injected. The design that *would* have hindered streaming is one written around `std::fs::File` — the current one. The wasm target just makes it impossible to write that again. Parquet gets the same shape for free: hyparquet already yields one row group at a time.

**Where the converter lives in the wasm**: measured, not decided. If the streaming CSV path costs the browser module little (≤ ~10 KB gz), it ships in the one module — a browser that can open a local CSV is a feature, and one artifact is simpler than two. If it costs more, a `tools` feature builds a second, unbudgeted module for Node only. The size gate stays on the browser module either way.

**This closes 0.4.** UDFs (Tier 1), the streaming converter, the Node CLI on wasm, one install. Then 0.5: the napi addon, `date_trunc`-family in core, the derived-cache and `IN`-in-SELECT items, and Tier 2 only if a plug-in appears.
</sv-prose>

<sv-prose id="d52">
## Build log 32 — the streaming converter, and `facetful` as an npm command (2026-09-19)

d51 built. The converter first, because everything else sits on it.

**`facetful-format::stream`** — sans-I/O, two passes. `CsvReader::push(bytes, on_row)` turns any chunking into rows (quotes and `""` escapes across chunk boundaries, CRLF, a leading BOM, an unterminated last row). `Sniffer::row()` runs the per-column state machine — int (with min/max for narrowing), float, date, timestamp, else text — and a first-appearance distinct map capped at 65,535 entries, dropped the moment it overflows; `finish()` applies `compile::plan`'s rules exactly (int → float → date → timestamp over non-empty cells; text is a dictionary when `distinct × 2 < rows`). `Encoder::row()` encodes against that plan into the current group and writes a group every `group_target` rows; `take_output()` hands finished bytes out; `finish()` writes the footer. The `Writer` gained `take_output()` — a `flushed` counter so group offsets stay absolute — and `finish()` is unchanged for existing callers. **The streamed image is byte-identical to the whole-column compiler's** for every chunk size and group size tested, which is the test: one oracle, no second opinion about the format.

**Native CLI.** `convert` reads the file twice in 1 MB chunks and writes each group as it completes. Measured against the previous binary on the 200K-row spike CSV (13 MB): **165 MB → 22 MB** peak RSS, same bytes. On a 3M-row, 198 MB CSV: **2,361 MB → 22 MB**, and faster, 3.72 → 2.65 s — the old path's `Vec<String>` per cell was 12× the file resident; the new one is 0.1× and flat. That was the feature David wanted next, and it fell out of the packaging question.

**wasm.** `convert_begin/feed/pass2/finish`, `convert_output_len/copy` to drain, `convert_schema`, `convert_error`, `convert_free` — the same state machine, fed from JS. **+7.4 KB gz (233.1 → 240.5, 78%)**, under d51's ~10 KB line, so it ships in the one browser module: a browser can now open a local CSV. `core.js` `convertCsv(chunksFactory)` drives the two passes over any async iterable (a `File.stream()` twice, or one buffer); the worker's `loadCsv(name, fileOrBuffer, {persist})` opens the result as a table. The image is assembled in JS memory for now — a streaming OPFS append sink is the obvious next step when a browser CSV outgrows that.

**`facetful` the command** (`bin/facetful.mjs`, on the same wasm): `convert`, `query` (one-shot or REPL, `--table name=path`, `--udf module.mjs`), `materialize`. Tables open lazily — the browser's `opfs_read` import is `fs.readSync` here, so large files never load whole. Node conversion of the 3M-row CSV: 4.2 s, 74 MB RSS including Node itself, byte-identical to native. **Checked the way a user would**: `npm install ./facetful-0.4.0.tgz` into an empty project, `npx facetful convert`, `npx facetful query … --udf node_modules/facetful/udfs.js` — one install, library and command, 266 KB packed.

**`facetful/udfs`** — ten ready-made functions as the d51 examples: `json_extract` (path over `JSON.parse`), `to_tz` (Intl's IANA tables), `date_trunc`, `date_add`, `weekday`, `quarter`, `country_name` (`Intl.DisplayNames`), `format_number` (compact/locale), `unaccent` (NFD + `\p{M}`), `url_host`. Each is a few lines over what the browser ships; the smoke test pins an answer for every one (`to_tz(… 'America/New_York')` = `08:00:00` for noon UTC in July, `date_add('2024-01-31', 1, 'month')` = Feb 29). One real semantics note from writing them: `variadic` means "the last parameter repeats", so an *optional* argument is declared as one required parameter plus `variadic: true` — `country_name('de')` and `country_name('de', 'fr')` both bind.

**94 Rust tests, node-smoke, the Parquet differential, size and bench gates clean.** READMEs rewritten around the one-install story; the root README no longer presents `cargo build` as the quickstart.

**Left, named**: an OPFS streaming sink for browser CSVs; `inspect` and `--bench` in the Node command (the native CLI keeps them); the `cargo install` / GitHub Release binary for the non-Node audience; then 0.5's napi addon behind the same command. This closes 0.4's feature list.
</sv-prose>

<sv-prose id="d53">
## Position — the Rust CLI is a development tool; versions (2026-09-20, discussion with David)

**The evidence was the code.** `regexp_extract`/`regexp_replace` took ten lines each in `udfs.js` and forty in the native CLI's host — a second implementation, a `$<name>`→`${name}` rewrite, flag-letter translation — whose only purpose was that the Rust binary could answer SQL the Node command already answers. David: the native version "is a bit of a waste of time now … the only real benefit is profiling and having something to compare against." Correct. What the Rust CLI uniquely does is development work: the SQLite differential, `--bench` and the perf baseline, `inspect`, a place to profile. Native conversion is 1.6× the wasm-under-Node time (2.65 vs 4.2 s on 3M rows), which no user will notice.

**Decision.** The `facetful` npm command is *the* CLI. The Rust CLI is a development tool: the native UDF host, the `regex` dependency and its test are removed; it knows no user-defined functions, and the "same SQL on both sides" promise is retired for it — it was only ever made for its own benefit. The root README says so. The `cargo install` / GitHub-Release binary idea is dropped. Native `convert` stays (the same `stream` module with a `File` around it; the bench suite needs images). Native speed for users arrives, if it is ever needed, as the napi addon inside the npm package — never as a separate binary.

**Signature note from the same day**: "variadic = the last parameter repeats" could not express `regexp_extract(s, pattern[, group])` with `group` a number *or* a name. Signatures gained `optional: n` (trailing parameters that may be omitted) and a parameter kind `any` (`Ty::Null` in the engine; the lane still carries its real kind). `country_name`, `regexp`, `regexp_extract`, `regexp_replace` use them. List-shaped results (`regexp_extract_all`, `regexp_split`) wait for a native list type rather than being faked as JSON text.

**Versions.** 0.4.0 is what is staged on npm: joins, CTEs, subqueries, EXISTS. **0.5.0 is everything since**: the Tier 1 UDF ABI and the thirteen default functions, the streaming converter, the `facetful` command, `loadCsv`, one `npm install`. **0.6** is the napi addon behind the same command. Tier 2 stays parked.
</sv-prose>

<sv-prose id="d54">
## Build log 33 — 0.5.1: the worker never ran under a gate (2026-09-23)

**The bug.** `worker.js` defined `setTable` as a function that called itself instead of `tables.set`, so every table load through the worker — `load`, `loadOpfs`, `openParquet`, `loadParquet`, `materialize`, `loadCsv` — died with "Maximum call stack size exceeded". It shipped in 0.4.0 and again in 0.5.0. Every gate (node-smoke, the parquet differential, the `facetful` command) talks to `core.js` directly; the browser path through `index.js` → worker was exercised only by hand. Fixed in 0.5.1, a one-line change plus the version bump.

**The gate gap is closed.** `node-worker-smoke.mjs` drives `worker.js` in Node through its own message protocol: it supplies `self`, `postMessage` and a file-reading `fetch`, imports the worker, and sends `init`, `load`, `query` (with a ready-made function), `materialize` and a `FROM` on the derived table, a two-table join, `loadCsv` from a buffer, `registerFunction` from source text, and the two error shapes (missing table, SQL error with `isQueryError`). Run against the 0.5.0 worker it fails at `load`. The release gate runs it after the parquet differential. OPFS commands stay untested here (no `navigator.storage` in Node); the browser remains the only place they run.

**Rule.** A module that ships must have a gate that imports *that module*, not the one beneath it.
</sv-prose>

<sv-prose id="d55">
## Position — results carry dictionaries; projection evaluates survivors only; the browser is a gate (2026-09-24, from a gratnav handoff)

**The ask.** gratnav's grid loads its entire filtered result — up to all 1,520,378 grants — through `columnRaw` after every filter change, and David wants to keep that. The payload is 255.7 MB, and most of it is repeated text: `Funding Org:Name` (372 distinct values) costs 52 MB as one string per row, `Best Available Region` (14 values) 23 MB. Both are dictionary columns in the image. The engine expanded them on output, so a value repeated 1.5M times was built, copied out of wasm memory (`core.js` `.slice()`s every column while the wasm result is still alive, so the worker briefly holds both) and transferred 1.5M times. Handoff: `~/projects/gratnav/docs/handoff-facetful-dict-results.md`.

**Decision 1 — the columnar channel keeps codes.** `OutCol::Dict { codes: Vec<u16>, dict: Rc<Vec<Rc<String>>>, valid }` joins the output enum. A dictionary column selected as-is (plain, sorted and grouped paths alike — a GROUP BY key is already codes in the group table) gathers its codes at memcpy speed; no string is touched. Every consumer is better off, not only the one that asked: `materialize` feeds the codes straight into the compiler as `InCol::Dict` (no strings built, no re-encoding; the compiler applies its usual payoff test, so two rows over a three-entry dictionary still land as text), `ensure_rows` decodes lazily for the CLI and the differential, and the wasm side expands to per-row offsets/bytes only when the JS caller asks for that form. The engine has no option for this; the choice lives at the boundary.

**Decision 2 — the JS opt-in is `query(sql, { dictText: true })`.** Off, nothing changes shape: `col_offsets_ptr` on a dictionary column expands it inside the wasm, once, on demand. On, `columnRaw` carries `codes` (Uint16Array) + `dict` ({ offsets, bytes }) for dictionary-backed text columns; computed text is still per row. `rows()`, `column()` and the new `Result.dictionary(name)` decode through the dictionary, each entry once — cheaper per scroll frame than decoding a string per row. The dictionary is **compacted** to the values present (linear in rows + entries, no string copied, image order kept): a ten-row filtered result does not carry a 60,000-entry dictionary, and codes are dense. `Uint32Array` is reserved in the type for dictionaries past 65,535 — the handoff's Stage 2 (dictionary-encoding plain text columns in the result builder, worth another 46 MB on gratnav's title and recipient) waits until gratnav has measured Stage 1; it needs u32 codes and a hash pass over every string, and the cheap win is taken first.

**Measured** (grantnav.facetful, 1.52M rows, whole grid result in Node): 255.7 MB → 186.7 MB, and the query itself 436 → 382 ms — expanding two dictionary columns was 50 ms of the old query. The handoff predicted ~187. Size: +2.6 KB gz.

**Decision 3 — select expressions run on survivors only.** The handoff's second finding was correct and worse than described: a paged grid query (`order by … limit 50 offset 800000`) took 65 ms bare and 530 ms with `substr(coalesce(…), 1, 140)`, because every path evaluated select expressions over every kept row before the window was known — the full-sort path at scan time, the plain path at scan time, the top-k path over whole groups at finish. The fix is one rule: **when the query has a window (a LIMIT), expressions are evaluated only for the rows in it.** Plain and full-sort keep each scanned group's lanes (Rc clones — nothing loads twice) and mark expression slots deferred; at finish the surviving rows are gathered out of their groups into small contexts and the expressions run over those. Top-k does the same with its winners instead of whole groups. Direct columns (text, dictionary, numeric) never needed this; they gather by reference. Without a LIMIT nothing changes: every row is output, so eager evaluation frees a group's inputs as the scan goes, which is the right trade for memory.

**Decision 4 — the browser is a gate.** The 0.4.0/0.5.0 worker bug (d54) and this feature's changes to `core.js`, `worker.js` and `transferables` share a lesson: the worker and the page-side API must run under a gate, in a browser, with the real APIs Node cannot imitate — module workers, transferables, OPFS sync access handles, `Blob.stream()`. `browser-smoke.mjs` drives headless Chromium over the DevTools protocol (the pattern gratnav's `tests/cdp.mjs` uses; no dependency), serves the repo over localhost, and calls the public API as a user does: open, load, query, `dictText`, materialize, loadCsv from a Blob, storeOpfs/loadOpfs/removeOpfs, registerFunction, close. The release gate runs it, and so does CI on every push — the JS side had green CI for four days with a dead worker because only the release workflow ran the JS checks.
</sv-prose>

<sv-prose id="d56">
## Build log 34 — dictionary results, windowed projection, the browser gate (2026-09-24)

**Dictionary results (d55, decisions 1–2).** Engine: `OutCol::Dict`, `SelSrc::Dict` (plain/sorted/grouped), `compact_dict`, `outcol_val` decoding; `materialize` maps it to `InCol::Dict`, and the compiler now applies its payoff test (`dict.len() * 2 < rows`) to pre-encoded input too — a dictionary of 3 over 2 rows lands as text, as it did when the strings went through. wasm: `ColBuf` grows `codes` + a compacted `dict_offsets`/`dict_bytes`; `col_offsets_ptr`/`col_bytes_ptr` expand on demand (memoized), new exports `col_dict_len`, `col_codes_ptr`, `col_dict_offsets_ptr`, `col_dict_bytes_ptr`, `col_dict_bytes_len`. JS: `core.js query(handle, sql, { dictText })`, `transferables` carries the three new buffers, the worker passes the flag, `index.js` `query(sql, { table, dictText })` + `Result.dictionary(name)` with the dictionary decoded once per result; types and README updated. Tests: `tests/dict_results.rs` (every path keeps codes; NULL codes ignored by compaction; materialize with and without payoff), node-smoke (decode equality, compaction, transferables), the worker smoke, the browser smoke.

| grantnav.facetful, 1,520,378 rows, whole grid result in Node | text | dictText |
|---|---|---|
| payload | 255.7 MB | 186.7 MB |
| query | 436 ms | 382 ms |
| funder (372 values) | 52 MB | 3 MB |
| region (14 values) | 23 MB | 3 MB |

**Windowed projection (d55, decision 3).** `GroupCtx` lanes can be `gather`ed at chosen rows (`gather_ctx`, `VV::gather`; text copies only the chosen strings). Plain and full-sort take `defer` = the query has a LIMIT: expression slots become `SelSrc::Deferred`, each scanned group's `Cols` map is kept (Rc clones), and `project` evaluates the deferred expressions over per-group sub-contexts built from the surviving refs. Top-k gathers each group's lanes at its winners before evaluating. The SQLite differential gained three windowed queries with computed columns and NULL inputs across the three paths (39 queries agree).

| grantnav.facetful, 50-row page | bare | + `substr(coalesce(…), 1, 140)` before | after |
|---|---|---|---|
| `order by date desc … offset 800000` (full sort) | 63 ms | 530 ms | 78 ms |
| `order by date desc … offset 50000` (top-k) | 50 ms | 224 ms | 56 ms |
| `limit 50 offset 800000` (plain) | 13 ms | 215 ms | 18 ms |

The handoff measured 508 vs 79; the 430 ms it worked around is gone. Without a LIMIT nothing changed (whole-result query 426 → 426 ms).

**Browser gate (d55, decision 4).** `js/facetful/browser-smoke.mjs`: a Node static server for the repo, headless Chromium found on PATH (`FACETFUL_BROWSER` overrides; `--no-sandbox` under `CI`), DevTools over WebSocket, a generated page that imports `index.js` and runs open → load → query with a ready-made function → `dictText` (codes, `dictionary()`, `column()`, `rows()`) → materialize → join → `loadCsv` from a Blob (two passes over `Blob.stream()`) → storeOpfs/loadOpfs/removeOpfs → registerFunction → the error path → close, reporting per-step times. Against the 0.5.0 worker it fails with the shipped error, "Maximum call stack size exceeded". The release gate runs it after the worker smoke, and `ci.yml` gained a `js` job that runs the whole gate on every push (Chrome is on the runner image).

**Size.** Unoptimized 246,084 → 253,161 gz (+7.1 KB: +2.6 dictionary results, +4.5 windowed projection). wasm-opt is not on this machine any more; CI measures the optimized build — expect ~247.5 KB, 81% of budget.
</sv-prose>

<sv-prose id="d57">
## Build log 35 — gratnav confirms; the worker's peak; text gathers off the image (2026-09-24)

**gratnav's numbers** (its `tests/memload.mjs`, headless Chromium, real worker, page ArrayBuffers after a forced GC, 0.5.1 vs the d56 build): all 1.52M rows 259 → 190 MB and load 575–615 → 495–565 ms; region=London 37 → 29 MB; two years 59 → 44 MB. Shape accepted as is. It keeps its paged-query workaround until a version is published. Its finding: Chrome's peak barely moved (2,179 → 2,131 MB) because the bulk is the worker — the 512 MB segment cache it asks for, and the wasm heap's high-water, which grows to hold each result plus the copy-out and never shrinks (`WebAssembly.Memory` cannot). It suggested building results straight into transferable buffers; wasm can only write into its own memory, and that memory cannot be transferred, so the lever is the high-water itself.

**Where the high-water was.** Measured in Node on grantnav.facetful, wasm memory growth per query: `count(*)` +0; one numeric column sorted +64 MB for a 12 MB result (refs, packed keys, the lane, the output); dictionary + numeric columns +93 for 30; **three text columns +421 MB for a 148 MB result, sorted or not (+403)**. Text was held twice: every group's text lane copied at scan (the same size as the output) and kept until finish, then the output gathered from it. The top-k path had already avoided this (`with_text_segments`, gathering off the borrowed image segments).

**Decision — a plain-text column selected as-is never gets a lane.** `Shared` marks such select items (`direct_text`) and leaves them out of the lanes to load; `sel_srcs_for_group` gives them `SelSrc::Segments(col)`; `project` gathers them at finish straight off the resident image segments, two passes per group (lengths and validity, then the bytes into place), allocating nothing but the output. A non-resident segment (tiny positional-cache budgets) falls back to a loaded lane for that group. Plain and full-sort record the row group behind each slot and take `&mut Table` at finish. The aggregate path is untouched (its keys live in the group table). Measured: three text columns sorted +421 → +200 MB and 467 → 347 ms; the whole grid result's wasm high-water 791 → 582 MB; whole-result query 436 → 358 ms (dictText 382 → 315); a plain 50-row page at offset 800,000 13 → 3 ms — the scan copies nothing for the columns it only displays.

**A bug the measurement exposed, and a rule.** The d56 build expanded a dictionary column to per-row text on demand, inside `col_offsets_ptr`. That allocation grew the wasm memory *after* `core.js` had taken its `ArrayBuffer` view, which detached the view: "Cannot perform Construct on a detached ArrayBuffer" on the first large text-form query after a dictText one. The spike's results are too small to grow memory, so no smoke saw it; the grantnav bench did. Fix: `query_run_opts(t, sql, len, flags)` (bit 0 = keep dictionaries) does every allocation the result needs inside the call, and **no `col_*` getter allocates** — stated in the wasm source. `core.js` also fetches pointers before taking its memory view, so a future slip fails the other way round rather than detaching a buffer. `query_run` remains as `query_run_opts(…, 0)`.

**gratnav's confirmation of this build** (same harness): full 1.52M-row load 495–565 → 390–465 ms, London 160 → 99 ms, two years 187 → 126–130 ms; steady Chrome RSS holding all rows ~2,080 → ~1,850–1,900 MB, holding London ~1,780 → ~1,580–1,630 MB; peak over five loads 2,131 → 2,045 MB (peaks are noisy ±100 MB, the steady state is consistent); no detached-buffer error in any run. By process while all rows are held: renderer 1,229 MB, and ~630 MB of Chrome's own fixed overhead across browser, zygote, GPU, network and storage — so the worker is roughly 0.9–1.0 GB, consistent with the 582 MB high-water plus resident segments. Its `cacheBytes` 512 → 256 MB changed nothing measurable, so the segment budget is not what fills memory here. It asked for a way to read the worker's wasm size from the page (attaching DevTools to a dedicated worker was flaky): `db.memoryStats()` → `{ wasmBytes, tables }`.

**The worker's exact number** (gratnav, `memoryStats().wasmBytes` right after the whole result landed, a fresh worker per reading, so this is the high-water of that page load's whole query set — facets, summary, count, first page, grid): all 1.52M rows **520 MB**; region=London (215K rows) 401 MB; two years (330K rows) 348 MB; identical across fresh and warm profiles. Renderer with all rows held: 1,222 MB = worker 520 + page result buffers 190 + ~510 MB of V8 isolates, DOM, compositor and app JS (the JS heap itself ~7 MB); Chrome's other processes ~615 MB. Peak RSS 1,857 MB warm, 2,018 MB fresh (the download-and-store path leaves ~110 MB behind). Open oddity: London's high-water exceeds the larger two-year result's, so something other than the grid query — likely a facet or summary query under that filter — sets it; not isolated yet. The 357 MB image copied into wasm accounts for two thirds of the 520; the whole-result working set is the rest.

**What remains of the worker's peak** is the working set of a 1.5M-row sorted projection: output (187 MB with dictText), refs and packed keys (~40 MB), the ORDER BY lane, plus the image itself and the caller's cache budget. Stage 2 (u32 codes for plain text) would take the output down further; the doubling is gone. Size: 254,911 gz unoptimized (+1.7 KB for this step; +8.8 KB since 0.5.1).
</sv-prose>

<sv-prose id="d58">
## Build log 36 — Stage 2 as `dictText: "all"`, `inspect` and `describe()`, repo hygiene (2026-09-24)

**Stage 2 is a second level of the option, not the default.** With `dictText: true` the engine hands over what it already has (the image's dictionary columns as codes) and pays nothing. Encoding a plain text column means a hash pass over every value; measured per column on grantnav (1.52M rows): title (664K distinct) 36 → 147 ms, recipient (426K) 28 → 117 ms, an image dictionary column 28 → 11 ms. On the whole grid result that is 315 → 551 ms for 186.7 → 140.8 MB. That trade belongs to the caller, so it is `dictText: "all"` (wire flag bit 1), and the numbers are in the type's doc comment. gratnav had ranked Stage 2 below the worker's peak, which d57 took; it can now measure the two levels against each other.

**How the pass is made cheap enough.** Keys are 64-bit hashes of the strings through an identity hasher (probing compares 8-byte keys, each string hashed once; a collision between different strings makes the column fall back to text, so the encoding is exact). Giving up early matters more than hashing speed: a probe of 4,096 rows **spread evenly** over the column abandons any column nine-tenths distinct — the identifier column costs 32 ms either way now, against 138 with a half pass. A head sample did not work: sorted by date, the first 4,096 recipients look unique although 72% of the column repeats; the spread probe encodes it. The distinct count also stops the pass the moment the payoff rule (`dict.len() * 2 < rows`, the compiler's) cannot hold. Codes are u16 when the dictionary fits, u32 past 65,535 (`col_codes_width`); `RawColumn.codes` is `Uint16Array | Uint32Array` as the type already said. `true` and `"all"` differ in nothing else — dictionary order for image columns, first appearance for encoded ones.

**`inspect` returns, and `describe()` with it.** The Rust CLI's `inspect` was lost when the command moved to Node (d53). `table_describe` in the wasm emits the catalog as JSON — version, rows, row groups and target, sort keys, and per column its kind (as the converter names it), on-disk bytes across groups, null count, min/max folded over every group's stats (the Rust CLI showed group 0's only), and the dictionary's size. `facetful inspect file.facetful [--json]` prints it as a table with human sizes and ISO dates; the gate checks it on the spike image. The same call is `db.describe({ table })` on the public API (`TableInfo` in the types), covered by the worker and browser smokes — a page can now ask a loaded table what its columns are without guessing from a query.

**Repo hygiene.** Twenty-two empty files — `.bashrc`, `.gitconfig`, `.zshrc`, `.mcp.json`, the `.claude/…` entries — had been tracked since the first commit: a sandboxed `git add -A` saw the sandbox's `/dev/null` masks of those home-directory paths as empty files. Untracked and ignored by name; `.gitmodules` among them was also why git warned "unable to access .gitmodules" on every command.

**Size.** 258,977 gz unoptimized: +4.1 KB for this step (the hash pass, the probe, `table_describe`), +12.9 KB since 0.5.1 before wasm-opt — about 82% of budget optimized. The release is what the tree holds: d54–d58.
</sv-prose>
