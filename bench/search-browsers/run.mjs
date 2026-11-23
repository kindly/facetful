// Run the search benchmark in headless browsers on this machine: for each
// browser, dataset and profile, a fresh browser profile opens the page,
// which works through its phases and posts results; done, or a timeout,
// closes the browser.
//   node bench/search-browsers/run.mjs   (BROWSERS, DATASETS, PROFILES: comma lists)
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { startServer } from "./server.mjs";

const list = (v, d) => (process.env[v] ? process.env[v].split(",") : d);
const browsers = list("BROWSERS", ["chromium", "firefox"]);
const datasets = list("DATASETS", ["units", "grantnav"]);
const profiles = list("PROFILES", ["local", "20mbps"]);
const out = process.env.OUT ?? `/tmp/browserdb-search-browsers/results-${new Date().toISOString().slice(0, 16).replace(/[:T]/g, "-")}.jsonl`;
const server = await startServer({ out });
console.log(`server on ${server.port}; results in ${out}`);

const launch = (browser, dir, url) => {
  const args = browser === "firefox"
    ? ["--headless", "--no-remote", "--profile", dir, url]
    : ["--headless=new", "--no-first-run", "--no-default-browser-check", "--disable-gpu", `--user-data-dir=${dir}`, url];
  return spawn(browser, args, { stdio: "ignore", detached: true });
};

for (const browser of browsers) {
  for (const dataset of datasets) {
    for (const profile of profiles) {
      const run = `${browser}-${dataset}-${profile}-${Date.now()}`;
      const dir = fs.mkdtempSync(path.join(os.tmpdir(), "facetful-bench-"));
      const url = `http://127.0.0.1:${server.port}/?dataset=${dataset}&p=${profile}&run=${run}`;
      const t0 = performance.now();
      const proc = launch(browser, dir, url);
      const outcome = await new Promise((resolve) => {
        const timer = setTimeout(() => resolve("timeout"), 10 * 60_000);
        const on = (r) => {
          if (r.run !== run) return;
          if (r.error) console.log(`  ${r.phase}: ERROR ${r.error.split("\n")[0]}`);
          else if (!r.done) console.log(`  ${r.phase}: ${JSON.stringify(r.marks)}`);
          if (r.done || r.error) {
            clearTimeout(timer);
            server.results.off("result", on);
            resolve(r.error ? "error" : "done");
          }
        };
        server.results.on("result", on);
      });
      // the browser and its helpers, then its profile once it has let go
      const exited = new Promise((r) => (proc.exitCode !== null ? r() : proc.once("exit", r)));
      try { process.kill(-proc.pid, "SIGTERM"); } catch { /* gone */ }
      const killer = setTimeout(() => { try { process.kill(-proc.pid, "SIGKILL"); } catch { /* gone */ } }, 5000);
      await exited;
      clearTimeout(killer);
      fs.rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
      console.log(`${run}: ${outcome} in ${((performance.now() - t0) / 1000).toFixed(0)} s`);
    }
  }
}
server.close();
console.log(`results: ${out}`);
