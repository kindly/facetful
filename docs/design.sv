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
