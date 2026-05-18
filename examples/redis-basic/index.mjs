// Minimal end-to-end example using the Redis backend.
//
// Run:
//   docker compose -f ../../docker-compose.dev.yml up -d redis
//   pnpm install
//   REDIS_URL=redis://127.0.0.1:6381 pnpm start

// `@eddyq/queue` ships as CommonJS (lib.cjs) — Node's ESM static analyzer
// can't see through `module.exports = { ...native, … }`, so we default-import
// and destructure at runtime. CJS consumers can keep using `require()`.
import pkg from "@eddyq/queue";
const { EddyqRedis, CancelError } = pkg;

const url = process.env.REDIS_URL ?? "redis://127.0.0.1:6381";

async function main() {
  // Connect + bootstrap-load the Redis Functions library.
  const queue = await EddyqRedis.connect(url, { line: "demo" });
  console.log(`connected (line=${queue.line})`);

  // Cap the `tenant-acme` group to 1 concurrent job — useful when an
  // upstream API is per-tenant rate-limited.
  await queue.setGroupConcurrency("tenant-acme", 1);

  // Interval schedule: fires every 1500ms. The `{ every }` shape is
  // sugar that skips cron entirely — leader-driven, fires from now.
  // Equivalent forms accepted:
  //   queue.addSchedule("heartbeat", "*/2 * * * * *", ...)  // cron
  //   queue.addSchedule("heartbeat", { cron: "..." }, ...)   // cron object
  //   queue.addSchedule("heartbeat", { every: 1500 }, ...)   // interval
  await queue.addSchedule("heartbeat", { every: 1500 }, "heartbeat", {});

  // Worker handlers. Throw `CancelError` to skip retries.
  queue.work("send-email", async ({ payload }) => {
    if (!payload.to) {
      throw new CancelError("missing recipient — won't retry");
    }
    console.log(`  → sending ${payload.subject} to ${payload.to}`);
    return { sentAt: Date.now() };
  });

  let heartbeats = 0;
  queue.work("heartbeat", async () => {
    heartbeats += 1;
    return null;
  });

  // schedulerIntervalMs: 200 means the leader's scheduler loop ticks every
  // 200ms — so the `{ every: 1500 }` schedule fires close to that cadence.
  await queue.start({ fetchPollIntervalMs: 50, schedulerIntervalMs: 200 });
  console.log("worker started");

  // Enqueue a few jobs. Two onto the per-tenant group — the rule auto-caps
  // them to 1 concurrent run.
  const r1 = await queue.enqueue("send-email", {
    to: "alice@example.com",
    subject: "hi",
  });
  const r2 = await queue.enqueue(
    "send-email",
    { to: "bob@example.com", subject: "yo" },
    { groupKey: "tenant-acme" },
  );
  const r3 = await queue.enqueue(
    "send-email",
    { to: "carol@example.com", subject: "ahoy" },
    { groupKey: "tenant-acme" },
  );
  console.log("enqueued:", r1.id, r2.id, r3.id);

  // Drain emails + give the scheduler a beat to fire the cron once. The
  // default scheduler interval is 5 s, so wait a bit longer to demonstrate.
  await new Promise((r) => setTimeout(r, 6500));
  console.log(`heartbeats fired: ${heartbeats}`);

  // Snapshot the dashboard surface.
  const stats = await queue.getStats();
  console.log("\nstats:");
  for (const s of stats.byQueueState ?? stats.by_queue_state ?? []) {
    console.log(`  ${s.queue}/${s.state}: ${s.count}`);
  }

  const groups = await queue.listGroups();
  console.log(`groups: ${groups.length} (${groups.map((g) => g.key).join(", ")})`);

  // Clean up.
  await queue.removeSchedule("heartbeat-every-sec");
  await queue.shutdown({ mode: "drain" });
  console.log("\nshutdown complete");
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
