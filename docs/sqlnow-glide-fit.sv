<sv-page label="sqlnow, Glide and Facetful: fit">

<sv-prose id="sg-lightweight-extraction">
## How much Glide could be cut?

6 September 2026. Source inspection and fresh in-memory bundles of installed Glide 6.0.3. **A focused read-only extraction targeting 30–50 KB gzipped looks plausible: about 50–70% below the roughly 100 KB initial editor baseline. This is an engineering estimate, not a completed grid or a measured saving from a working replacement.**

The probes below use esbuild, production minification, ESM splitting and gzip level 9. React and ReactDOM are external in every lane; other retained dependencies are included. CSS, fonts and application code are excluded. Initial size follows static imports; all-JS includes lazy chunks. Lower layers are exports, not wired or browser-tested grids. Sizes overlap and must not be added. Reproduce with `node .sideview/research/sqlnow-glide/layer-measure.mjs`.
</sv-prose>

<sv-csv id="sg-layer-sizes" src=".sideview/research/sqlnow-glide/layer-comparison.csv" freeze="1" height="20rem">
</sv-csv>

<sv-prose id="sg-extraction-scope">
**Lowest-disruption option:** the package already exports `DataEditorCore` and individual cell renderers. The stock `DataEditor` wrapper registers all built-in cell types. The text-only export probe reduces initial JS from 100,338 to 66,034 bytes, before app wiring and any additional required renderers, icons or image loader. It still retains editing and interaction machinery. This is a useful first experiment, not a drop-in preservation of sqlnow's rich cells.

**Larger extraction:** retain DPR scaling, font measurement/alignment, clipping, visible-cell drawing, scroll-frame copying and invalidation. Keep basic column resizing, sort callbacks, focus/keyboard navigation, copy and an accessible DOM representation. Remove editor overlays, paste/import editing, fill handles, row append/delete, rich media renderers, wrapping/variable heights, merged cells, group headers, row/column reordering and advanced selection. Removing UI flags alone cannot be assumed to remove runtime branches from bundles.

The internal scrolling component still imports drag/resize infrastructure, while drawing code still supports themes, spans, groups and selection decorations. Getting below the existing 40 KB scrolling-layer probe while adding the required cells and usability needs structural simplification, not just a new entry point. The 30–50 KB target assumes fixed-height, single-line text/number/link rows and a narrow interaction contract. Framework runtime would be extra if retained in a previously non-React host.

**Typography itself is not the bulk:** the exported text helpers bundle to 3,638 bytes gzipped in this probe, including their retained dependencies. Glide's font-quality techniques can be preserved without its whole editor. Keep the proven scrolling and accessibility behavior in scope: a small painter that loses those is not equivalent to the requested table.

No grid implementation or sqlnow source files changed. The earlier real sqlnow Vite comparison measured a 98,781-byte initial-JS difference when removing Glide and its used custom renderers; that app-specific marginal measurement is distinct from these isolated layer exports.
</sv-prose>

<sv-prose id="sg-gpu-typography">
## GPU text: quality first, grid second

6 September 2026. David has already tried DOM virtualization and Canvas grids; the current question is whether WebGL/WebGPU offers a better typography route, potentially as an independent project. The earlier DOM-first suggestion below is not the recommendation for this experiment.

Neither graphics API supplies a browser-style font drawing operation. The font pipeline matters more to quality than the choice between WebGL and WebGPU.

- **Target-size bitmap atlas:** rasterize glyphs or shaped text runs once, pack them into textures, and draw batches of textured rectangles. The [VS Code WebGPU editor prototype](https://github.com/microsoft/vscode/issues/221145) explicitly uses Canvas 2D to generate glyphs. This changes drawing throughput, not automatically the source font quality. Atlas resolution, pixel alignment, sampling and invalidation when font size or display scale changes still matter.
- **SDF/MSDF:** store distances to outlines, reconstruct edges in a shader. [MSDF](https://github.com/Chlumsky/msdfgen) preserves corners better than ordinary single-channel SDF and is attractive for zooming. It is not automatically superior for 12–14px table text: small-size hinting and pixel coverage are separate problems. [Qt's hinting documentation](https://doc.qt.io/qt-6/qml-qtquick-text.html#font.hintingPreference-prop) explains why matching glyphs to the target pixel grid can improve low-density legibility. Try the [WebGPU MSDF sample](https://webgpu.github.io/webgpu-samples/?sample=textRenderingMsdf), but judge at actual table sizes, not only magnified text.
- **Own rasterization/vector pipeline:** more control, but font loading, shaping, fallback, antialiasing and international text become substantial responsibilities. This is a text-engine project as well as a grid project.

Matching a canvas backing store to devicePixelRatio is distinct from rendering above physical display resolution and downsampling. GPU output also needs correct display scaling; moving APIs does not remove that requirement. [MDN canvas scaling guidance](https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API/Tutorial/Optimizing_canvas).

**Suggested bounded experiment, not an implemented result:** compare DOM reference text, correctly scaled Canvas 2D, a target-size GPU bitmap atlas and MSDF using the same font and strings at 12/14/16px. Test 1x, fractional and 2x display scaling, stationary and scrolling, light and dark backgrounds, numbers, accented names and representative non-Latin data. Measure cold font/atlas setup separately from warm frames and memory. Only build the grid once one typography route is convincing. Both WebGL and WebGPU can implement the atlas approaches; WebGPU is not inherently a font-quality upgrade.

The potential standalone contribution is a small, responsive, read-only GPU table with good typography and a windowed data interface. Neither bundle size nor performance has been established by this research, and accessibility/copy/keyboard behavior remain independent work.
</sv-prose>

<sv-prose id="sg-minimal-long-table">
## Minimal long-table direction for Facetful

6 September 2026. The desired feature is continuous, fast browsing of very long results, not necessarily pagination. A small read-only virtualized table is a plausible fit; performance and final bundle size remain to be measured.

**Two independent layers:** Facetful retains the filtered/sorted row-position mapping; the renderer requests and draws a bounded window. Recompute the mapping when filters or sorting change, not on every scroll. An optional Uint32 mapping for two million rows costs 8 MB by itself, excluding source columns, sort workspace and other state. An unfiltered natural-order view need not allocate that mapping.

Suggested initial UI: fixed-height single-line rows, defined column widths, sticky header, native scrolling, viewport rows plus modest overscan, simple text/number/link cells, click-to-sort, row selection, keyboard navigation and explicit copy-selected-rows. Keep inline editing, variable row heights, merged cells, spreadsheet range selection and rich charts out of the first version. Read-only still requires tested focus, table semantics, row indices and screen-reader behavior.

Use a small DOM implementation first, updating a bounded set of rows on animation frames. Fetch larger windows than are visible and cache neighboring windows; deduplicate requests and reject stale responses after filter changes. For very wide results, column virtualization may also be needed. Canvas is a later measured choice, not a prerequisite for a high source-row count. A headless virtualizer such as [TanStack Virtual](https://tanstack.com/virtual/latest/docs/introduction) is an alternative to owning the scrolling machinery, not a complete table or a guarantee of million-row scrollbar behavior.

**Two traps:** at 28 pixels each, two million rows imply a 56-million-pixel spacer. Browser element-height limits vary, so simple `rowCount × rowHeight` scrolling is insufficient across the target range; use a tested bounded physical scroll range with logical-row mapping or segmented scrolling, plus precise keyboard/jump-to-row navigation. [Browser-height issue and one established approach](https://www.ag-grid.com/javascript-data-grid/massive-row-count/). Also, repeatedly querying increasingly deep `LIMIT/OFFSET` windows can redo filtering/sorting; it is not equivalent to fetching from a retained result.

The public Facetful wrapper currently exposes complete SQL results and raw column buffers, not a retained-view/window-fetch API. A proposed API would create a view and return its row count, then fetch `[start, count]` for selected columns, and explicitly dispose the view. Keep a generation identifier so old windows cannot paint into a new filter state. This is a design extension, not a claim about existing functionality.

The useful experiment would test two million rows, scrollbar jumps near the bottom, rapid filter changes, resizing, keyboard navigation and touch scrolling, while measuring frame times and memory. That would establish whether a compact renderer meets the actual workload before expanding it toward a general grid.
</sv-prose>

<sv-prose id="sg-verdict">
## Verdict from the actual sqlnow code

5 September 2026. Inspected `/home/david/projects/querier`, its agent guide, frontend, Rust query endpoint, installed packages and shipped assets. **sqlnow's use of native DuckDB and Glide is coherent. Facetful has a different purpose, and does not need to replace either to be useful.**

The most actionable surprise is bundle attribution: a diagnostic production build narrowed the editor's language import to SQL and reduced initial JS from **853 KB to 351 KB gzipped**, retaining Glide and the used rich-cell renderers. Removing the grid entirely from that diagnostic reduced it a further **99 KB**, to **252 KB**. The null-grid build is only a dependency-removal experiment, not a replacement UI.

No sqlnow source, dependency or shipped asset was changed. Its Git status was unchanged after inspection. Builds were performed in memory, with results saved under this project's ignored `.sideview/research/sqlnow-glide/` directory. No browser frame-time, accessibility or end-to-end query benchmark was run for this review.
</sv-prose>

<sv-prose id="sg-sqlnow">
## sqlnow already implements much of the agent-to-human handoff

The implementation is more specific than a generic SQL scratchpad. It supports named queries and deep links, persisted sessions and history, attaching heterogeneous inputs, HTTP management alongside the CLI, live session updates and protection against overwriting unsaved editor changes. Agents can prepare an analytical view and people can inspect, modify and export it. The styling companions are particularly distinctive: `_sqlnow_format_`, `_sqlnow_cell_`, `_sqlnow_column_` and row-height directives let SQL carry useful visual presentation without generating a frontend for each result.

The execution path is **native bundled DuckDB in a local Rust process → bounded string-row JSON over HTTP → Glide in the browser**. It is not DuckDB-WASM. Browser engine-download and WASM-instantiation measurements from the earlier comparison do not apply to sqlnow's native path. Its database/file access, SQL surface and DuckDB-backed session format make an engine swap much larger than exchanging a query function.

The UI requests 500 rows by default. The backend attempts a query-level limit with one extra row to detect truncation, then formats the displayed values. Exports have a separate streaming path. This is a reasonable architecture for inspecting results from much larger underlying datasets: the grid need not hold the entire source table. Raising the display limit can still increase server conversion, JSON transfer and browser memory; canvas virtualization alone does not prevent those costs.

Key local evidence: `README.md`; `AGENTS.md` sections 1–7; `Cargo.toml:15`; `libsqlnow/src/lib.rs:1464` and `:1623`; `ui/src/query-form.jsx:140`, `:212` and `:476`; `ui/src/routes/root.jsx`.
</sv-prose>

<sv-prose id="sg-grid">
## What Glide contributes here

The installed core and cells packages are **6.0.3**. The current component uses lazy cell lookup, selection/copy support, grid search, resizable columns, variable row heights and custom range/sparkline/tag renderers. The format layer caches parsed styles and cell specifications; row heights are precomputed and the renderer array is stable. Those choices already avoid some unnecessary per-frame work.

This is a read-only results grid: there is no cell-edit persistence callback. My earlier suggestion of a full editable-grid requirement overstated what sqlnow actually uses. Nevertheless, rich read-only inspection still benefits from mature keyboard, selection, clipboard and rendering behavior. Replacing it with an HTML table would involve a feature tradeoff, not just deleting a dependency.

Glide is a renderer, not a competing faceting engine. Its official README explicitly leaves sorting and filtering to the supplied data source; its cell callback does not require any particular query backend. It can in principle consume a bounded Facetful result, but that integration has not been built or tested here. [Glide README and FAQ](https://github.com/glideapps/glide-data-grid).

Two interaction boundaries matter in sqlnow: grid search operates on the loaded result, not the whole underlying database; and the current UI's displayed elapsed time ends after response JSON processing, before React commits and canvas painting. Neither should be mistaken for full-dataset search or click-to-painted-result latency.
</sv-prose>

<sv-prose id="sg-size">
## Where the frontend weight actually comes from

`ui/src/query-form.jsx:19` imports `langs` from `@uiw/codemirror-extensions-langs`, then calls only `langs.sql()` at line 276. A dependency probe showed numerous non-SQL grammars retained. The controlled alternate build imports `sql` directly from `@codemirror/lang-sql`, aliased so it does not conflict with the component's SQL text state.

The baseline production entry exactly matches the checked-in HTML's referenced JS asset in raw and gzip byte counts. All lanes use the existing React/Tailwind Vite plugins and production minification. The unrelated local PostCSS configuration was incompatible with the installed Tailwind version, so it was bypassed in memory identically in every lane; no configuration file was edited. This is a JS bundle experiment, not verification of an application patch.

Sizes are gzip-9 measurements, not observed HTTP transfer or load time. Initial means the entry plus static imports; all-JS also counts lazy chunks. CSS, fonts and data are excluded. The build is written nowhere (`build.write=false`), and the SQL-only source substitution occurs only inside the diagnostic transform.
</sv-prose>

<sv-csv id="sg-sizes" src=".sideview/research/sqlnow-glide/build-comparison.csv" freeze="1" height="15rem">
</sv-csv>

<sv-prose id="sg-size-interpretation">
The SQL-only diagnostic saves **501,927 initial compressed bytes, approximately 59%**, without removing the grid. The further grid-removal diagnostic saves **98,781 initial compressed bytes**, or **106,610 bytes** counting lazy chunks. These are marginal differences in this application, not universal library-size figures. A replacement would add its own bytes and implementation work.

A separate esbuild core-Glide probe totaled about 154 KB gzipped including React dependencies and lazy chunks. Do not add it to the Vite figures: shared dependencies overlap. The esbuild rich-cell barrel probe also retained an unused lazy article editor that the actual Vite build eliminated, illustrating why dependency-directory sizes and generic bundler probes can mislead.

The frontend currently imports query routes eagerly, bringing the editor/grid dependency graph into the application entry even before results are shown. After narrowing the language import, optional editor loading is a plausible further investigation for an embedded read-only mode. No lazy-loading change or runtime speedup was implemented or measured here.
</sv-prose>

<sv-prose id="sg-fit">
## Fit for the three contexts

**sqlnow: keep native DuckDB, and Glide is a defensible choice.** The rich presentation contract and agent/human session workflow are its distinctive layer. A localhost-served or desktop-bundled UI has a different download budget from a public cold visit. I would investigate the demonstrated language-import saving before spending time on grid replacement.

**Facetful public explorer: decide the renderer independently.** Its main result may be a handful of facets, capacity summaries, a map and 50–200 visible records. A small semantic table with pagination may serve that experience well. Glide becomes more compelling if users really need wide pivots, spreadsheet-like selection and rich cells. The engine's 200K–2M source-row target alone does not imply a million-row grid. Keyboard, screen-reader and touch behavior should be verified on the chosen UI, not inferred from canvas versus DOM alone.

**Agent/MCP analytics: sqlnow is the natural existing foundation.** An embedded result mode could reuse its query/session/presentation contract without loading the whole SQL workbench. The paths inspected expose CLI and HTTP integration; I did not find an MCP Apps adapter in those paths. Any such adapter needs explicit host access and authorization design: the current local server is not an authenticated public service. Keeping native DuckDB for local analytical execution does not require putting DuckDB-WASM in the app.

The best reuse boundary appears to be **result metadata, formatting and view state**, rather than forcing one engine or full application shell into every context. Facetful could eventually consume the same view contract for prepared public snapshots. That is optional integration work, not a prerequisite for proving its public-data faceting value.
</sv-prose>

<sv-prose id="sg-next">
## What I would do next, when implementation is requested

1. Narrow the SQL language import, verify SQL highlighting/completion and run the existing frontend tests. The 59% byte reduction is a strong candidate, not a landed fix.
2. Keep Glide for sqlnow unless actual runtime profiling or a required accessibility/touch behavior gives a reason to replace it. Separate query time, JSON conversion, paint and scrolling measurements.
3. For Facetful, start with the actual result-page requirements. Compare a small table with Glide using the same engine and outputs, so grid cost is not confused with query cost.
4. If pursuing the MCP surface, extract an optional lightweight result view from sqlnow before designing another general analytics application.

One small correctness-of-presentation pointer: the backend supplies an explicit `truncated` flag, but the UI footer currently infers “limit reached” from displayed row count. Using the flag would distinguish an exactly complete result from a truncated one. This was observed in source, not changed.

Reproduction: `.sideview/research/sqlnow-glide/vite-measure.mjs` and `vite-results.json` for the production build comparison; `measure.mjs` and `bundle-results.json` for isolated dependency probes. Scripts use the existing dependencies in the sqlnow checkout. Sideview holds this report and the generated comparison table; no application implementation was modified.
</sv-prose>
