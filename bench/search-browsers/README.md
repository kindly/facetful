# Search in real browsers

How search behaves in a browser, not Node: a page streams an image in with
its search index built as it arrives, times typing (search(), searchSuggest,
ranked top 20 at every keystroke), keeps the image and index in OPFS, reopens
them on a fresh page, and (grantnav) loads a split image. Results go back to
the server as JSON lines.

Datasets: GEM units (183,125 rows, a typical dashboard; `data/`, or `UNITS=`)
and grantnav (1.52M rows, the stress case; read from `~/projects/gratnav` in
place, or `GRANTNAV=`). `units-packed` and `grantnav-packed` are the same
data as packed images (`facetful pack`). Network profiles: `local` (the file as it is, as fast as it
goes: what the device costs) and `100mbps` / `20mbps` (brotli, paced, with
latency: what users see).

## Desktop browsers, automated

```sh
node bench/search-browsers/prepare.mjs     # wasm, bundles, split parts, brotli copies -> /tmp/browserdb-search-browsers/assets
node bench/search-browsers/run.mjs         # BROWSERS=chromium,firefox DATASETS=units,grantnav PROFILES=local,20mbps
node bench/search-browsers/summarize.mjs /tmp/browserdb-search-browsers/results-*.jsonl
```

`SEARCH_BENCH_DIR=` moves everything written from `/tmp/browserdb-search-browsers`.

Each run starts the browser headless with a fresh profile on the page's URL
and waits for the page to post that it is done; no browser automation
library. The brotli copies are quality 9, to keep preparing to a couple of
minutes: grantnav is 67.6 MB on the wire against 58.1 MB at quality 11.

## A phone or another machine

```sh
node bench/search-browsers/server.mjs 8077
```

prints `http://<this machine>:8077/?dataset=units`; open it on the device
(same network). Add `&dataset=grantnav` or `&p=20mbps` as wanted. The page
shows its results and the server appends them to
`/tmp/browserdb-search-browsers/manual.jsonl`.

Plain HTTP to a LAN address is not a secure context, so OPFS is not there:
the page skips persisting and the repeat visit, and says so. Everything else
runs. For OPFS on a device, serve it over HTTPS (a tunnel, or a local
certificate).

## Phases

| phase | what it times |
|---|---|
| first | table ready and search ready while streaming; every keystroke of four queries; saving the index to OPFS; wasm memory, and in Chromium the page's memory (`measureUserAgentSpecificMemory`, which waits for a garbage collection, so a phase's total includes up to ~20 s of that) |
| repeat (a fresh page) | opening the image from OPFS; reading the index back; typing |
| split (grantnav) | the first part ready; the second attached |
