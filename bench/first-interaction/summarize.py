"""Validate every facet bucket and summarize measured navigation/interaction timings."""
import collections,csv,json,pathlib,statistics,sys
out=pathlib.Path(sys.argv[1] if len(sys.argv)>1 else 'bench/reports/2026-09-24-first-interaction')
files=[out/'measure-results.json',out/'sensitivity/measure-results.json',out/'cache-validators/measure-results.json',out/'cache-validators-sensitivity/measure-results.json']
rows=[]
for p in files:
 if not p.exists():continue
 started=json.loads(p.with_name('measure-environment.json').read_text())['startedAt']
 for r in json.loads(p.read_text()):
  r['_source']=str(p.relative_to(out));r['_run_started']=started;rows.append(r)
assert rows and all(r.get('done') and not r.get('error') for r in rows), 'Incomplete/error visit'
reference=next(r for r in rows if r['lane']=='native')
def canonical(facets):
 assert len(facets)==5
 result=[]
 for facet in facets:
  d={r['category']:r['n'] for r in facet}
  assert len(d)==len(facet), 'Duplicate bucket'
  assert all(isinstance(n,int) and n>0 for n in d.values())
  result.append(d)
 return result
expected={state:canonical(reference[state]) for state in ['initial','filtered']}
assert all(sum(f.values())==183125 for f in expected['initial'])
operating=expected['initial'][0]['operating']
assert sum(expected['filtered'][0].values())==183125
assert all(sum(f.values())==operating for f in expected['filtered'][1:])
for r in rows:
 for state,want in expected.items():
  assert canonical(r[state])==want,(r['profile']['name'],r['round'],r['lane'],r['cache'],state)
 m=r['marks']
 assert m['readyPainted']<=m['click']<=m['filterQueryEnd']<=m['filterPainted']
 assert r['wire_bytes']==sum(q['bytes'] for q in r['requests'])
 if r['lane']=='duck_range' and r['cache']=='cold':
  parquet=[q for q in r['requests'] if q['path'].endswith('units.parquet') and q['method']=='GET']
  assert parquet and all(q['status']==206 for q in parquet), 'DuckDB range lane did not use partial reads'
  assert sum(q['bytes'] for q in parquet)<13935086
 if r['lane'] in ('facet_image','facet_image_br'):
  assert r['loadInfo']['source']==('network' if r['cache']=='cold' else 'cache'), 'Unexpected OPFS image mode'
 if r['lane'] in ('facet_parquet','facet_parquet_zstd'):
  assert r['loadInfo']['source']==('transcode' if r['cache']=='cold' else 'cache'), 'Unexpected OPFS cache mode'
checks={'recorded_visits':len(rows),'facet_result_sets':len(rows)*10,'rows':183125,'operating_rows':operating,'all_buckets_equal':True}
labels={'facet_image':'Facetful WASM: prepared image','facet_image_br':'Facetful WASM: prepared image, brotli','facet_parquet':'Facetful WASM: full Parquet','facet_parquet_zstd':'Facetful WASM: full Parquet, ZSTD','duck_range':'DuckDB WASM: Parquet ranges','duck_full':'DuckDB WASM: full Parquet/table','native':'DuckDB native: warm server'}
groups=collections.defaultdict(list)
for r in rows:
 key=(r['profile']['name'],r['lane'],r['cache'])
 # A later control run supersedes that engine/profile's earlier run in the
 # headline table. Keep all samples above for independent correctness checks.
 if groups[key] and r['_run_started']>groups[key][0]['_run_started']:groups[key]=[]
 if not groups[key] or r['_run_started']==groups[key][0]['_run_started']:groups[key].append(r)
checks['selected_visits']=sum(len(rs) for rs in groups.values())
(out/'correctness.json').write_text(json.dumps(checks,indent=2)+'\n')
summary=[]
for (profile,lane,cache),rs in groups.items():
 med=lambda fn:statistics.median(fn(r) for r in rs)
 s={'profile':profile,'engine':labels[lane],'lane':lane,'cache':cache,'samples':len(rs),'first_interaction_s':round(med(lambda r:r['first_interaction_ms'])/1000,4),'min_s':round(min(r['first_interaction_ms'] for r in rs)/1000,4),'max_s':round(max(r['first_interaction_ms'] for r in rs)/1000,4),'ready_s':round(med(lambda r:r['ready_ms'])/1000,4),'click_ms':round(med(lambda r:r['click_to_result_ms']),2),'query_ms':round(med(lambda r:r['marks']['filterQueryEnd']-r['marks']['click']),2),'body_MB':round(med(lambda r:r['wire_bytes'])/1e6,5),'data_MB':round(med(lambda r:sum(q['bytes'] for q in r['requests'] if q['path'].startswith('/data/')))/1e6,5),'table_open_ms':round(med(lambda r:r['marks']['tableReady']-r['marks'].get('tableOpenStart',r['marks']['tableReady'])),2)}
 s['source']=rs[0]['_source'];summary.append(s)
with (out/'summary.csv').open('w') as f:
 w=csv.DictWriter(f,fieldnames=summary[0]);w.writeheader();w.writerows(summary)
(out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(checks));print('Profile, engine, visit, first interaction seconds, click milliseconds, body MB')
for s in summary:print(s['profile'],s['lane'],s['cache'],s['first_interaction_s'],s['click_ms'],s['body_MB'])
