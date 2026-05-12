// Smoke test for clean Node exit after Eddyq shutdown/close. Catches the
// regression where NAPI ThreadsafeFunction refs (handler TSFNs and the abort
// TSFN) keep libuv's loop alive, preventing the process from exiting on
// SIGTERM in production.
//
// Strategy: run several scenarios in sequence, each in a fresh subprocess.
// The parent waits for each subprocess to exit naturally with a timeout.
// If any subprocess hangs, that scenario leaks.
//
//   node smoke-shutdown.mjs

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const CHILD = path.join(__dirname, "_smoke-shutdown-child.mjs");

const SCENARIOS = [
  { name: "connect+close (no work, no start)", arg: "connect-close" },
  { name: "drain (work runs to completion)", arg: "full-lifecycle" },
  { name: "force (hostile handler, fast exit + reclaim)", arg: "force-shutdown" },
  { name: "abandon (hostile handler, fast exit, no DB ops)", arg: "abandon-shutdown" },
  { name: "skip-shutdown (defensive close cleans up)", arg: "skip-shutdown" },
  { name: "connect+enqueue+close (no worker)", arg: "enqueue-only" },
];

// Total budget per scenario, from spawn → exit. Includes sqlx connect, LISTEN
// setup, napi load, the actual workflow, and the inner shutdown assertion.
// Generous on purpose — the strict regression catch is each scenario's own
// internal timing assertion (e.g. abandon shutdown must complete in <2s).
// This parent budget exists to detect a *hang*, not to gate latency.
const EXIT_BUDGET_MS = 15000;

let failed = 0;
for (const s of SCENARIOS) {
  const startedAt = Date.now();
  const code = await runChild(s.arg);
  const elapsed = Date.now() - startedAt;
  if (code === 0 && elapsed < EXIT_BUDGET_MS) {
    console.log(`  PASS ${s.name} — exited cleanly in ${elapsed}ms`);
  } else if (code === null) {
    console.error(`  FAIL ${s.name} — process hung past ${EXIT_BUDGET_MS}ms`);
    failed += 1;
  } else if (code !== 0) {
    console.error(`  FAIL ${s.name} — exit code ${code} after ${elapsed}ms`);
    failed += 1;
  }
}

if (failed > 0) {
  console.error(`${failed} scenario(s) failed`);
  process.exit(1);
}
console.log("OK");

async function runChild(arg) {
  return new Promise((resolve) => {
    const child = spawn("node", [CHILD, arg], {
      stdio: ["ignore", "pipe", "pipe"],
      env: process.env,
    });
    let killed = false;
    const watchdog = setTimeout(() => {
      killed = true;
      child.kill("SIGKILL");
    }, EXIT_BUDGET_MS);
    let stderr = "";
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("exit", (code) => {
      clearTimeout(watchdog);
      if (killed) {
        if (stderr) console.error(`    child stderr: ${stderr.trim()}`);
        resolve(null);
      } else {
        if (code !== 0 && stderr) console.error(`    child stderr: ${stderr.trim()}`);
        resolve(code);
      }
    });
  });
}
