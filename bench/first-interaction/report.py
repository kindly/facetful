"""Render the September 24 report; narrative is specific to this measured snapshot."""
import csv,json,pathlib
out=pathlib.Path('bench/reports/2026-09-24-first-interaction')
ss=json.loads((out/'summary.json').read_text());env=json.loads((out/'measure-environment.json').read_text());checks=json.loads((out/'correctness.json').read_text())
s={(r['profile'],r['lane'],r['cache']):r for r in ss}
lanes=['facet_image','facet_image_br','duck_range','duck_full','facet_parquet','facet_parquet_zstd','native']
labels={'facet_image':'Facetful WASM, prepared image','facet_image_br':'Facetful WASM, prepared image (brotli)','facet_parquet':'Facetful WASM, Parquet conversion','facet_parquet_zstd':'Facetful WASM, ZSTD Parquet conversion','duck_range':'DuckDB WASM, Parquet ranges','duck_full':'DuckDB WASM, full Parquet/table','native':'DuckDB native, preloaded server'}
main=[];sensitivity=[]
for lane in lanes:
 c=s['representative_4g',lane,'cold'];w=s['representative_4g',lane,'repeat']
 main.append({'Engine / loading strategy':labels[lane],'Fresh visit (s)':f"{c['first_interaction_s']:.2f}",'Repeat visit (s)':f"{w['first_interaction_s']:.2f}",'First filter click (ms)':f"{c['click_ms']:.0f}",'Fresh HTTP body (MB)':f"{c['body_MB']:.3f}",'Fresh range (s)':f"{c['min_s']:.3f}–{c['max_s']:.3f}"})
 sensitivity.append({'Engine / loading strategy':labels[lane],**{p:f"{s[p,lane,'cold']['first_interaction_s']:.2f}" for p in ['representative_4g','slower_4g','slow_4g']}})
for name,rows in [('comparison.csv',main),('sensitivity.csv',sensitivity)]:
 with (out/name).open('w') as f:w=csv.DictWriter(f,fieldnames=rows[0]);w.writeheader();w.writerows(rows)
def table(rows):
 keys=list(rows[0]);return '\n'.join(['| '+' | '.join(keys)+' |','|'+'|'.join(['---']*len(keys))+'|']+['| '+' | '.join(str(r[k]) for k in keys)+' |' for r in rows])
report=f'''# Navigation to first completed filter over 4G

**Facetful WASM with a prepared image reaches the first completed interaction in 4.11 seconds; DuckDB WASM with partial Parquet reads takes 6.29 seconds.** Facetful's Parquet-conversion path takes 17.07 seconds on a fresh visit. Data preparation and transfer strategy matter more here than resident query speed.

Measured 24 September 2026. Dataset: **183,125 rows, 52 columns**, `data/units-2026-08`. The page renders Status, Type, Region, Country/area and Technology facets, automatically clicks **Operating plants** once the initial dashboard is rendered, and waits for all updated facet counts to render. The Status facet ignores its own filter; the other four facets describe **127,839 operating rows**. All five queries execute sequentially in each engine.

## Representative 4G assumption: 20 Mbps, 60 ms per request

Medians of three fresh-profile rounds and three repeat navigations per lane. Seconds from navigation through the completed first filter interaction. The click column isolates the interaction after the initial dashboard is ready, including results, DOM updates and the rendering wait.

{table(main)}

Facetful's prepared-image path is **about 1.5× faster** than DuckDB's best fresh browser path here. On repeat visits it is **about 5.9× faster** than the range-read path. These are results for this dataset, browser, loading strategy and machine, rather than universal engine ratios.

Native DuckDB is shown as a **server architecture reference**. Its engine and table are loaded before timing; the browser only downloads the page and two small API responses. It does not pay client-side engine/data transfer. Server startup, import, hosting and deployment costs are excluded.

## Where the time goes

- The prepared Facetful image is **50.37 MB uncompressed / 9.32 MB gzip**. The engine is **0.255 MB gzip**. Together with the small application and worker scripts, the first visit transfers **9.58 MB**. At 20 Mbps, the image alone has a roughly **3.73-second transmission floor**. The measured table-open step is only about **9 ms** once bytes have arrived.
- DuckDB's WASM engine is **8.06 MB gzip**, plus **0.736 MB** for the Parquet extension and about **0.242 MB** for its client/worker scripts. The range-read lane transfers only **0.959 MB of Parquet data**, from the same **13.94 MB SNAPPY Parquet** file. Its total payload is therefore only modestly larger than Facetful's, but engine startup, extension loading and sequential range requests sit on the startup path.
- The first DuckDB range-based filter causes **three additional 16 KiB range reads**. At 60 ms per response these contribute about 180 ms of delay; the full click takes about **250 ms**. Facetful's first filter takes about **10 ms through the query/result boundary**, or **33 ms including the rendering wait**.
- Facetful's cold `openParquet` path spends about **11.1 seconds** opening/converting the full 52-column source after download. Its repeat visit reopens the compiled OPFS image and avoids conversion. The first-visit result includes conversion and persistence costs.
- DuckDB's full-download lane materializes all columns into a table, which speeds later local filters but increases initial transfer to **22.98 MB** including engine and extension. It is not the best first-interaction strategy for this dashboard.

The most promising startup optimization outside multithreading is **reducing the data required before the first dashboard can respond**: serve a prepared image, build an initial image containing only needed columns, or support remote/partial loading. Saving half of a 10 ms local filter would save around 5 ms; reducing a 9.32 MB payload has a seconds-scale effect. These are optimization directions, not measured results for an implementation that does not yet exist.

## What “warm” means here

A fresh visit uses a new Chromium process and empty profile, including empty HTTP, OPFS and compilation caches. A repeat visit navigates again in that same browser/profile and creates a new worker/database. Static resources can come from HTTP cache; Facetful's Parquet lane can reuse its OPFS conversion. **A repeat visit does not preserve in-memory predicate masks.** The first Status predicate starts cold, and subsequent facet queries in that interaction may reuse it.

The earlier [resident-data assessment](../2026-09-21-dashboard-wasm/report.md) isolates cold/warm predicate caching; its numbers and source revision should not be confused with these navigation timings.

**Cache fairness control:** the initial server supplied long cache lifetimes but lacked ETag/Last-Modified validators. Chromium did not reuse partial Parquet responses: DuckDB's repeat visit took 2.39 s and fetched 0.959 MB again. Adding strong validators reduced it to **1.06 s with zero response-body bytes transferred**. The headline table uses those corrected DuckDB range results. Other client lanes already transferred zero body bytes on repeat visits. Original and control runs are both retained; each summary row names its source file. The final benchmark server includes validators.

## Slower-network sensitivity

Fresh visits. The 20 Mbps column has three rounds; the two slower profiles have **one run each**, so they show scale rather than a latency distribution.

{table(sensitivity)}

Profiles: `representative_4g` = **20 Mbps / 60 ms**; `slower_4g` = **5 Mbps / 100 ms**; `slow_4g` = **1.6 Mbps / 150 ms**.

20 Mbps / 60 ms is an explicit representative assumption, not a universal measured 4G average. For context, [Ofcom's 2025 research](https://www.ofcom.org.uk/phones-and-broadband/mobile-phones/mobile-matters-download-speeds-connection-rates-and-latency-levels-revealed) reports an average 0.7-second download for a 2 MB file over 4G. [Lighthouse's documented slow-4G profile](https://github.com/GoogleChrome/lighthouse/blob/main/docs/throttling.md) uses 1.6 Mbps and 150 ms; this benchmark uses those throughput/delay values for sensitivity, without claiming to reproduce Lighthouse's full simulation.

## Method, versions and limits

- Real headless **Chromium 153.0.8010.52**, Linux, **{env['cpu']}**, {env['logicalCpus']} logical CPUs, about {env['memoryBytes']/2**30:.1f} GiB RAM. **CPU is not throttled to imitate a phone.** These are laptop CPU results over a synthetic 4G network model.
- Facetful package **{env['assets']['facetfulPackage']}**, prepared source snapshot **`{env['assets']['sourceCommit'][:7]}`**. The exact bundled WASM SHA-256 is `{env['assets']['files']['facetful.wasm']['sha256']}`. Assets were frozen before the measured series; subsequent repository changes are excluded.
- DuckDB WASM npm **{env['assets']['duckdbPackage']}**, engine **1.5.4**, EH bundle, one execution thread. Native DuckDB **1.5.5**, 16 configured threads, persistent preloaded process. Native request timings include CLI/result-file plumbing.
- All engines use the same page, SQL and rendering logic. The UI shows the ten most frequent categories per facet, but every category/count is returned and compared. No joins, pivots, substring search, or general dashboard chart library is timed in this particular startup scenario.
- The server shares one throughput budget across **all** HTML, JS, worker, WASM, extension, data and API responses, and delays each response by the profile's latency. It does not simulate DNS, TLS, TCP slow start, loss, radio variation or upload throughput. Results are application-level simulations, not measurements on a cellular modem.
- JavaScript is bundled/minified. JS, WASM, the compiled image and API responses use gzip level 9. Parquet stays SNAPPY-compressed with identity HTTP encoding for valid byte-range offsets. Brotli/CDN delivery and differently compressed/partitioned datasets may change results.
- Rendering completion uses two requestAnimationFrame callbacks after updating the DOM, a browser rendering proxy rather than a physical display timestamp. No human think time. Browser-process launch precedes navigation and is excluded.
- “Transferred MB” and raw `wire_bytes` mean **encoded response-body bytes**, excluding HTTP headers and transport overhead. Repeat zero-byte results still involve disk/OPFS reads, decompression, engine creation and execution.
- Three main rounds are sufficient to identify the large differences observed here, not to establish p95/p99 behavior or small performance differences.

**Validation:** all **{checks['facet_result_sets']} facet result sets across {checks['recorded_visits']} recorded visits** match, including the original cache-policy runs and corrected controls. The headline/sensitivity summaries select **{checks['selected_visits']} visits**. Checks verify every category/count, initial totals of 183,125, filtered totals of 127,839, actual DuckDB partial reads, and Facetful transcode versus OPFS-cache paths.

Reproduction and harness details: [bench/first-interaction/README.md](../../first-interaction/README.md). Raw artifacts: [main runs](measure-results.json), [sensitivity](sensitivity/measure-results.json), [cache controls](cache-validators/measure-results.json), [cache-control sensitivity](cache-validators-sensitivity/measure-results.json), [environment and asset hashes](measure-environment.json), [validated summary](summary.csv), [correctness checks](correctness.json). No engine source changes were made for this benchmark.
'''
(out/'report.md').write_text(report)
page='''<sv-prose id="overview">
# Time to first interaction over 4G

**Facetful WASM: 4.11 seconds fresh / 0.18 seconds repeat. DuckDB WASM with partial reads: 6.29 seconds fresh / 1.06 seconds repeat.**

183,125 rows. Open the page, render five facets, click **Operating plants**, render updated counts. Representative assumption: **20 Mbps / 60 ms per request**, with the laptop CPU unthrottled. Three main rounds; complete result equality checks.

Serving a prepared Facetful image is the strongest client path tested. Browser-side Parquet conversion adds about 11 seconds on the first visit. The full report includes cache controls, versions, timing breakdowns and limitations.
</sv-prose>
<sv-csv id="main" src="bench/reports/2026-09-24-first-interaction/comparison.csv" freeze="1" height="19rem">
</sv-csv>
<sv-prose id="interpretation">
## What explains the result

Facetful transfers **9.58 MB**, dominated by its **9.32 MB gzip dataset**; the engine itself is only **0.255 MB**. DuckDB transfers **10.00 MB**, dominated by engine/extension startup; only **0.959 MB** is Parquet data. Sequential range requests also add latency, including three more reads for the first filter.

A repeat navigation creates a new engine. It can reuse HTTP/OPFS/compilation caches; it does **not** retain predicate masks. Adding proper HTTP validators improves DuckDB's repeat time from **2.39 s to 1.06 s**, and the table uses that improved result.

Native DuckDB is a preloaded server architecture: it sends just facet results and excludes server loading/deployment costs.

## Network sensitivity

Fresh visits. Three rounds at 20 Mbps; one run each at 5 Mbps / 100 ms and 1.6 Mbps / 150 ms. These are modeled networks, not mobile-CPU tests or measurements on a cellular modem.
</sv-prose>
<sv-csv id="sensitivity" src="bench/reports/2026-09-24-first-interaction/sensitivity.csv" freeze="1" height="18rem">
</sv-csv>
<sv-prose id="artifacts">
## Reproduction and evidence

`bench/reports/2026-09-24-first-interaction/report.md` contains the full report. `bench/first-interaction/` contains the runnable harness. Raw JSON retains every result bucket, navigation mark and HTTP request; asset hashes pin the measured build. Engine source is unchanged by this work.
</sv-prose>
'''
(out/'assessment.sv').write_text(page)
print(out/'report.md')
