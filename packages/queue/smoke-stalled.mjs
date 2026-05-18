// End-to-end smoke for the stalled_count surface:
//   1. Enqueue with maxAttempts: 1, maxStalledCount: 2 — verify both
//      round-trip into the row.
//   2. Plant a "crashed" running row directly via pg (ancient heartbeat).
//   3. Start the queue with a tight staleAfterMs so the heartbeat sweeper
//      picks the row up and recovers it.
//   4. The handler should run exactly once and observe stalledCount=1 /
//      maxStalledCount=2 on JobCall.
//
// Assumes docker-compose.dev.yml postgres is up on :5433.

import pg from "pg";
import { Eddyq } from "./index.js";

const BASE_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev";
const SCHEMA = "stalled_smoke";
const DB_URL = `${BASE_URL}?options=-c%20search_path%3D${SCHEMA}`;

// Create the schema (idempotent) before connecting via Eddyq.
{
  const c = new pg.Client(BASE_URL);
  await c.connect();
  await c.query(`CREATE SCHEMA IF NOT EXISTS ${SCHEMA}`);
  await c.end();
}

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });
console.log("connected; line:", q.line);
const report = await q.migrate();
console.log(
  "migrate: applied",
  report.applied.map((r) => `${r.version}:${r.name}`),
);

// pg client we'll use for the manipulations the public API doesn't expose.
const raw = new pg.Client(DB_URL);
await raw.connect();
await raw.query(`DELETE FROM eddyq_jobs WHERE kind = 'stalled.demo'`);

// (1) Enqueue with explicit maxStalledCount and verify it persisted.
const enq = await q.enqueue(
  "stalled.demo",
  { ts: Date.now() },
  { maxAttempts: 1, maxStalledCount: 2 },
);
console.log("enqueued:", enq);

{
  const { rows } = await raw.query(
    `SELECT max_attempts, max_stalled_count, stalled_count, attempt
       FROM eddyq_jobs WHERE id = $1`,
    [enq.id],
  );
  console.log("row after enqueue:", rows[0]);
  if (rows[0].max_stalled_count !== 2) {
    console.error("FAIL: max_stalled_count did not round-trip");
    process.exit(1);
  }
}

// (2) Simulate a crashed worker: state=running, attempt=1, old heartbeat.
await raw.query(
  `UPDATE eddyq_jobs
      SET state        = 'running',
          attempt      = 1,
          heartbeat_at = NOW() - INTERVAL '10 seconds',
          worker_id    = gen_random_uuid()
    WHERE id = $1`,
  [enq.id],
);

// (3) Run a handler. With staleAfterMs=500 + sweepIntervalMs=200, the
// sweeper will recover the planted row almost immediately.
let resolveSeen;
const seen = new Promise((r) => (resolveSeen = r));

q.work("stalled.demo", async (call) => {
  console.log("handler got call:", {
    id: call.id,
    attempt: call.attempt,
    maxAttempts: call.maxAttempts,
    stalledCount: call.stalledCount,
    maxStalledCount: call.maxStalledCount,
  });
  resolveSeen(call);
  return { ok: true };
});

q.setWorkerConcurrency(2);
q.subscribeTo(["default"]);
await q.start({
  sweepIntervalMs: 200,
  staleAfterMs: 500,
  heartbeatIntervalMs: 100,
});

const call = await Promise.race([
  seen,
  new Promise((_, reject) =>
    setTimeout(() => reject(new Error("timeout waiting for handler")), 10_000),
  ),
]).catch((e) => {
  console.error(e.message);
  return null;
});

await q.shutdown();
await q.close();
await raw.end();

if (!call) process.exit(1);

let ok = true;
if (call.stalledCount !== 1) {
  console.error("FAIL: stalledCount =", call.stalledCount, "(expected 1)");
  ok = false;
}
if (call.maxStalledCount !== 2) {
  console.error(
    "FAIL: maxStalledCount =",
    call.maxStalledCount,
    "(expected 2)",
  );
  ok = false;
}
if (call.attempt !== 1) {
  console.error("FAIL: attempt =", call.attempt, "(expected 1)");
  ok = false;
}

if (ok) {
  console.log("PASS: stalled smoke complete");
  process.exit(0);
} else {
  process.exit(1);
}
