// Smoke test for the schedule API: addSchedule, listSchedules, setScheduleEnabled,
// removeSchedule. Also starts a worker briefly to verify the cron tick enqueues
// and runs jobs.
//
//   node smoke-schedule.mjs

import { Eddyq, version } from "./index.js";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

console.log("eddyq-napi version:", version());

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });

const name = `smoke-sched-${Date.now()}`;
const kind = "schedule.demo";

// Ensure cleanup even if the test is killed or crashes mid-run.
const cleanup = async () => { await q.removeSchedule(name).catch(() => {}); };
process.once('SIGINT', async () => { await cleanup(); await q.close(); process.exit(1); });
process.once('SIGTERM', async () => { await cleanup(); await q.close(); process.exit(1); });

// cron crate dialect is 6-field (sec min hour dom month dow) — fire every second.
await q.addSchedule(name, "* * * * * *", kind, { hello: "world" }, { priority: 1 });
console.log(`added schedule: ${name}`);

const list = await q.listSchedules();
const mine = list.find((s) => s.name === name);
if (!mine) {
  console.error("BUG: schedule not found in listSchedules");
  process.exit(1);
}
console.log(`  kind=${mine.kind} cron=${mine.cronExpr} nextRunAt=${mine.nextRunAt} enabled=${mine.enabled}`);

// Register handler and start worker so we can observe the tick actually firing.
let fired = 0;
await q.work(kind, async ({ payload, id }) => {
  fired += 1;
  console.log(`  handler fired #${fired}: id=${id} payload=${JSON.stringify(payload)}`);
});
await q.start();

// Scheduler interval is 5s — wait up to 10s for at least one firing.
const start = Date.now();
while (fired < 1 && Date.now() - start < 10000) {
  await new Promise((r) => setTimeout(r, 200));
}
if (fired < 1) {
  console.error("FAIL: schedule did not fire within 10s");
  await q.shutdown(1000);
  await q.close();
  process.exit(1);
}
console.log(`observed ${fired} firing(s)`);

// Disable and verify.
const disabled = await q.setScheduleEnabled(name, false);
console.log(`setScheduleEnabled(false) → ${disabled}`);
const after = (await q.listSchedules()).find((s) => s.name === name);
if (after.enabled !== false) {
  console.error("BUG: schedule still enabled after disable");
  process.exit(1);
}

// Re-upsert should update in place, not duplicate.
await q.addSchedule(name, "0 0 * * * *", kind, { hello: "updated" }, { priority: 9 });
const re = (await q.listSchedules()).find((s) => s.name === name);
if (re.priority !== 9 || re.payload.hello !== "updated") {
  console.error("BUG: upsert did not update existing row");
  process.exit(1);
}
console.log("upsert updated existing row (priority=9, payload.hello=updated)");

// Remove and verify.
const removed = await q.removeSchedule(name);
console.log(`removeSchedule → ${removed}`);
const gone = (await q.listSchedules()).find((s) => s.name === name);
if (gone) {
  console.error("BUG: schedule still present after removal");
  process.exit(1);
}

// Removing a non-existent schedule returns false (not an error).
const noop = await q.removeSchedule("does-not-exist-xyz");
if (noop !== false) {
  console.error("BUG: removing nonexistent schedule should return false");
  process.exit(1);
}

// Invalid cron expression should reject.
try {
  await q.addSchedule("bad-cron", "not a cron", kind, {});
  console.error("BUG: invalid cron should have thrown");
  process.exit(1);
} catch (e) {
  console.log("invalid cron rejected:", e.message.split("\n")[0]);
}

await q.shutdown(2000);
await q.close();
console.log("OK");
