# Navigation to first completed filter

Browser benchmark for the existing 183,125-row, 52-column power plant dataset.
The same small page renders five facets (Status, Type, Region, Country/area,
Technology), enables its button, clicks Operating plants, and renders all five
updated facets. The Status facet drops its own filter. No human think time.

`first_interaction_ms` starts at browser navigation and ends after two
requestAnimationFrame callbacks following the updated DOM. `ready_ms` is the
initial dashboard rendered; `click_to_result_ms` includes queries, worker/API
messaging, result conversion, DOM updates and the rendering wait. The RAF metric
is a rendering proxy, not a captured physical screen-presentation timestamp.

Lanes:

- `facet_image`: Facetful WASM opens the complete prebuilt `.facetful` image.
- `facet_image_br`: the same image served with brotli (quality 11) instead of gzip.
- `facet_parquet`: Facetful WASM converts the complete Parquet on first visit;
  `openParquet` reuses its compiled OPFS image on the repeat visit.
- `facet_parquet_zstd`: the same, from the same rows written as ZSTD level 19
  Parquet (7.9 MB against 13.9 MB Snappy).
- `duck_range`: DuckDB WASM queries a Parquet view using HTTP range requests.
- `duck_full`: DuckDB WASM downloads Parquet and materializes all columns into a table.
- `native`: preloaded native DuckDB on a server; two HTTP API requests, one for
  initial facets and one for the filtered facets. This is an architecture
  reference, not a native executable running in the browser.

Both WASM engines use a single worker and one execution thread. All SQL lanes
execute the same five queries sequentially per dashboard update. The page shows
the ten most frequent categories per facet, but receives and verifies every bucket.

## Run

From repository root, with Node supporting WebSocket, Python 3, `duckdb`,
`/usr/bin/chromium`, the existing `web/spike-bench/node_modules` dependencies,
and esbuild installed:

```sh
node bench/first-interaction/prepare.mjs
node bench/first-interaction/run.mjs smoke
PROFILES=representative_4g node bench/first-interaction/run.mjs measure
PROFILES=slower_4g,slow_4g ROUNDS=1 OUT=bench/reports/2026-09-24-first-interaction/sensitivity node bench/first-interaction/run.mjs measure
python3 bench/first-interaction/summarize.py
```

Set `ESBUILD` to override the installed esbuild executable (default is the
neighboring gem-query project's esbuild). `prepare.mjs` builds into
`/tmp/browserdb-first-interaction/assets`, creates SNAPPY Parquet from the source
CSV if absent, preserves empty text strings, and fetches the matching official
DuckDB v1.5.4 `wasm_eh` Parquet extension if absent. The extension URL is pinned to
the installed DuckDB build. Update it and the package together for another version.
The manifest records source commit, versions, sizes and SHA-256 asset hashes.

`LANES`, `PROFILES`, `ROUNDS`, and `OUT` select subsets/output paths. Default
measurement runs three rounds across all three network profiles. Engines rotate
order between rounds. A fresh Chromium process/profile is used per engine/round;
its second navigation uses the same profile. Prepared assets remain fixed for a
whole run even if the repository changes elsewhere.

## Network and cache semantics

The server shares one bandwidth budget across every response, including worker,
WASM and extension downloads. Each request receives a fixed response delay. Profiles
are 20 Mbps / 60 ms, 5 Mbps / 100 ms, and 1.6 Mbps / 150 ms. This is a synthetic
application-level network model: no DNS, TLS, TCP slow start, packet loss, radio
variability, or separate upload shaping. CPU runs at the host's actual speed,
without mobile CPU emulation. The 20 Mbps profile is a representative assumption,
not a measured universal 4G average.

JavaScript is production bundled and minified. JS, WASM, compiled images and API
responses use gzip level 9; Parquet uses its existing SNAPPY compression and no
HTTP encoding so byte ranges work. All engine downloads are mirrored locally and
counted. Static responses have long cache lifetimes; no service worker is used.
`wire_bytes` in raw output means encoded HTTP **body** bytes, excluding headers and
transport overhead. Range reads retain original-file offsets.

Cold means fresh browser profile: HTTP/OPFS/WASM caches empty. Repeat means another
navigation after the initial interaction: HTTP/OPFS/compilation caches may help,
but a new worker/database is opened. **Repeat navigation does not mean warm
in-memory predicate masks.** The earlier dashboard-assessment benchmark isolates
predicate-cache behavior. Here the first Status predicate begins cold, and later
facet queries in the same interaction can reuse it.

Native query time includes CLI/result-file plumbing and uses 16 configured
threads with the table already loaded. Native startup, data import and deployment
costs are excluded. Server response bodies still pay the modeled network cost.

## Validation

`summarize.py` compares every returned category/count in both states against the
native reference, checks all initial facets total 183,125, checks the filter total,
requires partial reads for the DuckDB range lane, and verifies Facetful's cold
transcode / repeat OPFS cache paths. Raw results retain every count, timing mark,
HTTP request, payload byte count, cache result and browser version. Summaries use
medians and include observed minima/maxima; three rounds are not a tail-latency study.
