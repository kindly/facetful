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

<sv-ask id="pq1" round="1">
**Parser approach?**
- * Hand-rolled lexer + Pratt + diagnostics module — best errors, smallest, the learning part; pest as named fallback
- pest — grammar file as spec, 25 KB gz, good errors out of the box, less to write
- rust-peg — pure codegen, no grammar file, 34 KB gz
- sqlite3-parser — battle-tested full SQLite grammar (contradicts functions-over-syntax; 51 KB gz)
</sv-ask>

<sv-ask id="pq2" round="1">
**Confirm functions-over-syntax?** (the table in the first section: between/in/like/if/isnull/concat as functions; no BETWEEN, CASE, CAST, IS NULL, ||, or DISTINCT-inside-calls syntax)
- * Yes — minimal syntax, functions for everything beyond the query skeleton
- Mostly — but keep `IN (…)` and `IS NULL` as syntax (they're muscle-memory SQL)
- Prefer fuller SQL syntax even at parser-complexity cost
</sv-ask>

<sv-ask id="pq3" round="1">
**Who writes the parser?** (M3 split)
- Claude builds it end to end
- * Claude scaffolds (AST, token/span types, diagnostics renderer, planner plumbing) — David writes the grammar/parse functions
- David writes it solo; Claude reviews
</sv-ask>

<sv-ask id="pq4" round="1" role="close">
**SQL-parser direction sign-off.** On approval, M3 starts in the agreed split, building against the existing executor (facet queries become one consumer of the general SELECT pipeline).
</sv-ask>

</sv-page>
