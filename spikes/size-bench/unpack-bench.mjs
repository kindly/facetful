// Decode speed of the packed wire form in wasm (V8): unpack a whole packed
// image, best of 5, in MB of image produced per second.
//   KEEP=$TMPDIR/packed ./target/release/size-bench pack <image>   writes the .fctz files
//   node unpack-bench.mjs $TMPDIR/packed/*.fctz
import { readFileSync } from "node:fs";
const wasm = readFileSync(new URL("./wasm/target/wasm32-unknown-unknown/release/unpack_wasm.wasm", import.meta.url));
for (const path of process.argv.slice(2)) {
  const wire = readFileSync(path);
  let best = Infinity, len = 0;
  for (let i = 0; i < 5; i++) {
    // a fresh instance each time: memory growth is part of a real load
    const { instance } = await WebAssembly.instantiate(wasm, {});
    const x = instance.exports;
    const p = x.alloc(wire.length);
    new Uint8Array(x.memory.buffer, p, wire.length).set(wire);
    const t = performance.now();
    const out = x.unpack(p, wire.length);
    const ms = performance.now() - t;
    if (!out) throw new Error(`${path}: unpack failed`);
    len = x.out_len();
    best = Math.min(best, ms);
  }
  console.log(`${path.split("/").pop().padEnd(34)} wire ${(wire.length / 1e6).toFixed(1).padStart(6)} MB -> image ${(len / 1e6).toFixed(1).padStart(6)} MB  ${best.toFixed(0).padStart(5)} ms  ${(len / 1e6 / (best / 1e3)).toFixed(0).padStart(5)} MB/s`);
}
