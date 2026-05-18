// Child process for smoke-shutdown.mjs — runs a single shutdown scenario and
// exits naturally (no `process.exit`). The parent waits for the natural exit
// or kills us if we hang.

// Import from lib.cjs to pick up the AbortController wrapper that turns
// `call.signal` into a real AbortSignal. The raw NAPI binding (./index.js)
// passes the call object through unwrapped — handlers expecting `signal`
// would crash on `signal.addEventListener`.
import pkg from "./lib.cjs";
const { Eddyq } = pkg;

const DB_URL =
  process.env.EDDYQ_DATABASE_URL ??
  "postgres://eddyq:eddyq@localhost:5433/eddyq_dev?options=-c%20search_path%3Dv01";

const SCENARIO = process.argv[2];
const KIND = `shutdown-smoke.${SCENARIO}.${Date.now()}`;

async function connectAndClose() {
  const q = await Eddyq.connect(DB_URL, { maxConnections: 2 });
  await q.close();
}

async function fullLifecycle() {
  const q = await Eddyq.connect(DB_URL, { maxConnections: 2 });
  try {
    await q.migrate();
  } catch {}
  await q.work(KIND, async () => "ok");
  await q.enqueue(KIND, {}, { uniqueKey: `lc-${Date.now()}` });
  await q.start();
  // Briefly let the worker process the job.
  await new Promise((r) => setTimeout(r, 500));
  await q.shutdown({ mode: "drain", gracefulTimeoutMs: 2000 });
  await q.close();
}

// Cooperative slow handler — runs until either 30s elapse or the
// AbortSignal flips. Real handlers should look like this. Returns once
// the signal aborts, which lets Node's event loop drain naturally after
// queue.shutdown / close.
//
// Returns `{ handler, entered }` where `entered` resolves the first time
// the handler is invoked. This is a deterministic "job is in-flight"
// signal — by the time the JS handler body runs, the worker has already
// claimed the row (state=running) AND inserted into the in_flight set,
// so callers can replace DB-polling preconditions with `await entered`.
function makeSlowCooperative() {
  let onEntered;
  const entered = new Promise((r) => {
    onEntered = r;
  });
  const handler = async ({ signal }) => {
    onEntered();
    await new Promise((resolve) => {
      const t = setTimeout(resolve, 30_000);
      signal.addEventListener("abort", () => {
        clearTimeout(t);
        resolve();
      });
    });
  };
  return { handler, entered };
}

async function forceShutdown() {
  const q = await Eddyq.connect(DB_URL, { maxConnections: 2 });
  try {
    await q.migrate();
  } catch {}
  const { handler, entered } = makeSlowCooperative();
  await q.work(KIND, handler);
  await q.enqueue(KIND, {}, { uniqueKey: `force-${Date.now()}` });
  await q.start();
  // Deterministic wait: the handler resolves `entered` on first invocation,
  // which guarantees the worker has both transitioned the row to `running`
  // and registered it in the in_flight set.
  await entered;

  const t = Date.now();
  await q.shutdown({ mode: "force" });
  const elapsed = Date.now() - t;
  if (elapsed > 2000) throw new Error(`force shutdown took ${elapsed}ms`);

  // Verify the row was reclaimed back to `pending` (force-mode reclaim
  // is the value-add of this mode). Use pg directly — the napi binding
  // doesn't expose a single-row state read.
  const { default: pg } = await import("pg");
  const c = new pg.Client({ connectionString: DB_URL });
  await c.connect();
  const { rows } = await c.query(
    "SELECT state, stalled_count FROM eddyq_jobs WHERE kind = $1 ORDER BY id DESC LIMIT 1",
    [KIND],
  );
  await c.end();
  if (rows[0].state !== "pending") {
    throw new Error(
      `force-shutdown: row state should be 'pending' (reclaimed); got ${rows[0].state}`,
    );
  }
  // Reclaim mirrors stalled-sweep semantics: stalled_count bumps, attempt is
  // rolled back (the prior claim didn't reach a verdict). So a fresh job
  // post-reclaim has attempt=0, stalled_count=1.
  if (rows[0].stalled_count < 1) {
    throw new Error(
      `force-shutdown: stalled_count should be >= 1; got ${rows[0].stalled_count}`,
    );
  }

  await q.close();
}

async function abandonShutdown() {
  const q = await Eddyq.connect(DB_URL, { maxConnections: 2 });
  try {
    await q.migrate();
  } catch {}
  const { handler, entered } = makeSlowCooperative();
  await q.work(KIND, handler);
  await q.enqueue(KIND, {}, { uniqueKey: `abandon-${Date.now()}` });
  await q.start();
  // Deterministic wait: the handler resolves `entered` on first invocation,
  // which guarantees the worker has both transitioned the row to `running`
  // and registered it in the in_flight set.
  await entered;

  const t = Date.now();
  await q.shutdown({ mode: "abandon" });
  const elapsed = Date.now() - t;
  if (elapsed > 2000) throw new Error(`abandon shutdown took ${elapsed}ms`);

  // Abandon contract: don't bother awaiting handlers, don't reclaim. The row
  // typically stays `running` (heartbeat sweep recovers it later), but it's
  // also fine if the handler happened to resolve cooperatively before tokio
  // aborted the worker — `completed` is a benign outcome. What we DON'T want
  // is `pending` (would imply an unexpected retry path was taken).
  const { default: pg } = await import("pg");
  const c = new pg.Client({ connectionString: DB_URL });
  await c.connect();
  const { rows } = await c.query(
    "SELECT state FROM eddyq_jobs WHERE kind = $1 ORDER BY id DESC LIMIT 1",
    [KIND],
  );
  await c.end();
  if (rows[0].state !== "running" && rows[0].state !== "completed") {
    throw new Error(
      `abandon-shutdown: row state should be 'running' or 'completed'; got ${rows[0].state}`,
    );
  }

  await q.close();
}

async function skipShutdown() {
  // Verifies close() is defensive — calling close() without shutdown() must
  // still drop retained handler TSFNs so the process can exit. Don't enqueue
  // anything — just register a worker, start, then close.
  const q = await Eddyq.connect(DB_URL, { maxConnections: 2 });
  try {
    await q.migrate();
  } catch {}
  await q.work(KIND, async () => "ok");
  await q.start();
  await new Promise((r) => setTimeout(r, 100));
  // Skip shutdown(); go straight to close(). This is hostile usage — workers
  // were running — but consumers occasionally do this on hard-exit paths and
  // the binding shouldn't strand the process.
  await q.close();
}

async function enqueueOnly() {
  // No work() / start(). Connect-only producers are common (api pods).
  const q = await Eddyq.connect(DB_URL, { maxConnections: 2 });
  try {
    await q.migrate();
  } catch {}
  await q.enqueue(KIND, {}, { uniqueKey: `eo-${Date.now()}` });
  await q.close();
}

const SCENARIOS = {
  "connect-close": connectAndClose,
  "full-lifecycle": fullLifecycle,
  "force-shutdown": forceShutdown,
  "abandon-shutdown": abandonShutdown,
  "skip-shutdown": skipShutdown,
  "enqueue-only": enqueueOnly,
};

const fn = SCENARIOS[SCENARIO];
if (!fn) {
  console.error(`unknown scenario: ${SCENARIO}`);
  process.exit(2);
}
await fn();
// Intentionally no process.exit — the test point is that Node exits naturally.
