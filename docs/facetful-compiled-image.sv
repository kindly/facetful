<sv-page label="Facetful compiled image">

<sv-prose id="b1">
# Facetful compiled-image conclusion

## Executive decision

**Parquet is the canonical source and public interchange contract. Facetful is a rebuildable, engine-specific execution image.**

The useful analogy is:

```text
Parquet         = source code
Facetful image  = compiled machine code for one Facetful engine ABI
```

This removes the false choice between ecosystem compatibility and specialization. Everyone can supply Parquet. A controlled deployment can additionally compile an optimized image; arbitrary Parquet can be queried immediately and optionally compiled into OPFS afterward.

The image is deliberately **not a stable public data format**. A new engine may reject an old image and regenerate it from Parquet. That freedom is a feature: layout, encodings, statistics and precomputed answers can evolve without migrations or compatibility baggage.

> **Product story:** Open ordinary Parquet in a tiny, purpose-built browser analytics engine; optionally compile it once into a faster execution image.
</sv-prose>

<sv-prose id="b2">
## Why specialization can win

Parquet optimizes for interchange across many engines, languages, schemas and workloads. A Facetful image can optimize for one runtime and its actual browser workload:

| General Parquet obligation | Freedom in a Facetful image |
|---|---|
| Long-lived compatibility | Exact engine ABI match; rebuild on mismatch |
| Broad type and nesting support | Only layouts the executor actually uses |
| Encodings decoded before execution | Encodings may be executed on directly |
| Generic row groups and metadata | Browser-sized groups, pruning data and cache hints |
| Data only | Precomputed counts, ranks and sort permutations are allowed |
| One representation for every consumer | Physical layout coupled to Facetful kernels |

The objective is not to dominate Parquet on every axis. Compression ratio, decode cost, direct operability, random access and resident memory trade against one another. The image should win the combined browser metric it is compiled for: **time to first correct result, steady interaction latency, peak/resident memory and repeat-open time**.

Because the intended data is generally hundreds of thousands to a few million rows, compilation can spend seconds doing careful analysis once to save work in every browser session. For a CDN-hosted artifact shared by many visitors, that amortization is exceptionally favorable.
</sv-prose>

<sv-prose id="b3">
## Two compilation paths, one runtime

### Controlled deployment: ahead of time

```text
data.parquet
    │ native Facetful compiler
    │ tries encodings, builds statistics and precomputes hot results
    ▼
data.<source-hash>.<engine-abi>.facetful
    │ static host / CDN
    ▼
tiny browser runtime opens the execution image directly
```

The native compiler may carry expensive ALP, FSST, clustering and encoding-selection logic because its binary size does not affect page load. The browser runtime carries only the decoders and kernels required by the emitted image.

### Arbitrary Parquet: immediate fallback, optional background compile

```text
Parquet → decode selected chunks → query immediately
                              └→ optional lazy compiler worker
                                   → OPFS image for later visits
```

The optional browser compiler is a **separate, dynamically loaded module**, not part of the core query WASM. It runs after the first result and its worker is terminated after writing the image, releasing its memory. If it is never loaded, Parquet remains fully usable.
</sv-prose>

<sv-prose id="b4">
## Artifact identity and invalidation

The image needs an identity, not a compatibility promise:

```text
magic
engine_layout_abi_hash
source_content_or_schema_fingerprint
compiler_profile
required_kernel_feature_bits
compiled segments + precomputations
```

Open behavior is intentionally simple:

1. If the source fingerprint and engine ABI match, open the image.
2. If either differs, ignore or delete it.
3. Query the Parquet source immediately.
4. Recompile when the appropriate compiler is available.

There are no image migrations. Correctness is protected through compiler/runtime conformance tests and result equivalence against the Parquet path—not by preserving old layouts forever.

For OPFS, the cache key should include at least `(source identity, engine ABI, compiler profile)`. For a published sidecar, the application supplies both URLs and the runtime transparently falls back to Parquet when the image is absent or incompatible.
</sv-prose>

<sv-prose id="b5">
## What the compiler is allowed to optimize

Compilation speed is secondary. The compiler may inspect the complete dataset and choose independently per column or segment:

- raw or narrow fixed-width integers;
- global dictionary codes with the smallest safe code width;
- exact scaled integers for fixed-scale decimal data;
- run-end encoding only where measured run distributions justify it;
- future ALP/FSST/FoR encodings when their runtime kernels earn their binary cost;
- row order or clustering for the declared workload;
- row-group boundaries and min/max/null statistics;
- unfiltered facet counts for the first paint;
- default sort permutations for the first table render;
- dictionary rank arrays for string ordering.

The writer should evaluate candidates on the actual segment rather than rely on universal rules. Every encoding has a raw fallback; an encoding is emitted only when its measured size/runtime objective is better.

The key architectural rule is:

> **No mandatory transformation between the stored image and the execution representation.** An encoding belongs in the image only when the runtime can scan it directly or when its measured decode cost is worth the memory/storage reduction.
</sv-prose>

<sv-prose id="b6">
## Boundaries that keep the idea honest

### Facetful images are not

- a replacement for Parquet as the ecosystem format;
- required for opening a dataset;
- promised to remain readable across engine versions;
- guaranteed to be smaller than every tuned Parquet file;
- justification for putting every experimental encoder into the core WASM.

### They are

- optional compiled artifacts for controlled deployments;
- discardable OPFS cache images for arbitrary Parquet;
- allowed to be coupled tightly to one runtime ABI;
- allowed to contain workload-specific precomputations;
- the place where executor-native encodings can earn their keep.

For data too large or too dynamic to compile conveniently, the direct Parquet/range path remains the source-of-truth fallback. The image is an optimization, never a gate.
</sv-prose>

<sv-prose id="b7">
## Resulting product architecture

| Layer | Contract |
|---|---|
| Public data source | **Parquet**; broadly producible and portable |
| First-load fallback | Hyparquet decodes selected chunks into engine-native columns |
| Query runtime | Small Rust/WASM engine with a bounded kernel/decoder set |
| Native optimizer | Compiles Parquet into the best image without browser-size constraints |
| Optional browser optimizer | Lazy, separate worker module; writes OPFS after first result |
| Published optimization | Optional ABI-bound Facetful sidecar on the CDN |
| Cache policy | Rebuild rather than migrate |

### Final conclusion

Facetful should retain a specialized representation, native compiler and direct image loader. The representation is valuable precisely because it is **not** another permanent standard: it is free to be the best executable form for the current engine and current workload.

The core trade is unusually attractive for browser analytics:

> **Spend CPU once outside—or after the critical path of—the browser, then save CPU, memory and latency in every subsequent session.**
</sv-prose>

</sv-page>

<sv-prose id="b8">
## Review notes (Claude, 2026-09-01): adopted, with four disciplines

Agreed and adopted — this supersedes the design doc's "product posture" and "internal-representation" sections as the canonical statement. Four disciplines to keep it honest in practice:

1. **ABI identity should be an explicit hand-bumped integer, not a derived hash.** Auto-hashing "the layout ABI" invites both false stability (forgot to include something) and false churn (hash moved, layout didn't). A `LAYOUT_ABI: u32` bumped in code review, plus a CI golden-image test that fails loudly when a layout-affecting change forgets the bump, is boring and correct. The source fingerprint and compiler profile stay as real hashes.
2. **The runtime defines the kernel set; the compiler targets it — not the reverse.** "The browser runtime carries only the decoders the image needs" is only literally true for controlled deployments that ship a matched build. The general npm runtime is one binary, so every encoding kernel it contains pays size in every page. Rule: an encoding lands in the core runtime only with (a) a kernel that scans it directly, (b) flagship-benchmark evidence, (c) its size delta accounted against the budget. The feature-bits check then makes an over-ambitious image degrade gracefully to the Parquet path rather than fail.
3. **The optional browser compiler stays a baseline cache-filler, not an optimizer.** Two compilers that both make clever choices will drift. The lazy in-browser module should emit only the boring profile (narrow ints, dictionaries, stats); anything fancier is the native compiler's job. One compiler makes decisions; the other replays defaults.
4. **Differential testing is the compatibility story.** Since images are rebuildable rather than migratable, correctness rests entirely on "image path ≡ Parquet path for every query." That's an executable invariant — the bench lanes already agree cross-implementation; promote that into a permanent differential test suite (same queries, both paths, exact-match counts and null-skipping sums) that every compiler change must pass.

One consequence worth naming: the M7 promotion question ("is .facetful a publishing format?") becomes cleaner — it's now "does the *sidecar image* + range requests beat Parquet + range requests on large remote data?", measured per deployment, with no public-format promise riding on the answer.
</sv-prose>
