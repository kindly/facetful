// The three builds a page can load, through the public API: Facetful.open
// ({ build }) picks the file shipped next to index.js, and each build runs
// what it has and refuses what it leaves out with an error naming a build
// that has it. Run after build-package.sh has copied the wasm files here.
//
// The worker runs in this process: a stand-in Worker imports worker.js
// (a fresh copy per build, so each has its own engine) and passes messages.
import fs from "node:fs";
import { fileURLToPath } from "node:url";

const spike = new URL("../../spikes/facet-spike/data-200000.facetful", import.meta.url);

globalThis.self = globalThis;
globalThis.fetch = async (url) => new Response(fs.readFileSync(fileURLToPath(url)));
let current = null;
globalThis.postMessage = (msg) => current.onmessage({ data: msg });
let copies = 0;
globalThis.Worker = class {
  constructor(url) {
    current = this;
    this.ready = import(`${url}?copy=${++copies}`);
  }
  postMessage(msg) {
    this.ready.then(() => self.onmessage({ data: msg }));
  }
  terminate() {}
};

const { Facetful } = await import("./index.js");

const fail = (m) => {
  throw new Error(m);
};
const refuses = async (what, p, words) => {
  try {
    await p;
  } catch (e) {
    if (!e.message.includes(words)) fail(`${what}: error does not say "${words}": ${e.message}`);
    return;
  }
  fail(`${what} ran`);
};
const count = async (db, sql) => (await db.query(sql, { table: "t" })).column("n")[0];

const join = "select count(*) as n from t join m on t.id = m.id";
const cte = "with x as (select id from t where capacity > 290) select count(*) as n from x";
const search = "select count(*) as n from t where search('country_3', country)";

for (const build of ["lite", "sql", "full"]) {
  const db = await Facetful.open({ build });
  if (db.build !== build) fail(`asked for ${build}, loaded ${db.build}`);
  await db.load("t", fs.readFileSync(spike).buffer);
  // what every build runs: single-table queries and result handles
  const top = await db.query("select country, count(*) as n from t group by country order by n desc, country limit 3", { table: "t" });
  if (top.rowCount !== 3) fail(`${build}: group by gave ${top.rowCount} rows`);
  const h = await db.prepare("select id, capacity from t where capacity > 290", { table: "t" });
  const page = await h.page(0, 5);
  if (page.rowCount !== 5) fail(`${build}: a handle's page gave ${page.rowCount} rows`);
  await h.close();
  const want = await count(db, "select count(*) as n from t where capacity > 290");

  if (build === "lite") {
    await refuses("lite: a join", db.query(join, { table: "t" }), "not in this build (lite): use the sql or full build");
    await refuses("lite: a CTE", db.query(cte, { table: "t" }), "not in this build (lite)");
    await refuses("lite: materialize", db.materialize("m", "select id from t", { table: "t" }), "needs the sql or full facetful build (this one is lite)");
  } else {
    await db.materialize("m", "select id from t where capacity > 290", { table: "t" });
    if ((await db.query("select count(*) as n from m", { table: "m" })).column("n")[0] !== want) fail(`${build}: materialize counts wrong`);
    if ((await count(db, join)) !== want) fail(`${build}: the join counts wrong`);
    if ((await count(db, cte)) !== want) fail(`${build}: the CTE counts wrong`);
  }

  if (build === "full") {
    await db.buildSearchIndex(["country"], { table: "t" });
    if (!((await count(db, search)) > 0)) fail("full: search() found nothing");
  } else {
    await refuses(`${build}: search()`, db.query(search, { table: "t" }), `search() is not in this build (${build}): use the full build`);
    await refuses(`${build}: buildSearchIndex`, db.buildSearchIndex(["country"], { table: "t" }), `this facetful build (${build}) has no search index`);
    await refuses(`${build}: loadUrl with searchColumns`, db.loadUrl("u", spike, { searchColumns: ["country"] }), `(${build}) has no search index`);
  }
  db.close();
  console.log(`${build}: ok`);
}

await refuses("an unknown build", Facetful.open({ build: "medium" }), 'build: "full", "sql" or "lite"');
console.log("builds smoke: OK");
