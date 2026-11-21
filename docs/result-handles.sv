<sv-page label="Result handles: design">

<sv-prose id="rh-brief">
## Retained, paged results

11 November 2026. Step 1 of 2, built on the `result-handles` branch. Step 2 switches facetful-grid's demo to `need()` and handles, and waits for David.

facetful-grid's demo runs one `SELECT` over every column through `db.query()` and runs it again with `ORDER BY` on every header click. On grantnav that copies 233 MB to the page each time, and the demo has to leave out "Description Rare" to get there. `sqlnow-glide-fit.sv` proposed a view with a row count, window fetches and a dispose. This is that.

**Where it landed.**

- `db.prepare(sql)` runs the statement and keeps the result in the worker. `h.page(start, end)` returns any block of rows as an ordinary `Result`. `h.sort(orderBy)` re-orders it. `h.close()` frees it.
- For a plain SELECT over the table the worker keeps one u32 per row, 5.8 MB for all of grantnav, and nothing at all when the statement has no WHERE and no ORDER BY. Each page is gathered from the table.
- A grouped or joined result is not rows of the table. The worker computes it once, keeps it whole and slices it.
- It needs no cargo feature, so the lite build has it.

**The worry was scattered rows.** A sorted block's 512 rows can sit in all 24 of grantnav's row groups. Measured in wasm, such a block of all 19 columns takes 2 to 4 ms, against 0.45 ms unsorted. That settled the design before any of it was built: the first probe used the existing top-k gather in the native CLI and found 2.5 ms.

**Against today.** On grantnav a header click on Amount goes from 526 ms and 233 MB copied (without "Description Rare") to a 105 ms sort and a 2.7 ms block of 0.2 MB, with "Description Rare" included.
</sv-prose>

<sv-prose id="rh-api">
## The API

```js
const h = await db.prepare(
  `select * from t where search('mental health', "Title") and "Award Year" >= 2020`,
  { table: "grantnav", dictText: true },
);
h.rows; h.columns; h.kind;            // 376518, [{ name, kind }], "rows"
const page = await h.page(1024, 1536); // a Result: columnRaw, dictValue, rows()
page.start; page.generation;           // 1024, 0
await h.sort(`"Amount Awarded" desc`); // h.generation is now 1
await h.page(0, 512, { columns: ["Title", "Amount Awarded"] });
await h.close();
```

`page(start, end)` gives the rows the statement gives with `LIMIT end - start OFFSET start`. `end` is exclusive and is clamped to the result. `columns` picks result columns by name, in the order asked. `dictText` works per page, and a page's dictionary holds only the values that page uses, so codes from two pages don't compare.

`sort(orderBy)` takes the text after ORDER BY, with select aliases and positions as in SQL, and `""` for no order. The result is what the statement gives with that ORDER BY in place of its own. A LIMIT or OFFSET in the statement stays, and applies after the new order. A sort that does not bind rejects with the usual diagnostic and leaves the order as it was.

`generation` counts `sort()` calls. It moves when `sort()` is called, not when it resolves, and each page carries the value it was asked under. The worker answers messages in order, so a page with an older generation than the handle's shows the old order. That is the number facetful-grid's `need(start, end, generation)` passes on.

The rows are fixed when `prepare` resolves. A search index built later changes what `search()` matches for new statements. It doesn't change a handle's rows, and `sort()` doesn't run the WHERE again.
</sv-prose>

<sv-prose id="rh-design">
## How it works

**Row ids.** `exec/rowset.rs` runs the WHERE through the same conjuncts and mask cache as a query, evaluates the ORDER BY lanes a row group at a time, and sorts. `FullSort`'s sort moved into `sort_refs`, which both now call. The ids are global row numbers in result order.

**Pages.** A page turns its ids into (row group, row) pairs. For each row group it loads the lanes the select items read, as deep as the deepest row wanted, picks the page's rows with `gather_ctx`, and drops the lanes before the next group. Select expressions then evaluate over the picked rows only, so `search_rank()` or `upper("Title")` costs 512 rows a page. Plain text columns selected as they are never become lanes. `gather_segments` copies their bytes off the image. Dictionary columns stay codes plus the shared dictionary. All of this is code the executor already had. The page is a `QueryResult` and crosses to JS through `columnize` and the `col_*` getters like any query.

**Sort.** The handle keeps the resolved statement. `sort` parses the statement text with the new ORDER BY spliced in at `Query.tail` (the parser now records where ORDER BY starts), binds only that against the kept statement, and rebuilds the ids from the old ids instead of from the WHERE. It reads the key columns and nothing else.

**Ties are in table order.** A handle's sort breaks ties by row number, so a sort gives the same order whatever order came before it. `query()` doesn't promise that. Its full sort is unstable on numeric keys and its top-k path breaks ties its own way. So the tests compare a handle against `query()` with the unique row id as the last key. The stable sort reuses `sort_keyed3` and the packed-key sort with one more key, so it adds no sort instantiation.

**Kept results.** Aggregates, joins, CTEs, subqueries and `FROM another_table` run as a query does, and the handle keeps the `QueryResult`. `page` slices it. `sort` runs the statement again with the new ORDER BY, which for joins hits the derived-table cache. One rule covers both kinds of handle that way.

**Stale and forgotten handles.** A handle records its table's catalog registrations. When every name the table was loaded under now means another table, `page` and `sort` reject with "result handle is stale". facetful has no `drop`, so replacing a name is the only way a table goes away today. A closed handle rejects with "result handle N is closed".

**A pool of 5.** The worker holds at most `maxHandles` open handles, 5 unless `Facetful.open({ maxHandles })` says otherwise. With the pool full, `prepare` rejects before it runs anything and lists the open statements. The first version closed the least recently used handle instead. That hid a forgotten `close()` and could take a handle away from a grid still showing it, so David asked for a refusal (21 November). A grid that replaces its result prepares the new handle and then closes the old one, so it needs 2 places for a moment. On the page, a `ResultHandle` that is garbage collected unclosed is closed through a `FinalizationRegistry` with a `console.warn`. Collection can come late or never, so that only catches some leaks.
</sv-prose>

<sv-prose id="rh-numbers">
## Measurements

11 November 2026. The wasm build run through `core.js` in Node 26, as the worker runs it. `node bench/result-handles.mjs <image> --sort <column> …` reproduces them. Times are ms, medians of 21 blocks at offsets spread over the result. `query()` times include copying the result out of wasm memory, which the worker does today. The hop to the page is a transfer.

**grantnav**, 1,520,378 rows, 24 row groups, 19 columns, "Description Rare" 135 MB. Loads in 163 ms to 368 MB of wasm memory.

| | ms | |
|---|---|---|
| prepare `select *` | 7 | no row ids |
| block of 512, unsorted, all columns | 0.45 | 0.14 MB, 1 row group |
| sort by Amount Awarded (float) | 105 | |
| sort by Award Date | 97 | |
| sort by Funding Org:Name (dictionary) | 100 to 160 | |
| sort by Title (plain text) | 980 to 1,230 | |
| block of 512 sorted by Amount, all columns | 2.7 | 0.20 MB, slowest 3.9 |
| block of 512 sorted by Award Date | 2.1 | 12 of 24 row groups |
| block of 512 sorted by Title | 1.9 | 14 of 24 row groups |
| prepare with `order by "Amount Awarded" desc` | 83 | |
| `query()`, all but "Description Rare" | 246 | 233 MB copied out |
| the same, ordered by Amount | 526 | 233 MB |
| the same, ordered by Title | 1,330 | 233 MB |
| `query()`, every column | 282 | 368 MB |
| the same, ordered by Amount | 615 | 368 MB |

Leaving "Description Rare" out of a block saves about 0.1 ms. The grid has no reason to skip it now.

**grantnav filtered** to `"Award Year" >= 2020 and "Amount Awarded" > 10000`, 376,518 rows. Prepare 27 ms, sort by Amount 24 to 35 ms, sort by Title 320 to 420 ms. A sorted block touches all 24 row groups and takes 3.5 ms, slowest 3.7. That is the worst case for scatter. `query()` of every column takes 94 ms and copies 89 MB, 185 ms ordered by Amount.

**GEM units**, 183,125 rows, 3 row groups, 52 columns.

| | ms | |
|---|---|---|
| prepare `select *` | 14 | |
| block of 512, unsorted, all 52 columns | 1.1 | 0.14 MB |
| sort by Capacity (MW) | 11 to 13 | |
| sort by Country/area (dictionary) | 21 | |
| sort by Plant / Project name (plain text) | 63 | |
| block of 512, sorted, all columns | 0.9 to 2.3 | slowest 2.9 |
| `query()`, every column | 70 | 56 MB copied out |
| the same, ordered by Capacity | 103 | 56 MB |

**Memory.** The ids are 4 bytes a row. Sorting needs more while it runs: a numeric sort of all of grantnav raised wasm memory from 368 to 435 MB, and the Title sort took it to 583 MB. Wasm memory never shrinks, so that is the worker's new floor. It is still far below the 816 MB the worker reaches when it builds one full result of every column, and nothing is held on the page.

**Size.** Without wasm-opt, as this machine builds: lite 231.2 to 243.1 KB gz (budget 300), full 375.2 to 385.4 KB (budget 500).
</sv-prose>

<sv-prose id="rh-tests">
## Tests

- `crates/facetful-engine/tests/handles.rs`. 5 select lists, 6 WHEREs, 7 orders and 4 windows, every combination. Blocks of the handle equal the same rows of the query, including blocks across row groups, past the end and empty. Then every order after every other through `sort()`. The table has NULLs in a plain text, a dictionary and a float column, and many ties. Also aliases and positions in `sort`, refused sorts, chosen columns, dictionary columns staying codes, grouped and joined results, `search()` and `search_rank()` with and without the word index, and rows fixed at prepare.
- `tests/lite.rs` runs a handle in the lite build and checks a join is refused there.
- `node-worker-smoke.mjs` drives the worker's messages. It covers blocks against queries, a refused sort, closed handles, a stale handle after its table's name is loaded again, and a full pool refusing a sixth handle until one is closed.
- `browser-smoke.mjs` uses the public API in Chromium, including `generation` on a page asked for before a sort.

`./scripts/build-package.sh` passes.
</sv-prose>

<sv-prose id="rh-open">
## Left for later

- **Text sorts are slow.** Sorting grantnav by Title takes about a second, in a handle and in `query()` alike. The sort builds a `Val` with an `Rc<String>` per row and compares those. Comparing bytes on the image's blobs, and sorting dictionary columns by the rank of their codes, should cut most of it. David chose to measure it here and fix it separately.
- **`order by count(*)` on a statement with no GROUP BY panics** in the executor ("unbound scalar function count"). `query()` on main does the same. The binder doesn't look for aggregates in ORDER BY. `sort()` refuses such an order itself, but `prepare` and `query` still trap.
- **No `drop`.** A table loaded again under the same name leaves the old table in wasm memory. Handles notice the replacement. Freeing the table is its own change.
- **Page lanes load to the deepest row.** A page reads each lane of each row group it touches from the start of the group to its deepest row, then picks its rows. That costs 2 to 4 ms for grantnav's worst case. Picking straight from the segments would cut it if a table with far more row groups needs it.
- **Kept results stay whole in the worker.** A join of every grantnav column is as large as the query result. The pool of 5 handles is the only limit.
- **Step 2.** facetful-grid's demo moves to `prepare`, `need()` and `invalidate()`, with pages of 512 keyed by generation.
</sv-prose>

</sv-page>
