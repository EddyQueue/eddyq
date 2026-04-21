// Smoke test: enqueueMany bulk insert with mixed kinds and a unique_key
// collision. Proves the single-UNNEST INSERT path works end-to-end.
//
//   node smoke-bulk.mjs

import { Eddyq, version } from "./index.js";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

console.log("eddyq-napi version:", version());

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });

// Mixed-kind batch, 500 jobs across two kinds, with a delayed subset.
const stamp = Date.now();
const items = [];
for (let i = 0; i < 300; i++) {
  items.push({
    kind: "bulk.demo.email",
    payload: { to: `user${i}@example.com`, subject: `#${i}` },
    uniqueKey: `bulk-${stamp}-email-${i}`,
    priority: 2,
  });
}
for (let i = 0; i < 200; i++) {
  items.push({
    kind: "bulk.demo.report",
    payload: { id: i },
    uniqueKey: `bulk-${stamp}-report-${i}`,
    delayMs: 5_000,
  });
}

const t0 = Date.now();
const r1 = await q.enqueueMany(items);
const t1 = Date.now();
console.log(
  `enqueueMany(500 mixed-kind) → inserted=${r1.inserted} skipped=${r1.skipped} in ${t1 - t0}ms`,
);
if (r1.inserted !== 500 || r1.skipped !== 0) {
  console.error("FAIL: expected 500 inserted / 0 skipped");
  process.exit(1);
}

// Re-run with an overlapping uniqueKey set — 100 duplicates + 100 new.
const items2 = [];
for (let i = 250; i < 350; i++) {
  items2.push({
    kind: "bulk.demo.email",
    payload: { to: `user${i}@example.com`, subject: `dup #${i}` },
    uniqueKey: `bulk-${stamp}-email-${i}`, // 250..299 collide with first batch
  });
}
const r2 = await q.enqueueMany(items2);
console.log(
  `enqueueMany(100 w/ 50 collisions) → inserted=${r2.inserted} skipped=${r2.skipped}`,
);
if (r2.inserted !== 50 || r2.skipped !== 50) {
  console.error("FAIL: expected 50 inserted / 50 skipped");
  process.exit(1);
}

// Empty array is a no-op.
const r3 = await q.enqueueMany([]);
if (r3.inserted !== 0 || r3.skipped !== 0) {
  console.error("FAIL: empty array should return 0/0");
  process.exit(1);
}

// Bad option combo should reject.
try {
  await q.enqueueMany([
    {
      kind: "bulk.demo.email",
      payload: {},
      scheduledAtMs: Date.now() + 1000,
      delayMs: 1000,
    },
  ]);
  console.error("FAIL: combined scheduledAtMs + delayMs should have thrown");
  process.exit(1);
} catch (e) {
  console.log("combined scheduledAtMs + delayMs rejected:", e.message.split("\n")[0]);
}

// Tags round-trip in bulk path and are queryable via listJobs.
const tagStamp = Date.now();
const tagResult = await q.enqueueMany([
  {
    kind: "bulk.demo.tagged",
    payload: { n: 1 },
    uniqueKey: `tag-${tagStamp}-a`,
    tags: ["urgent", "billing"],
  },
  {
    kind: "bulk.demo.tagged",
    payload: { n: 2 },
    uniqueKey: `tag-${tagStamp}-b`,
    tags: ["urgent"],
  },
  {
    kind: "bulk.demo.tagged",
    payload: { n: 3 },
    uniqueKey: `tag-${tagStamp}-c`,
    // No tags — defaults to empty array.
  },
]);
console.log(
  `enqueueMany(3 tagged) → inserted=${tagResult.inserted} skipped=${tagResult.skipped}`,
);
const billing = await q.listJobs({ kind: "bulk.demo.tagged", tags: ["billing"] }, { limit: 10 });
const urgent = await q.listJobs({ kind: "bulk.demo.tagged", tags: ["urgent"] }, { limit: 10 });
console.log(`listJobs tags=[billing]: ${billing.rows.length}`);
console.log(`listJobs tags=[urgent]:  ${urgent.rows.length}`);
if (billing.rows.length < 1 || urgent.rows.length < 2) {
  console.error("FAIL: tag filter did not return expected rows");
  process.exit(1);
}

// Batch-size cap should reject at 5001 items.
try {
  const oversize = Array.from({ length: 5001 }, (_, i) => ({
    kind: "bulk.demo.oversize",
    payload: { i },
    uniqueKey: `oversize-${tagStamp}-${i}`,
  }));
  await q.enqueueMany(oversize);
  console.error("FAIL: 5001-item batch should have been rejected");
  process.exit(1);
} catch (e) {
  console.log("5001-item batch rejected:", e.message.split("\n")[0]);
}

await q.close();
console.log("OK");
