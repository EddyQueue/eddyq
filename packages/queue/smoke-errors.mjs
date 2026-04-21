// Smoke test for return values + structured errors + retry convention.
//   - "demo.ok" → returns a value, expect stored in eddyq_jobs.result
//   - "demo.cancel" → throws CancelError, expect state=failed, attempt=1, no retry
//   - "demo.retry" → throws RetryError first attempt, succeeds second
//   - "demo.boom" → throws Error, expect structured {name, stack} in errors[]
//
// Schema: uses `errsmoke` to stay isolated. Create with:
//   docker exec eddyq-dev-postgres psql -U eddyq -d eddyq_dev \
//     -c "CREATE SCHEMA IF NOT EXISTS errsmoke;"

import pkg from "./lib.cjs";
const { Eddyq, CancelError, RetryError } = pkg;

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });
const report = await q.migrate();
console.log("migrate applied:", report.applied.map((r) => `${r.version}:${r.name}`));

const outcomes = {};

q.work("demo.ok", async (call) => {
  outcomes.ok = true;
  return { echoed: call.payload, at: Date.now() };
});

q.work("demo.cancel", async () => {
  outcomes.cancelSeen = (outcomes.cancelSeen ?? 0) + 1;
  throw new CancelError("I refuse to do this");
});

let retryFirstAttempt = true;
q.work("demo.retry", async (call) => {
  if (retryFirstAttempt) {
    retryFirstAttempt = false;
    throw new RetryError("not yet", { delayMs: 500 });
  }
  outcomes.retrySucceededAttempt = call.attempt;
  return "eventually";
});

q.work("demo.boom", async () => {
  outcomes.boomSeen = (outcomes.boomSeen ?? 0) + 1;
  const e = new TypeError("intentional boom");
  throw e;
});

q.setWorkerConcurrency(4);

const okE = await q.enqueue("demo.ok", { hello: "world" }, { uniqueKey: `ok-${Date.now()}` });
const cancelE = await q.enqueue("demo.cancel", {}, { maxAttempts: 5, uniqueKey: `cancel-${Date.now()}` });
const retryE = await q.enqueue("demo.retry", {}, { uniqueKey: `retry-${Date.now()}` });
const boomE = await q.enqueue("demo.boom", {}, { maxAttempts: 2, uniqueKey: `boom-${Date.now()}` });
console.log("enqueued:", { ok: okE, cancel: cancelE, retry: retryE, boom: boomE });

await q.start();
console.log("worker started");

// Give the runtime a few seconds to process: boom retries ~1s, retry ~0.5s.
await new Promise((r) => setTimeout(r, 6000));

await q.shutdown();
console.log("shutdown complete");

console.log("outcomes:", outcomes);

// Peek at the DB rows to validate expectations.
import pg from "pg";
const client = new pg.Client({
  connectionString: "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01",
});
await client.connect();
const rows = await client.query(
  "SELECT id, kind, state, attempt, max_attempts, result, errors FROM eddyq_jobs WHERE id = ANY($1::bigint[]) ORDER BY id",
  [[okE.id, cancelE.id, retryE.id, boomE.id]],
);
for (const r of rows.rows) {
  console.log("  ", r.kind, {
    state: r.state,
    attempt: r.attempt,
    max: r.max_attempts,
    result: r.result,
    errors: r.errors,
  });
}
await client.end();
await q.close();
