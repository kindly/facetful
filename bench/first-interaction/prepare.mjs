import fs from 'node:fs';import path from 'node:path';import {execFileSync} from 'node:child_process';import {gzipSync,brotliCompressSync,constants as zc} from 'node:zlib';import crypto from 'node:crypto';
const root=process.cwd(),out='/tmp/browserdb-first-interaction/assets';fs.mkdirSync(out,{recursive:true});
// Keep empty strings in text columns, matching the existing Facetful image.
if(!fs.existsSync(out+'/units.parquet')){
 const columns=JSON.parse(fs.readFileSync('bench/dashboard-assessment/text-columns.json')).real;
 const quote=s=>"'"+s.replaceAll("'","''")+"'";
 execFileSync('duckdb',[':memory:','-c',`COPY (SELECT * FROM read_csv(${quote(root+'/data/units-2026-08.csv')},sample_size=-1,force_not_null=[${columns.map(quote)}])) TO ${quote(out+'/units.parquet')} (FORMAT PARQUET,COMPRESSION SNAPPY,ROW_GROUP_SIZE 65536)`],{stdio:'inherit'});
}
// The same rows and row groups, ZSTD level 19: what a publisher can choose for a smaller download.
if(!fs.existsSync(out+'/units-zstd.parquet'))execFileSync('duckdb',[':memory:','-c',`COPY (FROM '${out}/units.parquet') TO '${out}/units-zstd.parquet' (FORMAT PARQUET,COMPRESSION ZSTD,COMPRESSION_LEVEL 19,ROW_GROUP_SIZE 65536)`],{stdio:'inherit'});
const extensionUrl='https://extensions.duckdb.org/v1.5.4/wasm_eh/parquet.duckdb_extension.wasm';
if(!fs.existsSync(out+'/parquet.duckdb_extension.wasm'))execFileSync('curl',['-L','--compressed','--fail',extensionUrl,'-o',out+'/parquet.duckdb_extension.wasm'],{stdio:'inherit'});
const esbuild=process.env.ESBUILD||'/home/david/projects/gem-query/node_modules/esbuild/bin/esbuild';
for(const [input,output,extra] of [
 ['bench/first-interaction/app.js','app.js',[]],
 ['bench/first-interaction/facet-entry.js','facet-entry.js',[]],
 ['js/facetful/worker.js','facet-worker.js',['--external:hyparquet']],
 ['web/spike-bench/node_modules/hyparquet/src/index.js','hyparquet.js',[]],
 ['bench/first-interaction/duck-entry.js','duck-entry.js',[]]])execFileSync(esbuild,[input,'--bundle','--format=esm','--platform=browser','--external:/assets/*','--minify',`--outfile=${out}/${output}`,...extra],{stdio:'inherit'});
fs.copyFileSync('js/facetful/facetful_wasm.wasm',out+'/facetful.wasm');
fs.copyFileSync('web/spike-bench/node_modules/@duckdb/duckdb-wasm/dist/duckdb-eh.wasm',out+'/duckdb.wasm');
fs.copyFileSync('web/spike-bench/node_modules/@duckdb/duckdb-wasm/dist/duckdb-browser-eh.worker.js',out+'/duck-worker.js');
fs.copyFileSync('data/units-2026-08.facetful',out+'/units.facetful');
const manifest={preparedAt:new Date().toISOString(),sourceCommit:execFileSync('git',['rev-parse','HEAD'],{encoding:'utf8'}).trim(),facetfulPackage:JSON.parse(fs.readFileSync('js/facetful/package.json')).version,duckdbPackage:JSON.parse(fs.readFileSync('web/spike-bench/node_modules/@duckdb/duckdb-wasm/package.json')).version,extensionUrl,esbuild:execFileSync(esbuild,['--version'],{encoding:'utf8'}).trim(),files:{}};
// The image also as brotli (quality 11, 16 MB window), as a CDN or static host would serve it.
if(!fs.existsSync(out+'/units.facetful.br')){const b=fs.readFileSync(out+'/units.facetful');fs.writeFileSync(out+'/units.facetful.br',brotliCompressSync(b,{params:{[zc.BROTLI_PARAM_QUALITY]:11,[zc.BROTLI_PARAM_LGWIN]:24,[zc.BROTLI_PARAM_SIZE_HINT]:b.length}}))}
for(const name of fs.readdirSync(out).filter(n=>!n.endsWith('.gz')&&!n.endsWith('.br')&&n!=='manifest.json')){
 const bytes=fs.readFileSync(path.join(out,name));const gzip=gzipSync(bytes,{level:9});fs.writeFileSync(path.join(out,name+'.gz'),gzip);
 manifest.files[name]={bytes:bytes.length,gzip_bytes:gzip.length,...(fs.existsSync(path.join(out,name+'.br'))?{br_bytes:fs.statSync(path.join(out,name+'.br')).size}:{}),sha256:crypto.createHash('sha256').update(bytes).digest('hex')};
}
fs.writeFileSync(out+'/manifest.json',JSON.stringify(manifest,null,2));console.log(manifest);
