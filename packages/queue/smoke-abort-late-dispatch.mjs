// The abort broadcast is one-shot: it can only reach AbortControllers that
// exist at the instant shutdown fires. A job already claimed in Rust but handed
// to JS a moment later therefore used to build a fresh, never-aborted
// controller, and a handler waiting on that signal would wait forever. Because
// such a handler holds an active libuv timer, Node cannot exit — the process
// hangs for the handler's own duration no matter which shutdown mode ran, which
// is how `force` and `abandon` intermittently blew their exit budget in CI.
//
// This exercises the ordering directly rather than trying to lose the race by
// luck: broadcast first, dispatch second. It needs no database and no worker
// runtime, so it is deterministic on any machine — the end-to-end smoke test
// only reproduces the problem under CI-grade load.
//
// Run: node smoke-abort-late-dispatch.mjs

import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const lib = require("./lib.cjs");
const native = require("./index.js");

const HANDLER_WAIT_MS = 30_000;
const BUDGET_MS = 2_000;

let failures = 0;
const check = (name, ok, detail) => {
  console.log(`  ${ok ? "PASS" : "FAIL"} ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures += 1;
};

// A handler shaped like the one in the shutdown smoke test: it waits on a timer
// and only gives up when the signal tells it to.
function waitsForAbort() {
  return async ({ signal }) => {
    await new Promise((resolve) => {
      const t = setTimeout(resolve, HANDLER_WAIT_MS);
      signal.addEventListener("abort", () => {
        clearTimeout(t);
        resolve();
      });
    });
  };
}

// Capture the wrapped handler and the abort broadcast the way the runtime does,
// without starting one: `work()` installs both.
function harness() {
  const q = Object.create(native.Eddyq.prototype);
  let broadcast;
  let wrapped;
  q.setAbortHandler = (fn) => {
    broadcast = fn;
  };
  const origWork = Object.getPrototypeOf(q).work;
  q.work = (kind, handler) => {
    wrapped = handler;
  };
  void origWork;
  native.Eddyq.prototype.work.call(q, "late-dispatch", waitsForAbort());
  return { broadcast, wrapped };
}

const { broadcast, wrapped } = harness();

if (typeof broadcast !== "function" || typeof wrapped !== "function") {
  console.error("  FAIL harness did not capture the abort handler / wrapped handler");
  process.exit(1);
}

// Shutdown happens first...
broadcast("shutdown");

// ...and only then does the already-claimed job reach JS.
const started = Date.now();
let settled = "pending";
const dispatch = wrapped({ id: 1, kind: "late-dispatch", payload: {} })
  .then(() => {
    settled = "resolved";
  })
  .catch(() => {
    settled = "rejected";
  });

await Promise.race([
  dispatch,
  new Promise((r) => setTimeout(r, BUDGET_MS).unref?.()),
]);

const elapsed = Date.now() - started;
check(
  "a job dispatched after shutdown does not start its wait",
  settled !== "pending",
  settled === "pending"
    ? `still running after ${elapsed}ms — it will hold the event loop for ${HANDLER_WAIT_MS}ms`
    : `${settled} in ${elapsed}ms`,
);
check(
  "and it fails rather than reporting success",
  settled === "rejected",
  `settled=${settled}`,
);

if (failures > 0) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("OK");
