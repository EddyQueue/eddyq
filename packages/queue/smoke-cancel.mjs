// Validate cancellation plumbing:
//   - An in-flight handler receives an AbortSignal on its `call` arg.
//   - queue.shutdown() triggers .abort() on that signal.
//   - The handler exits promptly instead of hanging; shutdown returns within
//     the grace timeout.
//
// Schema: reuses `errsmoke` (schema already migrated by prior run).

import pkg from "./lib.cjs";
const { Eddyq } = pkg;

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

const q = await Eddyq.connect(DB_URL, { maxConnections: 4 });
await q.migrate();

let handlerStarted = 0;
let handlerAborted = 0;

q.work("demo.slow", async ({ signal }) => {
  handlerStarted++;
  // Pretend to wait 10 seconds. Abort should short-circuit this.
  await new Promise((resolve, reject) => {
    const timer = setTimeout(resolve, 10_000);
    signal.addEventListener("abort", () => {
      clearTimeout(timer);
      handlerAborted++;
      reject(signal.reason ?? new Error("aborted"));
    });
  });
  return "done";
});

await q.enqueue("demo.slow", { hi: "there" }, { uniqueKey: `slow-${Date.now()}` });

await q.start();

// Give the fetcher a chance to claim and start the handler.
await new Promise((r) => setTimeout(r, 500));

const t0 = Date.now();
await q.shutdown(5_000); // 5s grace
const elapsed = Date.now() - t0;

console.log({ handlerStarted, handlerAborted, shutdownMs: elapsed });

await q.close();

// handlerStarted>=1 proves we claimed the job.
// handlerAborted>=1 proves the abort signal propagated.
// elapsed<2000 proves shutdown wasn't blocked by the 10s sleep.
const pass = handlerStarted >= 1 && handlerAborted >= 1 && elapsed < 2_000;
console.log(pass ? "PASS" : "FAIL");
process.exit(pass ? 0 : 1);
