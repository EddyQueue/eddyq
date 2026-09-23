// Smoke test for @eddyq/nestjs shutdown ordering: an in-flight handler must
// finish before any user module's `onModuleDestroy` closes the resources it
// depends on. Which user module Nest tears down first varies by major, so a
// "pool" lives in both the root and a feature module.
//
//   node smoke-shutdown-order.mjs

import "reflect-metadata";
import { Injectable, Module } from "@nestjs/common";
import { NestFactory } from "@nestjs/core";

import { EddyqModule, Processor, JobHandler, getQueueToken } from "./dist/index.js";

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

const stamp = Date.now();
const QUEUE = `smoke-shutdown-${stamp}`;
const KIND = `smoke.shutdown.${stamp}`;
const events = [];

class RootPool {
  onModuleDestroy() {
    events.push("root pool closed");
  }
}
Injectable()(RootPool);

class FeaturePool {
  onModuleDestroy() {
    events.push("feature pool closed");
  }
}
Injectable()(FeaturePool);

let started = false;
class SlowProcessor {
  async handle() {
    started = true;
    await new Promise((r) => setTimeout(r, 1000));
    events.push("handler finished");
  }
}
Processor()(SlowProcessor);
JobHandler(KIND)(
  SlowProcessor.prototype,
  "handle",
  Object.getOwnPropertyDescriptor(SlowProcessor.prototype, "handle"),
);

class FeatureModule {}
Module({
  imports: [EddyqModule.registerQueue({ name: QUEUE })],
  providers: [FeaturePool, SlowProcessor],
})(FeatureModule);

class RootModule {}
Module({
  imports: [
    EddyqModule.forRoot({ databaseUrl: DB_URL, runMigrations: true, gracefulShutdownMs: 5000 }),
    FeatureModule,
  ],
  providers: [RootPool],
})(RootModule);

const app = await NestFactory.createApplicationContext(RootModule, {
  logger: ["error", "warn"],
});

await app.get(getQueueToken(QUEUE)).enqueue(KIND, {});
const t = Date.now();
while (!started && Date.now() - t < 10000) await new Promise((r) => setTimeout(r, 50));
if (!started) {
  console.error("FAIL: job never started");
  await app.close();
  process.exit(1);
}

await app.close();

// Only the drain-first guarantee is ours; the pools' relative order is Nest's
// (root-first on 11+, deepest-first on 10).
if (events[0] !== "handler finished" || events.length !== 3) {
  console.error(
    `FAIL: shutdown order ${JSON.stringify(events)}, expected the handler to finish before either pool closes`,
  );
  process.exit(1);
}
console.log(`OK shutdown order: ${events.join(" → ")}`);
