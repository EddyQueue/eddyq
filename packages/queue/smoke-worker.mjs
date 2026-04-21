// End-to-end worker smoke test:
//   - connect
//   - register a JS handler for "demo.hello"
//   - enqueue one job
//   - start the worker runtime
//   - wait for the handler to fire
//   - shutdown + close cleanly
//
// Assumes docker-compose.dev.yml postgres is up on :5433 and the schema is
// already migrated (the earlier enqueue smoke test did that).

import { Eddyq } from "./index.js";

// Isolate this smoke run in its own Postgres schema so we don't collide with
// tables from prior Rust dev runs. Schema must exist — create with:
//   docker exec eddyq-dev-postgres psql -U eddyq -d eddyq_dev \
//     -c "CREATE SCHEMA IF NOT EXISTS smoke;"
const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });
console.log("connected; line:", q.line);

// First run on a fresh schema — apply migrations.
const report = await q.migrate();
console.log(
  "migrate: applied",
  report.applied.map((r) => `${r.version}:${r.name}`),
);

let resolveSeen;
const seen = new Promise((resolve) => (resolveSeen = resolve));

q.work("demo.hello", async (call) => {
  console.log("handler got job:", {
    id: call.id,
    kind: call.kind,
    attempt: call.attempt,
    maxAttempts: call.maxAttempts,
    payload: call.payload,
  });
  resolveSeen(call);
  return { ok: true };
});

q.setWorkerConcurrency(2);
q.subscribeTo(["default"]);

// Enqueue the job BEFORE start() so there's work waiting when the fetcher kicks in.
const enq = await q.enqueue(
  "demo.hello",
  { who: "world", ts: Date.now() },
  { priority: 10, tags: ["smoke-worker"] },
);
console.log("enqueued:", enq);

await q.start();
console.log("worker started");

const ran = await Promise.race([
  seen.then(() => "handled"),
  new Promise((resolve) => setTimeout(() => resolve("timeout"), 8000)),
]);
console.log("result:", ran);

await q.shutdown();
console.log("shutdown complete");

await q.close();
console.log("closed");

process.exit(ran === "handled" ? 0 : 1);
