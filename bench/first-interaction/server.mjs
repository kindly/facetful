import http from 'node:http';import fs from 'node:fs';import path from 'node:path';import {gzipSync} from 'node:zlib';import {spawn} from 'node:child_process';import readline from 'node:readline';
const root='/tmp/browserdb-first-interaction/assets';
export async function startServer(){
 const child=spawn('python3',['bench/first-interaction/native-rpc.py'],{stdio:['pipe','pipe','inherit']});let nativeId=0;const pending=new Map();
 let readyResolve;const ready=new Promise(r=>readyResolve=r);
 readline.createInterface({input:child.stdout}).on('line',line=>{const m=JSON.parse(line);if(m.ready)readyResolve();else{pending.get(m.id)?.(m);pending.delete(m.id)}});
 await ready;
 const native=filtered=>new Promise(r=>{const id=++nativeId;pending.set(id,r);child.stdin.write(JSON.stringify({id,filtered})+'\n')});
 const cache=new Map();for(const name of fs.readdirSync(root)){if(name==='manifest.json')continue;cache.set(name,fs.readFileSync(path.join(root,name)))}
 const manifest=JSON.parse(fs.readFileSync(path.join(root,'manifest.json')));
 const html=gzipSync(fs.readFileSync('bench/first-interaction/index.html'),{level:9});
 let profile={mbps:1000,latency:0},metrics=[],queue=[],tokens=0,last=performance.now();
 const ticker=setInterval(()=>{const now=performance.now();const rate=profile.mbps*1e6/8;tokens=Math.min(tokens+(now-last)/1000*rate,Math.max(rate*.025,32768));last=now;
  let guard=queue.length*4+1;while(tokens>=1&&queue.length&&guard--){const q=queue.shift();if(q.res.destroyed){q.record.aborted=true;continue}const n=Math.min(16384,Math.floor(tokens),q.buf.length-q.pos);tokens-=n;q.res.write(q.buf.subarray(q.pos,q.pos+n));q.pos+=n;q.record.bytes+=n;if(q.pos===q.buf.length){q.res.end();q.record.end=performance.now()}else queue.push(q)}
 },5);
 const server=http.createServer(async(req,res)=>{
  const url=new URL(req.url,'http://localhost'),record={path:url.pathname,method:req.method,range:req.headers.range||null,bytes:0,start:performance.now()};metrics.push(record);
  try{
   let body,status=200,headers={'Cache-Control':'public, max-age=31536000','Timing-Allow-Origin':'*'};
   if(url.pathname==='/api/facets'){
    let request='';for await(const c of req)request+=c;const parsed=JSON.parse(request);const result=await native(!!parsed.filtered);body=gzipSync(Buffer.from(JSON.stringify(result)));headers={...headers,'Cache-Control':'no-store','Content-Type':'application/json','Content-Encoding':'gzip'};record.query_ms=result.query_ms;
   }else if(url.pathname==='/'||url.pathname==='/index.html') {body=html;headers['Content-Type']='text/html';headers['Content-Encoding']='gzip'}
   else{
    const name=path.basename(url.pathname);const isParquet=name.endsWith('.parquet');const br=url.searchParams.get('enc')==='br'&&cache.has(name+'.br');const compressed=!isParquet;body=cache.get(name+(br?'.br':compressed?'.gz':''));
    if(!body){status=404;body=Buffer.from('Missing '+name);headers['Content-Type']='text/plain'}
    else{
     headers['ETag']='"'+manifest.files[name].sha256+'"';
     headers['Last-Modified']='Mon, 21 Sep 2026 00:00:00 GMT';
     headers['Content-Type']=name.endsWith('.wasm')?'application/wasm':name.endsWith('.js')?'text/javascript':'application/octet-stream';if(compressed)headers['Content-Encoding']=br?'br':'gzip';
     if(isParquet){headers['Accept-Ranges']='bytes';const range=/bytes=(\d+)-(\d*)/.exec(req.headers.range||'');if(range){const start=Number(range[1]),end=Math.min(range[2]?Number(range[2]):body.length-1,body.length-1);headers['Content-Range']=`bytes ${start}-${end}/${body.length}`;body=body.subarray(start,end+1);status=206}}
    }
   }
   headers['Content-Length']=body.length;record.status=status;record.encodedSize=body.length;
   setTimeout(()=>{if(res.destroyed)return;res.writeHead(status,headers);if(req.method==='HEAD'||body.length===0){res.end();record.end=performance.now()}else queue.push({res,buf:body,pos:0,record})},profile.latency);
  }catch(e){res.writeHead(500);res.end(String(e));record.error=String(e)}
 });
 await new Promise((r,j)=>{server.once('error',j);server.listen(0,'127.0.0.1',r)});
 return {port:server.address().port,setProfile(p){profile=p;tokens=0;last=performance.now()},reset(){metrics=[]},metrics:()=>JSON.parse(JSON.stringify(metrics)),async close(){clearInterval(ticker);server.closeAllConnections();await new Promise(r=>server.close(r));child.stdin.end()}};
}
