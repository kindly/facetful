<sv-page label="Full-text search: design">

<sv-prose id="fts-brief">
## Full-text search for facetful apps

1 October 2026. This replaces the 27 September handoff and the 29 and 30 September drafts. It records the design David and Claude worked out over four sessions, the measurements on grantnav, and the prototype built so far.

**Where it landed.**

- Search lives **inside facetful**, behind the `search` cargo feature, as a `search()` predicate, with `search_rank()` for ordering and a `searchSuggest` call for word completions and did-you-mean. There's no separate module and no extension mechanism.
- The match set comes from a **word index that the browser builds while the image loads**, held **in memory**. Nothing extra is downloaded. Until the index is ready, and in the lite build, `search()` scans the text instead (built, see *Prototype*).
- Words match at **word starts**, after spell checking against the data's own vocabulary and expansion to variants and inflections. Every word must match. Ranking is rule-based tiers, Algolia and Meilisearch style, not Lucene-style relevance.
- **Load time** is the main bottleneck. The plan is to split the image into two aligned files (descriptions and identifiers second) that the engine scans as one table, to stream the load, and to let each app ship its own pre-aggregates for the first screen.

**Budget.** facetful's limit goes from 300 KB to 500 KB gz. `scripts/size-check.sh` gates the full build at 500 KB and the lite build (`--no-default-features`, no search) at 300 KB.

**Next job:** prototype the index build and measure build time and memory on grantnav (see *Next steps*).
</sv-prose>

<sv-prose id="fts-query">
## Query model

**The match set is every word, after spell checking.** A row matches when it contains each query word in one of that word's accepted forms. No word is dropped to find more results. Suggestions do that job instead, by steering the user towards phrases that exist.

**Each word expands at query time**, all against the vocabulary (every distinct folded word with its row count):

| expansion | rule |
|---|---|
| exact | the folded word itself |
| spelling variants | a small built-in UK/US list (`-ise/-ize`, `-our/-or`, `-re/-er`, `-mme/-m`) plus an optional equivalence file given at build time (`kerb`/`curb`, `uk`/`united kingdom`). These count as exact matches, not typos. |
| inflections | strip a known English ending to a base (plurals `-s/-es/-ies`, `-ed`, `-ing` with dropped `e` and doubled consonants, `-er/-ers`), generate the base's forms, and keep only the forms that exist in the vocabulary. `fund` gives `funds`, `funded`, `funding`, `funders`, never `fundamental`. Don't strip `-s` after `-ss`, `-us`, `-is`. English only, and switchable per field and per query. Name fields default to plurals only. |
| typos | only when the word is missing from the vocabulary or a neighbour within the allowed edits is far more common (about 50 times, to tune). Allowed edits by length: none under 4 characters, 1 for 4 to 7, 2 for 8 and over, Damerau-Levenshtein. Keep a capped number of candidates, the most frequent first. |
| prefix | the last word, if the query doesn't end in a space. Typo tolerance on a prefix starts at 4 characters. |

A word's bitmap is the union of its accepted forms. The query ANDs one bitmap per word. That gives the same rows as running every combination of corrections separately, at a cost that grows with the number of candidates and not their product. Each word keeps its bitmap split by how it matched (exact, inflection, typo) so ranking can tell them apart.

**Stemming at index time is out.** It mangles names, can't be undone, and loses the difference between an exact and an inflected match.

**The dictionary is the data's own vocabulary**, with each term's row count. No language dictionary is shipped. Corrections only matter if they find rows, names and specialist terms aren't in language dictionaries, and mixed languages come for free. The data's own misspellings sit in the vocabulary too, which is why the typo rule compares against more common neighbours instead of only checking whether the word exists.

**Ranking is rule-based tiers, not a relevance score.** In order: field weights (each word counts the best-weighted field it is in, summed over the words, lower first), then words matched exactly (as typed, another spelling, an inflection) over words matched only as the start of a longer word, then words matched only through a typo, then the app's own ORDER BY. `search_rank` computes all three (see *The `search()` operator*). Phrase order and adjacency were dropped as tiers (3 October): too costly per keystroke without positions. BM25 and term frequencies are not planned.

**The engine reports what it did.** For each word it returns the forms it used with their row counts (`halth: health 41K, halt 310`). The app can show "including results for *health*" and pin a correction with one click, which just re-queries. Quoting a word turns its expansion off.

**Words match at word starts.** `men` matches `mental` and `members`, not `government`. On grantnav, `men` as a word start is in 4.7% of rows and anywhere in a word in 39.3%; `me` is 16% against 71% (see *Key measurements*). Results are better, and candidate sets are far smaller. Matching inside words stays available as an option: with the index, it scans the vocabulary (269K words, about 3 MB) for the fragment instead of the text (about 226 MB), then unions the matching words' rows.

**Broad prefixes.** A term that matches too many rows comes back flagged as too broad, with its row count, so the app can show "keep typing". There's no fixed minimum length. `a` matches 1,519,008 of grantnav's 1,520,378 rows and is useless as a filter. `qz` is fine at two characters. The limit is a reader option.

**Still to settle:** the exact tier order, and how identifiers and numbers tokenize (`GB-CHC-123`, `2024`).
</sv-prose>

<sv-prose id="fts-index">
## The word index, built on load

**Why on load.** Every earlier reason to avoid an inverted index was download size: tens of MB on top of grantnav's 58 MB. Built in the browser from the image, it adds nothing to the download. What's left is CPU at load time and memory. An index shipped from the converter was optimizing for download: one read per prefix, heavy compression. Those advantages matter little once nothing is downloaded.

**Layout: group-major.** Each row group (65,536 rows) gets its own segment: the ids of the words present in that group, sorted, and each word's rows as a Roaring-style container (a sorted `u16` array, or an 8 KB bitmap when dense). A query reads one entry per group, which matches how facetful executes. Groups already ruled out by facets, stats or an empty cached mask skip the index read. A prefix is a contiguous range of word ids within each group. There's no merge step.

**Word ids.** If the image has a vocabulary section, ids are global and known before the first group arrives. Without one (`loadCsv`, Parquet, older images), each group keeps its own sorted word list and the browser merges the 24 sorted lists into a global vocabulary at the end, with a small id map per group.

**What the converter can prepare, cheaply:**

- The vocabulary with row counts: 806 KB brotli for grantnav. Lists can be allocated at their final size, ids are fixed, and spell checking and "too broad" work before the index is ready.
- Optionally a minimal perfect hash over the vocabulary, about 100 KB, if word lookup turns out to dominate the build.
- An "all ASCII" flag per text segment, to skip folding checks.

Measure first. The build may already be hidden behind the download.

**Memory: held in memory by default.** No browser storage is needed. Rough figures for grantnav (49.0M (word, row) pairs, measured): Roaring containers at up to 2 bytes per entry, with bitmaps for the 81 words in over 100K rows (about 16 MB of bitmaps), come to roughly **60 to 80 MB**, plus about 3 MB of words. Packed deltas would roughly halve the sparse part at some build cost. gratnav's worker already peaks around 520 MB with every row held, so this adds 10 to 15%. Smaller datasets need far less: GEM units (183K rows) should be a few MB. On phones grantnav is at the edge, and the scan fallback covers it.

**Build time: not measured.** Estimated 2 to 4 s in wasm for grantnav: one tokenizing pass over about 226 MB of plain text, dictionary columns tokenized once per distinct value, about 35M word lookups and 49M appends. Built row group by row group as the image streams in, most of it overlaps the download (58 MB takes several seconds even on a fast connection). On later visits the image opens from OPFS in milliseconds; the index then comes from the OPFS cache below, or, without it, rebuilds in the background after the app is interactive while `search()` scans.

**OPFS as a cache, never read on demand.** After the first build, write the whole index to OPFS as one file in its in-memory layout. Later visits read it all back into memory (on the order of 0.1 s for 60 to 80 MB, estimated) instead of rebuilding for 2 to 4 s. A small header holds the image's version, the split id if any, and a format and tokenizer version. Any mismatch, or a missing or unreadable file, means rebuild. Without OPFS, rebuild each visit. Paged reads from OPFS or IndexedDB were considered and left out: they only help when the index doesn't fit in memory, and then the scan path is the fallback.

**What the index changes:** finding rows for a word or prefix becomes a lookup, not a scan. Typo, variant and inflection expansion becomes a union of word lists. Common words get cheap too: `support` (35% of rows) is about a millisecond to turn into a mask, against 40+ ms of scanning with 530K hits. `search_rank` reads the same row lists, kept per field.

**Tokenizer and folding**, shared by the index build, the scan and the converter:

- Unicode lowercase (already in the build for `lower()` and `LIKE`).
- Accent folding through a table of the 958 precomposed characters that map to one base letter once U+0300 to U+036F are dropped, about 2 to 3 KB gz. In decomposed text, skip U+0300 to U+036F. **Don't strip every combining mark.** Dropping all `\p{M}` removes Hindi and Thai vowel signs and decomposes Korean syllables. JS's `normalize` and `Intl.Collator` do this too, but calling them per row costs more than the table.
- Split on non-alphanumerics, but keep apostrophes inside words (`children's`, not `children s`).
- `Intl.Segmenter` could split Chinese and Japanese into words later, since the browser ships the dictionary that needs.
</sv-prose>

<sv-prose id="fts-interface">
## The `search()` operator

```sql
select … from grants
where search('mental helth') and country = 'UK'
order by search_rank('mental helth', title, 1, recipient, 3, description, 5), amount desc
limit 50
```

`search('q')` is one conjunct, cached as one mask like any other. Everything specific to search stays inside it, so the executor's OR handling and the generic mask cache don't change.

- **Columns.** For now an explicit list, `search('q', col, …)`. Later the image can declare its search columns.
- **With the index:** per word, look up the word or prefix range, union the accepted forms, AND across words.
- **Without it** (index still building, lite build, memory too tight): scan the columns. Each word's mask is cached, keyed by the column set, and a longer word is checked only on the rows its cached shorter form matched, when that's under a quarter of the rows. Built and measured (see *Prototype*).
- **Expansion** comes from the vocabulary: spell check, variants, inflections (see *Query model*).
- **Too broad.** The vocabulary gives a prefix's row count before any work, so `m` or `co` can report "keep typing".

**`search_rank('q', col[, weight], …)`** is a sort key, lower first: field weights, then exact, then typos (see step 6 of *Next steps*).

**Suggestions** come from one engine call, something like `db.searchSuggest(table, q, { where, didYouMean })`, not SQL:

- **Word completions** for the word being typed: the vocabulary's prefix range, the most common candidates counted against the rows the other words and the facets leave (each candidate's row list ANDed with the current mask). Spell-checked candidates come from the same vocabulary.
- **Did you mean**, on request: the query stays as typed, and the best alternative comes back with its count. Candidates within 1 to 2 edits per word; a few combinations counted through the index; prefer ones that return rows, then fewer edits, then more rows. Also split words (`climatechange`) and joined words (`fund raising`).

**Phrase completion was dropped** (3 October): taken from document text it needs an arbitrary length (one word, two, up to punctuation), its suggestions read oddly (*mental health lead to*), and repeated boilerplate dominates row counts. Query-log systems get phrase ends from users; we have no logs. A single next word after a space would avoid the length question, but needs a text scan or positions, so it is left out unless word completion feels thin.
</sv-prose>

<sv-prose id="fts-load">
## Load time

The download is the main wait, and compression is near its floor (see *Where the download bytes go*). The plan:

**1. Split images, scanned as one table.** The converter writes one source as two images with the same row groups, `facetful convert --split "Description Rare,Identifier"`. Both carry the same **random split id**, generated at conversion. `db.attach(table, file)` checks the split id and the per-group row counts (free, from both catalogs, and it stops a bad pair reading past a segment), then appends the second image's columns to the table. Appending keeps column indices stable, so cached masks stay valid. Queries that name a column not yet attached fail with a clear error. The app's SQL is unchanged: one table, and `search()` spans both halves. For grantnav the first image is about 27 MB of the 57 MB brotli, so the app becomes interactive at about half the wait, and the descriptions join search when the second file arrives. The engine work is a table with more than one source: a map from column to (source, column), with per-source dictionaries and stats. Nothing below the segment reader changes.

**2. Streaming load.** Hand the worker the response stream instead of a finished buffer. A `.facetful` file reads front to back: header and dictionaries first, then row groups, each with its own header. That lets the index build per group as the bytes arrive, write to OPFS as they arrive instead of holding full-size copies (gratnav keeps the compressed body and the decompressed buffer today), and show the schema and dictionary values early.

**3. Pre-aggregates at the app level.** No format change. Each app runs its own first-screen SQL against the image at build time with the facetful CLI and writes the results to a small JSON file: `{ sql or key → result columns }`. Any result shape fits. The app fetches it first (a few KB), renders facets and totals, and switches to live queries once the image is ready. Tag it with the image's version (gratnav already derives one for its OPFS path) and ignore it on a mismatch. Because it's the same SQL on the same engine, the counts can't disagree with live ones.

**Smaller download savings**, not yet measured: dictionaries with 32-bit codes for values that repeat far apart (Recipient has 425K distinct values in 1.5M rows; Title probably similar), amounts as scaled integers (3.1 MB as float64 today), and checking whether Identifier is needed in the browser at all.
</sv-prose>

<sv-prose id="fts-text">
## Prototype (30 September, uncommitted)

The scan-based `search()`:

- `crates/facetful-engine/src/sql/exec/search.rs`: query words (folded as `LIKE` folds, split on non-alphanumerics, words contained in another word dropped), the WHERE conjunct path with per-word masks and narrowing, and the row-by-row path for SELECT and OR.
- `mask_cache.rs`: `put_word` and `word_superset`, per-word entries scoped by column set.
- `filter.rs`, `eval.rs`, `binder.rs`: the hook, the function, and bind checks (query must be a text literal, arguments must be text columns).
- `text.rs`: `mark_rows_ci`, the one-pass blob scan.
- The `search` feature in both crates' `Cargo.toml` (the wasm crate turns off the engine's default features).
- `tests/search.rs`: `search()` against the `LIKE` expression it stands for, cold, cached, while typing, in SELECT and under OR, with a facet, and bind errors. Breaking the matcher fails them.
- `scripts/size-check.sh`: lite and full builds, each with its budget.

Matching is still **substring**, as `LIKE`. Word starts come next.

Size without wasm-opt: lite 291.7 KB gz, full 301.4 KB gz, so **+9.8 KB** for the scan path.

On grantnav (`spikes/fts-bench/search-bench.mjs`, same row counts as `LIKE` every time):

| | per-term `LIKE` | `search()` |
|---|---|---|
| `climate`, cold | 75.5 ms | 39.7 ms |
| `mental health`, cold | 128.6 ms | 74.3 ms |
| `youth sport london`, cold | 187.4 ms | 116.7 ms |
| typing `mental health`, total | 1,053 ms | 566 ms |
| typing `youth sport london`, total | 1,370 ms | 700 ms |
| typing `climate change adaptation`, total | 1,885 ms | 730 ms |

Typing costs 40 to 60 ms per letter for the first letters of a word and 20 to 45 ms once narrowing applies, against 75 to 105 ms for `LIKE`. Narrowing only runs when the cached shorter word admits under a quarter of the rows; without that rule the 2nd and 3rd letters took 100 to 135 ms.

Cold cost per word, split by column (each column as its own query, so each figure carries some fixed overhead): a word with no hits costs 29.9 ms over all five columns. Plain text runs at 5 to 9 GB/s (Title 8.8 ms, Recipient 7.1, Description Rare 15.2). The two dictionary columns cost about 6 ms each even with no hits. `climate` adds about 22 ms of hit handling and `e` about 59 ms. So the scan path's realistic floor is about 20 ms per new word, after fixing the dictionary cost and checking later words only on earlier words' rows.
</sv-prose>

<sv-prose id="fts-index-results">
## Index prototype (3 October)

`crates/facetful-engine/src/search_index.rs` builds the word index; `Table::build_search_index(cols)` keeps it, and `search()` uses it when its columns match the index's exactly. Both paths match at **word starts**, folded and split by one tokenizer (`for_each_word`), cache each word's mask under the same key, and give identical rows: `tests/search.rs` checks both against a brute-force reading on every query and keystroke. In JS, `buildSearchIndex(table, columns)` and `dropSearchIndex(table)`.

**Structure.**

- Vocabulary: every folded word, in one byte buffer with end offsets, **in word order**, so a prefix is a range of word ids (two binary searches). Row counts per word.
- Plain text columns, per row group: one row list per word and field (list id = word id × fields + field, so a word range is still one run), the list ids present (ascending) and, parallel to them, each list's rows: a sorted `u16` array, or a 65,536-bit bitmap above 4,096 rows. A prefix is one contiguous run of a group's entries.
- Dictionary columns: per word, the codes of the values that hold it (CSR). A query turns a word's codes into a byte table and reads each group's codes. That keeps Description Common, whose 61K values repeat about 17 words each over 820K rows (26M of the 67M word occurrences), out of the build and the row lists.

**Build.** The vocabulary is discovered while reading; plain columns are read row by row across columns, so each word's rows arrive in order; pairs (word id, row) go into one buffer per group and a counting sort on word id turns them into lists, dropping repeats. ASCII text finds words from a bitmask of letter-and-digit bytes, and a word of up to 15 bytes becomes its hash key in one 16-byte load. A last pass renumbers the words into word order.

grantnav, five search columns:

| | first version | now, native | now, wasm (Node) |
|---|---|---|---|
| build | 2.92 s native, 3.9 s wasm | **1.42 s** | **2.29 s** |
| (word, row) pairs in row lists | 49.0M | 30.8M, plus 1.65M (word, value) pairs | same |
| index holds | 79.6 MB | 75.4 MB | 75.4 MB |

Row lists per field (3 October, for `search_rank`'s field weights) cost 12.5 MB, 0.1 s of build and 34 MB of wasm high-water memory. Before them, one list per word: 27.6M pairs, 62.9 MB (lists 58.8, vocabulary 4.2), 1.32 s native, 2.16 s wasm.

Without Description Rare (the first image of a split) the build is 0.61 s native and the index about 40 MB.

**Memory.** The build raises wasm memory by about 130 MB over the loaded dictionaries, of which the index holds 63 MB. The rest is a high-water mark, not a leak: rebuilding the index twice more leaves wasm memory unchanged, so the engine reuses it. Cutting the peak further would mean building each group in halves or packing pairs into 6 bytes, for perhaps 10 to 20 MB.

**Queries (wasm, mask cache on).** 5 to 8 ms per keystroke; the first letter of a word, which covers thousands of words, 5 to 22 ms. `mental health` letter by letter is 91 ms in total, against 1,262 ms on the scan path. Cold whole queries: 4.9 to 12.6 ms (`m` 6.6, `a` 8.7) against 62 to 186 ms scanning. Reading the dictionary columns' codes per word (about 3M lookups) is most of a keystroke.

**Tried and dropped.**

- A radix sort on word id: slower than the counting sort, more memory.
- Reusing a repeated short value's words within a group: too few repeats in 65,536 rows to pay for hashing every value.
- **A words file** written beforehand from the image (sorted vocabulary with row counts, and per group each word's row count, 1.47 MB brotli for grantnav) so the browser could allocate exactly and write each row straight into place: correct, but **slower** (1.71 s against 1.31 s natively). Lookups in a prebuilt vocabulary cost more than the normal build's lookups and appends, and the direct fill writes scattered. Sorted ids, its one real gain, come from the renumbering pass instead.

**Positions (estimated, not built).** About 35M word occurrences in the plain columns, about 1 byte each delta-coded plus a count per (word, row): roughly +60 to 70 MB, doubling the index, and perhaps +20 to 30% build time. They would give phrase and adjacency matching from the index. Phrase and adjacency ranking were dropped (3 October): checking text on the matching rows cost up to 157 ms a keystroke (`community project`), and a two-word index (pairs of words that both have 20K rows or more: 82K pairs, about 29 MB) was not pursued.

The scan path with word starts is slower than the substring version committed on 30 September (typing `mental health` 1,262 against 566 ms). It only serves until the index is built and in the lite build.

Size, unoptimized: lite 291.7 KB gz, full 323.6 KB gz (+32 KB for the scan path and the index).
</sv-prose>

<sv-prose id="fts-next">
## Next steps

1. ~~Last word only as a prefix~~ (done 3 October): finished words match whole words; the last word is a prefix unless the query ends in a separator. On grantnav `mental health` went from 45,916 to 45,825 rows (`mental` no longer matches `mentally`).
2. ~~Incremental build~~ (done 3 October): `IndexBuilder` (`new`, `add_group`, `finish`); `Table::begin_search_index` / `search_index_step`; in JS `beginSearchIndex` / `stepSearchIndex` on the core, and `db.buildSearchIndex(columns)` in the worker, which steps a row group at a time and yields between groups. Queries run meanwhile, scanning; the worker smoke checks a query sent mid-build is answered first, with the same count. On grantnav (wasm) a query waits at most one step: begin 198 ms (the dictionaries), a row group 71 ms, the last step 165 ms (with the renumbering). Begin and finish could be split further if that ever matters.
3. ~~Spell checker~~ (done 3 October, `SearchIndex::word_forms`): edits allowed by length (none under 4 bytes, 1 up to 7, 2 from 8; optimal string alignment, so a swap is one edit). One edit is tried first by generating every string one edit away (letters and digits) and looking each up in the sorted vocabulary, about 0.1 to 0.2 ms a word natively; two edits, and non-ASCII words, measure against words of a similar length (6 to 18 ms), and only when one edit found nothing. Kept: the 5 most common candidates with at least 1% of the best one's rows. A finished word is corrected when it is missing or a neighbour has 200 times its rows (`helth` 2 rows → *health*; `heath` 2,258 is left alone for did-you-mean); the word being typed only when nothing starts with it, to prefixes one edit away (`londn…` → *london…*). Only the index can correct: the scan has no vocabulary, so a corrected word's mask, and a `search()` answered by the index, are cached under their own keys. On grantnav every test typo corrected to the intended word (`enviromental`, `yuoth`, `chnage`, `comunity`, `projcet`, `chairty`, `wellbeeing`, `acommodation` …), and typo queries cost the same 5 to 13 ms as spelled-right ones (`spikes/fts-bench/typo-bench.mjs`). To tune: `suport` also takes *sport* and `helth` *heath* (both one edit, over 1% of the best); a prefix with a few rows (`climt…`, 2) is not corrected.
4. ~~Inflections, UK/US variants, quoted words and phrases~~ (done 3 October). All query-side: a finished word's forms are looked up in the vocabulary, nothing in the index changes.
   - **Inflections:** plurals, `-ed`, `-ing` (no `-er`: *funder*, *heather* are other words). A stem generates forms only if it is a word itself (`training` does not make *trainees* through *traine*), stems need 3 letters (`used` is not *us*), `-es` only after s/x/z/ch/sh (`plan` is not *planes*), not-plurals listed (`news`). A form needs 5 rows and 1/1000 of the word's rows (drops *charitys* 8, *planed* 9). `fund` → *funds, funded, funding*; `charities` → *charity*; `planning` → *plan, plans, planned*.
   - **UK/US spellings:** `-ise/-ize` (and forms), `-our/-or`, `-re/-er`, `-mme/-m`, doubled `l`, `-ogue/-og`, `-ence/-ense`, both ways, when the other spelling is a word. `organization` (585 rows) also finds *organisation(s)*: 92,408 rows. A spelling variant is never a typo to correct.
   - **Spelling correction** got stricter alongside: only words under 100 rows are corrected (`news`, 754, is not *new*), and never to their own singular or plural.
   - **Quoted word** (`"fund"`): only itself (345,660 rows against 735,603 for `fund `).
   - **Quoted phrase** (`"mental health"`): its words next to each other, in order, within one field; separators between them do not count (`mental-health`). Candidates are the rows holding every word as typed; then their text is checked: dictionary values only as candidate rows use them, each once; ASCII text by finding the first word with the SIMD search and comparing the words after it (a test checks it agrees with splitting into words over 400 texts and 294 phrases). An unclosed quote is a phrase being typed. Phrases work on both paths. grantnav, wasm, first time (then cached): 20 to 50 ms (`"mental health"` 45, `"youth sport"` 20), against 130 to 230 ms scanning.
   - Expansions need the vocabulary, so like corrections they only apply with the index; without it words match as typed. Outside a WHERE conjunct (a SELECT item, under OR) search() also matches as typed.
5. ~~`searchSuggest`~~ (done 3 October). `db.searchSuggest(q, { columns, where, limit, didYouMean })` (worker), `searchSuggest(handle, columns, q, opts)` (core), `sql::search_suggest` (Rust). Returns completions, the words corrected automatically, on request a did-you-mean, and `prefixRows` (the word being typed over the whole table from the index's totals, approximate: a measure of "too broad"). **No counts** (David, 3 October: the app runs the query and sees them). A first version counted every completion against the context; those counts were not what clicking finds (a word in two columns counted twice, inflections left out: *mental* showed 61,225 for 52,044 rows) and the counting was most of the cost.
   - **Completions** of the word being typed: its prefix's words, and those of prefixes one edit away when it looks like a typo; most common first, keeping those found in at least one row the other words and `where` (the app's facets, a SQL condition) leave; each check stops at the first row (row lists against the context; dictionary values against the values context rows have).
   - **The word being typed is corrected** like a finished word when a prefix one edit away has 200 times its rows (`comun…` 322 rows → *commun…*), not only when nothing starts with it; corrections add to it, never replace it.
   - **Did-you-mean:** each corrected word replaced by its best correction (the word being typed by its first completion, a whole word: `climt` → *climate*), each real word whose neighbour has 20 times its rows by that neighbour (`heath` → *health*), never a word's own singular or plural (`news`, *new*); offered when it finds a row under the filter.
   - grantnav, wasm, every keystroke of `mental health`, `youth sport london`, `climte chnage`, `comunity projcet`, with and without a Region filter: median 4.5 to 8.5 ms, worst 18 ms (`spikes/fts-bench/suggest-bench.mjs`; with counts it was 10 to 17 and 30). To tune: corrected prefixes can be loose (`comun…` also takes *coun…*: *council*, *country*).
6. ~~`search_rank`~~ (done 3 October). `search_rank('q', col[, weight], …)`, an integer sort key, lower first. Weights are whole numbers after a column; a column without one gets its position (1, 2, …). Per row:
   - **Field weights:** for each query word, the lowest weight among the fields it matched in (with all its accepted forms: inflections, spellings, corrections, the prefix of the word being typed), summed over the words. A word in no field adds 1000.
   - **Exact:** then fewer words matched only inexactly. The word as typed, its UK/US spellings and the inflections of both are exact; the longer words the word being typed starts (`heath` → *heathland*) and typos are not.
   - **Typo:** words matched only through a correction.
   - Phrases rank as their words, quoted. Then the app's own ORDER BY.
   - With the index (top-k and full sorts over the index's columns), weights come from the per-field row lists and dictionary values; a top-k does not load the plain columns for it, only the rows the WHERE keeps are ranked, and each word's forms are looked up once per statement. Otherwise (no index, a SELECT item) it reads the text, words as typed.
   - grantnav, wasm, every keystroke, top 20 by rank then amount (`spikes/fts-bench/rank-bench.mjs`): median 15 to 30 ms against 9 to 13 ordering by amount alone, worst 85 ms (the first letter). Natively 10 to 23 ms against 8 to 12 for a cheap expression key (`examples/rank_time.rs`). The first version took 526 ms: it loaded the five text columns per group (the long descriptions), looked forms up per group and ranked every row.
   - Size, unoptimized: full 356.1 KB gz (lite unchanged, 291.9 KB).
   - A misspelling that is itself in the data ranks as exact: `comunity project` leads with *Barley Croft Comunity Counselling Project*.
7. ~~Streaming load with the build integrated~~ (done 4 October). `db.loadUrl(name, url, { searchColumns, persist, expectedBytes })` (worker), `openStream(chunks, opts)` (core), `facetful_engine::load::TableLoad` (Rust).
   - **Reading front to back:** `read::open_prefix` gives the catalog once the header and dictionaries are in; `read::next_group` reads each row group as its bytes complete. The footer has no marker, so a group header is accepted only if every segment has the length its column's type gives for that row count (only text bytes vary) and validity exactly when there are NULLs; `finish` checks the streamed groups against the footer and starts the index over if they ever disagree. The table is the same object throughout: groups join its catalog as they arrive, and at the end it takes the footer's catalog (statistics, layout flags). No copy, no second parse.
   - **Indexing:** dictionary columns are tokenized as soon as the dictionaries arrive; a row group is indexed when the reader has had to wait on the network since the last one (it is level with the download, so indexing costs nothing). When chunks are already waiting the download is ahead, and the table opens at the last byte anyway: the groups left are indexed in the background, search() scans meanwhile, `db.searchReady()` resolves when the index is in use.
   - **OPFS:** with `persist` the bytes are written as they arrive; a file cut short does not open (no footer), so `loadOpfs` refuses it.
   - grantnav, wasm in Node, the download simulated at a brotli image's rate (`spikes/fts-bench/stream-bench.mjs`):

     | wire | download | before: table / search ready | streamed: table / search ready |
     |---|---|---|---|
     | 20 Mbps | 23.2 s | 23.40 / 25.80 s | 23.22 / **23.34 s** |
     | 100 Mbps | 4.6 s | 5.33 / 7.64 s | 4.79 / **4.93 s** |
     | 200 Mbps | 2.3 s | 2.64 / 4.98 s | 2.52 / **2.65 s** |
     | 400 Mbps | 1.2 s | 1.48 / 3.79 s | 1.76 / **2.31 s** |

     Search is ready 0.1 to 0.15 s after the last byte while indexing keeps up (to about 200 Mbps; indexing is 2.3 s). Faster than that the table opens up to 0.3 s later than before (a group being indexed when the last byte lands), and search 1.5 s sooner.
   - **The image's size is in its header** (4 October). With `Content-Encoding` the response's length is the compressed size, and without the size the buffer grew by doubling: 1,105 MB of wasm memory for grantnav against 533 MB. The header now ends with the image's length (u64, after the sort keys), so the loader allocates once: 533 MB either way. Forward compatible: readers go by `header_len`, so older engines skip the field (checked: the previous commit's CLI answers alike on a new image), and an image without it reads as size unknown. Writers that hand the header out first (CSV conversion: the CLI, wasm, the JS command) fill it in at the end.
   - **Not done:** feeding the hook from the Parquet converter (needs the per-column decisions made incrementally), and a built-in "fetch once, keep in OPFS" in front of `loadUrl` (the first-interaction benchmark's image lanes do it by hand: `loadOpfs` when the file is there).
8. **Split images with attach,** each image's columns indexed as it streams, merged into one index at attach (same row groups; combine the two sorted vocabularies).
9. **OPFS cache of the whole index,** read back in full on later visits.
10. **Accent folding** and keeping apostrophes inside words (tokenizer changes; any time).
11. **Measurements in Chrome, Firefox, Safari and a phone;** build peak memory if phones need it (groups in halves, 6-byte pairs).
</sv-prose>

<sv-prose id="fts-rejected">
## Considered and set aside

| option | why not |
|---|---|
| Lucene-style retrieval (BM25, top-k) | Good for finding documents and poor for records. In practice it needs minimum-should-match tuning. We want every match, with facets, ordered by clear rules. |
| Word-pair index (Meilisearch's `word_pair_proximity_docids`, Elasticsearch's `search_as_you_type` shingles) | Gives adjacency and next-word counts from the index, but the dictionary of distinct pairs is huge. Meilisearch has open issues on its size and cut its maximum proximity from 7. |
| Word positions by default | Contains everything pairs do, and term frequency too, but would roughly double the index (67M word occurrences). Phrase order is checked in the text instead. |
| Precomputed phrase table | Its counts ignore the facet filters, and the text check gives filtered, exact counts. |
| Trigram index over the vocabulary | Scanning the 3 MB vocabulary for a fragment is already cheap. |
| Inlining small row lists in the term dictionary | Only saves round trips, and reads are local. |
| Lazy loading by HTTP range as the main mode | gratnav downloads its image once and reads it locally. |
| A separate search module, or an extension hook for building indexes while streaming | The index build must see each row group as it loads and match facetful's groups and tokenizer. A cargo feature gives the separation at compile time. |
| An inverted index shipped in a file | Tens of MB of extra download for grantnav. Replaced by building the same index on load. |
| 3-character word-start masks | Exact for 3-letter prefixes and 1.4 to 1.5 times the true matches for longer words, but 16K keys and about 44 MB (estimated, uncompressed): most of a word index, with less in return. |
| Row-level trigram masks | One 190 KB bitmap per trigram uncompressed; compressed, an estimated 250 to 300M (trigram, row) pairs, several times the word index. |
| Masks on 2-character keys (word start, 2nd and 3rd, or last two) | Too common to filter: the top keys are in 51 to 81% of rows. 2nd and 3rd characters with the first two filter worse than the first three and cost more (54 MB against 44). |
| Block-level skip indexes (ClickHouse `ngrambf`) | Only skip blocks with no match at all, so they help only words under about 0.1% of rows. |
| Facet histograms in the file format | Each app writes its own pre-aggregates as JSON at build time with the same SQL. |
| Hashing the source to verify a split | A random id shared by both halves is enough; a mismatch has to be deliberate. |
| Sorting rows, per-column compression, zstd | Measured on grantnav: per-column brotli saves 1%, sorting 5%, zstd is slightly worse. |
| A combined search column | Duplicates text (Description Common is 3 MB of codes but 170 MB as per-row text) and scans no faster. |
| A fixed minimum query length | Replaced by the row-count limit per term. |
| Pinning each typo to a single correction | The union of candidates costs no more, and the app can pin one on request. |
</sv-prose>

<sv-prose id="fts-keys">
## Key measurements (30 September)

**Which characters make useful mask keys** (`spikes/fts-bench/keys.mjs`). Share of grantnav's rows each scheme's mask lets through for a typed prefix:

| typed | word starts with it | first 2 chars | first 3 chars | first 2 + 2nd/3rd | anywhere in a word |
|---|---|---|---|---|---|
| `men` | 4.7% | 16.4% | 4.7% | 8.8% | 39.3% |
| `mental` | 3.4% | 16.4% | 4.7% | 8.8% | 6.4% |
| `hea` | 10.6% | 23.1% | 10.6% | 14.4% | 12.4% |
| `cli` | 3.0% | 12.9% | 3.0% | 3.3% | 3.6% |
| `lon` | 7.2% | 27.6% | 7.2% | 12.7% | 8.0% |
| `com` | 32.6% | 62.3% | 32.6% | 35.7% | 36.5% |
| `pro` | 57.7% | 62.9% | 57.7% | 58.7% | 61.5% |

Keys and rough storage (1.5 bytes per row entry, a key capped at a 190 KB bitmap, uncompressed): first 2 characters 1,343 keys, 27 MB; first 3, 16,043 keys, 44 MB; 2nd and 3rd, 1,291 keys, 27 MB; last 2, 1,355 keys, 23 MB; any pair, 1,537 keys, 58 MB. Common prefixes stay expensive whatever is stored, because the words themselves are common (`project` 29%, `support` 35%).

### Where the download bytes go

`spikes/fts-bench/colsize.mjs`: each column compressed on its own in its stored form, brotli q11 (window 24) and zstd 19 with long-distance matching.

| | brotli | zstd |
|---|---|---|
| whole file, as deployed | 58.0 MB | not run |
| per column, file order | 57.3 MB | 59.2 MB |
| per column, sorted by funder, programme, date | 54.4 MB | 55.9 MB |

| column | raw | brotli | sorted |
|---|---|---|---|
| Description Rare | 141.3 MB | 24.4 MB | 23.8 MB |
| Title | 62.8 MB | 10.3 MB | 9.9 MB |
| Identifier | 52.0 MB | 6.2 MB | 4.9 MB |
| Recipient Org:Name | 40.0 MB | 5.3 MB | 6.7 MB |
| Amount Awarded | 12.2 MB | 3.1 MB | 2.8 MB |
| Description Common | 16.0 MB | 3.0 MB | 2.7 MB |
| everything else | about 32 MB | about 5 MB | |

Text is 86% of the download. The file already comes grouped by funder (the funder columns compress to almost nothing in file order), so sorting gains little and makes Recipient and the region columns worse.
</sv-prose>

<sv-prose id="fts-results">
## First results (29 September, facetful 0.7.1, Node 26)

These were measured to judge scanning without an index. The (word, row) count and the spell-check results feed the index design.

Harness in `spikes/fts-bench/`: `vocab.mjs` builds the vocabulary from the image, `bench.mjs` covers measurement 6 and spell check plus `LIKE`, and `bench2.mjs` covers inflections, pruning and the JS text check. Raw output is in `spikes/fts-bench/out/` (not committed).

**Vocabulary.** 268,761 distinct terms, 66.9M word occurrences, and 49.0M distinct (term, row) pairs. JS takes 16 s to build it from the image. The term list with row counts is 2.9 MB raw and **806 KB brotli**, against the image's 58 MB. Only 81 terms occur in more than 100K rows, but they hold 19.1M of the pairs. The 750 terms above 10K rows hold 36.4M, which is 74%. Terms at or under 10K rows hold 12.6M pairs. At roughly a byte per pair, a shipped index would have been tens of MB of download, which is why it was first set aside.

**`LIKE` on 0.7.1**, mask cache off. The truth-table fix worked. Per column for `climate`, Description Common dropped from 69.6 to 4.5 ms and Programme from 13.2 to 4.5 ms. Description Rare (27 ms) and Title (13.8 ms) now dominate.

| query | one substring | per-term AND |
|---|---|---|
| `climate` | 72.8 ms | 72.0 ms, 31,093 rows |
| `mental health` | 68.8 ms | 123.5 ms, 62,946 rows |
| `youth sport london` | 64.7 ms, 0 rows | 184.8 ms, 209 rows |

Typing per term with the mask cache on costs **70 to 107 ms per keystroke**, and a space costs 1.5 to 3 ms. The cache's narrowing doesn't apply to an OR across five columns, so every letter rescans.

**Spell check against the vocabulary**, by brute-force OSA distance in JS with a length filter, takes **6 to 26 ms per finished word**, and results can be cached per word. Corrections were right in every test: `enviromental → environmental`, `helth → health, heath`, `yuoth → youth`, `chnage → change`, `projcet → project`, `chairty → charity`, `organization → organisation`. Two rules need tuning:

- The 50 times rule expands `heath` (2,258 rows) to `health` (120,730). That's harmless for ranking, but it adds 120K rows to the match set and the facet counts. Real words above some absolute row count probably shouldn't expand.
- The candidate cap keeps noise: `sprot` gives `sport, spot, prot, sprout, sprog`. Keeping only candidates with at least 1% of the best candidate's rows fixed every test case.

**`LIKE` over corrected words, without an index.** Each accepted form costs one full scan across the columns, about 65 ms, so the cost grows with the number of forms. `yuoth sprot londn` with 11 forms took 1,467 ms, and 186 ms after pruning to 3. Typo queries with two or three forms per word took 300 to 410 ms.

**Substring and whole-word counts are close.** `LIKE '%w%'` behaves like whole words plus inflections plus some junk:

| query | LIKE | whole words | + inflections |
|---|---|---|---|
| `organisation support` | 47,313 | 13,979 | 45,955 |
| `fund` | 739,170 | 345,608 | 735,944 |
| `community project` | 123,340 | 115,753 | 122,550 |
| `mental health` | 62,946 | 45,482 | 45,482 (`healthy`, `healthcare`, `mentally` aren't generated) |
| `plan` | 58,131 | 28,036 | 42,892 (`planet`, `explanation` are junk) |
| `heath` | 3,373 | 2,258 | 2,358 (`heathrow` is junk) |

The inflection generator also keeps forms that are really misspellings in the data (`londond`, `communitied`, `charitying`) and wrongly takes `heather` as a form of `heath`. It needs a minimum row count per form, and the doubling and `-er` rules need tightening.

**JS text check over candidate rows.** Per-word `LIKE` stood in for `mask('q')`. The steps were: decode each result lane once, lowercase it (NFD for non-ASCII rows), `indexOf` with a word-boundary check, then `next_words`.

| phrase | filter | candidates | text | lanes | exact | next_words |
|---|---|---|---|---|---|---|
| `mental health` | none | 62,946 | 49.8 MB | 262 ms | 37 ms | 99 ms |
| `mental health` | North East | 3,049 | 2.1 MB | 9 ms | 2 ms | 5 ms |
| `climate change` | none | 26,250 | 42.6 MB | 184 ms | 28 ms | 62 ms |
| `climate change` | North East | 960 | 1.6 MB | 4 ms | 1 ms | 2 ms |
| `support` | none | 524,179 | 180 MB | 1,184 ms | 126 ms | 797 ms |
| `support` | North East | 37,968 | 9.0 MB | 54 ms | 7 ms | 43 ms |

Decoding and lowercasing dominate. The search itself is cheap. With a facet filter or a specific phrase, plain JS is fine. For broad, unfiltered queries it isn't. The text volume is high because every row carries its full Description Common, so a UDF that runs once per dictionary value would scan much less.

**Suggestion quality.** Counting per row lets repeated boilerplate win. `mental health` suggests *lead training* (8,056) and *and wellbeing* (7,855), both from one funder's repeated description. `support` suggests *children s*, which shows the tokenizer should join apostrophes (`children's`) instead of splitting on them.
</sv-prose>

<sv-prose id="fts-today">
## What facetful does today

**`LIKE`** (`crates/facetful-engine/src/sql/exec/eval.rs`, `classify_like`; kernels in `crates/facetful-engine/src/text.rs`) sorts patterns into shapes: `%x%` contains, `x%` prefix, `%x` suffix, no wildcard exact, and anything else goes to a per-character matcher. A plain text column with a contains pattern is one wasm SIMD scan over the column's byte blob, about 4.4 GB/s on grantnav. A dictionary column evaluates the pattern once per dictionary entry. Since 0.7.1 that truth table is built once per (dictionary, pattern) and not once per row group, `LIKE` folds case for every alphabet, and `ILIKE` exists. There's still no accent folding. `unaccent()` and `regexp()` are ready-made JS UDFs.

**Mask cache** (`crates/facetful-engine/src/mask_cache.rs`) keeps a bitmap per WHERE conjunct and row group. A contains `LIKE` that extends a cached needle on the same column checks only the cached rows, which is what makes typing cheap.

**Earlier spike** (`spikes/text-bench`, `docs/design.sv`, *Engine architecture*). A stemmed token index over 200K rows of ~230-character notes: about 0 ms for one term, 0.07 ms for a two-term AND, 276 ms to build, 17 to 25% of the text volume.
</sv-prose>

<sv-prose id="fts-measured">
## Measurements without an index (27 September, facetful 0.7.0)

Node 26 (V8), mask cache off unless stated. Each term is `LIKE '%term%'` ORed across the searched columns, and terms are ANDed.

### GEM units, 183,125 rows, `data/units-2026-08.facetful`

Searched: `Plant / Project name` (plain, 6.5 MB) plus 10 dictionary columns.

| typing `adani solar guj` | ms per keystroke |
|---|---|
| any letter inside a word | 23 to 25 |
| typing a space (the finished term is cached) | 0.3 to 0.5 |
| cold, 3 terms at once | 71 |

The cost follows the volume of text scanned, not the row count. At this size the scan is enough for a search box, and GEM units doesn't need the index.

### grantnav, 1,520,378 rows

gratnav's search box (`~/projects/gratnav/app/src/lib/sql.ts`, `SEARCH_COLS`, 250 ms debounce) matches the whole query as one substring across five columns. The 65,000 most common descriptions are a dictionary column covering 54% of rows, and the rest are plain text cut at 500 characters.

Per column, one term (`climate`), cold:

| column | stored as | ms | rows |
|---|---|---|---|
| Description Common | dictionary, 65K values | 69.6 | 26,073 |
| Description Rare | plain, 141 MB | 32.4 | 1,893 |
| Title | plain, 63 MB | 13.6 | 6,937 |
| Grant Programme:Title | dictionary, 12K values | 13.2 | 5,130 |
| Recipient Org:Name | plain, 40 MB | 9.3 | 535 |

Whole queries, cold:

| query | one substring | per-term AND |
|---|---|---|
| `climate` | 149 ms, 31,093 rows | 149 ms, 31,093 rows |
| `mental health` | 138 ms, 41,623 rows | 261 ms, 62,946 rows |
| `youth sport london` | 134 ms, 0 rows | 400 ms, 209 rows |
| `a` | 132 ms, 1,519,008 rows | not run |

The Description Common and Programme times predate the truth-table fix and should now be much lower (estimated 145 to about 65 ms per term overall). Measurement 6 re-times them. After that fix, text volume sets the floor, about 250 MB scanned per term. That's usable behind a 250 ms debounce, but it grows with every term and every row, which is the case for the index on grantnav. Per-term matching also finds 209 grants for `youth sport london` where the single substring finds none.
</sv-prose>

<sv-prose id="fts-landscape">
## Landscape (checked September 2026)

| engine | what's relevant |
|---|---|
| Lucene 10 | BlockTree term dictionary: blocks of about 25 to 48 terms sharing a prefix, with an FST of block prefixes in memory. Postings in 128-integer bit-packed blocks with two levels of skip data. A term in one document stores its doc id in the dictionary. Positions and payloads are separate files, so queries that don't use them never read them. `FuzzyQuery` runs a Levenshtein automaton over the dictionary, at most 2 edits, with an exact `prefixLength`. |
| tantivy / Quickwit | Lucene's design in Rust. Quickwit added an SSTable term dictionary for object storage, because an FST lookup needs the whole dictionary downloaded, and a "hotcache" file that holds everything needed to open an index. |
| Meilisearch | The closest in behaviour. Ranking rules (words, typo, proximity, attribute, exactness) as tiers over roaring bitmaps, typo tolerance by word length, prefix search. Stores word-pair proximity databases, which are its size problem. |
| Algolia | The same model, hosted. Default typo tolerance: 1 typo from 4 characters, 2 from 8. "prefixLast" for search as you type. |
| Elasticsearch `search_as_you_type` | 2- and 3-word shingle subfields plus an index-prefix subfield. |
| Pagefind 1.5 | Rust indexer, wasm search, index split into chunk files by term range. Built for ranked site pages, not millions of rows. |
| SQLite FTS5 over sql.js-httpvfs | Complete features and range reads, but all of SQLite and many B-tree round trips. |
| FlexSearch, MiniSearch, Orama, Lunr, Fuse.js | In-memory JS libraries for thousands to hundreds of thousands of documents. |
| pg_trgm, Zoekt | Trigram indexes over documents with a recheck. They work with the whole index local, and the lists are large. |

Sources: [Lucene101PostingsFormat](https://lucene.apache.org/core/10_1_0/core/org/apache/lucene/codecs/lucene101/Lucene101PostingsFormat.html), [FuzzyQuery](https://lucene.apache.org/core/9_6_0/core/org/apache/lucene/search/FuzzyQuery.html), [tantivy architecture](https://github.com/quickwit-oss/tantivy/blob/main/ARCHITECTURE.md), [tantivy-sstable](https://lib.rs/crates/tantivy-sstable), [Meilisearch index size issue](https://github.com/meilisearch/meilisearch/issues/3761), [search_as_you_type](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/search-as-you-type), [Pagefind](https://github.com/pagefind/pagefind/releases).
</sv-prose>

<sv-prose id="fts-open">
## Open questions

- Build time and memory of the index on grantnav, in wasm, and on a phone.
- Tokenizing identifiers and numbers (`GB-CHC-123`, `2024`), and scripts written without spaces.
- Plural folding only, or the full inflection rules, as the default for description fields.
- When a real word should still expand to a far more common neighbour (`heath` to `health` adds 120K rows to the counts).
- Whether the caller-supplied mask is still wanted for anything; search no longer needs it.
</sv-prose>

</sv-page>
