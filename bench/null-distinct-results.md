Measured locally on 2026-09-13 with the existing 1,000,000-row spike image
(16 row groups), native optimized CLI. Each value is the median of 10 runs
after three warmups. Cold clears filter masks; column segments and dictionaries
remain warm. These are synthetic-image timings, not the reported real-world data.

```sh
cargo build -p facetful-cli --profile native
target/native/facetful query spikes/facet-spike/data-1000000.facetful \
  --bench bench/null-distinct-queries.sql
```

| Query | Before cold ms | After cold ms | Before warm ms | After warm ms |
| --- | ---: | ---: | ---: | ---: |
| count_all | 0.00 | 0.00 | 0.00 | 0.00 |
| distinct_small_dict | 11.77 | 1.25 | 11.79 | 1.26 |
| distinct_large_dict | 12.01 | 1.30 | 12.03 | 1.29 |
| distinct_coalesce | 29.00 | 1.30 | 29.04 | 1.29 |
| coalesce_filter | 18.15 | 1.99 | 1.18 | 1.17 |
| bare_filter | 2.00 | 1.99 | 1.17 | 1.18 |
| distinct_filtered | 6.39 | 4.84 | 3.14 | 1.42 |
| distinct_grouped | 32.12 | 4.54 | 32.22 | 4.55 |

`count_all` is below the CLI's 0.01 ms display resolution.

Validation: `cargo test --workspace`, `bash scripts/size-check.sh`, and
`node js/facetful/node-smoke.mjs` passed. WASM gzip size is 192,863 bytes
against the 307,200-byte budget; wasm-opt was unavailable. Performance timings
above are native, not WASM.
