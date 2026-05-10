// Smoke test for per-schedule queue routing + queue-name validation.
// Verifies:
//   1. A schedule with `queue: "X"` fires jobs onto queue X (not "default").
//   2. Empty / invalid queue names are rejected at the API boundary.
//   3. addSchedule with no queue defaults to "default" (back-compat).
//
//   node smoke-schedule-queue.mjs

import { Eddyq, version } from "./index.js";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

console.log("eddyq-napi version:", version());

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });

// Migrate idempotently — first run creates the schema; later runs are a no-op.
try {
  await q.migrate();
} catch (e) {
  console.log("migrate skipped:", e.message);
}

const stamp = Date.now();
const schedName = `smoke-q-${stamp}`;
const kind = `schedule.queue.demo.${stamp}`;
const targetQueue = `smoke-routed-${stamp}`;

const cleanup = async () => {
  await q.removeSchedule(schedName).catch(() => {});
};
process.once("SIGINT", async () => { await cleanup(); await q.close(); process.exit(1); });
process.once("SIGTERM", async () => { await cleanup(); await q.close(); process.exit(1); });

// 1. Register a schedule with queue routing.
await q.syncSchedules([
  {
    name: schedName,
    cronExpr: "* * * * * *",
    kind,
    payload: { hello: "world" },
    queue: targetQueue,
  },
]);
const listed = (await q.listSchedules()).find((s) => s.name === schedName);
if (!listed || listed.queue !== targetQueue) {
  console.error(`FAIL: listSchedules should expose queue=${targetQueue}, got ${listed?.queue}`);
  process.exit(1);
}
console.log(`schedule queue field round-trips: ${listed.queue}`);

// Subscribe a worker to the target queue so we observe the routed firing.
q.subscribeTo([targetQueue]);
let fired = 0;
let firedQueue = null;
await q.work(kind, async ({ payload, id }) => {
  fired += 1;
  // Look up the row's queue column to confirm routing.
  const list = await q.listJobs({ queue: targetQueue, kind }, { limit: 1 });
  if (list.rows.length > 0) firedQueue = list.rows[0].queue;
  console.log(`  handler fired #${fired}: id=${id} payload=${JSON.stringify(payload)} queue=${firedQueue}`);
});
await q.start();

const startedAt = Date.now();
while (fired < 1 && Date.now() - startedAt < 10000) {
  await new Promise((r) => setTimeout(r, 200));
}
if (fired < 1) {
  console.error(`FAIL: schedule did not fire on queue=${targetQueue} within 10s`);
  await q.shutdown(1000);
  await cleanup();
  await q.close();
  process.exit(1);
}
if (firedQueue !== targetQueue) {
  console.error(`FAIL: fired job landed on queue=${firedQueue}, expected ${targetQueue}`);
  process.exit(1);
}
console.log(`schedule fire routed correctly to queue=${targetQueue}`);

await q.shutdown(2000);
await cleanup();

// 2. Empty queue name on a schedule should reject at the API boundary.
try {
  await q.syncSchedules([
    {
      name: `${schedName}-bad-empty`,
      cronExpr: "* * * * * *",
      kind,
      payload: {},
      queue: "",
    },
  ]);
  console.error("FAIL: empty queue name should have thrown");
  process.exit(1);
} catch (e) {
  console.log("empty queue name rejected:", e.message.split("\n")[0]);
}

// 3. Invalid characters should reject.
try {
  await q.syncSchedules([
    {
      name: `${schedName}-bad-chars`,
      cronExpr: "* * * * * *",
      kind,
      payload: {},
      queue: "has space",
    },
  ]);
  console.error("FAIL: queue name with space should have thrown");
  process.exit(1);
} catch (e) {
  console.log("invalid queue chars rejected:", e.message.split("\n")[0]);
}

// 4. Enqueue path also rejects invalid queue names.
try {
  await q.enqueue(kind, {}, { queue: "bad name!" });
  console.error("FAIL: invalid queue on enqueue should have thrown");
  process.exit(1);
} catch (e) {
  console.log("enqueue rejects invalid queue name:", e.message.split("\n")[0]);
}

// 5. Default queue (no queue field) is back-compat — defaults to "default".
const defaultName = `${schedName}-default`;
await q.addSchedule(defaultName, "0 0 0 * * *", kind, {});
const def = (await q.listSchedules()).find((s) => s.name === defaultName);
if (!def || def.queue !== "default") {
  console.error(`FAIL: addSchedule without queue should default to "default", got ${def?.queue}`);
  process.exit(1);
}
console.log("addSchedule defaults queue to 'default' when omitted");
await q.removeSchedule(defaultName);

await q.close();
console.log("OK");
