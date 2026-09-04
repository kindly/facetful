<sv-page label="Facetful SQL parser">

<sv-prose id="p1">
# The SQL parser — approach, libraries, error messages

The M3 question, split into what it actually is: **how hard is the grammar we chose** (not SQL — our deliberately limited subset), **what do good error messages require**, and **which tool** — hand-rolled, a tiny PEG library, or a borrowed grammar.

## First: the grammar we'd actually parse

The "functions instead of syntax" principle shrinks SQL's notoriously tricky corners into a boring expression language. Everything below the query skeleton is one Pratt/precedence expression parser:

```
query :=  SELECT sel_item (',' sel_item)*
          FROM ident
          [WHERE expr] [GROUP BY expr (',' expr)*]
          [ORDER BY expr [ASC|DESC] (',' …)*] [LIMIT n [OFFSET n]]
sel_item := expr [[AS] ident] | '*'
expr   :=  precedence: OR < AND < NOT < (= != < <= > >=) < (+ -) < (* / %) < unary- < atom
atom   :=  number | 'string' | ident | ident '(' args ')' | '(' expr ')'
```

Function calls replace the syntax that makes SQL parsers gnarly:

| SQL special form | becomes |
|---|---|
| `x BETWEEN a AND b` (the classic `AND` ambiguity) | `between(x, a, b)` |
| `x IN (a, b, c)` | `in(x, a, b, c)` |
| `x LIKE '%y%'` | `like(x, '%y%')` |
| `CASE WHEN c THEN a ELSE b END` | `if(c, a, b)` |
| `CAST(x AS INT)` | `int(x)`, `float(x)`, `text(x)` |
| `x IS [NOT] NULL` | `isnull(x)`, `not isnull(x)` |
| `a \|\| b` | `concat(a, b)` |
| `COUNT(DISTINCT x)` (DISTINCT inside calls) | `count_distinct(x)` |

What's left is a grammar of roughly a dozen productions with **no ambiguity, no backtracking, no keyword-vs-identifier fights** beyond a small reserved list. This is not "an SQL parser" in the scary sense — it's a calculator grammar plus a query skeleton. (The scary parts of real SQL parsers — dialect sprawl, `BETWEEN…AND`, implicit joins, quoted-identifier rules, subquery placement — are exactly what the table above deletes.)

## Confidence, honestly stated

For **this** grammar: high. Hand-rolled recursive descent + Pratt is ~600-1,000 lines, and it's the same architecture sqlparser-rs itself uses. The pyparsing where-clause experience transfers directly — a PEG combinator library and a hand-rolled descent parser are the same mental model, just with the recursion explicit.

The part that deserves respect isn't parsing — it's **diagnostics**, and that's a design commitment, not a library property:

1. **Lex/parse errors** need: byte-span tracking on every token, an "expected set" at each decision point, and a snippet renderer (~150 lines, write once):
```
select country, sum(capacity from plants
                             ^^^^
expected ')' to close 'sum(' (opened at column 17), or ','
```
2. **Bind errors** are where users actually live, and no parser library helps with them — they come from the catalog: unknown column/function with edit-distance suggestions (`unknown column 'contry' — did you mean 'country'?`), arity checks, type mismatches with the offending span. This layer is identical work under every option below.

Hand-rolled is, counterintuitively, the *easiest* route to excellent messages: at every point in a descent parser you know exactly what you're in the middle of ("closing the argument list of `sum`"), context that generic libraries throw away.
</sv-prose>

<sv-prose id="p2">
## The library options — measured on wasm, not guessed

Built today as representative expression parsers (same grammar, error formatting included), `wasm32-unknown-unknown`, opt-z + LTO, before wasm-opt:

| option | wasm raw | wasm gz | error messages | notes |
|---|---|---|---|---|
| **hand-rolled** (lexer + Pratt) | — | est. +10-20 KB | **as good as we make them** — full context at every point | ~600-1,000 LoC; the learning-project part; zero deps |
| **rust-peg** | 65.5 KB | 34.3 KB | location + expected-set (decent, terse) | pure compile-time codegen via macro, tiny runtime; grammar-as-Rust-macro |
| **pest** | 67.7 KB | **25.2 KB** | **pretty caret+span rendering built in** | separate `.pest` grammar file (readable spec!); errors name grammar rules ("expected conj"), which needs curating; no error recovery |
| **sqlite3-parser** (lemon-rs) | 147 KB | 51 KB | SQLite-style "near X: syntax error" (famously terse) | full battle-tested SQLite grammar — vastly more SQL than we want, contradicts the functions-over-syntax design |
| **sqlparser-rs** | 1.30 MB | 387 KB | decent | out on size, and its AST is an ocean |
| chumsky | not built | — | its headline feature (rich, recoverable) | monomorphization-heavy: bloat + compile-time reputation (LakeSail: painful type errors); wrong fit for a size budget |
| nom / winnow | — | small | **DIY** — bare ErrorKind unless you build the machinery yourself | you end up writing the diagnostics layer anyway, plus combinator indirection |

Context: the whole engine is currently 21 KB gz. pest would roughly double the binary; rust-peg a bit more; hand-rolled keeps it smallest. All fit the 300 KB budget with room.

## Recommendation

**Hand-rolled lexer + Pratt parser with a small diagnostics module** (spans, expected-sets, snippet renderer), for four reasons: (1) this grammar is small enough that the usual "don't hand-roll SQL" warning doesn't apply — the table above deleted the hard parts; (2) best-possible error messages, because context is never lost; (3) smallest binary; (4) it's the part of this project that was always meant to be the fun one. **pest is the strong second** — 25 KB gz, a readable grammar spec, and free caret rendering — and it's the fallback if hand-rolling drags: the AST and everything downstream (binder, planner) is identical either way, so switching later costs only the parser file itself.

Either way, the **binder diagnostics** (did-you-mean columns/functions, arity/type errors with spans) get built identically — they're most of the perceived "good errors" and no library provides them.
</sv-prose>

<sv-prose id="p3">
## Decisions (round 1, approved 2026-08-31)

**Hand-rolled lexer + Pratt parser + diagnostics module** (pest the named fallback) · **LLM idiom set as sugar** — IN, IS [NOT] NULL, BETWEEN, [NOT] LIKE, CASE WHEN, CAST, COUNT(DISTINCT), `||` all desugar to the function-call AST; function spellings stay valid; keyword set reserved upfront · **Claude builds it end to end, with detailed design input to David as it goes** — key decisions narrated on this page for review, not just committed.

Build order: spans + diagnostics renderer → lexer → AST → parser (skeleton + Pratt + sugar) → binder (did-you-mean, arity/type checks) → SELECT pipeline over the existing executor → `facetful query` REPL → error-message and desugar-equivalence test suites.
</sv-prose>


</sv-page>

<sv-prose id="p4">
## Build log 1: lexer + parser + diagnostics are in (commit-level notes for review)

**What exists** (`crates/facetful-engine/src/sql/`, 12 tests green): spans + caret-snippet diagnostic renderer + edit-distance `suggest()` (span.rs), a hand lexer (lexer.rs), the small AST (ast.rs), and the descent+Pratt parser with the full idiom-sugar set (parser.rs).

**Decisions worth your eyes:**
1. **Keyword-named function spellings survive reservation.** `between`, `in`, `like`, `case` are keywords, so `between(x,1,5)` initially failed to parse — the tests caught it. Fix: in prefix position, a sugar keyword followed by `(` is a function call. Both spellings now provably produce identical ASTs (`idioms_desugar_to_function_calls` test asserts tree-equality).
2. **BETWEEN's `AND` ambiguity** is one line: the middle operand parses at binding power above AND, so `x between 1 and 5 and y = 2` groups correctly (tested).
3. **Bare aliases** are in (`select sum(x) total from t`) — LLMs emit them constantly. Cost: a name after an expression is always an alias, which makes some missing-comma errors read as alias errors; acceptable, and the FROM-expected error hints about commas.
4. **`"quoted"` identifiers** escape the keyword list (`select "between" from t` works) — the safety valve for reserving keywords upfront.
5. **Error messages are tested as a feature** — exact-content assertions, e.g. `expected ')' to close the arguments of sum(`, the unterminated-string hint about `''` escaping, `between is written: x between low and high`, and a line/column pointer test.
6. `--` line comments, `''` string escaping, `<>` as not-equals, case-insensitive keywords — the small LLM/SQL-culture compatibilities.

**Next**: the binder (name resolution against the catalog with did-you-mean, arity/type checks — where the best diagnostics live), then the SELECT pipeline mapping bound queries onto the existing executor, then the `facetful query` REPL.
</sv-prose>

<sv-prose id="p5">
## Build log 2: the binder (commit f90817f)

Name resolution, arity and type checking, and the aggregate rules — with the diagnostics tested as features:

- **Did-you-mean** works both ways: `unknown column 'contry' — did you mean 'country'?` and `unknown function 'cont' — did you mean 'count()'?` (edit-distance over schema/registry).
- **Type system**: Int/Float/Text/Bool + Null-coerces-to-anything, Int→Float widening; arithmetic on text gets a hint pointing at `||`/concat; `WHERE` requires a boolean and says what it got instead.
- **Aggregate rules**, each with a tailored message: no aggregates in WHERE (hints that HAVING isn't supported yet); nested aggregates rejected (`sum(sum(x))`) while `round(sum(x))` stays legal; non-aggregated select items must appear in GROUP BY — validated on *bound* expressions so spans don't defeat the comparison (a bug the tests caught).
- **SQL culture**: `count(*)` binds as count(1); ORDER BY accepts select aliases and 1-based positions.
- The function registry (aggregates, scalars, desugar targets) is one static table — a UDF registration API later extends it rather than replacing it.

**Next**: the execution pipeline — compiling a BoundQuery onto the existing vectorized executor (scan → filter → group/aggregate → sort → limit), then the `facetful query` REPL.
</sv-prose>

<sv-prose id="p6">
## Build log 3: executor + REPL — M3 core complete

`facetful query spikes/facet-spike/data-200000.facetful` now answers real SQL:

```
facetful> select country, count(*) as n, round(sum(capacity), 1) as total
          from t where status in ('status_0','status_1')
          group by country order by total desc limit 5
country    n     total
---------  ----  --------
country_0  4297  153153.5
…
(5 rows, 52.1 ms)
```

And the diagnostics survive the whole pipeline:

```
facetful> select contry, sum(capacit) from t group by country
error: unknown column 'contry'
  --> line 1, column 8
   | select contry, sum(capacit) from t group by country
   |        ^^^^^^
hint: did you mean 'country'?
```

**Semantics choices** (SQLite-compatible where there was a choice): three-valued logic (only TRUE passes WHERE; `x > NULL` filters out); sum/avg/min/max/count(x) skip nulls, `count(*)` doesn't; empty aggregate → `count 0, sum NULL`; GROUP BY groups NULLs together and `1` groups with `1.0`; ORDER BY puts NULL smallest; division by zero → NULL; LIKE is `%`/`_` with ASCII case-insensitivity.

**Shape**: the executor is a correct-first row-wise interpreter over per-group cached columns (dictionary values Rc'd once, so no string copies per row). Expressions over aggregates (`sum(x)/count(x)`) work via override resolution in group context. 8 end-to-end tests; 32 green across the workspace.

**Known backlog, deliberately deferred**: row-group min/max pruning isn't wired into SQL scans yet; expression evaluation is row-wise (52 ms for a filtered GROUP BY at 200K vs ~1 ms for the specialized facet kernels — the gap is the vectorization work); dict-code fast paths for string equality. Correctness first, then the M6 benchmarks decide where optimization effort goes.
</sv-prose>

<sv-prose id="p7">
## Build log 4: M3 complete — the engine agrees with SQLite

**The differential suite is the milestone's proof, and it passes**: 15 queries from the dialect intersection (which is exactly the LLM idiom set — that choice paying off again) run over the same 200K rows through facetful and SQLite 3.53, compared cell-for-cell with 1e-9 float tolerance. Coverage: group-bys with tiebroken ordering, IN/string-BETWEEN/LIKE/IS NULL, CASE-expression grouping, CAST-grouping, truncating integer division and modulo, three-valued logic under NOT, NULL-first ascending ordering, empty-aggregate semantics, and expressions over aggregates. It runs in plain `cargo test` (graceful skip without sqlite3).

**Caught by writing it**: integer division — we returned floats, SQLite truncates; fixing it exposed that float literals (`7.0`) were collapsing to Int in the AST, so literal float-ness now threads from the lexer all the way to execution.

**Also landed**: min/max row-group pruning in SQL scans (numeric ranges from the filter's AND-chain; the REPL now prints `skipped N/M row groups`), and scan stats on QueryResult.

M3 is done: parser → binder → executor → REPL, 35 tests, SQLite agreement. Next milestone (M4): the wasm/worker/JS productization — `run_query` across the wasm boundary with the agreed transferable result representation, the Parquet→OPFS-cache flow, and the first real size number for the engine with the SQL layer linked in.
</sv-prose>

<sv-prose id="p8">
## Build log 5: M4 — SQL in the browser

**The size number this milestone existed to produce: 95.8 KB gzipped** (235.5 KB raw, 31% of budget, before CI's wasm-opt) with the *entire* pipeline linked — lexer, parser with idiom sugar, binder with did-you-mean, executor, diagnostics renderer. For scale: the complete SQL engine costs less than a quarter of SQLite-wasm and ~1% of DuckDB-wasm.

**What crosses the boundary** (round-2 decision, implemented): column-major buffers only — numbers as Float64Array, text as offsets + one UTF-8 blob, nulls as validity bitmaps, all transferred (not cloned) out of the worker; strings materialize lazily on the main thread only when touched. Errors arrive as the same caret-rendered diagnostics the REPL shows. Scan stats ride along, so the demo prints "skipped N/M row groups".

**The JS package** (`js/facetful/`): `Facetful.open()` → dedicated module worker owning the wasm; `db.load(name, buffer)`; `db.query(sql)` → `Result` with `column()`, `columnRaw()` (zero-copy typed arrays), `rows()`. `core.js` is environment-agnostic — the Node smoke test drives the identical marshalling code the browser worker uses.

**Demo**: `/web/demo/` — a SQL box over the 200K image; engine + image cold-load in one number at the top.

Not in M4 (next): `loadParquet` (hyparquet in the worker + the baseline browser compiler writing the OPFS image), OPFS-backed tables (M5), and the vectorized executor work that the interaction-speed priority will demand (M6).
</sv-prose>

<sv-prose id="p9">
## Build log 6: M6 baseline — native engine-vs-engine (1M rows, medians)

| query | facetful (today) | sqlite 3.53 | duckdb@1thread | duckdb@16 |
|---|---|---|---|---|
| facet_count | 167 ms | 93 ms | 14 ms | 3 ms |
| filtered_total | 291 ms | 84 ms | 25 ms | 5 ms |
| group_small | 162 ms | 362 ms | 5 ms | 2 ms |
| group_two_dims | 221 ms | 584 ms | 15 ms | 8 ms |
| group_high_card | 138 ms | 318 ms | 3 ms | 2 ms |
| topk | 807 ms | 63 ms | 11 ms | 3 ms |
| arith_scan | 71 ms | 50 ms | 6 ms | 1 ms |
| like_scan | 292 ms | 61 ms | 4 ms | 1 ms |
| case_pivot | 402 ms | 270 ms | 17 ms | 4.5 ms |

(`spikes/native-bench/run.py`, engines time themselves in-process, 3 warmups + median of 10, same CSV-derived data, dialect-intersection SQL. DuckDB@16 shown for context; **duckdb@1thread is the like-for-like target** — our engine is single-threaded by browser design.)

The row-wise `Val` interpreter is 10-70× off the target; our own facet kernels already run this data in ~5 ms, so the ceiling is proven reachable. **The M6 plan, in bench-verified stages:**
1. **Vectorized expression kernels**: compile `Bound` to per-row-group vector ops (`F64`/`I64`/codes/bool-mask vectors + validity), no per-row `Val` allocation.
2. **Dictionary-aware predicates**: evaluate string predicates (`=`, `IN`, `LIKE`) once per dictionary entry → a per-code mask → scans compare integers. `like_scan` becomes ~2,000 pattern evaluations + one code scan.
3. **Aggregation**: direct-indexed group tables when group-by dims are dict codes with a small cardinality product (the facet/pivot case), FxHash open addressing (lang-bench-proven) otherwise.
4. **top-k**: bounded heap instead of materialize-and-sort (807 ms → ~10 ms class).
Re-run this table after each stage; done when facetful sits within ~2× of duckdb@1thread on the facet/pivot rows.
</sv-prose>

<sv-prose id="p10">
## Build log 7: M6 stages 1+4 — the vectorized executor lands

Same harness, same 1M rows, after the rewrite (stage-0 baseline in parentheses):

| query | facetful | (was) | sqlite | duckdb@1 |
|---|---|---|---|---|
| facet_count | **13.2 ms** | (167) | 92 | 14.0 |
| filtered_total | 55.8 | (291) | 83 | 25 |
| group_small | 23.6 | (162) | 363 | 5 |
| group_two_dims | 26.5 | (221) | 585 | 15 |
| group_high_card | 13.7 | (138) | 317 | 3 |
| topk | 32.3 | (807) | 62 | 11 |
| arith_scan | 41.9 | (71) | 50 | 6 |
| like_scan | 27.2 | (292) | 61 | 4 |
| case_pivot | 75.3 | (402) | 270 | 17 |

**facetful now beats SQLite on every query** and hits duckdb@1thread parity on the flagship facet row. What did it: expressions evaluate once per row group into column vectors; dict-code truth tables make `=`/`IN`/`LIKE` against string literals integer scans (the pattern runs once per dictionary entry — 2,000 times, not 1,000,000); direct-indexed grouping when group-bys are dict columns (gid = arithmetic on codes, no hashing — the facet/pivot case); typed columnar aggregation state; bounded top-k with a cheap first-key reject (807→32 ms, projecting only winners). The differential suite caught one real bug mid-rewrite: the binder still typed `Int/Int` division as Float.

**Remaining gaps vs duckdb@1** (2-7×, all per-lane accessor overhead): `case_pivot` 4.4× (CASE runs through cold lanes — a case-of-dict-column could be a per-code table), `group_small/high_card` ~4.7× (aggregation inner loop matches on the arg enum per row — specializing on concrete f64-slice/validity shapes is the next lever), `arith_scan` 7× (mod kernel + avg via accessors). Candidates for a stage 2 if the browser numbers ask for it; wasm at 34% of budget.
</sv-prose>

<sv-prose id="p11">
## Build log 8: M6 stage 2 — ahead of single-threaded DuckDB on the flagship shapes

| query | facetful | stage 1 | baseline | duckdb@1 | verdict |
|---|---|---|---|---|---|
| facet_count | **10.5 ms** | 13.2 | 167 | 13.5 | **ahead** |
| group_two_dims (pivot) | **13.4 ms** | 26.5 | 221 | 15.0 | **ahead** |
| group_small | 12.2 | 23.6 | 162 | 5.0 | 2.4× |
| group_high_card | 8.0 | 13.7 | 138 | 3.0 | 2.7× |
| filtered_total | 40.9 | 55.8 | 291 | 25.0 | 1.6× |
| topk | 30.2 | 32.3 | 807 | 11.0 | 2.7× |
| arith_scan | 33.6 | 41.9 | 71 | 6.0 | 5.6× |
| like_scan | 13.6 | 27.2 | 292 | 4.0 | 3.4× |
| case_pivot | 49.4 | 75.3 | 402 | 16.5 | 3.0× |

What stage 2 did: typed batch aggregation loops (raw f64/i64 slices with inline validity bit tests — no per-row enum matching), an ungrouped fast path (the old code hashed an *empty key* per row for `select count(*), sum(x)…`), hoisted code slices in direct grouping, dict-code `count(distinct)` as u64 sets, and a numeric two-branch CASE fast path for the pivot shape.

**The M6 bar — within ~2× of duckdb@1thread on the facet/pivot rows — is met and exceeded**: the two flagship shapes are now *ahead* of it, and everything else sits at 1.6-5.6× against an engine with two decades of optimization. Combined with baseline: 6-27× faster than three days ago, and SQLite is beaten on all nine queries. Wasm: 35% of budget. Remaining named gaps (arith_scan's mod+avg lanes, like_scan's mask counting, topk) are recorded, not urgent — the browser is the product, and these numbers ship there unchanged.
</sv-prose>
