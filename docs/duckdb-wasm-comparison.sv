<sv-page label="Facetful vs DuckDB-WASM">

<sv-prose id="facetful-mcp-direction">
## A second delivery surface: interactive analytics in MCP Apps

The author's intended return includes professional visibility and practical value to the open-data publishing community, not necessarily a standalone commercial product. Those are legitimate success criteria: adoption in real publications, saved implementation/operational work, and a documented engineering contribution. A real deployment and reproducible comparisons would make that contribution more convincing than an abstract performance claim.

**A coherent extension is one reusable explorer with two delivery surfaces: a static public website and an MCP App.** Keep the core dataset/view API independent of either host. The proposed experience is: the assistant configures a pivot; the person explores it directly; selected view state and small verified results can be returned to the conversation when useful.

MCP Apps provide interactive HTML resources inside host-controlled sandboxes, with tool-result delivery and bidirectional communication. Interactive exploration need not require a new prompt for every action. This makes the proposed experience compatible with the documented model, but does not prove that Facetful's present worker/WASM implementation runs in any particular host. [Official MCP Apps overview](https://modelcontextprotocol.io/extensions/apps/overview).

### The performance hypothesis

Load an authorized dataset once, retain it while the view is alive, and run ordinary filtering, grouping and sorting inside the app. The model chooses or changes the analytical specification; it need not execute each click. This local execution is a proposed Facetful architecture, not an automatic property of MCP Apps. An app can also call server tools without a model turn; eliminating model latency is not unique to Facetful.

Use a validated declarative view specification: dataset identity/version, row dimensions, column dimensions, measures, filters and sort. Both the UI and assistant should operate on the same state. Keep data and bulk results outside model-visible text; send bounded summaries, provenance and explicit selected results as needed. Treat local/private datasets separately from public datasets: a sandbox cannot simply read a user's filesystem, and sending even aggregate results to the model is a separate disclosure decision.

Hosted prepared images are closest to the current engine's strengths. Arbitrary local CSV/Parquet introduces a different cold path: parsing, schema selection and image preparation must happen somewhere and be measured. An authorized local helper could perform preparation, but adds a setup dependency. Browser ingestion is another option to implement and test, not an existing guarantee.

Host-specific validation is essential for WASM compilation, workers, allowed fetch origins, CORS, memory limits, persistence and rehydration after the view is destroyed. The MCP layer still requires a local or remote server/integration even if the dataset and analytical execution need no query backend. [MCP App content-security-policy API](https://apps.extensions.modelcontextprotocol.io/api/interfaces/app.McpUiResourceCsp.html).

### Pivoting changes the comparison

Perspective is a direct baseline for this proposed pivot niche: its current project documents browser-WASM analytics, configurable views, a virtualized grid and charts. An MCP wrapper alone therefore would not establish a novel analytics capability. Facetful would need to demonstrate an advantage such as smaller cold-start payload, faster ingestion of its prepared snapshots, coordinated faceting performance, or simpler publication/integration. None of these has been measured against Perspective here. [Perspective project](https://github.com/perspective-dev/perspective).

Fast aggregation also does not imply fast pivot rendering. High-cardinality row/column combinations can produce an enormous output. Bound output cardinality, page or virtualize cells, and measure selection-to-paint separately from query time. Full spreadsheet behavior, calculated measures and arbitrary joins need not be initial requirements.

### A bounded next experiment, not a second product

Use one public tracker snapshot and one reusable view: country rows, technology columns, capacity sum, status/year filters and a detail page. Demonstrate it on a static page and in one explicitly supported MCP host. Measure cold data-to-first-useful-view and changing-filter-to-painted-result; verify the aggregates and preserve a reproducible view specification. Only then add arbitrary local files and broader pivot functionality.

This would connect the profile-building goal to a concrete demonstration: a dataset publisher or assistant supplies a small specification and gets a responsive, inspectable explorer. The open-data deployment remains the grounding use case; MCP Apps is a promising distribution/integration hypothesis rather than proof of future adoption.
</sv-prose>

<sv-prose id="duck-scope-clarified">
## Scope clarified: public-data faceting, 200K–2M rows

The author clarified that Facetful is a prototype intended for fast faceting over 200,000–2 million public-data rows served as static files. An earlier open-data explorer they built used Elasticsearch behind a server; the current application context is a public energy-infrastructure tracker (unit-level rows, faceted exploration). This is an existing application need, not an attempt to discover a use for a general browser database.

**Revised framing: a build-time indexer and browser faceting runtime for public-data explorers.** The relevant comparison is the complete publishing and viewing workflow, including the operational work avoided by having no query service. Offline preparation is a deliberate architectural choice. The LIMIT issue is an implementation pointer, not evidence against this fit; broader SQL gaps only matter when required by the explorer.

The earlier explorer was a Django/Elasticsearch application. The tracker is a unit-level dataset with capacity, status, ownership, technology, dates and locations, plus map/table interfaces and release updates. Those descriptions ground the application analogy; this review has not audited the current frontend or inferred its backend architecture.

The engineering priorities become:

1. **One filter update, one coordinated result:** self-excluding facet counts, sums, histograms, map aggregates and a bounded result page where needed. Make changing-state latency and stale-result handling central benchmarks.
2. **A small interaction dataset:** prepare categorical codes, required measures, IDs and location data; consider loading long descriptions and detailed records separately. Decide using compressed bytes and peak browser memory, not row count alone.
3. **Correct analytical grain:** explicitly distinguish units from facilities and define ownership attribution. A naive one-to-many owner expansion can double-count capacity. Pin the counting and summing rules before optimizing them.
4. **A publishing contract:** versioned snapshots, atomic manifest updates, stable record IDs, filter URLs tied to release versions, filtered exports and provenance. Static hosting still has bandwidth, build and maintenance costs, but removes the need to operate a query service.

The next meaningful proof is a representative public tracker release exercising realistic country/status/technology/owner/year filters, with counts, capacity totals and actual rendering. Start at the real dataset size, then use clearly labeled larger fixtures for 2M-row scaling. Keep a tuned DuckDB implementation as an alternative baseline. Full-text relevance search and map tile delivery should have explicit scope boundaries; “explorer-like” does not imply replacing every Elasticsearch feature or every map service.

**Assessment:** this makes the application fit substantially more concrete. A useful, reusable open-source component can be justified by solving that tracker workflow well; demand for a separate commercial product remains a different question. A portable library and a thin reference explorer are a sensible initial boundary, with reuse tested on a second independently shaped public dataset.
</sv-prose>

<sv-prose id="duck-verdict">
## Fresh DuckDB-WASM comparison

5 September 2026. **Your impression of slow loading is supported by this browser experiment. DuckDB is nevertheless a serious, tunable competitor—not a generally slow query engine.**

On the million-row fixture, a complete eight-query refresh took **437 ms** with an ordinary DuckDB table, **387 ms** reading Parquet, **161 ms** after converting DuckDB's six categorical columns to ENUM, and **68 ms** through Facetful's existing SQL worker API. Facetful's fused facet API was not used. ENUM brings several individual DuckDB aggregates level with, or ahead of, Facetful.

The more defensible direction is **a small, quick-starting runtime for responsive exploration of published snapshots**, not “a faster general-purpose DuckDB.” These measurements support that direction; they do not establish product-market fit.
</sv-prose>

<sv-prose id="duck-method">
## What was actually tested

Installed and npm-latest-tag package: `@duckdb/duckdb-wasm@1.33.1-dev57.0`, gitHead `ef8a4f8912b6e7f62bc0cc490145ebd391b79e1f`. The engine reports `v1.5.4`. This is the latest-tag development build, not a claim that it is the latest stable release.

Chromium 151, Linux, AMD Ryzen 7 5800H. Real browser workers, DuckDB EH single-threaded bundle; bundled/minified JS and gzip-compressed engine assets. Facetful uses its current release WASM and public JS worker wrapper, including the user's existing working-tree changes. No application implementation was modified for this review.

Each case gets a fresh isolated browser context; cases run sequentially within one Chromium process. This is not a guarantee that every browser-process compilation cache is cold. Startup values are individual observations, not distributions. Warm query figures are medians of five calls after a separately measured first call. Repeating the same query/filter state is a microbenchmark, not a changing-filter interaction trace or a p95 latency study.

The existing synthetic 200K- and 1M-row fixtures have six dictionary-friendly string columns, nullable numeric capacity and an ID. These strongly favor categorical exploration; they are not a general analytical benchmark. Main-thread call-to-result timing includes the worker boundary and the engine's structured result. Rendering is excluded. Converting those results to JS values is measured separately. No peak-memory measurement, mobile test, multithreaded COI test, OPFS reload test or persistent DuckDB-file comparison was performed.

Data is served uncompressed at HTTP transport level over localhost; Parquet retains its own encoding/compression. WAN download, browser paint and Facetful's offline preparation are excluded. The prepared `.facetful` image is **19.16 MB**, the Parquet fixture **13.31 MB**, and CSV **66.47 MB** at 1M rows. Facetful's smaller engine does not imply its data file is smaller.

The automated cross-check passed **114 output comparisons**: common queries against Facetful, plus matching sequential/combined refresh outputs where captured. Strings, nulls and shapes are exact; floating-point tolerance is `1e-7 × max(1, |a|, |b|)`. Unordered facet outputs are sorted for comparison. This is not a SQL conformance suite or an audit of all integer semantics.
</sv-prose>

<sv-prose id="duck-loading">
## Getting data in: separate four costs

**Engine startup:** DuckDB usually took **645–727 ms** locally, with one 929 ms observation. Facetful took approximately **18–25 ms** across runs. The measured compressed core resources were about **8.31 MB** for DuckDB's WASM, worker and bundled client, versus **0.140 MB** for Facetful's WASM and JS glue. Harness and data are excluded; separately loaded extensions are not included in DuckDB's core total. At 10 Mbps, 8.31 MB alone has a theoretical transmission floor of about 6.6 seconds before latency and execution; this is arithmetic, not a throttled-browser measurement. Caching changes repeat visits substantially.

**First format use:** a separate run timed explicit `LOAD parquet` at **377 ms**. It then imported the million-row file in **360 ms**, and repeated the import into another table in **311 ms**. Thus the initial 703 ms Parquet-table creation cannot all be described as data decoding. The extension-load stage may include fetching, compilation and initialization; those internal pieces were not separately instrumented. DuckDB supports extension autoloading and explicit loading. [WASM extension documentation](https://duckdb.org/docs/current/clients/wasm/extensions).

**Parsing/materialization:** million-row CSV import took **704 ms** with autodetection and **645 ms** with a fixed schema. Those are different runs, so the modest difference is suggestive, not a precise benefit estimate. At 200K, the corresponding import values were 229 and 175 ms. A Parquet view avoids copying every row into an internal table, but the first preview/group query still has reading and decoding work to do.

**JS conversion:** at 200K rows, this fixture-specific CSV-to-Arrow route spent **392 ms** parsing CSV and building Arrow, **7 ms** serializing IPC, then **89 ms** inserting it. Direct DuckDB CSV import took 229 ms in the other run. Converting CSV to JS arrays to Arrow is not automatically a fast import path. Already-existing Arrow is a different case: the 392 ms construction cost would not apply. The successful probe uses matching Arrow 17 and explicit IPC serialization; an earlier `insertArrowTable` probe across separately bundled Arrow copies left no table, and its unsuccessful timing is excluded. Its exact failure cause was not established. DuckDB's documented APIs support direct Arrow IPC, registered byte buffers, local handles and remote files. [Import documentation](https://duckdb.org/docs/current/clients/wasm/data_ingestion).

At 1M rows, engine-plus-data ready time was **102 ms** for Facetful's prepared image, **1.11 s** for the Parquet view, **1.52 s** for a Parquet-materialized DuckDB table, and **1.59 s** for CSV autodetection. The first actual 50-row result arrived at approximately **192 ms**, **1.13 s**, **1.54 s**, and **1.60 s**, respectively. ENUM preparation added another **494 ms** after the ordinary DuckDB table import.

These compare shipping/preparing strategies, not equal ingestion work. Facetful has already paid conversion offline. A prebuilt DuckDB database with appropriate types could move some of DuckDB's preparation offline too; that important counterfactual remains unmeasured. Facetful does not yet offer the same arbitrary-file ingestion convenience.
</sv-prose>

<sv-csv id="duck-load-table" src=".sideview/research/duckdb/loading.csv" freeze="2" height="25rem">
</sv-csv>

<sv-prose id="duck-interaction">
## Clicking a filter: tuned DuckDB is the meaningful baseline

The refresh requests six facet counts, a filtered count/sum, and the top 50 matching records. Country and status have selections; each facet omits its own filter while retaining the others. Both engines execute equivalent eight-query workloads.

Combining DuckDB's eight queries into one `UNION ALL` result barely helped: ordinary table **437 → 434 ms**, Parquet view **387 → 383 ms**, ENUM table **161 → 158 ms**. This combines requests and adds JSON payload construction; it does not fuse the underlying scans. A trivial worker query was around **0.4 ms** in the measured ordinary-table case. Worker messaging is therefore not a persuasive explanation for most of this refresh's hundreds of milliseconds.

DuckDB ENUMs are dictionary-encoded categorical types. The tuned lane creates each type from sorted distinct values, casts the six dimensions into a replacement table, and then runs the same SQL. Sorting the enum dictionary matters because enum ordering is not automatically equivalent to arbitrary string ordering. [DuckDB ENUM documentation](https://duckdb.org/docs/current/sql/data_types/enum).

The complete warm timings below show why the categorical encoding control matters. Country count/sum is **5.6 ms in tuned DuckDB vs 6.7 ms in Facetful**. Two-dimension grouping is **7.1 vs 7.4 ms**. Owner grouping is **3.6 vs 4.8 ms**. DuckDB also wins the tested arithmetic aggregate and CASE pivot. Facetful retains a larger lead for the filtered facet count (**2.9 vs 16.8 ms**) and the full refresh (**68 vs 161 ms**, about 2.4×).

This is a useful specialist advantage, not a universal victory. Prepared predicates, explicit enum-typed comparisons, preaggregated tables, query fusion, alternative Parquet layouts and other tuning could change the result further. This experiment did not exhaust DuckDB optimization.
</sv-prose>

<sv-csv id="duck-query-table" src=".sideview/research/duckdb/queries.csv" freeze="1" height="24rem">
</sv-csv>

<sv-prose id="duck-preview">
## Two ways a UI can still feel slow

**Facetful currently loses badly on an ordinary warm preview:** selecting 50 rows took **49 ms**, versus **0.7–2.2 ms** across the DuckDB lanes. The current ordinary projection path constructs rows before applying its final LIMIT. It deserves priority because the first table view is part of the product promise. The harness also encountered a Facetful `SELECT *` error and uses explicit projection columns in both engines instead. Neither issue was fixed as part of this review.

**Materializing large JS results can cost more than the query:** with the ordinary million-row DuckDB table, fetching a 100K-row, eight-column Arrow result took **67 ms**; the harness's subsequent per-row/per-field conversion to nested JS value arrays took **576 ms**. Facetful took **142 ms** to query and **189 ms** for its row-object decoding followed by equivalent value arrays. Those are particular conversion loops, not unavoidable engine costs or timings for every `.toArray()` use. Both incurred substantial main-thread work.

Keep large results columnar, return only visible rows, and benchmark the actual chart/table adapter. DuckDB exposes `query()` for materialized results and `send()` for lazy record-batch consumption; streaming can improve time to the first consumable batch but does not make a blocking sort or aggregate free. [Query and streaming documentation](https://duckdb.org/docs/current/clients/wasm/query).
</sv-prose>

<sv-prose id="duck-range">
## HTTP ranges: real support, conditional benefit

With this build's default configuration, registering the remote Parquet URL led to one full-file GET. A second diagnostic explicitly set `filesystem: { allowFullHTTPReads: false, forceFullHTTPReads: false, reliableHeadRequests: true }`. Against the controlled range-capable server, that produced a HEAD plus actual 206 range responses.

Across the preview and repeated country-aggregate workload it still transferred **13.23 MB of the 13.31 MB file**, in 12 requests including HEAD. This fixture/layout and workload therefore showed little total-byte saving. That is not evidence that range reads are generally ineffective: sparse projections, selective row-group pruning and different layouts can behave very differently. The experiment does not provide a separate byte count at the first preview milestone.

Do not infer lazy/range behavior merely from the registration API name; inspect requests and fallback settings. Conversely, do not dismiss DuckDB's ability to query data without first importing a complete table. Facetful's public Parquet/range adapter remains future work, so it cannot claim that capability as an existing advantage.
</sv-prose>

<sv-prose id="duck-direction">
## Product direction after the comparison

**My recommendation: continue the specialist engine, but sell the complete interaction experience, not an isolated aggregate benchmark.** The strongest candidate is an embeddable explorer for published datasets: a small runtime, prepared snapshots, linked facets and charts, quick first useful view, repeat visits and no query server. Publishers and viewers have different needs; preparing once for many readers is precisely where moving work offline can be valuable.

DuckDB remains the stronger default when users need broad analytical SQL and flexible formats, or when its startup cost is amortized across a long session. It is a direct substitute for the query-engine choice even when it is also a possible upstream data-preparation tool. Observable can host either implementation; its role as a host does not eliminate engine competition.

Priorities I would choose:

1. Fix and regression-test ordinary LIMIT/preview behavior. Expose the existing fused facet operation through the supported JS API and measure a real changing-filter trace, not just repeat SQL.
2. Make “publish a dataset, embed an explorer” the coherent workflow. Accept common source formats through a preparation step without presenting the private image format as another ecosystem users must adopt.
3. Benchmark a well-prepared, persistent DuckDB database with ENUM columns, cached reloads, and realistic result adapters before making competitive claims. Include a phone, slow network, multiple dataset shapes and p50/p95 click-to-paint.
4. Validate demand with people already publishing searchable data: which existing explorer is too heavy or laggy, what they would replace, and whether they will actually install or pay. Performance evidence alone does not demonstrate demand.

A hybrid is plausible—DuckDB for preparation or complex ad hoc work, Facetful for the recurring explorer—but adds routing and semantic complexity. I would only build it for a concrete user workflow.

**Bottom line:** your loading concern is real in this test. Facetful still has a credible quick-start and coordinated-filtering opportunity. DuckDB's ENUM results materially narrow the performance claim, and its current preview behavior exposes a Facetful issue that should be fixed before promoting “instant interaction.”
</sv-prose>

<sv-prose id="duck-artifacts">
## Evidence and reproduction

Browser harness: `.sideview/research/duckdb/bench.js`; page: `index.html`; generated bundled dependencies alongside them. Per-lane JSON files contain stages, SQL, five timing samples, small query results and same-origin HTTP request logs. `summarize.mjs` checks common outputs and regenerates `queries.csv`, `loading.csv` and `verification.json`.

The local CDP runner has been retained as `.sideview/research/duckdb/driver.mjs`. Example: `node .sideview/research/duckdb/driver.mjs duck-parquet-table:1000000:all facetful:1000000:all`, followed by `node .sideview/research/duckdb/summarize.mjs`. It serves this checkout read-only on localhost and writes research results; it requires permission to launch Chromium and bind the server. It creates a separate temporary browser profile, never uses a personal profile, and closes Chromium on completion. Research artifacts under `.sideview` are ignored by Git; the report does not make them version-controlled automatically.

The use of Sideview keeps the full comparison and generated measurement tables together without changing the application.
</sv-prose>
