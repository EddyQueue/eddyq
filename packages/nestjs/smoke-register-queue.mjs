// Smoke test for @eddyq/nestjs: registerQueue + processor discovery + worker
// runtime + queue-name validation. Boots a real Nest app context (no HTTP),
// registers two queues, enqueues via the QueueHandle pulled from DI under
// `getQueueToken(name)` (the same token `@InjectQueue(name)` resolves to),
// runs a processor, and asserts everything wires up before shutting down.
//
// Decorators are applied manually rather than via TS syntax because this is a
// `.mjs` file — same constraint as the queue-package smoke tests. Constructor
// injection isn't exercised here (it requires `design:paramtypes` metadata
// emitted by tsc); the smoke instead resolves providers via `app.get(...)`,
// which exercises the same DI wiring.
//
//   node smoke-register-queue.mjs

import "reflect-metadata";
import { Module } from "@nestjs/common";
import { NestFactory } from "@nestjs/core";

import {
  EddyqModule,
  Processor,
  JobHandler,
  getQueueToken,
} from "./dist/index.js";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

const stamp = Date.now();
const EMAIL_QUEUE = `smoke-email-${stamp}`;
const REPORTS_QUEUE = `smoke-reports-${stamp}`;
const KIND = `smoke.demo.${stamp}`;

let processed = 0;
const seen = [];

class DemoProcessor {
  async handle({ payload, kind }) {
    processed += 1;
    seen.push({ kind, fromQueue: payload.fromQueue });
    console.log(`  processor fired: kind=${kind} fromQueue=${payload.fromQueue}`);
  }
}
Processor()(DemoProcessor);
JobHandler(KIND)(
  DemoProcessor.prototype,
  "handle",
  Object.getOwnPropertyDescriptor(DemoProcessor.prototype, "handle"),
);

class SmokeModule {}
Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: DB_URL,
      runMigrations: true,
      gracefulShutdownMs: 2000,
      // subscribeTo derives from registerQueue — both queues should be picked up.
    }),
    EddyqModule.registerQueue({
      name: EMAIL_QUEUE,
      defaults: { maxAttempts: 5 },
    }),
    EddyqModule.registerQueue({
      name: REPORTS_QUEUE,
      defaults: { priority: 5 },
    }),
  ],
  providers: [DemoProcessor],
})(SmokeModule);

const app = await NestFactory.createApplicationContext(SmokeModule, {
  logger: ["error", "warn", "log"],
});

const emailQueue = app.get(getQueueToken(EMAIL_QUEUE));
const reportsQueue = app.get(getQueueToken(REPORTS_QUEUE));
if (emailQueue.name !== EMAIL_QUEUE || reportsQueue.name !== REPORTS_QUEUE) {
  console.error("FAIL: queue handles bound to wrong name", {
    email: emailQueue.name,
    reports: reportsQueue.name,
  });
  await app.close();
  process.exit(1);
}
console.log(`@InjectQueue tokens resolved: ${emailQueue.name}, ${reportsQueue.name}`);

const r1 = await emailQueue.enqueue(
  KIND,
  { fromQueue: EMAIL_QUEUE },
  { uniqueKey: `email:${stamp}` },
);
const r2 = await reportsQueue.enqueue(
  KIND,
  { fromQueue: REPORTS_QUEUE },
  { uniqueKey: `reports:${stamp}` },
);
if (!r1.inserted || !r2.inserted) {
  console.error("FAIL: enqueue should have inserted both jobs", { r1, r2 });
  await app.close();
  process.exit(1);
}
console.log(`enqueued via QueueHandle: email.id=${r1.id} reports.id=${r2.id}`);

// Wait for the worker runtime (started by the module) to process both.
const startedAt = Date.now();
while (processed < 2 && Date.now() - startedAt < 10000) {
  await new Promise((r) => setTimeout(r, 100));
}
if (processed < 2) {
  console.error(`FAIL: expected 2 jobs processed, got ${processed}`);
  await app.close();
  process.exit(1);
}
const queues = seen.map((s) => s.fromQueue).sort();
if (queues[0] !== EMAIL_QUEUE || queues[1] !== REPORTS_QUEUE) {
  console.error("FAIL: did not see one fire from each queue", seen);
  await app.close();
  process.exit(1);
}
console.log(`processor handled ${processed} jobs from both registered queues`);

// Validation: registerQueue should reject empty / invalid names eagerly.
function expectThrows(label, fn) {
  let threw = false;
  try {
    fn();
  } catch {
    threw = true;
  }
  if (!threw) {
    console.error(`FAIL: ${label} should have thrown`);
    throw new Error(label);
  }
  console.log(`${label} rejected`);
}

try {
  expectThrows("registerQueue('')", () => EddyqModule.registerQueue({ name: "" }));
  expectThrows("registerQueue('has space')", () =>
    EddyqModule.registerQueue({ name: "has space" }),
  );
  expectThrows("registerQueue('weird!chars')", () =>
    EddyqModule.registerQueue({ name: "weird!chars" }),
  );
  expectThrows("registerQueue('x'.repeat(65))", () =>
    EddyqModule.registerQueue({ name: "x".repeat(65) }),
  );
} catch (e) {
  await app.close();
  process.exit(1);
}

// Watchdog: assert Node exits naturally (no leaked TSFNs / runtime threads)
// within 3s of `app.close()`. The watchdog is `unref()`'d so it doesn't
// itself keep the loop alive — if libuv has nothing else holding it, Node
// exits and we never see this fire. If TSFN/runtime refs leak, the loop
// stays up and the timer fires.
const exitWatchdog = setTimeout(() => {
  console.error(
    "FAIL: Node did not exit within 3s of app.close() — TSFN/runtime leak.",
  );
  process.exit(1);
}, 3000);
exitWatchdog.unref();

await app.close();
console.log("OK (waiting for natural exit — watchdog 3s)");
