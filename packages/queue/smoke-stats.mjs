// Smoke test for the dashboard read-surface: getStats, listJobs, listNamedQueues,
// listGroups, listSchedules.
//
//   node smoke-stats.mjs

import { Eddyq, version } from "./index.js";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

console.log("eddyq-napi version:", version());

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });

// Seed a few jobs across queues / states so the counts aren't empty.
const stamp = Date.now();
await q.enqueue("stats.demo", { n: 1 }, { queue: "default", uniqueKey: `stats-${stamp}-a` });
await q.enqueue("stats.demo", { n: 2 }, { queue: "default", uniqueKey: `stats-${stamp}-b` });
await q.enqueue("stats.demo", { n: 3 }, { queue: "urgent", uniqueKey: `stats-${stamp}-c` });

// Set up a named queue + group so list* have something to return.
await q.setQueueConcurrency("urgent", 8);
await q.setGroupConcurrency("stats-group", 2);

const stats = await q.getStats();
console.log("stats.byQueueState:");
for (const s of stats.byQueueState) {
  console.log(`  ${s.queue} / ${s.state}: ${s.count}`);
}

const all = await q.listJobs(undefined, { limit: 5 });
console.log(`listJobs all: total=${all.total}, rows=${all.rows.length}`);
for (const r of all.rows) {
  console.log(`  #${r.id} ${r.queue}/${r.kind} state=${r.state} attempt=${r.attempt}/${r.maxAttempts}`);
}

const filtered = await q.listJobs({ queue: "urgent", state: "pending" }, { limit: 10 });
console.log(`listJobs urgent+pending: total=${filtered.total}, rows=${filtered.rows.length}`);

try {
  await q.listJobs({ state: "bogus" });
  console.error("BUG: invalid state filter should have thrown");
  process.exit(1);
} catch (e) {
  console.log("invalid state rejected:", e.message.split("\n")[0]);
}

const queues = await q.listNamedQueues();
console.log("listNamedQueues:", queues.map((x) => `${x.name} (cap=${x.maxConcurrency}, running=${x.runningCount})`).join(", "));

const groups = await q.listGroups();
console.log("listGroups:", groups.map((x) => `${x.key} (cap=${x.maxConcurrency})`).join(", "));

const scheds = await q.listSchedules();
console.log("listSchedules:", scheds.length === 0 ? "(none)" : scheds.map((s) => s.name).join(", "));

await q.close();
console.log("OK");
