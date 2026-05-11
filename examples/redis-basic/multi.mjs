// EddyqApp demo: webhooks on Redis, payments on Postgres, one process.
//
// Run:
//   docker compose -f ../../docker-compose.dev.yml up -d postgres redis
//   pnpm install
//   DATABASE_URL=postgres://eddyq:eddyq@localhost:5433/eddyq_dev \
//   REDIS_URL=redis://127.0.0.1:6381 \
//   node multi.mjs

import pkg from "@eddyq/queue";
const { EddyqApp } = pkg;

const dbUrl = process.env.DATABASE_URL;
const redisUrl = process.env.REDIS_URL ?? "redis://127.0.0.1:6381";
if (!dbUrl) {
  console.error("set DATABASE_URL (e.g. postgres://eddyq:eddyq@localhost:5433/eddyq_dev)");
  process.exit(1);
}

const counters = { webhooks: 0, payments: 0 };

async function main() {
  const app = await EddyqApp.connect({
    postgres: { databaseUrl: dbUrl },
    redis:    { url: redisUrl, line: "demo-app" },
    queues: [
      { name: "webhooks", provider: "redis" },
      { name: "payments", provider: "postgres" },
    ],
    defaultProvider: "postgres",
  });

  // One handler, runs on both backends — the backend that fetched the job
  // invokes it. The handler reads `call.payload.queue` (whatever we put in)
  // to know which lane it came from.
  app.work("process", async ({ payload }) => {
    counters[payload.queue] += 1;
    return { ranOn: payload.queue, at: Date.now() };
  });

  await app.start({ fetchPollIntervalMs: 50 });

  await app.enqueue("process", { queue: "webhooks" }, { queue: "webhooks" });
  await app.enqueue("process", { queue: "webhooks" }, { queue: "webhooks" });
  await app.enqueue("process", { queue: "payments" }, { queue: "payments" });

  // Drain.
  await new Promise((r) => setTimeout(r, 1500));
  console.log("counters:", counters);

  await app.shutdown({ mode: "drain" });

  const ok = counters.webhooks === 2 && counters.payments === 1;
  console.log(ok ? "OK — routed per queue" : "FAIL");
  process.exit(ok ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
