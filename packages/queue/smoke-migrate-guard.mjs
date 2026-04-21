// Exercise the two start() migration modes against a fresh schema.
//   1. start() with no opts → errors out if pending migrations exist.
//   2. start({ skipMigrationCheck: true }) → starts even with pending (and
//      would error at runtime, which we DON'T test here — we just prove the
//      check is bypassed).
//
// Migrations are a DEPLOY-STEP concern (run `eddyq migrate run` or call
// `await eddyq.migrate()` from a one-shot Node script before booting workers);
// there is intentionally no auto-migrate-on-start. See ADR 011.

import pkg from "./lib.cjs";
const { Eddyq } = pkg;

const SCHEMA = `mg_${Date.now().toString(36)}`;
const BASE = "postgres://eddyq:eddyq@localhost:5433/eddyq_dev";
const URL = `${BASE}?options=-c%20search_path%3D${SCHEMA}`;

import pg from "pg";
const admin = new pg.Client({ connectionString: BASE });
await admin.connect();
await admin.query(`CREATE SCHEMA IF NOT EXISTS ${SCHEMA}`);
console.log(`fresh schema ${SCHEMA}`);

// --- Mode 1: default start() should error ---
{
  const q = await Eddyq.connect(URL, { maxConnections: 2 });
  q.work("demo", async () => {});
  let errMsg;
  try {
    await q.start();
  } catch (e) {
    errMsg = e.message;
  }
  await q.close();
  const gotGuard = errMsg && errMsg.includes("pending migration");
  console.log("1) default start():", gotGuard ? "PASS (errored)" : `FAIL: ${errMsg}`);
  if (!gotGuard) process.exit(1);
}

// --- Mode 2: explicit migrate() unblocks start() ---
{
  const q = await Eddyq.connect(URL, { maxConnections: 2 });
  q.work("demo.ok", async () => ({ ran: true }));
  // This is the prescribed path: apply migrations as an explicit step, then
  // boot workers. In prod that step is typically `eddyq migrate run` from
  // your deploy script; in a Node-only deployment it can be this call.
  await q.migrate();
  await q.start();
  await q.enqueue("demo.ok", {}, { uniqueKey: `k-${Date.now()}` });
  await new Promise((r) => setTimeout(r, 800));
  await q.shutdown(3000);
  await q.close();
  console.log("2) explicit migrate() + start():", "PASS");
}

// --- Mode 3: skipMigrationCheck bypasses guard ---
// We use a second, un-migrated schema so the check would fire if not skipped.
{
  const SCHEMA2 = `mg2_${Date.now().toString(36)}`;
  await admin.query(`CREATE SCHEMA IF NOT EXISTS ${SCHEMA2}`);
  const url2 = `${BASE}?options=-c%20search_path%3D${SCHEMA2}`;
  const q = await Eddyq.connect(url2, { maxConnections: 2 });
  q.work("demo", async () => {});
  let started = false;
  try {
    await q.start({ skipMigrationCheck: true });
    started = true;
  } catch (e) {
    console.log("unexpected error:", e.message);
  }
  if (started) {
    // Shutdown — we expect runtime SQL errors in worker logs, but start() itself succeeded.
    await q.shutdown(1000).catch(() => {});
  }
  await q.close();
  await admin.query(`DROP SCHEMA ${SCHEMA2} CASCADE`);
  console.log("3) skipMigrationCheck: true:", started ? "PASS (bypassed guard)" : "FAIL");
  if (!started) process.exit(1);
}

// Cleanup
await admin.query(`DROP SCHEMA ${SCHEMA} CASCADE`);
await admin.end();
console.log("all modes OK");
