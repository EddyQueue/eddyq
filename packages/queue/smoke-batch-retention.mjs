// Smoke test: batch retention reaps finalized eddyq_batches rows.
//
//   - enqueue + complete a batch
//   - boot the queue with batchRetentionSecs=1 and a fast cleanup interval
//   - verify the row is gone after the cleanup loop runs
//
//   node smoke-batch-retention.mjs

import { Eddyq } from "./index.js";
import pg from "pg";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });
await q.migrate();

let resolveDone;
const done = new Promise((resolve) => (resolveDone = resolve));

q.work("retn.item", async () => ({ ok: true }));
q.work("retn.done", async () => {
  resolveDone();
  return { ok: true };
});

q.setWorkerConcurrency(4);
q.subscribeTo(["default"]);

const stamp = Date.now();
const result = await q.enqueueBatch({
  items: [0, 1, 2].map((n) => ({
    kind: "retn.item",
    payload: { n },
    uniqueKey: `smoke-batch-retn-${stamp}-${n}`,
  })),
  onComplete: { kind: "retn.done", payload: { stamp } },
});
console.log(`enqueueBatch → batchId=${result.batchId} inserted=${result.inserted}`);

await q.start({
  cleanupIntervalMs: 500,
  // Disable job retention so we isolate batch reaping behavior.
  completedRetentionSecs: -1,
  failedRetentionSecs: -1,
  cancelledRetentionSecs: -1,
  batchRetentionSecs: 1,
});

const fired = await Promise.race([
  done.then(() => true),
  new Promise((resolve) => setTimeout(() => resolve(false), 10_000)),
]);
if (!fired) {
  console.error("FAIL: callback never fired");
  process.exit(1);
}

// At this point the batch is `state='complete'` with `finalized_at = NOW()`.
// Wait long enough for the row to age past the 1s retention + at least one
// cleanup tick (500ms). Pad generously to absorb scheduler jitter.
await new Promise((r) => setTimeout(r, 3000));

const client = new pg.Client({ connectionString: DB_URL });
await client.connect();
const { rows } = await client.query(
  "SELECT state, finalized_at FROM eddyq_batches WHERE id = $1",
  [result.batchId],
);
await client.end();

if (rows.length !== 0) {
  console.error("FAIL: batch row still present after retention window:", rows[0]);
  process.exit(1);
}
console.log(`batch ${result.batchId} reaped`);

await q.shutdown();
await q.close();
console.log("OK");
