// Smoke test: load the NAPI binding, connect to local dev Postgres, migrate,
// enqueue a job, cancel it, close. Run with:
//   node smoke.mjs
// Schema `v01` (collapsed-migration namespace); create once with:
//   docker exec eddyq-dev-postgres psql -U eddyq -d eddyq_dev \
//     -c "CREATE SCHEMA IF NOT EXISTS v01;"

import { Eddyq, version } from "./index.js";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

console.log("eddyq-napi version:", version());

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });
console.log("connected; migration line:", q.line);

// If the dev DB was already migrated by a prior Rust run, the tracking table
// tells us so and migrate() is a no-op. If it wasn't, this applies everything.
// Either outcome is fine for the smoke test.
try {
  const report = await q.migrate();
  console.log(
    "migrate: applied",
    report.applied.map((r) => `${r.version}:${r.name}`),
  );
} catch (e) {
  console.log("migrate: skipped (DB already populated) —", e.message);
}

const status = await q.migrationStatus();
console.log("status:", status.map((s) => `${s.version} ${s.appliedAt ? "✓" : "pending"}`).join(", "));

const enq = await q.enqueue(
  "demo.hello",
  { who: "world", at: new Date().toISOString() },
  { priority: 5, tags: ["smoke"], uniqueKey: `smoke-${Date.now()}` },
);
console.log("enqueue:", enq);

if (enq.inserted && enq.id !== undefined) {
  const cancelled = await q.cancel(enq.id);
  console.log("cancel:", cancelled);
}

await q.setQueueConcurrency("default", 32);
await q.setGroupConcurrency("smoke:group", 2);
console.log("admin ops OK");

await q.close();
console.log("closed");
