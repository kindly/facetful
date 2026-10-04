import fs from 'node:fs';import path from 'node:path';import os from 'node:os';import {spawn} from 'node:child_process';import {startServer} from './server.mjs';
const mode=process.argv[2]||'smoke';
const output=path.resolve(process.env.OUT||'bench/reports/2026-09-24-first-interaction');fs.mkdirSync(output,{recursive:true});
fs.writeFileSync(path.join(output,mode+'-environment.json'),JSON.stringify({startedAt:new Date().toISOString(),cpu:os.cpus()[0].model,logicalCpus:os.cpus().length,memoryBytes:os.totalmem(),platform:os.platform(),release:os.release(),node:process.version,assets:JSON.parse(fs.readFileSync('/tmp/browserdb-first-interaction/assets/manifest.json'))},null,2));
const pause=ms=>new Promise(r=>setTimeout(r,ms));
class CDP{
 constructor(url){this.id=0;this.pending=new Map();this.ws=new WebSocket(url);this.opened=new Promise((r,j)=>{this.ws.onopen=r;this.ws.onerror=j});this.ws.onmessage=e=>{const m=JSON.parse(e.data);if(m.id){const p=this.pending.get(m.id);if(!p)return;this.pending.delete(m.id);if(m.error)p.reject(Error(JSON.stringify(m.error)));else p.resolve(m.result)}}}
 async send(method,params={},sessionId){await this.opened;return new Promise((resolve,reject)=>{const id=++this.id;const timer=setTimeout(()=>{this.pending.delete(id);reject(Error('CDP timeout '+method))},20000);this.pending.set(id,{resolve:v=>{clearTimeout(timer);resolve(v)},reject:e=>{clearTimeout(timer);reject(e)}});this.ws.send(JSON.stringify({id,method,params,sessionId}))})}
}
async function chrome(){
 const profile=fs.mkdtempSync('/tmp/browserdb-tti-chrome-');
 const child=spawn('/usr/bin/chromium',['--headless=new','--no-sandbox','--disable-dev-shm-usage','--disable-background-networking','--no-proxy-server','--no-first-run','--no-default-browser-check','--disable-component-update','--disable-sync','--disable-background-timer-throttling','--disable-renderer-backgrounding','--remote-debugging-port=0',`--user-data-dir=${profile}`,'--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE localhost, EXCLUDE 127.0.0.1','about:blank'],{stdio:['ignore','ignore','pipe']});
 let stderr='';const url=await new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error('Chromium start timeout '+stderr)),20000);child.stderr.on('data',b=>{stderr+=b;const m=/DevTools listening on (ws:\/\/[^\s]+)/.exec(stderr);if(m){clearTimeout(timer);resolve(m[1])}});child.once('error',reject);child.once('exit',code=>{if(!/DevTools listening/.test(stderr)){clearTimeout(timer);reject(Error('Chromium exit '+code+' '+stderr))}})});
 const c=new CDP(url);const version=await c.send('Browser.getVersion');
 const {targetId}=await c.send('Target.createTarget',{url:'about:blank'});const {sessionId}=await c.send('Target.attachToTarget',{targetId,flatten:true});
 await c.send('Page.enable',{},sessionId);await c.send('Runtime.enable',{},sessionId);
 await c.send('Emulation.setDeviceMetricsOverride',{width:1280,height:900,deviceScaleFactor:1,mobile:false},sessionId);
 return {c,sessionId,version,async close(){try{await c.send('Browser.close')}catch{}c.ws.close();if(child.exitCode===null)await Promise.race([new Promise(r=>child.once('exit',r)),pause(3000)]);if(child.exitCode===null)child.kill('SIGTERM')}};
}
async function visit(browser,url,server){
 await browser.c.send('Runtime.evaluate',{expression:'window.bench=null'},browser.sessionId);
 server.reset();const navigation=await browser.c.send('Page.navigate',{url},browser.sessionId);if(navigation.errorText)throw Error(navigation.errorText);const deadline=Date.now()+240000;
 while(Date.now()<deadline){
  const v=await browser.c.send('Runtime.evaluate',{expression:'JSON.stringify(window.bench || null)',returnByValue:true},browser.sessionId);
  const state=v.result.value?JSON.parse(v.result.value):null;
  if(state?.error||state?.done){
   const reqs=server.metrics();state.requests=reqs;state.wire_bytes=reqs.reduce((a,r)=>a+r.bytes,0);state.browser=browser.version.product;
   if(state.done){state.ready_ms=state.marks.readyPainted;state.first_interaction_ms=state.marks.filterPainted;state.click_to_result_ms=state.marks.filterPainted-state.marks.click}
   return state;
  }
  await pause(100);
 }
 throw Error('Visit timeout '+url);
}
const lanes=process.env.LANES?.split(',')||['facet_image','facet_image_br','facet_parquet','facet_parquet_zstd','duck_range','duck_full','native'];
const profiles=mode==='smoke'?[{name:'unthrottled_smoke',mbps:1000,latency:0}]:[
 {name:'representative_4g',mbps:20,latency:60},
 {name:'slower_4g',mbps:5,latency:100},
 {name:'slow_4g',mbps:1.6,latency:150},
].filter(p=>!process.env.PROFILES||process.env.PROFILES.split(',').includes(p.name));
const rounds=Number(process.env.ROUNDS||(mode==='smoke'?1:3));let server;const all=[];
try{
 server=await startServer();console.log('Local server ready',server.port);
 for(const profile of profiles)for(let round=1;round<=rounds;round++){
  const shift=(round-1)%lanes.length;const order=[...lanes.slice(shift),...lanes.slice(0,shift)];
  for(const lane of order){
   server.setProfile(profile);console.log('Starting browser',profile.name,round,lane);const b=await chrome();
   try{
    for(const cache of mode==='smoke'?['cold']:['cold','repeat']){
     const state=await visit(b,`http://127.0.0.1:${server.port}/?lane=${lane}`,server);
     const result={profile,round,lane,cache,...state};all.push(result);
     console.log(profile.name,round,lane,cache,state.error||`${(state.first_interaction_ms/1000).toFixed(3)}s ready=${(state.ready_ms/1000).toFixed(3)}s click=${state.click_to_result_ms.toFixed(1)}ms wire=${state.wire_bytes}`);
     fs.writeFileSync(path.join(output,mode+'-results.json'),JSON.stringify(all,null,2));
     if(state.error)break;
    }
   }finally{await b.close()}
  }
 }
}finally{if(server)await server.close()}
if(all.some(r=>r.error))process.exitCode=1;
