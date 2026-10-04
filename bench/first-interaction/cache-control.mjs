// Follow the sensitivity run without running two browser benchmarks concurrently.
import fs from 'node:fs';import {spawn} from 'node:child_process';
const root='bench/reports/2026-09-24-first-interaction';
const deadline=Date.now()+15*60*1000;
while(true){
 let finished=false;try{const a=JSON.parse(fs.readFileSync(root+'/sensitivity/measure-results.json'));finished=a.length===20&&a.every(r=>r.done)}catch{}
 if(finished)break;
 if(Date.now()>deadline)throw Error('Previous sensitivity run did not finish');
 await new Promise(r=>setTimeout(r,1000));
}
await new Promise(r=>setTimeout(r,2000));
for(const [profiles,rounds,out] of [['representative_4g','3','cache-validators'],['slower_4g,slow_4g','1','cache-validators-sensitivity']]){
 const child=spawn(process.execPath,['bench/first-interaction/run.mjs','measure'],{stdio:'inherit',env:{...process.env,LANES:'duck_range',PROFILES:profiles,ROUNDS:rounds,OUT:root+'/'+out}});
 const code=await new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',resolve)});
 if(code!==0)process.exit(code??1);
}
