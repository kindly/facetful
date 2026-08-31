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
