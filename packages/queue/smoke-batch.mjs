// Smoke test: enqueueBatch end-to-end.
//
//   - register handlers for "batch.item" and "batch.done"
//   - enqueueBatch(3 items + onComplete)
//   - start the worker, wait for onComplete to fire
//   - verify the _eddyq_batch envelope in the payload
//   - empty-batch fast path: items=[] still fires onComplete immediately
//
//   node smoke-batch.mjs

import { Eddyq } from "./index.js";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });
await q.migrate();

let itemsSeen = 0;
let doneCall = null;
let resolveDone;
const done = new Promise((resolve) => (resolveDone = resolve));

q.work("batch.item", async (call) => {
  itemsSeen += 1;
  return { n: call.payload.n };
});

q.work("batch.done", async (call) => {
  doneCall = call;
  resolveDone();
  return { ok: true };
});

let emptyDoneFired = false;
let resolveEmpty;
const emptyDone = new Promise((resolve) => (resolveEmpty = resolve));
q.work("batch.done.empty", async () => {
  emptyDoneFired = true;
  resolveEmpty();
  return { ok: true };
});

q.setWorkerConcurrency(4);
q.subscribeTo(["default"]);

const stamp = Date.now();
const items = [0, 1, 2].map((n) => ({
  kind: "batch.item",
  payload: { n },
  uniqueKey: `smoke-batch-${stamp}-${n}`,
}));

const result = await q.enqueueBatch({
  items,
  onComplete: {
    kind: "batch.done",
    payload: { marker: "smoke", stamp },
    uniqueKey: undefined, // eddyq stamps a deterministic one based on batchId
  },
});
console.log(
  `enqueueBatch(3) → batchId=${result.batchId} inserted=${result.inserted} skipped=${result.skipped}`,
);
if (result.inserted !== 3 || result.skipped !== 0) {
  console.error("FAIL: expected inserted=3, skipped=0");
  process.exit(1);
}

await q.start();

const fired = await Promise.race([
  done.then(() => true),
  new Promise((resolve) => setTimeout(() => resolve(false), 10_000)),
]);
if (!fired) {
  console.error(`FAIL: onComplete did not fire within 10s. itemsSeen=${itemsSeen}`);
  process.exit(1);
}

const env = doneCall.payload._eddyq_batch;
console.log("onComplete fired. envelope:", env);
const expected = ["batchId", "total", "completed", "failed", "cancelled", "durationMs"];
for (const k of expected) {
  if (!(k in env)) {
    console.error(`FAIL: envelope missing key '${k}'`);
    process.exit(1);
  }
}
if (env.total !== 3 || env.completed !== 3 || env.failed !== 0 || env.cancelled !== 0) {
  console.error("FAIL: envelope counts wrong:", env);
  process.exit(1);
}
if (doneCall.payload.marker !== "smoke") {
  console.error("FAIL: user payload didn't survive merge");
  process.exit(1);
}

// Empty batch: still fires onComplete with all-zero counts.
const empty = await q.enqueueBatch({
  items: [],
  onComplete: { kind: "batch.done.empty", payload: { from: "empty-test" } },
});
console.log(
  `enqueueBatch(empty) → batchId=${empty.batchId} inserted=${empty.inserted} skipped=${empty.skipped}`,
);
if (empty.inserted !== 0) {
  console.error("FAIL: expected inserted=0");
  process.exit(1);
}

const emptyFired = await Promise.race([
  emptyDone.then(() => true),
  new Promise((resolve) => setTimeout(() => resolve(false), 5_000)),
]);
if (!emptyFired) {
  console.error("FAIL: empty-batch onComplete did not fire");
  process.exit(1);
}

await q.shutdown();
await q.close();
console.log("OK");
