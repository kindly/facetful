<sv-page label="Smaller downloads: measurements and proposal">

<sv-prose id="fs-brief">
## How much smaller an image can be on the wire

25 November 2026, on the `file-size` branch. This records what was measured, a prototype in `spikes/size-bench`, and the proposal that came of it. David agreed to it the same day and the packed form is now built: [What was built](#fs-built) at the end has the result and what is left.

Size here means bytes transferred: the image after brotli quality 11 (window 24, which is what `brotli -q 11` picks for files this large), with gzip -9 beside it for hosts that only serve gzip. An encoding counts only for what it saves after that compression.

**Where it landed.**

- A **packed form** of the image for the wire. The loader unpacks it row group by row group into the image the engine already reads, byte for byte the same. Queries, the search index, OPFS, split images and fingerprints see no difference.
- grantnav goes from 58.0 to **46.9 MB** brotli (19% smaller) and from 83.0 to 58.2 MB gzip (30%). GEM units goes from 6.83 to **5.63 MB** brotli (18%) and from 9.35 to 6.96 MB gzip (26%).
- Two changes give nearly all of it. Text values and dictionary values **end in a 0 byte** in place of their u32 offsets. **Common words become one or two byte codes**, which pays on prose and is neutral elsewhere.
- Unpacking the whole of grantnav takes about 0.15 s more than loading it plain (360 ms against 217 ms in wasm on this laptop), spread over the download. The decoder adds 7 KB gz to both builds (the prototype alone measured 5).
- Most of the classic columnar encodings lose to brotli on the stored bytes: bit-packing, run lengths, frame of reference, byte planes, front coding. Three number forms earn a place for particular columns.

**What surprised me.** The offsets. Brotli sees a rising u32 per row as close to random in its low byte, and they cost 0.75 to 1.8 bytes a row after compression. grantnav has four plain text columns and 1.52M rows, so 6.6 MB of its 58 was offsets.
</sv-prose>

<sv-prose id="fs-method">
## Method

`spikes/size-bench` is a Rust program with no dependencies beyond `facetful-format`. It pipes bytes to the `brotli` and `gzip` commands.

- `size-bench cols <image>` re-encodes each column in every candidate form, laid out row group by row group as a streamed image must be, and compresses each on its own. `summarize.py` tabulates the result.
- `size-bench pack <image>` writes whole packed images with each idea alone and all together, unpacks each, checks the result equals the image byte for byte, and compresses the whole thing as it would be served.
- `wasm/` builds the unpacker alone for wasm, and `unpack-bench.mjs` times it in Node.

Per-column numbers pick out which ideas are worth a whole-image run. Only whole-image numbers count. The two disagree in both directions: GEM units is 7.45 MB as a sum of columns and 6.83 MB whole, because brotli matches the wiki URL against the plant name and one id column against the other.

Three images: grantnav (1.52M rows, 357 MB, 86% text), GEM units (183K rows, 50 MB, 52 columns, mostly dictionaries), and the synthetic million rows of `spikes/facet-spike` (19 MB, a float column, a sequential id, six dictionary columns). The third has no text and stands in for number-heavy data.
</sv-prose>

<sv-prose id="fs-results">
## Results, whole image

Each row is one idea applied alone to the whole image, then compressed. Savings are against the image as served today.

### grantnav

| | brotli | gzip | saved, brotli | saved, gzip |
|---|---|---|---|---|
| image as served today | 58.04 MB | 83.00 MB | | |
| packed, nothing re-encoded (no padding) | 58.16 | 83.00 | +0.12 | 0 |
| text values end in 0 | 51.45 | 74.73 | −6.59 | −8.27 |
| the same, with numbered identifiers | 51.41 | 74.36 | −6.63 | −8.64 |
| dictionary values end in 0 | 58.00 | 82.84 | −0.04 | −0.16 |
| decimals as scaled integers | 58.02 | 82.53 | −0.02 | −0.47 |
| all of the above | 51.18 | 73.72 | −6.87 | −9.28 |
| all, with word codes | **46.87** | **58.18** | **−11.17 (19.2%)** | **−24.82 (29.9%)** |

Split with Description Rare and Identifier second: the first part goes from 27.01 to 21.77 MB brotli and the second from 31.20 to 24.54 MB. The app is usable after 21.8 MB where today it takes 27.0.

### GEM units

| | brotli | gzip | saved, brotli | saved, gzip |
|---|---|---|---|---|
| image as served today | 6.833 MB | 9.351 MB | | |
| packed, nothing re-encoded | 6.839 | 9.351 | +0.006 | 0 |
| text values end in 0 | 6.251 | 8.165 | −0.583 | −1.186 |
| the same, with numbered identifiers | 6.115 | 7.825 | −0.718 | −1.526 |
| dictionary values end in 0 | 6.554 | 8.975 | −0.280 | −0.376 |
| decimals as scaled integers | 6.639 | 8.979 | −0.195 | −0.372 |
| sequences as differences | 6.839 | 9.351 | 0 | 0 |
| all of the above | **5.620** | 7.079 | **−1.214 (17.8%)** | −2.272 |
| all, with word codes | 5.632 | **6.962** | −1.201 (17.6%) | **−2.389 (25.6%)** |

### Synthetic, 1M rows, no text

| | brotli | gzip | saved, brotli | saved, gzip |
|---|---|---|---|---|
| image as served today | 7.492 MB | 9.073 MB | | |
| packed, nothing re-encoded | 7.642 | 9.075 | +0.150 | 0 |
| decimals as scaled integers | 6.771 | 7.955 | −0.721 | −1.118 |
| sequences as differences | 6.351 | 7.673 | −1.141 | −1.400 |
| all | **5.683** | **6.571** | **−1.809 (24.1%)** | **−2.502 (27.6%)** |

The +0.15 MB of the second row is brotli doing a little worse without the 8-byte alignment. The encodings buy it back many times over, so I left it.
</sv-prose>

<sv-prose id="fs-ideas">
## The ideas that pay

### Values end in a 0 byte

A plain text segment stores n + 1 u32 offsets, then the bytes. The packed form writes each value followed by a 0 byte and no offsets. The unpacker finds the zeros and writes the offsets back. Dictionaries get the same treatment.

A text group that holds a 0 byte keeps its stored form, with a tag saying so. None of the three images has one.

Why it beats the alternatives, on grantnav's Title column (10.30 MB brotli as stored):

| form | brotli | saved |
|---|---|---|
| values end in 0 | 8.24 MB | −2.06 |
| a varint length before each value | 8.96 | −1.34 |
| lengths as varints in their own run, then the bytes | 9.06 | −1.25 |
| lengths as u16 in their own run | 9.20 | −1.10 |
| front coding against the previous row | 9.02 | −1.29 |

Brotli predicts the end of a value from the two bytes before it, and it cannot do that for a length held somewhere else.

Description Rare is the extreme case: 877K of its 1.52M rows are empty, and each empty row was four bytes of offset that differ from the last.

### Word codes

A word is a run of ASCII letters, with the space before it when there is one. Per column the packer counts them, keeps the 16,448 that save most, and writes the list once before the row groups, as dictionaries already are. In the text, the best 64 words become one byte (0x80 to 0xBF) and the rest two bytes (0xC0 to 0xFF, then any byte). Bytes of 0x80 and over in the text itself follow a 0x01 escape, so the packer leaves a column alone when over 2% of its bytes are non-ASCII or when it is under 64 KB.

Per column on grantnav, brotli:

| column | stored | ends in 0 | word codes | gzip: stored → word codes |
|---|---|---|---|---|
| Description Rare | 24.47 MB | 23.33 | 19.89 | 37.87 → 25.75 |
| Title | 10.30 | 8.24 | 7.38 | 15.15 → 10.09 |
| Recipient Org:Name | 5.39 | 3.71 | 3.29 | 7.67 → 4.40 |
| Identifier | 6.35 | 4.33 | 4.20 | 8.21 → 5.56 |

Brotli pays for each repeated word as a length and a distance, about two bytes. A code that Huffman coding then shrinks costs about one. I expected brotli's built-in dictionary to have this covered, and it doesn't for text this repetitive.

The split between one and two byte codes matters a little: 64 one-byte codes beat 0, 32 and 120 on Description Rare (19.89 against 20.33, 20.12, 20.52 MB).

On GEM units the word codes do nothing either way for brotli (+12 KB) and save 117 KB for gzip. Its text is plant names in sorted order, where whole values repeat and brotli's long matches already win. So word codes are safe to leave on, and they are where a gzip-only host gains most: on grantnav gzip with word codes (58.2 MB) matches brotli today (58.0 MB).

### Numbered identifiers

When nine values in ten of a text column end in digits, four or more on average, the packer writes the text before the number (ending in 0), the digit count, and the number as its difference from the previous row's. `G100001086205` then `G100001086212` becomes `G`, 12, +7.

On GEM units the two id columns go from 298 and 268 KB to 236 and 222 KB, and the whole image by 135 KB (2%). On grantnav it is 0.04 MB for brotli and 0.37 MB for gzip, because most identifiers there are Salesforce ids and not numbers.

### Decimals as scaled integers

After ALP. Per row group the packer finds the power of ten that turns every value into an integer and back to the same bits, writes the integers as varints, and lists the values that don't fit (row, raw bits). A group with over a quarter exceptions stays stored. The division by a power of ten is exact for integers under 2^53 and powers up to 10^22, which is the same reason decimal parsing has a fast path.

GEM units: 195 KB (3%), from latitude, longitude and capacity. Synthetic: 0.72 MB (10%). grantnav: 20 KB. Amount Awarded saves 94 KB on its own and loses most of that in the whole image.

### Sequences as differences

Brotli finds repeats and has no arithmetic. The synthetic image's `id` column counts up from 0, and it costs 1.6 MB of a 7.5 MB download as stored and 100 bytes as differences. Any row number, serial key or sorted timestamp will do this.

Differences hurt everything else (Award Date gets 258 KB worse), so the packer takes them per row group only when the order-0 entropy of the difference bytes is under half that of the stored bytes. Neither real image has such a column, so this is insurance, and cheap.
</sv-prose>

<sv-prose id="fs-dropped">
## Measured and dropped

Per column, brotli, against the stored form. grantnav unless said.

| idea | result |
|---|---|
| Dictionary codes bit-packed to the dictionary's width | Worse on nearly every column: District +151 KB on 813, Programme +154 KB on 613, units Owner +53 KB on 106. Bit-packing hides the byte repeats brotli lives on. |
| Codes as runs | Within ±40 KB everywhere. Brotli already codes runs. |
| Codes renumbered by frequency | Worse: Description Common +92 KB on 522. |
| u16 codes as two byte planes | Mixed. One real win, units' local plant names, 38 KB. |
| Integers: differences | Worse unless the column is a sequence (above). Award Date +258 KB on 1,363. |
| Integers: frame of reference at a narrower width | Award Date −14 KB, Award Year −29 KB. Under 0.1% of the image. |
| Integers: frame of reference, bit-packed | Award Date +250 KB. |
| Integers and floats: a value table and codes | Award Date −34 KB, Amount −75 KB, units Capacity −39 KB. Small, and it needs a table before the first row group. |
| Floats as eight byte planes | Amount +801 KB on 3,059. Latitude +563 KB on 461. |
| Floats XORed with the previous value | Amount +867 KB. |
| Text: a repeated value as a distance back to its last use | Title −0.42 MB and Recipient −0.21 MB beyond ending in 0, then only −0.18 and −0.08 MB beyond word codes. 0.5% of the image for a hash table in the packer and cross-group lookups in the unpacker. Worth 1.6 MB to gzip, if gzip hosts ever matter that much. |
| Text: 32-bit dictionary codes for Recipient | The same thing with a value number in place of a distance, and worse: 4.05 MB against 3.50 MB. |
| Dictionaries sorted, or sorted and front coded | units' dictionaries: 1.73 MB ending in 0, 1.64 sorted, 1.54 front coded. About 0.1 to 0.2 MB of 5.6, and the codes would have to be renumbered, which changes the image and how its codes compress. Not pursued. |
| Validity bitmaps | units: 16.5 KB after brotli, 0.2%. grantnav has none. |
| Headers, group headers, footer | 1.7 KB on grantnav, 1.3 KB on units. |
| Row group size, column order within a group | Not run. Everything in a group sits within brotli's 16 MB window already, and the sum of grantnav's columns compressed apart (57.7 MB) is within 1% of the whole (58.0 MB). |
| xz, zstd, a stronger compressor inside wasm | Not pursued. The browser's brotli is free, and the earlier zstd run was worse. |
| A brotli shared dictionary | A hosting question, handled elsewhere. |

**Row order** is the one idea here that is not about the wire form. Sorted by funder, programme and date, the packed grantnav is 43.92 MB against 46.84 MB in the same run (6% less), and the stored one 55.21 MB against 58.04 MB (5%, as measured in October). So sorting stacks with packing: 58.0 to 43.9 MB, 24% in all. On GEM units three orders (type, country, name; country, state, type, name; location id) came within about 1% of the source order as stored, and from 0.6% better to 4% worse packed. It needs no format work: `facetful materialize in.facetful "select * from t order by …" out.facetful` already does it. It changes the order rows come back in without ORDER BY, so it is the publisher's choice per image, and worth a line in the publishing notes.

**Columns the browser may not need** dwarf most of the above. Identifier is 4.4 MB of grantnav's 46.9. On GEM units the wiki URL is 0.85 MB of 5.6 and is the plant name with underscores. Those are app decisions.
</sv-prose>

<sv-prose id="fs-cost">
## What it costs

**Decode time.** Unpacking a whole image, best of 5, Ryzen 7 5800H:

| | native | wasm (Node 26) | per MB downloaded |
|---|---|---|---|
| grantnav, everything | 473 ms | 701 ms (509 MB of image a second) | 15 ms |
| grantnav, without word codes | 275 ms | 221 ms | 4 ms |
| GEM units, everything | 44 ms | 66 ms | 12 ms |

At 20 Mbps the download brings 2.5 MB a second, so unpacking takes about 4% of one core on this laptop. I have not measured a phone. Taking an iPhone 15 as about this laptop per core, and a mid-range Android as three to four times slower, that is 4% and 15%, and indexing keeps its place. The unpacker is untuned: word codes copy each word with a variable-length `memcpy`, and fixed 16-byte copies should about halve it.

Some of that comes back from brotli, which has less to produce. Node's brotli decoder takes 751 ms for today's grantnav (58.0 MB to 357 MB) and 542 ms for the packed one (46.9 MB to 167 MB). So on this laptop the packed image costs about 0.5 s more CPU in all, for 11 MB less to download.

At 20 Mbps those 11 MB are 4.5 s. At 400 Mbps they are 0.2 s, less than the extra CPU, so on a very fast link the packed grantnav opens up to 0.3 s later until the word copy is tuned.

**Wasm size.** The unpacker alone is 4.9 KB gz (3.4 KB without word codes and differences), measured as a standalone module against the same module without it. It belongs in the lite build too, so every build opens every image: lite about 244 of 300 KB, full about 384 of 500. (As built it is 7.0 KB: see [What was built](#fs-built).) The packer is another few KB and only goes where images are written.

**Memory.** The image buffer is allocated once from the header's length, as now. The loader also holds the wire bytes of the row group in flight, about 7 MB for grantnav, and the word lists, under 1 MB.

**Packing time.** About 3 s for grantnav, before brotli's several minutes.
</sv-prose>

<sv-prose id="fs-design">
## Proposal: a packed form the loader unpacks

The image stays as it is. Packing is a lossless rewrite for transport, and the unpacker's output is the image, byte for byte. That one property keeps every constraint:

- **In-memory layout and query speed:** unchanged, since the engine never sees packed bytes.
- **Streaming:** the packed form is the image's header, the dictionaries, then row groups, each prefixed with its wire length, then the footer as it is. `TableLoad::push` collects wire bytes until a group is whole, unpacks it onto the end of the image buffer, and then runs the loop it runs today. `open_prefix`, `next_group` and `finish` read image bytes as before, and the search index builds per group as before.
- **Image length in the header:** it is the header of the image, so the length is the unpacked length and the loader allocates once.
- **Split and attach:** `facetful split` works on images. Each part is packed on its own and unpacks to the part it was. Split ids and row counts are in the parts' headers as now.
- **Fingerprint:** computed from the catalog of the unpacked image, so it is the same number, and a saved search index still matches.
- **OPFS holds the unpacked image.** A repeat visit then opens in 3 ms as today, and segments are read from OPFS where they lie. Keeping the packed form would halve grantnav's OPFS use (167 MB against 357 MB) but every visit would unpack for 0.4 s into memory, so I would not. The cost is in the loader: `persist` writes each chunk as it arrives, and for a packed download it must write the bytes the unpacker produced. That is one more export and a few lines in `core.js` and `worker.js`.
- **Old engines refuse it clearly.** The packed form starts with the image's own header with the version set to 2. Every reader checks the version right after the magic and header length, so an engine from before says "unsupported format version 2" on any path. The unpacker writes version 1 into the image it produces.
- **Lite:** the unpacker is in both builds, so there is no image lite cannot open.

**Surface.** `facetful pack in.facetful out.facetful` and `unpack`, in the CLI and the npm command. `loadUrl`, `attach` and `openTable` take either form and tell them apart by the version. Whether `convert` should write the packed form directly can wait.

**What changes hands with the other session.** The loader edit for `persist` touches `streamImage` and `openStream`, where the brotli and gzip publishing work is going on. I would build the format and engine side first and make the loader edit after that work merges.

**Not covered.** A packed file cannot be read by HTTP range. Nothing reads images that way today.

**Hostile input.** The unpacker checks every length against its input, fails when its output would pass the header's image length, and the image it produces then goes through the same checks as any image.
</sv-prose>

<sv-prose id="fs-recommend">
## Recommendation

Build in this order. Each step is usable on its own.

| step | what | grantnav brotli | units brotli | cost |
|---|---|---|---|---|
| 1 | The packed form, with text and dictionary values ending in 0. Version 2, `pack` and `unpack`, the unpacker in `TableLoad`, the `persist` edit. | 58.0 → about 51.4 MB (11%) | 6.83 → about 5.97 MB (13%) | Most of the work. About 2.5 KB gz. |
| 2 | Word codes. | → 46.9 MB (19%) | no change | About 1.5 KB gz in the unpacker, a counting pass in the packer. |
| 3 | Numbered identifiers, decimals, sequences. | no change | → 5.63 MB (18%) | About 1 KB gz. Three small payload forms. |

Expected total: **19% off grantnav, 18% off GEM units, 24% off number-heavy data, for brotli. 26 to 30% for gzip.**

Steps 2 and 3 are payload tags inside the step 1 container, so they can follow later without another version, as long as the step 1 unpacker refuses tags it does not know.

If only one step gets built, it is step 1.

**Decided, 25 November.** David agreed to all three steps in this order, then the size report and the row-order search described under [What was built](#fs-built). `pack` is its own command. The sorted-rows saving became `facetful order`, which reports and leaves the choice to the publisher.
</sv-prose>

<sv-prose id="fs-built">
## What was built

25 November, after David agreed. All of it is on the `file-size` branch, uncommitted.

**The packed form.** `crates/facetful-format/src/packed.rs` is the unpacker and holds the layout in its header comment. `pack.rs` is the packer. All three steps are in: values ending in 0, word codes, and the three number forms. One thing differs from the proposal. After the header the packed form carries the unpacked length (u64) and the length of what comes before the first row group (u32). The unpacker needs the first to bound its output for images from before the header had a length, and the second to know when the dictionaries are whole.

| | as it is | packed | |
|---|---|---|---|
| grantnav, brotli -q 11 | 58,044,008 | 46,878,530 | 19.2% less |
| grantnav, gzip -9 | 83,000,309 | 58,190,273 | 29.9% less |
| GEM units, brotli -q 11 | 6,833,303 | 5,630,206 | 17.6% less |
| GEM units, gzip -9 | 9,351,395 | 6,961,994 | 25.6% less |

These are the built packer's files, compressed with the commands. They match the prototype. Both images come back from `facetful unpack` identical under `cmp`.

**Where it is wired in.**

- `TableLoad::push` reads the first 10 bytes, and for a packed image sends the rest through an `Unpacker` that appends whole row groups to the image buffer. The existing loop reads that buffer as before. `finish` fails if the packed stream stopped early.
- `Table::open` unpacks a packed image held in memory, so `table_open` in wasm and every CLI command take either form.
- The wasm has three new exports for the loader, `load_packed`, `load_image_ptr` and `load_image_len`, which give the unpacked bytes to keep for the next visit.
- An engine from before reads version 2 and says "unsupported format version 2". A test holds the current reader to that.

**Cost, as built.**

| | as it is | packed |
|---|---|---|
| grantnav through `openStream` in wasm (Node 26, 64 KB chunks, best of 3) | 217 ms | 360 ms |
| GEM units, the same | 31 ms | 53 ms |
| lite build, gz | 243.1 KB | 250.1 KB of 300 |
| full build, gz | 385.4 KB | 392.8 KB of 500 |
| the command's build, gz (no page loads it) | | 414.0 KB |

The wasm sizes are without `wasm-opt`, which this machine lacks, so CI's numbers will be a little lower on both sides. Packing grantnav takes 3.5 s, which includes unpacking the result and comparing it with the input. The packer and `split` are in neither build a page loads. They are 21 KB gz, and only the `facetful` command runs them, so they sit behind a `publish` feature that only the command's build (`facetful_cli.wasm`, shipped in the npm package beside `facetful_wasm.wasm`) turns on. A page pays for the unpacker alone, 7 KB. The cost is install size: the package carries two wasm files, 871 KB packed where one was about half that.

**Tests.** In `pack.rs`: every column kind with NULLs and a short last group, under six option sets; the wire cut into chunks of 1, 7 and 4096 bytes with the output checked as a prefix of the image after every push; truncation at every 97th byte; 4,000 single-bit flips, none of which may panic or produce more than the declared length; an empty image. In `tests/load.rs`: a packed image streams in to the same fingerprint and the same query results as the image, split parts pack and attach, and a word index built while a packed image arrives equals the one built over the image.

**Two commands for publishers**, in `crates/facetful-cli/src/size.rs`. Both run the `brotli` or `gzip` command, so they print the sizes a host would send.

`facetful sizes file.facetful` gives the image as it is and packed, then each column's share:

```
units-2026-08.facetful: 183125 rows, 52 columns
  as it is     50.37 MB      6.83 MB after brotli -q 11
  packed       28.98 MB      5.63 MB after brotli -q 11  (17.6% less to download)

  column                          type                 in image      packed    download  share
  GEM.Wiki URL                    text                 10.41 MB     4.78 MB     0.88 MB  15.1%
  Plant / Project name            text                  6.57 MB     2.42 MB     0.82 MB  14.2%
  Plant / Project name (local)    text, dictionary      2.96 MB     2.80 MB     0.58 MB   9.9%
  Longitude                       float64               1.47 MB     0.71 MB     0.38 MB   6.5%
  …
```

`facetful order file.facetful` searches for a row order that compresses better. It tries every dictionary, integer, date and bool column as a sort key, keeps the best, tries each remaining column as a second key under it, and stops when a key gains under half a percent or at `--depth` (3). It searches at brotli quality 5 and `--confirm` reruns the winner at 11.

| | as it is | best found | |
|---|---|---|---|
| GEM units (q5) | 6.84 MB | 6.82 MB | under half a percent: "leave it" |
| grantnav (q5) | 55.60 MB | 52.70 MB | 5.2% less: programme, award date, county |
| grantnav (q11) | 46.87 MB | 44.61 MB | 4.8% less, the same order |

The order I picked by hand earlier (funder, programme, date) gave 6% at quality 11, which is 43.92 MB against the search's 44.61. So the greedy search finds most of the gain and not the best order: a first key that looks second best alone (funder) can lead somewhere better. Trying the best two or three at each level would cost that many times the run and is not built. It prints the `materialize` command that writes the sorted image, and a reminder that row order is also the order rows come back in without ORDER BY, the order a page sees while the image loads, and what lets a filter on the sort key skip row groups.

**In a browser.** `bench/search-browsers` has both datasets in both forms (`?dataset=units-packed`, `grantnav-packed`). Headless Chromium on this laptop, brotli 9 on the paced profiles (the benchmark's own setting; grantnav is 67.6 MB plain and 52.4 MB packed at that quality), search index built while loading, image written to OPFS as it arrives:

| table ready | as it is | packed |
|---|---|---|
| units, local (no network) | 257 ms | 288 ms |
| units, 100 Mbit/s | 759 ms | 626 ms |
| units, 20 Mbit/s | 3.52 s | 2.79 s |
| grantnav, local | 2.32 s | 2.41 s |
| grantnav, 100 Mbit/s | 5.68 s | 4.35 s |
| grantnav, 20 Mbit/s | 27.3 s | 21.0 s |

With no network the packed image costs 30 ms more on units and 80 ms on grantnav. On a paced connection it is ready sooner by about the bytes saved: 21 to 23% at these speeds. Every run wrote OPFS, and the repeat visit opened the kept copy in 3.2 to 4.7 ms with the index read back, the same as for a plain image: OPFS holds the unpacked image. Typing times are the same in both forms. Wasm memory after loading is 12 MB higher for packed grantnav (544 against 532 MB) and 20 MB for units (100 against 80).

`js/facetful/browser-smoke.mjs` also loads a packed image with `persist` and a search index, compares the OPFS file with the plain image byte for byte, and attaches packed split parts.

**The npm command** has `pack`, `unpack`, `sizes` and `order` (`js/facetful/bin/size.mjs`), and `inspect`, `query` and `materialize` take a packed file. It packs to the same bytes as the Rust CLI on both datasets. Sizes come from Node's zlib, so they differ slightly from the `brotli` and `gzip` commands (grantnav packed at brotli 5: 55.22 MB against 55.60). `order` on grantnav takes 260 s and picks the same order as the Rust CLI. It also has `split`, which was in the Rust CLI only, so that the first-file pattern below can be built from npm.


**Against hand-built formats: the agent experiment.** `bench/agent-ab` in the main checkout (11 and 12 November) gave agents the same dashboard brief over GEM units with and without facetful, and measured the pages in a throttled browser. In its last round, with all 52 columns of every unit in the browser, every run got every step right and clicks were as fast either way. Facetful lost on size only:

| arm | total download, gzip | later clicks, median |
|---|---|---|
| no guidance: a format of the agent's own | 5.7, 6.3, 7.1 MB | 95 to 168 ms |
| "static files only": the same | 6.0, 6.3, 7.9 MB | 35 to 132 ms |
| facetful | 5.7, 8.3, 9.3 MB | 75 to 116 ms |
| DuckDB-WASM | 10.9 to 23.3 MB | 72 to 225 ms |

The 5.7 MB facetful run kept facetful for the filter columns and wrote its own row file for the table. Where facetful stands on the same data now, gzip as the harness serves it:

| | data | with the engine |
|---|---|---|
| as tested | 9.35 MB | about 9.7 MB |
| packed | 6.96 MB | about 7.4 MB |
| packed, without the wiki URL column | 5.93 MB | about 6.3 MB |

So packing closes most of the gap and one column is the rest. What the hand-built formats did that the packer does not:

- **A column derived from another.** Three agents separately stored `GEM.Wiki URL` as "the plant name with underscores" and kept only the exceptions (about 12% of plants). It is 1.03 MB of the 6.96, the largest single column. A general encoding, "a prefix plus another column with a substitution, else stored", would cover it. Not built.
- **Values stored once per plant.** Ten columns never vary within a plant (145,396 plants, 183,125 units). Ordering the rows by location saved 0.12 MB with gzip, which is not a full test of per-plant storage.
- **A first file with only the filter columns**, 240 to 420 KB, so filters worked after about half a megabyte.

That last point is also the experiment's start-up cost. In its first round a facetful page took 1.2 to 2.0 s to answer the first click where hand-built pages took 0.3 to 1.0 s. The run labelled unthrottled still paced the network at 20 Mbit/s, and the difference is download: 3.03 MB (image and engine) at 20 Mbit/s is 1.21 s, and the measured first click was 1.20 s. The engine's own start is about 100 ms (131 ms against 31 ms on the repeat visit). A table answers nothing until its file has fully arrived, because an image is laid out by row group. `facetful split` is the answer, and the guide the agents were given did not mention it: a first part with the five filter columns, capacity and start year is 449 KB gzipped (389 KB packed) against a 2.7 MB image. That page has not been built and run through the harness. The pattern is now in `js/facetful/README.md`.

Two other things the agents worked around: `LIKE` treats a typed `%` or `_` as a wildcard and has no escape, and it does not match "İ" to "i". And nearly every agent turned capacity into whole kW to sum without float drift.

**Found on the way: `search()` before the index is finished.** A table streamed in faster than its index is built (`searchReady: false`) finds no rows with `search()` until the index is done, where the comments say it scans meanwhile. The engine from before this branch does the same with a plain image, so it is not from packing. Not fixed here; `index.d.ts` now says to await `searchReady()`.

**Not done, and why.**

- **A phone.** The browser runs are a laptop. Firefox was not run on the packed datasets.
- **More speed in the unpacker.** Word codes were 0.45 s of a 0.52 s native unpack of grantnav. Copying words and plain runs as a fixed 16 or 8 bytes, where the buffers have room, halved the whole unpack to 0.27 s natively, and took the wasm load from 729 ms to 360 ms (units 78 to 53 ms) for 130 bytes of wasm. What is left: word codes are still about 0.2 s of the 0.27 s, and every output byte is zeroed before it is written.

**Things asked about along the way, and measured.**

*Combining numeric columns into one.* Writing unrelated columns row by row makes them larger: units' three year columns 104 to 117 KB, capacity with latitude, longitude and start year 1,227 to 1,425 KB, grantnav's amount with award date 4.45 to 4.79 MB. Latitude with longitude is unchanged (935 and 937 KB). It pays only where one column is a function of another: grantnav's Award Year is `year("Award Date")` on every row and Amount Band is a step function of Amount Awarded. Stored as now they cost 0.49 and 0.43 MB; a payload that sends a lookup from one column's distinct values to the other's could take most of that 0.9 MB, 2% of the packed image. Not built.

*Sorting one column on its own and storing where each row went.* For distinct values the row index is a permutation, which needs 14.6 bits a row for a 65,536-row group however it is coded, against the 0.9 bytes a row Award Date costs now. With repeated values it becomes a table of distinct values and an index per row, which the `cols` runs had measured (`valdict`): grantnav Amount 3.06 to 3.04 MB, Award Date 1.36 to 1.33 MB, units Capacity 190 to 152 KB, and worse for Latitude (461 to 608 KB) and the synthetic id (1.63 to 2.22 MB). Not worth a payload form.

*What else a size optimizer could do.* Choose each column's encoding by compressing both ways in place of the packer's rules, and tune for gzip hosts, where word codes matter most. Report float precision (latitude and longitude are 13% of units' download) and columns whose type looks wrong. None of it is built; each is worth 1 to 2% at most except precision, which is lossy and the publisher's call.
</sv-prose>

</sv-page>
