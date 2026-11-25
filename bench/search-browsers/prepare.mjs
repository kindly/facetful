// Assets for the search benchmark in real browsers: the full wasm build and
// the bundled client, and the datasets (as-is for the local profile, brotli
// for the throttled ones) with a manifest of their sizes.
//   node bench/search-browsers/prepare.mjs
// Writes /tmp/browserdb-search-browsers/assets. The datasets are read in
// place and never written: grantnav from ~/projects/gratnav (its split parts
// are written here), GEM units from data/.
import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import { execFileSync } from "node:child_process";

const root = new URL("../../", import.meta.url).pathname;
const out = `${process.env.SEARCH_BENCH_DIR ?? "/tmp/browserdb-search-browsers"}/assets`;
fs.mkdirSync(out, { recursive: true });
const esbuild = process.env.ESBUILD || "/home/david/projects/gem-query/node_modules/esbuild/bin/esbuild";
const run = (cmd, args) => execFileSync(cmd, args, { cwd: root, stdio: "inherit" });

// the engine and the client, as a page would ship them
run("cargo", ["build", "-q", "--release", "--target", "wasm32-unknown-unknown", "-p", "facetful-wasm"]);
fs.copyFileSync(path.join(root, "target/wasm32-unknown-unknown/release/facetful_wasm.wasm"), `${out}/facetful.wasm`);
run("cargo", ["build", "-q", "--release", "-p", "facetful-cli"]);
for (const [input, output, extra] of [
  ["js/facetful/worker.js", "worker.js", ["--external:hyparquet"]],
  ["bench/search-browsers/app.js", "app.js", []],
]) {
  run(esbuild, [input, "--bundle", "--format=esm", "--platform=browser", "--minify", `--outfile=${out}/${output}`, ...extra]);
}
fs.copyFileSync(path.join(root, "bench/search-browsers/index.html"), `${out}/index.html`);

// the datasets
const grantnav = process.env.GRANTNAV || `${process.env.HOME}/projects/gratnav/data/site/grantnav.facetful`;
const link = (src, name) => {
  const dst = `${out}/${name}`;
  if (!fs.existsSync(dst)) fs.symlinkSync(src, dst);
};
link(grantnav, "grantnav.facetful");
link(process.env.UNITS || path.join(root, "data/units-2026-08.facetful"), "units.facetful");
if (!fs.existsSync(`${out}/grantnav-1.facetful`)) {
  run(path.join(root, "target/release/facetful"), ["split", grantnav, `${out}/grantnav-1.facetful`, `${out}/grantnav-2.facetful`, "--columns", "Description Rare,Identifier"]);
}
// the packed form of each (facetful pack): what the page unpacks as it arrives
for (const name of ["units", "grantnav"]) {
  if (!fs.existsSync(`${out}/${name}-packed.facetful`)) {
    run(path.join(root, "target/release/facetful"), ["pack", `${out}/${name}.facetful`, `${out}/${name}-packed.facetful`]);
  }
}
const files = ["units.facetful", "grantnav.facetful", "grantnav-1.facetful", "grantnav-2.facetful", "units-packed.facetful", "grantnav-packed.facetful"];
const manifest = { preparedAt: new Date().toISOString(), files: {} };
for (const f of files) {
  const raw = fs.statSync(`${out}/${f}`).size;
  if (!fs.existsSync(`${out}/${f}.br`)) {
    console.log(`brotli ${f} (${(raw / 1e6).toFixed(0)} MB)…`);
    const b = fs.readFileSync(`${out}/${f}`);
    fs.writeFileSync(`${out}/${f}.br`, zlib.brotliCompressSync(b, {
      params: { [zlib.constants.BROTLI_PARAM_QUALITY]: 9, [zlib.constants.BROTLI_PARAM_LGWIN]: 24, [zlib.constants.BROTLI_PARAM_SIZE_HINT]: b.length },
    }));
  }
  manifest.files[f] = { bytes: raw, br: fs.statSync(`${out}/${f}.br`).size };
}
fs.writeFileSync(`${out}/manifest.json`, JSON.stringify(manifest, null, 2));
console.log(manifest);
