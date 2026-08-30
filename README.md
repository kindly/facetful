# facetful

A small, read-only, columnar SQL engine for the browser — Rust compiled to WASM,
querying immutable `.facetful` files from memory, OPFS, or HTTP range requests.
Publish data files on any static host; do analytics at the user, with no server.

- Design doc: `docs/design.sv` (a [sideview](https://github.com/) page — open with `sideview open docs/design.sv`)
- Size budget: the wasm engine must stay ≤ 300 KB gzipped (`scripts/size-check.sh`, enforced in CI)
- Status: M0/M1 — workspace + validation spike

## Workspace

| crate | what |
|---|---|
| `facetful-format` | `.facetful` read/write, footer codec, stats — shared by CLI & engine |
| `facetful-engine` | vectorized executor (later: parser + planner) — pure, no I/O |
| `facetful-wasm`   | wasm bindings, worker protocol, browser SegmentSources |
| `facetful-cli`    | native: `facetful convert`, `facetful inspect`, `facetful query` |
