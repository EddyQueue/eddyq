// Smoke test: boot a minimal Nest app with a Processor, enqueue a job, wait
// for the handler to fire, shut down.
//
//   node smoke.mjs

import "reflect-metadata";
import common from "@nestjs/common";
import core from "@nestjs/core";
import {
  EddyqModule,
  InjectEddyq,
  JobHandler,
  Processor,
} from "./dist/index.js";

const { Module } = common;
const { NestFactory } = core;

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

let handlerFired = false;

class SmokeProcessor {
  async ping({ payload, id }) {
    console.log(`handler: ping received — id=${id} payload=${JSON.stringify(payload)}`);
    handlerFired = true;
  }
}
// Apply decorators manually — avoids needing a .ts compile for the smoke script.
Processor()(SmokeProcessor);
JobHandler("nest.smoke.ping")(
  SmokeProcessor.prototype,
  "ping",
  Object.getOwnPropertyDescriptor(SmokeProcessor.prototype, "ping"),
);

class EnqueueService {
  constructor(queue) {
    this.queue = queue;
  }
  async onApplicationBootstrap() {
    const r = await this.queue.enqueue(
      "nest.smoke.ping",
      { at: new Date().toISOString() },
      { uniqueKey: `nest-smoke-${Date.now()}` },
    );
    console.log(`enqueued id=${r.id}`);
  }
}
// Constructor param DI annotation for @InjectEddyq()
EnqueueService.prototype.constructor = EnqueueService;
Reflect.defineMetadata("design:paramtypes", [Object], EnqueueService);
InjectEddyq()(EnqueueService, undefined, 0);

class AppModule {}
Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: DB_URL,
      workerConcurrency: 2,
      runMigrations: true,
    }),
  ],
  providers: [SmokeProcessor, EnqueueService],
})(AppModule);

const app = await NestFactory.createApplicationContext(AppModule, { logger: ["log", "warn", "error"] });
app.enableShutdownHooks();

// Wait up to 5s for the handler to fire.
const start = Date.now();
while (!handlerFired && Date.now() - start < 5000) {
  await new Promise((r) => setTimeout(r, 100));
}

if (!handlerFired) {
  console.error("FAIL: handler did not fire within 5s");
  await app.close();
  process.exit(1);
}

console.log("OK — handler fired. Shutting down…");
await app.close();
console.log("shutdown complete");
