import os,sys,json,pty,select,subprocess,time,pathlib
base=pathlib.Path('/tmp/browserdb-first-interaction');master,slave=pty.openpty()
p=subprocess.Popen(['duckdb',':memory:'],stdin=subprocess.PIPE,stdout=slave,stderr=(base/'native.stderr').open('w'),text=True);os.close(slave)
seq=0
def send(sql):
 global seq
 seq+=1;marker=f'DONE_{seq}';p.stdin.write(sql+'\n.print '+marker+'\n');p.stdin.flush();buf=b'';end=time.monotonic()+60
 while marker.encode() not in buf:
  if time.monotonic()>end:raise RuntimeError('native timeout')
  ready,_,_=select.select([master],[],[],1)
  if ready:buf+=os.read(master,65536)
  if p.poll() is not None:raise RuntimeError('native exited')
send(".bail on\n.mode json\nSET threads=16;\nCREATE TABLE t AS FROM '/tmp/browserdb-first-interaction/assets/units.parquet';")
print(json.dumps({'ready':True}),flush=True)
try:
 for line in sys.stdin:
  req=json.loads(line);start=time.perf_counter();results=[];sql=[];paths=[]
  for i,dim in enumerate(['Status','Type','Region','Country/area','Technology']):
   filt='where "Status"=\'operating\'' if req['filtered'] and dim!='Status' else ''
   path=base/f'native-{i}.json';paths.append(path)
   sql.append(f'.once {path}\nselect "{dim}" as category,count(*) as n from t {filt} group by "{dim}";')
  send('\n'.join(sql));results=[json.loads(path.read_text()) for path in paths]
  print(json.dumps({'id':req['id'],'query_ms':(time.perf_counter()-start)*1000,'results':results}),flush=True)
finally:
 p.stdin.write('.quit\n');p.stdin.flush();p.wait()
