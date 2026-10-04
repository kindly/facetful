const lane=new URLSearchParams(location.search).get('lane');
const dims=['Status','Type','Region','Country/area','Technology'];
const marks={navigation:0,app:performance.now()};
const mark=n=>marks[n]=performance.now();
const paint=()=>new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r)));
window.bench={lane,marks,ready:false,done:false,error:null};
let db,conn,batch;
function render(results){
 document.querySelector('#facets').replaceChildren(...results.map((rows,i)=>{
  const s=document.createElement('section');const h=document.createElement('h2');h.textContent=dims[i];s.append(h);
  const ul=document.createElement('ul');for(const row of [...rows].sort((a,b)=>b.n-a.n).slice(0,10)){const li=document.createElement('li');li.textContent=`${row.category||'(empty)'}: ${row.n.toLocaleString()}`;ul.append(li)}s.append(ul);return s;
 }));
}
const sqlFor=(dim,filtered)=>`select "${dim}" as category,count(*) as n from t ${filtered&&dim!=='Status'?"where \"Status\"='operating'":''} group by "${dim}"`;
const fetchData=async path=>{mark('dataFetchStart');const r=await fetch(path);if(!r.ok)throw Error('data '+r.status);const b=await r.arrayBuffer();mark('dataFetchEnd');return b};
const IMAGE_PATH='bench/units.facetful';
const opfsHas=async p=>{try{const [d,f]=p.split('/');await (await (await navigator.storage.getDirectory()).getDirectoryHandle(d)).getFileHandle(f);return true}catch{return false}};
async function main(){
 mark('setupStart');
 if(lane.startsWith('facet')){
  // Image lanes keep the image in OPFS like openParquet keeps its compiled
  // image: the first visit stores it before the dashboard is ready, a repeat
  // visit finds it and opens it without fetching.
  const image=lane.startsWith('facet_image'),cached=image&&await opfsHas(IMAGE_PATH);
  const data=cached?null:fetchData(lane==='facet_image'?'/data/units.facetful':lane==='facet_image_br'?'/data/units.facetful?enc=br':lane==='facet_parquet_zstd'?'/data/units-zstd.parquet':'/data/units.parquet');
  const {Facetful}=await import('/assets/facet-entry.js');mark('clientImported');
  db=await Facetful.open({wasmUrl:'/assets/facetful.wasm',workerUrl:'/assets/facet-worker.js',hyparquetUrl:'/assets/hyparquet.js'});mark('engineReady');
  const b=await data;mark('tableOpenStart');
  let info;
  if(cached)info={...await db.loadOpfs('t',IMAGE_PATH),source:'cache'};
  else if(image){info={...await db.load('t',b.slice(0)),source:'network'};await db.storeOpfs(IMAGE_PATH,b)}
  else info=await db.openParquet('t',b);
  window.bench.loadInfo=info;mark('tableReady');
  batch=async filtered=>{const out=[];for(const d of dims){const r=await db.query(sqlFor(d,filtered));out.push([...r.rows()])}return out};
 }else if(lane.startsWith('duck')){
  const data=lane==='duck_full'?fetchData('/data/units.parquet'):null;
  const duck=await import('/assets/duck-entry.js');mark('clientImported');
  db=new duck.AsyncDuckDB(new duck.VoidLogger(),new Worker('/assets/duck-worker.js'));
  await db.instantiate('/assets/duckdb.wasm');await db.open({filesystem:{reliableHeadRequests:true,allowFullHTTPReads:true,forceFullHTTPReads:false}});conn=await db.connect();await conn.query('set threads=1');mark('engineReady');mark('tableOpenStart');
  await conn.query(`set custom_extension_repository='${location.origin}'`);mark('extensionLoadStart');await conn.query('load parquet');mark('extensionReady');
  if(data){await db.registerFileBuffer('units.parquet',new Uint8Array(await data));mark('fileRegistered');await conn.query("create table t as from 'units.parquet'")}
  else{await db.registerFileURL('units.parquet',location.origin+'/data/units.parquet',duck.DuckDBDataProtocol.HTTP,false);mark('fileRegistered');await conn.query("create view t as from 'units.parquet'")}
  mark('tableReady');
  batch=async filtered=>{const out=[];for(const d of dims){const r=await conn.query(sqlFor(d,filtered));out.push(r.toArray().map(x=>({category:x.category,n:Number(x.n)})))}return out};
 }else{
  mark('engineReady');mark('tableReady');
  batch=async filtered=>{const r=await fetch('/api/facets',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({filtered})});const v=await r.json();if(v.error)throw Error(v.error);(window.bench.serverQueries??=[]).push(v.query_ms);return v.results};
 }
 mark('initialQueryStart');window.bench.initial=await batch(false);mark('initialQueryEnd');render(window.bench.initial);document.querySelector('#state').textContent='Ready';
 const button=document.querySelector('#filter');button.disabled=false;
 button.onclick=async()=>{try{mark('click');button.disabled=true;window.bench.filtered=await batch(true);mark('filterQueryEnd');render(window.bench.filtered);document.querySelector('#state').textContent='Operating filter applied';await paint();mark('filterPainted');window.bench.done=true}catch(e){window.bench.error=String(e.stack||e)}};
 await paint();mark('readyPainted');window.bench.ready=true;setTimeout(()=>button.click(),0);
}
main().catch(e=>{window.bench.error=String(e.stack||e);document.querySelector('#state').textContent=window.bench.error});
