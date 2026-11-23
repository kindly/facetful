// The search benchmark's server: the page and its bundles, the datasets
// (paced to a network profile), and a place for the page to post results.
//   node bench/search-browsers/server.mjs [port]   (standalone: for a phone on the LAN)
// Data requests name their profile (`?p=`): `local` sends the file as it is,
// as fast as it goes; the others send the brotli copy at that bandwidth,
// after that latency. Every response is cross-origin isolated (COOP/COEP),
// so Chromium can measure the page's memory.
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import { EventEmitter } from "node:events";

export const ASSETS = "/tmp/browserdb-search-browsers/assets";
export const PROFILES = {
  local: null,
  "100mbps": { mbps: 100, latency: 20 },
  "20mbps": { mbps: 20, latency: 60 },
};
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".json": "application/json" };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Start the server; `results` emits each posted result (`result`). */
export async function startServer({ port = 0, host = "127.0.0.1", out } = {}) {
  const results = new EventEmitter();
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://x");
    const headers = {
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "require-corp",
      "Cross-Origin-Resource-Policy": "same-origin",
      "Cache-Control": "no-store",
    };
    try {
      if (req.method === "POST" && url.pathname === "/results") {
        let body = "";
        for await (const c of req) body += c;
        const r = JSON.parse(body);
        if (out) fs.appendFileSync(out, JSON.stringify(r) + "\n");
        results.emit("result", r);
        res.writeHead(204, headers);
        return res.end();
      }
      if (url.pathname.startsWith("/data/")) {
        const name = url.pathname.slice(6);
        const profile = PROFILES[url.searchParams.get("p") ?? "local"];
        if (!/^[\w.-]+\.facetful$/.test(name) || profile === undefined) throw new Error("bad data request");
        const file = `${ASSETS}/${name}${profile ? ".br" : ""}`;
        const size = fs.statSync(file).size;
        res.writeHead(200, { ...headers, "Content-Type": "application/octet-stream", "Content-Length": size, ...(profile ? { "Content-Encoding": "br" } : {}) });
        if (!profile) return fs.createReadStream(file).pipe(res);
        // paced: each chunk leaves when the profile's bandwidth allows
        await sleep(profile.latency);
        const start = performance.now();
        const perMs = (profile.mbps * 1e6) / 8 / 1000;
        let sent = 0;
        for await (const chunk of fs.createReadStream(file, { highWaterMark: 64 * 1024 })) {
          sent += chunk.length;
          const wait = start + sent / perMs - performance.now();
          if (wait > 0) await sleep(wait);
          if (!res.write(chunk)) await new Promise((r) => res.once("drain", r));
        }
        return res.end();
      }
      const name = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
      if (!/^[\w.-]+$/.test(name) || !fs.existsSync(`${ASSETS}/${name}`)) {
        res.writeHead(404, headers);
        return res.end();
      }
      res.writeHead(200, { ...headers, "Content-Type": TYPES[name.slice(name.lastIndexOf("."))] ?? "application/octet-stream" });
      fs.createReadStream(`${ASSETS}/${name}`).pipe(res);
    } catch (e) {
      res.writeHead(400, headers);
      res.end(String(e.message));
    }
  });
  await new Promise((r) => server.listen(port, host, r));
  return { port: server.address().port, results, close: () => server.close() };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const s = await startServer({ port: Number(process.argv[2] ?? 8077), host: "0.0.0.0", out: "/tmp/browserdb-search-browsers/manual.jsonl" });
  const lan = Object.values(os.networkInterfaces()).flat().filter((a) => a.family === "IPv4" && !a.internal).map((a) => a.address);
  console.log(`serving on port ${s.port}; open on a device: ${lan.map((ip) => `http://${ip}:${s.port}/?dataset=units`).join("  ")}`);
  console.log("results are appended to /tmp/browserdb-search-browsers/manual.jsonl");
  s.results.on("result", (r) => console.log(`${r.phase ?? ""} ${r.dataset ?? ""}: ${r.error ? "ERROR " + r.error : "ok"}`));
}
